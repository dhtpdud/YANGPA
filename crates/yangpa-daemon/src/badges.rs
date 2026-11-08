//! 도전과제 뱃지(decisions.md "도전과제 뱃지", protocol.md "뱃지 카드").
//!
//! - 내 카드는 `badges.card.<나>`(서명된 [`SignedBadgeCard`])에 둡니다. 모은 뱃지는 한 번 얻으면 남고, 주기 작업([`TICK`])이
//!   기준(친구·소유한 크루 멤버 수·시딩·통화·화면 공유 시간·후원액)을 다시 보아 새로 넘은 것을 더해 다시 서명합니다.
//! - 계정 단위입니다: 내 기기끼리 기기별 값([`OwnBadgeCounts`], `badges.own.<기기>`)을 주고받아 시딩은 더하고 후원액은
//!   가장 큰 것을 씁니다(통화·화면 공유 시간도 더함). 내 카드를 내 다른 기기에게서 받으면 모은 뱃지는 합치고 보일 뱃지는 더 새 판을 따릅니다.
//! - 카드는 인증 직후와 바뀔 때 인증된 친구·멤버·내 기기에게 보내고(`YpRequest::Badges`), 여럿의 공간에는 상태
//!   이벤트(`MemberBadges`)로 실어 직접 연결되지 않은 멤버도 받게 합니다(프로필과 같음). 받은 카드는 `badges.card.<사용자>`에
//!   더 새 판만 둡니다.
//! - 원년멤버는 개발자 계정([`yangpa_core::badge::DEVELOPER`])이 서명한 증서만 인정합니다. 개발자 기기는 준 증서를
//!   `badges.granted.<사용자>`에 두고 그 사용자의 기기와 인증될 때마다 보냅니다.
//! - 시딩은 남(내 기기 제외)에게 내준 블롭 조각 바이트입니다([`add_seeded`]). 조각마다 DB에 쓰지 않고 RAM에 모았다가 주기
//!   작업과 종료 때 씁니다. 통화·화면 공유(WebRTC·Tor 중계·SFU)는 블롭이 아니라 시딩에 들지 않고 따로 시간으로 셉니다.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tracing::debug;
use yangpa_core::badge::{
    Badge, BadgeCard, CREW_MEMBERS, FRIENDS, FounderGrant, MAX_SHOWN, SCREEN_SECS, SEED_BYTES,
    SUPPORT_CENTS, ShownBadge, SignedBadgeCard, VOICE_SECS,
};
use yangpa_core::ids::{DeviceId, UserId};
use yangpa_core::model::SpaceKind;
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{BadgeMsg, OwnBadgeCounts, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::PeerId;
use yangpa_rpc::api::{BadgeView, Notification, ShownBadgeView};
use yangpa_store::ContactKind;

use crate::error::{Error, Result};
use crate::node::Node;

const CARD_PREFIX: &str = "badges.card.";
const OWN_PREFIX: &str = "badges.own.";
const GRANTED_PREFIX: &str = "badges.granted.";
const SEEDED_KEY: &str = "badges.seeded";
const SUPPORT_KEY: &str = "badges.support_cents";
const VOICE_KEY: &str = "badges.voice_secs";
const SCREEN_KEY: &str = "badges.screen_secs";
/// 기준을 다시 보고 시딩 바이트를 쓰는 간격.
const TICK: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct BadgeState {
    /// 아직 DB에 쓰지 않은 시딩 바이트.
    pending_seeded: AtomicU64,
    /// 내 카드 고치기를 한 줄로(주기 작업·받은 카드·RPC가 겹쳐 판이 엇갈리지 않게).
    card_lock: Mutex<()>,
    /// 마지막으로 내 다른 기기에 보낸 기기별 값.
    sent_own: Mutex<Option<OwnBadgeCounts>>,
}

pub(crate) fn start(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            tick(&node);
        }
    });
}

/// 시딩 바이트·통화 시간을 쓰고, 기기별 값이 바뀌었으면 내 다른 기기에 알리고, 기준을 다시 봅니다.
pub(crate) fn tick(node: &Arc<Node>) {
    flush(node);
    add_call_time(node);
    let own = own_counts(node);
    if node.badges.sent_own.lock().replace(own) != Some(own) {
        for_own_devices(node, |peer| send(node, peer, BadgeMsg::Own(own)));
    }
    refresh(node, None);
}

/// 종료 직전: 모은 시딩 바이트를 씁니다.
pub(crate) fn flush(node: &Node) {
    let add = node.badges.pending_seeded.swap(0, Ordering::Relaxed);
    if add == 0 {
        return;
    }
    let total = setting::<u64>(node, SEEDED_KEY).saturating_add(add);
    if let Err(e) = node.store.put_setting(SEEDED_KEY, &total) {
        debug!(error = %e, "시딩 바이트 저장 실패");
    }
}

/// shortcut: 주기 작업 때 통화 중이면 한 주기를 통째로 더합니다(통화마다 1분 안쪽 오차, 내 두 기기가 함께 통화하면 둘 다
/// 셈). 더 정확해야 하면 참가·나감 시각으로 셉니다.
fn add_call_time(node: &Node) {
    let (voice, screen) = crate::voice::talking(node);
    for (on, key) in [(voice, VOICE_KEY), (screen, SCREEN_KEY)] {
        if !on {
            continue;
        }
        let total = setting::<u64>(node, key).saturating_add(TICK.as_secs());
        if let Err(e) = node.store.put_setting(key, &total) {
            debug!(error = %e, "통화 시간 저장 실패");
        }
    }
}

/// 남의 기기에 블롭 조각을 내줌(`blobs::serve`). 통화·화면 공유는 이 길을 지나지 않습니다.
pub(crate) fn add_seeded(node: &Node, to: &UserId, bytes: u64) {
    if node.me().is_ok_and(|me| me.user() != *to) {
        node.badges
            .pending_seeded
            .fetch_add(bytes, Ordering::Relaxed);
    }
}

/// 후원자 확인에서 본 한 번 결제액(`supporter`). 가장 큰 값만 남깁니다.
pub(crate) fn on_support_seen(node: &Node, cents: u64) {
    if cents > setting::<u64>(node, SUPPORT_KEY)
        && let Err(e) = node.store.put_setting(SUPPORT_KEY, &cents)
    {
        debug!(error = %e, "후원액 저장 실패");
    }
}

fn setting<T: serde::de::DeserializeOwned + Default>(node: &Node, key: &str) -> T {
    node.store
        .get_setting(key)
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn card_key(user: &UserId) -> String {
    format!("{CARD_PREFIX}{user}")
}

fn developer(node: &Node) -> Option<UserId> {
    node.cfg.developer
}

fn own_counts(node: &Node) -> OwnBadgeCounts {
    OwnBadgeCounts {
        seeded_bytes: setting::<u64>(node, SEEDED_KEY)
            .saturating_add(node.badges.pending_seeded.load(Ordering::Relaxed)),
        support_cents: setting(node, SUPPORT_KEY),
        voice_secs: setting(node, VOICE_KEY),
        screen_secs: setting(node, SCREEN_KEY),
    }
}

/// 지금 넘은 기준의 뱃지 비트(원년멤버 빼고).
fn reached(node: &Node, me: &UserId) -> u64 {
    let mut sum = own_counts(node);
    if let Ok(rows) = node.store.settings_with_prefix(OWN_PREFIX) {
        for (_, v) in rows {
            if let Ok(c) = serde_json::from_value::<OwnBadgeCounts>(v) {
                sum.seeded_bytes = sum.seeded_bytes.saturating_add(c.seeded_bytes);
                sum.support_cents = sum.support_cents.max(c.support_cents);
                sum.voice_secs = sum.voice_secs.saturating_add(c.voice_secs);
                sum.screen_secs = sum.screen_secs.saturating_add(c.screen_secs);
            }
        }
    }
    let friends = node
        .store
        .list_friends()
        .map(|f| f.iter().filter(|f| f.kind == ContactKind::Friend).count())
        .unwrap_or(0) as u64;
    let crew = node
        .space_ids()
        .into_iter()
        .filter_map(|s| node.space_state(&s))
        .filter(|st| st.kind == SpaceKind::Crew && st.owner == *me)
        .map(|st| st.members.len() as u64)
        .max()
        .unwrap_or(0);
    Badge::reached(&SUPPORT_CENTS, sum.support_cents)
        | Badge::reached(&FRIENDS, friends)
        | Badge::reached(&CREW_MEMBERS, crew)
        | Badge::reached(&SEED_BYTES, sum.seeded_bytes)
        | Badge::reached(&VOICE_SECS, sum.voice_secs)
        | Badge::reached(&SCREEN_SECS, sum.screen_secs)
}

pub(crate) fn load_card(node: &Node, user: &UserId) -> Option<SignedBadgeCard> {
    node.store.get_setting(&card_key(user)).ok().flatten()
}

/// 내 카드를 고칩니다: 기준을 다시 보고 `edit`을 적용해 바뀌었으면 새 판으로 서명·저장하고 퍼뜨립니다. 결과가 내 다른
/// 기기에게서 받은 카드(`adopt`)와 같으면 다시 서명하지 않고 그대로 둡니다(두 기기가 서로 판을 올리며 주고받지 않게).
fn update_card(
    node: &Arc<Node>,
    adopt: Option<&SignedBadgeCard>,
    edit: impl FnOnce(&mut BadgeCard),
) -> Result<()> {
    let me = node.me()?;
    let _g = node.badges.card_lock.lock();
    let old = load_card(node, &me.user());
    let mut card = old.as_ref().map(|c| c.card.clone()).unwrap_or_default();
    card.earned |= reached(node, &me.user());
    if card.founder.is_some() {
        card.earned |= Badge::Founder.mask();
    }
    edit(&mut card);
    if let Some(c) = adopt.filter(|c| c.card == card) {
        // 같은 카드를 직접·상태 이벤트로 또 받으면 아무것도 하지 않습니다.
        if old.as_ref() != Some(c) {
            node.store.put_setting(&card_key(&me.user()), c)?;
            node.notify
                .publish(Notification::BadgesUpdated { user: me.user() });
        }
        return Ok(());
    }
    if old.as_ref().is_some_and(|o| o.card == card)
        || (old.is_none() && card == BadgeCard::default())
    {
        return Ok(());
    }
    card.version = card
        .version
        .saturating_add(1)
        .max(Timestamp::now().as_millis());
    let signed = card.sign(&me.identity.signing)?;
    node.store.put_setting(&card_key(&me.user()), &signed)?;
    node.notify
        .publish(Notification::BadgesUpdated { user: me.user() });
    broadcast(node, &signed);
    Ok(())
}

/// 기준을 다시 보고(받은 증서가 있으면 더해) 바뀌었으면 퍼뜨립니다.
pub(crate) fn refresh(node: &Arc<Node>, founder: Option<FounderGrant>) {
    if let Err(e) = update_card(node, None, |c| {
        if let Some(g) = founder {
            c.founder = Some(g);
            c.earned |= Badge::Founder.mask();
        }
    }) {
        debug!(error = %e, "뱃지 카드 갱신 실패");
    }
}

/// `badge.set_shown`.
pub(crate) fn set_shown(node: &Arc<Node>, shown: &[ShownBadgeView]) -> Result<BadgeView> {
    let me = node.me()?;
    if shown.len() > MAX_SHOWN {
        return Err(Error::invalid("뱃지는 두 개까지 보일 수 있습니다"));
    }
    let mut seen = 0u64;
    for s in shown {
        if seen & s.badge.mask() != 0 {
            return Err(Error::invalid("같은 뱃지를 두 번 고를 수 없습니다"));
        }
        seen |= s.badge.mask();
    }
    let mut missing = false;
    update_card(node, None, |c| {
        missing = c.earned & seen != seen;
        if !missing {
            c.shown = shown
                .iter()
                .map(|s| ShownBadge {
                    bit: s.badge.bit(),
                    after: s.after,
                })
                .collect();
        }
    })?;
    if missing {
        return Err(Error::invalid("모으지 않은 뱃지는 보일 수 없습니다"));
    }
    Ok(view(node, &me.user()))
}

/// `badge.get`.
pub(crate) fn view(node: &Node, user: &UserId) -> BadgeView {
    let dev = developer(node);
    let card = load_card(node, user);
    let earned = card.as_ref().map_or(0, |c| c.earned(dev.as_ref()));
    let me = node.me().ok().map(|m| m.user());
    BadgeView {
        earned: Badge::list(earned),
        shown: card
            .map(|c| c.shown(dev.as_ref()))
            .unwrap_or_default()
            .into_iter()
            .map(|(badge, after)| ShownBadgeView { badge, after })
            .collect(),
        can_grant_founder: dev.is_some()
            && dev == me
            && earned & Badge::Founder.mask() == 0
            && node
                .store
                .get_setting::<FounderGrant>(&format!("{GRANTED_PREFIX}{user}"))
                .ok()
                .flatten()
                .is_none(),
    }
}

/// `badge.grant_founder`: 개발자 계정만 원년멤버 증서를 서명해 그 사용자의 기기에 보냅니다(나 자신이면 바로 내 카드에).
pub(crate) fn grant_founder(node: &Arc<Node>, user: UserId) -> Result<()> {
    let me = node.me()?;
    if developer(node) != Some(me.user()) {
        return Err(Error::forbidden("개발자 계정만 원년멤버를 줄 수 있습니다"));
    }
    let grant = FounderGrant::sign(&me.identity.signing, user)?;
    node.store
        .put_setting(&format!("{GRANTED_PREFIX}{user}"), &grant)?;
    if user == me.user() {
        update_card(node, None, |c| {
            c.founder = Some(grant);
            c.earned |= Badge::Founder.mask();
        })?;
    } else {
        for (peer, u, _) in node.peers.authed_peers() {
            if u == user {
                send(node, peer, BadgeMsg::Grant(grant.clone()));
            }
        }
    }
    Ok(())
}

/// 인증된 상대 기기(`identity::on_authenticated`·`on_auth_accepted`): 내 카드, 내 기기면 기기별 값, 내가 준 증서를 보냅니다.
pub(crate) fn on_authed(node: &Arc<Node>, peer: PeerId, user: UserId, device: DeviceId) {
    let Ok(me) = node.me() else { return };
    if crate::archive::archive_only(node, &user, &device) {
        return;
    }
    if user == me.user() {
        send(node, peer, BadgeMsg::Own(own_counts(node)));
    } else if let Some(g) = node
        .store
        .get_setting::<FounderGrant>(&format!("{GRANTED_PREFIX}{user}"))
        .ok()
        .flatten()
    {
        send(node, peer, BadgeMsg::Grant(g));
    }
    if let Some(card) = load_card(node, &me.user()) {
        send(node, peer, BadgeMsg::Card(card));
    }
}

fn for_own_devices(node: &Arc<Node>, mut f: impl FnMut(PeerId)) {
    let Ok(me) = node.me() else { return };
    for (peer, user, device) in node.peers.authed_peers() {
        if user == me.user() && device != me.device_id() {
            f(peer);
        }
    }
}

/// 바뀐 내 카드: 인증된 피어에게 바로, 셋 이상인 공간에는 상태 이벤트로(`cards::spread`).
fn broadcast(node: &Arc<Node>, card: &SignedBadgeCard) {
    for (peer, user, device) in node.peers.authed_peers() {
        if !crate::archive::archive_only(node, &user, &device) {
            send(node, peer, BadgeMsg::Card(card.clone()));
        }
    }
    node.spawn_detached(|node| async move {
        let spaces = node.space_ids();
        crate::cards::spread(&node, spaces).await;
    });
}

fn send(node: &Arc<Node>, peer: PeerId, msg: BadgeMsg) {
    node.spawn_detached(move |node| async move {
        let req = YpRequest::Badges(Box::new(msg));
        match crate::identity::request_after_auth(&node, peer, req).await {
            Ok(YpResponse::Ok) => {}
            // 옛 빌드는 이 요청을 모릅니다.
            Ok(_) => debug!(peer = %peer, "뱃지를 받지 않음"),
            Err(e) => debug!(peer = %peer, error = %e, "뱃지 보내기 실패"),
        }
    });
}

/// 받은 `YpRequest::Badges`.
pub(crate) fn handle(node: &Arc<Node>, peer: PeerId, msg: BadgeMsg) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증되지 않음");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    match msg {
        BadgeMsg::Card(c) => {
            if c.user != user {
                return YpResponse::error(WireErrorCode::BadRequest, "다른 사용자의 카드");
            }
            if accept_card(node, &c).is_err() {
                return YpResponse::error(WireErrorCode::BadRequest, "카드 서명이 올바르지 않음");
            }
        }
        BadgeMsg::Grant(g) => {
            let dev = developer(node);
            if dev != Some(user) || g.user != me.user() || g.verify(&user).is_err() {
                return YpResponse::error(WireErrorCode::BadRequest, "증서가 올바르지 않음");
            }
            let have = load_card(node, &me.user()).is_some_and(|c| c.card.founder.is_some());
            if !have {
                refresh(node, Some(g));
            }
        }
        BadgeMsg::Own(c) => {
            if user != me.user() || device == me.device_id() {
                return YpResponse::error(WireErrorCode::BadRequest, "내 기기가 아님");
            }
            if let Err(e) = node.store.put_setting(&format!("{OWN_PREFIX}{device}"), &c) {
                debug!(error = %e, "기기별 뱃지 값 저장 실패");
            }
            refresh(node, None);
        }
    }
    YpResponse::Ok
}

/// 받은 카드(직접 받았거나 상태 이벤트에 실린 것). 서명이 맞고 더 새 판이면 둡니다. 내 카드면 합칩니다.
pub(crate) fn accept_card(node: &Arc<Node>, c: &SignedBadgeCard) -> Result<()> {
    c.verify()
        .map_err(|_| Error::invalid("카드 서명이 올바르지 않음"))?;
    let me = node.me()?;
    if c.user == me.user() {
        let dev = developer(node);
        let theirs = c.card.clone();
        let earned = c.earned(dev.as_ref());
        update_card(node, Some(c), |mine| {
            mine.earned |= earned;
            if mine.founder.is_none() && earned & Badge::Founder.mask() != 0 {
                mine.founder = theirs.founder;
            }
            if theirs.version > mine.version {
                mine.shown = theirs.shown;
                mine.version = theirs.version;
            }
        })?;
        return Ok(());
    }
    let newer = load_card(node, &c.user).is_none_or(|old| c.card.version > old.card.version);
    if newer {
        node.store.put_setting(&card_key(&c.user), c)?;
        node.notify
            .publish(Notification::BadgesUpdated { user: c.user });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use yangpa_core::badge::Badge;

    #[test]
    fn thresholds_are_inclusive() {
        use yangpa_core::badge::{SEED_BYTES, SUPPORT_CENTS};
        assert_eq!(Badge::reached(&SUPPORT_CENTS, 299), 0);
        assert_eq!(
            Badge::list(Badge::reached(&SUPPORT_CENTS, 1400)),
            vec![Badge::Support3, Badge::Support14]
        );
        assert_eq!(
            Badge::list(Badge::reached(&SEED_BYTES, 1_000_000_000)),
            vec![Badge::Seed1Gb]
        );
    }
}
