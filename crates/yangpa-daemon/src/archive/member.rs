//! 멤버 쪽: 보관 노드 지정·해제, 명단·내 이벤트·첨부 알림·삭제 반영 보내기, 해제 알리기, 보관 노드를 시더처럼 쓰기.
//!
//! - 주기 작업([`tick`], `space.updated`·삭제·첨부가 깨움): 지정마다 보관 노드에 닿아 인증하고(못 닿으면 물러남),
//!   명단, 첨부 알림, 쌓인 삭제 반영을 보냅니다. 명단은 권한자가 내용이 바뀌었을 때, 추방·밴 권한자가 내용이 바뀌었을
//!   때, 그 밖의 멤버는 자기 항목(기기)이 바뀌었을 때 보내고, 보내기 전에 보관 노드와 공간 스트림을 맞춥니다. 보관 노드가
//!   더 새 명단을 가졌다고 답하면(`Expired`) 물러났다 다시 보냅니다. 상태에서 사라진 지정은 권한자가 `Release`를 닿을
//!   때까지 다시 보냅니다.
//! - 명단 차이([`send_authority`]): 체크포인트가 있으면 권한자는 바탕 체크포인트 직후 상태로 만든 **바탕 명단**(누가
//!   만들어도 같음)을 한 번 보내고, 그 뒤 멤버만 바뀌면 그 위의 차이(`RosterDelta`, 변경당 약 100B)만 보냅니다. 권한자·
//!   관리자·채널이 바뀌거나 차이를 받지 않으면 통째 명단입니다. 주기적으로 통째 명단을 다시 보내지 않습니다. 가입을 받아
//!   준 멤버는 가입자를 넣는 차이를 바로 보냅니다([`add_joiner`]).
//! - 보관 노드에는 내가 쓴 이벤트만 직접 보내고(`forward`), 남의 이벤트는 보관 노드가 따라잡기로 가져갑니다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::debug;
use yangpa_core::archive::{
    ArchiveBlobRef, ArchiveDesignate, ArchiveEntry, ArchiveGrant, ArchiveRequest, ArchiveRoster,
    ArchiveRosterDelta, MAX_ARCHIVES, MAX_DELTA_CHANGES, MAX_NAME_CHARS, MAX_PRIVATE_CHANNELS,
    MAX_PRUNE_EVENTS, MAX_REF_BLOBS, MAX_REF_EVENTS, ROSTER_VERSION, RosterMember, roster_hash,
};
use yangpa_core::body::EventBody;
use yangpa_core::event::{KeyRef, SignedEvent, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::links::{ArchiveOfferLink, ContactInfo};
use yangpa_core::model::{ChannelKind, Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{MAX_EVENTS_PER_MESSAGE, PeerAuth, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::{Multiaddr, PeerId};
use yangpa_rpc::api::Notification;
use yangpa_sync::archive::{self as rules, RosterInput};
use yangpa_sync::policy::{SeedRequest, TokenBucket};

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::node::Node;
use crate::{blobs, crews, peers, policy, sync_service};

/// 상태에서 본 지정(사라진 것을 알아채 `Release`를 보내려고 공간마다 기억).
const KNOWN_PREFIX: &str = "archive.known.";
/// 보낼 해제(`archive.release.<공간>.<기기>`).
const RELEASE_PREFIX: &str = "archive.release.";
/// 보낼 삭제 반영(`archive.prune.<공간>.<기기>`).
const PRUNE_PREFIX: &str = "archive.prune.";
/// 보관 기기마다 쌓아 둘 삭제 반영 수(넘으면 오래된 것부터 버림).
const MAX_PENDING_PRUNES: usize = 10_000;
/// 첨부 알림을 받지 않은(이벤트가 아직 없음 등) 메시지를 다시 알리는 횟수.
const MAX_REF_ATTEMPTS: u8 = 5;
/// 기억해 둘 첨부 알림 수(넘으면 비우고 다음에 다시 알림, 보관 노드는 같은 알림을 그대로 받음).
const MAX_ANNOUNCED: usize = 100_000;
/// 닿지 못한 보관 노드에 다시 시도하는 최대 간격.
const MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);
/// 보관 노드에 보낸 내 명단 차이 사슬(`archive.delta.<공간>.<기기>`).
const DELTA_PREFIX: &str = "archive.delta.";

/// 보낸 명단 기록.
#[derive(Clone, Copy, Debug)]
struct SentRoster {
    /// 명단 내용(순서·시각을 뺀) 해시.
    hash: [u8; 32],
    /// 내 항목(사용자·기기) 해시.
    own: [u8; 32],
}

/// 명단을 보내는 쪽.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sender {
    /// 소유자·`MANAGE_SPACE`: 통째 명단.
    Authority,
    /// 추방·밴 권한자: 권한자가 아닌 멤버를 빼는 것만 반영됨.
    Warden,
    /// 그 밖의 멤버: 자기 기기·자기를 빼는 것만 반영됨.
    Member,
}

/// 지정 하나(해제 알림에 필요한 것만).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct KnownGrant {
    device: DeviceId,
    user: UserId,
    contact: ContactInfo,
}

impl KnownGrant {
    fn of(g: &ArchiveGrant) -> Self {
        KnownGrant {
            device: g.device,
            user: g.user,
            contact: g.contact.clone(),
        }
    }
}

/// 보관 노드에 보낸 내 차이 사슬: 바탕 명단 해시와 내 마지막 차이(순번, 해시). 재시작해도 이어야 하므로 저장합니다
/// (순번을 1부터 다시 쓰면 보관 노드가 이미 받은 차이로 보고 버림).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DeltaChain {
    base: [u8; 32],
    seq: u64,
    prev: [u8; 32],
}

impl DeltaChain {
    fn fresh(base: [u8; 32]) -> Self {
        DeltaChain {
            base,
            seq: 0,
            prev: base,
        }
    }
}

#[derive(Default)]
pub(super) struct MemberState {
    /// 보관 노드가 가진 것으로 아는 명단 내용(권한자: 다음 차이를 셈). 재시작하면 바탕 명단에서 다시 셉니다.
    described: Mutex<HashMap<(SpaceId, DeviceId), Arc<ArchiveRoster>>>,
    /// 보낸 명단(멤버는 처음 본 자기 항목). 바뀔 때만 다시 보냅니다.
    sent_rosters: Mutex<HashMap<(SpaceId, DeviceId), SentRoster>>,
    /// 보관 노드가 받지 않은 명단을 다시 보낼 시각과 간격.
    roster_retry: Mutex<HashMap<(SpaceId, DeviceId), (Instant, Duration)>>,
    /// 첨부 알림 (보관 기기, 메시지) → 보낸 횟수. [`MAX_REF_ATTEMPTS`]면 받았거나 포기함.
    announced: Mutex<HashMap<(DeviceId, EventId), u8>>,
    /// 닿지 못한 보관 기기: 다음 시도 시각과 간격.
    backoff: Mutex<HashMap<DeviceId, (Instant, Duration)>>,
    /// 연결·인증을 시도 중인 보관 기기.
    reaching: Mutex<HashSet<DeviceId>>,
    /// 지정을 본 적 있는 공간(해제를 알아채려고 지정이 없어져도 한 번 더 봄).
    known_spaces: Mutex<HashSet<SpaceId>>,
}

/// 저장된 기록을 읽고 알림을 듣기 시작합니다.
pub(super) fn start(node: &Arc<Node>) {
    let known: HashSet<SpaceId> = node
        .store
        .settings_with_prefix(KNOWN_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, _)| k[KNOWN_PREFIX.len()..].parse().ok())
        .collect();
    node.archive.member.known_spaces.lock().extend(known);
    let mut rx = node.notify.subscribe();
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        loop {
            let n = match rx.recv().await {
                Ok(n) => n,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let Some(node) = weak.upgrade() else { break };
            match n {
                Notification::SpaceUpdated { space } if involved(&node, &space) => {
                    super::wake(&node)
                }
                Notification::MessageUpdated {
                    space,
                    channel,
                    message,
                } if message.deleted && !message.volatile => {
                    on_deleted(&node, space, channel, message.id, message.author)
                }
                _ => {}
            }
        }
    });
}

/// 이 공간에 지정이 있거나 있었는지.
fn involved(node: &Node, space: &SpaceId) -> bool {
    node.space_state(space)
        .is_some_and(|s| !s.archives.is_empty())
        || node.archive.member.known_spaces.lock().contains(space)
}

// ───────────── 고리 ─────────────

/// 요청한 기기가 이 상태에서 지정된 보관 노드인지(멤버가 아니어야 함).
pub(crate) fn is_active(state: &SpaceState, user: &UserId, device: &DeviceId) -> bool {
    rules::grant_for(state, user, device).is_some() && !state.is_member(user)
}

/// 내가 멤버인 공간 중 하나에서 지정된 보관 기기인지.
pub(super) fn trusts(node: &Node, user: &UserId, device: &DeviceId) -> bool {
    let Ok(me) = node.me() else { return false };
    node.space_ids().iter().any(|s| {
        node.space_state(s)
            .is_some_and(|st| st.is_member(&me.user()) && is_active(&st, user, device))
    })
}

/// 시더 검사 고리(`sync_service::check`): 요청자가 지정된 보관 노드면 보관 규칙으로 판단합니다. 아니면 `None`.
#[allow(clippy::result_large_err)]
pub(crate) fn check_seed(
    node: &Node,
    space: &SpaceId,
    req: &SeedRequest<'_>,
) -> Option<std::result::Result<(), YpResponse>> {
    if req.state.archives.is_empty() {
        return None;
    }
    rules::grant_for(req.state, &req.requester, &req.requester_device)?;
    let p = policy::seeder_policy(node, space);
    let mut bucket = node.sync.bucket.lock();
    let bucket = bucket.get_or_insert_with(|| TokenBucket::new(p.upload_limit_bps, req.now_ms));
    rules::check_archive_seed(&p, req, bucket).map(|r| r.map_err(|e| e.to_response()))
}

/// 이 공간의 지정된 보관 노드 중 지금 인증된 연결이 있는 피어(블롭 스웜·따라잡기 상대).
pub(crate) fn archive_peers(node: &Node, state: &SpaceState) -> Vec<PeerId> {
    state
        .archives
        .values()
        .filter_map(|e| authed_peer(node, &e.grant))
        .collect()
}

/// 지정된 보관 기기의 인증된 피어(그 피어가 지정의 사용자·기기로 인증했을 때만).
fn authed_peer(node: &Node, g: &ArchiveGrant) -> Option<PeerId> {
    let p = node.peers.authed_peer_of_device(&g.device)?;
    (node.peers.identity_of(&p) == Some((g.user, g.device))).then_some(p)
}

/// 이 이벤트를 보관 노드에 줘도 되는지(보관 범위).
fn in_scope(state: &SpaceState, g: &ArchiveGrant, ev: &SignedEvent) -> bool {
    if matches!(ev.header.key, KeyRef::Volatile { .. }) {
        return false;
    }
    match ev.header.stream() {
        StreamId::Space => true,
        StreamId::Channel(c) => rules::is_archived(state, g, &c),
        // 받은편지함은 보관 노드가 그 사용자의 기기에게만 내줍니다(가입자가 오프라인이어도 키를 받게). 키 그룹은 보관하지
        // 않습니다(그 그룹 채널을 볼 수 있는 기기끼리만).
        StreamId::Inbox(_) => true,
        StreamId::KeyGroup(_) => false,
    }
}

/// 전파 고리(`sync_service::propagate`): 내가 쓴 이벤트를 인증된 보관 노드에 바로 보냅니다(못 보낸 것은 보관 노드의
/// 따라잡기가 가져감).
pub(crate) fn forward(node: &Arc<Node>, space: SpaceId, events: &[SignedEvent]) {
    let Some(state) = node.space_state(&space) else {
        return;
    };
    if state.archives.is_empty() {
        return;
    }
    let Ok(me) = node.me() else { return };
    let mut message = false;
    for e in state.archives.values() {
        let g = &e.grant;
        let Some(peer) = authed_peer(node, g) else {
            continue;
        };
        let evs: Vec<SignedEvent> = events
            .iter()
            .filter(|ev| ev.header.author == me.device_id() && in_scope(&state, g, ev))
            .cloned()
            .collect();
        if evs.is_empty() {
            continue;
        }
        message |= evs.iter().any(|ev| ev.header.channel.is_some());
        node.spawn_detached(move |node| async move {
            for chunk in evs.chunks(MAX_EVENTS_PER_MESSAGE) {
                let req = YpRequest::PushEvents {
                    space,
                    events: chunk.to_vec(),
                };
                match node.net.request(peer, req).await {
                    Ok(YpResponse::Ok) => {}
                    Ok(r) => {
                        debug!(peer = %peer, response = ?response_kind(&r), "보관 노드가 이벤트를 받지 않음");
                        return;
                    }
                    Err(e) => {
                        debug!(peer = %peer, error = %e, "보관 노드에 이벤트 보내기 실패");
                        return;
                    }
                }
            }
        });
    }
    // 첨부 알림은 보관 노드가 그 메시지를 받은 뒤에 받으므로 주기 작업에 맡깁니다.
    if message {
        super::wake(node);
    }
}

/// 응답 종류만(로그용, 내용 없이).
fn response_kind(r: &YpResponse) -> String {
    match r {
        YpResponse::Error { code, .. } => format!("{code:?}"),
        YpResponse::Busy { .. } => "Busy".into(),
        _ => "다른 응답".into(),
    }
}

/// `seed.coverage` 고리: 채널마다 그 채널을 제공하는 온라인 보관 노드 수(`GetHeads`에 답하면 셈).
pub(crate) async fn coverage(
    node: &Arc<Node>,
    space: &SpaceId,
    state: &SpaceState,
    channels: &[ChannelId],
) -> BTreeMap<ChannelId, u32> {
    const PROBE: Duration = Duration::from_secs(3);
    let mut out: BTreeMap<ChannelId, u32> = channels.iter().map(|c| (*c, 0)).collect();
    let mut probes = Vec::new();
    for e in state.archives.values() {
        let Some(peer) = authed_peer(node, &e.grant) else {
            continue;
        };
        for c in channels
            .iter()
            .filter(|c| rules::is_archived(state, &e.grant, c))
        {
            let (node, space, c) = (node.clone(), *space, *c);
            probes.push(async move {
                let req = YpRequest::GetHeads {
                    space,
                    stream: StreamId::Channel(c),
                };
                let r = tokio::time::timeout(PROBE, node.net.request(peer, req)).await;
                let ok = matches!(
                    r,
                    Ok(Ok(YpResponse::Heads { .. } | YpResponse::Busy { .. }))
                );
                (c, ok)
            });
        }
    }
    for (c, ok) in futures::future::join_all(probes).await {
        if ok && let Some(n) = out.get_mut(&c) {
            *n += 1;
        }
    }
    out
}

// ───────────── 삭제 반영 ─────────────

/// 메시지가 지워짐(`message.updated`): 내가 작성자·운영진·권한자면 보관 노드마다 삭제 반영을 쌓습니다.
fn on_deleted(node: &Arc<Node>, space: SpaceId, channel: ChannelId, id: EventId, author: UserId) {
    let Some(state) = node.space_state(&space) else {
        return;
    };
    if state.archives.is_empty() {
        return;
    }
    let Ok(me) = node.me() else { return };
    let me = me.user();
    let allowed = author == me
        || state
            .base_permissions(&me)
            .contains(Permissions::MANAGE_MESSAGES)
        || rules::authorities(&state).contains(&me);
    if !allowed {
        return;
    }
    for e in state.archives.values() {
        if !rules::is_archived(&state, &e.grant, &channel) {
            continue;
        }
        let key = prune_key(&space, &e.grant.device);
        let mut v: Vec<EventId> = node
            .store
            .get_setting(&key)
            .ok()
            .flatten()
            .unwrap_or_default();
        if v.contains(&id) {
            continue;
        }
        v.push(id);
        if v.len() > MAX_PENDING_PRUNES {
            let over = v.len() - MAX_PENDING_PRUNES;
            v.drain(..over);
        }
        if let Err(e) = node.store.put_setting(&key, &v) {
            debug!(error = %e, "삭제 반영 저장 실패");
        }
    }
    super::wake(node);
}

fn prune_key(space: &SpaceId, device: &DeviceId) -> String {
    format!("{PRUNE_PREFIX}{space}.{device}")
}

fn release_key(space: &SpaceId, device: &DeviceId) -> String {
    format!("{RELEASE_PREFIX}{space}.{device}")
}

// ───────────── 명단 ─────────────

/// 지금 상태(또는 지정을 넣어 본 상태)로 이 보관 노드에 보낼 명단.
fn build_roster(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    grant: &ArchiveGrant,
) -> Result<ArchiveRoster> {
    let genesis = node
        .spaces
        .read()
        .get(space)
        .and_then(|rt| rt.genesis)
        .ok_or_else(|| Error::conflict("이 공간의 시작 이벤트를 아직 모릅니다"))?;
    // 공간 스트림 기준(논리 시계는 공간 전체에서 하나라 비공개 채널 이벤트가 섞이면 멤버마다 크게 달라짐).
    let anchor = node.store.stream_max_lamport(space, StreamId::Space)?;
    let since = |u: &UserId| sync_service::requester_since(node, space, u);
    Ok(rules::build_roster(
        state,
        grant,
        RosterInput {
            space: *space,
            genesis,
            archive: grant.device,
            anchor,
            issued_at: Timestamp::now(),
            since: &since,
        },
    ))
}

/// 명단 내용 해시(순서·발행 시각 빼고): 같으면 다시 보내지 않습니다.
fn content_hash(r: &ArchiveRoster) -> [u8; 32] {
    let mut c = r.clone();
    c.anchor = 0;
    c.issued_at = Timestamp(0);
    *blake3::hash(&postcard::to_stdvec(&c).unwrap_or_default()).as_bytes()
}

/// 내 항목(사용자·기기) 해시.
fn own_hash(r: &ArchiveRoster, me: &UserId) -> [u8; 32] {
    *blake3::hash(&postcard::to_stdvec(&r.member(me)).unwrap_or_default()).as_bytes()
}

/// 이 명단을 보내야 하는지. 멤버는 처음 본 자기 항목을 기준으로 기록만 합니다.
fn roster_wanted(
    node: &Node,
    key: (SpaceId, DeviceId),
    roster: &ArchiveRoster,
    role: Sender,
    me: &UserId,
) -> bool {
    let mut sent = node.archive.member.sent_rosters.lock();
    let prev = sent.get(&key).copied();
    match role {
        Sender::Authority => prev.is_none_or(|p| p.hash != content_hash(roster)),
        Sender::Warden => prev.is_none_or(|p| p.hash != content_hash(roster)),
        Sender::Member => {
            let own = own_hash(roster, me);
            match prev {
                Some(p) => p.own != own,
                None => {
                    sent.insert(
                        key,
                        SentRoster {
                            hash: content_hash(roster),
                            own,
                        },
                    );
                    false
                }
            }
        }
    }
}

fn roster_retry_due(node: &Node, key: &(SpaceId, DeviceId)) -> bool {
    node.archive
        .member
        .roster_retry
        .lock()
        .get(key)
        .is_none_or(|(at, _)| Instant::now() >= *at)
}

fn note_roster_retry(node: &Node, key: (SpaceId, DeviceId)) {
    let start = node.cfg.sync_interval.min(Duration::from_secs(30));
    let mut r = node.archive.member.roster_retry.lock();
    let next = r
        .get(&key)
        .map_or(start, |(_, d)| (*d * 2).min(MAX_BACKOFF));
    r.insert(key, (Instant::now() + next, next));
}

async fn send_roster(
    node: &Arc<Node>,
    space: SpaceId,
    grant: &ArchiveGrant,
    peer: PeerId,
    role: Sender,
) -> Result<()> {
    let key = (space, grant.device);
    let me = node.me()?;
    let me_user = me.user();
    if !roster_retry_due(node, &key) {
        return Ok(());
    }
    let Some(state) = node.space_state(&space) else {
        return Ok(());
    };
    let roster = build_roster(node, &space, &state, grant)?;
    if !roster_wanted(node, key, &roster, role, &me_user) {
        return Ok(());
    }
    // 재시작·오프라인 뒤 따라잡지 못한 상태로 명단을 내지 않게 보관 노드와 공간 스트림을 먼저 맞추고 다시 만듭니다.
    sync_service::sync_space_stream(node, space, peer).await?;
    let Some(state) = node.space_state(&space) else {
        return Ok(());
    };
    if !state.is_member(&me_user) || !state.archives.contains_key(&grant.device) {
        return Ok(());
    }
    let roster = build_roster(node, &space, &state, grant)?;
    let sent = SentRoster {
        hash: content_hash(&roster),
        own: own_hash(&roster, &me_user),
    };
    if role == Sender::Authority {
        match send_authority(node, space, grant, peer, &roster).await {
            Ok(true) => {
                node.archive.member.sent_rosters.lock().insert(key, sent);
                node.archive.member.roster_retry.lock().remove(&key);
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => debug!(error = %e, "명단 차이를 보내지 못해 통째로 보냄"),
        }
    }
    match send_full(node, peer, &roster).await? {
        YpResponse::Ok => {
            if role == Sender::Authority {
                // 보관 노드의 바탕이 이 명단이 됨: 이어지는 차이는 이 명단 위에서.
                set_chain(node, key, Some(DeltaChain::fresh(roster_hash(&roster))));
                node.archive
                    .member
                    .described
                    .lock()
                    .insert(key, Arc::new(roster));
            }
            node.archive.member.sent_rosters.lock().insert(key, sent);
            node.archive.member.roster_retry.lock().remove(&key);
            Ok(())
        }
        // 보관 노드의 명단이 더 새로움: 받은 것으로 기록하지 않고 따라잡은 뒤 다시 보냅니다.
        r => {
            note_roster_retry(node, key);
            Err(Error::Network(format!(
                "보관 노드가 명단을 받지 않음: {}",
                response_kind(&r)
            )))
        }
    }
}

async fn send_full(node: &Node, peer: PeerId, roster: &ArchiveRoster) -> Result<YpResponse> {
    let me = node.me()?;
    let signed = roster.clone().sign(&me.device.signing)?;
    let req = YpRequest::Archive(Box::new(ArchiveRequest::Roster(Box::new(signed))));
    Ok(node.net.request(peer, req).await?)
}

// ───────────── 명단 차이 ─────────────

fn delta_key(space: &SpaceId, device: &DeviceId) -> String {
    format!("{DELTA_PREFIX}{space}.{device}")
}

fn chain_of(node: &Node, key: (SpaceId, DeviceId)) -> Option<DeltaChain> {
    node.store
        .get_setting(&delta_key(&key.0, &key.1))
        .ok()
        .flatten()
}

fn set_chain(node: &Node, key: (SpaceId, DeviceId), chain: Option<DeltaChain>) {
    let k = delta_key(&key.0, &key.1);
    let r = match chain {
        Some(c) => node.store.put_setting(&k, &c),
        None => node.store.delete_setting(&k).map(|_| ()),
    };
    if let Err(e) = r {
        debug!(error = %e, "명단 차이 사슬 저장 실패");
    }
}

/// 바탕 체크포인트 직후 상태로 만든 이 보관 노드의 바탕 명단. 같은 바탕을 받은 멤버는 누구나 같은 명단(같은 해시)을
/// 만듭니다: 순서는 체크포인트 이벤트의 논리 시계, 발행 시각은 그 이벤트의 작성 시각. 체크포인트가 없거나 그때 이
/// 보관 노드가 지정돼 있지 않았으면 `None`.
fn base_roster(node: &Node, space: &SpaceId, archive: &DeviceId) -> Option<ArchiveRoster> {
    let (genesis, genesis_lamport, core) = {
        let spaces = node.spaces.read();
        let rt = spaces.get(space)?;
        (rt.genesis?, rt.genesis_lamport, rt.core.clone())
    };
    let (base, state, since_map) = {
        let c = core.lock();
        let base = c.resolver.base()?;
        let (st, since) = c.resolver.base_state()?;
        (base, st, since)
    };
    let grant = state.archives.get(archive)?.grant.clone();
    let created = node.store.get_event(&base.event).ok()??.header.created_at;
    let since = |u: &UserId| match (since_map.get(u), genesis_lamport) {
        (Some(s), Some(g)) if *s <= g => None,
        (Some(s), _) => Some(*s),
        (None, _) => Some(u64::MAX),
    };
    Some(rules::build_roster(
        &state,
        &grant,
        RosterInput {
            space: *space,
            genesis,
            archive: *archive,
            anchor: base.lamport,
            issued_at: created,
            since: &since,
        },
    ))
}

/// 멤버 말고 같은지(순서·시각 빼고): 같으면 차이로 보낼 수 있습니다.
fn same_frame(a: &ArchiveRoster, b: &ArchiveRoster) -> bool {
    a.space == b.space
        && a.genesis == b.genesis
        && a.archive == b.archive
        && a.owner == b.owner
        && a.authorities == b.authorities
        && a.moderators == b.moderators
        && a.wardens == b.wardens
        && a.channels == b.channels
        && a.member(&a.owner) == b.member(&b.owner)
}

/// `from`에서 `to`로 가는 멤버 차이(넣기·바꾸기, 빼기). 차이로 나타낼 수 없으면 `None`.
fn member_diff(
    from: &ArchiveRoster,
    to: &ArchiveRoster,
) -> Option<(Vec<RosterMember>, Vec<UserId>)> {
    if !same_frame(from, to) {
        return None;
    }
    let old: HashMap<UserId, &RosterMember> = from.members.iter().map(|m| (m.user, m)).collect();
    let add: Vec<RosterMember> = to
        .members
        .iter()
        .filter(|m| old.get(&m.user).is_none_or(|o| *o != *m))
        .cloned()
        .collect();
    let now: HashSet<UserId> = to.members.iter().map(|m| m.user).collect();
    let remove: Vec<UserId> = from
        .members
        .iter()
        .map(|m| m.user)
        .filter(|u| !now.contains(u))
        .collect();
    (add.len() + remove.len() <= MAX_DELTA_CHANGES).then_some((add, remove))
}

/// 차이 하나를 보냅니다. 받았으면 사슬을 잇고 true, 바탕이 다르거나 사슬이 끊겼으면(`Expired`) 사슬을 지우고 false.
async fn send_delta(
    node: &Node,
    key: (SpaceId, DeviceId),
    peer: PeerId,
    chain: DeltaChain,
    anchor: u64,
    add: Vec<RosterMember>,
    remove: Vec<UserId>,
) -> Result<bool> {
    let me = node.me()?;
    let delta = ArchiveRosterDelta {
        v: ROSTER_VERSION,
        space: key.0,
        archive: key.1,
        base: chain.base,
        seq: chain.seq + 1,
        prev_hash: chain.prev,
        anchor,
        issued_at: Timestamp::now(),
        add,
        remove,
    };
    let hash = delta.hash();
    let seq = delta.seq;
    let signed = delta.sign(&me.device.signing)?;
    let req = YpRequest::Archive(Box::new(ArchiveRequest::RosterDelta(Box::new(signed))));
    match node.net.request(peer, req).await? {
        YpResponse::Ok => {
            set_chain(
                node,
                key,
                Some(DeltaChain {
                    base: chain.base,
                    seq,
                    prev: hash,
                }),
            );
            Ok(true)
        }
        YpResponse::Error {
            code: WireErrorCode::Expired,
            ..
        } => {
            set_chain(node, key, None);
            Ok(false)
        }
        r => Err(Error::Network(format!(
            "보관 노드가 명단 차이를 받지 않음: {}",
            response_kind(&r)
        ))),
    }
}

/// 권한자: 바탕 명단 위 차이로 `roster`를 맞춥니다. 맞췄으면 true, 통째 명단을 보내야 하면 false.
///
/// 1. 아는 사슬과 그 위 명단 내용(재시작 뒤면 바탕 명단)에서 차이를 보냅니다. 사슬이 없으면 바탕 명단 위 새 사슬로
///    해 봅니다(다른 권한자가 이미 바탕 명단을 보냈을 수 있음).
/// 2. 보관 노드의 바탕이 다르면 바탕 명단을 통째로 한 번 보내고 다시 차이를 보냅니다.
async fn send_authority(
    node: &Node,
    space: SpaceId,
    grant: &ArchiveGrant,
    peer: PeerId,
    roster: &ArchiveRoster,
) -> Result<bool> {
    let key = (space, grant.device);
    let base = base_roster(node, &space, &grant.device);
    let base_hash = base.as_ref().map(roster_hash);
    let mut chain = chain_of(node, key);
    let mut described = node.archive.member.described.lock().get(&key).cloned();
    if described.is_none() && chain.is_some_and(|c| Some(c.base) == base_hash) {
        described = base.clone().map(Arc::new);
    }
    if chain.is_none()
        && let (Some(h), Some(b)) = (base_hash, &base)
    {
        chain = Some(DeltaChain::fresh(h));
        described = Some(Arc::new(b.clone()));
    }
    for attempt in 0..2 {
        if let (Some(c), Some(from)) = (chain, described.as_deref()) {
            let Some((add, remove)) = member_diff(from, roster) else {
                return Ok(false);
            };
            if add.is_empty() && remove.is_empty() {
                return Ok(true);
            }
            if send_delta(node, key, peer, c, roster.anchor, add, remove).await? {
                node.archive
                    .member
                    .described
                    .lock()
                    .insert(key, Arc::new(roster.clone()));
                return Ok(true);
            }
        }
        node.archive.member.described.lock().remove(&key);
        // 보관 노드의 바탕이 다름: 바탕 명단을 한 번 보냅니다.
        let (Some(b), Some(h)) = (&base, base_hash) else {
            return Ok(false);
        };
        if attempt > 0 {
            return Ok(false);
        }
        match send_full(node, peer, b).await? {
            YpResponse::Ok => {
                chain = Some(DeltaChain::fresh(h));
                set_chain(node, key, chain);
                described = Some(Arc::new(b.clone()));
            }
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// 가입을 받아 준 멤버: 보관 노드의 바탕 명단에 가입자를 넣는 차이를 바로 보냅니다(권한자가 다음에 명단을 보낼 때까지
/// 가입자가 보관 노드에서 기록을 받지 못하는 틈을 없앰). 체크포인트가 없거나 보관 노드의 바탕이 다르면 권한자 몫으로
/// 둡니다.
pub(crate) fn add_joiner(node: &Arc<Node>, space: SpaceId, joiner: UserId) {
    node.spawn_detached(move |node| async move {
        let Some(state) = node.space_state(&space) else {
            return;
        };
        let Some(devices) = state.devices.get(&joiner) else {
            return;
        };
        let member = RosterMember {
            user: joiner,
            devices: devices.active().map(|e| e.device).collect(),
            since: sync_service::requester_since(&node, &space, &joiner),
        };
        for e in state.archives.values() {
            let g = &e.grant;
            let key = (space, g.device);
            let Some(peer) = authed_peer(&node, g) else {
                continue;
            };
            let Some(base) = base_roster(&node, &space, &g.device) else {
                continue;
            };
            let h = roster_hash(&base);
            let chain = chain_of(&node, key)
                .filter(|c| c.base == h)
                .unwrap_or_else(|| DeltaChain::fresh(h));
            let anchor = node
                .store
                .stream_max_lamport(&space, StreamId::Space)
                .unwrap_or(0);
            let add = vec![member.clone()];
            match send_delta(&node, key, peer, chain, anchor, add, Vec::new()).await {
                Ok(true) => debug!(space = %space.short(), "보관 노드 명단에 가입자를 넣음"),
                Ok(false) => {
                    debug!(space = %space.short(), "보관 노드의 바탕 명단이 달라 권한자에게 맡김")
                }
                Err(e) => debug!(error = %e, "가입자를 넣는 명단 차이 실패"),
            }
        }
    });
}

// ───────────── 첨부 알림 ─────────────

/// 공간마다 첨부를 실은 (메시지, 채널, 블롭).
type AttachmentRefs = HashMap<SpaceId, Vec<(EventId, ChannelId, BlobId)>>;

/// 공간마다 보관 노드에 알릴 첨부: (메시지, 채널, 블롭). 키가 확인된 기록만.
fn attachment_refs(node: &Node) -> AttachmentRefs {
    let mut out: AttachmentRefs = HashMap::new();
    for blob in blobs::referenced_blobs(node) {
        for r in blobs::verified_refs(node, &blob) {
            out.entry(r.space)
                .or_default()
                .push((r.event, r.channel, blob));
        }
    }
    out
}

/// 이 보관 노드에 아직 알리지 않은 첨부(보관 범위·지워지지 않음·내가 쓴 메시지, 권한자면 모든 메시지).
fn pending_refs(
    node: &Node,
    state: &SpaceState,
    grant: &ArchiveGrant,
    refs: &[(EventId, ChannelId, BlobId)],
    me: &UserId,
    authority: bool,
) -> Vec<ArchiveBlobRef> {
    let announced = node.archive.member.announced.lock().clone();
    let mut by_event: BTreeMap<EventId, Vec<BlobId>> = BTreeMap::new();
    for (event, channel, blob) in refs {
        if announced
            .get(&(grant.device, *event))
            .is_some_and(|n| *n >= MAX_REF_ATTEMPTS)
            || !rules::is_archived(state, grant, channel)
            || node.store.is_deleted(event).unwrap_or(true)
        {
            continue;
        }
        let v = by_event.entry(*event).or_default();
        if v.len() < MAX_REF_BLOBS && !v.contains(blob) {
            v.push(*blob);
        }
    }
    by_event
        .into_iter()
        .filter(|(event, _)| {
            authority
                || node
                    .store
                    .get_event(event)
                    .ok()
                    .flatten()
                    .is_some_and(|e| e.header.user == *me)
        })
        .map(|(event, blobs)| ArchiveBlobRef { event, blobs })
        .collect()
}

/// 지금 바탕 체크포인트의 스냅숏 알림(권한자나 그 체크포인트를 쓴 사용자만): 보관 노드가 받아 두었다가 체크포인트로
/// 들어오는 멤버에게 내줍니다(내용은 상태 키로 암호화돼 보관 노드도 열지 못함).
fn snapshot_ref(
    node: &Node,
    space: &SpaceId,
    grant: &ArchiveGrant,
    me: &UserId,
    authority: bool,
) -> Option<ArchiveBlobRef> {
    let (event, blob) = crate::checkpoint::current_snapshot(node, space)?;
    if node
        .archive
        .member
        .announced
        .lock()
        .get(&(grant.device, event))
        .is_some_and(|n| *n >= MAX_REF_ATTEMPTS)
    {
        return None;
    }
    let author = node.store.get_event(&event).ok()??.header.user;
    (authority || author == *me).then(|| ArchiveBlobRef {
        event,
        blobs: vec![blob],
    })
}

async fn send_refs(
    node: &Arc<Node>,
    space: SpaceId,
    device: DeviceId,
    peer: PeerId,
    refs: Vec<ArchiveBlobRef>,
) -> Result<()> {
    for chunk in refs.chunks(MAX_REF_EVENTS) {
        let req = YpRequest::Archive(Box::new(ArchiveRequest::BlobRefs {
            space,
            refs: chunk.to_vec(),
        }));
        let accepted: HashSet<EventId> = match node.net.request(peer, req).await? {
            YpResponse::Ids(ids) => ids.into_iter().collect(),
            r => {
                return Err(Error::Network(format!(
                    "보관 노드가 첨부 알림을 받지 않음: {}",
                    response_kind(&r)
                )));
            }
        };
        let mut announced = node.archive.member.announced.lock();
        if announced.len() >= MAX_ANNOUNCED {
            announced.clear();
        }
        for r in chunk {
            let n = announced.entry((device, r.event)).or_insert(0);
            *n = if accepted.contains(&r.event) {
                MAX_REF_ATTEMPTS
            } else {
                n.saturating_add(1)
            };
        }
    }
    Ok(())
}

async fn send_prunes(
    node: &Arc<Node>,
    space: SpaceId,
    device: DeviceId,
    peer: PeerId,
) -> Result<()> {
    let key = prune_key(&space, &device);
    let pending: Vec<EventId> = node.store.get_setting(&key)?.unwrap_or_default();
    if pending.is_empty() {
        return Ok(());
    }
    let mut sent = 0;
    for chunk in pending.chunks(MAX_PRUNE_EVENTS) {
        let req = YpRequest::Archive(Box::new(ArchiveRequest::Prune {
            space,
            events: chunk.to_vec(),
        }));
        match node.net.request(peer, req).await? {
            YpResponse::Ok => sent += chunk.len(),
            r => {
                debug!(response = ?response_kind(&r), "보관 노드가 삭제 반영을 받지 않음");
                break;
            }
        }
    }
    // 보내는 사이 더 쌓였을 수 있어 다시 읽고 보낸 것만 뺍니다.
    let done: HashSet<EventId> = pending[..sent].iter().copied().collect();
    let mut now: Vec<EventId> = node.store.get_setting(&key)?.unwrap_or_default();
    now.retain(|e| !done.contains(e));
    if now.is_empty() {
        node.store.delete_setting(&key)?;
    } else {
        node.store.put_setting(&key, &now)?;
    }
    Ok(())
}

// ───────────── 닿기·인증 ─────────────

/// 보관 노드에 연결합니다(지정의 접속 정보 → 저장된 주소 → 위치 기록 → DHT).
async fn reach(
    node: &Arc<Node>,
    peer: PeerId,
    user: &UserId,
    device: &DeviceId,
    contact: &ContactInfo,
) -> bool {
    if node.peers.is_connected(&peer) {
        return true;
    }
    let mut addrs: Vec<Multiaddr> = crate::tor::contact_addrs(contact);
    if let Ok(Some(rec)) = node.store.get_peer(&peer.to_bytes()) {
        addrs.extend(rec.addrs.iter().filter_map(|a| a.parse().ok()));
    }
    if addrs.is_empty()
        && let Some((p, found)) = crate::locator::lookup(node, user, device).await
        && p == peer
    {
        addrs = found;
    }
    if addrs.is_empty()
        && let Ok(found) = node.net.find_peer(peer).await
    {
        addrs = found;
    }
    if addrs.is_empty() {
        return false;
    }
    peers::connect(node, peer, addrs).await.is_ok()
}

fn backoff_due(node: &Node, device: &DeviceId) -> bool {
    node.archive
        .member
        .backoff
        .lock()
        .get(device)
        .is_none_or(|(at, _)| Instant::now() >= *at)
}

fn note_failure(node: &Node, device: DeviceId) {
    let start = node.cfg.sync_interval.min(Duration::from_secs(30));
    let mut b = node.archive.member.backoff.lock();
    let next = b
        .get(&device)
        .map_or(start, |(_, d)| (*d * 2).min(MAX_BACKOFF));
    b.insert(device, (Instant::now() + next, next));
}

/// 보관 노드에 닿아 인증합니다(백그라운드, 기기마다 하나). 인증되면 서로 따라잡고 주기 작업을 다시 깨웁니다.
fn spawn_reach(node: &Arc<Node>, g: &ArchiveGrant) {
    let Ok(peer) = PeerId::from_bytes(&g.contact.peer_id) else {
        return;
    };
    if !backoff_due(node, &g.device) || !node.archive.member.reaching.lock().insert(g.device) {
        return;
    }
    let g = g.clone();
    node.spawn_detached(move |node| async move {
        let ok = reach(&node, peer, &g.user, &g.device, &g.contact).await
            && matches!(
                peers::ensure_auth(&node, peer).await,
                Ok((u, d)) if u == g.user && d == g.device
            );
        node.archive.member.reaching.lock().remove(&g.device);
        if ok {
            node.archive.member.backoff.lock().remove(&g.device);
            sync_service::catch_up_peer(&node, peer);
            super::wake(&node);
        } else {
            debug!(device = %g.device.short(), "보관 노드에 닿지 못함(나중에 다시)");
            note_failure(&node, g.device);
        }
    });
}

// ───────────── 주기 작업 ─────────────

pub(super) async fn tick(node: &Arc<Node>) {
    let Ok(me) = node.me() else { return };
    let me_user = me.user();
    let mut refs: Option<AttachmentRefs> = None;
    for space in node.space_ids() {
        if !involved(node, &space) {
            continue;
        }
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        if !state.is_member(&me_user) {
            continue;
        }
        let authority = rules::authorities(&state).contains(&me_user);
        let role = if authority {
            Sender::Authority
        } else if rules::wardens(&state).contains(&me_user) {
            Sender::Warden
        } else {
            Sender::Member
        };
        note_removed(node, &space, &state, authority);
        for e in state.archives.values() {
            let g = &e.grant;
            let Some(peer) = authed_peer(node, g) else {
                spawn_reach(node, g);
                continue;
            };
            if let Err(e) = send_roster(node, space, g, peer, role).await {
                debug!(error = %e, "명단 보내기 실패");
            }
            let all = refs.get_or_insert_with(|| attachment_refs(node));
            let mut pending = all
                .get(&space)
                .map(|v| pending_refs(node, &state, g, v, &me_user, authority))
                .unwrap_or_default();
            pending.extend(snapshot_ref(node, &space, g, &me_user, authority));
            if !pending.is_empty()
                && let Err(e) = send_refs(node, space, g.device, peer, pending).await
            {
                debug!(error = %e, "첨부 알림 실패");
            }
            if let Err(e) = send_prunes(node, space, g.device, peer).await {
                debug!(error = %e, "삭제 반영 보내기 실패");
            }
        }
    }
    deliver_releases(node).await;
}

/// 상태에서 사라진 지정을 찾아(권한자면) 해제 알림을 쌓고, 그 보관 기기의 인증을 지웁니다.
fn note_removed(node: &Node, space: &SpaceId, state: &SpaceState, authority: bool) {
    let key = format!("{KNOWN_PREFIX}{space}");
    let known: Vec<KnownGrant> = node
        .store
        .get_setting(&key)
        .ok()
        .flatten()
        .unwrap_or_default();
    let now: Vec<KnownGrant> = state
        .archives
        .values()
        .map(|e| KnownGrant::of(&e.grant))
        .collect();
    if known == now {
        return;
    }
    let gone: Vec<&KnownGrant> = known
        .iter()
        .filter(|k| !state.archives.contains_key(&k.device))
        .collect();
    for k in &gone {
        node.archive
            .member
            .sent_rosters
            .lock()
            .retain(|(s, d), _| !(s == space && *d == k.device));
        node.archive
            .member
            .roster_retry
            .lock()
            .retain(|(s, d), _| !(s == space && *d == k.device));
        let _ = node.store.delete_setting(&prune_key(space, &k.device));
        if authority && let Err(e) = node.store.put_setting(&release_key(space, &k.device), *k) {
            debug!(error = %e, "해제 알림 저장 실패");
        }
    }
    // 다른 이유로 믿는 기기가 아니면 인증을 지워, 그 연결의 요청을 더 받지 않게 합니다.
    let forget: Vec<DeviceId> = gone
        .iter()
        .filter(|k| !super::trusts(node, &k.user, &k.device))
        .map(|k| k.device)
        .collect();
    node.peers.forget_devices(&forget);
    let r = if now.is_empty() {
        node.archive.member.known_spaces.lock().remove(space);
        node.store.delete_setting(&key).map(|_| ())
    } else {
        node.archive.member.known_spaces.lock().insert(*space);
        node.store.put_setting(&key, &now)
    };
    if let Err(e) = r {
        debug!(error = %e, "지정 기록 저장 실패");
    }
}

/// 쌓인 해제 알림을 보냅니다(받았거나 이미 지웠으면 그만).
async fn deliver_releases(node: &Arc<Node>) {
    let pending = node
        .store
        .settings_with_prefix(RELEASE_PREFIX)
        .unwrap_or_default();
    for (key, v) in pending {
        let Some(space) = key[RELEASE_PREFIX.len()..]
            .split('.')
            .next()
            .and_then(|s| s.parse::<SpaceId>().ok())
        else {
            let _ = node.store.delete_setting(&key);
            continue;
        };
        let Ok(k) = serde_json::from_value::<KnownGrant>(v) else {
            let _ = node.store.delete_setting(&key);
            continue;
        };
        // 다시 지정됐으면 해제하지 않습니다.
        if node
            .space_state(&space)
            .is_some_and(|s| s.archives.contains_key(&k.device))
        {
            let _ = node.store.delete_setting(&key);
            continue;
        }
        if !backoff_due(node, &k.device) {
            continue;
        }
        if release(node, space, &k).await {
            let _ = node.store.delete_setting(&key);
            node.archive.member.backoff.lock().remove(&k.device);
        } else {
            note_failure(node, k.device);
        }
    }
}

/// 해제 알림 하나. 인증 전에도 받는 요청이라 이 연결에 묶은 기기 증명을 싣습니다.
async fn release(node: &Arc<Node>, space: SpaceId, k: &KnownGrant) -> bool {
    let Ok(peer) = PeerId::from_bytes(&k.contact.peer_id) else {
        return true;
    };
    let Ok(me) = node.me() else { return false };
    if !reach(node, peer, &k.user, &k.device, &k.contact).await {
        return false;
    }
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    let req = YpRequest::Archive(Box::new(ArchiveRequest::Release {
        space,
        auth: Box::new(auth),
    }));
    match node.net.request(peer, req).await {
        Ok(YpResponse::Ok) => true,
        Ok(YpResponse::Error {
            code: WireErrorCode::NotFound,
            ..
        }) => true,
        Ok(r) => {
            debug!(response = ?response_kind(&r), "보관 노드가 해제를 받지 않음");
            false
        }
        Err(e) => {
            debug!(error = %e, "해제 알림 실패");
            false
        }
    }
}

// ───────────── API ─────────────

/// `archive.designate`: 보관 노드의 제안 링크로 지정합니다. 보관 노드가 첫 명단을 받아 수락하면 `ArchiveGrant`를 냅니다.
pub(crate) async fn designate(
    node: &Arc<Node>,
    space: SpaceId,
    url: &str,
    private_channels: Vec<ChannelId>,
    name: Option<String>,
) -> Result<ArchiveEntry> {
    let link = ArchiveOfferLink::parse(url).map_err(|e| match e {
        yangpa_core::error::CoreError::Expired => Error::invalid("만료된 보관 노드 링크입니다"),
        e => Error::invalid(format!("보관 노드 링크: {e}")),
    })?;
    let (state, me_user) = crews::crew_state(node, &space)?;
    crews::need(&state, &me_user, Permissions::MANAGE_SPACE)?;
    let me = node.me()?;
    if link.user == me_user || state.is_member(&link.user) {
        return Err(Error::invalid(
            "멤버의 노드는 보관 노드로 지정할 수 없습니다(보관 노드는 멤버가 아닌 별도 신원이어야 합니다)",
        ));
    }
    if state.bans.contains_key(&link.user) {
        return Err(Error::invalid("밴된 사용자의 노드입니다"));
    }
    if !state.archives.contains_key(&link.device) && state.archives.len() >= MAX_ARCHIVES {
        return Err(Error::conflict(format!(
            "보관 노드는 크루마다 {MAX_ARCHIVES}개까지입니다"
        )));
    }
    let private_channels = check_private_channels(&state, &me_user, private_channels)?;
    let name = match name {
        Some(n) => n.trim().to_owned(),
        None if !link.name_hint.trim().is_empty() => link.name_hint.trim().to_owned(),
        None => "보관 노드".to_owned(),
    };
    if name.is_empty() {
        return Err(Error::invalid("보관 노드 이름이 비었습니다"));
    }
    crews::check_text(&name, MAX_NAME_CHARS, "보관 노드 이름")?;
    let peer = PeerId::from_bytes(&link.contact.peer_id)
        .map_err(|_| Error::invalid("링크의 노드 ID가 잘못됨"))?;
    let grant = ArchiveGrant {
        device: link.device,
        user: link.user,
        name,
        contact: link.contact.clone(),
        private_channels,
    };
    // 지정이 적용된 것으로 계산한 첫 명단(명단에는 지정 자체가 들어가지 않음).
    let mut planned = (*state).clone();
    planned.archives.insert(
        grant.device,
        ArchiveEntry {
            grant: grant.clone(),
            granted_by: me_user,
            granted_at: Timestamp::now(),
        },
    );
    let roster = build_roster(node, &space, &planned, &grant)?;
    let sent = SentRoster {
        hash: content_hash(&roster),
        own: own_hash(&roster, &me_user),
    };
    let signed = roster.sign(&me.device.signing)?;
    if !reach(node, peer, &grant.user, &grant.device, &grant.contact).await {
        return Err(Error::Network("보관 노드에 연결할 수 없습니다".into()));
    }
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    let req = YpRequest::Archive(Box::new(ArchiveRequest::Designate(Box::new(
        ArchiveDesignate {
            token: link.token,
            auth,
            roster: signed,
        },
    ))));
    match node.net.request(peer, req).await? {
        YpResponse::Ok => {}
        YpResponse::Error { code, message } => {
            return Err(match code {
                WireErrorCode::InvalidToken | WireErrorCode::Expired => {
                    Error::invalid(format!("보관 노드 링크를 쓸 수 없습니다: {message}"))
                }
                _ => Error::forbidden(format!("보관 노드가 지정을 거절했습니다: {message}")),
            });
        }
        _ => return Err(Error::internal("보관 노드의 응답이 아님")),
    }
    let body = EventBody::ArchiveGrant(grant.clone());
    if let Err(e) = author::publish(node, space, None, &body, KeyChoice::State).await {
        // 받아 둔 공간이 남지 않게 바로 해제합니다.
        release(node, space, &KnownGrant::of(&grant)).await;
        return Err(e);
    }
    node.archive
        .member
        .sent_rosters
        .lock()
        .insert((space, grant.device), sent);
    let g = grant.clone();
    node.spawn_detached(move |node| async move {
        if matches!(peers::ensure_auth(&node, peer).await, Ok((u, d)) if u == g.user && d == g.device)
        {
            sync_service::catch_up_peer(&node, peer);
        }
        super::wake(&node);
    });
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    state
        .archives
        .get(&grant.device)
        .cloned()
        .ok_or_else(|| Error::conflict("지정이 상태에 반영되지 않았습니다"))
}

/// 함께 보관할 비공개 채널 검사(해소기 규칙과 같고, 오류를 먼저 알려 줌).
fn check_private_channels(
    state: &SpaceState,
    me: &UserId,
    mut list: Vec<ChannelId>,
) -> Result<Vec<ChannelId>> {
    list.sort();
    list.dedup();
    if list.len() > MAX_PRIVATE_CHANNELS {
        return Err(Error::invalid(format!(
            "함께 보관할 비공개 채널은 {MAX_PRIVATE_CHANNELS}개까지입니다"
        )));
    }
    for c in &list {
        let ch = state
            .channels
            .get(c)
            .ok_or_else(|| Error::not_found("없는 채널"))?;
        if ch.is_volatile() {
            return Err(Error::invalid("휘발성 채널은 보관할 수 없습니다"));
        }
        if ch.kind == ChannelKind::Category || !ch.is_private() {
            return Err(Error::invalid(
                "비공개 채널(@everyone 보기 거부)만 따로 고를 수 있습니다(공개 채널은 늘 보관)",
            ));
        }
        if !state
            .channel_permissions(me, c)
            .contains(Permissions::VIEW_CHANNEL)
        {
            return Err(Error::forbidden(
                "볼 수 없는 채널은 보관하게 할 수 없습니다",
            ));
        }
    }
    Ok(list)
}

/// `archive.revoke`: `ArchiveRevoke`를 내고, 그 보관 노드에 해제를 알립니다.
pub(crate) async fn revoke(node: &Arc<Node>, space: SpaceId, device: DeviceId) -> Result<()> {
    let (state, me) = crews::crew_state(node, &space)?;
    crews::need(&state, &me, Permissions::MANAGE_SPACE)?;
    let Some(e) = state.archives.get(&device) else {
        return Err(Error::not_found("지정되지 않은 보관 노드입니다"));
    };
    let k = KnownGrant::of(&e.grant);
    author::publish(
        node,
        space,
        None,
        &EventBody::ArchiveRevoke { device },
        KeyChoice::State,
    )
    .await?;
    node.store.put_setting(&release_key(&space, &device), &k)?;
    node.archive.member.backoff.lock().remove(&device);
    super::wake(node);
    Ok(())
}

/// `archive.list`의 한 줄과 지금 온라인인지.
pub(crate) fn list(node: &Node, space: &SpaceId) -> Result<Vec<(ArchiveEntry, bool)>> {
    let (state, _) = crews::crew_state(node, space)?;
    Ok(state
        .archives
        .values()
        .map(|e| (e.clone(), authed_peer(node, &e.grant).is_some()))
        .collect())
}

/// 시험용: 보낼 삭제 반영·해제가 남았는지.
pub(crate) fn pending_counts(node: &Node, space: &SpaceId) -> (usize, usize) {
    let prunes = node
        .store
        .settings_with_prefix(&format!("{PRUNE_PREFIX}{space}."))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<Vec<EventId>>(v).ok())
        .map(|v| v.len())
        .sum();
    let releases = node
        .store
        .settings_with_prefix(&format!("{RELEASE_PREFIX}{space}."))
        .unwrap_or_default()
        .len();
    (prunes, releases)
}
