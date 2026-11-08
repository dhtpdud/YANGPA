//! 확정 기기 쪽 키 그룹 표([`KeyTree`]): 크루 그룹과 비공개 그룹(자기 트리).

use std::collections::{BTreeSet, HashMap, HashSet};

use rand::RngCore;
use yangpa_core::body::Secret32;
use yangpa_core::ids::{DeviceId, EventId, SpaceId, UserId};
use yangpa_core::keyupdate::{
    CrewRef, DIRECT_BACK, DIRECT_MAX, KeyGroupId, KeyUpdateKind, KeyUpdatePayload, LayoutOp,
    MAX_DEVICES, NodeIdx, RCt, SealedBox, Wrap,
};

#[cfg(test)]
use super::WrapSite;
use super::tree::{Tree, depth_for};
use super::{
    Bundle, CommitCtx, DeviceBundle, GroupKind, KeyUpdateError, KeyUpdateOutput, Node, Result,
    WrapCx, WrapKind, advance_node, bundle_ad, bundle_tree, direct_ad, leaf_key, private_leaf,
    recipients,
};
use crate::keysched;
use crate::sealed::seal_to;

#[derive(Clone)]
pub(crate) enum Enc {
    /// 직접 인코딩: 기기 목록만.
    Direct(BTreeSet<DeviceId>),
    Tree(Box<Tree>),
}

/// 확정 기기가 가진 키 그룹 하나의 전체 상태(모든 노드 키). 확정 기기 밖으로는 [`super::seal_handover`]로만 나갑니다.
///
/// 작업은 상태를 제자리에서 바꿉니다. 발행한 갱신이 받아들여질 때까지 작업 전 복제본(`clone`)을 남겨 두었다가 진
/// 갈래·발행 실패면 그것으로 돌아갑니다(키는 `Secret32`라 복제본도 버릴 때 지워짐).
#[derive(Clone)]
pub struct KeyTree {
    pub(crate) space: SpaceId,
    pub(crate) group: KeyGroupId,
    pub(crate) kind: GroupKind,
    pub(crate) seq: u64,
    pub(crate) root_commit: [u8; 32],
    pub(crate) last_event: Option<EventId>,
    /// 크루: leaf_master 세대. 비공개: 잎을 만든 크루 세대.
    pub(crate) generation: u64,
    pub(crate) t: Secret32,
    pub(crate) r: Secret32,
    pub(crate) enc: Enc,
    /// 넘겨받지 못하고 공개 정보로 이어받은 상태(키를 모름): 다음 작업은 전체 재키여야 합니다. 값은 마지막이 트리였는지.
    pub(crate) recovering: Option<bool>,
    /// 크루만: 이 기기 말고 표(leaf_master·노드 키·모든 그룹의 T·R)를 가진 기기. 크루 전체 재키가 비웁니다.
    pub(crate) holders: BTreeSet<DeviceId>,
    /// 크루만: 전체 재키를 할 때마다 오르는 판. 비공개·덮개 그룹은 이 값이 바뀌면 새 T를 냅니다.
    pub(crate) rekey_epoch: u64,
    /// 비공개만: 마지막으로 새 T를 냈을 때의 크루 재키 판.
    pub(crate) crew_epoch: u64,
    /// 시험용: 마지막 갱신의 감싸기 자리(감싸기와 같은 순서).
    #[cfg(test)]
    pub(crate) last_sites: Vec<WrapSite>,
}

/// 갱신 사슬의 공개 끝. 넘겨받지 못한 새 확정 기기가 마지막 `KeyUpdate`에서 읽어 [`KeyTree::recover`]에 넘깁니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainTip {
    pub seq: u64,
    pub root_commit: [u8; 32],
    pub depth: u8,
    /// 크루: 마지막 `FullRekey`의 세대(새 세대는 이것 + 1). 비공개: 쓰지 않음.
    pub generation: u64,
}

impl ChainTip {
    /// 합친 페이로드 뒤의 끝. 세대는 `FullRekey`에만 실리므로 그 전 값을 `generation`으로 넘깁니다.
    pub fn after(p: &KeyUpdatePayload, generation: u64) -> Self {
        ChainTip {
            seq: p.seq,
            root_commit: p.root_commit,
            depth: p.depth,
            generation: match p.kind {
                KeyUpdateKind::FullRekey { generation: g, .. } => g,
                _ => generation,
            },
        }
    }
}

impl std::fmt::Debug for KeyTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyTree")
            .field("group", &self.group)
            .field("kind", &self.kind)
            .field("seq", &self.seq)
            .field("generation", &self.generation)
            .field("depth", &self.depth())
            .field("devices", &self.len())
            .field("holders", &self.holders.len())
            .finish_non_exhaustive()
    }
}

/// 한 갱신에서 만든 것(공통 마무리에 넘김).
struct Draft {
    seq: u64,
    salt: [u8; 24],
    kind: KeyUpdateKind,
    t: Secret32,
    /// 새 R(바뀌면).
    r: Option<Secret32>,
    r_ct: Option<RCt>,
    crew: Option<CrewRef>,
    bundle_root: Option<[u8; 32]>,
    depth: u8,
    layout: Vec<LayoutOp>,
    wraps: Vec<Wrap>,
    sealed: Vec<SealedBox>,
    bundles: Vec<DeviceBundle>,
    #[cfg(test)]
    sites: Vec<WrapSite>,
}

impl Draft {
    fn new(seq: u64, kind: KeyUpdateKind, t: Secret32, depth: u8, crew: Option<CrewRef>) -> Self {
        Draft {
            seq,
            salt: random_salt(),
            kind,
            t,
            r: None,
            r_ct: None,
            crew,
            bundle_root: None,
            depth,
            layout: Vec::new(),
            wraps: Vec::new(),
            sealed: Vec::new(),
            bundles: Vec::new(),
            #[cfg(test)]
            sites: Vec::new(),
        }
    }

    fn wrap(&mut self, cx: &WrapCx<'_>, wrapping: &Secret32, node: NodeIdx, key: &Secret32) {
        let site = super::WrapSite {
            kind: WrapKind::Tree,
            node,
            version: self.seq,
        };
        self.wraps.push(cx.seal(wrapping, site, key));
        #[cfg(test)]
        self.sites.push(site);
    }

    /// 새 R을 뽑아 새 T로 암호화합니다.
    fn new_r(&mut self, space: &SpaceId, group: &KeyGroupId) {
        let r = Secret32::random();
        self.r_ct = Some(keysched::seal_r(
            &self.t, space, group, self.seq, &self.salt, &r,
        ));
        self.r = Some(r);
    }

    /// 이 갱신의 뿌리 약속(새 R이 없으면 지금 R `r`).
    fn commit(&self, space: &SpaceId, group: &KeyGroupId, r: &Secret32) -> [u8; 32] {
        keysched::root_commit(
            space,
            group,
            self.seq,
            &self.t,
            self.r.as_ref().unwrap_or(r),
        )
    }
}

pub(crate) fn random_salt() -> [u8; 24] {
    let mut s = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut s);
    s
}

fn sorted_unique(v: &[DeviceId]) -> Vec<DeviceId> {
    let mut v = v.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

/// 같은 공간의 크루 그룹(비공개·덮개 그룹 작업에 필요).
pub(crate) fn crew_of<'a>(space: &SpaceId, cx: &CommitCtx<'a>) -> Result<&'a KeyTree> {
    match cx.crew {
        Some(c) if c.kind == GroupKind::Crew && c.space == *space && c.recovering.is_none() => {
            Ok(c)
        }
        _ => Err(KeyUpdateError::NeedCrewTree),
    }
}

impl KeyTree {
    /// 공간의 크루 그룹(아직 기기 없음, 순번 0). 첫 [`Self::update`]가 처음 기기들을 넣습니다.
    pub fn crew(space: SpaceId) -> Self {
        Self::new(space, KeyGroupId::crew(&space), GroupKind::Crew)
    }

    /// 비공개 키 그룹(자기 트리). 잎이 크루 잎에서 나오고 크루 재키를 따라가야 하므로 모든 작업에
    /// [`CommitCtx::crew`]가 필요합니다.
    pub fn private(space: SpaceId, group: KeyGroupId) -> Self {
        Self::new(space, group, GroupKind::Private)
    }

    fn new(space: SpaceId, group: KeyGroupId, kind: GroupKind) -> Self {
        KeyTree {
            space,
            group,
            kind,
            seq: 0,
            root_commit: [0; 32],
            last_event: None,
            generation: 0,
            t: Secret32::random(),
            r: Secret32::random(),
            enc: Enc::Direct(BTreeSet::new()),
            recovering: None,
            holders: BTreeSet::new(),
            rekey_epoch: 0,
            crew_epoch: 0,
            #[cfg(test)]
            last_sites: Vec::new(),
        }
    }

    /// 넘겨받지 못한 새 확정 기기가 공개 정보(사슬 끝, 지금 멤버 기기)로 이어받습니다. 노드 키·leaf_master·T를 모르므로
    /// 다음 작업은 [`Self::full_rekey`]여야 하고, 다른 작업은 [`KeyUpdateError::NeedFullRekey`]입니다.
    pub fn recover(
        space: SpaceId,
        group: KeyGroupId,
        kind: GroupKind,
        tip: ChainTip,
        devices: &[DeviceId],
    ) -> Self {
        let mut k = Self::new(space, group, kind);
        k.seq = tip.seq;
        k.root_commit = tip.root_commit;
        k.generation = tip.generation;
        k.enc = Enc::Direct(devices.iter().copied().collect());
        k.recovering = Some(tip.depth > 0);
        k
    }

    fn known(&self) -> Result<()> {
        match self.recovering {
            Some(_) => Err(KeyUpdateError::NeedFullRekey),
            None => Ok(()),
        }
    }

    pub fn space(&self) -> &SpaceId {
        &self.space
    }

    /// 공개 정보로 이어받아 키를 모르는 상태인지(다음 작업은 [`Self::full_rekey`]).
    pub fn is_recovering(&self) -> bool {
        self.recovering.is_some()
    }

    pub fn group(&self) -> &KeyGroupId {
        &self.group
    }

    pub fn kind(&self) -> GroupKind {
        self.kind
    }

    /// 마지막으로 낸 갱신의 순번(아직 없으면 0).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn t(&self) -> &Secret32 {
        &self.t
    }

    pub fn r(&self) -> &Secret32 {
        &self.r
    }

    pub fn root_commit(&self) -> &[u8; 32] {
        &self.root_commit
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 마지막 갱신 뒤의 사슬 끝. 넘겨받은 표가 내가 적용한 사슬과 같은지 볼 때 씁니다.
    pub fn tip(&self) -> ChainTip {
        ChainTip {
            seq: self.seq,
            root_commit: self.root_commit,
            depth: self.depth(),
            generation: self.generation,
        }
    }

    /// 트리 깊이(직접 인코딩이면 0).
    pub fn depth(&self) -> u8 {
        match &self.enc {
            Enc::Direct(_) => 0,
            Enc::Tree(t) => t.depth,
        }
    }

    pub fn is_tree(&self) -> bool {
        matches!(self.enc, Enc::Tree(_))
    }

    pub fn len(&self) -> usize {
        match &self.enc {
            Enc::Direct(m) => m.len(),
            Enc::Tree(t) => t.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, device: &DeviceId) -> bool {
        match &self.enc {
            Enc::Direct(m) => m.contains(device),
            Enc::Tree(t) => t.dev_slot.contains_key(device),
        }
    }

    /// 그룹 기기(트리면 칸 순서, 직접이면 정렬 순서).
    pub fn devices(&self) -> Vec<DeviceId> {
        match &self.enc {
            Enc::Direct(m) => m.iter().copied().collect(),
            Enc::Tree(t) => t.devices().map(|(_, d)| *d).collect(),
        }
    }

    pub fn slot_of(&self, device: &DeviceId) -> Option<u32> {
        self.tree()?.dev_slot.get(device).copied()
    }

    /// 확정 기기가 이 그룹의 다음 `KeyUpdate`를 앞 이벤트에 잇도록, 발행한 첫 조각의 이벤트 ID를 알려 줍니다.
    pub fn set_last_event(&mut self, id: EventId) {
        self.last_event = Some(id);
    }

    /// 크루 그룹의 표 보유 기기(이 기기 말고 leaf_master와 모든 그룹의 키를 가진 기기). 마지막 크루 전체 재키 뒤
    /// 넘기기로 표를 받거나 넘겨준 기기들입니다.
    pub fn holders(&self) -> &BTreeSet<DeviceId> {
        &self.holders
    }

    /// 해소기가 아는 표 보유 기기(넘겨준 옛 확정 기기, `KeyBackupSet` 예비 기기 등)를 더합니다. 크루 그룹에만 뜻이
    /// 있습니다. 이 기기들은 크루 전체 재키로만 뺄 수 있습니다.
    pub fn add_holders(&mut self, devices: impl IntoIterator<Item = DeviceId>) {
        debug_assert_eq!(self.kind, GroupKind::Crew, "표 보유 기기는 크루 그룹에 둠");
        self.holders.extend(devices);
    }

    /// 이 비공개 그룹이 크루 전체 재키(표 보유 기기 제거, 확정 기기 잃음)나 크루 트리 재구성 뒤에 머물러 있어 다음
    /// 작업이 새 T를 내야 하는지. 크루 전체 재키와 같은 묶음에서 불러야 표를 가졌던 기기가 비공개 키를 따라가지 못합니다.
    pub fn needs_rebuild(&self, crew: &KeyTree) -> bool {
        self.stale(crew)
    }

    /// 공개 사슬이 이 그룹의 마지막 새 T가 크루 전체 재키보다 앞선다고 말하면(넘겨받은 표의 판 정보는 믿지 않음) 다음
    /// 작업이 새 T를 내게 합니다. 비공개 그룹에만 뜻이 있습니다.
    pub fn mark_stale(&mut self) {
        if self.kind == GroupKind::Private {
            self.crew_epoch = u64::MAX;
        }
    }

    pub(crate) fn tree(&self) -> Option<&Tree> {
        match &self.enc {
            Enc::Tree(t) => Some(t),
            Enc::Direct(_) => None,
        }
    }

    pub(crate) fn leaf_of(&self, device: &DeviceId) -> Option<&Node> {
        let t = self.tree()?;
        t.get(NodeIdx::leaf(*t.dev_slot.get(device)?))
    }

    /// 비공개 그룹이 기대는 크루 상태(크루 그룹은 `None`).
    fn crew_ref<'a>(&self, cx: &CommitCtx<'a>) -> Result<Option<&'a KeyTree>> {
        match self.kind {
            GroupKind::Crew => Ok(None),
            _ => crew_of(&self.space, cx).map(Some),
        }
    }

    fn anchor(crew: Option<&KeyTree>) -> Option<CrewRef> {
        crew.map(|c| CrewRef {
            seq: c.seq,
            root_commit: c.root_commit,
        })
    }

    /// 비공개 그룹이 크루 재키를 따라가지 않았는지: 크루 전체 재키 뒤(넘긴 표에 이 그룹의 T·노드 키가 있었음)거나,
    /// 트리면 잎을 만든 크루 세대가 지나감.
    fn stale(&self, crew: &KeyTree) -> bool {
        self.kind == GroupKind::Private
            && (crew.rekey_epoch != self.crew_epoch
                || (self.is_tree() && crew.generation != self.generation))
    }

    /// 표 보유 기기를 빼는지. 크루에서는 늘 전체 재키가 필요하고, 비공개 그룹은 결과가 트리일 때만(직접 인코딩은 새
    /// T를 남는 기기에게만 봉인하므로 leaf_master를 알아도 소용없음).
    fn removes_holder(&self, removed: &[DeviceId], crew: Option<&KeyTree>) -> Option<DeviceId> {
        let holders = match self.kind {
            GroupKind::Crew => &self.holders,
            _ => &crew?.holders,
        };
        removed.iter().find(|d| holders.contains(d)).copied()
    }

    /// 기기를 빼고(`removed`) 넣습니다(`joined`). 빼는 기기가 있으면 `Rotate`(가입도 같은 이벤트에), 아니면 `Advance`.
    /// 인코딩 경계를 넘으면 직접 ↔ 트리로 바꿉니다(직접 → 트리는 `FullRekey`). 비공개 그룹이 크루 재키 뒤에 머물러
    /// 있으면 바뀐 기기가 없어도 새 T를 냅니다. 실패하면 상태는 그대로입니다.
    pub fn update(
        &mut self,
        removed: &[DeviceId],
        joined: &[DeviceId],
        cx: &CommitCtx<'_>,
    ) -> Result<KeyUpdateOutput> {
        self.known()?;
        let crew = self.crew_ref(cx)?;
        let (removed, joined) = self.check_change(removed, joined)?;
        if self.kind == GroupKind::Crew
            && let Some(d) = self.removes_holder(&removed, None)
        {
            return Err(KeyUpdateError::HolderNeedsFullRekey(d));
        }
        let after = self.len() - removed.len() + joined.len();
        let stale = crew.is_some_and(|c| self.stale(c));
        // 비공개 트리의 잎은 크루 잎에서 나오므로 크루가 직접 인코딩이면 비공개 그룹도 직접입니다.
        let crew_direct = crew.is_some_and(|c| !c.is_tree());
        if removed.is_empty() && joined.is_empty() && !stale && !(self.is_tree() && crew_direct) {
            return Err(KeyUpdateError::NothingToDo);
        }
        let fresh_kind = |removed: Vec<DeviceId>, joined: Vec<DeviceId>| {
            if removed.is_empty() && joined.is_empty() {
                KeyUpdateKind::Refresh
            } else {
                KeyUpdateKind::Rotate { removed, joined }
            }
        };
        if self.is_tree() {
            if after <= DIRECT_BACK || crew_direct {
                return self.direct_fresh(fresh_kind(removed, joined), cx, crew);
            }
            if stale {
                return self.rebuild(&removed, &joined, cx, crew);
            }
            return self.tree_change(&removed, &joined, cx, crew);
        }
        if after > DIRECT_MAX {
            self.rebuild(&removed, &joined, cx, crew)
        } else if removed.is_empty() && !stale {
            self.direct_advance(joined, cx, crew)
        } else {
            self.direct_fresh(fresh_kind(removed, joined), cx, crew)
        }
    }

    /// 시간 회전: 새 T(트리면 새 뿌리)와 새 R. 크루 재키 뒤에 머문 비공개 그룹이면 [`Self::update`]처럼 새로 만듭니다.
    pub fn refresh_root(&mut self, cx: &CommitCtx<'_>) -> Result<KeyUpdateOutput> {
        self.known()?;
        if self.is_empty() {
            return Err(KeyUpdateError::NothingToDo);
        }
        let crew = self.crew_ref(cx)?;
        if let Some(c) = crew
            && (self.stale(c) || (self.is_tree() && !c.is_tree()))
        {
            return self.update(&[], &[], cx);
        }
        if !self.is_tree() {
            return self.direct_fresh(KeyUpdateKind::Refresh, cx, crew);
        }
        let (space, group, seq) = (self.space, self.group, self.seq + 1);
        let anchor = Self::anchor(crew);
        let Enc::Tree(tree) = &mut self.enc else {
            unreachable!("위에서 확인")
        };
        let mut d = Draft::new(
            seq,
            KeyUpdateKind::Refresh,
            Secret32::random(),
            tree.depth,
            anchor,
        );
        let salt = d.salt;
        let wcx = WrapCx {
            space: &space,
            group: &group,
            seq,
            salt: &salt,
            crew: anchor.as_ref(),
        };
        let root = tree.root();
        let t = d.t.clone();
        for c in root.children() {
            if let Some(k) = tree.get(c) {
                let k = k.key.clone();
                d.wrap(&wcx, &k, root, &t);
            }
        }
        tree.set(
            root,
            Some(Node {
                key: t,
                version: d.seq,
            }),
        );
        d.new_r(&space, &group);
        Ok(self.finish(d))
    }

    /// R만 새로(T는 그대로). 채널이 그룹을 떠나거나 기록 공개를 끌 때. 크루 재키 뒤에 머문 비공개 그룹이면 R만이
    /// 아니라 T까지 새로 냅니다(넘긴 표를 가졌던 기기가 옛 T로 새 R을 열지 못하게).
    pub fn refresh_r(&mut self, cx: &CommitCtx<'_>) -> Result<KeyUpdateOutput> {
        self.known()?;
        let crew = self.crew_ref(cx)?;
        if let Some(c) = crew
            && (self.stale(c) || (self.is_tree() && !c.is_tree()))
        {
            return self.update(&[], &[], cx);
        }
        let mut d = Draft::new(
            self.seq + 1,
            KeyUpdateKind::RRefresh,
            self.t.clone(),
            self.depth(),
            Self::anchor(crew),
        );
        d.new_r(&self.space, &self.group);
        Ok(self.finish(d))
    }

    /// 전체 재키: 트리면 새 leaf_master(크루) 또는 새 크루 잎(비공개)으로 모든 노드를 새로 만들고, 직접 인코딩이면 새 T를
    /// 모두에게 봉인합니다. 표 보유 기기(확정 기기였던 기기·넘겨받은 기기)를 뺄 때와 확정 기기를 잃었을 때 씁니다.
    /// 크루 그룹이면 표 보유 기기 목록을 비우고 재키 판을 올려, 같은 공간의 비공개·덮개 그룹이 다음 작업에서 새 T를
    /// 내게 합니다. 직접 인코딩이어도 종류는 `FullRekey`(깊이 0, 세대 그대로)라, 해소기가 이 갱신을 표 보유 기기를 비우는
    /// 재키로 알아봅니다(같은 모양의 보통 회전과 가리려고).
    pub fn full_rekey(
        &mut self,
        removed: &[DeviceId],
        joined: &[DeviceId],
        cx: &CommitCtx<'_>,
    ) -> Result<KeyUpdateOutput> {
        let crew = self.crew_ref(cx)?;
        let (removed, joined) = self.check_change(removed, joined)?;
        let after = self.len() - removed.len() + joined.len();
        if after == 0 && self.is_empty() {
            return Err(KeyUpdateError::NothingToDo);
        }
        let was_tree = self.recovering.unwrap_or(self.is_tree());
        let direct = crew.is_some_and(|c| !c.is_tree())
            || if was_tree {
                after <= DIRECT_BACK
            } else {
                after <= DIRECT_MAX
            };
        let out = if !direct {
            self.rebuild(&removed, &joined, cx, crew)?
        } else {
            let kind = KeyUpdateKind::FullRekey {
                generation: self.generation,
                removed,
                joined,
            };
            self.direct_fresh(kind, cx, crew)?
        };
        if self.kind == GroupKind::Crew {
            self.holders.clear();
            self.rekey_epoch += 1;
        }
        Ok(out)
    }

    fn check_change(
        &self,
        removed: &[DeviceId],
        joined: &[DeviceId],
    ) -> Result<(Vec<DeviceId>, Vec<DeviceId>)> {
        let (removed, joined) = (sorted_unique(removed), sorted_unique(joined));
        if let Some(d) = removed.iter().find(|d| !self.contains(d)) {
            return Err(KeyUpdateError::UnknownDevice(*d));
        }
        if let Some(d) = joined.iter().find(|d| self.contains(d)) {
            return Err(KeyUpdateError::AlreadyMember(*d));
        }
        if self.len() - removed.len() + joined.len() > MAX_DEVICES {
            return Err(KeyUpdateError::TooManyDevices);
        }
        Ok((removed, joined))
    }

    /// 비공개 트리가 쓸 크루 트리.
    fn crew_tree(crew: Option<&KeyTree>) -> Result<&KeyTree> {
        crew.filter(|c| c.is_tree())
            .ok_or(KeyUpdateError::NeedCrewTree)
    }

    /// 새 잎들(트리 인코딩). 크루면 leaf_master, 비공개면 크루 잎에서.
    fn leaves(
        &self,
        devices: &[DeviceId],
        leaf_master: Option<&Secret32>,
        generation: u64,
        crew: Option<&KeyTree>,
    ) -> Result<Vec<Node>> {
        match self.kind {
            GroupKind::Crew => {
                let lm = leaf_master.expect("크루 트리에는 leaf_master가 있음");
                Ok(devices
                    .iter()
                    .map(|d| Node {
                        key: leaf_key(lm, &self.group, d, generation),
                        version: generation,
                    })
                    .collect())
            }
            _ => {
                let crew = Self::crew_tree(crew)?;
                devices
                    .iter()
                    .map(|d| {
                        let leaf = crew.leaf_of(d).ok_or(KeyUpdateError::UnknownDevice(*d))?;
                        Ok(Node {
                            key: private_leaf(&leaf.key, &self.group, crew.generation),
                            version: crew.generation,
                        })
                    })
                    .collect()
            }
        }
    }

    fn members_after(&self, removed: &[DeviceId], joined: &[DeviceId]) -> BTreeSet<DeviceId> {
        let mut m: BTreeSet<DeviceId> = self.devices().into_iter().collect();
        for d in removed {
            m.remove(d);
        }
        m.extend(joined.iter().copied());
        m
    }

    /// 직접 인코딩 가입: T' = advance(T)를 가입 기기에게만 봉인합니다(R도 함께).
    fn direct_advance(
        &mut self,
        joined: Vec<DeviceId>,
        cx: &CommitCtx<'_>,
        crew: Option<&KeyTree>,
    ) -> Result<KeyUpdateOutput> {
        let to = recipients(cx.boxes, &joined)?;
        let seq = self.seq + 1;
        let t = keysched::advance(&self.t, &self.group, seq, &keysched::joined_hash(&joined));
        let mut d = Draft::new(
            seq,
            KeyUpdateKind::Advance {
                joined: joined.clone(),
            },
            t,
            0,
            Self::anchor(crew),
        );
        let commit = d.commit(&self.space, &self.group, &self.r);
        let bundle = Bundle::Direct {
            t: d.t.clone(),
            r: Some(self.r.clone()),
        };
        d.sealed.push(SealedBox(seal_to(
            &to,
            &direct_ad(&self.space, &self.group, seq, &commit),
            &bundle.encode(),
        )));
        let Enc::Direct(m) = &mut self.enc else {
            unreachable!("직접 인코딩에서만 부름")
        };
        m.extend(joined.iter().copied());
        Ok(self.finish(d))
    }

    /// 직접 인코딩으로 새 T·새 R을 남는 기기 모두에게(트리 → 직접 전환 포함).
    fn direct_fresh(
        &mut self,
        kind: KeyUpdateKind,
        cx: &CommitCtx<'_>,
        crew: Option<&KeyTree>,
    ) -> Result<KeyUpdateOutput> {
        let members = self.members_after(kind.removed(), kind.joined());
        let to = recipients(cx.boxes, &members)?;
        let mut d = Draft::new(
            self.seq + 1,
            kind,
            Secret32::random(),
            0,
            Self::anchor(crew),
        );
        d.new_r(&self.space, &self.group);
        if !to.is_empty() {
            let commit = d.commit(&self.space, &self.group, &self.r);
            let bundle = Bundle::Direct {
                t: d.t.clone(),
                r: None,
            };
            d.sealed.push(SealedBox(seal_to(
                &to,
                &direct_ad(&self.space, &self.group, d.seq, &commit),
                &bundle.encode(),
            )));
        }
        self.enc = Enc::Direct(members);
        if let Some(c) = crew {
            self.crew_epoch = c.rekey_epoch;
        }
        Ok(self.finish(d))
    }

    /// 트리를 새로 만듭니다(`FullRekey`): 직접 → 트리 전환, leaf_master 교체, 비공개 트리의 크루 재키 따라잡기.
    fn rebuild(
        &mut self,
        removed: &[DeviceId],
        joined: &[DeviceId],
        cx: &CommitCtx<'_>,
        crew: Option<&KeyTree>,
    ) -> Result<KeyUpdateOutput> {
        if self.kind == GroupKind::Private
            && let Some(d) = self.removes_holder(removed, crew)
        {
            return Err(KeyUpdateError::HolderNeedsFullRekey(d));
        }
        let gone: HashSet<&DeviceId> = removed.iter().collect();
        let mut order: Vec<DeviceId> = self
            .devices()
            .into_iter()
            .filter(|d| !gone.contains(d))
            .collect();
        order.extend(joined.iter().copied());
        // 새로 짓는 트리는 칸을 차례로 주므로, 사용자별로 모아 두면 한 사용자의 기기가 이웃 칸에 들어갑니다.
        order.sort_by_cached_key(|d| (cx.boxes.user_of(d), *d));
        let (generation, leaf_master) = match self.kind {
            GroupKind::Crew => (self.generation + 1, Some(Secret32::random())),
            _ => (Self::crew_tree(crew)?.generation, None),
        };
        let leaves = self.leaves(&order, leaf_master.as_ref(), generation, crew)?;
        let to = match self.kind {
            GroupKind::Crew => recipients(cx.boxes, &order)?,
            _ => Vec::new(),
        };
        let depth = depth_for(order.len());
        let mut tree = Tree::new(depth, leaf_master);
        let seq = self.seq + 1;
        let anchor = Self::anchor(crew);
        let mut d = Draft::new(
            seq,
            KeyUpdateKind::FullRekey {
                generation,
                removed: removed.to_vec(),
                joined: joined.to_vec(),
            },
            Secret32([0; 32]),
            depth,
            anchor,
        );
        let private = self.kind == GroupKind::Private;
        for (slot, (dev, leaf)) in order.iter().zip(leaves).enumerate() {
            tree.place(*dev, slot as u32, leaf);
            if private {
                d.layout.push(LayoutOp::Fill {
                    slot: slot as u32,
                    device: *dev,
                });
            }
        }
        let (space, group, salt) = (self.space, self.group, d.salt);
        let wcx = WrapCx {
            space: &space,
            group: &group,
            seq,
            salt: &salt,
            crew: anchor.as_ref(),
        };
        // 칸을 앞에서부터 채웠으므로 높이 l에서 키가 있는 노드는 앞 ceil(n / 2^l)개입니다.
        let n = order.len();
        for l in 1..=depth {
            let width = n.div_ceil(1 << l);
            for p in 0..width as u32 {
                let node = NodeIdx { level: l, pos: p };
                let key = Secret32::random();
                if private {
                    for c in node.children() {
                        if let Some(k) = tree.get(c) {
                            let k = k.key.clone();
                            d.wrap(&wcx, &k, node, &key);
                        }
                    }
                }
                tree.set(node, Some(Node { key, version: seq }));
            }
        }
        d.t = tree
            .get(tree.root())
            .expect("기기가 있으면 뿌리 키가 있음")
            .key
            .clone();
        d.new_r(&space, &group);
        let commit = d.commit(&space, &group, &self.r);
        let items = to
            .into_iter()
            .zip(0u32..)
            .map(|((dev, pk), slot)| {
                let bundle = Bundle::Tree {
                    slot,
                    generation,
                    leaf: tree.get(NodeIdx::leaf(slot)).expect("잎").key.clone(),
                    path: tree.path(slot),
                    r: None,
                };
                let env = seal_to(
                    &[(dev, pk)],
                    &bundle_ad(&space, &group, seq, &dev, &commit),
                    &bundle.encode(),
                );
                (dev, env)
            })
            .collect();
        (d.bundle_root, d.bundles) = bundle_tree(items);
        self.generation = generation;
        self.enc = Enc::Tree(Box::new(tree));
        if let Some(c) = crew {
            self.crew_epoch = c.rekey_epoch;
        }
        Ok(self.finish(d))
    }

    /// 트리 인코딩의 제거·가입. 제거 경로는 새 무작위 키로, 가입 경로(제거 경로 밖)는 한 방향 전진으로 바꿉니다.
    fn tree_change(
        &mut self,
        removed: &[DeviceId],
        joined: &[DeviceId],
        cx: &CommitCtx<'_>,
        crew: Option<&KeyTree>,
    ) -> Result<KeyUpdateOutput> {
        if self.kind == GroupKind::Private
            && let Some(d) = self.removes_holder(removed, crew)
        {
            return Err(KeyUpdateError::HolderNeedsFullRekey(d));
        }
        let generation = self.generation;
        let lm = self.tree().and_then(|t| t.leaf_master.clone());
        // 넣는 순서: 사용자별로 모으고, 같은 사용자의 기기가 이미 있으면 그 칸 가까이(`take_slot_near`).
        let mut placing: Vec<(Option<UserId>, DeviceId)> =
            joined.iter().map(|d| (cx.boxes.user_of(d), *d)).collect();
        placing.sort_unstable();
        let mut near: HashMap<UserId, u32> = HashMap::new();
        if placing.iter().any(|(u, _)| u.is_some())
            && let Some(t) = self.tree()
        {
            let want: HashSet<UserId> = placing.iter().filter_map(|(u, _)| *u).collect();
            for (slot, d) in t.devices() {
                if let Some(u) = cx.boxes.user_of(d).filter(|u| want.contains(u)) {
                    near.entry(u).or_insert(slot);
                }
            }
        }
        let placing_devices: Vec<DeviceId> = placing.iter().map(|(_, d)| *d).collect();
        let leaves = self.leaves(&placing_devices, lm.as_ref(), generation, crew)?;
        let to = match self.kind {
            GroupKind::Crew => recipients(cx.boxes, &placing_devices)?,
            _ => Vec::new(),
        };
        let (space, group, kind) = (self.space, self.group, self.kind);
        let private = kind == GroupKind::Private;
        let seq = self.seq + 1;
        let anchor = Self::anchor(crew);
        let Enc::Tree(tree) = &mut self.enc else {
            unreachable!("트리 인코딩에서만 부름")
        };
        let kind = if removed.is_empty() {
            KeyUpdateKind::Advance {
                joined: joined.to_vec(),
            }
        } else {
            KeyUpdateKind::Rotate {
                removed: removed.to_vec(),
                joined: joined.to_vec(),
            }
        };
        let mut d = Draft::new(seq, kind, Secret32([0; 32]), 0, anchor);
        let d0 = tree.depth;

        // 1. 제거: 칸을 비우고 경로를 표시한 뒤, 아래가 빈 노드의 키를 지웁니다.
        let mut dirty: BTreeSet<NodeIdx> = BTreeSet::new();
        for dev in removed {
            let slot = tree.clear(dev).expect("검사한 멤버");
            d.layout.push(LayoutOp::Clear { slot });
            dirty.extend((1..=d0).map(|l| NodeIdx::ancestor(slot, l)));
        }
        for &n in &dirty {
            let [a, b] = n.children();
            if !tree.present(a) && !tree.present(b) {
                tree.set(n, None);
            }
        }
        let old_root_present = tree.present(NodeIdx::root(d0));

        // 2. 가입: 가장 왼쪽 빈 칸(모자라면 트리를 키움).
        let mut joined_slots: BTreeSet<u32> = BTreeSet::new();
        for ((user, dev), leaf) in placing.iter().zip(leaves) {
            let slot = tree.take_slot_near(user.and_then(|u| near.get(&u).copied()));
            if let Some(u) = user {
                near.entry(*u).or_insert(slot);
            }
            tree.place(*dev, slot, leaf);
            d.layout.push(LayoutOp::Fill { slot, device: *dev });
            joined_slots.insert(slot);
        }
        let depth = tree.depth;
        d.depth = depth;
        // 가입 기기가 아래에 있는 노드(잎 포함).
        let mut newly: HashSet<NodeIdx> = HashSet::new();
        for &s in &joined_slots {
            for l in 0..=depth {
                if !newly.insert(NodeIdx::ancestor(s, l)) && l > 0 {
                    break;
                }
            }
        }
        let mut todo: BTreeSet<NodeIdx> = dirty.clone();
        todo.extend(newly.iter().filter(|n| n.level > 0));
        todo.extend((d0 + 1..=depth).map(NodeIdx::root));

        // 이 갱신 전부터 있던(빠지지 않은) 기기가 아래에 있는지. 내부 노드 키는 아직 바꾸지 않았으므로 지금 키가 있는지가
        // 곧 그것입니다(가입으로는 잎만 바뀜).
        let mut pre: HashMap<NodeIdx, bool> = HashMap::new();
        for n in &todo {
            for c in n.children() {
                if c.level >= 1 && c.level <= d0 {
                    pre.insert(c, tree.present(c));
                }
            }
        }
        let existing = |c: NodeIdx, tree: &Tree| -> bool {
            if c.level == 0 {
                tree.slots[c.pos as usize].is_some() && !joined_slots.contains(&c.pos)
            } else if c.level > d0 {
                c.pos == 0 && old_root_present
            } else {
                pre.get(&c).copied().unwrap_or(false)
            }
        };

        // 3. 아래에서 위로 새 키를 정하고 감쌉니다.
        let jh = keysched::joined_hash(joined);
        let salt = d.salt;
        let wcx = WrapCx {
            space: &space,
            group: &group,
            seq,
            salt: &salt,
            crew: anchor.as_ref(),
        };
        for &n in &todo {
            let [a, b] = n.children();
            if !tree.present(a) && !tree.present(b) {
                tree.set(n, None);
                continue;
            }
            let fresh = n.level > d0 || dirty.contains(&n) || !tree.present(n);
            let key = if fresh {
                Secret32::random()
            } else {
                let old = &tree.get(n).expect("있음").key;
                advance_node(old, &group, seq, n, &jh)
            };
            for c in [a, b] {
                let Some(kc) = tree.get(c) else { continue };
                if (fresh && existing(c, tree)) || (private && newly.contains(&c)) {
                    let kc = kc.key.clone();
                    d.wrap(&wcx, &kc, n, &key);
                }
            }
            tree.set(n, Some(Node { key, version: seq }));
        }
        d.t = tree
            .get(tree.root())
            .expect("기기가 있으면 뿌리 키가 있음")
            .key
            .clone();

        // 4. R: 제거가 있으면 새로. 가입만이면 그대로이고, 비공개 가입자는 같은 R을 `r_ct`로 받습니다.
        if !removed.is_empty() {
            d.new_r(&space, &group);
        } else if private {
            d.r_ct = Some(keysched::seal_r(&d.t, &space, &group, seq, &salt, &self.r));
        }
        // 크루 가입자 몫: 잎과 경로 전체(와 `r_ct`가 없으면 지금 R). R도 뿌리 약속에 들어 있어 가입자가 확인합니다.
        let commit = d.commit(&space, &group, &self.r);
        let r_for_joiner = d.r_ct.is_none().then(|| self.r.clone());
        let Enc::Tree(tree) = &self.enc else {
            unreachable!()
        };
        let items = to
            .into_iter()
            .map(|(dev, pk)| {
                let slot = tree.dev_slot[&dev];
                let bundle = Bundle::Tree {
                    slot,
                    generation,
                    leaf: tree.get(NodeIdx::leaf(slot)).expect("잎").key.clone(),
                    path: tree.path(slot),
                    r: r_for_joiner.clone(),
                };
                let env = seal_to(
                    &[(dev, pk)],
                    &bundle_ad(&space, &group, seq, &dev, &commit),
                    &bundle.encode(),
                );
                (dev, env)
            })
            .collect();
        (d.bundle_root, d.bundles) = bundle_tree(items);
        Ok(self.finish(d))
    }

    fn finish(&mut self, d: Draft) -> KeyUpdateOutput {
        let root_commit = d.commit(&self.space, &self.group, &self.r);
        let payload = KeyUpdatePayload {
            group: self.group,
            seq: d.seq,
            prev: self.last_event.take(),
            kind: d.kind,
            root_commit,
            prev_root_commit: self.root_commit,
            crew: d.crew,
            bundle_root: d.bundle_root,
            shards: None,
            depth: d.depth,
            salt: d.salt,
            layout: d.layout,
            wraps: d.wraps,
            sealed: d.sealed,
            r_ct: d.r_ct,
            part: 0,
            parts: 1,
        };
        self.seq = d.seq;
        self.t = d.t;
        if let Some(r) = d.r {
            self.r = r;
        }
        self.root_commit = root_commit;
        self.recovering = None;
        #[cfg(test)]
        {
            self.last_sites = d.sites;
        }
        KeyUpdateOutput {
            payload,
            bundles: d.bundles,
        }
    }
}
