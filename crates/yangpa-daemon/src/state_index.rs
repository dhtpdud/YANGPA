//! 상태 색인: 상태 사본(`Arc<SpaceState>`)마다 한 번만 계산하는 파생값. 전파(`sync_service::propagate`), gossip·직접
//! 전파 받기의 멤버 기기 확인, 시더 확인, 작성 기기별 속도 제한이 씁니다.
//!
//! - 활성 기기 수·목록(멤버 순), 기기 → 사용자 표(활성 멤버 기기만)
//! - 채널마다 모두 볼 수 있는지, 볼 수 있는 기기(목록 또는 기기 자리마다 비트). 볼 수 있는지는 채널의 덮어쓰기(권한
//!   규칙)에만 달려 있어, 규칙이 같은 채널끼리는 한 번만 계산합니다(채널 수백 개가 대개 규칙 몇 개).
//!
//! 3만 명 크루에서 채널 하나의 '누가 볼 수 있나'는 멤버 수만큼 권한 계산(5ms 남짓), 기기 목록은 4.5만 대 모으기라 보낼
//! 때마다 다시 하지 않습니다. 둘 다 멤버 표와 기기 표를 나란히 걷고(`SpaceState::members_with_devices`) 쥔 항목으로
//! 권한을 계산합니다(사용자마다 B-트리를 다시 찾으면 세 배 넘게 느림). 값은 처음 물을 때 계산합니다(게으름).
//!
//! 색인은 계산한 사본을 강한 참조로 들고, 찾을 때 묻는 쪽 사본과 포인터가 같은지 봅니다. 색인이 사본을 붙잡고 있는
//! 동안 그 할당은 다른 상태에 다시 쓰이지 않으므로 주소가 같으면 같은 사본입니다. 그래서 묻는 쪽이 쥔 사본과 다른
//! 사본의 값(예: 권한을 막기 전의 '모두 볼 수 있음')이 나올 수 없고, 상태 적용과 엇갈려도 묻는 쪽은 자기 사본에 맞는
//! 값을 받습니다. 상태는 바뀌면 새 `Arc`로 갈아 끼우므로(제자리에서 고치지 않음, `ingest::apply_state`) 새 사본은 늘
//! 새 색인을 받습니다. 기기 → 사용자 표는 바로 앞 색인의 표에서 바뀐 멤버만 고쳐(지속 맵이라 복제가 O(1)) 상태가
//! 자주 바뀌는 따라잡기 중에도 4.5만 항목을 매번 다시 짓지 않습니다.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{PermissionOverwrite, Permissions, SharedMap, SpaceState};

/// 멤버의 활성 기기 목록([`member_devices`] 순서).
pub(crate) type Devices = Arc<[(UserId, DeviceId)]>;

/// 채널 하나를 누가 볼 수 있는지(한 상태 사본 기준).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelView {
    /// 멤버 전원이 볼 수 있음(공간 gossip 토픽으로 보내도 됨).
    pub(crate) everyone: bool,
    /// 볼 수 있는 멤버의 활성 기기 수(이 기기 포함).
    pub(crate) viewer_devices: usize,
    /// 볼 수 있는 기기가 `few_limit` 이하일 때만 그 목록(직접 보낼 곳). 많으면 담지 않아 메모리가 크루 크기에 묶이지
    /// 않습니다.
    pub(crate) few: Option<Devices>,
    /// 모두는 아니고 `few`에 담기에는 많을 때: 같은 사본의 [`StateIndex::devices`] 자리마다 볼 수 있는지(비트). 4.5만
    /// 대에서 6KB 남짓입니다(기기 목록을 채널마다 따로 담으면 수 MB).
    pub(crate) mask: Option<Arc<[u64]>>,
}

impl ChannelView {
    /// [`StateIndex::devices`]의 `i`번째 기기가 볼 수 있는지(`mask`가 없으면 거짓).
    pub(crate) fn mask_allows(&self, i: usize) -> bool {
        self.mask
            .as_ref()
            .and_then(|m| m.get(i / 64))
            .is_some_and(|w| w >> (i % 64) & 1 == 1)
    }
}

/// 볼 수 있는지를 정하는 채널의 권한 규칙(없는 채널은 `None`: 아무도 못 봄). 목록 상한도 열쇠에 넣습니다.
type RuleKey = (Option<Vec<PermissionOverwrite>>, usize);

/// 한 상태 사본의 색인.
pub(crate) struct StateIndex {
    /// 이 값들을 계산한 사본(강한 참조: 주소가 다른 상태에 다시 쓰이지 않게).
    state: Arc<SpaceState>,
    /// 기기 → 사용자 표를 이어서 고칠 바로 앞 색인(표를 지으면 놓음).
    prev: Mutex<Option<Arc<StateIndex>>>,
    count: OnceLock<usize>,
    devices: OnceLock<Devices>,
    device_user: OnceLock<SharedMap<DeviceId, UserId>>,
    rules: Mutex<HashMap<RuleKey, Arc<ChannelView>>>,
    channels: Mutex<HashMap<(ChannelId, usize), Arc<ChannelView>>>,
}

impl StateIndex {
    fn new(state: &Arc<SpaceState>, prev: Option<Arc<StateIndex>>) -> Self {
        // 앞 색인이 표를 아직 짓지 않았으면 이을 것이 없습니다(앞 색인의 앞 색인까지 사본을 붙잡지 않게).
        let prev = prev.filter(|p| p.device_user.get().is_some());
        StateIndex {
            state: state.clone(),
            prev: Mutex::new(prev),
            count: OnceLock::new(),
            devices: OnceLock::new(),
            device_user: OnceLock::new(),
            rules: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn is_for(&self, state: &Arc<SpaceState>) -> bool {
        Arc::ptr_eq(&self.state, state)
    }

    /// 멤버의 활성 기기 수(이 기기 포함).
    pub(crate) fn active_count(&self) -> usize {
        *self.count.get_or_init(|| match self.devices.get() {
            Some(d) => d.len(),
            None => member_devices(&self.state).count(),
        })
    }

    /// 멤버의 활성 기기 목록(이 기기 포함, [`member_devices`] 순서). [`ChannelView::mask`]의 자리도 이 순서입니다.
    pub(crate) fn devices(&self) -> Devices {
        self.devices
            .get_or_init(|| member_devices(&self.state).collect())
            .clone()
    }

    /// 활성 멤버 기기의 사용자.
    pub(crate) fn device_user(&self, device: &DeviceId) -> Option<UserId> {
        self.device_users().get(device).copied()
    }

    /// 기기가 그 사용자의 활성 멤버 기기인지(`is_member && device_belongs`와 같음).
    pub(crate) fn is_member_device(&self, user: &UserId, device: &DeviceId) -> bool {
        self.device_user(device).as_ref() == Some(user)
    }

    fn device_users(&self) -> &SharedMap<DeviceId, UserId> {
        self.device_user.get_or_init(|| {
            let prev = self.prev.lock().take();
            match prev
                .as_ref()
                .and_then(|p| p.device_user.get().map(|m| (p, m)))
            {
                Some((p, m)) => patch_device_users(m.clone(), &p.state, &self.state),
                None => member_devices(&self.state).map(|(u, d)| (d, u)).collect(),
            }
        })
    }

    /// 채널을 누가 볼 수 있는지. `few_limit`은 목록을 담을 기기 수 상한(노드에서 늘 같은 값).
    pub(crate) fn channel(&self, channel: &ChannelId, few_limit: usize) -> Arc<ChannelView> {
        if let Some(v) = self.channels.lock().get(&(*channel, few_limit)) {
            return v.clone();
        }
        let rule: RuleKey = (
            self.state
                .channels
                .get(channel)
                .map(|c| c.overwrites.clone()),
            few_limit,
        );
        let cached = self.rules.lock().get(&rule).cloned();
        let v = match cached {
            Some(v) => v,
            None => {
                // 멤버 수만큼의 권한 계산은 잠금 밖에서 합니다. 덮어쓰기가 같은 채널이면 결과도 같습니다.
                let v = Arc::new(compute(&self.state, channel, few_limit));
                self.rules.lock().insert(rule, v.clone());
                v
            }
        };
        self.channels
            .lock()
            .insert((*channel, few_limit), v.clone());
        v
    }
}

/// 앞 사본의 기기 → 사용자 표에서 멤버·기기 기록이 바뀐 사용자만 고칩니다(바뀐 만큼만 돎).
fn patch_device_users(
    mut m: SharedMap<DeviceId, UserId>,
    old: &SpaceState,
    new: &SpaceState,
) -> SharedMap<DeviceId, UserId> {
    let users = old.changed_members(new);
    for u in &users {
        for d in old.member_active_devices(u) {
            if m.get(&d.device) == Some(u) {
                m.remove(&d.device);
            }
        }
    }
    for u in &users {
        for d in new.member_active_devices(u) {
            m.insert(d.device, *u);
        }
    }
    m
}

/// 노드의 상태 색인 모음(공간마다 지금 사본의 색인 하나).
#[derive(Default)]
pub(crate) struct StateIndexes {
    spaces: Mutex<HashMap<SpaceId, Arc<StateIndex>>>,
}

impl StateIndexes {
    /// `state` 사본의 색인. 다른 사본의 색인이 있으면 갈아 끼웁니다: 옛 사본으로 묻는 쪽이 새 색인을 밀어내도 다음에
    /// 다시 계산할 뿐 틀린 값이 나오지는 않습니다.
    pub(crate) fn get(&self, space: &SpaceId, state: &Arc<SpaceState>) -> Arc<StateIndex> {
        let mut g = self.spaces.lock();
        if let Some(i) = g.get(space)
            && i.is_for(state)
        {
            return i.clone();
        }
        let prev = g.remove(space);
        let i = Arc::new(StateIndex::new(state, prev));
        g.insert(*space, i.clone());
        i
    }

    /// 공간의 색인을 버립니다(공간을 지웠을 때).
    pub(crate) fn invalidate(&self, space: &SpaceId) {
        self.spaces.lock().remove(space);
    }

    /// 모두 비웁니다(이 기기의 데이터를 지울 때).
    pub(crate) fn clear(&self) {
        self.spaces.lock().clear();
    }
}

/// 멤버의 활성 기기(상자 키 없이 기록만 빌려 봄).
pub(crate) fn member_devices(state: &SpaceState) -> impl Iterator<Item = (UserId, DeviceId)> + '_ {
    state.active_devices().map(|(u, d)| (*u, d.device))
}

/// [`member_devices`]와 같은 순서로 돌며 볼 수 있는 기기를 셉니다. 비트는 기기 자리마다라 볼 수 없는 멤버의 기기도
/// 자리는 셉니다.
fn compute(state: &SpaceState, channel: &ChannelId, few_limit: usize) -> ChannelView {
    let mut everyone = true;
    let mut viewer_devices = 0usize;
    let mut few: Vec<(UserId, DeviceId)> = Vec::new();
    let mut mask: Vec<u64> = Vec::new();
    let mut at = 0usize;
    let ch = state.channels.get(channel);
    for (user, member, list) in state.members_with_devices() {
        let can = state
            .permissions_in(user, Some(member), ch)
            .contains(Permissions::VIEW_CHANNEL);
        everyone &= can;
        let Some(list) = list else {
            continue;
        };
        for d in list.active() {
            if at / 64 == mask.len() {
                mask.push(0);
            }
            if can {
                mask[at / 64] |= 1 << (at % 64);
                viewer_devices += 1;
                if viewer_devices <= few_limit {
                    few.push((*user, d.device));
                }
            }
            at += 1;
        }
    }
    let few = (viewer_devices <= few_limit).then(|| Devices::from(few));
    ChannelView {
        everyone,
        viewer_devices,
        mask: (!everyone && few.is_none()).then(|| mask.into()),
        few,
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::crews::tests_support::empty_state;
    use yangpa_core::device::{DeviceRecord, MemberDevices};
    use yangpa_core::model::{ChannelSettings, Member, OverwriteTarget, PermissionOverwrite};
    use yangpa_core::time::Timestamp;

    /// 기기 하나짜리 기록(키 약속은 시험용 아무 값).
    pub(crate) fn one_device(device: DeviceId) -> Arc<MemberDevices> {
        Arc::new(MemberDevices {
            version: 1,
            devices: vec![DeviceRecord {
                device,
                added_at: Timestamp(0),
                revoked_at: None,
                ed_pk: [0; 32],
                pq_hash: [0; 32],
                key_commit: device.0,
            }],
            otp: None,
        })
    }

    /// 멤버 `n`명(기기 하나씩)과 모두 볼 수 있는 채널 하나. 소유자는 멤버 목록에 없습니다.
    pub(crate) fn crew(n: usize) -> (SpaceState, ChannelId) {
        let mut st = empty_state();
        for _ in 0..n {
            let user = UserId::random();
            st.members.insert(
                user,
                Arc::new(Member {
                    user,
                    roles: Vec::new(),
                    nickname: None,
                    automation: false,
                }),
            );
            st.devices.insert(user, one_device(DeviceId::random()));
        }
        let ch = ChannelId::random();
        st.channels.insert(ch, ChannelSettings::text("일반"));
        (st, ch)
    }

    /// 채널 덮어쓰기 하나를 더합니다.
    pub(crate) fn overwrite(
        st: &mut SpaceState,
        ch: &ChannelId,
        target: OverwriteTarget,
        allow: Permissions,
        deny: Permissions,
    ) {
        st.channels
            .get_mut(ch)
            .expect("채널")
            .overwrites
            .push(PermissionOverwrite {
                target,
                allow,
                deny,
            });
    }

    /// 이 사용자에게만 채널을 막습니다(멤버 덮어쓰기 하나로 '일부만 볼 수 있는 채널'이 됨).
    pub(crate) fn deny_view(st: &mut SpaceState, ch: &ChannelId, user: UserId) {
        overwrite(
            st,
            ch,
            OverwriteTarget::Member(user),
            Permissions::NONE,
            Permissions::VIEW_CHANNEL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use yangpa_core::ids::RoleId;
    use yangpa_core::model::{ChannelSettings, OverwriteTarget};

    #[test]
    fn permission_change_flips_cached_value_at_once() {
        let idx = StateIndexes::default();
        let space = SpaceId::random();
        let (st, ch) = crew(20);
        let s1 = Arc::new(st.clone());
        let v1 = idx.get(&space, &s1).channel(&ch, 8);
        assert!(v1.everyone);
        assert_eq!(v1.viewer_devices, 20);
        assert!(v1.few.is_none(), "8대를 넘으면 목록을 담지 않음");
        assert_eq!(idx.get(&space, &s1).active_count(), 20);

        // 멤버 하나의 덮어쓰기로 막은 새 상태: 새 사본으로 물으면 바로 '모두는 아님'.
        let mut st2 = st.clone();
        let victim = *st2.members.keys().next().unwrap();
        deny_view(&mut st2, &ch, victim);
        let s2 = Arc::new(st2);
        let v2 = idx.get(&space, &s2).channel(&ch, 8);
        assert!(!v2.everyone);
        assert_eq!(v2.viewer_devices, 19);

        // 옛 사본을 쥔 쪽은 자기 사본의 값을 받고(섞이지 않음), 새 사본은 막힌 값을 받습니다.
        assert!(idx.get(&space, &s1).channel(&ch, 8).everyone);
        assert!(!idx.get(&space, &s2).channel(&ch, 8).everyone);

        // 비운 뒤에도 같습니다.
        idx.invalidate(&space);
        assert!(idx.spaces.lock().get(&space).is_none());
        assert!(!idx.get(&space, &s2).channel(&ch, 8).everyone);
        assert!(idx.spaces.lock().get(&space).is_some_and(|e| e.is_for(&s2)));
    }

    #[test]
    fn cached_value_is_reused_for_the_same_snapshot_and_rule() {
        let idx = StateIndexes::default();
        let space = SpaceId::random();
        let (mut st, ch) = crew(3);
        // 덮어쓰기가 같은(없는) 다른 채널은 같은 계산을 나눠 씀.
        let ch2 = ChannelId::random();
        st.channels.insert(ch2, ChannelSettings::text("둘째"));
        let s = Arc::new(st);
        let i = idx.get(&space, &s);
        let a = i.channel(&ch, 8);
        let b = i.channel(&ch, 8);
        assert!(Arc::ptr_eq(&a, &b), "같은 사본이면 다시 계산하지 않음");
        assert!(
            Arc::ptr_eq(&a, &i.channel(&ch2, 8)),
            "규칙이 같으면 한 번만"
        );
        assert_eq!(a.few.as_deref().map(<[_]>::len), Some(3));
        assert!(Arc::ptr_eq(&i, &idx.get(&space, &s)));
    }

    #[test]
    fn different_rules_are_not_shared() {
        let idx = StateIndexes::default();
        let space = SpaceId::random();
        let (mut st, ch) = crew(4);
        let ch2 = ChannelId::random();
        st.channels.insert(ch2, ChannelSettings::text("비밀"));
        let victim = *st.members.keys().next().unwrap();
        deny_view(&mut st, &ch2, victim);
        let s = Arc::new(st);
        let i = idx.get(&space, &s);
        assert!(i.channel(&ch, 8).everyone);
        assert!(!i.channel(&ch2, 8).everyone);
        assert_eq!(i.channel(&ch2, 8).viewer_devices, 3);
        // 없는 채널은 아무도 못 봄.
        assert_eq!(i.channel(&ChannelId::random(), 8).viewer_devices, 0);
    }

    #[test]
    fn dropped_snapshot_never_matches_a_new_one() {
        // 색인이 사본을 붙잡아, 새 사본이 같은 주소를 받아 옛 값을 쓰는 일이 없습니다.
        let idx = StateIndexes::default();
        let space = SpaceId::random();
        let (open, ch) = crew(4);
        let mut closed = open.clone();
        let victim = *closed.members.keys().next().unwrap();
        deny_view(&mut closed, &ch, victim);
        for _ in 0..64 {
            let s1 = Arc::new(open.clone());
            assert!(idx.get(&space, &s1).channel(&ch, 8).everyone);
            drop(s1);
            let s2 = Arc::new(closed.clone());
            assert!(!idx.get(&space, &s2).channel(&ch, 8).everyone);
        }
    }

    #[test]
    fn few_list_only_holds_viewers() {
        let (mut st, ch) = crew(12);
        let mut users: Vec<UserId> = st.members.keys().copied().collect();
        // 모두에게 막고 셋에게만 열어 줍니다.
        overwrite(
            &mut st,
            &ch,
            OverwriteTarget::Role(RoleId::EVERYONE),
            Permissions::NONE,
            Permissions::VIEW_CHANNEL,
        );
        users.truncate(3);
        for u in &users {
            overwrite(
                &mut st,
                &ch,
                OverwriteTarget::Member(*u),
                Permissions::VIEW_CHANNEL,
                Permissions::NONE,
            );
        }
        let v = compute(&st, &ch, 8);
        assert!(!v.everyone);
        assert_eq!(v.viewer_devices, 3);
        let mut got: Vec<UserId> = v.few.unwrap().iter().map(|(u, _)| *u).collect();
        got.sort();
        users.sort();
        assert_eq!(got, users);
        // 상한보다 많으면 목록 대신 기기 자리마다의 비트만 둡니다. 비트가 선 자리는 정확히 볼 수 있는 기기입니다.
        let v = compute(&st, &ch, 2);
        assert_eq!((v.viewer_devices, &v.few), (3, &None));
        let all: Vec<(UserId, DeviceId)> = member_devices(&st).collect();
        let mut marked: Vec<UserId> = (0..all.len())
            .filter(|i| v.mask_allows(*i))
            .map(|i| all[i].0)
            .collect();
        marked.sort();
        assert_eq!(marked, users);
        assert!(!v.mask_allows(all.len() + 64), "목록 밖 자리는 거짓");
    }

    #[test]
    fn mask_is_only_kept_for_many_viewers_of_a_restricted_channel() {
        // 모두 볼 수 있는 채널은 목록이 넘쳐도 비트를 담지 않습니다(전파가 쓰지 않음).
        let (mut st, ch) = crew(70);
        let v = compute(&st, &ch, 8);
        assert!(v.everyone && v.few.is_none() && v.mask.is_none());
        // 한 명만 막으면 69대가 볼 수 있고, 64대를 넘는 자리도 비트로 셉니다.
        let all: Vec<(UserId, DeviceId)> = member_devices(&st).collect();
        let victim = all[66].0;
        deny_view(&mut st, &ch, victim);
        let v = compute(&st, &ch, 8);
        assert!(!v.everyone && v.few.is_none());
        assert_eq!(v.viewer_devices, 69);
        assert_eq!(v.mask.as_deref().map(<[u64]>::len), Some(2));
        for (i, (u, _)) in all.iter().enumerate() {
            assert_eq!(v.mask_allows(i), *u != victim, "{i}번째 자리");
        }
    }

    #[test]
    fn device_list_follows_the_snapshot() {
        let idx = StateIndexes::default();
        let space = SpaceId::random();
        let (st, _) = crew(5);
        let s1 = Arc::new(st.clone());
        let d1 = idx.get(&space, &s1).devices();
        assert_eq!(d1.len(), 5);
        assert!(
            Arc::ptr_eq(&d1, &idx.get(&space, &s1).devices()),
            "같은 사본이면 다시 모으지 않음"
        );
        assert_eq!(idx.get(&space, &s1).active_count(), 5);
        // 멤버가 하나 빠진 새 사본은 새 목록.
        let mut st2 = st.clone();
        let gone = *st2.members.keys().next().unwrap();
        st2.members.remove(&gone);
        let s2 = Arc::new(st2);
        let d2 = idx.get(&space, &s2).devices();
        assert_eq!(d2.len(), 4);
        assert!(d2.iter().all(|(u, _)| *u != gone));
        assert_eq!(idx.get(&space, &s2).active_count(), 4);
    }

    /// 기기 → 사용자 표는 앞 색인에서 바뀐 멤버만 고쳐도 처음부터 지은 것과 같습니다.
    #[test]
    fn device_user_map_is_patched_from_the_previous_snapshot() {
        let idx = StateIndexes::default();
        let space = SpaceId::random();
        let (st, _) = crew(50);
        let s1 = Arc::new(st.clone());
        let (u0, d0) = member_devices(&st).next().unwrap();
        assert!(idx.get(&space, &s1).is_member_device(&u0, &d0));
        // 멤버 하나는 떠나고, 하나는 기기를 바꾸고, 새 멤버가 들어옴.
        let mut st2 = st.clone();
        st2.members.remove(&u0);
        let (u1, d1) = member_devices(&st).nth(7).unwrap();
        let d1b = DeviceId::random();
        st2.devices.insert(u1, one_device(d1b));
        let newbie = UserId::random();
        let dn = DeviceId::random();
        st2.members.insert(
            newbie,
            Arc::new(yangpa_core::model::Member {
                user: newbie,
                roles: vec![],
                nickname: None,
                automation: false,
            }),
        );
        st2.devices.insert(newbie, one_device(dn));
        let s2 = Arc::new(st2.clone());
        let i2 = idx.get(&space, &s2);
        assert!(!i2.is_member_device(&u0, &d0), "떠난 멤버의 기기");
        assert!(!i2.is_member_device(&u1, &d1), "바꾼 기기");
        assert!(i2.is_member_device(&u1, &d1b));
        assert!(i2.is_member_device(&newbie, &dn));
        assert_eq!(i2.device_user(&dn), Some(newbie));
        let fresh: SharedMap<DeviceId, UserId> =
            member_devices(&st2).map(|(u, d)| (d, u)).collect();
        assert_eq!(*i2.device_users(), fresh);
        assert!(i2.prev.lock().is_none(), "지은 뒤에는 앞 색인을 놓음");
    }
}
