// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
//! 후원자 표시(decisions.md "후원자 카드 배너와 후광"): 월 14달러 이상 정기 후원자는 프로필 카드 배경에 배너를, 50달러
//! 이상은 프로필 사진 뒤에 두 색 그라데이션 후광을 보입니다. GitHub 1회 후원도 같은 금액 이상이면 결제 뒤 30일 동안
//! 같고, 낸 표시의 기한도 그날을 넘기지 않습니다.
//!
//! - 내 표시는 상태 이벤트 `MemberFlair`로 내가 멤버인 모든 공간(1:1 친구 공간 포함)에 냅니다. 프로필 이벤트처럼 직접
//!   연결되지 않은 크루 멤버와 내 다른 기기도 동기화로 받습니다. 공간 상태에는 넣지 않습니다(빌드마다 상태 해시가 같게).
//! - 후원 단계는 후원 계정을 연결해 직접 확인한 기기만, 단계(배너·후광)가 바뀔 때와 낸 기한이 3일 안으로 다가올 때만
//!   정합니다. 3만 명 크루에도 퍼지므로 매일 확인마다 내지 않습니다. 금액은 싣지 않습니다.
//! - 후광 색은 고른 기기가 냅니다. 받는 쪽은 사용자마다 두 부분을 따로, `at`이 더 늦은 것으로 모읍니다.
//! - 낸 뒤에 생긴 공간(새 크루·새 친구, 혼자였던 크루에 멤버가 든 때)에는 그 표시가 없습니다. 공간마다 실린 표시를
//!   기억해(`flair.sent.<공간>`) 빠진 공간에만 내 기기가 모은 표시를 `at`·`until` 그대로 다시 냅니다. 연결한 기기가
//!   꺼져 있어도 내 화면에 후광을 보이는 기기가 나르게, 계정을 연결하지 않은 내 기기도 냅니다.
//! - 체크포인트 경계 아래로 묻힌 표시는 새 멤버에게 가지 않고, 표시는 신원 키 서명이 없어 남이 대신 전할 수 없습니다.
//!   실린 이벤트의 논리 시계와 그때 멤버 수도 기억해, 묻혔고 멤버 수가 달라졌으면 내 기기가 다시 냅니다.
//! - 받은 표시는 해소기가 적용한 것(작성자가 멤버, 작성 기기가 그 사용자의 것)만 `flair.user.<사용자>`에 모으고, 기한은
//!   받은 때부터 40일로 자릅니다.
//! - 서버가 없어 남의 후원 단계는 그 사람 기기의 말을 믿습니다(고친 빌드는 꾸밀 수 있음).
//!
//! shortcut: 새 멤버를 멤버 수로 어림합니다. 같은 수로 들고 나면 놓치고 나가기만 해도 한 번 더 내며, 내 기기가 모두 꺼져
//! 있으면 켜질 때 닿습니다. 켜진 기기 없이도 닿게 하려면 표시에 신원 키 서명을 더해 멤버가 대신 전하게 합니다.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::body::EventBody;
use yangpa_core::ids::{SpaceId, UserId};
use yangpa_core::model::{Flair, FlairHalo, FlairTier};
use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{FlairSetParams, FlairView, Notification};

use crate::author::{self, KeyChoice};
use crate::node::Node;

const PREFIX: &str = "flair.user.";
/// 이 기기가 마지막으로 낸 후원 단계.
const PUBLISHED_KEY: &str = "flair.published";
/// 공간에 마지막으로 실린 내 표시(`flair.sent.<공간>`).
const SENT_PREFIX: &str = "flair.sent.";
const BANNER_CENTS: u32 = 1400;
const HALO_CENTS: u32 = 5000;
/// 후광 색을 고르지 않았을 때(앱의 불꽃 색).
const DEFAULT_HALO: [u32; 2] = [0xE8541E, 0xFFC24B];
/// 낸 단계를 믿는 기간. 후원이 끝나면 확인한 기기가 바로 내리므로, 그 기기가 아주 사라졌을 때만 이만큼 남습니다.
const TRUST: Duration = Duration::from_secs(31 * 24 * 3600);
const REPUBLISH_BEFORE: Duration = Duration::from_secs(3 * 24 * 3600);
const MAX_AHEAD: Duration = Duration::from_secs(40 * 24 * 3600);

/// 사용자 한 명의 모은 표시(DB 설정).
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    user: UserId,
    #[serde(default)]
    tier: Option<FlairTier>,
    #[serde(default)]
    halo: Option<FlairHalo>,
}

/// 표시 두 부분의 `at`(단계, 후광 색).
type Stamps = (Option<Timestamp>, Option<Timestamp>);

/// 공간에 마지막으로 실린 내 표시.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Sent {
    stamps: Stamps,
    /// 그 이벤트의 논리 시계와 그때 멤버 수(경계 아래로 묻힌 뒤 멤버가 바뀌었는지 보려고).
    lamport: u64,
    members: usize,
}

fn stamps(tier: Option<FlairTier>, halo: Option<FlairHalo>) -> Stamps {
    (tier.map(|t| t.at), halo.map(|h| h.at))
}

fn key(user: &UserId) -> String {
    format!("{PREFIX}{user}")
}

fn sent_key(space: &SpaceId) -> String {
    format!("{SENT_PREFIX}{space}")
}

fn load(node: &Node, user: &UserId) -> Option<Entry> {
    node.store.get_setting(&key(user)).ok().flatten()
}

/// 부분마다 더 늦게 정한 것을 씁니다. 바뀌었으면 참.
fn merge(e: &mut Entry, f: &Flair, now: Timestamp) -> bool {
    let mut changed = false;
    if let Some(mut t) = f.tier
        && e.tier.is_none_or(|old| t.at > old.at)
    {
        t.until = t.until.min(now.saturating_add(MAX_AHEAD));
        e.tier = Some(t);
        changed = true;
    }
    if let Some(h) = f.halo
        && e.halo.is_none_or(|old| h.at > old.at)
    {
        e.halo = Some(h);
        changed = true;
    }
    changed
}

fn view(e: &Entry, now: Timestamp) -> FlairView {
    let tier = e.tier.filter(|t| now < t.until);
    let halo_allowed = tier.is_some_and(|t| t.halo);
    FlairView {
        user: e.user,
        banner: tier.is_some_and(|t| t.banner),
        halo_allowed,
        halo: if halo_allowed {
            e.halo.map_or(Some(DEFAULT_HALO), |h| h.colors)
        } else {
            None
        },
        until: tier.map_or(now, |t| t.until),
    }
}

/// 받은(또는 낸) 표시를 모으고, 바뀌었으면 앱에 알립니다.
fn absorb(node: &Node, user: UserId, f: &Flair) {
    if !f.is_valid() {
        return;
    }
    let now = Timestamp::now();
    let mut e = load(node, &user).unwrap_or(Entry {
        user,
        tier: None,
        halo: None,
    });
    if !merge(&mut e, f, now) {
        return;
    }
    if let Err(err) = node.store.put_setting(&key(&user), &e) {
        debug!(error = %err, "후원자 표시 저장 실패");
        return;
    }
    node.notify.publish(Notification::Flair(view(&e, now)));
}

/// 해소기가 적용한 `MemberFlair`(`crews::on_resolved`: 작성자가 멤버이고 작성 기기가 그 사용자의 것).
pub(crate) fn on_applied(node: &Node, space: SpaceId, user: UserId, lamport: u64, f: &Flair) {
    absorb(node, user, f);
    // 내 표시 전체가 이 공간에 실렸습니다(내 다른 기기가 낸 것이면 다시 내지 않게).
    let sent = Sent {
        stamps: stamps(f.tier, f.halo),
        lamport,
        members: node.space_state(&space).map_or(0, |st| st.members.len()),
    };
    if node.me().is_ok_and(|me| me.user() == user)
        && load(node, &user).is_some_and(|e| stamps(e.tier, e.halo) == sent.stamps)
        && let Err(e) = node.store.put_setting(&sent_key(&space), &sent)
    {
        debug!(error = %e, "후원자 표시 기록 실패");
    }
}

/// 공간 상태가 바뀐 뒤(적용 잠금 안, `crews::on_resolved`): 새 공간·새 멤버에게도 내 표시가 닿게 합니다.
pub(crate) fn on_state_changed(node: &Arc<Node>, space: SpaceId) {
    if due_in(node, &space).is_some() {
        node.spawn_detached(move |node| async move { spread(&node, vec![space]).await });
    }
}

/// 이 기기에서 공간을 지울 때(`crews::purge_local`).
pub(crate) fn forget(node: &Node, space: &SpaceId) {
    let _ = node.store.delete_setting(&sent_key(space));
}

/// 시험용(`testkit::hold_flair`): 공간에 내지 않고 내 후광 단계만 모읍니다(표시가 빠진 공간이 이미 있는 기기처럼).
pub(crate) fn hold_for_test(node: &Node) {
    let Ok(me) = node.me() else { return };
    let now = Timestamp::now();
    let tier = FlairTier {
        banner: true,
        halo: true,
        until: now.saturating_add(TRUST),
        at: now,
    };
    absorb(
        node,
        me.user(),
        &Flair {
            tier: Some(tier),
            halo: None,
        },
    );
}

/// `flair.list`: 지금 무언가 보이는 사용자만(크루가 커도 짧게).
pub(crate) fn list(node: &Node) -> Vec<FlairView> {
    let now = Timestamp::now();
    node.store
        .settings_with_prefix(PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<Entry>(v).ok())
        .map(|e| view(&e, now))
        .filter(|v| v.banner || v.halo_allowed)
        .collect()
}

/// `flair.set`: 후광 색(없으면 끔). 지금 후광을 쓸 수 있을 때만.
pub(crate) fn set(node: &Arc<Node>, p: FlairSetParams) -> Result<FlairView, RpcError> {
    let me = crate::api::me(node)?.user();
    if p.halo.is_some_and(|c| c.iter().any(|&x| x > 0xFF_FFFF)) {
        return Err(RpcError::invalid_input("색은 0xRRGGBB여야 합니다"));
    }
    let now = Timestamp::now();
    if !load(node, &me).is_some_and(|e| view(&e, now).halo_allowed) {
        return Err(RpcError::forbidden(
            "후광은 월 50달러 이상 정기 후원자와 GitHub에서 50달러 이상 1회 후원한 사람(30일 동안)만 쓸 수 있습니다",
        ));
    }
    publish(
        node,
        Flair {
            tier: None,
            halo: Some(FlairHalo {
                colors: p.halo,
                at: now,
            }),
        },
    );
    // 바로 위 `publish`가 모았으므로 있습니다(저장이 실패했으면 빈 표시).
    Ok(load(node, &me).map_or_else(
        || FlairView {
            user: me,
            banner: false,
            halo_allowed: false,
            halo: None,
            until: now,
        },
        |e| view(&e, now),
    ))
}

/// 이 기기가 확인한 후원 단계를 필요할 때만 냅니다(`supporter::check_due` 끝, 계정 연결·해제 뒤).
pub(crate) fn refresh(node: &Arc<Node>) {
    let last: Option<FlairTier> = node.store.get_setting(PUBLISHED_KEY).ok().flatten();
    // 연결을 해제했으면 전에 낸 단계를 내립니다. 연결한 적 없는 기기는 내지 않습니다.
    let Some((cents, paid_until)) = crate::supporter::own_support(node).or(last.map(|_| (0, None)))
    else {
        return;
    };
    let Some(tier) = next_tier(last, cents, paid_until, Timestamp::now()) else {
        return;
    };
    if let Err(e) = node.store.put_setting(PUBLISHED_KEY, &tier) {
        debug!(error = %e, "후원자 표시 기록 실패");
        return;
    }
    publish(
        node,
        Flair {
            tier: Some(tier),
            halo: None,
        },
    );
}

/// 낼 단계(낼 필요가 없으면 `None`). 1회 후원은 낸 기간이 끝나는 때(`paid_until`)를 넘겨 믿게 하지 않습니다(그때 이
/// 기기가 꺼져 있어도 남에게서 내려가게). 기한이 다가와도 새 기한이 더 늦을 때만 다시 냅니다(끝이 정해진 1회 후원을
/// 매 확인마다 다시 내지 않게).
fn next_tier(
    last: Option<FlairTier>,
    cents: u32,
    paid_until: Option<Timestamp>,
    now: Timestamp,
) -> Option<FlairTier> {
    let banner = cents >= BANNER_CENTS;
    let halo = cents >= HALO_CENTS;
    let until = if banner || halo {
        let trust = now.saturating_add(TRUST);
        paid_until.map_or(trust, |end| end.min(trust))
    } else {
        now
    };
    let due = match last {
        // 후원자가 아니고 낸 적도 없으면 아무것도 내지 않습니다.
        None => banner || halo,
        Some(l) => {
            l.banner != banner
                || l.halo != halo
                || ((banner || halo)
                    && until > l.until
                    && l.until.as_millis().saturating_sub(now.as_millis())
                        < REPUBLISH_BEFORE.as_millis() as u64)
        }
    };
    due.then_some(FlairTier {
        banner,
        halo,
        until,
        at: now,
    })
}

/// 내 표시를 바로 모으고(내 화면), 내 공간마다 냅니다.
fn publish(node: &Arc<Node>, f: Flair) {
    let Ok(me) = node.me() else { return };
    absorb(node, me.user(), &f);
    node.spawn_detached(|node| async move {
        let spaces = node.space_ids();
        spread(&node, spaces).await;
    });
}

/// 보이는 표시는 빠진 공간에 내고, 안 보이게 된 것(내린 단계)은 낸 적이 있는 공간에만 냅니다. 보이는 표시가 경계 아래로
/// 묻혔으면 그 뒤로 멤버가 바뀌었을 때만 다시 냅니다(체크포인트가 날마다 나와도 그때마다 내지 않게).
fn due(last: Option<Sent>, now: Stamps, shown: bool, floor: u64, members: usize) -> bool {
    last.map_or(shown, |l| {
        l.stamps != now || (shown && l.lamport < floor && l.members != members)
    })
}

/// 이 공간에 지금 내야 할 내 표시(`identity::push_profile`과 같은 공간, 1:1 친구 공간도). 후원자가 아니면 설정 한 번만 읽습니다.
fn due_in(node: &Node, space: &SpaceId) -> Option<Flair> {
    let me = node.me().ok()?;
    let e = load(node, &me.user())?;
    let st = node.space_state(space)?;
    if st.members.len() < 2
        || !st.is_member(&me.user())
        || !st.device_belongs(&me.user(), &me.device_id())
    {
        return None;
    }
    let last = node.store.get_setting(&sent_key(space)).ok().flatten();
    let v = view(&e, Timestamp::now());
    let floor = crate::checkpoint::floor(node, space);
    due(
        last,
        stamps(e.tier, e.halo),
        v.banner || v.halo_allowed,
        floor,
        st.members.len(),
    )
    .then_some(Flair {
        tier: e.tier,
        halo: e.halo,
    })
}

/// 내 표시가 빠진 공간에 모은 표시를 그대로 냅니다(`at`을 새로 찍지 않아 내 다른 기기가 정한 더 새 값을 이기지 않음). 낸
/// 표시는 적용될 때 [`on_applied`]가 적어 둡니다. 내지 못한 공간은 다음 상태 변화나 `supporter::check_due`에서 다시 봅니다.
pub(crate) async fn spread(node: &Arc<Node>, spaces: Vec<SpaceId>) {
    // 한 줄로: 상태가 잇달아 바뀌어도 같은 공간에 겹쳐 내지 않게.
    let _one = node.supporter.spreading.lock().await;
    for space in spaces {
        let Some(f) = due_in(node, &space) else {
            continue;
        };
        if crate::policy::is_dormant(node, &space) {
            continue;
        }
        let body = EventBody::MemberFlair(f);
        if let Err(e) = author::publish(node, space, None, &body, KeyChoice::State).await {
            debug!(space = %space.short(), error = %e, "후원자 표시 발행 실패");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = Timestamp(1_800_000_000_000);
    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn tier(banner: bool, halo: bool, at: u64) -> FlairTier {
        FlairTier {
            banner,
            halo,
            until: NOW.saturating_add(DAY),
            at: Timestamp(at),
        }
    }

    #[test]
    fn parts_merge_by_their_own_time() {
        let user = UserId([1; 32]);
        let mut e = Entry {
            user,
            tier: None,
            halo: None,
        };
        // 확인한 기기의 단계와 다른 기기가 고른 색이 서로를 지우지 않습니다.
        assert!(merge(
            &mut e,
            &Flair {
                tier: Some(tier(true, true, 5)),
                halo: None
            },
            NOW
        ));
        let colors = Some([0x112233, 0x445566]);
        let halo = FlairHalo {
            colors,
            at: Timestamp(3),
        };
        assert!(merge(
            &mut e,
            &Flair {
                tier: None,
                halo: Some(halo)
            },
            NOW
        ));
        assert_eq!(view(&e, NOW).halo, colors);
        // 더 이른 단계는 무시합니다.
        assert!(!merge(
            &mut e,
            &Flair {
                tier: Some(tier(false, false, 4)),
                halo: None
            },
            NOW
        ));
        let v = view(&e, NOW);
        assert!(v.banner && v.halo_allowed);
        // 기한이 지나면 둘 다 꺼집니다.
        let late = view(&e, NOW.saturating_add(DAY));
        assert!(!late.banner && !late.halo_allowed && late.halo.is_none());
    }

    #[test]
    fn banner_only_tier_has_no_halo_and_far_until_is_capped() {
        let user = UserId([2; 32]);
        let mut e = Entry {
            user,
            tier: None,
            halo: Some(FlairHalo {
                colors: Some([1, 2]),
                at: Timestamp(1),
            }),
        };
        let mut t = tier(true, false, 1);
        t.until = Timestamp(u64::MAX);
        merge(
            &mut e,
            &Flair {
                tier: Some(t),
                halo: None,
            },
            NOW,
        );
        let v = view(&e, NOW);
        assert!(v.banner && !v.halo_allowed && v.halo.is_none());
        assert_eq!(v.until, NOW.saturating_add(MAX_AHEAD));
        // 색을 고르지 않은 후광은 기본 색, 끈 후광은 없음.
        let mut e = Entry {
            user,
            tier: Some(tier(true, true, 1)),
            halo: None,
        };
        assert_eq!(view(&e, NOW).halo, Some(DEFAULT_HALO));
        e.halo = Some(FlairHalo {
            colors: None,
            at: Timestamp(2),
        });
        assert!(view(&e, NOW).halo_allowed && view(&e, NOW).halo.is_none());
    }

    #[test]
    fn only_spaces_missing_the_flair_are_due() {
        let now = (Some(Timestamp(2)), None);
        let sent = |stamps| {
            Some(Sent {
                stamps,
                lamport: 10,
                members: 3,
            })
        };
        // 낸 적 없는 공간에는 보이는 표시만 냅니다.
        assert!(due(None, now, true, 0, 3));
        assert!(!due(None, now, false, 0, 3));
        // 이미 실린 공간에는 다시 내지 않습니다.
        assert!(!due(sent(now), now, true, 0, 3));
        // 바뀌면 내고, 낸 적이 있는 공간에는 내린 단계도 냅니다.
        assert!(due(sent((Some(Timestamp(1)), None)), now, true, 0, 3));
        assert!(due(sent((Some(Timestamp(1)), None)), now, false, 0, 3));
    }

    #[test]
    fn flair_below_the_floor_goes_out_again_only_for_new_members() {
        let now = (Some(Timestamp(2)), None);
        let last = Some(Sent {
            stamps: now,
            lamport: 10,
            members: 3,
        });
        // 경계 아래로 묻혔고 멤버가 바뀌면 다시 냅니다.
        assert!(due(last, now, true, 11, 4));
        // 묻혔어도 멤버가 그대로면(체크포인트만 나온 때) 내지 않습니다.
        assert!(!due(last, now, true, 11, 3));
        // 아직 경계 위면 새 멤버가 꼬리로 받습니다.
        assert!(!due(last, now, true, 10, 4));
        // 안 보이는 표시는 새 멤버에게 다시 내지 않습니다.
        assert!(!due(last, now, false, 11, 4));
    }

    #[test]
    fn one_time_tier_ends_with_payment() {
        let end = NOW.saturating_add(DAY * 30);
        // 1회 50달러: 처음 낼 때 기한은 30일이 끝나는 때(31일보다 이름).
        let first = next_tier(None, 5000, Some(end), NOW).unwrap();
        assert!(first.banner && first.halo);
        assert_eq!(first.until, end);
        // 끝이 다가와도 새 기한이 늘지 않으면 다시 내지 않습니다(매시간 확인마다 내지 않게).
        let near = NOW.saturating_add(DAY * 28);
        assert!(next_tier(Some(first), 5000, Some(end), near).is_none());
        // 다시 결제해 끝이 늘면 다시 냅니다.
        let later = end.saturating_add(DAY * 28);
        assert_eq!(
            next_tier(Some(first), 5000, Some(later), near).map(|t| t.until),
            Some(later)
        );
        // 끝나면(금액 0) 내립니다. 정기 후원은 31일 기한.
        let off = next_tier(Some(first), 0, None, end).unwrap();
        assert!(!off.banner && !off.halo);
        assert_eq!(
            next_tier(None, 1400, None, NOW).map(|t| (t.banner, t.halo, t.until)),
            Some((true, false, NOW.saturating_add(TRUST)))
        );
        // 1회 3달러는 표시 없음.
        assert!(next_tier(None, 300, Some(end), NOW).is_none());
    }
}
