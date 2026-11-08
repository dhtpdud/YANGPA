import { describe, expect, it } from 'vitest';
import type { ContactKind, FriendView, SpaceKind, SpaceView } from '../api/types';
import { canTrustUser, connectionLabel, trustWarningLead, trustWarningPoints } from './trust';

const ME = 'ME';
const PEER = 'PEER';
const profile = { version: 1, name: '민지', bio: '', avatar: null, banner: null, theme_color: null, status: '' };

const view = (space: string, kind: SpaceKind, members: string[]) =>
  ({ space, kind, members: members.map((user) => ({ user, profile, roles: [], online: true, automation: false })) }) as unknown as SpaceView;
const friend = (kind: ContactKind): FriendView => ({ user: PEER, profile, space: 'DM', online: true, safety_number_verified: false, kind, proposed: false });

describe('신뢰 스위치를 켤 수 있는 사람', () => {
  it('나 자신은 안 되고, 함께 멤버인 공간이 있어야 한다', () => {
    const views = { DM: view('DM', 'space', [ME, PEER]) };
    expect(canTrustUser(ME, ME, views, [])).toBe(false);
    expect(canTrustUser(null, PEER, views, [])).toBe(false);
    expect(canTrustUser(ME, PEER, views, [friend('friend')])).toBe(true);
    expect(canTrustUser(ME, PEER, views, [friend('temporary')])).toBe(true);
    expect(canTrustUser(ME, 'STRANGER', views, [])).toBe(false);
  });

  it('만료된 연락처의 1대1 공간은 세지 않지만, 같은 크루 멤버면 된다', () => {
    const dm = { DM: view('DM', 'space', [ME, PEER]) };
    expect(canTrustUser(ME, PEER, dm, [friend('expired')])).toBe(false);
    const withCrew = { ...dm, CREW: view('CREW', 'crew', [ME, PEER, 'OTHER']) };
    expect(canTrustUser(ME, PEER, withCrew, [friend('expired')])).toBe(true);
    // 내가 나간(멤버가 아닌) 크루는 세지 않습니다.
    expect(canTrustUser(ME, PEER, { CREW: view('CREW', 'crew', [PEER, 'OTHER']) }, [])).toBe(false);
  });

  it('공간 보기를 아직 읽지 못했어도 친구·임시 연락처면 된다', () => {
    expect(canTrustUser(ME, PEER, {}, [friend('friend')])).toBe(true);
    expect(canTrustUser(ME, PEER, {}, [friend('expired')])).toBe(false);
  });
});

describe('연결 방식 문구', () => {
  it('방향마다 누구에게 IP가 보이는지 말로 알리고, 모르는 값은 알 수 없음으로', () => {
    expect(connectionLabel('p2p')).toBe('P2P 직접 연결(서로 IP가 보임)');
    expect(connectionLabel('one_way_me_exposed')).toBe('단방향: 상대에게 내 IP가 보임(상대는 숨음)');
    expect(connectionLabel('one_way_peer_exposed')).toBe('단방향: 내게 상대 IP가 보임(나는 숨음)');
    expect(connectionLabel('tor')).toBe('Tor(서로 숨음)');
    expect(connectionLabel('offline')).toBe('오프라인');
    expect(connectionLabel('relay')).toBe('알 수 없음');
    expect(connectionLabel('turn')).toBe('알 수 없음');
    expect(connectionLabel('toString')).toBe('알 수 없음');
  });
});

describe('IP 노출 경고 문구', () => {
  it('사람은 그 사람에게, 크루는 멤버 모두(나중에 들어올 멤버 포함)에게 보인다고 알린다', () => {
    expect(trustWarningLead({ type: 'user', user: PEER, name: '민지' })).toBe('민지 님을 신뢰하면 그 사람이 이 기기의 IP 주소를 볼 수 있습니다.');
    expect(trustWarningLead({ type: 'space', space: 'CREW', name: '게임 모임' })).toContain('「게임 모임」 크루를 신뢰하면 이 크루의 멤버 모두가');
    const crew = trustWarningPoints({ type: 'space', space: 'CREW', name: '게임 모임' });
    expect(crew.some((p) => p.includes('나중에 이 크루에 들어오는 멤버'))).toBe(true);
    const user = trustWarningPoints({ type: 'user', user: PEER, name: '민지' });
    expect(user.some((p) => p.includes('나중에'))).toBe(false);
    expect(user.some((p) => p.includes('이미 보인 IP 주소는 다시 숨길 수 없습니다'))).toBe(true);
  });
});
