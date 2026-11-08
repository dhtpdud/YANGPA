//! 신뢰 스위치: 이 기기가 IP를 보여도 되는 사람·크루(설정 `trust.users`·`trust.spaces`, 기기마다).
//!
//! 사람을 신뢰하면 그 사람에게, 크루를 신뢰하면 그 크루의 멤버에게 내 IP를 보여도 된다는 뜻입니다. IP는 기기마다
//! 다르므로 동의도 기기마다 둡니다. 두 기기가 서로 신뢰하면 P2P 스웜으로 바로 잇고, 한쪽만 신뢰하면 신뢰하지 않은
//! 쪽이 Tor 출구를 거쳐 신뢰한 쪽의 주소로 겁니다(단방향, 신뢰하지 않은 쪽은 숨음). 연결은 `p2p.rs`가 맡습니다.
//!
//! 내 기기끼리는 같은 사람이라 IP를 가릴 상대가 아니므로 스위치 없이 서로 신뢰합니다.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::warn;
use yangpa_core::ids::{SpaceId, UserId};
use yangpa_core::model::{SpaceKind, SpaceState};

use crate::error::{Error, Result};
use crate::node::Node;

/// 신뢰하는 사용자 목록 설정 키.
const USERS_SETTING: &str = "trust.users";
/// 신뢰하는 크루 목록 설정 키.
const SPACES_SETTING: &str = "trust.spaces";
/// 목록마다 상한(설정 한 줄이 끝없이 커지지 않게).
const MAX_TRUSTED: usize = 4096;

/// 목록 읽고-고쳐-쓰기를 한 번에 하나만(동시에 온 켜기·끄기가 서로를 덮어쓰지 않게).
static WRITE: Mutex<()> = Mutex::new(());

fn write_guard() -> MutexGuard<'static, ()> {
    WRITE.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Default)]
struct Trusted {
    users: BTreeSet<UserId>,
    spaces: BTreeSet<SpaceId>,
}

/// 신뢰 목록 캐시(연결을 고를 때마다 설정을 읽지 않게). `None`이면 아직 읽지 않음.
#[derive(Default)]
pub(crate) struct TrustState {
    cache: parking_lot::Mutex<Option<Trusted>>,
}

fn load(node: &Node) -> Trusted {
    let mut cache = node.trust.cache.lock();
    if let Some(t) = &*cache {
        return t.clone();
    }
    let read = |key: &str| -> Option<Vec<String>> {
        match node.store.get_setting::<Vec<String>>(key) {
            Ok(v) => Some(v.unwrap_or_default()),
            Err(e) => {
                warn!(error = %e, "신뢰 목록을 읽지 못함");
                None
            }
        }
    };
    let (Some(users), Some(spaces)) = (read(USERS_SETTING), read(SPACES_SETTING)) else {
        // 읽지 못하면 아무도 신뢰하지 않는 쪽으로(IP를 드러내지 않게). 캐시하지 않아 다음에 다시 읽습니다.
        return Trusted::default();
    };
    let t = Trusted {
        users: users.iter().filter_map(|s| s.parse().ok()).collect(),
        spaces: spaces.iter().filter_map(|s| s.parse().ok()).collect(),
    };
    *cache = Some(t.clone());
    t
}

fn save(node: &Node, t: &Trusted) -> Result<()> {
    let put = |key: &str, v: Vec<String>| -> Result<()> {
        if v.is_empty() {
            node.store.delete_setting(key)?;
        } else {
            node.store.put_setting(key, &v)?;
        }
        Ok(())
    };
    put(
        USERS_SETTING,
        t.users.iter().map(|u| u.to_string()).collect(),
    )?;
    put(
        SPACES_SETTING,
        t.spaces.iter().map(|s| s.to_string()).collect(),
    )?;
    *node.trust.cache.lock() = Some(t.clone());
    Ok(())
}

/// 신뢰하는 사람이나 크루가 하나라도 있는지(P2P 스웜을 띄워 둘지).
pub(crate) fn any(node: &Node) -> bool {
    let t = load(node);
    !t.users.is_empty() || !t.spaces.is_empty()
}

/// 받아들여진 이 기기에 내 다른 활성 기기가 있는지(내 기기끼리 이을 P2P 스웜을 띄워 둘지). 기기가 하나면 띄우지 않습니다.
pub(crate) fn own_devices(node: &Node) -> bool {
    node.me().is_ok_and(|m| {
        let mine = m.device_id();
        m.devices
            .list
            .devices
            .iter()
            .any(|d| d.device != mine && d.revoked_at.is_none())
    })
}

/// 이 사람의 신뢰 스위치(사람 단위로 켰는지).
pub(crate) fn user_switch(node: &Node, user: &UserId) -> bool {
    load(node).users.contains(user)
}

/// 이 크루의 신뢰 스위치.
pub(crate) fn space_switch(node: &Node, space: &SpaceId) -> bool {
    load(node).spaces.contains(space)
}

/// 크루 신뢰가 그 크루의 이 사람에게 닿는지: 크루이고, 나와 그 사람이 지금 멤버.
fn crew_covers(state: &SpaceState, me: &UserId, user: &UserId) -> bool {
    state.kind == SpaceKind::Crew && state.is_member(me) && state.is_member(user)
}

/// 이 기기가 이 사람에게 IP를 보여도 되는지: 나 자신(내 다른 기기)이거나, 사람을 신뢰했거나, 함께 멤버인 크루 중 신뢰한
/// 크루가 있음.
pub(crate) fn trusts(node: &Node, user: &UserId) -> bool {
    let Ok(me) = node.me() else { return false };
    let me = me.user();
    if *user == me {
        return true;
    }
    let t = load(node);
    t.users.contains(user)
        || t.spaces.iter().any(|s| {
            node.space_state(s)
                .is_some_and(|st| crew_covers(&st, &me, user))
        })
}

/// 이 사람이 멤버인 크루 중 내가 신뢰한 크루(`trust.get`).
pub(crate) fn via_crews(node: &Node, user: &UserId) -> Vec<SpaceId> {
    let Ok(me) = node.me() else {
        return Vec::new();
    };
    let me = me.user();
    if *user == me {
        return Vec::new();
    }
    load(node)
        .spaces
        .into_iter()
        .filter(|s| {
            node.space_state(s)
                .is_some_and(|st| crew_covers(&st, &me, user))
        })
        .collect()
}

/// `trust.set_user`: 사람 단위 신뢰를 켜고 끕니다. 켤 때는 함께 쓰는 공간이 있는(아는) 사람이어야 합니다.
pub(crate) fn set_user(node: &Arc<Node>, user: UserId, enabled: bool) -> Result<()> {
    let me = node.me()?.user();
    if user == me {
        return Err(Error::invalid("나 자신은 신뢰 대상이 아닙니다"));
    }
    let _w = write_guard();
    let mut t = load(node);
    if enabled {
        if t.users.contains(&user) {
            return Ok(());
        }
        let known = node.space_ids().iter().any(|s| {
            node.space_state(s)
                .is_some_and(|st| st.is_member(&me) && st.is_member(&user))
                && !crate::friends::space_deactivated(node, s)
        });
        if !known {
            return Err(Error::not_found(
                "친구·연락처나 같은 크루의 멤버가 아닙니다",
            ));
        }
        if t.users.len() >= MAX_TRUSTED {
            return Err(Error::conflict(format!(
                "신뢰하는 사람은 {MAX_TRUSTED}명까지입니다"
            )));
        }
        t.users.insert(user);
    } else if !t.users.remove(&user) {
        return Ok(());
    }
    save(node, &t)?;
    crate::p2p::reconcile(node);
    Ok(())
}

/// 친구·연락처를 지울 때: 사람 신뢰도 지웁니다(나중에 같은 크루에서 다시 만나도 저절로 IP를 보이지 않게).
pub(crate) fn forget_user(node: &Arc<Node>, user: &UserId) {
    if user_switch(node, user)
        && let Err(e) = set_user(node, *user, false)
    {
        warn!(error = %e, "지운 연락처의 신뢰를 지우지 못함");
    }
}

/// `trust.set_space`: 크루 단위 신뢰를 켜고 끕니다. 켤 때는 내가 멤버인 크루여야 합니다. 바뀌었으면 참.
pub(crate) fn set_space(node: &Arc<Node>, space: SpaceId, enabled: bool) -> Result<bool> {
    let _w = write_guard();
    let mut t = load(node);
    if enabled {
        if t.spaces.contains(&space) {
            return Ok(false);
        }
        let (state, _) = crate::crews::member_state(node, &space)?;
        if state.kind != SpaceKind::Crew {
            return Err(Error::invalid(
                "크루만 신뢰할 수 있습니다(1:1 대화는 상대 프로필에서)",
            ));
        }
        if t.spaces.len() >= MAX_TRUSTED {
            return Err(Error::conflict(format!(
                "신뢰하는 크루는 {MAX_TRUSTED}개까지입니다"
            )));
        }
        t.spaces.insert(space);
    } else if !t.spaces.remove(&space) {
        return Ok(false);
    }
    save(node, &t)?;
    crate::p2p::reconcile(node);
    Ok(true)
}

/// 해소된 상태가 바뀔 때(`crews::on_state_applied`): 내가 나간 크루의 신뢰를 지웁니다(다시 들어오면 다시 켜야 함).
/// 신뢰한 크루의 멤버가 바뀌었으면 연결을 다시 맞춥니다. 맞춰야 하면 참을 돌려줍니다.
pub(crate) fn on_state_applied(
    node: &Node,
    space: &SpaceId,
    old: Option<&SpaceState>,
    new: &SpaceState,
) -> bool {
    if !space_switch(node, space) {
        return false;
    }
    let left = node
        .me()
        .is_ok_and(|m| old.is_some_and(|o| o.is_member(&m.user())) && !new.is_member(&m.user()));
    if left {
        let _w = write_guard();
        let mut t = load(node);
        if t.spaces.remove(space)
            && let Err(e) = save(node, &t)
        {
            warn!(error = %e, "나간 크루의 신뢰를 지우지 못함");
        }
    }
    true
}
