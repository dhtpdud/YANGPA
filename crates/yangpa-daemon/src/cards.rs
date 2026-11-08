//! 프로필·뱃지 카드를 공간마다 빠뜨리지 않고 전합니다(protocol.md "프로필").
//!
//! - 싣기([`spread`]): 공간마다 실린 판([`Sent`])을 적어 두고 지금 판이 더 높은 공간에만 상태 이벤트를 냅니다. 휴면·실패로
//!   건너뛴 공간, 둘뿐일 때 바꾼 프로필, 이벤트에 실린 적 없는 크루 만든 사람의 프로필이 여기서 메워집니다.
//! - 받아오기([`need_pull`]): 체크포인트로 들어온 기기는 경계 아래 이벤트를 받지 않습니다. 멤버마다 새 멤버가 올 때 다시
//!   내면 3만 명 크루가 멈추므로 새 멤버가 멤버 기기에게 묶어 묻습니다. 둘 다 신원 키 서명이라 누가 전해도 확인됩니다.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::body::EventBody;
use yangpa_core::ids::{DeviceId, SpaceId, UserId};
use yangpa_core::wire::{
    MAX_MEMBER_CARD_BYTES, MAX_MEMBER_CARDS_REQUEST, MemberCard, WireErrorCode, YpRequest,
    YpResponse,
};
use yangpa_net::PeerId;
use yangpa_sync::DecodedEvent;

use crate::author::{self, KeyChoice};
use crate::node::Node;

/// 건너뛴 공간을 다시 보고 받아오기를 잇는 간격.
const TICK: Duration = Duration::from_secs(60);
const SENT_PREFIX: &str = "cards.sent.";
const PULL_PREFIX: &str = "cards.pull.";

#[derive(Default)]
pub(crate) struct CardState {
    spreading: tokio::sync::Mutex<()>,
    pulling: tokio::sync::Mutex<()>,
    /// 이번 실행에서 끝까지 답한 기기(남은 사람을 같은 기기에 다시 묻지 않게).
    asked: Mutex<HashSet<(SpaceId, DeviceId)>>,
}

/// 이 공간에 실린 내 프로필·카드의 가장 높은 판.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Sent {
    profile: u64,
    card: u64,
}

fn sent_key(space: &SpaceId) -> String {
    format!("{SENT_PREFIX}{space}")
}

fn pull_key(space: &SpaceId) -> String {
    format!("{PULL_PREFIX}{space}")
}

pub(crate) fn start(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        loop {
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            spread(&node, node.space_ids()).await;
            pull_pending(&node).await;
            drop(node);
            tokio::time::sleep(TICK).await;
        }
    });
}

/// 해소기에 넣기 직전(적용 잠금 안). 내 이벤트에 실린 판을 적습니다. 시작 때 다시 세우며 여기를 지나므로 재시작해도 다시
/// 내지 않습니다.
///
/// shortcut: 경계 아래 이벤트는 다시 세울 때 지나지 않아, 이 기록이 없던 기기는 체크포인트가 있는 공간에 한 번 더 냅니다.
/// 공개 전 빌드만 해당해 그대로 둡니다.
pub(crate) fn on_decoded(node: &Node, ev: &DecodedEvent) {
    let (owner, profile, card) = match &ev.body {
        // 가입 이벤트는 받아 준 멤버가 씁니다. 가입자를 봐야 새 멤버마다 프로필을 또 내지 않습니다.
        EventBody::MemberJoin {
            member,
            profile: Some(p),
            ..
        } if p.user == member.user => (member.user, p.profile.version, 0),
        EventBody::MemberProfile {
            profile: Some(p), ..
        } => (ev.header.user, p.profile.version, 0),
        EventBody::MemberBadges { card } => (ev.header.user, 0, card.card.version),
        _ => return,
    };
    if !node.me().is_ok_and(|me| me.user() == owner) {
        return;
    }
    let key = sent_key(&ev.header.space);
    let old: Sent = node
        .store
        .get_setting(&key)
        .ok()
        .flatten()
        .unwrap_or_default();
    let new = Sent {
        profile: old.profile.max(profile),
        card: old.card.max(card),
    };
    if new != old
        && let Err(e) = node.store.put_setting(&key, &new)
    {
        debug!(error = %e, "실린 프로필 판 기록 실패");
    }
}

/// 공간 상태가 바뀐 뒤(적용 잠금 안): 셋째 멤버가 들어온 크루처럼 이제 실어야 할 공간이면 바로 냅니다.
pub(crate) fn on_state_changed(node: &Arc<Node>, space: SpaceId) {
    if !due_in(node, &space).is_empty() {
        node.spawn_detached(move |node| async move { spread(&node, vec![space]).await });
    }
}

/// 공간을 지울 때.
pub(crate) fn forget(node: &Node, space: &SpaceId) {
    let _ = node.store.delete_setting(&sent_key(space));
    let _ = node.store.delete_setting(&pull_key(space));
    node.cards.asked.lock().retain(|(s, _)| s != space);
}

/// 이 공간에 아직 싣지 않은 내 프로필·카드. 둘뿐인 공간은 서로 직접 주고받고, 셋째가 들어오면 그때 실립니다.
fn due_in(node: &Node, space: &SpaceId) -> Vec<EventBody> {
    let Ok(me) = node.me() else { return Vec::new() };
    let group = node.space_state(space).is_some_and(|st| {
        st.members.len() > 2
            && !st.disbanded
            && st.is_member(&me.user())
            && st.device_belongs(&me.user(), &me.device_id())
    });
    if !group {
        return Vec::new();
    }
    let sent: Sent = node
        .store
        .get_setting(&sent_key(space))
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut due = Vec::new();
    if me.profile.profile.version > sent.profile {
        due.push(EventBody::MemberProfile {
            nickname: None,
            profile: Some(me.profile.clone()),
        });
    }
    if let Some(card) = crate::badges::load_card(node, &me.user())
        && card.card.version > sent.card
    {
        due.push(EventBody::MemberBadges { card });
    }
    due
}

/// 빠진 공간에 내 프로필·카드를 냅니다. 휴면이거나 실패한 공간은 기록이 그대로라 다음 주기에 다시 봅니다.
pub(crate) async fn spread(node: &Arc<Node>, spaces: Vec<SpaceId>) {
    let _one = node.cards.spreading.lock().await;
    for space in spaces {
        let due = due_in(node, &space);
        if due.is_empty() || crate::policy::is_dormant(node, &space) {
            continue;
        }
        for body in &due {
            if let Err(e) = author::publish(node, space, None, body, KeyChoice::State).await {
                debug!(space = %space.short(), error = %e, "프로필·뱃지 이벤트 발행 실패");
            }
        }
    }
}

/// 체크포인트로 공간을 세운 뒤: 경계 아래에 묻힌 멤버의 프로필·카드를 받아 옵니다.
pub(crate) fn need_pull(node: &Arc<Node>, space: SpaceId) {
    if let Err(e) = node.store.put_setting(&pull_key(&space), &space) {
        debug!(error = %e, "받아 올 공간 기록 실패");
    }
    on_peer(node);
}

/// 새 피어가 인증됨: 기다리던 공간의 멤버면 이제 물을 수 있습니다.
pub(crate) fn on_peer(node: &Arc<Node>) {
    node.spawn_detached(|node| async move { pull_pending(&node).await });
}

async fn pull_pending(node: &Arc<Node>) {
    // 돌고 있으면 그쪽에 맡깁니다. 그 뒤에 인증된 피어는 다음 주기가 봅니다.
    let Ok(_one) = node.cards.pulling.try_lock() else {
        return;
    };
    let rows = node
        .store
        .settings_with_prefix(PULL_PREFIX)
        .unwrap_or_default();
    for (_, v) in rows {
        if let Ok(space) = serde_json::from_value::<SpaceId>(v) {
            pull(node, space).await;
        }
    }
}

fn unknown(node: &Node, user: &UserId) -> bool {
    matches!(node.store.get_profile(user), Ok(None))
}

/// 프로필을 모르는 멤버를 물어 받습니다. 받은 것은 곧바로 저장되므로 끊겨도 남은 사람만 다시 묻습니다. 카드는 없는 사람이
/// 대부분이라 기다릴 기준이 못 되므로 프로필과 함께만 받습니다.
///
/// shortcut: 프로필을 이미 아는 멤버의 카드는 묻지 않습니다(친구·다른 크루로 아는 사이면 카드도 그 경로로 받음). 빠지는 일이
/// 보이면 카드 판을 함께 견주는 요청으로 넓힙니다.
async fn pull(node: &Arc<Node>, space: SpaceId) {
    let (Some(st), Ok(me)) = (node.space_state(&space), node.me()) else {
        return;
    };
    let mut want: Vec<UserId> = st
        .members
        .keys()
        .filter(|u| **u != me.user() && unknown(node, u))
        .copied()
        .collect();
    let peers = crate::keygroup::relay::candidates(node, &space, |_, d| {
        !node.cards.asked.lock().contains(&(space, *d))
    });
    for peer in peers {
        if want.is_empty() {
            break;
        }
        let done = ask(node, space, peer, &want).await;
        want.retain(|u| unknown(node, u));
        if done && let Some((_, device)) = node.peers.identity_of(&peer) {
            node.cards.asked.lock().insert((space, device));
        }
    }
    if want.is_empty() {
        let _ = node.store.delete_setting(&pull_key(&space));
        node.cards.asked.lock().retain(|(s, _)| *s != space);
    }
}

/// 끝까지 답을 받았으면(답하지 못하는 피어도) true, 연결이 끊겨 다시 물어야 하면 false.
async fn ask(node: &Arc<Node>, space: SpaceId, peer: PeerId, want: &[UserId]) -> bool {
    for page in want.chunks(MAX_MEMBER_CARDS_REQUEST) {
        let req = YpRequest::GetMemberCards {
            space,
            users: page.to_vec(),
        };
        match crate::p2p::request(node, peer, req).await {
            Ok(YpResponse::MemberCards(items)) => accept(node, page, items),
            Ok(_) => return true,
            Err(e) => {
                debug!(space = %space.short(), %peer, error = %e, "멤버 프로필을 묻지 못함");
                return false;
            }
        }
    }
    true
}

/// 물은 사람의 것만 받습니다(서명은 받는 함수가 확인).
fn accept(node: &Arc<Node>, asked: &[UserId], items: Vec<MemberCard>) {
    for it in items {
        if let Some(p) = it.profile.filter(|p| asked.contains(&p.user)) {
            let _ = crate::identity::accept_profile(node, &p);
        }
        if let Some(c) = it.card.filter(|c| asked.contains(&c.user)) {
            let _ = crate::badges::accept_card(node, &c);
        }
    }
}

/// `GetMemberCards`: 이 공간 멤버 기기에게, 이 공간 멤버의 것만 답합니다(프로필의 이미지 키는 같은 공간 멤버까지만 갑니다).
pub(crate) fn handle_get(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    users: Vec<UserId>,
) -> YpResponse {
    let state = match crate::checkpoint::member_peer(node, &peer, &space) {
        Ok((_, s)) => s,
        Err(r) => return r,
    };
    if users.len() > MAX_MEMBER_CARDS_REQUEST {
        return YpResponse::error(WireErrorCode::BadRequest, "멤버가 너무 많음");
    }
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let mut items = Vec::new();
    for user in users.iter().filter(|u| state.is_member(u)) {
        let profile = if *user == me.user() {
            Some(me.profile.clone())
        } else {
            node.store.get_profile(user).ok().flatten()
        };
        let item = MemberCard {
            profile,
            card: crate::badges::load_card(node, user),
        };
        let fits = postcard::to_stdvec(&item).is_ok_and(|b| b.len() <= MAX_MEMBER_CARD_BYTES);
        if fits && (item.profile.is_some() || item.card.is_some()) {
            items.push(item);
        }
    }
    YpResponse::MemberCards(items)
}
