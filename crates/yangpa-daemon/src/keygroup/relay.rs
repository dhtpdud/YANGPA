//! 놓친 키 중계: 모르는 채널 뿌리(`GetChannelRoots`)와 놓친 키 그룹의 지금 T·R(`GetGroupKeys`)을 같은 공간 멤버에게
//! 묻고 답합니다. 답은 요청한 기기의 상자 키로 봉인합니다(AD = "yangpa/v3/relayed-keys" ‖ 공간 ‖ 요청 기기).
//!
//! - 답하는 쪽(`GetChannelRoots`): 활성 멤버 기기 → 그 채널을 볼 수 있음 → 기록 공개(아니면 `HistoryPrivate`) → 가진
//!   뿌리(그 공간·채널의 것만). 메시지 시더와 같은 순서라, 볼 수 없는 채널·입장 전 비공개 기록의 뿌리는 나가지 않습니다.
//! - 답하는 쪽(`GetGroupKeys`): 요청 기기가 그 그룹 사슬 끝의 멤버(크루 그룹이면 확정 기기도)이고 나도 그 끝의 상태를
//!   가졌을 때만. 내가 사슬 끝 멤버를 모르면(체크포인트 키 머리에서 세운 바닥) 비공개·덮개 그룹 키는 주지 않습니다: 지금
//!   볼 수 있다는 것만으로 주면 갓 권한을 얻은 기기가 가입 갱신 전 T로 들기 전 비공개 기록을 엽니다.
//! - 답하는 쪽(`GetGroupMembers`): 요청 기기가 활성 멤버 기기이고 그 그룹 채널을 지금 볼 수 있으면, 내가 아는 그 순번
//!   멤버 목록(받는 쪽은 체크포인트 키 머리의 멤버 요약과 맞는 것만 씀).
//! - 답하는 쪽(`GetStateKeys`): 요청 기기가 활성 멤버 기기일 때 가진 크루 상태 키만(상태 이벤트는 모든 멤버가 다시
//!   세우는 것이라 멤버면 누구나 받을 자격이 있음, 그룹 상태 키는 싣지 않음).
//! - 묻는 쪽: 뿌리·상태 키는 받은 키에서 계산한 ID가 물은 ID일 때만, T·R은 사슬 끝의 뿌리 약속과 맞을 때만 씁니다(누가
//!   답했든 같음). 같은 것을 [`ASK_AGAIN`] 안에 다시 묻지 않습니다. 상태 키는 상태가 아직 없는 가입자도 묻기에, 그때는
//!   받아 준 노드(`crews::join_peer_ok`)와 내 다른 기기에게 묻습니다.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::body::{Secret32, state_key_id};
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, KeyGroupId, SpaceId, StateKeyId};
use yangpa_core::model::{HistoryVisibility, Permissions};
use yangpa_core::wire::{MAX_ROOT_IDS, MAX_STATE_KEY_IDS, WireErrorCode, YpRequest, YpResponse};
use yangpa_crypto::keysched;
use yangpa_crypto::{GroupKind, MemberKeys, open_sealed, seal_to};
use yangpa_net::PeerId;
use yangpa_sync::Resolver;
use zeroize::Zeroizing;

use super::member::{MemberState, encode_secret};
use crate::ingest::{self, Need};
use crate::node::Node;
use crate::{devkeys, keys};

/// 같은 것을 다시 묻기까지.
const ASK_AGAIN: Duration = Duration::from_secs(20);
/// 막힌 그룹을 묻기 전에 묶음을 기다리는 시간.
const ASK_DELAY: Duration = Duration::from_secs(1);
/// 한 번에 묻는 멤버 수.
const ASK_PEERS: usize = 3;
/// 막힌 그룹 키를 묻는 바퀴 수(처음 [`ASK_DELAY`] 뒤, 그다음은 [`ASK_AGAIN`]마다).
const ASK_ROUNDS: usize = 4;
/// 중계 봉투 판(2: 상태 키를 실음, 3: 그룹 키에 그 순번 뒤 멤버 목록을 실음, 4: 멤버 목록만 따로 실음).
const RELAY_VERSION: u8 = 4;

#[derive(Serialize, Deserialize)]
struct Relayed {
    v: u8,
    roots: Vec<(ChannelId, Secret32)>,
    group: Option<GroupKeys>,
    state_keys: Vec<Secret32>,
    /// `GetGroupMembers`의 답: (그룹, 순번, 뿌리 약속, 그 갱신 뒤 멤버).
    members: Option<(KeyGroupId, u64, [u8; 32], Vec<DeviceId>)>,
}

#[derive(Serialize, Deserialize)]
struct GroupKeys {
    group: KeyGroupId,
    seq: u64,
    root_commit: [u8; 32],
    t: Secret32,
    r: Secret32,
    /// 그 갱신 뒤 그룹 멤버(답하는 쪽이 알면). 체크포인트 키 머리에서 사슬을 시작해 멤버를 모르는 기기가 배웁니다
    /// (`GroupChain::learn_members`).
    members: Option<Vec<DeviceId>>,
}

fn relay_ad(space: &SpaceId, requester: &DeviceId) -> Vec<u8> {
    let mut ad = b"yangpa/v3/relayed-keys".to_vec();
    ad.extend_from_slice(space.as_bytes());
    ad.extend_from_slice(requester.as_bytes());
    ad
}

fn seal_relayed(
    node: &Node,
    state: &yangpa_core::model::SpaceState,
    space: &SpaceId,
    user: &yangpa_core::ids::UserId,
    device: &DeviceId,
    r: &Relayed,
) -> YpResponse {
    let Some(bk) = devkeys::device_box_key(node, state, user, device) else {
        return YpResponse::error(WireErrorCode::NotFound, "요청 기기의 상자 키를 모름");
    };
    let plain = encode_secret(r);
    YpResponse::SealedKeys(seal_to(&[(*device, bk)], &relay_ad(space, device), &plain))
}

// ───────────── 답하는 쪽 ─────────────

/// 들어온 `GetChannelRoots`.
pub(crate) fn handle_get_roots(
    node: &Node,
    peer: &PeerId,
    space: SpaceId,
    channel: ChannelId,
    ids: Vec<ChannelRootId>,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    let Some(ch) = state.channels.get(&channel).filter(|c| !c.is_volatile()) else {
        return YpResponse::error(WireErrorCode::NotFound, "없는 채널");
    };
    if !state
        .channel_permissions(&user, &channel)
        .contains(Permissions::VIEW_CHANNEL)
    {
        return YpResponse::error(WireErrorCode::Forbidden, "볼 수 없는 채널");
    }
    if ch.history != HistoryVisibility::Public {
        return YpResponse::error(WireErrorCode::HistoryPrivate, "기록 비공개 채널");
    }
    if crate::checkpoint::is_unconfirmed(node, &space) {
        // 한 출처에서만 받은 체크포인트로 세운 상태(위조됐을 수 있음)의 권한으로 키를 나눠 주지 않습니다.
        return YpResponse::error(WireErrorCode::NotFound, "아직 확인하는 중인 상태");
    }
    if ids.is_empty() || ids.len() > MAX_ROOT_IDS {
        return YpResponse::error(WireErrorCode::BadRequest, "뿌리 ID 수");
    }
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        if let Ok(Some(k)) = keys::root_in(node, &space, &channel, &id) {
            roots.push((channel, k));
        }
    }
    if roots.is_empty() {
        return YpResponse::error(WireErrorCode::NotFound, "가진 뿌리가 없음");
    }
    let r = Relayed {
        v: RELAY_VERSION,
        roots,
        group: None,
        state_keys: Vec::new(),
        members: None,
    };
    seal_relayed(node, &state, &space, &user, &device, &r)
}

/// 들어온 `GetGroupKeys`. 적용 잠금을 먼저 잡고 `SpaceCore`를 읽습니다(긴 적용 동안 요청 처리 스레드를 묶지 않음:
/// 잠금은 비동기로 기다림).
pub(crate) async fn handle_get_group_keys(
    node: &Node,
    peer: &PeerId,
    space: SpaceId,
    group: KeyGroupId,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if crate::checkpoint::is_unconfirmed(node, &space) {
        // 한 출처에서만 받은 체크포인트로 세운 상태(위조됐을 수 있음)의 명단으로 키를 나눠 주지 않습니다.
        return YpResponse::error(WireErrorCode::NotFound, "아직 확인하는 중인 상태");
    }
    let l = ingest::lock_space(node, space).await;
    let Some(core) = node.spaces.read().get(&space).map(|rt| rt.core.clone()) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    let k = {
        // 적용 잠금을 쥐었으니 `SpaceCore`를 쥔 쪽은 잠깐 읽는 쪽뿐이지만, 그래도 기다리지 않습니다.
        let Some(c) = core.try_lock() else {
            return YpResponse::Busy {
                retry_after_ms: 1000,
            };
        };
        if group == KeyGroupId::crew(&space) {
            // 확정 기기(소유자·관리자의 기기, 표를 쥐면 모든 그룹 키를 앎)는 크루 키 그룹 밖이어도 받습니다: 표 없이 확정
            // 기기가 된 새 기기가 전체 재키를 내려면 지금 크루 상태 키가 있어야 합니다.
            let entitled = state.committer == device
                || c.resolver
                    .key_view()
                    .is_some_and(|kv| kv.crew.members.contains(&device));
            if !entitled {
                return YpResponse::error(WireErrorCode::Forbidden, "그룹 멤버가 아님");
            }
            c.keys
                .crew_current(&space, &c.resolver)
                .cloned()
                .map(|k| (k, None))
        } else {
            // 사슬 끝 멤버여도 지금 그 그룹 채널을 볼 수 없으면(역할을 잃었는데 Rotate가 아직 오지 않음) 주지 않습니다.
            // 사슬 끝 멤버를 모르면(체크포인트 키 머리에서 세운 바닥) 누구에게도 주지 않습니다(모듈 문서).
            let tip = c.keys.chains.get(&group).and_then(|ch| ch.send_tip());
            let entitled = state.can_view_key_group(&space, &group, &user)
                && tip
                    .as_ref()
                    .is_some_and(|t| t.members_known && t.members.contains(&device));
            if !entitled {
                return YpResponse::error(WireErrorCode::Forbidden, "그룹 멤버가 아님");
            }
            c.keys.group_current(&group).cloned().map(|k| {
                let members = tip
                    .filter(|t| t.members_known && t.root_commit == *k.root_commit())
                    .map(|t| t.members.iter().copied().collect());
                (k, members)
            })
        }
    };
    drop(l);
    let Some((k, members)) = k else {
        return YpResponse::error(WireErrorCode::NotFound, "나도 지금 키가 없음");
    };
    let r = Relayed {
        v: RELAY_VERSION,
        roots: Vec::new(),
        members: None,
        group: Some(GroupKeys {
            group,
            seq: k.seq(),
            root_commit: *k.root_commit(),
            t: k.t().clone(),
            r: k.r().clone(),
            members,
        }),
        state_keys: Vec::new(),
    };
    seal_relayed(node, &state, &space, &user, &device, &r)
}

/// 들어온 `GetGroupMembers`.
pub(crate) async fn handle_get_group_members(
    node: &Node,
    peer: &PeerId,
    space: SpaceId,
    group: KeyGroupId,
    seq: u64,
    root_commit: [u8; 32],
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if group == KeyGroupId::crew(&space) || !state.can_view_key_group(&space, &group, &user) {
        return YpResponse::error(WireErrorCode::Forbidden, "볼 수 없는 그룹");
    }
    if crate::checkpoint::is_unconfirmed(node, &space) {
        return YpResponse::error(WireErrorCode::NotFound, "아직 확인하는 중인 상태");
    }
    let l = ingest::lock_space(node, space).await;
    let Some(core) = node.spaces.read().get(&space).map(|rt| rt.core.clone()) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    let Some(c) = core.try_lock() else {
        return YpResponse::Busy {
            retry_after_ms: 1000,
        };
    };
    let list = c
        .keys
        .chains
        .get(&group)
        .and_then(|ch| ch.members_at(seq, &root_commit));
    drop(c);
    drop(l);
    let Some(list) = list else {
        return YpResponse::error(WireErrorCode::NotFound, "그 순번 멤버를 모름");
    };
    let r = Relayed {
        v: RELAY_VERSION,
        roots: Vec::new(),
        group: None,
        state_keys: Vec::new(),
        members: Some((group, seq, root_commit, list)),
    };
    seal_relayed(node, &state, &space, &user, &device, &r)
}

/// 들어온 `GetStateKeys`.
pub(crate) fn handle_get_state_keys(
    node: &Node,
    peer: &PeerId,
    space: SpaceId,
    ids: Vec<StateKeyId>,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if ids.is_empty() || ids.len() > MAX_STATE_KEY_IDS {
        return YpResponse::error(WireErrorCode::BadRequest, "상태 키 ID 수");
    }
    if crate::checkpoint::is_unconfirmed(node, &space) {
        return YpResponse::error(WireErrorCode::NotFound, "아직 확인하는 중인 상태");
    }
    let mut seen = HashSet::new();
    let mut state_keys = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        if let Ok(Some(k)) = keys::crew_state_key(node, &space, &id) {
            state_keys.push(k);
        }
    }
    if state_keys.is_empty() {
        return YpResponse::error(WireErrorCode::NotFound, "가진 상태 키가 없음");
    }
    let r = Relayed {
        v: RELAY_VERSION,
        roots: Vec::new(),
        group: None,
        state_keys,
        members: None,
    };
    seal_relayed(node, &state, &space, &user, &device, &r)
}

// ───────────── 묻는 쪽 ─────────────

/// 지금 묻지 않은 것만 남기고 물었다고 적습니다.
fn fresh(node: &Node, space: &SpaceId, keys: impl IntoIterator<Item = [u8; 32]>) -> Vec<[u8; 32]> {
    let mut asked = node.keygroup.asked.lock();
    let now = Instant::now();
    if asked.len() > 4096 {
        asked.retain(|_, t| now.duration_since(*t) < ASK_AGAIN);
    }
    keys.into_iter()
        .filter(|k| {
            let due = asked
                .get(&(*space, *k))
                .is_none_or(|t| now.duration_since(*t) >= ASK_AGAIN);
            if due {
                asked.insert((*space, *k), now);
            }
            due
        })
        .collect()
}

fn root_key(id: &ChannelRootId) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(id.as_bytes());
    k
}

/// 물어볼 멤버 피어(인증된 연결이 있는 활성 멤버 기기, `ok`를 통과한 것).
pub(crate) fn candidates(
    node: &Node,
    space: &SpaceId,
    ok: impl Fn(&yangpa_core::ids::UserId, &DeviceId) -> bool,
) -> Vec<PeerId> {
    let Some(state) = node.space_state(space) else {
        return Vec::new();
    };
    let me = node.me().ok().map(|m| m.device_id());
    let mut committer = None;
    let mut v: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, d)| {
            Some(*d) != me && state.is_member(u) && state.device_belongs(u, d) && ok(u, d)
        })
        .map(|(p, _, d)| {
            if d == state.committer {
                committer = Some(p);
            }
            p
        })
        .collect();
    v.dedup();
    // 확정 기기는 사슬 끝 키·상태 키를 늘 가지므로 자격이 있으면 빠뜨리지 않되, 몰리지 않게 다른 멤버 뒤 끝자리에 둡니다.
    // (앞 몇 곳만 고르면 막 같은 갱신을 기다리는 멤버들만 물어 못 받고, 그 순번은 다시 묻지 않아 멈출 수 있음.)
    match committer {
        Some(c) => {
            v.retain(|p| *p != c);
            v.truncate(ASK_PEERS - 1);
            v.push(c);
        }
        None => v.truncate(ASK_PEERS),
    }
    v
}

fn open_relayed(node: &Node, space: &SpaceId, bytes: &[u8]) -> Option<Relayed> {
    let me = node.me().ok()?;
    let plain: Zeroizing<Vec<u8>> = open_sealed(
        &me.device_id(),
        &me.device.boxkey,
        &relay_ad(space, &me.device_id()),
        bytes,
    )
    .ok()?;
    let r: Relayed = postcard::from_bytes(&plain).ok()?;
    (r.v == RELAY_VERSION).then_some(r)
}

/// 모르는 채널 뿌리를 멤버에게 묻습니다(백그라운드). 받으면 저장하고 그 뿌리를 기다리던 메시지를 다시 엽니다.
pub(crate) fn ask_roots(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    ids: Vec<ChannelRootId>,
) {
    if node
        .keygroup
        .relay_hold
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let want: Vec<[u8; 32]> = fresh(node, &space, ids.iter().map(root_key));
    let ids: Vec<ChannelRootId> = ids
        .into_iter()
        .filter(|id| want.contains(&root_key(id)))
        .take(MAX_ROOT_IDS)
        .collect();
    if ids.is_empty() {
        return;
    }
    node.spawn_detached(move |node| async move {
        let peers = candidates(&node, &space, |u, _| {
            node.space_state(&space).is_some_and(|s| {
                s.channel_permissions(u, &channel)
                    .contains(Permissions::VIEW_CHANNEL)
            })
        });
        let mut left: HashSet<ChannelRootId> = ids.iter().copied().collect();
        for peer in peers {
            if left.is_empty() {
                break;
            }
            let req = YpRequest::GetChannelRoots {
                space,
                channel,
                ids: left.iter().copied().collect(),
            };
            let bytes = match node.net.request(peer, req).await {
                Ok(YpResponse::SealedKeys(b)) => b,
                Ok(_) => continue,
                Err(e) => {
                    debug!(error = %e, "채널 뿌리 묻기 실패");
                    continue;
                }
            };
            let Some(r) = open_relayed(&node, &space, &bytes) else {
                continue;
            };
            let got = take_roots(&node, &space, &channel, &mut left, &r);
            if !got.is_empty() {
                ingest::wake_needs(&node, space, got).await;
            }
        }
    });
}

/// 중계받은 뿌리 가운데 물은 채널의 것이고, 받은 키에서 계산한 ID가 아직 기다리는(물은) ID인 것만 저장합니다. 답한
/// 멤버가 다른 키를 보내도 ID가 키에서 나오므로 그 ID의 메시지를 엉뚱한 키로 열지 못합니다.
fn take_roots(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    left: &mut HashSet<ChannelRootId>,
    r: &Relayed,
) -> Vec<Need> {
    let mut got = Vec::new();
    for (c, key) in &r.roots {
        if c != channel {
            continue;
        }
        let id = keysched::channel_root_id(space, c, key);
        if left.remove(&id) && keys::put_root(node, space, c, key).is_ok() {
            got.push(Need::Record(*channel, id));
        }
    }
    got
}

/// 시험용: 답하는 멤버처럼 `roots`를 요청 기기의 상자 키로 봉인한 `SealedKeys` 바이트(키가 맞는지는 보지 않음).
pub(crate) fn seal_roots_for(
    node: &Node,
    space: &SpaceId,
    user: &yangpa_core::ids::UserId,
    device: &DeviceId,
    roots: Vec<(ChannelId, Secret32)>,
) -> Option<Vec<u8>> {
    let state = node.space_state(space)?;
    let r = Relayed {
        v: RELAY_VERSION,
        roots,
        group: None,
        state_keys: Vec::new(),
        members: None,
    };
    match seal_relayed(node, &state, space, user, device, &r) {
        YpResponse::SealedKeys(b) => Some(b),
        _ => None,
    }
}

/// 시험용: 물은 것처럼 `ids`를 기다리는 상태에서 중계 답 `bytes`를 받아들입니다([`ask_roots`]와 같은 확인).
/// 반환: 저장한 뿌리 수.
pub(crate) async fn accept_roots(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    ids: Vec<ChannelRootId>,
    bytes: &[u8],
) -> usize {
    let Some(r) = open_relayed(node, &space, bytes) else {
        return 0;
    };
    let mut left: HashSet<ChannelRootId> = ids.into_iter().collect();
    let got = take_roots(node, &space, &channel, &mut left, &r);
    let n = got.len();
    if n > 0 {
        ingest::wake_needs(node, space, got).await;
    }
    n
}

fn state_key_key(id: &StateKeyId) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(id.as_bytes());
    k[16] = 1;
    k
}

/// 상태 키를 물을 피어: 상태가 있으면 활성 멤버 기기, 아직 없으면(가입 중) 받아 준 노드와 내 다른 기기.
fn state_key_peers(node: &Node, space: &SpaceId) -> Vec<PeerId> {
    if node.space_state(space).is_some() {
        return candidates(node, space, |_, _| true);
    }
    let Ok(me) = node.me() else {
        return Vec::new();
    };
    let mut v: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(p, u, d)| {
            *d != me.device_id()
                && (*u == me.user() || crate::crews::join_peer_ok(node, space, p, u))
        })
        .map(|(p, _, _)| p)
        .collect();
    v.dedup();
    v.truncate(ASK_PEERS);
    v
}

/// 열지 못한 상태 이벤트의 크루 상태 키를 멤버에게 묻습니다(백그라운드). 묶음·키 갱신이 곧 올 수 있어 잠깐 기다렸다가
/// 그래도 기다리는 ID만 묻고, 받은 키는 지금 키 후보로 올리지 않고 저장한 뒤 기다리던 이벤트를 다시 엽니다.
pub(crate) fn ask_state_keys(node: &Arc<Node>, space: SpaceId, ids: Vec<StateKeyId>) {
    if node
        .keygroup
        .relay_hold
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let want: Vec<[u8; 32]> = fresh(node, &space, ids.iter().map(state_key_key));
    let ids: Vec<StateKeyId> = ids
        .into_iter()
        .filter(|id| want.contains(&state_key_key(id)))
        .collect();
    if ids.is_empty() {
        return;
    }
    node.spawn_detached(move |node| async move {
        tokio::time::sleep(ASK_DELAY).await;
        let mut left: HashSet<StateKeyId> = {
            // 적용 잠금을 먼저(긴 적용 동안 이 작업이 스레드를 묶지 않게).
            let _l = ingest::lock_space(&node, space).await;
            let Some(core) = node.spaces.read().get(&space).map(|rt| rt.core.clone()) else {
                return;
            };
            let c = core.lock();
            ids.into_iter()
                .filter(|id| c.waiting.contains_key(&Need::StateKey(*id)))
                .collect()
        };
        for peer in state_key_peers(&node, &space) {
            if left.is_empty() {
                break;
            }
            let batch: Vec<Vec<StateKeyId>> = left
                .iter()
                .copied()
                .collect::<Vec<_>>()
                .chunks(MAX_STATE_KEY_IDS)
                .map(<[StateKeyId]>::to_vec)
                .collect();
            for ids in batch {
                let req = YpRequest::GetStateKeys { space, ids };
                let bytes = match node.net.request(peer, req).await {
                    Ok(YpResponse::SealedKeys(b)) => b,
                    Ok(_) => continue,
                    Err(e) => {
                        debug!(error = %e, "상태 키 묻기 실패");
                        break;
                    }
                };
                let Some(r) = open_relayed(&node, &space, &bytes) else {
                    continue;
                };
                let got = take_state_keys(&node, &space, &mut left, &r);
                if !got.is_empty() {
                    ingest::wake_needs(&node, space, got).await;
                }
            }
        }
    });
}

/// 중계받은 상태 키 가운데 계산한 ID가 아직 기다리는(물은) ID인 것만 저장합니다(지금 키 후보로 올리지 않음).
fn take_state_keys(
    node: &Node,
    space: &SpaceId,
    left: &mut HashSet<StateKeyId>,
    r: &Relayed,
) -> Vec<Need> {
    let mut got = Vec::new();
    for key in &r.state_keys {
        let id = state_key_id(space, key);
        if left.remove(&id) && keys::put_state_key(node, space, &id, key, false, None).is_ok() {
            got.push(Need::StateKey(id));
        }
    }
    got
}

/// 놓친 그룹의 지금 T·R을 그 사슬 끝 멤버에게 묻습니다(백그라운드). 적용 잠금 안(정리 중)에서 부르므로 사슬 끝은
/// 여기서 읽고, 묻는 일은 따로 돕니다.
pub(crate) fn ask_group(
    node: &Arc<Node>,
    space: SpaceId,
    group: KeyGroupId,
    keys: &MemberState,
    resolver: &Resolver,
) {
    let tip = if group == KeyGroupId::crew(&space) {
        resolver
            .key_view()
            .map(|kv| (kv.crew.seq, kv.crew.root_commit, kv.crew.members.clone()))
    } else {
        keys.chains.get(&group).and_then(|ch| {
            let t = ch.send_tip()?;
            // 멤버를 모르면(체크포인트 키 머리에서 시작) 크루 멤버에게 묻습니다(자격 없는 기기는 거절함).
            let members = if t.members_known {
                t.members
            } else {
                resolver.key_view()?.crew.members.clone()
            };
            Some((t.seq, t.root_commit, members))
        })
    };
    let Some((seq, root_commit, members)) = tip else {
        return;
    };
    if seq == 0 {
        return;
    }
    let kind = keys.group_kind(&space, &group);
    let mut k = *blake3::hash(group.as_bytes()).as_bytes();
    k[..8].copy_from_slice(&seq.to_le_bytes());
    if fresh(node, &space, [k]).is_empty() {
        return;
    }
    node.spawn_detached(move |node| async move {
        // 묶음(받은편지함·조각 블롭)은 갱신 바로 뒤에 오는 것이 보통이라 잠깐 기다렸다가, 그래도 막혀 있을 때만 묻습니다.
        // 물은 멤버들도 아직 못 받았을 수 있어, 막혀 있는 동안 [`ASK_ROUNDS`]번까지 [`ASK_AGAIN`]마다 다시 묻습니다
        // (같은 순번은 막힐 때 한 번만 부르므로, 여기서 끝내면 다음 갱신까지 멈춤).
        for round in 0..ASK_ROUNDS {
            tokio::time::sleep(if round == 0 { ASK_DELAY } else { ASK_AGAIN }).await;
            let eligible = |d: &DeviceId| members.contains(d);
            if ask_group_once(&node, space, group, kind, seq, root_commit, &eligible).await {
                return;
            }
        }
    });
}

/// [`ask_group`]의 한 바퀴. 더 물을 까닭이 없으면(받았거나 더는 막히지 않음) 참.
async fn ask_group_once(
    node: &Arc<Node>,
    space: SpaceId,
    group: KeyGroupId,
    kind: GroupKind,
    seq: u64,
    root_commit: [u8; 32],
    eligible: &(dyn Fn(&DeviceId) -> bool + Sync),
) -> bool {
    let still = {
        let _l = ingest::lock_space(node, space).await;
        let Some(core) = node.spaces.read().get(&space).map(|rt| rt.core.clone()) else {
            return true;
        };
        core.lock()
            .keys
            .groups
            .get(&group)
            .is_some_and(|g| g.stuck == Some(seq))
    };
    if !still {
        return true;
    }
    let Ok(me) = node.me() else { return true };
    for peer in candidates(node, &space, |_, d| eligible(d)) {
        let req = YpRequest::GetGroupKeys { space, group };
        let bytes = match node.net.request(peer, req).await {
            Ok(YpResponse::SealedKeys(b)) => b,
            Ok(_) => {
                debug!(space = %space.short(), "그룹 키 묻기에 키 대신 다른 답(자격 없음·나도 없음 등)");
                continue;
            }
            Err(e) => {
                debug!(error = %e, "그룹 키 묻기 실패");
                continue;
            }
        };
        let Some(r) = open_relayed(node, &space, &bytes) else {
            continue;
        };
        let Some(g) = r.group.filter(|g| g.group == group) else {
            continue;
        };
        if g.seq != seq || g.root_commit != root_commit {
            // 그 사이 사슬이 나아갔거나 다른 갈래: 다음에 다시 묻습니다.
            continue;
        }
        let Ok(k) = MemberKeys::from_root(
            space,
            group,
            kind,
            me.device_id(),
            g.seq,
            &g.root_commit,
            g.t,
            g.r,
        ) else {
            debug!(space = %space.short(), "중계받은 그룹 키가 뿌리 약속과 맞지 않음");
            continue;
        };
        let members = g.members;
        ingest::offer_keys(node, space, move |m| {
            m.offer_root(k);
            if let Some(list) = members {
                m.learn_members(group, seq, &root_commit, &list);
            }
        })
        .await;
        return true;
    }
    false
}

/// 체크포인트 키 머리에서 세워 바닥 멤버를 모르는 그룹의 멤버 목록을 그 그룹을 볼 수 있는 멤버에게 묻습니다(백그라운드,
/// 같은 순번은 [`ASK_AGAIN`]마다 한 번). 받은 목록은 사슬이 머리의 멤버 요약과 맞을 때만 씁니다(누가 답했든 같음).
pub(crate) fn ask_members(
    node: &Arc<Node>,
    space: SpaceId,
    group: KeyGroupId,
    keys: &MemberState,
    _resolver: &Resolver,
) {
    let Some(t) = keys.chains.get(&group).map(|ch| ch.tip()) else {
        return;
    };
    if t.members_known || t.seq == 0 {
        return;
    }
    let mut k = *blake3::hash(group.as_bytes()).as_bytes();
    k[..8].copy_from_slice(&t.seq.to_le_bytes());
    k[8] ^= 0x6d;
    if fresh(node, &space, [k]).is_empty() {
        return;
    }
    let (seq, root_commit) = (t.seq, t.root_commit);
    node.spawn_detached(move |node| async move {
        let can_view = |u: &yangpa_core::ids::UserId| {
            node.space_state(&space)
                .is_some_and(|s| s.can_view_key_group(&space, &group, u))
        };
        for round in 0..ASK_ROUNDS {
            if round > 0 {
                tokio::time::sleep(ASK_AGAIN).await;
            }
            for peer in candidates(&node, &space, |u, _| can_view(u)) {
                let req = YpRequest::GetGroupMembers {
                    space,
                    group,
                    seq,
                    root_commit,
                };
                let Ok(YpResponse::SealedKeys(bytes)) = node.net.request(peer, req).await else {
                    continue;
                };
                let Some(r) = open_relayed(&node, &space, &bytes) else {
                    continue;
                };
                let Some((g, s, rc, list)) = r.members else {
                    continue;
                };
                if g != group || s != seq || rc != root_commit {
                    continue;
                }
                let ok = ingest::offer_keys(&node, space, move |m| {
                    m.learn_members(group, seq, &root_commit, &list)
                })
                .await;
                if ok {
                    return;
                }
            }
        }
    });
}
