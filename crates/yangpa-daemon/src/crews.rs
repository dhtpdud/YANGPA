//! 크루(B1): 초대 가입(양쪽), 비공개 채널 그룹, 포럼 게시글 변경 반영, 크루 공용 도우미.
//!
//! 코어가 부르는 고리:
//! - [`start`]: 노드 시작 끝. 진행 중이던 초대 가입을 이어 갑니다.
//! - [`on_state_applied`]: 해소된 공간 상태가 바뀔 때마다(적용 잠금 안, 짧게).
//! - [`handle_redeem_invite`]: 들어온 `RedeemInvite`(인증 전에도 옴).
//! - [`on_decoded`]·[`on_resolved`]·[`on_forum_post_applied`]: 포럼 게시글 변경(상태 레인)을 보기 행에 반영.
//! - [`trusted_for_join`]·[`join_peer_ok`]: 가입 대기 중(아직 상태를 못 푼) 공간의 인증·동기화 허용.
//!
//! 초대 가입(가입자 쪽, [`join`]):
//! 1. 링크를 풀고 대기 기록을 남긴 뒤, 접속 정보의 노드(받아 주는 노드)에 `RedeemInvite`를 보냅니다.
//! 2. `InviteAccepted`의 공간 스트림 이벤트를 링크의 시작 이벤트로 고정해 적용합니다. 받아 준 노드가 커밋 확정
//!    기기면 크루 키 갱신과 봉인 키 묶음까지 들어 있어 바로 가입이 끝나고, 아니면 확정 기기가 크루 키 그룹에 넣어 줄 때까지
//!    기다립니다.
//! 3. 기다리는 동안에는 받아 준 노드와 공간을 만든 사용자의 기기만 믿고(상태를 못 풀어 멤버를 모름), 받아 준 노드에서
//!    공간 스트림을 되풀이해 따라잡습니다(간격을 늘려 가며).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::CoreError;
use yangpa_core::body::{EventBody, InviteProof};
use yangpa_core::checkpoint::OwnershipCert;
use yangpa_core::event::{EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::links::{ContactInfo, InviteLink};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, Flair, Member, Permissions, SpaceKind, SpaceState,
};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    InviteRedeem, MAX_EVENTS_PER_MESSAGE, MAX_WIRE_MESSAGE, PeerAuth, WireErrorCode, YpRequest,
    YpResponse,
};
use yangpa_crypto::decrypt_body;
use yangpa_net::{Multiaddr, PeerId};
use yangpa_rpc::api::Notification;
use yangpa_store::{ForumUpdate, MessageKind, MessageRecord};
use yangpa_sync::reconcile::full_range;
use yangpa_sync::state::limits::MAX_NAME_CHARS;
use yangpa_sync::{DecodedEvent, ResolveUpdate};

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::{identity, keys, messages, peers, spaces, sync_service};

/// 인증 시각 허용 오차(`peers`와 같음).
const MAX_AUTH_SKEW_MS: u64 = 5 * 60 * 1000;
/// 가입 대기 기록 설정 키 앞부분(`crew.join.<공간>`).
const JOIN_PREFIX: &str = "crew.join.";
/// 게시글마다 변경 기록 설정 키 앞부분(`forum.meta.<공간>.<게시글>`, 내용 없이 ID·논리 시계만).
const FORUM_META_PREFIX: &str = "forum.meta.";
/// 게시글 하나에 기록해 두는 변경 수 상한(넘으면 지금 값을 정하지 않은 오래된 것부터 버림).
const MAX_FORUM_UPDATES_PER_POST: usize = 256;
/// `InviteAccepted` 묶음 크기 어림 상한(전송 한도 `MAX_WIRE_MESSAGE`의 절반).
const BOOTSTRAP_BYTES: usize = MAX_WIRE_MESSAGE / 2;
/// 가입 대기 중 따라잡기 간격(처음, 최대).
const WATCH_FIRST: Duration = Duration::from_secs(1);
const WATCH_MAX: Duration = Duration::from_secs(30);
/// 가입 때 옛 상태 키를 물어 받기까지 기다리는 시간(묻기 전 지연 1초 + 왕복 몇 번).
const JOIN_STATE_KEY_WAIT: Duration = Duration::from_secs(5);
/// 새 크루의 첫 채널 이름.
pub(crate) const FIRST_CHANNEL: &str = "일반";
/// 받은편지함 전달자를 기억하는 (공간, 사용자) 수([`CrewState::inbox_relays`], 넘으면 오래된 것부터 버림).
const MAX_INBOX_RELAYS: usize = 16_384;
/// 해독했지만 아직 해소기가 적용하지 않은 가입·기기 목록 갱신을 기억하는 수.
const MAX_RELAY_CANDIDATES: usize = 8_192;
/// 사용자마다 기억하는 전달자 수(가장 최근 것부터).
const RELAYS_PER_USER: usize = 2;
/// `InviteAccepted`에서 가입자의 받은편지함 이벤트에 먼저 떼어 두는 몫(나머지는 공간 스트림).
const BOOTSTRAP_INBOX_BYTES: usize = BOOTSTRAP_BYTES / 4;

/// 한 공간에서 해소기의 적용을 기다리는 후원자 표시(작성자, 본문).
type HeldFlair = HashMap<EventId, (UserId, u64, Flair)>;

/// 크루 영역의 노드 수준 상태(`Node::crews`).
#[derive(Default)]
pub struct CrewState {
    /// 진행 중인 초대 가입. 설정 `crew.join.<공간>`에도 둡니다(재시작 뒤 이어 감).
    joins: Mutex<HashMap<SpaceId, PendingJoin>>,
    /// 가입 대기 따라잡기를 돌리는 공간.
    watching: Mutex<HashSet<SpaceId>>,
    /// 공간마다 해소기에 넣은 포럼 게시글 변경의 ID(RAM, 재시작하면 다시 적용하며 쌓임). 해소 결과(적용·해제된
    /// ID)에서 게시글 변경을 고르는 데 씁니다.
    forum_updates: Mutex<HashMap<SpaceId, HashSet<EventId>>>,
    /// 공간마다 해소기에 넣은 후원자 표시(`MemberFlair`)의 작성자와 본문. 해소기가 적용하면(멤버의 기기·서명 확인)
    /// 모읍니다. 가입 첫 해소에서는 상태 키가 아직 임시 키라 저장소에서 다시 풀 수 없어 본문을 들고 있습니다.
    flair_events: Mutex<HashMap<SpaceId, HeldFlair>>,
    /// 받은편지함 전달자([`InboxRelay`]).
    inbox_relays: Mutex<InboxRelays>,
}

/// 받은편지함 전달자: 그 사용자의 기기를 더한 상태 이벤트(가입 `MemberJoin`, 기기 목록 갱신 `DeviceListUpdate`)를 쓴
/// 기기. 가입자·연결한 기기는 상태를 풀기 전에는 받아 준 기기(연결이면 쓰던 기기)만 믿으므로, 확정 기기는 그 사용자의
/// 받은편지함 묶음을 전달자에게도 보내고, 전달자는 저장해 그 사용자의 기기에게 넘깁니다(시더 규칙도 전달자에게는 내줌).
///
/// 해소기가 **적용한** 이벤트로만 정합니다: 거부될 가짜 가입 하나로 전달자를 가로채지 못하고, 같은 이벤트 집합을 가진
/// 노드는 같은 답을 얻습니다. RAM에 두지만 시작 때 저장된 상태 이벤트를 다시 적용하며 다시 쌓입니다
/// (`spaces::load_all` → [`on_decoded`]·[`on_resolved`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InboxRelay {
    /// 전달자 기기와 그 사용자.
    pub(crate) user: UserId,
    pub(crate) device: DeviceId,
    /// 그 상태 이벤트(확정 기기가 받은편지함 묶음의 기준점에 넣음).
    pub(crate) event: EventId,
    /// 그 이벤트의 작성 시각(전달자가 최근 것만 당겨 옴).
    pub(crate) at: Timestamp,
    /// 그 이벤트의 논리 시계(사용자마다 최근 전달자를 고르는 순서. 작성 시각과 달리 부풀림 검사로 묶임).
    pub(crate) lamport: u64,
}

#[derive(Default)]
struct InboxRelays {
    /// 해독한 가입·기기 목록 갱신: 이벤트 → (공간, 사용자, 전달자). 적용되거나 거부돼도 지우지 않습니다: 해소기는 빈
    /// 부모가 채워지면 다시 해소해, 먼저 거부된 가입(초대 생성이 아직 안 옴 등)이 나중에 적용되거나 적용이 풀렸다가
    /// 다시 적용될 수 있습니다. [`MAX_RELAY_CANDIDATES`]를 넘으면 오래된 것부터 버립니다.
    candidates: HashMap<EventId, (SpaceId, UserId, InboxRelay)>,
    /// 적용된 것: (공간, 사용자) → 최근 전달자(새 것 먼저, [`RELAYS_PER_USER`]개까지).
    applied: HashMap<(SpaceId, UserId), Vec<InboxRelay>>,
}

impl InboxRelays {
    fn note(&mut self, space: SpaceId, user: UserId, relay: InboxRelay) {
        if self.candidates.len() >= MAX_RELAY_CANDIDATES
            && !self.candidates.contains_key(&relay.event)
        {
            // 오래된 후보(작성 시각 순) 절반을 버립니다. 버린 후보가 나중에 (다시) 적용되면 전달자로 기억하지 못할
            // 뿐입니다(전달자는 최근 가입·기기 연결에만 쓰임).
            let mut ats: Vec<Timestamp> = self.candidates.values().map(|(_, _, x)| x.at).collect();
            ats.sort_unstable();
            let cut = ats[ats.len() / 2];
            self.candidates.retain(|_, (_, _, x)| x.at > cut);
        }
        self.candidates.insert(relay.event, (space, user, relay));
    }

    /// 해소 결과로 전달자를 고칩니다: 적용된 후보는 올리고, 적용이 풀린 이벤트의 전달자는 지웁니다. 거부는 그대로
    /// 둡니다(다시 해소하면 적용될 수 있고, 적용되지 않는 한 전달자가 되지 않음).
    fn settle(&mut self, upd: &ResolveUpdate) {
        if !upd.unapplied.is_empty() {
            let gone: HashSet<&EventId> = upd.unapplied.iter().collect();
            for list in self.applied.values_mut() {
                list.retain(|x| !gone.contains(&x.event));
            }
            self.applied.retain(|_, list| !list.is_empty());
        }
        for id in &upd.applied {
            let Some((space, user, relay)) = self.candidates.get(id).copied() else {
                continue;
            };
            let list = self.applied.entry((space, user)).or_default();
            list.retain(|x| x.device != relay.device);
            // 새 것 먼저(논리 시계 순). 다시 해소해 옛 가입이 늦게 적용돼도 최근 전달자를 밀어내지 않게.
            let at = list
                .iter()
                .position(|x| x.lamport < relay.lamport)
                .unwrap_or(list.len());
            list.insert(at, relay);
            list.truncate(RELAYS_PER_USER);
        }
        if self.applied.len() > MAX_INBOX_RELAYS {
            let mut ats: Vec<Timestamp> = self
                .applied
                .values()
                .filter_map(|l| l.first())
                .map(|x| x.at)
                .collect();
            ats.sort_unstable();
            let cut = ats[ats.len() / 4];
            self.applied
                .retain(|_, l| l.first().is_some_and(|x| x.at > cut));
        }
    }
}

/// 해독한 가입·기기 목록 갱신을 후보로 둡니다(해소기가 적용하면 [`on_resolved`]가 전달자로 올림).
fn note_relay_candidate(node: &Node, ev: &DecodedEvent, user: UserId) {
    node.crews.inbox_relays.lock().note(
        ev.header.space,
        user,
        InboxRelay {
            user: ev.header.user,
            device: ev.header.author,
            event: ev.id,
            at: ev.header.created_at,
            lamport: ev.header.lamport,
        },
    );
}

fn settle_relays(node: &Node, upd: &ResolveUpdate) {
    node.crews.inbox_relays.lock().settle(upd);
}

/// 이 사용자의 받은편지함 전달자(새 것 먼저).
pub(crate) fn inbox_relays(node: &Node, space: &SpaceId, user: &UserId) -> Vec<InboxRelay> {
    node.crews
        .inbox_relays
        .lock()
        .applied
        .get(&(*space, *user))
        .cloned()
        .unwrap_or_default()
}

/// `device`가 이 사용자의 받은편지함 전달자인지.
pub(crate) fn is_inbox_relay(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    device: &DeviceId,
) -> bool {
    node.crews
        .inbox_relays
        .lock()
        .applied
        .get(&(*space, *user))
        .is_some_and(|l| l.iter().any(|x| x.device == *device))
}

/// `device`가 `since` 뒤에 전달자가 된 사용자(최근 것부터 `max`명). 전달자는 확정 기기에게서 이들의 받은편지함을
/// 당겨 옵니다(확정 기기가 직접 보낼 때 꺼져 있었을 수 있음).
pub(crate) fn relayed_users(
    node: &Node,
    space: &SpaceId,
    device: &DeviceId,
    since: Timestamp,
    max: usize,
) -> Vec<UserId> {
    let r = node.crews.inbox_relays.lock();
    let mut out: Vec<(Timestamp, UserId)> = r
        .applied
        .iter()
        .filter(|((s, _), _)| s == space)
        .filter_map(|((_, u), l)| {
            l.iter()
                .find(|x| x.device == *device && x.at >= since)
                .map(|x| (x.at, *u))
        })
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.truncate(max);
    out.into_iter().map(|(_, u)| u).collect()
}

fn forget_relays(node: &Node, space: &SpaceId) {
    let mut r = node.crews.inbox_relays.lock();
    r.candidates.retain(|_, (s, _, _)| s != space);
    r.applied.retain(|(s, _), _| s != space);
}

/// 초대 가입 대기 기록.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingJoin {
    /// 링크에 든 시작 이벤트.
    genesis: EventId,
    /// 초대를 받아 준 노드(링크 접속 정보의 PeerId).
    gateway: Vec<u8>,
    /// 공간을 만든 사용자(시작 이벤트 머리의 작성자). 대기 중에도 이 사용자의 기기는 믿습니다.
    #[serde(default)]
    creator: Option<UserId>,
}

fn join_key(space: &SpaceId) -> String {
    format!("{JOIN_PREFIX}{space}")
}

fn forum_meta_prefix(space: &SpaceId) -> String {
    format!("{FORUM_META_PREFIX}{space}.")
}

pub(crate) fn start(node: &Arc<Node>) {
    let Ok(rows) = node.store.settings_with_prefix(JOIN_PREFIX) else {
        return;
    };
    for (key, value) in rows {
        let space = key
            .strip_prefix(JOIN_PREFIX)
            .and_then(|s| s.parse::<SpaceId>().ok());
        let parsed = serde_json::from_value::<PendingJoin>(value);
        match (space, parsed) {
            (Some(space), Ok(p)) => {
                node.crews.joins.lock().insert(space, p);
                spawn_watch(node, space);
            }
            _ => {
                let _ = node.store.delete_setting(&key);
            }
        }
    }
}

/// 해소된 상태가 바뀐 직후(적용 잠금 안). 지금까지 볼 수 없던 채널을 볼 수 있게 되면(역할을 받음·비공개 해제)
/// 멤버 피어에게서 바로 따라잡아 지난 기록을 받습니다(주기 따라잡기를 기다리지 않게).
pub(crate) fn on_state_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    old: Option<&SpaceState>,
    new: &SpaceState,
) {
    crate::peers::on_state_applied(node, new);
    // 나간 크루의 신뢰를 먼저 지워 아래 P2P 정리가 그것까지 보게 합니다.
    let trust_changed = crate::trust::on_state_applied(node, space, old, new);
    crate::p2p::on_state_applied(node, space, old, new);
    crate::voice::on_state_applied(node, space, old, new);
    if trust_changed {
        crate::p2p::reconcile(node);
    }
    let (Some(old), Ok(me)) = (old, node.me()) else {
        return;
    };
    let me = me.user();
    if !new.is_member(&me) {
        if old.is_member(&me) {
            debug!(space = %space.short(), "이 공간의 멤버가 아니게 됨(추방·탈퇴)");
        }
        return;
    }
    let newly_visible = new.channels.iter().any(|(c, ch)| {
        !ch.is_volatile()
            && old.channels.contains_key(c)
            && new
                .channel_permissions(&me, c)
                .contains(Permissions::VIEW_CHANNEL)
            && (!old
                .channel_permissions(&me, c)
                .contains(Permissions::VIEW_CHANNEL)
                // P2P 전용에서 Tor로 돌아온 채널: P2P로만 오가던 기록을 이제 Tor로 받습니다.
                || (old.is_p2p(c) && !new.is_p2p(c)))
    });
    if !newly_visible {
        return;
    }
    let space = *space;
    node.spawn_detached(move |node| async move {
        let Some(st) = node.space_state(&space) else {
            return;
        };
        for (peer, user, device) in node.peers.authed_peers() {
            if st.device_belongs(&user, &device) {
                sync_service::spawn_catch_up(&node, space, peer);
            }
        }
    });
}

/// 이 채널을 볼 수 없는 멤버가 하나라도 있는지(@everyone 보기 거부든, 역할 기본 권한·멤버별 거부든). 그런 채널은
/// 크루 키가 아니라 보기 규칙의 비공개 키 그룹 키를 쓰므로, 상태 레인(크루 상태 키)에 그 채널 내용을 실으면 안 됩니다.
pub(crate) fn needs_channel_group(state: &SpaceState, c: &ChannelId, ch: &ChannelSettings) -> bool {
    if ch.is_volatile() || ch.kind == ChannelKind::Category {
        return false;
    }
    ch.is_private()
        || state.members.keys().any(|u| {
            !state
                .channel_permissions(u, c)
                .contains(Permissions::VIEW_CHANNEL)
        })
}

// ───────────── 소유권 이전 ─────────────

/// 소유권을 넘깁니다. 넘긴 뒤 확정 기기가 새 소유자 기기로 옮겨 가면(해소기 `repair_committer`) 옛 확정 기기가 키
/// 그룹 표를 새 확정 기기에게 봉인해 넘기고(`keygroup` 표 넘기기), 넘겨받지 못하면 새 확정 기기가 공개 끝으로 이어받아
/// 전체 재키합니다. 그래서 새 소유자를 그룹마다 먼저 넣을 일이 없습니다(키 그룹은 확정 기기가 표만 있으면 어느 그룹이든
/// 갱신을 낼 수 있음).
pub(crate) async fn transfer_ownership(node: &Arc<Node>, space: SpaceId, to: UserId) -> Result<()> {
    let me = node.me()?;
    let l = ingest::lock_space(node, space).await;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("없는 공간"))?;
    if state.owner != me.user() {
        return Err(Error::forbidden("소유자만 소유권을 넘길 수 있습니다"));
    }
    if !state.is_member(&to) {
        return Err(Error::not_found("이 크루의 멤버가 아닙니다"));
    }
    let (ev, report) = author::publish_locked(
        node,
        &l,
        space,
        None,
        &EventBody::OwnershipTransfer {
            new_owner: to,
            // 체크포인트 사슬로 들어오는 멤버는 이 이벤트를 열 수 없어 인증서로 소유자가 바뀐 것을 확인합니다.
            // 지금 바탕에 묶어, 소유권이 돌아온 뒤 옛 인증서를 다시 싣지 못하게 합니다.
            cert: OwnershipCert::create(
                space,
                crate::checkpoint::base_id(node, &space),
                to,
                Timestamp::now(),
                &me.identity.signing,
                &me.device.signing,
            ),
        },
        KeyChoice::State,
        &[],
    )?;
    drop(l);
    ingest::after_unlock(node, space, &report);
    sync_service::propagate(node, space, vec![ev]);
    Ok(())
}

// ───────────── 초대: 받아 주는 쪽 ─────────────

/// 초대 링크 사용 요청. 토큰 해시·만료·사용 횟수를 확인하고 `MemberJoin`을 발행합니다. 내가 확정 기기면 응답 전에
/// 키 그룹을 맞춰(`KeyUpdate{Advance}`와 가입자 받은편지함 키 묶음), 가입자가 키까지 한 번에 받게 합니다.
pub(crate) async fn handle_redeem_invite(
    node: &Arc<Node>,
    peer: PeerId,
    req: Box<InviteRedeem>,
) -> YpResponse {
    let bad = |m: &str| YpResponse::error(WireErrorCode::BadRequest, m);
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let r = *req;
    if r.auth
        .verify(
            &peer.to_bytes(),
            &node.peer_id().to_bytes(),
            MAX_AUTH_SKEW_MS,
        )
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증이 올바르지 않음");
    }
    let (user, device) = (r.auth.user, r.auth.device);
    if r.profile.user != user || r.profile.verify().is_err() {
        return bad("프로필이 올바르지 않음");
    }
    let Some(state) = node.space_state(&r.space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&me.user()) {
        return YpResponse::error(WireErrorCode::NotMember, "이 공간의 멤버가 아님");
    }
    if state.kind != SpaceKind::Crew {
        return bad("크루가 아님");
    }
    // 밴된 사용자는 옛 초대든 새 초대든 받아 주지 않습니다(다른 노드가 가입 이벤트를 내도 해소기가 거부).
    if state.bans.contains_key(&user) {
        return YpResponse::error(WireErrorCode::Forbidden, "이 크루에서 밴된 사용자");
    }
    if state.is_member(&user) {
        // 응답을 잃고 다시 온 요청: 새로 가입시키지 않고 이벤트만 다시 보냅니다. 다른 기기는 기기 목록 갱신으로 들어옵니다.
        if !state.device_belongs(&user, &device) {
            return bad("이미 멤버인 사용자의 등록되지 않은 기기");
        }
    } else {
        let Some(inv) = state
            .invites
            .iter()
            .find(|i| i.info.token_hash == r.proof.token_hash)
        else {
            return YpResponse::error(WireErrorCode::InvalidToken, "모르거나 폐기된 초대");
        };
        if !inv.usable_at(Timestamp::now()) {
            return YpResponse::error(WireErrorCode::Expired, "만료됐거나 사용 횟수를 다 쓴 초대");
        }
        // 토큰을 가진 사람이 이 기기 목록으로 만든 증명인지(받아 주는 쪽도 토큰 원문은 모름).
        if r.proof
            .verify(&inv.info.verify_key, &r.space, &r.auth.devices)
            .is_err()
        {
            return YpResponse::error(WireErrorCode::InvalidToken, "초대 증명이 맞지 않음");
        }
        let body = EventBody::MemberJoin {
            member: Member {
                user,
                roles: Vec::new(),
                nickname: None,
                automation: false,
            },
            devices: r.auth.devices.clone(),
            profile: Some(r.profile.clone()),
            invite: Some(r.proof.clone()),
        };
        match author::publish(node, r.space, None, &body, KeyChoice::State).await {
            // 받아 준 멤버가 보관 노드 명단에 가입자를 바로 넣습니다(권한자가 명단을 보낼 때까지 기다리지 않게).
            Ok(_) => crate::archive::add_joiner(node, r.space, user),
            Err(e) => {
                // 같은 사람의 요청이 동시에 와서 먼저 처리됐을 수 있습니다.
                let joined = node
                    .space_state(&r.space)
                    .is_some_and(|s| s.is_member(&user));
                if !joined {
                    debug!(error = %e, "초대 가입 이벤트 거부");
                    return YpResponse::error(WireErrorCode::InvalidToken, "초대를 쓸 수 없음");
                }
            }
        }
    }
    let _ = identity::accept_device_list(node, &r.auth.devices);
    let _ = identity::accept_profile(node, &r.profile);
    let _ = node
        .store
        .set_peer_identity(&peer.to_bytes(), &user, &device);
    if node
        .space_state(&r.space)
        .is_some_and(|s| s.committer == me.device_id())
    {
        // 함께 온 가입은 키 갱신 하나로 묶입니다(`commit_batch_window`). 응답 전에 돌려 가입자가 받은편지함 키 묶음까지
        // 한 번에 받게 합니다.
        crate::keygroup::reconcile_batched(node, r.space).await;
    }
    let events = match bootstrap_bundle(node, &r.space, &user) {
        Ok(e) => e,
        Err(_) => return YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    };
    // 응답 뒤 인증해 두면 가입자가 곧바로 따라잡고, 이쪽의 전파도 닿습니다.
    node.spawn_detached(move |node| async move {
        if let Err(e) = peers::ensure_auth(&node, peer).await {
            debug!(error = %e, "가입자 인증 실패");
        }
    });
    YpResponse::InviteAccepted {
        space: r.space,
        events,
    }
}

/// `InviteAccepted`에 담을 공간 스트림 이벤트: 시작 이벤트를 맨 앞에 꼭 넣고, 오래된 것부터 개수·크기 한도 안에서.
/// 응답이 전송 한도를 넘으면 가입이 통째로 실패하므로(이미 `MemberJoin`을 낸 뒤라 멤버로만 남음) 크기를 어림해
/// 자르고, 나머지는 가입자가 대기 중 따라잡기로 받습니다. 가입자의 받은편지함 이벤트(내가 확정 기기라 바로 낸 키 묶음,
/// 또는 확정 기기가 보내 준 것)가 있으면 몫을 떼어 함께 넣습니다: 그 키가 있어야 공간 스트림을 엽니다.
fn bootstrap_bundle(node: &Node, space: &SpaceId, joiner: &UserId) -> Result<Vec<SignedEvent>> {
    let inbox = take_within(
        node.store.stream_events(
            space,
            StreamId::Inbox(*joiner),
            full_range(u64::MAX - 1),
            MAX_EVENTS_PER_MESSAGE as u32,
        )?,
        wire_size,
        BOOTSTRAP_INBOX_BYTES,
        MAX_EVENTS_PER_MESSAGE / 4,
    );
    let inbox_bytes: usize = inbox.iter().map(wire_size).sum();
    let genesis = match spaces::load_genesis(node, space)? {
        Some(g) => node.store.get_event(&g)?,
        None => None,
    };
    let gid = genesis.as_ref().map(SignedEvent::id);
    // 체크포인트가 있으면 경계 위 꼬리만(체크포인트 이벤트 포함): 가입자는 사슬·스냅숏을 따로 받아 그 위에서 시작합니다.
    let from = crate::checkpoint::bootstrap_floor(node, space).map_or(0, |(f, _)| f);
    let rest = node.store.stream_events(
        space,
        StreamId::Space,
        yangpa_core::wire::LamportRange {
            start: from,
            ..full_range(u64::MAX - 1)
        },
        MAX_EVENTS_PER_MESSAGE as u32,
    )?;
    let all = genesis
        .into_iter()
        .chain(rest.into_iter().filter(|e| Some(e.id()) != gid))
        .collect();
    let mut out = take_within(
        all,
        wire_size,
        BOOTSTRAP_BYTES.saturating_sub(inbox_bytes),
        MAX_EVENTS_PER_MESSAGE - inbox.len(),
    );
    out.extend(inbox);
    Ok(out)
}

/// 앞에서부터 크기 합이 `budget` 이하, 개수가 `max` 이하인 만큼(첫 항목은 크기와 상관없이 넣음). 중간에서 끊어
/// 순서(인과)를 지킵니다.
fn take_within<T>(items: Vec<T>, size: impl Fn(&T) -> usize, budget: usize, max: usize) -> Vec<T> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for it in items {
        let sz = size(&it);
        if out.len() >= max || (!out.is_empty() && used.saturating_add(sz) > budget) {
            break;
        }
        used = used.saturating_add(sz);
        out.push(it);
    }
    out
}

/// 이벤트 하나의 전송 크기 어림(본문 + 머리·서명 몫. 고정 길이 ID는 바이트마다 1~2바이트로 인코딩되므로 넉넉히).
fn wire_size(ev: &SignedEvent) -> usize {
    ev.body
        .as_ref()
        .map_or(0, Vec::len)
        .saturating_add(1024)
        .saturating_add(yangpa_core::sign::Sig::LEN)
        .saturating_add(64 * ev.header.parents.len())
}

// ───────────── 초대: 가입하는 쪽 ─────────────

fn pending_join(node: &Node, space: &SpaceId) -> Option<PendingJoin> {
    node.crews.joins.lock().get(space).cloned()
}

fn remember_join(node: &Node, space: &SpaceId, p: &PendingJoin) -> Result<()> {
    node.store.put_setting(&join_key(space), p)?;
    node.crews.joins.lock().insert(*space, p.clone());
    Ok(())
}

pub(crate) fn forget_join(node: &Node, space: &SpaceId) {
    node.crews.joins.lock().remove(space);
    let _ = node.store.delete_setting(&join_key(space));
}

fn restore_join(node: &Node, space: &SpaceId, prev: Option<PendingJoin>) {
    match prev {
        Some(p) => {
            let _ = remember_join(node, space, &p);
        }
        None => forget_join(node, space),
    }
}

/// 가입 대기 중인 공간인지. 그새 나를 넣은 크루 키 갱신을 받아 가입이 끝났으면 기록을 지웁니다(감시 작업은 간격을
/// 두고 돌기 때문).
pub(crate) fn is_pending_join(node: &Node, space: &SpaceId) -> bool {
    if !node.crews.joins.lock().contains_key(space) {
        return false;
    }
    if is_joined(node, space) {
        forget_join(node, space);
        return false;
    }
    true
}

/// 가입 대기 중에 이 기기의 `Auth`를 믿을지: 받아 준 노드이거나 공간을 만든 사용자의 기기(`peers::is_trusted`).
pub(crate) fn trusted_for_join(node: &Node, peer: &PeerId, user: &UserId) -> bool {
    let peer = peer.to_bytes();
    node.crews
        .joins
        .lock()
        .values()
        .any(|j| j.gateway == peer || j.creator == Some(*user))
}

/// 상태를 아직 못 푼 가입 대기 공간에서 이 피어와 동기화할지(따라잡기·`PushEvents`).
pub(crate) fn join_peer_ok(node: &Node, space: &SpaceId, peer: &PeerId, user: &UserId) -> bool {
    node.crews
        .joins
        .lock()
        .get(space)
        .is_some_and(|j| j.gateway == peer.to_bytes() || j.creator == Some(*user))
}

/// 멤버이고 크루 키 그룹 키를 얻어 메시지를 주고받을 수 있는지.
pub(crate) fn is_joined(node: &Node, space: &SpaceId) -> bool {
    let Ok(me) = node.me() else { return false };
    node.space_state(space)
        .is_some_and(|s| s.is_member(&me.user()))
        && crate::keygroup::has_crew_key(node, space)
}

/// 링크 주인 노드에 닿을 수 있게 합니다(연결돼 있지 않으면 릴레이 경유·onion 주소, 없으면 DHT).
async fn reach(node: &Arc<Node>, peer: PeerId, contact: &ContactInfo) {
    if node.peers.is_connected(&peer) {
        return;
    }
    let mut addrs: Vec<Multiaddr> = crate::tor::contact_addrs(contact);
    if addrs.is_empty()
        && let Ok(found) = node.net.find_peer(peer).await
    {
        addrs = found;
    }
    if !addrs.is_empty()
        && let Err(e) = peers::connect(node, peer, addrs).await
    {
        debug!(error = %e, "초대한 노드에 연결 실패");
    }
}

/// 내 노드에 닿는 방법(링크용). 자기 IP는 넣지 않습니다(릴레이 경유 주소·onion만).
pub(crate) async fn my_contact(node: &Node) -> ContactInfo {
    let status = node.net.status().await;
    ContactInfo {
        peer_id: node.peer_id().to_bytes(),
        relay_addrs: status
            .relay_addrs
            .iter()
            .map(|a| a.to_string())
            .filter(|a| a.contains("/p2p-circuit"))
            .collect(),
        onion: crate::tor::contact_onion(status.onion_addr.as_ref()),
    }
}

/// 초대 링크로 가입합니다. 반환: (공간, 가입이 끝났는지). 끝나지 않았으면 확정 기기가 내 기기를 넣은 크루 키 갱신
/// (`KeyUpdate`)을 내기를 기다리는 중입니다.
pub(crate) async fn join(node: &Arc<Node>, url: &str) -> Result<(SpaceId, bool)> {
    let link = InviteLink::parse(url).map_err(|e| match e {
        CoreError::Expired => Error::invalid("만료된 초대 링크입니다"),
        e => Error::invalid(format!("초대 링크: {e}")),
    })?;
    let me = node.me()?;
    let space = link.space;
    if node
        .space_state(&space)
        .is_some_and(|s| s.is_member(&me.user()))
    {
        let joined = is_joined(node, &space);
        if !joined && is_pending_join(node, &space) {
            spawn_watch(node, space);
        }
        return Ok((space, joined));
    }
    if let Some(g) = spaces::load_genesis(node, &space)?
        && g != link.genesis
    {
        return Err(Error::invalid(
            "이미 아는 공간과 시작 이벤트가 다른 링크입니다",
        ));
    }
    let peer = PeerId::from_bytes(&link.contact.peer_id)
        .map_err(|_| Error::invalid("링크의 노드 ID가 잘못됨"))?;
    if peer == node.peer_id() {
        return Err(Error::invalid("이 노드가 만든 링크입니다"));
    }
    let prev = pending_join(node, &space);
    let pending = PendingJoin {
        genesis: link.genesis,
        gateway: peer.to_bytes(),
        creator: prev.as_ref().and_then(|p| p.creator),
    };
    // 받아 준 노드가 응답 직후 보내는 인증(Auth)을 받아 줄 수 있게 요청 전에 남깁니다.
    remember_join(node, &space, &pending)?;
    reach(node, peer, &link.contact).await;
    let redeem = async {
        let auth = PeerAuth::create(
            &me.device.signing,
            me.devices.clone(),
            &node.peer_id().to_bytes(),
            &peer.to_bytes(),
        );
        // 토큰 원문은 보내지 않고, 토큰으로 이 기기 목록에 서명한 증명만 보냅니다(상태 레인에도 증명만 남음).
        // 기기 키로도 서명합니다. 옛 빌드의 복구 기기가 스스로 들어온 크루인지 볼 때 씁니다(W3).
        let proof = InviteProof::create(&link.token, &space, &me.devices, Timestamp::now())?
            .signed_by_joiner(&space, &me.devices, &me.device.signing)?;
        let req = YpRequest::RedeemInvite(Box::new(InviteRedeem {
            space,
            proof,
            auth,
            profile: me.profile.clone(),
        }));
        match node.net.request(peer, req).await {
            Ok(YpResponse::InviteAccepted { space: s, events }) if s == space => Ok(events),
            Ok(YpResponse::Error { code, message }) => Err(match code {
                WireErrorCode::InvalidToken | WireErrorCode::Expired => {
                    Error::invalid("초대가 만료됐거나 폐기됐거나 사용 횟수를 다 썼습니다")
                }
                WireErrorCode::NotFound | WireErrorCode::NotMember => {
                    Error::invalid("링크의 노드가 이 크루의 멤버가 아닙니다")
                }
                WireErrorCode::Forbidden => {
                    Error::forbidden("이 크루에서 밴되어 들어갈 수 없습니다")
                }
                _ => Error::invalid(format!("상대가 거절함: {message}")),
            }),
            Ok(_) => Err(Error::internal("예상하지 못한 응답")),
            Err(e) => Err(Error::Network(format!(
                "초대한 노드에 닿지 못했습니다(같은 링크로 다시 시도할 수 있음): {e}"
            ))),
        }
    };
    let events = match redeem.await {
        Ok(evs) => evs,
        Err(e) => {
            restore_join(node, &space, prev);
            return Err(e);
        }
    };
    let creator = match precheck_bootstrap(&space, &link.genesis, &me.user(), &events) {
        Ok(c) => c,
        Err(why) => {
            restore_join(node, &space, prev);
            return Err(Error::invalid(why));
        }
    };
    remember_join(
        node,
        &space,
        &PendingJoin {
            creator: Some(creator),
            ..pending
        },
    )?;
    spaces::ensure_runtime(node, &space, Some(link.genesis))?;
    // 링크에 고정이 있으면 받아 준 노드의 사슬을 그 고정에서 확인하고 스냅숏을 받아 둡니다(고정이 없으면 처음부터
    // 받음). 경계가 먼저 서야 묶음을 적용하며 경계 아래 부모를 따라가 받지 않습니다.
    // 받지 못하면(연결·사슬 오류) 감시 작업이 다시 묻고, 그 전에는 처음부터 따라잡습니다: 전체 기록은 시작 이벤트에서
    // 해소기가 스스로 확인하므로 안전하고 느릴 뿐입니다. 이미 가입 이벤트가 나갔으므로 가입을 실패로 돌리지 않습니다.
    if let Err(e) = crate::checkpoint::prepare_join(node, space, peer, link.checkpoint).await {
        tracing::warn!(space = %space.short(), error = %e, "받아 준 노드의 체크포인트를 쓰지 못함(다시 물음)");
        crate::checkpoint::prepare_later(node, space, peer, link.checkpoint);
    }
    if let Err(e) = ingest::ingest(node, space, events, Source::Bootstrap(peer)).await {
        debug!(error = %e, "초대 묶음 적용 실패(따라잡기로 다시 받음)");
    }
    if crate::checkpoint::has_stash(node, &space)
        && let Err(e) = crate::checkpoint::try_install(node, space).await
    {
        debug!(error = %e, "받아 둔 체크포인트를 아직 세우지 못함");
    }
    let joined = is_joined(node, &space) || wait_older_state_keys(node, &space).await;
    if joined {
        forget_join(node, &space);
    } else {
        spawn_watch(node, space);
    }
    Ok((space, joined))
}

/// 가입 묶음에는 최근 상태 키만 실리므로(`keys::JOINER_STATE_KEYS`) 오래된 크루에서는 옛 키로 쓴 상태 이벤트를 열지
/// 못한 채 들어옵니다. 그 키를 멤버에게 묻는 동안(`keygroup::relay::ask_state_keys`) 잠깐 기다려 대개 바로 `joined`를
/// 돌려줍니다. 처음부터 기다리는 상태 키가 없거나 시간 안에 멤버가 되지 못하면 `pending`으로 두고 감시(`spawn_watch`)가
/// 마저 합니다. 키가 온 뒤에도 다시 해소·가입 갱신 적용이 잠깐 이어지므로 기다림이 풀려도 시간까지는 지켜봅니다.
async fn wait_older_state_keys(node: &Arc<Node>, space: &SpaceId) -> bool {
    let deadline = tokio::time::Instant::now() + JOIN_STATE_KEY_WAIT;
    let mut asked = false;
    loop {
        if is_joined(node, space) {
            return true;
        }
        // 공간 목록 잠금을 쥔 채 공간 코어를 잠그지 않게 복제해 둡니다.
        let core = node.spaces.read().get(space).map(|rt| rt.core.clone());
        let waits = core.is_some_and(|c| {
            c.lock()
                .waiting
                .keys()
                .any(|n| matches!(n, ingest::Need::StateKey(_)))
        });
        asked |= waits;
        if !asked || tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 받은 가입 묶음을 저장하기 전에 확인합니다: 이 공간의 공간 스트림과 내 받은편지함(키 묶음) 이벤트만, 상한 이하,
/// 링크의 시작 이벤트(서명이 맞는 `SpaceCreate`)가 들어 있어야 합니다. 상태 키가 아직 없어 내용은 열 수 없으므로
/// 고정만 하고, 나머지는 해소기·기준점 검사가 확인합니다. 공간을 만든 사용자를 돌려줍니다.
fn precheck_bootstrap(
    space: &SpaceId,
    genesis: &EventId,
    me: &UserId,
    events: &[SignedEvent],
) -> std::result::Result<UserId, &'static str> {
    if events.is_empty() || events.len() > MAX_EVENTS_PER_MESSAGE {
        return Err("이벤트 수가 잘못됨");
    }
    let mine = StreamId::Inbox(*me);
    if events.iter().any(|e| {
        let stream = e.header.stream();
        e.header.space != *space || (stream != StreamId::Space && stream != mine)
    }) {
        return Err("이 공간의 공간 스트림이 아닌 이벤트");
    }
    let g = events
        .iter()
        .find(|e| e.id() == *genesis)
        .ok_or("링크의 시작 이벤트가 없음")?;
    if g.header.kind != EventKind::SpaceCreate || g.verify().is_err() {
        return Err("시작 이벤트가 올바르지 않음");
    }
    Ok(g.header.user)
}

/// 가입이 끝날 때까지 받아 준 노드와 인증하고 공간 스트림을 따라잡습니다(간격을 늘려 가며). 공간마다 하나만 돕니다.
fn spawn_watch(node: &Arc<Node>, space: SpaceId) {
    if !node.crews.watching.lock().insert(space) {
        return;
    }
    node.spawn_detached(move |node| async move {
        let mut delay = WATCH_FIRST;
        while let Some(p) = pending_join(&node, &space) {
            crate::checkpoint::retry_prepare(&node, space).await;
            if crate::checkpoint::has_stash(&node, &space)
                && let Err(e) = crate::checkpoint::try_install(&node, space).await
            {
                debug!(error = %e, "받아 둔 체크포인트를 아직 세우지 못함");
            }
            if is_joined(&node, &space) {
                forget_join(&node, &space);
                break;
            }
            if let Ok(peer) = PeerId::from_bytes(&p.gateway)
                && peers::ensure_auth(&node, peer).await.is_ok()
            {
                sync_service::spawn_catch_up(&node, space, peer);
            }
            // 상태를 풀었으면 다른 멤버 피어에게서도 받습니다(나를 넣은 키 갱신이 받아 준 노드에 아직 없을 수 있음).
            if let Some(st) = node.space_state(&space) {
                for (peer, user, device) in node.peers.authed_peers() {
                    if st.device_belongs(&user, &device) {
                        sync_service::spawn_catch_up(&node, space, peer);
                    }
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(WATCH_MAX);
        }
        node.crews.watching.lock().remove(&space);
    });
}

// ───────────── 탈퇴·정리 ─────────────

/// 이 기기에서 공간 데이터를 모두 지웁니다(탈퇴가 전해졌거나, 추방된 뒤 사용자가 지울 때, 가입 대기 취소).
pub(crate) async fn purge_local(node: &Arc<Node>, space: &SpaceId) -> Result<()> {
    forget_join(node, space);
    node.crews.forum_updates.lock().remove(space);
    node.crews.flair_events.lock().remove(space);
    crate::flair::forget(node, space);
    crate::cards::forget(node, space);
    forget_relays(node, space);
    if let Ok(rows) = node.store.settings_with_prefix(&forum_meta_prefix(space)) {
        for (k, _) in rows {
            let _ = node.store.delete_setting(&k);
        }
    }
    let l = ingest::lock_space(node, *space).await;
    spaces::discard_space_locked(node, &l, space)?;
    drop(l);
    node.notify
        .publish(Notification::SpaceUpdated { space: *space });
    Ok(())
}

/// 이벤트를 인증된 멤버 피어들에게 바로 보내고, 받아 준 피어 수를 돌려줍니다(탈퇴처럼 보낸 뒤 지울 때).
/// 상대가 이미 나를 멤버로 보지 않는다고 답해도(`NotMember`) 받은 것으로 칩니다.
pub(crate) async fn deliver_now(
    node: &Arc<Node>,
    space: SpaceId,
    state: &SpaceState,
    events: Vec<SignedEvent>,
) -> usize {
    let Ok(me) = node.me() else { return 0 };
    let targets: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, d)| *d != me.device_id() && state.device_belongs(u, d))
        .map(|(p, _, _)| p)
        .collect();
    let sends = targets.into_iter().map(|peer| {
        let req = YpRequest::PushEvents {
            space,
            events: events.clone(),
        };
        async move {
            // `NotMember`: 상대가 이미 이 사용자를 멤버로 보지 않음(다른 길로 받았음).
            matches!(
                node.net.request(peer, req).await,
                Ok(YpResponse::Ok)
                    | Ok(YpResponse::Error {
                        code: WireErrorCode::NotMember,
                        ..
                    })
            )
        }
    });
    futures::future::join_all(sends)
        .await
        .into_iter()
        .filter(|ok| *ok)
        .count()
}

/// 크루에서 나갑니다. 멤버면 `MemberLeave`를 발행하고, 다른 멤버 노드 하나 이상이 받은 것을 확인한 뒤에만 이
/// 기기의 공간 데이터를 지웁니다(아무도 못 받으면 내 기기가 크루 키 그룹에서 빠지지 않으므로). 이미 멤버가 아니면(추방됐거나
/// 전에 나가기를 부른 뒤 전하지 못함) 남은 탈퇴 이벤트를 다시 전해 보고 지웁니다. 해산한 크루면 해산한 소유자만 해산
/// 이벤트를 다시 전하고(멤버는 해산을 이미 받음) 지웁니다.
pub(crate) async fn leave(node: &Arc<Node>, space: SpaceId, state: &SpaceState) -> Result<()> {
    let me = node.me()?;
    let user = me.user();
    if state.disbanded {
        let ev = my_last_event(node, &space, &user, EventKind::CrewDisband)?;
        return deliver_then_purge(node, space, state, ev, DISBAND_UNSENT).await;
    }
    let leave_ev = if state.is_member(&user) {
        if state.owner == user {
            return Err(Error::conflict(
                "소유자는 먼저 소유권을 넘겨야 나갈 수 있습니다(crew.transfer_ownership)",
            ));
        }
        let body = EventBody::MemberLeave { user };
        Some(author::publish(node, space, None, &body, KeyChoice::State).await?)
    } else {
        my_last_event(node, &space, &user, EventKind::MemberLeave)?
    };
    deliver_then_purge(
        node,
        space,
        state,
        leave_ev,
        "탈퇴를 아직 다른 멤버에게 전하지 못했습니다. 멤버 노드와 연결된 뒤 다시 부르세요",
    )
    .await
}

const DISBAND_UNSENT: &str =
    "해산을 아직 다른 멤버에게 전하지 못했습니다. 멤버 노드와 연결된 뒤 나가기로 다시 전하세요";

/// 크루를 해산합니다(소유자 확인은 API가). 나가기처럼 다른 멤버 노드 하나 이상이 받은 뒤에만 이 기기의 데이터를
/// 지웁니다. 못 전하면 해산한 채 남고, 나가기가 해산 이벤트를 다시 전합니다.
pub(crate) async fn disband(node: &Arc<Node>, space: SpaceId, state: &SpaceState) -> Result<()> {
    let ev = author::publish(node, space, None, &EventBody::CrewDisband, KeyChoice::State).await?;
    deliver_then_purge(node, space, state, Some(ev), DISBAND_UNSENT).await
}

/// 다른 멤버가 있으면 `ev`를 하나 이상의 멤버 노드에 전한 뒤에만(못 전하면 `unsent` 오류) 이 기기의 공간 데이터를 지웁니다.
async fn deliver_then_purge(
    node: &Arc<Node>,
    space: SpaceId,
    state: &SpaceState,
    ev: Option<SignedEvent>,
    unsent: &str,
) -> Result<()> {
    if let Some(ev) = ev {
        let user = node.me()?.user();
        let st = node
            .space_state(&space)
            .unwrap_or_else(|| Arc::new(state.clone()));
        let others = st.members.keys().any(|u| *u != user);
        if others && deliver_now(node, space, &st, vec![ev]).await == 0 {
            return Err(Error::Network(unsent.into()));
        }
    }
    purge_local(node, &space).await
}

/// 저장소에 남은 내 그 종류 상태 이벤트(가장 나중 것).
fn my_last_event(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    kind: EventKind,
) -> Result<Option<SignedEvent>> {
    Ok(node
        .store
        .events_by_lane(space, &[Lane::State])?
        .into_iter()
        .filter(|e| e.header.kind == kind && e.header.user == *user)
        .max_by_key(|e| (e.header.lamport, e.id())))
}

/// 추방 이벤트를 쫓겨난 사용자의 (인증된) 기기에도 보냅니다. 상태에서 빠져 전파 대상이 아니지만, 알려야 그 노드의
/// 화면이 멤버가 아님을 보여 줍니다(내용은 그 사용자가 이미 읽을 수 있던 상태 키로 암호화됨).
pub(crate) fn tell_removed(
    node: &Arc<Node>,
    space: SpaceId,
    devices: Vec<DeviceId>,
    ev: SignedEvent,
) {
    let targets: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, _, d)| devices.contains(d))
        .map(|(p, _, _)| p)
        .collect();
    for peer in targets {
        let req = YpRequest::PushEvents {
            space,
            events: vec![ev.clone()],
        };
        node.spawn_detached(move |node| async move {
            let _ = node.net.request(peer, req).await;
        });
    }
}

// ───────────── API 공용 ─────────────

/// 내가 멤버인 공간의 상태와 내 사용자 ID.
pub(crate) fn member_state(node: &Node, space: &SpaceId) -> Result<(Arc<SpaceState>, UserId)> {
    let me = node.me()?.user();
    let st = node
        .space_state(space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    if !st.is_member(&me) {
        return Err(Error::forbidden("이 공간의 멤버가 아닙니다"));
    }
    if st.disbanded {
        return Err(Error::forbidden("해산한 크루입니다"));
    }
    Ok((st, me))
}

/// 내가 멤버인 크루.
pub(crate) fn crew_state(node: &Node, space: &SpaceId) -> Result<(Arc<SpaceState>, UserId)> {
    let (st, me) = member_state(node, space)?;
    if st.kind != SpaceKind::Crew {
        return Err(Error::invalid("크루가 아닙니다"));
    }
    Ok((st, me))
}

/// 공간 단위 권한(채널 덮어쓰기 전).
pub(crate) fn need(state: &SpaceState, user: &UserId, p: Permissions) -> Result<()> {
    if state.base_permissions(user).contains(p) {
        Ok(())
    } else {
        Err(Error::forbidden(format!(
            "권한이 없습니다(필요: {})",
            p.names().join(", ")
        )))
    }
}

/// 이름류: 앞뒤 공백을 지우고 1~100자, 제어 문자 금지(해소기 규칙과 같음).
pub(crate) fn check_name(s: &str, what: &str) -> Result<String> {
    let t = s.trim();
    let n = t.chars().count();
    if n == 0 || n > MAX_NAME_CHARS || t.chars().any(char::is_control) {
        return Err(Error::invalid(format!(
            "{what}은(는) 1~{MAX_NAME_CHARS}자여야 하고 제어 문자를 쓸 수 없습니다"
        )));
    }
    Ok(t.to_owned())
}

/// 자유 텍스트: `max`자 이하, 줄바꿈·탭 말고 제어 문자 금지.
pub(crate) fn check_text(s: &str, max: usize, what: &str) -> Result<()> {
    let n = s.chars().count();
    if n > max || s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err(Error::invalid(format!(
            "{what}은(는) {max}자 이하여야 하고 제어 문자를 쓸 수 없습니다"
        )));
    }
    Ok(())
}

// ───────────── 포럼 게시글 변경 ─────────────
//
// 게시글 변경(`ForumPostUpdate`)은 상태 레인이라 해소기가 적용 여부를 정하고, 게시글(메시지 레인)과는 따로 옵니다.
// 게시글마다 설정 한 줄(`forum.meta.<공간>.<게시글>`, 내용 없이 ID·논리 시계만)에 적용된 변경 목록과 항목별 지금
// 값을 정한 변경을 둡니다. 변경이 적용되면 그 줄에 더하고(게시글을 알면 항목별 마지막 쓰기 우선으로 바로 반영),
// 게시글이 늦게 오거나 다시 해소하며 변경이 빠지면 그 줄의 남은 변경으로 게시글을 다시 계산합니다.

/// 게시글 하나의 변경 기록. 항목마다 (논리 시계, 이벤트 ID)가 큰 변경이 이기고, 저장소는 게시글 단위 표시 하나만
/// 두므로 그 표시에는 실제 논리 시계 대신 적용 순번(`seq`)을 넘깁니다(순서 판단은 여기서 끝냄).
#[derive(Default, Serialize, Deserialize)]
struct ForumRow {
    #[serde(default)]
    seq: u64,
    /// 해소기가 적용한 이 게시글의 변경(논리 시계, ID).
    #[serde(default)]
    updates: Vec<(u64, EventId)>,
    #[serde(default)]
    title: Option<(u64, EventId)>,
    #[serde(default)]
    tags: Option<(u64, EventId)>,
    #[serde(default)]
    pinned: Option<(u64, EventId)>,
    #[serde(default)]
    locked: Option<(u64, EventId)>,
}

impl ForumRow {
    fn winners(&self) -> [Option<(u64, EventId)>; 4] {
        [self.title, self.tags, self.pinned, self.locked]
    }

    /// 변경을 기록합니다(이미 있으면 거짓). 너무 많으면 지금 값을 정하지 않은 오래된 것부터 버립니다.
    fn add(&mut self, stamp: (u64, EventId)) -> bool {
        if self.updates.iter().any(|(_, id)| *id == stamp.1) {
            return false;
        }
        self.updates.push(stamp);
        if self.updates.len() > MAX_FORUM_UPDATES_PER_POST {
            self.updates.sort_unstable();
            let winners = self.winners();
            if let Some(i) = self
                .updates
                .iter()
                .position(|s| !winners.contains(&Some(*s)))
            {
                self.updates.remove(i);
            }
        }
        true
    }
}

fn forum_row_key(space: &SpaceId, post: &EventId) -> String {
    format!("{}{post}", forum_meta_prefix(space))
}

/// 게시글 변경 하나가 실제로 정하는 항목.
struct ForumFields {
    title: Option<String>,
    tags: Option<Vec<u32>>,
    pinned: Option<bool>,
    locked: Option<bool>,
}

/// 해소기에 넣기 직전(적용 잠금 안). 포럼 게시글 변경의 ID를 기억하고(해소 결과에서 게시글 변경을 고르는 데 씀),
/// 가입 이벤트에 실린 서명된 프로필·기기 목록은 저장합니다(멤버 목록 표시·피어 인증용, 서명은 저장 전에 검증).
pub(crate) fn on_decoded(node: &Arc<Node>, events: &[DecodedEvent]) {
    crate::moderation::on_decoded(node, events);
    for ev in events {
        crate::cards::on_decoded(node, ev);
        match &ev.body {
            EventBody::MemberFlair(f) => {
                node.crews
                    .flair_events
                    .lock()
                    .entry(ev.header.space)
                    .or_default()
                    .insert(ev.id, (ev.header.user, ev.header.lamport, f.clone()));
            }
            EventBody::ForumPostUpdate { .. } => {
                node.crews
                    .forum_updates
                    .lock()
                    .entry(ev.header.space)
                    .or_default()
                    .insert(ev.id);
            }
            EventBody::MemberJoin {
                member,
                devices,
                profile,
                ..
            } => {
                note_relay_candidate(node, ev, member.user);
                let _ = identity::accept_device_list(node, devices);
                if let Some(p) = profile {
                    let _ = identity::accept_profile(node, p);
                }
            }
            EventBody::DeviceListUpdate { devices, .. } => {
                note_relay_candidate(node, ev, devices.list.user);
            }
            _ => {}
        }
    }
}

/// 해소 뒤(적용 잠금 안). 해소기가 새로 적용한 게시글 변경을 반영하고, 다시 해소하며 빠진 변경은 그 게시글을 남은
/// 변경으로 다시 계산합니다. 본문은 RAM에 두지 않고 저장소에서 다시 풀어 씁니다: 먼저 거부됐다가(권한·채널이
/// 아직 안 옴) 빈 부모가 채워져 나중에 적용되는 변경도 잃지 않게.
pub(crate) fn on_resolved(
    node: &Arc<Node>,
    space: &SpaceId,
    state: Option<&SpaceState>,
    upd: &ResolveUpdate,
    notify: bool,
) -> Result<()> {
    // 밴과 함께 지울 최근 메시지(모더레이션).
    crate::moderation::on_resolved(node, space, state, upd, notify)?;
    settle_relays(node, upd);
    let flair: Vec<(UserId, u64, Flair)> = match node.crews.flair_events.lock().get_mut(space) {
        None => Vec::new(),
        Some(held) => {
            for (id, _) in &upd.rejected {
                held.remove(id);
            }
            upd.applied
                .iter()
                .filter_map(|id| held.remove(id))
                .collect()
        }
    };
    for (user, lamport, f) in flair {
        crate::flair::on_applied(node, *space, user, lamport, &f);
    }
    if notify && upd.state_changed {
        crate::flair::on_state_changed(node, *space);
        crate::cards::on_state_changed(node, *space);
    }
    let (applied, unapplied): (Vec<EventId>, Vec<EventId>) = {
        let known = node.crews.forum_updates.lock();
        let Some(ids) = known.get(space) else {
            return Ok(());
        };
        (
            upd.applied
                .iter()
                .filter(|id| ids.contains(id))
                .copied()
                .collect(),
            upd.unapplied
                .iter()
                .filter(|id| ids.contains(id))
                .copied()
                .collect(),
        )
    };
    for id in applied {
        if let Some(ev) = load_state_event(node, &id)? {
            apply_forum_update(node, state, &ev, notify)?;
        }
    }
    let mut touched = BTreeSet::new();
    for id in unapplied {
        let Some(ev) = load_state_event(node, &id)? else {
            continue;
        };
        let EventBody::ForumPostUpdate { post, .. } = ev.body else {
            continue;
        };
        let key = forum_row_key(space, &post);
        let mut row: ForumRow = node.store.get_setting(&key)?.unwrap_or_default();
        let before = row.updates.len();
        row.updates.retain(|(_, x)| *x != id);
        if row.updates.len() != before {
            node.store.put_setting(&key, &row)?;
            touched.insert(post);
        }
    }
    for post in touched {
        recompute_post(node, space, state, &post, notify)?;
    }
    Ok(())
}

/// 게시글(`ForumPost`)의 보기 행이 막 생겼을 때(적용 잠금 안). 먼저 와서 기록만 해 둔 변경으로 게시글을 계산합니다.
pub(crate) fn on_forum_post_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    state: Option<&SpaceState>,
    post: &EventId,
    notify: bool,
) -> Result<()> {
    recompute_post(node, space, state, post, notify)
}

/// 저장된 상태 레인 이벤트를 상태 키로 다시 풉니다(키가 없으면 `None`).
fn load_state_event(node: &Node, id: &EventId) -> Result<Option<DecodedEvent>> {
    let Some(ev) = node.store.get_event(id)? else {
        return Ok(None);
    };
    let (Some(bytes), KeyRef::State(kid)) = (ev.body.as_deref(), ev.header.key) else {
        return Ok(None);
    };
    let Some(k) = keys::crew_state_key(node, &ev.header.space, &kid)? else {
        return Ok(None);
    };
    Ok(decrypt_body(&k, &ev.header, bytes)
        .ok()
        .map(|body| DecodedEvent {
            id: ev.id(),
            header: ev.header.clone(),
            body,
        }))
}

/// 이 변경이 이 게시글에 정할 수 있는 항목. 다른 채널·게시글이 아닌 대상이면 `None`. 남의 게시글 제목·태그는 지금
/// MANAGE_MESSAGES가 있어야 합니다(해소기는 게시글 작성자를 모름). 아니면 그 변경 전체를 무시합니다.
fn forum_fields(
    state: Option<&SpaceState>,
    rec: &MessageRecord,
    ev: &DecodedEvent,
) -> Option<ForumFields> {
    let EventBody::ForumPostUpdate {
        title,
        tags,
        pinned,
        locked,
        ..
    } = &ev.body
    else {
        return None;
    };
    let channel = ev.header.channel?;
    if rec.space != ev.header.space || rec.channel != channel || rec.kind != MessageKind::ForumPost
    {
        debug!(event = %ev.id.short(), "다른 채널이나 게시글이 아닌 대상을 가리키는 게시글 변경 무시");
        return None;
    }
    if (title.is_some() || tags.is_some()) && ev.header.user != rec.author {
        let allowed = state.is_some_and(|s| {
            s.channel_permissions(&ev.header.user, &channel)
                .contains(Permissions::MANAGE_MESSAGES)
        });
        if !allowed {
            debug!(event = %ev.id.short(), "권한 없는 남의 게시글 제목·태그 변경 무시");
            return None;
        }
    }
    Some(ForumFields {
        title: title.clone(),
        tags: tags.clone(),
        pinned: *pinned,
        locked: *locked,
    })
}

/// 해소기가 적용한(작성 시점 권한·형식 확인을 거친) 게시글 변경 하나를 기록하고, 게시글을 알면 항목별 마지막 쓰기
/// 우선으로 바로 반영합니다. 게시글을 아직 모르면 기록만 해 두었다가 게시글이 오면 [`recompute_post`].
fn apply_forum_update(
    node: &Arc<Node>,
    state: Option<&SpaceState>,
    ev: &DecodedEvent,
    notify: bool,
) -> Result<()> {
    let EventBody::ForumPostUpdate { post, .. } = &ev.body else {
        return Ok(());
    };
    let space = ev.header.space;
    let key = forum_row_key(&space, post);
    let mut row: ForumRow = node.store.get_setting(&key)?.unwrap_or_default();
    let stamp = (ev.header.lamport, ev.id);
    let added = row.add(stamp);
    let fields = match node.store.get_message(post, None)? {
        Some(rec) => forum_fields(state, &rec, ev),
        None => None,
    };
    let Some(f) = fields else {
        if added {
            node.store.put_setting(&key, &row)?;
        }
        return Ok(());
    };
    // 이 항목을 더 나중 변경이 이미 정했으면 이번 값은 버립니다.
    let take = |slot: &mut Option<(u64, EventId)>| {
        let newer = slot.is_none_or(|cur| stamp > cur);
        if newer {
            *slot = Some(stamp);
        }
        newer
    };
    let update = ForumUpdate {
        event: ev.id,
        lamport: 0,
        title: f.title.filter(|_| take(&mut row.title)),
        tags: f.tags.filter(|_| take(&mut row.tags)),
        pinned: f.pinned.filter(|_| take(&mut row.pinned)),
        locked: f.locked.filter(|_| take(&mut row.locked)),
        edited_at: None,
    };
    // 제목·태그가 바뀌면 게시글에 '수정됨'을 남깁니다(고정·잠금은 아님).
    let update = ForumUpdate {
        edited_at: (update.title.is_some() || update.tags.is_some())
            .then_some(ev.header.created_at),
        ..update
    };
    if update.title.is_none()
        && update.tags.is_none()
        && update.pinned.is_none()
        && update.locked.is_none()
    {
        if added {
            node.store.put_setting(&key, &row)?;
        }
        return Ok(());
    }
    store_forum_update(node, state, &space, post, &key, row, update, notify)
}

/// 기록된 변경 전부로 게시글의 항목을 처음부터 다시 정합니다(게시글이 늦게 왔을 때, 적용됐던 변경이 다시 해소하며
/// 빠졌을 때). 정하는 변경이 없는 항목은 게시글 원래 값(고정·잠금은 꺼짐)으로 돌립니다.
fn recompute_post(
    node: &Arc<Node>,
    space: &SpaceId,
    state: Option<&SpaceState>,
    post: &EventId,
    notify: bool,
) -> Result<()> {
    let key = forum_row_key(space, post);
    let Some(mut row) = node.store.get_setting::<ForumRow>(&key)? else {
        return Ok(());
    };
    let Some(rec) = node.store.get_message(post, None)? else {
        return Ok(());
    };
    let ids: Vec<EventId> = row.updates.iter().map(|(_, id)| *id).collect();
    let mut evs = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(ev) = load_state_event(node, id)? {
            evs.push(ev);
        }
    }
    evs.sort_by_key(|e| (e.header.lamport, e.id));
    // 논리 시계 순으로 덮어 가면 항목마다 마지막 변경이 남습니다.
    let mut title = None;
    let mut tags = None;
    let mut pinned = None;
    let mut locked = None;
    let mut edited_at = None;
    for ev in &evs {
        let Some(f) = forum_fields(state, &rec, ev) else {
            continue;
        };
        let stamp = (ev.header.lamport, ev.id);
        if f.title.is_some() || f.tags.is_some() {
            edited_at = edited_at.max(Some(ev.header.created_at));
        }
        if let Some(v) = f.title {
            title = Some((stamp, v));
        }
        if let Some(v) = f.tags {
            tags = Some((stamp, v));
        }
        if let Some(v) = f.pinned {
            pinned = Some((stamp, v));
        }
        if let Some(v) = f.locked {
            locked = Some((stamp, v));
        }
    }
    let stamp_of = |w: &Option<((u64, EventId), _)>| w.as_ref().map(|(s, _)| *s);
    let next = [
        title.as_ref().map(|(s, _)| *s),
        tags.as_ref().map(|(s, _)| *s),
        stamp_of(&pinned),
        stamp_of(&locked),
    ];
    if next == row.winners() {
        return Ok(());
    }
    // 정하는 변경이 없어진 제목·태그는 게시글 원래 값으로.
    let original = if title.is_none() || tags.is_none() {
        original_post_meta(node, post)?
    } else {
        None
    };
    let update = ForumUpdate {
        event: next.iter().flatten().max().map_or(*post, |(_, id)| *id),
        lamport: 0,
        title: title
            .map(|(_, v)| v)
            .or_else(|| original.as_ref().map(|o| o.0.clone())),
        tags: tags
            .map(|(_, v)| v)
            .or_else(|| original.as_ref().map(|o| o.1.clone())),
        pinned: Some(pinned.is_some_and(|(_, v)| v)),
        locked: Some(locked.is_some_and(|(_, v)| v)),
        edited_at,
    };
    [row.title, row.tags, row.pinned, row.locked] = next;
    store_forum_update(node, state, space, post, &key, row, update, notify)
}

/// 표시를 먼저 남기고 저장소의 게시글 보기 행을 바꾼 뒤 알립니다. 그 사이에 멈추면 이 변경 하나를 잃을 뿐이지만,
/// 거꾸로면 순번이 저장소보다 뒤처져 이 게시글의 이후 변경이 모두 막힐 수 있습니다.
#[allow(clippy::too_many_arguments)]
fn store_forum_update(
    node: &Arc<Node>,
    state: Option<&SpaceState>,
    space: &SpaceId,
    post: &EventId,
    key: &str,
    mut row: ForumRow,
    update: ForumUpdate,
    notify: bool,
) -> Result<()> {
    row.seq = row.seq.saturating_add(1);
    node.store.put_setting(key, &row)?;
    let changed = node.store.apply_forum_update(
        post,
        &ForumUpdate {
            lamport: row.seq,
            ..update
        },
    )?;
    if !changed || !notify {
        return Ok(());
    }
    let Ok(me) = node.me() else { return Ok(()) };
    let Some(rec) = node.store.get_message(post, Some(&me.user()))? else {
        return Ok(());
    };
    let can_view = state.is_some_and(|s| {
        s.channel_permissions(&me.user(), &rec.channel)
            .contains(Permissions::VIEW_CHANNEL)
    });
    if can_view {
        node.notify.publish(Notification::MessageUpdated {
            space: *space,
            channel: rec.channel,
            message: messages::view_of(node, &rec),
        });
    }
    Ok(())
}

/// 게시글을 처음 쓸 때의 제목·태그(게시글 이벤트를 기록 키로 다시 풂).
fn original_post_meta(node: &Node, post: &EventId) -> Result<Option<(String, Vec<u32>)>> {
    let Some(ev) = node.store.get_event(post)? else {
        return Ok(None);
    };
    let (Some(bytes), KeyRef::Record { root }) = (ev.body.as_deref(), ev.header.key) else {
        return Ok(None);
    };
    let Some(k) = keys::root(node, &root)? else {
        return Ok(None);
    };
    let k = yangpa_crypto::keysched::record_key(&k);
    Ok(match decrypt_body(&k, &ev.header, bytes) {
        Ok(EventBody::ForumPost { title, tags, .. }) => Some((title, tags)),
        _ => None,
    })
}

#[cfg(test)]
pub(crate) mod tests_support {
    use std::collections::BTreeMap;

    use yangpa_core::ids::{DeviceId, RoleId, UserId};
    use yangpa_core::model::{Permissions, Role, SpaceKind, SpaceSettings, SpaceState};

    /// 소유자만 있는(멤버 목록은 빈) 크루 상태.
    pub(crate) fn empty_state() -> SpaceState {
        let mut roles = BTreeMap::new();
        roles.insert(
            RoleId::EVERYONE,
            Role::everyone(Permissions::DEFAULT_EVERYONE),
        );
        SpaceState {
            kind: SpaceKind::Crew,
            owner: UserId::random(),
            committer: DeviceId::random(),
            settings: SpaceSettings {
                name: "크루".into(),
                description: String::new(),
                icon: None,
                icon_key: None,
                allow_external_emoji: false,
                strict_rotation: false,
            },
            members: Default::default(),
            roles,
            channels: Default::default(),
            devices: Default::default(),
            invites: Default::default(),
            emojis: BTreeMap::new(),
            pending_crew_conversion: None,
            bans: Default::default(),
            archives: BTreeMap::new(),
            channel_transport: BTreeMap::new(),
            role_display: Default::default(),
            anonymous_channels: Default::default(),
            crew_servers: Default::default(),
            forum_featured: Default::default(),
            attach_limits: Default::default(),
            volatile_caps: Default::default(),
            edit_rules: Default::default(),
            emoji_limits: Default::default(),
            emoji_by: Default::default(),
            disbanded: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::ids::RoleId;
    use yangpa_core::model::{ChannelSettings, OverwriteTarget, PermissionOverwrite};

    #[test]
    fn relay_survives_a_transient_rejection_and_reapply() {
        use yangpa_sync::AuthError;
        // 확정 기기가 꺼진 사이 받은 가입은 다시 켜진 확정 기기에서 먼저 거부되고(초대 생성이 아직 안 옴) 다시 해소하면
        // 적용됩니다. 예전에는 거부 때 후보를 지워, 적용된 뒤에도 받아 준 기기가 전달자가 되지 않았습니다.
        let space = SpaceId::random();
        let joiner = UserId::random();
        let relay = |lamport: u64| InboxRelay {
            user: UserId::random(),
            device: DeviceId::random(),
            event: EventId::random(),
            at: Timestamp(lamport),
            lamport,
        };
        let devices = |r: &InboxRelays| -> Vec<DeviceId> {
            r.applied
                .get(&(space, joiner))
                .map(|l| l.iter().map(|x| x.device).collect())
                .unwrap_or_default()
        };
        let mut r = InboxRelays::default();
        let gateway = relay(10);
        r.note(space, joiner, gateway);
        r.settle(&ResolveUpdate {
            rejected: vec![(gateway.event, AuthError::Invalid("초대 없음"))],
            ..Default::default()
        });
        assert!(
            devices(&r).is_empty(),
            "거부된 가입의 작성 기기가 전달자가 됨"
        );
        let applied = |ids: Vec<EventId>| ResolveUpdate {
            applied: ids,
            ..Default::default()
        };
        r.settle(&applied(vec![gateway.event]));
        assert_eq!(devices(&r), vec![gateway.device]);

        // 적용이 풀리면 빠지고, 다시 적용되면 돌아옵니다.
        r.settle(&ResolveUpdate {
            unapplied: vec![gateway.event],
            ..Default::default()
        });
        assert!(devices(&r).is_empty());
        r.settle(&applied(vec![gateway.event]));
        assert_eq!(devices(&r), vec![gateway.device]);

        // 최근(논리 시계) 것 먼저 두 대까지: 늦게 적용된 옛 갱신이 최근 전달자를 밀어내지 않습니다.
        let recent = relay(30);
        let old = relay(5);
        r.note(space, joiner, recent);
        r.note(space, joiner, old);
        r.settle(&applied(vec![recent.event]));
        r.settle(&applied(vec![old.event]));
        assert_eq!(devices(&r), vec![recent.device, gateway.device]);
    }

    #[test]
    fn bootstrap_bundle_stays_within_budget_and_keeps_the_first() {
        // 첫 항목(시작 이벤트)은 혼자 한도를 넘어도 넣고, 그 뒤는 한도·개수 안에서 앞에서부터 끊습니다.
        assert_eq!(take_within(vec![10, 1, 1], |x| *x, 5, 512), vec![10]);
        assert_eq!(take_within(vec![2, 2, 2, 2], |x| *x, 5, 512), vec![2, 2]);
        assert_eq!(take_within(vec![1, 9, 1], |x| *x, 5, 512), vec![1]);
        assert_eq!(take_within(vec![1; 10], |x| *x, 100, 3), vec![1, 1, 1]);
        // 본문이 큰 이벤트 512개(어림 128MiB)는 전송 한도의 절반 안으로 잘립니다.
        let sizes = vec![256 * 1024 + 1024; 512];
        let kept = take_within(sizes, |x| *x, BOOTSTRAP_BYTES, MAX_EVENTS_PER_MESSAGE);
        assert!(kept.iter().sum::<usize>() <= BOOTSTRAP_BYTES);
        assert!(!kept.is_empty() && kept.len() < 512);
    }

    #[test]
    fn precheck_requires_pinned_genesis_in_space_stream() {
        let space = SpaceId::random();
        assert!(precheck_bootstrap(&space, &EventId::random(), &UserId::random(), &[]).is_err());
    }

    #[test]
    fn private_groups_follow_view_permission() {
        let mut st = tests_support::empty_state();
        let owner = st.owner;
        let bob = UserId::random();
        let role = RoleId::random();
        for u in [owner, bob] {
            st.members.insert(
                u,
                Arc::new(Member {
                    user: u,
                    roles: Vec::new(),
                    nickname: None,
                    automation: false,
                }),
            );
        }
        let secret = ChannelId::random();
        let mut s = ChannelSettings::text("비밀");
        s.overwrites = vec![
            PermissionOverwrite {
                target: OverwriteTarget::Role(RoleId::EVERYONE),
                allow: Permissions::NONE,
                deny: Permissions::VIEW_CHANNEL,
            },
            PermissionOverwrite {
                target: OverwriteTarget::Role(role),
                allow: Permissions::VIEW_CHANNEL,
                deny: Permissions::NONE,
            },
        ];
        st.channels.insert(secret, s.clone());
        let open = ChannelId::random();
        let plain = ChannelSettings::text("일반");
        st.channels.insert(open, plain.clone());
        // 전원이 보는 채널은 크루 키를 쓰고, 역할로 가린 채널만 비공개 그룹(열람 규칙에서 나온 ID)을 씁니다.
        assert!(needs_channel_group(&st, &secret, &s));
        assert!(!needs_channel_group(&st, &open, &plain));
        let space = SpaceId::random();
        let group = st.channel_key_group(&space, &s).expect("비공개 그룹");
        assert!(st.can_view_key_group(&space, &group, &owner));
        assert!(!st.can_view_key_group(&space, &group, &bob));
        assert!(
            !st.channel_permissions(&bob, &secret)
                .contains(Permissions::VIEW_CHANNEL)
        );
        assert!(
            st.channel_permissions(&owner, &secret)
                .contains(Permissions::VIEW_CHANNEL)
        );
    }

    #[test]
    fn channels_hidden_by_role_or_member_deny_get_their_own_group() {
        let mut st = tests_support::empty_state();
        let owner = st.owner;
        let bob = UserId::random();
        for u in [owner, bob] {
            st.members.insert(
                u,
                Arc::new(Member {
                    user: u,
                    roles: Vec::new(),
                    nickname: None,
                    automation: false,
                }),
            );
        }
        // 멤버 한 명만 보기 거부: @everyone 거부가 아니어도 공간 그룹 키로 나누면 안 됨.
        let denied = ChannelId::random();
        let mut s = ChannelSettings::text("밥 빼고");
        s.overwrites = vec![PermissionOverwrite {
            target: OverwriteTarget::Member(bob),
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        }];
        assert!(!s.is_private());
        st.channels.insert(denied, s.clone());
        assert!(needs_channel_group(&st, &denied, &s));
        let open = ChannelId::random();
        let plain = ChannelSettings::text("일반");
        st.channels.insert(open, plain.clone());
        assert!(!needs_channel_group(&st, &open, &plain));
        // 역할 수준: @everyone 기본 권한에 보기가 없으면 덮어쓰기가 없어도 모든 채널이 채널 그룹.
        st.roles.get_mut(&RoleId::EVERYONE).unwrap().permissions = Permissions::NONE;
        assert!(needs_channel_group(&st, &open, &plain));
        // 휘발성·카테고리는 그룹이 없음(세션 키, 메시지 없음).
        let cat = ChannelSettings {
            kind: ChannelKind::Category,
            ..ChannelSettings::text("묶음")
        };
        assert!(!needs_channel_group(&st, &ChannelId::random(), &cat));
    }

    #[test]
    fn names_and_texts_are_checked() {
        assert_eq!(check_name("  일반 ", "이름").unwrap(), "일반");
        assert!(check_name("   ", "이름").is_err());
        assert!(check_name("a\u{7}b", "이름").is_err());
        assert!(check_name(&"가".repeat(MAX_NAME_CHARS + 1), "이름").is_err());
        assert!(check_text("줄\n바꿈", 10, "글").is_ok());
        assert!(check_text("abc", 2, "글").is_err());
    }
}
