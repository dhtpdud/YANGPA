// 실제 데몬 종단 간 시험: yangpad 세 개를 띄우고 앱의 API 클라이언트(ApiClient + Node 전송)로 조작합니다.
// 신원 만들기 → 연락처 링크(임시 연락처 → 친구 추가·수락) → 1:1 메시지 양방향 → 크루 초대·세 사람 대화 → 휘발성 채널 메시지·만료(누출 검사 포함)
// → 첨부 왕복(자동 받기·blob.read 바이너리 프레임·blob.save) → 전송 속도·범위별 속도 제한·파일 다시 잇기 → 신뢰 스위치(사람·크루, 지원하는 데몬만)
// → 모더레이션(차단·신고·처리·밴, 지원하는 데몬만).
//
// yangpad가 없으면 건너뜁니다(cargo build -p yangpa-daemon 또는 YANGPA_YANGPAD=경로). CI처럼 꼭 돌려야 하면
// YANGPA_E2E_REQUIRED=1을 주면 없을 때 실패합니다.

/// <reference types="node" />
import { randomBytes } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { prepareZXingModule, readBarcodes } from 'zxing-wasm/reader';
import { RpcError } from '../api/client';
import { ErrorCode, type ChannelId, type ConnectionMode, type MessageView, type SpaceId } from '../api/types';
import { Cluster, Daemon, eventually, findYangpad } from './harness';
import { appLinkKind, WEB_BASE } from '../lib/links';

const BIN = findYangpad();
const REQUIRED = process.env.YANGPA_E2E_REQUIRED === '1';
const STEP = 180_000;
/** 휘발성 채널 수명(초). 만료는 알림으로 기다리므로 시험이 이만큼 길어질 뿐 타이밍에 기대지 않습니다. */
const VOLATILE_TTL_SECS = 20;

if (!BIN) {
  const msg =
    '[e2e] yangpad 실행 파일을 찾지 못해 실제 데몬 종단 간 시험을 건너뜁니다. ' +
    '`cargo build -p yangpa-daemon`으로 만들거나 YANGPA_YANGPAD에 경로를 주세요.';
  if (REQUIRED) throw new Error(msg);
  console.warn(msg);
}

/** 보기 타입의 필수 필드가 실제 응답에 있는지(types.ts와 데몬 JSON이 어긋나지 않게). */
function expectKeys(obj: unknown, keys: string[], what: string) {
  expect(obj, what).toBeTypeOf('object');
  for (const k of keys) expect(Object.prototype.hasOwnProperty.call(obj, k), `${what}.${k}`).toBe(true);
}

const CONNECTION_MODES: ConnectionMode[] = ['p2p', 'one_way_me_exposed', 'one_way_peer_exposed', 'tor', 'offline'];
/** 응답에 IP 주소(점 넷 표기·multiaddr)가 실리지 않았는지. */
const IP_LIKE = /\b\d{1,3}(\.\d{1,3}){3}\b|\/ip[46]\//;

const MESSAGE_KEYS = ['id', 'author', 'author_device', 'created_at', 'lamport', 'text', 'attachments', 'embeds', 'reactions', 'deleted', 'volatile'];

async function firstText(d: Daemon, space: SpaceId): Promise<ChannelId> {
  const list = await d.client.call('channel.list', { space });
  const ch = list.find((c) => c.settings.kind === 'text' && c.settings.security === 'normal');
  if (!ch) throw new Error(`${d.name}: 텍스트 채널 없음`);
  return ch.channel;
}

async function findMessage(d: Daemon, space: SpaceId, channel: ChannelId, pred: (m: MessageView) => boolean): Promise<MessageView | undefined> {
  const r = await d.client.call('message.list', { space, channel, limit: 200 });
  return r.messages.find(pred);
}

function waitText(d: Daemon, space: SpaceId, channel: ChannelId, text: string) {
  return eventually(`${d.name}가 "${text}"를 받음`, () => findMessage(d, space, channel, (m) => m.text === text));
}

describe.skipIf(!BIN)('실제 데몬 종단 간', () => {
  let cluster: Cluster;
  let a: Daemon;
  let b: Daemon;
  let c: Daemon;
  let dm: SpaceId;
  let crew: SpaceId;

  beforeAll(async () => {
    cluster = new Cluster(BIN!);
    // 먼저 띄운 데몬의 주소를 다음 데몬의 부트스트랩으로 넘기므로 차례로 띄웁니다.
    a = await cluster.start('a');
    b = await cluster.start('b');
    c = await cluster.start('c');
  }, STEP);

  afterAll(async () => {
    await cluster?.stop();
  }, STEP);

  const guard = (fn: () => Promise<void>) => async () => {
    try {
      await fn();
    } catch (e) {
      console.error(`[e2e] 실패. 데몬 로그 끝부분:\n${cluster.logs()}`);
      throw e;
    }
  };

  /** 앱이 그리는 QR(둘레 4칸 여백)을 화소로 옮깁니다. 앱의 해독기로 읽어 보려고요. */
  const qrPixels = ({ size, modules }: { size: number; modules: string }, cell: number) => {
    const quiet = 4;
    const n = (size + quiet * 2) * cell;
    const px = new Uint8ClampedArray(n * n * 4).fill(255);
    for (let i = 0; i < size * size; i++) {
      if (modules[i] !== '1') continue;
      const x0 = ((i % size) + quiet) * cell;
      const y0 = (Math.floor(i / size) + quiet) * cell;
      for (let y = y0; y < y0 + cell; y++) for (let x = x0; x < x0 + cell; x++) px.fill(0, (y * n + x) * 4, (y * n + x) * 4 + 3);
    }
    return { px, n };
  };

  it(
    '신원을 만들고 서로의 주소를 알린다',
    guard(async () => {
      for (const [d, name] of [
        [a, '앨리스'],
        [b, '밥'],
        [c, '캐럴'],
      ] as const) {
        await expect(d.client.call('identity.get', {})).rejects.toMatchObject({ code: 1008 });
        const r = await d.client.call('identity.create', { name });
        const id = await d.client.call('identity.get', {});
        expectKeys(id, ['user', 'device', 'profile', 'devices', 'first_syncing'], 'IdentityView');
        expect(id.user).toBe(r.user);
        expect(id.profile.name).toBe(name);
        expect(id.devices.map((x) => x.device)).toContain(r.device);
        const st = await d.client.call('system.status', {});
        expect(st.identity).toBe(r.user);
        expectKeys(st.net, ['peer_id', 'mode', 'listen_addrs', 'nat', 'connections'], 'NetStatus');
        expect(st.net).not.toHaveProperty('low_power');
        expect(st.net).not.toHaveProperty('hide_ip');
      }
      await cluster.mesh();
    }),
    STEP,
  );

  it(
    '연락처 링크로 1:1 공간을 만들고(임시 연락처 → 친구 추가·수락) 양쪽으로 메시지를 주고받는다',
    guard(async () => {
      const link = await a.client.call('friend.link.create', { expires_in_secs: 600, single_use: true });
      expect(link.url.startsWith(`${WEB_BASE}#friend/`)).toBe(true);
      const links = await a.client.call('friend.link.list', {});
      expect(links.some((l) => l.token_id === link.token_id && l.single_use)).toBe(true);
      const bUser = (await b.client.call('identity.get', {})).user;
      const aUser = (await a.client.call('identity.get', {})).user;
      const added = await b.client.call('friend.add', { url: link.url });
      expect(added.user).toBe(aUser);
      // 링크 주인의 수락 없이 양쪽에 같은 1:1 공간과 임시 연락처가 생깁니다.
      const tempA = await eventually('앨리스에게 밥이 임시 연락처로 생김', async () =>
        (await a.client.call('friend.list', {})).find((f) => f.user === bUser && f.kind === 'temporary'),
      );
      dm = tempA.space;
      await eventually('밥도 같은 1:1 공간을 임시 연락처로 받음', async () =>
        (await b.client.call('friend.list', {})).find((f) => f.user === aUser && f.space === dm && f.kind === 'temporary'),
      );
      // 친구 추가: 밥이 보내면 앨리스에게 친구 요청 알림이 앱 구독으로 옵니다.
      expect((await b.client.call('friend.propose', { user: aUser })).status).toBe('requested');
      expect((await b.client.call('friend.list', {})).find((f) => f.user === aUser)?.proposed).toBe(true);
      const requestId = await a.waitEvent('friend.request', (n) => (n.type === 'friend.request' && n.user === bUser ? n.request_id : undefined));
      const reqs = await a.client.call('friend.requests', {});
      expectKeys(reqs[0], ['request_id', 'user', 'profile', 'received_at'], 'FriendRequestView');
      // 수락해도 새 대화는 생기지 않고 지금 1:1 공간이 이어집니다.
      expect((await a.client.call('friend.accept', { request_id: requestId })).space).toBe(dm);
      await eventually('밥에게도 친구가 됨', async () =>
        (await b.client.call('friend.list', {})).find((f) => f.user === aUser && f.space === dm && f.kind === 'friend'),
      );
      const friends = await a.client.call('friend.list', {});
      expectKeys(friends[0], ['user', 'profile', 'space', 'online', 'safety_number_verified', 'kind', 'proposed'], 'FriendView');
      expect(friends.find((f) => f.user === bUser)?.kind).toBe('friend');
      const sa = await a.client.call('friend.safety_number', { user: bUser });
      const sb = await b.client.call('friend.safety_number', { user: aUser });
      expect(sa.digits).toBe(sb.digits);

      const summary = (await a.client.call('space.list', {})).find((s) => s.space === dm);
      expectKeys(summary, ['space', 'kind', 'name', 'unread', 'mention', 'dormant', 'member_count'], 'SpaceSummary');
      expect(summary?.kind).toBe('space');
      const view = await a.client.call('space.get', { space: dm });
      expectKeys(view, ['space', 'kind', 'settings', 'owner', 'committer', 'members', 'roles', 'channels', 'emojis', 'my_permissions', 'pending_commit', 'dormant'], 'SpaceView');
      expectKeys(view.channels[0], ['channel', 'settings', 'unread', 'api_read_allowed', 'p2p', 'p2p_joined'], 'ChannelView');
      expectKeys(view.channels[0].settings, ['name', 'kind', 'topic', 'position', 'security', 'join_mode', 'ttl', 'history', 'link_previews', 'overwrites', 'forum_tags'], 'ChannelSettings');

      const chA = await firstText(a, dm);
      const chB = await firstText(b, dm);
      expect(chB).toBe(chA);
      const hello = `안녕 밥 ${randomBytes(4).toString('hex')}`;
      const sent = await a.client.call('message.send', { space: dm, channel: chA, text: hello });
      const got = await waitText(b, dm, chB, hello);
      expectKeys(got, MESSAGE_KEYS, 'MessageView');
      expect(got.id).toBe(sent.id);
      expect(got.author).toBe(aUser);
      expect(got.volatile).toBe(false);
      // message.new 알림도 앱 구독으로 옵니다(OS 알림용 notify 포함).
      const note = await b.waitEvent('message.new', (n) => (n.type === 'message.new' && n.message.id === sent.id ? n : undefined));
      expect(note.notify.title).toBeTruthy();

      const reply = `반가워 앨리스 ${randomBytes(4).toString('hex')}`;
      const r2 = await b.client.call('message.send', { space: dm, channel: chB, text: reply, reply_to: sent.id });
      const back = await waitText(a, dm, chA, reply);
      expect(back.reply_to).toBe(sent.id);

      // 수정·반응이 상대에게 반영됩니다.
      await b.client.call('message.edit', { space: dm, channel: chB, id: r2.id, text: `${reply} (고침)` });
      await a.client.call('message.react', { space: dm, channel: chA, id: r2.id, emoji: '👍' });
      await eventually('수정·반응 반영', async () => {
        const m = await findMessage(b, dm, chB, (x) => x.id === r2.id);
        return m && m.edited_at && m.text.endsWith('(고침)') && m.reactions.some((x) => x.emoji === '👍' && x.count === 1) ? m : undefined;
      });
      await a.client.call('channel.mark_read', { space: dm, channel: chA, event: r2.id });
      const found = await a.client.call('message.search', { query: '앨리스', limit: 10 });
      expect(found.messages.some((m) => m.id === r2.id)).toBe(true);
      expect(found.total).toBeGreaterThanOrEqual(found.messages.length);
    }),
    STEP,
  );

  it(
    '크루를 만들고 초대 링크로 세 사람이 대화한다',
    guard(async () => {
      crew = (await a.client.call('crew.create', { name: 'E2E 크루', description: '종단 간 시험' })).space;
      const inv = await a.client.call('invite.create', { space: crew, max_uses: 5 });
      const invites = await a.client.call('invite.list', { space: crew });
      expectKeys(invites[0], ['token_hash', 'uses', 'roles', 'created_by'], 'InviteView');
      for (const d of [b, c]) {
        const j = await eventually(`${d.name} 초대 가입`, async () => d.client.call('invite.join', { url: inv.url }));
        expect(j.space).toBe(crew);
        await eventually(`${d.name}가 크루 상태를 받음`, async () => (await d.client.call('space.list', {})).find((s) => s.space === crew && s.kind === 'crew'));
      }
      await eventually('소유자에게 멤버 셋', async () => {
        const members = await a.client.call('member.list', { space: crew });
        return members.length === 3 ? members : undefined;
      });
      const members = await a.client.call('member.list', { space: crew });
      expectKeys(members[0], ['user', 'profile', 'roles', 'online', 'automation'], 'MemberView');
      const roles = await a.client.call('role.list', { space: crew });
      expectKeys(roles[0], ['id', 'name', 'color', 'rank', 'permissions'], 'Role');

      const ch = await firstText(a, crew);
      const texts: string[] = [];
      for (const d of [a, b, c]) {
        await eventually(`${d.name}가 크루 채널을 봄`, async () => (await d.client.call('channel.list', { space: crew })).find((x) => x.channel === ch));
        const t = `${d.name}의 크루 인사 ${randomBytes(3).toString('hex')}`;
        texts.push(t);
        await d.client.call('message.send', { space: crew, channel: ch, text: t });
      }
      for (const d of [a, b, c]) for (const t of texts) await waitText(d, crew, ch, t);
    }),
    STEP,
  );

  it(
    '휘발성 채널 메시지가 참가자에게 가고 수명이 끝나면 지워지며 디스크에 남지 않는다',
    guard(async () => {
      const { channel } = await a.client.call('channel.create', {
        space: crew,
        name: '비밀',
        kind: 'text',
        security: 'volatile',
        join_mode: 'join_leave',
        // 전달이 느린 러너에서도 받기 전에 만료되지 않을 만큼(받았는지는 알림으로 보고, 만료도 알림으로 기다림).
        ttl_secs: VOLATILE_TTL_SECS,
      });
      for (const d of [a, b]) {
        await eventually(`${d.name}가 휘발성 채널을 봄`, async () => (await d.client.call('channel.list', { space: crew })).find((x) => x.channel === channel));
        await d.client.call('channel.join', { space: crew, channel });
      }
      const view = (await b.client.call('channel.list', { space: crew })).find((x) => x.channel === channel)!;
      expect(view.settings.security).toBe('volatile');
      expect(view.settings.ttl).toEqual({ mode: 'seconds', seconds: VOLATILE_TTL_SECS });
      expect(view.joined).toBe(true);
      expect(view.api_read_allowed).toBe(false);
      // 이 기기의 API 읽기 허용(앱의 휘발성 안내 띠·상단 표시가 쓰는 길): 켜고 끄면 보기에 그대로 반영됩니다.
      const apiRead = async () => (await b.client.call('channel.list', { space: crew })).find((x) => x.channel === channel)!.api_read_allowed;
      await b.client.call('channel.allow_api_read', { space: crew, channel, allowed: true });
      expect(await apiRead()).toBe(true);
      await b.client.call('channel.allow_api_read', { space: crew, channel, allowed: false });
      expect(await apiRead()).toBe(false);
      // 보내기 전에 서로를 참가자로 볼 때까지 기다립니다(앨리스의 세션 키가 밥에게도 가도록).
      const [aUser, bUser] = await Promise.all([a, b].map(async (d) => (await d.client.call('identity.get', {})).user));
      for (const d of [a, b]) {
        await eventually(`${d.name}가 두 참가자를 봄`, async () => {
          const ch = (await d.client.call('channel.list', { space: crew })).find((x) => x.channel === channel);
          return ch?.participants?.includes(aUser) && ch.participants.includes(bUser);
        });
      }
      // 누출 검사 표식: 메시지 본문에만 넣고 어디에도 출력하지 않습니다.
      const marker = `vol-${randomBytes(12).toString('hex')}`;
      const sent = await eventually('휘발성 메시지 보내기(세션 키 받기 대기)', async () =>
        a.client.call('message.send', { space: crew, channel, text: marker }),
      );
      // 받았는지는 목록을 짧은 수명 안에 훑는 대신 앱 구독의 message.new로 봅니다(타이밍에 기대지 않게).
      const n = await b.waitEvent('휘발성 message.new', (x) => (x.type === 'message.new' && x.message.id === sent.id ? x : undefined));
      expect(n.message.volatile).toBe(true);
      expect(n.message.text).toBe(marker);
      expect(n.message.expires_at).toBeGreaterThan(n.message.created_at);
      expect(n.notify.body ?? '').not.toContain(marker);
      // 아직 수명이 남았으면 목록에도 있습니다(늦게 와서 이미 만료됐다면 이 확인만 건너뜀).
      const listed = await findMessage(b, crew, channel, (m) => m.id === sent.id);
      if (Date.now() + 1_000 < (n.message.expires_at ?? 0)) {
        expect(listed?.volatile).toBe(true);
      }
      // 수명이 끝나면 모든 참가자 기기에서 사라지고 만료 알림이 옵니다.
      await b.waitEvent('message.expired', (x) => (x.type === 'message.expired' && x.ids.includes(sent.id) ? true : undefined));
      for (const d of [a, b]) {
        await eventually(`${d.name}에서 만료`, async () => !(await findMessage(d, crew, channel, (m) => m.id === sent.id)));
      }
      for (const d of [a, b, c]) {
        const leak = await d.client.call('diag.leakcheck', { marker });
        expect(leak, `${d.name} 누출 검사`).toEqual({ clean: true, findings: [] });
      }
      await b.client.call('channel.leave', { space: crew, channel });
      const left = (await b.client.call('channel.list', { space: crew })).find((x) => x.channel === channel)!;
      expect(left.joined).toBe(false);
    }),
    STEP,
  );

  it(
    '첨부를 보내고 받아 같은 바이트로 읽고 저장한다',
    guard(async () => {
      const ch = await firstText(a, dm);
      const bytes = randomBytes(300 * 1024);
      const path = join(a.dir, 'e2e-첨부.bin');
      writeFileSync(path, bytes);
      const sent = await a.client.call('message.send', { space: dm, channel: ch, text: '파일', attachments: [{ path, name: '시험.bin' }] });
      const msg = await eventually('밥이 첨부 메시지를 받음', () => findMessage(b, dm, ch, (m) => m.id === sent.id && m.attachments.length === 1));
      const att = msg.attachments[0];
      expectKeys(att, ['blob', 'name', 'mime', 'size', 'downloaded'], 'AttachmentView');
      expect(att.name).toBe('시험.bin');
      expect(att.size).toBe(bytes.length);
      // 직접 받기(1:1은 자동으로도 받지만 우선순위를 올림) → 조각을 다 받을 때까지.
      // 데몬은 메시지를 보기 행에 넣은 다음에 블롭 참조를 기록하므로, 목록에 보인 직후 잠깐은 NotFound일 수 있습니다.
      await eventually('첨부 받기 요청', async () => {
        try {
          await b.client.call('blob.fetch', { space: dm, blob: att.blob, priority: 200 });
          return true;
        } catch (e) {
          if (e instanceof RpcError && e.code === ErrorCode.NotFound) return undefined;
          // 다른 오류는 기다려도 소용없으니 바로 실패합니다(eventually는 RpcError만 다시 시도).
          throw new Error(`blob.fetch 실패: ${(e as Error).message}`);
        }
      });
      await eventually('첨부 내려받기', async () => {
        const st = await b.client.call('blob.status', { space: dm, blob: att.blob });
        return st.total_chunks > 0 && st.have_chunks === st.total_chunks ? st : undefined;
      });
      const r = await b.client.call('blob.read', { space: dm, blob: att.blob, key_from: sent.id });
      expect(Buffer.from(r.data_base64 ?? '', 'base64').equals(bytes)).toBe(true);
      // 바이너리 프레임으로도 읽힙니다(앱의 커스텀 프로토콜이 쓰는 길).
      const bin = await b.client.transport.readBlob(dm, att.blob, sent.id, att.mime);
      expect(Buffer.from(bin).equals(bytes)).toBe(true);
      const out = join(b.dir, 'saved.bin');
      await b.client.call('blob.save', { space: dm, blob: att.blob, key_from: sent.id, path: out });
      expect(readFileSync(out).equals(bytes)).toBe(true);
      await eventually('downloaded 표시', () => findMessage(b, dm, ch, (m) => m.id === sent.id && m.attachments[0]?.downloaded === true));
    }),
    STEP,
  );

  it(
    '전송 속도·범위별 속도 제한·파일 다시 잇기(해시가 같아야 함)',
    guard(async () => {
      const rates = await a.client.call('transfer.rates', {});
      expectKeys(rates, ['up_bps', 'down_bps'], 'TransferRates');
      expect(rates.up_bps).toBeTypeOf('number');
      expect(rates.down_bps).toBeTypeOf('number');
      expect(JSON.stringify(rates)).not.toMatch(IP_LIKE);

      const scope = { kind: 'crew' as const, space: crew };
      // 0은 막음으로 저장됩니다.
      await a.client.call('rate.set_limit', { scope, up_bps: 0, down_bps: null });
      expect(await a.client.call('rate.limits', {})).toContainEqual({ scope, up_bps: 0, down_bps: null });
      await a.client.call('rate.set_limit', { scope, up_bps: 64 * 1024, down_bps: null });
      expect(await a.client.call('rate.limits', {})).toContainEqual({ scope, up_bps: 64 * 1024, down_bps: null });
      await a.client.call('rate.set_limit', { scope, up_bps: null, down_bps: null });
      expect((await a.client.call('rate.limits', {})).some((l) => l.scope.kind === 'crew' && l.scope.space === crew)).toBe(false);

      const ch = await firstText(a, dm);
      const bytes = randomBytes(64 * 1024);
      const path = join(a.dir, 'e2e-다시잇기.bin');
      writeFileSync(path, bytes);
      const sent = await a.client.call('message.send', { space: dm, channel: ch, text: '다시 잇기', attachments: [{ path, name: '다시잇기.bin' }] });
      const msg = await eventually('보낸 첨부가 목록에 보임', () => findMessage(a, dm, ch, (m) => m.id === sent.id && m.attachments.length === 1));
      const base = { space: dm, channel: ch, message: sent.id, blob: msg.attachments[0].blob };
      const other = join(a.dir, 'e2e-다른파일.bin');
      writeFileSync(other, randomBytes(64 * 1024));
      await expect(a.client.call('blob.relink', { ...base, path: other })).rejects.toMatchObject({
        code: ErrorCode.InvalidInput,
        message: '파일 내용이 첨부와 달라 다시 이을 수 없습니다(해시가 다름)',
      });
      // 원본을 옮겨도 같은 내용이면 다시 이어집니다.
      const moved = join(a.dir, 'e2e-옮긴파일.bin');
      writeFileSync(moved, bytes);
      const st = await a.client.call('blob.relink', { ...base, path: moved });
      expect(st.total_chunks).toBeGreaterThan(0);
      expect(st.have_chunks).toBe(st.total_chunks);
    }),
    STEP,
  );

  // 신뢰 스위치 전 데몬이면 건너뜁니다. 켠 것은 끝에서 모두 끕니다(뒤 시험의 연결이 바뀌지 않게).
  it(
    '신뢰 스위치: 사람·크루 단위로 켜고 끄며, 연결 방식만 알리고 IP는 싣지 않는다',
    async (ctx) => {
      const [aUser, bUser, cUser] = await Promise.all([a, b, c].map(async (d) => (await d.client.call('identity.get', {})).user));
      try {
        await a.client.call('trust.get', { user: bUser });
      } catch (e) {
        if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) {
          console.warn('[e2e] 이 yangpad는 신뢰 스위치(trust.*)를 지원하지 않아 건너뜁니다.');
          ctx.skip();
        }
        throw e;
      }
      await guard(async () => {
        const v0 = await a.client.call('trust.get', { user: bUser });
        expectKeys(v0, ['user', 'trusted', 'via_crews', 'connection'], 'TrustView');
        expect(v0).toMatchObject({ user: bUser, trusted: false, via_crews: [] });
        expect(CONNECTION_MODES).toContain(v0.connection);
        expect(JSON.stringify(v0)).not.toMatch(IP_LIKE);
        expect((await a.client.call('space.get', { space: crew })).trusted).toBe(false);

        // 사람: 나 자신은 안 되고, 함께 쓰는 공간이 있는 사람은 켜고 끌 수 있습니다.
        await expect(a.client.call('trust.set_user', { user: aUser, enabled: true })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
        await a.client.call('trust.set_user', { user: bUser, enabled: true });
        const v1 = await a.client.call('trust.get', { user: bUser });
        expect(v1.trusted).toBe(true);
        expect(CONNECTION_MODES).toContain(v1.connection);
        expect(JSON.stringify(v1)).not.toMatch(IP_LIKE);

        // 크루: 1:1 공간은 안 되고, 켜면 보기의 trusted와 space.updated, 그 크루 멤버에게 via_crews.
        await expect(a.client.call('trust.set_space', { space: dm, enabled: true })).rejects.toMatchObject({ code: ErrorCode.InvalidInput });
        const seen = a.events.length;
        await a.client.call('trust.set_space', { space: crew, enabled: true });
        expect((await a.client.call('space.get', { space: crew })).trusted).toBe(true);
        await eventually('신뢰 후 space.updated', async () => a.events.slice(seen).some((n) => n.type === 'space.updated' && n.space === crew));
        expect(await a.client.call('trust.get', { user: cUser })).toMatchObject({ user: cUser, trusted: false, via_crews: [crew] });
        // 크루 신뢰는 내 기기의 동의일 뿐 다른 멤버의 보기에는 드러나지 않습니다.
        expect((await b.client.call('space.get', { space: crew })).trusted).toBe(false);

        await a.client.call('trust.set_space', { space: crew, enabled: false });
        await a.client.call('trust.set_user', { user: bUser, enabled: false });
        expect((await a.client.call('space.get', { space: crew })).trusted).toBe(false);
        expect(await a.client.call('trust.get', { user: cUser })).toMatchObject({ trusted: false, via_crews: [] });
        expect(await a.client.call('trust.get', { user: bUser })).toMatchObject({ trusted: false, via_crews: [] });
      })();
    },
    STEP,
  );

  // 크루 멤버를 밴하므로 맨 끝에 둡니다. 모더레이션 전 데몬이면 건너뜁니다.
  it(
    '차단하면 메시지에 표시가 붙고, 신고는 운영진에게만 가며, 처리·밴이 모두에게 반영된다',
    async (ctx) => {
      try {
        await a.client.call('user.blocked', {});
      } catch (e) {
        if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) {
          console.warn('[e2e] 이 yangpad는 모더레이션(user.block·message.report·member.ban)을 지원하지 않아 건너뜁니다.');
          ctx.skip();
        }
        throw e;
      }
      await guard(async () => {
        const [aUser, bUser, cUser] = await Promise.all([a, b, c].map(async (d) => (await d.client.call('identity.get', {})).user));
        const ch = await firstText(a, crew);

        // 차단: 이 기기에서만. 차단한 사람의 메시지·멤버에 표시가 붙습니다.
        await b.client.call('user.block', { user: cUser });
        const blocked = await b.client.call('user.blocked', {});
        expect(blocked.map((x) => x.user)).toEqual([cUser]);
        expectKeys(blocked[0], ['user', 'profile', 'blocked_at'], 'BlockedView');
        const t1 = `캐럴의 메시지 ${randomBytes(3).toString('hex')}`;
        const m1 = await c.client.call('message.send', { space: crew, channel: ch, text: t1 });
        const seen = await waitText(b, crew, ch, t1);
        expect(seen.author_blocked).toBe(true);
        expect((await waitText(a, crew, ch, t1)).author_blocked).toBe(false);
        const bMembers = await b.client.call('member.list', { space: crew });
        expect(bMembers.find((m) => m.user === cUser)?.blocked).toBe(true);
        expect(bMembers.find((m) => m.user === aUser)?.blocked).toBe(false);
        await b.client.call('user.unblock', { user: cUser });
        expect(await b.client.call('user.blocked', {})).toEqual([]);

        // 신고: 운영진(소유자 앨리스)에게 내용 없는 알림 → 목록에서 사유 확인. 신고하며 숨긴 메시지는 밥 목록에서 빠짐.
        await expect(b.client.call('message.report', { space: crew, channel: ch, message: m1.id, reason: '  ' })).rejects.toMatchObject({
          code: ErrorCode.InvalidInput,
        });
        const { report_id } = await b.client.call('message.report', { space: crew, channel: ch, message: m1.id, reason: '광고 도배', hide: true });
        expect(typeof report_id).toBe('string');
        expect(await findMessage(b, crew, ch, (m) => m.id === m1.id)).toBeUndefined();
        const note = await a.waitEvent('report.new', (n) => (n.type === 'report.new' && n.report_id === report_id ? n : undefined));
        expect(note).toEqual({ type: 'report.new', space: crew, report_id });
        const open = await eventually('앨리스의 신고 목록', async () =>
          (await a.client.call('report.list', { space: crew, status: 'open' })).find((r) => r.report_id === report_id),
        );
        expectKeys(open, ['report_id', 'space', 'channel', 'message', 'reporter', 'reason', 'reported_at', 'status', 'resolution'], 'ReportView');
        expect(open).toMatchObject({ space: crew, channel: ch, message: m1.id, reporter: bUser, reason: '광고 도배', status: 'open', resolution: null });
        // 운영진이 아닌 사람은 신고 목록을 볼 수 없습니다.
        await expect(c.client.call('report.list', { space: crew })).rejects.toMatchObject({ code: ErrorCode.Forbidden });

        // 처리(메시지 삭제) → 모두에게 삭제가 퍼지고 처리 결과가 남습니다.
        await a.client.call('report.resolve', { space: crew, report_id, action: 'delete_message', note: '확인함' });
        await eventually('캐럴 쪽 메시지 삭제', () => findMessage(c, crew, ch, (m) => m.id === m1.id && m.deleted));
        const done = (await a.client.call('report.list', { space: crew, status: 'resolved' })).find((r) => r.report_id === report_id);
        expect(done).toMatchObject({ status: 'resolved', resolution: 'delete_message: 확인함' });
        await expect(a.client.call('report.resolve', { space: crew, report_id, action: 'dismiss' })).rejects.toBeInstanceOf(RpcError);

        // 밴: 목록에 남고 모든 멤버 기기에서 빠지며, 풀 수 있습니다.
        await a.client.call('member.ban', { space: crew, user: cUser, reason: '반복 위반' });
        const bans = await a.client.call('member.bans', { space: crew });
        expectKeys(bans[0], ['user', 'profile', 'reason', 'banned_by', 'banned_at'], 'BanView');
        expect(bans.find((x) => x.user === cUser)).toMatchObject({ reason: '반복 위반', banned_by: aUser });
        await expect(a.client.call('member.ban', { space: crew, user: cUser })).rejects.toMatchObject({ code: ErrorCode.Conflict });
        for (const d of [a, b]) {
          await eventually(`${d.name}에서 캐럴이 빠짐`, async () => !(await d.client.call('member.list', { space: crew })).some((m) => m.user === cUser));
        }
        await a.client.call('member.unban', { space: crew, user: cUser });
        expect((await a.client.call('member.bans', { space: crew })).some((x) => x.user === cUser)).toBe(false);
      })();
    },
    STEP,
  );

  it(
    '기기 연결: 쓰던 기기가 띄운 QR을 새 기기가 읽고, 같은 번호를 보고 허용하면 같은 계정이 된다',
    guard(async () => {
      const offer = await a.client.call('device.link.create', {});
      expectKeys(offer, ['url', 'qr', 'expires_at'], 'DeviceLinkOffer');
      expect(appLinkKind(offer.url)).toBe('device');
      const { px, n } = qrPixels(offer.qr, 3);
      const wasm = readFileSync(new URL('../../node_modules/zxing-wasm/dist/reader/zxing_reader.wasm', import.meta.url));
      prepareZXingModule({ overrides: { wasmBinary: new Uint8Array(wasm).buffer } });
      const [read] = await readBarcodes({ data: px, width: n, height: n } as ImageData, { formats: ['QRCode'] });
      expect(read?.text, `링크 ${offer.url.length}자, QR ${offer.qr.size}칸`).toBe(offer.url);

      const d = await cluster.start('d');
      const { code } = await d.client.call('device.link.join', { url: offer.url });
      const request = await eventually('허용을 기다리는 새 기기', () => a.client.call('device.link.pending', {}));
      expectKeys(request, ['name', 'code', 'contested'], 'DeviceLinkRequestView');
      expect(request.code).toBe(code);
      await a.client.call('device.link.decide', { approve: true });

      const id = await eventually('새 기기가 받은 신원', () => d.client.call('identity.get', {}).catch(() => null));
      expect(id.user).toBe((await a.client.call('identity.get', {})).user);
      expect((await a.client.call('device.list', {})).map((x) => x.device)).toContain(id.device);
    }),
    STEP,
  );
});
