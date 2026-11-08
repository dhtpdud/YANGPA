import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiClient, RpcError } from './client';
import { createMockTransport } from './mock';
import { WEB_BASE } from '../lib/links';
import { ANONYMOUS_DEVICE, ANONYMOUS_USER, EVERYONE_ROLE, ErrorCode, type Notification } from './types';

const open: Array<{ dispose(): void }> = [];

function setup(opts: { noIdentity?: boolean } = {}) {
  const t = createMockTransport({ quiet: true, seed: 7, ...opts });
  open.push(t.backend);
  return { t, client: new ApiClient(t) };
}

afterEach(() => {
  while (open.length) open.pop()?.dispose();
});

async function crew(client: ApiClient) {
  const spaces = await client.call('space.list', {});
  const s = spaces.find((x) => x.name === '게임 모임')!;
  const view = await client.call('space.get', { space: s.space });
  const ch = (name: string) => view.channels.find((c) => c.settings.name === name)!;
  return { spaces, view, ch };
}

describe('MOCK 백엔드', () => {
  it('친구 둘, 임시·만료된 연락처 하나씩, 크루 둘과 역할·채널 종류를 갖춘다', async () => {
    const { client } = setup();
    const { spaces, view, ch } = await crew(client);
    expect(spaces.filter((s) => s.kind === 'space')).toHaveLength(4);
    expect(spaces.filter((s) => s.kind === 'crew')).toHaveLength(2);
    const friends = await client.call('friend.list', {});
    expect(friends.map((f) => f.kind).sort()).toEqual(['expired', 'friend', 'friend', 'temporary']);
    expect(friends.find((f) => f.kind === 'temporary')?.expires_at).toBeGreaterThan(Date.now());
    expect(view.roles.map((r) => r.name)).toContain('@everyone');
    expect(view.roles.length).toBeGreaterThanOrEqual(4);
    expect(ch('비밀-작전').settings).toMatchObject({ security: 'volatile', join_mode: 'join_leave', ttl: { mode: 'seconds', seconds: 60 } });
    expect(ch('공략-게시판').settings.kind).toBe('forum');
    expect(ch('일반 음성').settings.kind).toBe('voice');
  });

  it('보기를 읽어도 읽음 처리되지 않고, mark_read로만 줄어든다', async () => {
    // 읽음 위치(MOCK은 위치와 상관없이 채널 전체를 읽음으로).
    const LAST = 'A'.repeat(44);
    const { client } = setup();
    const { spaces, view, ch } = await crew(client);
    const before = spaces.find((s) => s.name === '게임 모임')!.unread;
    expect(before).toBeGreaterThan(0);
    await client.call('space.get', { space: view.space });
    const again = (await client.call('space.list', {})).find((s) => s.space === view.space)!;
    expect(again.unread).toBe(before);
    await client.call('channel.mark_read', { space: view.space, channel: ch('잡담').channel, event: LAST });
    const after = (await client.call('space.list', {})).find((s) => s.space === view.space)!;
    expect(after.unread).toBe(Math.max(0, before - ch('잡담').unread));
    // 1대1 공간은 채널 하나를 읽으면 전부 읽음.
    const dm = spaces.find((s) => s.kind === 'space' && s.unread > 0)!;
    const dmView = await client.call('space.get', { space: dm.space });
    await client.call('channel.mark_read', { space: dm.space, channel: dmView.channels[0].channel, event: LAST });
    expect((await client.call('space.list', {})).find((s) => s.space === dm.space)!.unread).toBe(0);
  });

  it('잡담 2000개를 최신 페이지부터 keyset으로 거슬러 주고(페이지 안은 오래된 것부터), 받지 않은 구간을 알린다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('잡담').channel;
    const seen = new Set<string>();
    let before: string | undefined;
    let gaps = 0;
    for (let i = 0; i < 20; i++) {
      const page = await client.call('message.list', { space: view.space, channel, before, limit: 200 });
      if (page.messages.length === 0) break;
      // 페이지 안은 오래된 것부터(데몬과 같음)
      expect(page.messages[0].lamport).toBeLessThan(page.messages.at(-1)!.lamport);
      for (const m of page.messages) seen.add(m.id);
      gaps += page.gaps.length;
      before = page.messages[0].id;
    }
    expect(seen.size).toBe(1970);
    expect(gaps).toBeGreaterThanOrEqual(1);
  });

  it('구간 바로 뒤에서 다시 읽으면(따라잡기 흉내) 빠진 30개가 채워지고 구간이 사라진다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('잡담').channel;
    let gap;
    let before: string | undefined;
    while (!gap) {
      const page = await client.call('message.list', { space: view.space, channel, before, limit: 200 });
      gap = page.gaps[0];
      before = page.messages[0]?.id;
      if (!before) break;
    }
    expect(gap?.before).toBeTruthy();
    const page = await client.call('message.list', { space: view.space, channel, before: gap!.before, limit: 200 });
    expect(page.gaps).toEqual([]);
    expect(page.messages.some((m) => m.id === gap!.after)).toBe(true);
  }, 10_000);

  it('입장·퇴장 휘발성 채널은 입장 전엔 비어 있고, 퇴장하면 만료 알림으로 지운다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('비밀-작전').channel;
    expect((await client.call('message.list', { space: view.space, channel })).messages).toEqual([]);
    await expect(client.call('message.send', { space: view.space, channel, text: '안녕' })).rejects.toMatchObject({ code: ErrorCode.Forbidden });

    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    await client.call('channel.join', { space: view.space, channel });
    const after = await client.call('message.list', { space: view.space, channel });
    expect(after.messages.length).toBeGreaterThan(0);
    expect(after.messages.every((m) => m.volatile && m.expires_at)).toBe(true);

    await client.call('message.send', { space: view.space, channel, text: '비밀 내용' });
    await client.call('channel.leave', { space: view.space, channel });
    // 알림은 묶어서 나중에 옵니다. 만료 알림까지 올 때를 기다립니다.
    await vi.waitFor(() => expect(batches.flat().some((n) => n.type === 'message.expired')).toBe(true));
    const all = batches.flat();
    const created = all.find((n) => n.type === 'message.new');
    expect(created && created.type === 'message.new' && created.notify.body).toBeUndefined();
    expect(created && created.type === 'message.new' && created.notify.title).not.toContain('비밀 내용');
    expect(all.some((n) => n.type === 'message.expired')).toBe(true);
  });

  it('휘발성 채널에도 첨부를 보내고, 저장은 allow_volatile 확인이 있어야 한다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('비밀-작전').channel;
    await client.call('channel.join', { space: view.space, channel });
    const { id } = await client.call('message.send', { space: view.space, channel, text: '', attachments: [{ path: '/tmp/작전.png' }] });
    const list = await client.call('message.list', { space: view.space, channel });
    const att = list.messages.find((m) => m.id === id)!.attachments[0];
    expect(att.downloaded).toBe(true);
    const save = { space: view.space, blob: att.blob, key_from: id, path: '/tmp/out.png' };
    await expect(client.call('blob.save', save)).rejects.toMatchObject({ code: ErrorCode.Conflict });
    await expect(client.call('blob.save', { ...save, allow_volatile: true })).resolves.toEqual({});
  });

  it('일반 채널 알림에는 본문이 있고, 반응·수정·삭제가 updated로 온다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('잡담').channel;
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    const { id } = await client.call('message.send', { space: view.space, channel, text: '공지 확인!' });
    await client.call('message.react', { space: view.space, channel, id, emoji: '👍' });
    await client.call('message.edit', { space: view.space, channel, id, text: '공지 확인함' });
    await client.call('message.delete', { space: view.space, channel, id });
    await vi.waitFor(() => expect(batches.flat().filter((n) => n.type === 'message.updated')).toHaveLength(3));
    const all = batches.flat();
    const created = all.find((n) => n.type === 'message.new');
    expect(created && created.type === 'message.new' && created.notify.body).toBe('공지 확인!');
    const updates = all.filter((n) => n.type === 'message.updated');
    expect(updates).toHaveLength(3);
    const last = updates.at(-1);
    expect(last && last.type === 'message.updated' && last.message.deleted).toBe(true);
    // 알림은 프레임 단위로 묶임
    expect(batches.length).toBeLessThan(all.length);
  });

  it('보관 노드를 지정하면 목록과 커버리지에 따로 세고, 해제하면 빠진다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const space = view.space;
    expect(await client.call('archive.list', { space })).toEqual([]);
    await expect(client.call('archive.designate', { space, url: 'https://example.com' })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    const { url } = await client.call('archive.offer', { name: '동아리 보관함' });
    const a = await client.call('archive.designate', { space, url, name: '동아리 보관함' });
    expect(a).toMatchObject({ name: '동아리 보관함', online: true, private_channels: [] });
    expect((await client.call('archive.list', { space })).map((x) => x.device)).toEqual([a.device]);
    const cov = await client.call('seed.coverage', { space });
    expect(cov.find((c) => c.channel === ch('잡담').channel)?.online_archives).toBe(1);
    await client.call('archive.revoke', { space, device: a.device });
    expect(await client.call('archive.list', { space })).toEqual([]);
    await expect(client.call('archive.revoke', { space, device: a.device })).rejects.toMatchObject({ code: ErrorCode.NotFound });
  });

  it('잘못된 입력과 모르는 메서드는 오류 코드로 거절한다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    await expect(client.call('message.send', { space: view.space, channel: ch('잡담').channel, text: '' })).rejects.toBeInstanceOf(RpcError);
    await expect(client.call('message.send', { space: view.space, channel: ch('잡담').channel, text: 'x'.repeat(4001) })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    await expect(t.call('nope.method', {})).rejects.toMatchObject({ code: ErrorCode.MethodNotFound });
    await expect(client.call('space.get', { space: 'missing' })).rejects.toMatchObject({ code: ErrorCode.NotFound });
  });

  it('신원 없이 시작해 만든다', async () => {
    const { client } = setup({ noIdentity: true });
    await expect(client.call('identity.get', {})).rejects.toMatchObject({ code: ErrorCode.NoIdentity });
    await client.call('identity.create', { name: '테스트' });
    await expect(client.call('identity.create', { name: '또' })).rejects.toMatchObject({ code: ErrorCode.Conflict });
    expect((await client.call('identity.get', {})).profile.name).toBe('테스트');
  });

  it('음성 참가는 32바이트 미디어 키와 세대를 주고, Tor에서는 참가한 P2P 전용 채널만 WebRTC, 나머지는 Tor 통화', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    // 앱은 늘 Tor로 돕니다. MOCK의 '일반 음성'은 통화 화면을 볼 수 있게 참가한 P2P 전용 채널입니다.
    expect(await client.call('net.status', {})).toMatchObject({ mode: 'tor', listen_addrs: [] });
    expect(ch('일반 음성')).toMatchObject({ p2p: true, p2p_joined: true });
    const r = await client.call('voice.join', { space: view.space, channel: ch('일반 음성').channel });
    expect(atob(r.media_key)).toHaveLength(32);
    expect(r.participants.some((p) => p.screen)).toBe(true);
    expect(r.topology).toBe('mesh');
    expect(r.ice_servers).toEqual([]);
    expect(r).not.toHaveProperty('ice_transport_policy');
    expect(r).not.toHaveProperty('relay_screen');
    expect(r).not.toHaveProperty('relay_camera');
    // P2P 전용이 아닌 채널은 Tor 통화(WebRTC 없이 소리만, ICE 서버 없음)입니다.
    const t = await client.call('voice.join', { space: view.space, channel: ch('게임 음성').channel });
    expect(t.topology).toBe('tor');
    expect(t.ice_servers).toEqual([]);
  });

  it('STUN 서버 목록은 기본 목록에서 시작하고, host:port로 맞춰 저장하며, null이면 기본 목록으로 돌아간다', async () => {
    const { client } = setup();
    const first = await client.call('net.stun_servers', {});
    expect(first.default).toBe(true);
    expect(first.servers).toContain('stun.cloudflare.com:3478');
    await client.call('net.set_stun_servers', { servers: ['stun:S.example:3479', 's.example:3479', '198.51.100.7'] });
    expect(await client.call('net.stun_servers', {})).toEqual({ servers: ['s.example:3479', '198.51.100.7:3478'], default: false });
    for (const bad of ['turn:t.example:3478', 'u:p@s.example', 's.example:0']) {
      await expect(client.call('net.set_stun_servers', { servers: [bad] })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    }
    await client.call('net.set_stun_servers', { servers: [] });
    expect(await client.call('net.stun_servers', {})).toEqual({ servers: [], default: false });
    await client.call('net.set_stun_servers', { servers: null });
    expect((await client.call('net.stun_servers', {})).default).toBe(true);
  });

  it('추방 금지·차단·신고를 계약대로 처리한다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    // 추방 금지 목록(예시 하나) → 멤버 하나 추방 금지 → 목록에 생기고 멤버에서 빠짐
    const before = await client.call('member.bans', { space: view.space });
    expect(before).toHaveLength(1);
    expect(before[0]).toMatchObject({ reason: '광고 도배' });
    const target = view.members.find((m) => m.user !== view.owner && !m.automation)!;
    await client.call('member.ban', { space: view.space, user: target.user, reason: '규칙 위반', delete_recent: true });
    const bans = await client.call('member.bans', { space: view.space });
    expect(bans.map((b) => b.user)).toContain(target.user);
    expect(bans.find((b) => b.user === target.user)).toMatchObject({ reason: '규칙 위반', profile: { name: target.profile.name } });
    expect((await client.call('member.list', { space: view.space })).some((m) => m.user === target.user)).toBe(false);
    await client.call('member.unban', { space: view.space, user: target.user });
    expect((await client.call('member.bans', { space: view.space })).some((b) => b.user === target.user)).toBe(false);
    // 이 크루 소유자는 나: 자기 자신은 밴할 수 없음(데몬과 같이 소유자 검사보다 먼저)
    expect(view.owner).toBe((await client.call('identity.get', {})).user);
    await expect(client.call('member.ban', { space: view.space, user: view.owner })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    // 이미 밴한 사람을 다시 밴하면 충돌
    const second = view.members.find((m) => m.user !== view.owner && m.user !== target.user && !m.automation)!;
    await client.call('member.ban', { space: view.space, user: second.user });
    await expect(client.call('member.ban', { space: view.space, user: second.user })).rejects.toMatchObject({ code: ErrorCode.Conflict });
    await client.call('member.unban', { space: view.space, user: second.user });

    // 차단: 목록과 메시지·멤버 표시
    // 예시 데이터에는 이미 차단한 사람이 하나 있습니다.
    const pre = new Set((await client.call('user.blocked', {})).map((b) => b.user));
    expect(pre.size).toBe(1);
    const page = await client.call('message.list', { space: view.space, channel: ch('잡담').channel, limit: 50 });
    // 위에서 밴했다 푼 사람은 멤버가 아니므로 지금 멤버인 사람을 고릅니다.
    const members = new Set((await client.call('member.list', { space: view.space })).map((m) => m.user));
    const author = page.messages.find((m) => m.author !== view.owner && !pre.has(m.author) && members.has(m.author))!.author;
    await client.call('user.block', { user: author });
    expect((await client.call('user.blocked', {})).map((b) => b.user)).toContain(author);
    const again = await client.call('message.list', { space: view.space, channel: ch('잡담').channel, limit: 50 });
    expect(again.messages.filter((m) => m.author === author).every((m) => m.author_blocked)).toBe(true);
    expect(again.messages.filter((m) => m.author !== author && !pre.has(m.author)).some((m) => m.author_blocked)).toBe(false);
    expect((await client.call('member.list', { space: view.space })).find((m) => m.user === author)?.blocked).toBe(true);
    await client.call('user.unblock', { user: author });
    expect((await client.call('user.blocked', {})).map((b) => b.user)).not.toContain(author);

    // 신고 → 운영진 알림(내용 없음) → 목록 → 처리
    const msg = again.messages.find((m) => !m.deleted && m.author !== view.owner)!;
    await expect(client.call('message.report', { space: view.space, channel: ch('잡담').channel, message: msg.id, reason: ' ' })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    await expect(
      client.call('message.report', { space: view.space, channel: ch('잡담').channel, message: msg.id, reason: 'x'.repeat(513) }),
    ).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    const { report_id } = await client.call('message.report', { space: view.space, channel: ch('잡담').channel, message: msg.id, reason: '욕설' });
    // 같은 메시지를 다시 신고하면 같은 신고 ID
    expect((await client.call('message.report', { space: view.space, channel: ch('잡담').channel, message: msg.id, reason: '욕설' })).report_id).toBe(report_id);
    await vi.waitFor(() => expect(batches.flat().some((n) => n.type === 'report.new')).toBe(true));
    const note = batches.flat().find((n) => n.type === 'report.new');
    expect(note).toEqual({ type: 'report.new', space: view.space, report_id });
    const open = await client.call('report.list', { space: view.space, status: 'open' });
    expect(open[0]).toMatchObject({ report_id, message: msg.id, reason: '욕설', status: 'open', resolution: null });
    await client.call('report.resolve', { space: view.space, report_id, action: 'delete_message', note: '확인함' });
    const resolved = await client.call('report.list', { space: view.space, status: 'resolved' });
    // 처리 결과 글은 데몬과 같이 `동작: 메모`
    expect(resolved.find((r) => r.report_id === report_id)).toMatchObject({ status: 'resolved', resolution: 'delete_message: 확인함' });
    await expect(client.call('report.resolve', { space: view.space, report_id, action: 'dismiss' })).rejects.toMatchObject({ code: ErrorCode.Conflict });
    const after = await client.call('message.list', { space: view.space, channel: ch('잡담').channel, limit: 50 });
    expect(after.messages.find((m) => m.id === msg.id)?.deleted).toBe(true);

    // 신고하며 숨기면 이 기기 목록에서 빠지고, 자기 메시지·1:1 대화는 신고할 수 없음
    const other = after.messages.find((m) => !m.deleted && m.author !== view.owner && m.id !== msg.id)!;
    await client.call('message.report', { space: view.space, channel: ch('잡담').channel, message: other.id, reason: '스팸', hide: true });
    const hidden = await client.call('message.list', { space: view.space, channel: ch('잡담').channel, limit: 50 });
    expect(hidden.messages.some((m) => m.id === other.id)).toBe(false);
    const { id: mine } = await client.call('message.send', { space: view.space, channel: ch('잡담').channel, text: '내 글' });
    await expect(client.call('message.report', { space: view.space, channel: ch('잡담').channel, message: mine, reason: '?' })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
  });

  it('모더레이션 권한이 없으면 신고 목록·추방 금지를 거절한다', async () => {
    const { client } = setup();
    const spaces = await client.call('space.list', {});
    const study = spaces.find((s) => s.name === '러스트 스터디')!;
    await expect(client.call('report.list', { space: study.space })).rejects.toMatchObject({ code: ErrorCode.Forbidden });
    await expect(client.call('member.bans', { space: study.space })).rejects.toMatchObject({ code: ErrorCode.Forbidden });
  });

  it('연락처 링크를 붙여 넣으면 바로 임시 연락처와 1대1 대화가 생기고, 친구 추가·수락으로 친구가 된다', async () => {
    const { client, t } = setup();
    const link = await client.call('friend.link.create', { expires_in_secs: 3600, single_use: true });
    expect(link.url.startsWith(`${WEB_BASE}#friend/`)).toBe(true);
    await expect(client.call('friend.add', { url: 'https://example.com/' + 'x'.repeat(40) })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    // 웹 초대 페이지가 앱에 넘기는 꼴(yangpa://)도 받습니다.
    const old = await client.call('friend.add', { url: 'yangpa://friend/' + 'A'.repeat(40) });
    const added = await client.call('friend.add', { url: link.url });
    const friends = await client.call('friend.list', {});
    const contact = friends.find((f) => f.user === added.user)!;
    expect(contact).toMatchObject({ kind: 'temporary', proposed: false });
    expect(friends.some((f) => f.user === old.user)).toBe(true);
    expect((await client.call('space.list', {})).some((s) => s.space === contact.space)).toBe(true);

    expect(await client.call('friend.propose', { user: added.user })).toEqual({ status: 'requested' });
    expect((await client.call('friend.list', {})).find((f) => f.user === added.user)?.proposed).toBe(true);

    // 임시 연락처가 보낸 친구 추가를 수락하면 새 대화 없이 지금 대화가 이어집니다.
    const temp = friends.find((f) => f.kind === 'temporary' && f.user !== added.user && f.user !== old.user)!;
    t.backend.requests.push({ request_id: 'req-t', user: temp.user, profile: temp.profile, received_at: Date.now() });
    const r = await client.call('friend.accept', { request_id: 'req-t' });
    expect(r.space).toBe(temp.space);
    expect((await client.call('friend.list', {})).find((f) => f.user === temp.user)).toMatchObject({ kind: 'friend', proposed: false });
  });

  it('친구를 끊으면 1대1 공간도 사라진다', async () => {
    const { client } = setup();
    const friend = (await client.call('friend.list', {})).find((f) => f.kind === 'friend')!;
    await client.call('friend.remove', { user: friend.user });
    expect((await client.call('space.list', {})).some((s) => s.space === friend.space)).toBe(false);
  });

  it('1대1을 크루로 바꾼 뒤 대화를 열면 둘만의 1대1 대화가 새로 생긴다', async () => {
    const { client } = setup();
    const f = (await client.call('friend.list', {})).find((x) => x.kind === 'friend')!;
    expect(await client.call('friend.open', { user: f.user })).toEqual({ space: f.space });
    await client.call('crew.convert_accept', { space: f.space });
    const { space } = await client.call('friend.open', { user: f.user });
    expect(space).not.toBe(f.space);
    expect((await client.call('space.get', { space })).kind).toBe('space');
    expect((await client.call('space.get', { space: f.space })).kind).toBe('crew');
    expect(await client.call('friend.open', { user: f.user })).toEqual({ space });
  });

  it('링크를 폐기하면 그 링크로 만난 임시 연락처가 만료되고 대화는 읽기만 된다', async () => {
    const { client, t } = setup();
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    const [link] = await client.call('friend.link.list', {});
    const temp = (await client.call('friend.list', {})).find((f) => f.kind === 'temporary')!;
    await client.call('friend.link.revoke', { token_id: link.token_id });
    expect((await client.call('friend.list', {})).find((f) => f.user === temp.user)?.kind).toBe('expired');
    await vi.waitFor(() => expect(batches.flat()).toContainEqual({ type: 'friend.updated', user: temp.user }));
    await expect(client.call('friend.propose', { user: temp.user })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    const view = await client.call('space.get', { space: temp.space });
    await expect(client.call('message.send', { space: temp.space, channel: view.channels[0].channel, text: '안녕' })).rejects.toMatchObject({
      code: ErrorCode.Forbidden,
    });
  });

  it('Tor 전용: 모드 바꾸기·토르망 메서드와 없어진 망 설정(IP 숨기기·저전력·상시 노드·통화 ICE 서버·TURN) 메서드가 없다', async () => {
    const { client } = setup();
    const st = await client.call('net.status', {});
    expect(st).not.toHaveProperty('tor_open');
    expect(st).not.toHaveProperty('tor_mode');
    expect(st).not.toHaveProperty('low_power');
    expect(st).not.toHaveProperty('hide_ip');
    expect(st.onion_addr).toMatch(/^\/onion3\//);
    const call = client.call.bind(client) as (method: string, params: unknown) => Promise<unknown>;
    for (const m of [
      'net.set_tor',
      'net.set_tor_open',
      'net.set_hide_ip',
      'net.set_low_power',
      'net.add_bootstrap',
      'net.remove_bootstrap',
      'net.bootstrap_list',
      'net.set_public_nodes',
      'net.ice_servers',
      'net.set_ice_servers',
      'net.turn',
      'net.set_turn',
      'net.check_turn',
    ]) {
      await expect(call(m, { enabled: false })).rejects.toMatchObject({ code: ErrorCode.MethodNotFound });
    }
  });

  it('P2P 전용 채널 하나는 잠긴 채로 시작하고, 참가·그만두기는 이 기기의 참가 표시만 바꾼다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const p2p = ch('직접-대화');
    expect(p2p).toMatchObject({ p2p: true, p2p_joined: false, settings: { kind: 'text' } });
    expect(ch('잡담')).toMatchObject({ p2p: false, p2p_joined: false });
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    const args = { space: view.space, channel: p2p.channel };
    await client.call('channel.p2p_join', args);
    await client.call('channel.p2p_join', args);
    let again = (await client.call('channel.list', { space: view.space })).find((c) => c.channel === p2p.channel)!;
    expect(again.p2p_joined).toBe(true);
    await vi.waitFor(() => expect(batches.flat()).toContainEqual({ type: 'space.updated', space: view.space }));
    await client.call('channel.p2p_leave', args);
    await client.call('channel.p2p_leave', args);
    again = (await client.call('channel.list', { space: view.space })).find((c) => c.channel === p2p.channel)!;
    expect(again).toMatchObject({ p2p: true, p2p_joined: false });
    // P2P 전용이 아닌 채널에는 참가할 수 없습니다.
    await expect(client.call('channel.p2p_join', { space: view.space, channel: ch('잡담').channel })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
  });

  it('set_p2p는 양쪽으로 바꾸고, 바뀌면 이 기기의 참가 표시를 지운다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const read = async (id: string) => (await client.call('channel.list', { space: view.space })).find((c) => c.channel === id)!;
    const p2p = ch('직접-대화').channel;
    await client.call('channel.p2p_join', { space: view.space, channel: p2p });
    // Tor로 되돌리면 참가 기록도 사라집니다.
    await client.call('channel.set_p2p', { space: view.space, channel: p2p, enabled: false });
    expect(await read(p2p)).toMatchObject({ p2p: false, p2p_joined: false });
    // 다시 P2P 전용이 되면 경고부터 다시 받아야 합니다.
    await client.call('channel.set_p2p', { space: view.space, channel: p2p, enabled: true });
    expect(await read(p2p)).toMatchObject({ p2p: true, p2p_joined: false });
    await client.call('channel.p2p_join', { space: view.space, channel: p2p });
    // 지금 값과 같으면 아무것도 바꾸지 않습니다.
    await client.call('channel.set_p2p', { space: view.space, channel: p2p, enabled: true });
    expect(await read(p2p)).toMatchObject({ p2p: true, p2p_joined: true });
    // 통화 채널은 되고, 포럼·카테고리는 안 됩니다.
    await client.call('channel.set_p2p', { space: view.space, channel: ch('일반 음성').channel, enabled: true });
    expect((await read(ch('일반 음성').channel)).p2p).toBe(true);
    await expect(client.call('channel.set_p2p', { space: view.space, channel: ch('공략-게시판').channel, enabled: true })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    const cat = view.channels.find((c) => c.settings.kind === 'category')!;
    await expect(client.call('channel.set_p2p', { space: view.space, channel: cat.channel, enabled: true })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    // 관리 권한이 없는 크루에서는 바꿀 수 없습니다.
    const study = (await client.call('space.list', {})).find((s) => s.name === '러스트 스터디')!;
    const general = (await client.call('space.get', { space: study.space })).channels[0];
    await expect(client.call('channel.set_p2p', { space: study.space, channel: general.channel, enabled: true })).rejects.toMatchObject({
      code: ErrorCode.Forbidden,
    });
  });

  it('p2p: true로 만든 채널은 P2P 전용이고 만든 기기도 참가 전이며, 포럼은 P2P로 만들 수 없다', async () => {
    const { client } = setup();
    const { view } = await crew(client);
    const { channel } = await client.call('channel.create', { space: view.space, name: '모임-통화', kind: 'voice', p2p: true });
    const made = (await client.call('channel.list', { space: view.space })).find((c) => c.channel === channel)!;
    expect(made).toMatchObject({ p2p: true, p2p_joined: false, settings: { kind: 'voice' } });
    const before = (await client.call('channel.list', { space: view.space })).length;
    await expect(client.call('channel.create', { space: view.space, name: '글', kind: 'forum', p2p: true })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    // 거절되면 채널도 만들지 않습니다.
    expect(await client.call('channel.list', { space: view.space })).toHaveLength(before);
  });

  it('역할 표시 설정은 역할마다의 기본값과 다를 때만 role_display에 남고, @everyone에는 볼 수 있는 역할을 줄 수 없다', async () => {
    const { client } = setup();
    const { view } = await crew(client);
    const [a, b] = view.roles.filter((r) => r.id !== EVERYONE_ROLE);
    const display = async () => (await client.call('space.get', { space: view.space })).role_display ?? {};
    await client.call('role.update', { space: view.space, role: a.id, visible_to: [b.id], see_admins: true });
    expect((await display())[a.id]).toEqual({ visible_to: [b.id], see_admins: true });
    await client.call('role.update', { space: view.space, role: a.id, visible_to: null });
    expect((await display())[a.id]).toEqual({ visible_to: null, see_admins: true });
    await client.call('role.update', { space: view.space, role: a.id, see_admins: false });
    expect(await display()).not.toHaveProperty(a.id);
    // @everyone은 관리자 보기가 기본으로 켜져 있어 끈 것만 남습니다.
    await client.call('role.update', { space: view.space, role: EVERYONE_ROLE, see_admins: false });
    expect((await display())[EVERYONE_ROLE]).toEqual({ visible_to: null, see_admins: false });
    await expect(client.call('role.update', { space: view.space, role: EVERYONE_ROLE, visible_to: [a.id] })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
    await expect(client.call('role.update', { space: view.space, role: a.id, visible_to: ['nope'] })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    const study = (await client.call('space.list', {})).find((s) => s.name === '러스트 스터디')!;
    await expect(client.call('role.update', { space: study.space, role: EVERYONE_ROLE, see_admins: false })).rejects.toMatchObject({
      code: ErrorCode.Forbidden,
    });
  });

  it('채널 권한 덮어쓰기는 목록 전체를 바꾸고, 계약대로 잘못된 목록을 거절하며 빈 항목은 버린다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const role = view.roles.find((r) => r.id !== EVERYONE_ROLE)!;
    const everyone = { type: 'role' as const, id: EVERYONE_ROLE };
    const sp = view.space;
    const channel = ch('잡담').channel;
    const read = async () => (await client.call('channel.list', { space: sp })).find((c) => c.channel === channel)!.settings.overwrites;
    const ok = [
      { target: everyone, allow: 0, deny: 1 },
      { target: { type: 'role' as const, id: role.id }, allow: 3, deny: 0 },
      { target: { type: 'member' as const, id: view.members[1].user }, allow: 0, deny: 0 },
    ];
    await client.call('channel.update', { space: sp, channel, overwrites: ok });
    expect(await read()).toEqual(ok.slice(0, 2));
    const bad: unknown[][] = [
      // 비트 11은 크루 STUN 사용, 12는 예전 크루 TURN 사용(다시 쓰지 않음), 13은 많이 멘션하기라 쓰지 않는 14를 씁니다.
      [{ target: everyone, allow: 2 ** 14, deny: 0 }],
      [{ target: everyone, allow: 1, deny: 1 }],
      [ok[0], { target: everyone, allow: 2, deny: 0 }],
      [{ target: { type: 'role', id: 'nope' }, allow: 1, deny: 0 }],
      [{ target: { type: 'member', id: 'stranger' }, allow: 1, deny: 0 }],
      Array.from({ length: 101 }, () => ok[0]),
    ];
    for (const overwrites of bad) {
      await expect(client.call('channel.update', { space: sp, channel, overwrites: overwrites as never })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    }
    expect(await read()).toEqual(ok.slice(0, 2));
    await expect(client.call('channel.update', { space: sp, channel, overwrites: [], private_roles: [] })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    const made = await client.call('channel.create', { space: sp, name: '묶음', kind: 'category', overwrites: [ok[0]] });
    expect((await client.call('channel.list', { space: sp })).find((c) => c.channel === made.channel)!.settings.overwrites).toEqual([ok[0]]);
    // 지운 역할을 대상으로 한 덮어쓰기도 함께 사라집니다.
    await client.call('role.delete', { space: sp, role: role.id });
    expect(await read()).toEqual([ok[0]]);
  });

  it('내게 없는 권한 비트는 채널 덮어쓰기에서 바꿀 수 없다', async () => {
    const { client } = setup();
    const study = (await client.call('space.list', {})).find((s) => s.name === '러스트 스터디')!;
    const general = (await client.call('space.get', { space: study.space })).channels[0].channel;
    const everyone = { type: 'role' as const, id: EVERYONE_ROLE };
    // 보기(비트 0)는 내게 있고, 추방(비트 19)은 없습니다.
    await client.call('channel.update', { space: study.space, channel: general, overwrites: [{ target: everyone, allow: 0, deny: 1 }] });
    await expect(
      client.call('channel.update', { space: study.space, channel: general, overwrites: [{ target: everyone, allow: 2 ** 19, deny: 1 }] }),
    ).rejects.toMatchObject({ code: ErrorCode.Forbidden });
  });

  it('신뢰 스위치: 사람·크루 단위로 켜고 끄며, 신뢰 방향에 따라 연결 방식이 바뀌고 IP는 싣지 않는다', async () => {
    const { t, client } = setup();
    const { view } = await crew(client);
    const u = t.backend.userIds;
    const events: Notification[] = [];
    t.subscribe((b) => events.push(...b));
    const get = (user: string) => client.call('trust.get', { user });

    // 민지는 이미 나를 신뢰합니다(내게만 민지의 IP가 보임). 도윤은 서로 Tor, 꺼진 사람은 오프라인.
    expect(await get(u.minji)).toEqual({ user: u.minji, trusted: false, via_crews: [], connection: 'one_way_peer_exposed' });
    expect((await get(u.doyun)).connection).toBe('tor');
    expect((await get(u.siwoo)).connection).toBe('offline');
    expect(JSON.stringify(await get(u.minji))).not.toMatch(/\d{1,3}(\.\d{1,3}){3}|\/ip[46]\//);

    await client.call('trust.set_user', { user: u.minji, enabled: true });
    expect(await get(u.minji)).toMatchObject({ trusted: true, connection: 'p2p' });
    await client.call('trust.set_user', { user: u.doyun, enabled: true });
    expect(await get(u.doyun)).toMatchObject({ trusted: true, connection: 'one_way_me_exposed' });
    await client.call('trust.set_user', { user: u.doyun, enabled: false });
    expect(await get(u.doyun)).toMatchObject({ trusted: false, connection: 'tor' });

    // 나 자신, 함께 쓰는 공간이 없는 사람은 켤 수 없고, 끄기는 늘 됩니다.
    await expect(client.call('trust.set_user', { user: t.backend.me, enabled: true })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    const stranger = (await client.call('friend.requests', {}))[0].user;
    await expect(client.call('trust.set_user', { user: stranger, enabled: true })).rejects.toMatchObject({ code: ErrorCode.NotFound });
    await expect(client.call('trust.set_user', { user: stranger, enabled: false })).resolves.toEqual({});

    // 크루 신뢰: 보기의 trusted와 space.updated, 그 크루 멤버에게 via_crews로 보입니다.
    expect(view.trusted).toBe(false);
    await client.call('trust.set_space', { space: view.space, enabled: true });
    expect((await client.call('space.get', { space: view.space })).trusted).toBe(true);
    await vi.waitFor(() => expect(events).toContainEqual({ type: 'space.updated', space: view.space }));
    expect(await get(u.doyun)).toMatchObject({ trusted: false, via_crews: [view.space], connection: 'one_way_me_exposed' });
    // 그 크루 밖의 사람(친구 요청만 보낸 사람)에게는 닿지 않습니다.
    expect(await get(stranger)).toMatchObject({ via_crews: [], connection: 'offline' });

    // 1대1 공간은 크루 신뢰 대상이 아닙니다.
    const dm = (await client.call('space.list', {})).find((s) => s.kind === 'space')!;
    await expect(client.call('trust.set_space', { space: dm.space, enabled: true })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });

    await client.call('trust.set_space', { space: view.space, enabled: false });
    expect((await client.call('space.get', { space: view.space })).trusted).toBe(false);
    expect(await get(u.doyun)).toMatchObject({ via_crews: [], connection: 'tor' });
  });

  it('크루 STUN 서버: 관리하는 사람만 목록을 보고 바꾸며, 멤버는 쓸 수 있는지만 보고 이 기기에서 쓸지 고른다', async () => {
    const { client, t } = setup();
    const { view } = await crew(client);
    const events: Notification[] = [];
    t.subscribe((b) => events.push(...b));
    // 게임 모임은 내가 소유자라 목록을 보고, 관리자라 역할로도 쓸 수 있습니다.
    expect(view.crew_servers).toEqual({ stun: ['stun.game-crew.example:3478'] });
    expect(view.crew_stun).toBe(true);
    expect(view).not.toHaveProperty('crew_turn');
    expect(view.use_crew_servers).toBeUndefined();

    await client.call('crew.set_servers', { space: view.space, stun: ['stun:S.crew.example', 's.crew.example:3478'] });
    let v = await client.call('space.get', { space: view.space });
    expect(v.crew_servers).toEqual({ stun: ['s.crew.example:3478'] });
    await vi.waitFor(() => expect(events).toContainEqual({ type: 'space.updated', space: view.space }));
    // TURN 줄은 받지 않습니다.
    for (const stun of [['turn:crew:pw@turn.crew.example'], ['turns:turn.crew.example']]) {
      await expect(client.call('crew.set_servers', { space: view.space, stun })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    }
    await expect(
      client.call('crew.set_servers', { space: view.space, stun: Array.from({ length: 9 }, (_, i) => `s${i}.example.org`) }),
    ).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    // STUN을 비우면 쓸 수 있는 크루 서버도 사라집니다.
    await client.call('crew.set_servers', { space: view.space, stun: [] });
    v = await client.call('space.get', { space: view.space });
    expect(v.crew_stun).toBeUndefined();

    await client.call('crew.use_servers', { space: view.space, enabled: true });
    expect((await client.call('space.get', { space: view.space })).use_crew_servers).toBe(true);
    await client.call('crew.use_servers', { space: view.space, enabled: false });
    expect((await client.call('space.get', { space: view.space })).use_crew_servers).toBeUndefined();

    // 러스트 스터디는 일반 멤버: 목록은 오지 않고, 역할(크루 STUN 사용)로 쓸 수 있는 STUN만 알립니다.
    const studyId = (await client.call('space.list', {})).find((s) => s.name === '러스트 스터디')!.space;
    const study = await client.call('space.get', { space: studyId });
    expect(study.crew_servers).toBeUndefined();
    expect(study.crew_stun).toBe(true);
    expect(JSON.stringify(study)).not.toContain('stun.rust-study.example');
    await expect(client.call('crew.set_servers', { space: studyId, stun: [] })).rejects.toMatchObject({ code: ErrorCode.Forbidden });
    await client.call('crew.use_servers', { space: studyId, enabled: true });
    expect((await client.call('space.get', { space: studyId })).use_crew_servers).toBe(true);

    // 1대1 공간에는 크루 서버가 없습니다.
    const dm = (await client.call('space.list', {})).find((s) => s.kind === 'space')!;
    await expect(client.call('crew.set_servers', { space: dm.space, stun: [] })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    await expect(client.call('crew.use_servers', { space: dm.space, enabled: true })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    expect((await client.call('space.get', { space: dm.space })).crew_servers).toBeUndefined();
  });

  it('만료된 연락처는 함께 쓰는 크루가 있을 때만 신뢰할 수 있다', async () => {
    const { t, client } = setup();
    const u = t.backend.userIds;
    // 예린은 만료된 연락처지만 게임 모임 멤버라 켤 수 있습니다.
    await expect(client.call('trust.set_user', { user: u.yerin, enabled: true })).resolves.toEqual({});
    await client.call('trust.set_user', { user: u.yerin, enabled: false });
    const { view } = await crew(client);
    await client.call('crew.leave', { space: view.space });
    await expect(client.call('trust.set_user', { user: u.yerin, enabled: true })).rejects.toMatchObject({ code: ErrorCode.NotFound });
  });
});

describe('MOCK 익명 채널', () => {
  it('익명 채널의 글은 작성자·기기를 가리고 내 글에만 mine을 붙이며, 알림 제목도 익명이다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const anon = ch('익명-건의함');
    expect(anon.anonymous).toBe(true);
    const { messages } = await client.call('message.list', { space: view.space, channel: anon.channel, limit: 50 });
    expect(messages.length).toBeGreaterThan(0);
    expect(new Set(messages.map((m) => m.author))).toEqual(new Set([ANONYMOUS_USER]));
    expect(new Set(messages.map((m) => m.author_device))).toEqual(new Set([ANONYMOUS_DEVICE]));
    expect(messages.every((m) => m.anonymous)).toBe(true);
    // 시드에 내 글이 섞여 있어 내 것만 mine으로 알 수 있습니다.
    expect(messages.some((m) => m.mine)).toBe(true);
    expect(messages.some((m) => !m.mine)).toBe(true);

    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    await client.call('message.send', { space: view.space, channel: anon.channel, text: '건의합니다' });
    await vi.waitFor(() => expect(batches.flat().some((n) => n.type === 'message.new')).toBe(true));
    const created = batches.flat().find((n) => n.type === 'message.new');
    if (created?.type !== 'message.new') throw new Error('새 메시지 알림이 없음');
    expect(created.message).toMatchObject({ author: ANONYMOUS_USER, author_device: ANONYMOUS_DEVICE, anonymous: true, mine: true });
    expect(created.notify.title.startsWith('익명')).toBe(true);
  });

  it('익명은 크루의, 카테고리가 아닌 채널에서 채널 관리 권한이 있을 때만 켤 수 있다', async () => {
    const { client } = setup();
    const { spaces, view } = await crew(client);
    const { channel } = await client.call('channel.create', { space: view.space, name: '익명-포럼', kind: 'forum', anonymous: true });
    expect((await client.call('channel.list', { space: view.space })).find((c) => c.channel === channel)?.anonymous).toBe(true);
    await expect(client.call('channel.create', { space: view.space, name: '묶음', kind: 'category', anonymous: true })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
      message: '카테고리는 익명으로 만들 수 없습니다',
    });
    const cat = view.channels.find((c) => c.settings.kind === 'category')!;
    await expect(client.call('channel.update', { space: view.space, channel: cat.channel, anonymous: true })).rejects.toMatchObject({
      message: '카테고리는 익명으로 만들 수 없습니다',
    });
    const dm = spaces.find((s) => s.kind === 'space')!;
    await expect(client.call('channel.create', { space: dm.space, name: '익명', kind: 'text', anonymous: true })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
      message: '익명 채널은 크루에서만 만들 수 있습니다',
    });
    const study = spaces.find((s) => s.name === '러스트 스터디')!;
    const general = (await client.call('space.get', { space: study.space })).channels.find((c) => c.settings.kind === 'text')!;
    await expect(client.call('channel.update', { space: study.space, channel: general.channel, anonymous: true })).rejects.toMatchObject({
      code: ErrorCode.Forbidden,
    });
  });

  it('익명을 꺼도 켜져 있는 동안 쓴 글은 익명으로 남고, 그 뒤에 쓴 글은 이름이 보인다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const anon = ch('익명-건의함').channel;
    const before = await client.call('message.send', { space: view.space, channel: anon, text: '켜져 있을 때' });
    await client.call('channel.update', { space: view.space, channel: anon, anonymous: false });
    const after = await client.call('message.send', { space: view.space, channel: anon, text: '끈 뒤' });
    const me = (await client.call('identity.get', {})).user;
    const { messages } = await client.call('message.list', { space: view.space, channel: anon, limit: 50 });
    expect(messages.find((m) => m.id === before.id)).toMatchObject({ author: ANONYMOUS_USER, anonymous: true, mine: true });
    const plain = messages.find((m) => m.id === after.id)!;
    expect(plain).toMatchObject({ author: me, mine: true });
    expect(plain.anonymous).toBeUndefined();
    expect((await client.call('channel.list', { space: view.space })).find((c) => c.channel === anon)?.anonymous).toBeUndefined();
  });

  it('익명 통화 채널은 참가자 수만 보이고, 통화에서는 들어온 순서 번호와 내 기기의 mine을 준다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const voice = ch('익명-수다');
    expect(voice.participants).toEqual([ANONYMOUS_USER, ANONYMOUS_USER]);
    const myDevice = (await client.call('identity.get', {})).device;
    const r = await client.call('voice.join', { space: view.space, channel: voice.channel });
    expect(r.participants.map((p) => [p.user, p.anon, !!p.mine])).toEqual([
      [ANONYMOUS_USER, 1, false],
      [ANONYMOUS_USER, 2, false],
      [ANONYMOUS_USER, 3, true],
    ]);
    expect(r.participants.find((p) => p.mine)?.device).toBe(myDevice);
    expect(r.sfu_host_user).toBeUndefined();
    expect((await client.call('space.get', { space: view.space })).channels.find((c) => c.channel === voice.channel)?.participants).toEqual([
      ANONYMOUS_USER,
      ANONYMOUS_USER,
      ANONYMOUS_USER,
    ]);

    // 익명을 끄면 데몬처럼 모두 통화에서 나옵니다.
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    await client.call('channel.update', { space: view.space, channel: voice.channel, anonymous: false });
    // 참가할 때 낸 목록이 늦게 묶여 올 수 있어, 마지막 목록이 빈 목록이 될 때까지 기다립니다.
    await vi.waitFor(() => {
      const roster = batches.flat().filter((n) => n.type === 'voice.participants' && n.channel === voice.channel).at(-1);
      expect(roster?.type === 'voice.participants' && roster.participants).toEqual([]);
    });
    const off = (await client.call('channel.list', { space: view.space })).find((c) => c.channel === voice.channel)!;
    expect(off.anonymous).toBeUndefined();
    expect(off.participants).toEqual([]);
    // 다시 들어가면 이름이 보이는 평범한 통화입니다.
    const again = await client.call('voice.join', { space: view.space, channel: voice.channel });
    expect(again.participants).toEqual([expect.objectContaining({ device: myDevice, mine: true })]);
    expect(again.participants[0].anon).toBeUndefined();
    expect(again.participants[0].user).not.toBe(ANONYMOUS_USER);
  });
});

describe('MOCK 포럼 글타래', () => {
  it('글타래의 첫 메시지는 게시글 자체(ID = 글 ID, 글타래 없음)다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const forum = ch('공략-게시판').channel;
    const [first] = await client.call('forum.list_posts', { space: view.space, channel: forum, sort: 'created' });
    const { messages } = await client.call('message.list', { space: view.space, channel: forum, thread: first.id, limit: 50 });
    expect(messages[0]).toMatchObject({ id: first.id, author: first.author });
    expect(messages[0].thread).toBeUndefined();
    expect(messages.slice(1).every((m) => m.thread === first.id)).toBe(true);
  });

  it('새 글은 글타래 없는 message.new로 알리고, 익명 포럼이면 목록·알림 모두 가린다', async () => {
    const { client, t } = setup();
    const { view } = await crew(client);
    const { channel } = await client.call('channel.create', { space: view.space, name: '익명-고민', kind: 'forum', anonymous: true });
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    const { id } = await client.call('forum.create_post', { space: view.space, channel, title: '고민 있어요', text: '본문입니다', tags: [] });
    await vi.waitFor(() => expect(batches.flat().some((n) => n.type === 'message.new')).toBe(true));
    const created = batches.flat().find((n) => n.type === 'message.new');
    if (created?.type !== 'message.new') throw new Error('새 글 알림이 없음');
    expect(created.message).toMatchObject({ id, text: '본문입니다', author: ANONYMOUS_USER, anonymous: true, mine: true });
    expect(created.message.thread).toBeUndefined();
    const posts = await client.call('forum.list_posts', { space: view.space, channel, sort: 'activity' });
    expect(posts).toEqual([expect.objectContaining({ id, author: ANONYMOUS_USER, anonymous: true, mine: true })]);
    // 글 고정 같은 글 변경도 글타래 없는 message.updated로 알립니다.
    await client.call('forum.update_post', { space: view.space, channel, post: id, pinned: true });
    await vi.waitFor(() => expect(batches.flat().some((n) => n.type === 'message.updated' && n.message.id === id && !n.message.thread)).toBe(true));
  });

  it('시드 포럼은 첫 글이 대문이고, 미리보기·표지·첨부 수는 게시글 메시지에서 만든다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const forum = ch('공략-게시판');
    const posts = await client.call('forum.list_posts', { space: view.space, channel: forum.channel, sort: 'created', order: 'asc' });
    expect(posts).toHaveLength(6);
    expect(forum.featured_post).toBe(posts[0].id);
    expect(posts.filter((x) => x.featured).map((x) => x.id)).toEqual([posts[0].id]);
    expect(posts[0].preview.startsWith('# 공략 게시판 이용 안내')).toBe(true);
    expect(posts.every((x) => Array.from(x.preview).length <= 200)).toBe(true);
    // 첨부 수·표지(첫 이미지·영상)는 글타래 맨 앞의 게시글 메시지와 같습니다.
    for (const x of posts) {
      const { messages } = await client.call('message.list', { space: view.space, channel: forum.channel, thread: x.id, limit: 200 });
      const head = messages.find((m) => m.id === x.id)!;
      expect(x.attachment_count).toBe(head.attachments.length);
      const cover = head.attachments.find((a) => a.mime.startsWith('image/') || a.mime.startsWith('video/'));
      expect(x.cover?.blob).toBe(cover?.blob);
    }
    // 꾸민 두 글은 공략 이미지가 표지입니다.
    const covered = posts.filter((x) => x.cover?.name.startsWith('공략_')).map((x) => x.title);
    expect(covered.sort()).toEqual(['보스 레이드 3페이즈 노데스 공략', '초보용 골드 파밍 루트 정리']);
  });

  it('list_posts는 고정 글을 먼저 두고 방향(asc/desc)과 태그 일부/모두 일치를 따른다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('공략-게시판').channel;
    const list = (extra: object) => client.call('forum.list_posts', { space: view.space, channel, sort: 'created', ...extra });
    const desc = await list({});
    const asc = await list({ order: 'asc' });
    expect(desc[0].pinned && asc[0].pinned).toBe(true);
    const times = (xs: typeof desc) => xs.slice(1).map((x) => x.created_at);
    expect(times(desc)).toEqual([...times(desc)].sort((a, b) => b - a));
    expect(times(asc)).toEqual([...times(desc)].reverse());
    const titles = async (extra: object) => (await list({ order: 'asc', ...extra })).map((x) => x.title);
    expect(await titles({ tags: [1, 2] })).toHaveLength(4);
    expect(await titles({ tags: [1, 2], tag_match: 'all' })).toEqual(['초보용 골드 파밍 루트 정리']);
    // 예전 `tag` 하나도 `tags`에 더해 맞춥니다.
    expect(await titles({ tag: 2, tags: [1], tag_match: 'all' })).toEqual(['초보용 골드 파밍 루트 정리']);
    expect(await titles({ tag: 3 })).toEqual(['14.2 패치 이후 매칭 오래 걸리는 버그']);
  });

  it('get_post는 글 하나를 주고, 지운 글은 없는 글(1004)이며 대문에서도 빠진다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('공략-게시판').channel;
    const featured = ch('공략-게시판').featured_post!;
    const got = await client.call('forum.get_post', { space: view.space, channel, post: featured });
    const [listed] = (await client.call('forum.list_posts', { space: view.space, channel, sort: 'activity' })).filter((x) => x.id === featured);
    expect(got).toEqual(listed);
    await expect(client.call('forum.get_post', { space: view.space, channel, post: 'X'.repeat(32) })).rejects.toMatchObject({ code: ErrorCode.NotFound });

    await client.call('message.delete', { space: view.space, channel, id: featured });
    await expect(client.call('forum.get_post', { space: view.space, channel, post: featured })).rejects.toMatchObject({ code: ErrorCode.NotFound });
    const after = await client.call('space.get', { space: view.space });
    expect(after.channels.find((c) => c.channel === channel)?.featured_post).toBeUndefined();
    expect((await client.call('forum.list_posts', { space: view.space, channel, sort: 'activity' })).some((x) => x.id === featured)).toBe(false);
  });

  it('set_featured는 대문을 바꾸거나(null이면 내림) 같은 값이면 아무것도 하지 않는다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('공략-게시판').channel;
    const first = ch('공략-게시판').featured_post!;
    const other = (await client.call('forum.list_posts', { space: view.space, channel, sort: 'created' })).find((x) => x.id !== first)!.id;
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    const updates = () => batches.flat().filter((n) => n.type === 'space.updated').length;
    const featuredNow = async () => (await client.call('space.get', { space: view.space })).channels.find((c) => c.channel === channel)?.featured_post;

    // 앞서 쌓인 알림이 다 나온 뒤부터 셉니다.
    await new Promise((r) => setTimeout(r, 30));
    const base = updates();
    await client.call('forum.set_featured', { space: view.space, channel, post: first });
    await new Promise((r) => setTimeout(r, 30));
    expect(updates()).toBe(base);
    await client.call('forum.set_featured', { space: view.space, channel, post: other });
    await vi.waitFor(() => expect(updates()).toBe(base + 1));
    expect(await featuredNow()).toBe(other);
    expect((await client.call('forum.get_post', { space: view.space, channel, post: other })).featured).toBe(true);
    expect((await client.call('forum.get_post', { space: view.space, channel, post: first })).featured).toBeUndefined();
    await client.call('forum.set_featured', { space: view.space, channel, post: null });
    expect(await featuredNow()).toBeUndefined();

    await expect(client.call('forum.set_featured', { space: view.space, channel, post: 'X'.repeat(32) })).rejects.toMatchObject({ code: ErrorCode.NotFound });
    await expect(client.call('forum.set_featured', { space: view.space, channel: ch('잡담').channel, post: null })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
  });

  it('create_post는 본문 없이 첨부만 있어도 되고, 대문으로 바로 지정할 수 있으며, 빈 글은 거절한다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('공략-게시판').channel;
    const { id } = await client.call('forum.create_post', {
      space: view.space,
      channel,
      title: '스크린샷',
      text: '',
      tags: [1],
      attachments: [{ path: '/f/shot.png', name: 'shot.png' }],
      featured: true,
    });
    const got = await client.call('forum.get_post', { space: view.space, channel, post: id });
    expect(got).toMatchObject({ id, title: '스크린샷', preview: '', attachment_count: 1, featured: true, mine: true, cover: { name: 'shot.png', mime: 'image/png' } });
    expect((await client.call('space.get', { space: view.space })).channels.find((c) => c.channel === channel)?.featured_post).toBe(id);
    await expect(client.call('forum.create_post', { space: view.space, channel, title: '빈 글', text: '  ', tags: [] })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
  });
});

describe('MOCK 전송 속도·범위별 제한·다시 잇기·포럼 태그', () => {
  it('transfer.rates는 정모 사진을 받는 두 사람을 빠른 순으로 주고, 전체 올리기는 그 합 이상이다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const r = await client.call('transfer.rates', {});
    expect(r.down_bps).toBeGreaterThan(0);
    const ups = r.uploads ?? [];
    expect(ups).toHaveLength(2);
    expect(ups.every((u) => u.space === view.space && u.channel === ch('잡담').channel && u.blob === ups[0].blob)).toBe(true);
    expect(ups[0].bps).toBeGreaterThanOrEqual(ups[1].bps);
    expect(r.up_bps).toBeGreaterThanOrEqual(ups[0].bps + ups[1].bps);
    const names = ups.map((u) => view.members.find((m) => m.user === u.user)?.profile.name);
    expect(names.every(Boolean)).toBe(true);
  });

  it('rate.set_limit은 0(막음)을 받고, 음수는 거절하고, 둘 다 null이면 지운다', async () => {
    const { client } = setup();
    const { view } = await crew(client);
    const scope = { kind: 'crew' as const, space: view.space };
    const before = await client.call('rate.limits', {});
    await expect(client.call('rate.set_limit', { scope, up_bps: -1 })).rejects.toMatchObject({ code: ErrorCode.InvalidParams });
    await client.call('rate.set_limit', { scope, up_bps: 0, down_bps: null });
    expect(await client.call('rate.limits', {})).toContainEqual({ scope, up_bps: 0, down_bps: null });
    await client.call('rate.set_limit', { scope, up_bps: 2048, down_bps: null });
    expect(await client.call('rate.limits', {})).toContainEqual({ scope, up_bps: 2048, down_bps: null });
    await client.call('rate.set_limit', { scope, up_bps: null, down_bps: null });
    expect(await client.call('rate.limits', {})).toEqual(before);
  });

  it('blob.relink는 이름이 같은 파일(해시가 같은 흉내)만 받아 받은 첨부로 만들고, 다르면·휘발성이면 거절한다', async () => {
    const { client, t } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('잡담').channel;
    const page = await client.call('message.list', { space: view.space, channel, limit: 50 });
    const m = page.messages.find((x) => x.attachments.length > 0)!;
    const att = m.attachments[0];
    const base = { space: view.space, channel, message: m.id, blob: att.blob };
    await expect(client.call('blob.relink', { ...base, path: '/다른/파일.bin' })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
      message: '파일 내용이 첨부와 달라 다시 이을 수 없습니다(해시가 다름)',
    });
    await expect(client.call('blob.relink', { ...base, blob: 'Z'.repeat(64), path: `/x/${att.name}` })).rejects.toMatchObject({ code: ErrorCode.NotFound });
    const batches: Notification[][] = [];
    t.subscribe((b) => batches.push(b));
    const st = await client.call('blob.relink', { ...base, path: `/새 위치/${att.name}` });
    expect(st.have_chunks).toBe(st.total_chunks);
    await vi.waitFor(() => expect(batches.flat().some((n) => n.type === 'blob.progress' && n.blob === att.blob)).toBe(true));
    const again = await client.call('message.list', { space: view.space, channel, limit: 50 });
    expect(again.messages.find((x) => x.id === m.id)?.attachments[0].downloaded).toBe(true);

    await client.call('channel.join', { space: view.space, channel: ch('비밀-작전').channel });
    await expect(
      client.call('blob.relink', { space: view.space, channel: ch('비밀-작전').channel, message: m.id, blob: att.blob, path: `/x/${att.name}` }),
    ).rejects.toMatchObject({ code: ErrorCode.InvalidInput, message: '휘발성 채널의 첨부는 다시 이을 수 없습니다' });
  });

  it('channel.update의 forum_tags는 목록 전체를 바꾸고 데몬과 같은 한도로 검사한다', async () => {
    const { client } = setup();
    const { view, ch } = await crew(client);
    const channel = ch('공략-게시판').channel;
    const up = (forum_tags: unknown) => client.call('channel.update', { space: view.space, channel, forum_tags } as never);
    await up([{ id: 5, name: '🎉 공지' }]);
    const after = await client.call('space.get', { space: view.space });
    expect(after.channels.find((c) => c.channel === channel)?.settings.forum_tags).toEqual([{ id: 5, name: '🎉 공지' }]);
    await expect(up(Array.from({ length: 21 }, (_, i) => ({ id: i, name: `t${i}` })))).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    await expect(up([{ id: 1, name: ' 앞공백' }])).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    await expect(up([{ id: 1, name: 'a' }, { id: 1, name: 'b' }])).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    await expect(up([{ id: 1, name: '가'.repeat(101) }])).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
    await expect(client.call('channel.update', { space: view.space, channel: ch('잡담').channel, forum_tags: [{ id: 1, name: 'x' }] })).rejects.toMatchObject({
      code: ErrorCode.InvalidInput,
    });
  });
});
