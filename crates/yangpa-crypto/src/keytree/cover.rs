//! 제외 덮개 그룹([`CoverGroup`]): "이 몇 기기만 빼고 크루 전체". 자기 트리 없이 크루 트리의 노드로 그룹 키를 감쌉니다.

use std::collections::BTreeSet;

use yangpa_core::body::Secret32;
use yangpa_core::ids::{DeviceId, EventId, SpaceId};
use yangpa_core::keyupdate::{
    CrewRef, KeyGroupId, KeyUpdateKind, KeyUpdatePayload, NodeIdx, RCt, SealedBox, Wrap,
};

use super::committer::{crew_of, random_salt};
use super::{
    Bundle, CommitCtx, KeyTree, KeyUpdateError, KeyUpdateOutput, Result, WrapCx, WrapKind,
    WrapSite, direct_ad, recipients,
};
use crate::keysched;
use crate::sealed::seal_to;

/// 깊이 `depth` 트리의 칸 배치에서, 제외하지 않은 기기를 모두 덮고 제외 기기는 하나도 덮지 않는 가장 작은 완전 하위
/// 트리 집합(노드 번호 순).
///
/// 제외 기기가 없는 하위 트리 가운데 가장 큰 것들(부모에는 제외 기기가 있음) 중 기기가 있는 것이 답입니다. 그런
/// 하위 트리끼리는 겹치지 않고, 어떤 정확한 덮개든 그 하나하나 안에 노드가 적어도 하나 있어야 하므로 최소입니다.
/// 덮개 크기는 많아야 (제외 기기 수 × 깊이)입니다.
pub fn exclusion_cover(
    depth: u8,
    slots: &[Option<DeviceId>],
    excluded: &BTreeSet<DeviceId>,
) -> Vec<NodeIdx> {
    assert_eq!(slots.len(), 1usize << depth, "칸 수는 2^깊이");
    // 높이마다 (아래에 덮을 기기가 있음, 아래에 제외 기기가 있음).
    let mut levels: Vec<Vec<(bool, bool)>> = Vec::with_capacity(usize::from(depth) + 1);
    levels.push(
        slots
            .iter()
            .map(|s| match s {
                Some(d) if excluded.contains(d) => (false, true),
                Some(_) => (true, false),
                None => (false, false),
            })
            .collect(),
    );
    for l in 1..=usize::from(depth) {
        let below = &levels[l - 1];
        let up = below
            .chunks(2)
            .map(|c| (c[0].0 || c[1].0, c[0].1 || c[1].1))
            .collect();
        levels.push(up);
    }
    let mut out = Vec::new();
    let mut stack = vec![NodeIdx::root(depth)];
    while let Some(n) = stack.pop() {
        let (member, excl) = levels[usize::from(n.level)][n.pos as usize];
        if !member {
            continue;
        }
        if !excl {
            out.push(n);
        } else {
            // 덮을 기기와 제외 기기가 함께 있으면 잎이 아님.
            stack.extend(n.children());
        }
    }
    out.sort_unstable();
    out
}

/// 덮개 그룹의 확정 기기 쪽 상태. 작업은 제자리에서 바꾸므로 받아들여질 때까지 복제본을 남겨 둡니다([`KeyTree`]와 같음).
#[derive(Clone)]
pub struct CoverGroup {
    pub(crate) space: SpaceId,
    pub(crate) group: KeyGroupId,
    pub(crate) seq: u64,
    pub(crate) root_commit: [u8; 32],
    pub(crate) last_event: Option<EventId>,
    pub(crate) t: Secret32,
    pub(crate) r: Secret32,
    pub(crate) members: BTreeSet<DeviceId>,
    /// 마지막으로 감쌀 때 쓴 크루 세대. 크루 트리를 새로 만들면 새 T로 다시 감쌉니다.
    pub(crate) crew_generation: u64,
    /// 마지막으로 새 T를 냈을 때의 크루 재키 판([`KeyTree`]의 `rekey_epoch`). 크루가 전체 재키를 하면 새 T를 냅니다.
    pub(crate) crew_epoch: u64,
    /// 공개 정보로 이어받아 T·R을 모름: 다음 갱신은 새 T로 회전합니다.
    pub(crate) recovering: bool,
    #[cfg(test)]
    pub(crate) last_sites: Vec<WrapSite>,
}

impl std::fmt::Debug for CoverGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverGroup")
            .field("group", &self.group)
            .field("seq", &self.seq)
            .field("members", &self.members.len())
            .finish_non_exhaustive()
    }
}

impl CoverGroup {
    pub fn new(space: SpaceId, group: KeyGroupId) -> Self {
        CoverGroup {
            space,
            group,
            seq: 0,
            root_commit: [0; 32],
            last_event: None,
            t: Secret32::random(),
            r: Secret32::random(),
            members: BTreeSet::new(),
            crew_generation: 0,
            crew_epoch: 0,
            recovering: false,
            #[cfg(test)]
            last_sites: Vec::new(),
        }
    }

    /// 넘겨받지 못한 새 확정 기기가 공개 정보(사슬 끝, 지금 멤버)로 이어받습니다. 다음 [`Self::update`]는 새 T·R로
    /// 회전합니다.
    pub fn recover(
        space: SpaceId,
        group: KeyGroupId,
        tip: super::ChainTip,
        members: &BTreeSet<DeviceId>,
    ) -> Self {
        let mut c = Self::new(space, group);
        c.seq = tip.seq;
        c.root_commit = tip.root_commit;
        c.members = members.clone();
        c.recovering = true;
        c
    }

    /// 공개 사슬이 이 그룹의 마지막 새 T가 크루 전체 재키보다 앞선다고 말하면 다음 작업이 새 T를 내게 합니다.
    pub fn mark_stale(&mut self) {
        self.crew_epoch = u64::MAX;
    }

    /// 공개 정보로 이어받아 키를 모르는 상태인지(다음 갱신은 새 T).
    pub fn is_recovering(&self) -> bool {
        self.recovering
    }

    pub fn space(&self) -> &SpaceId {
        &self.space
    }

    pub fn group(&self) -> &KeyGroupId {
        &self.group
    }

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

    pub fn members(&self) -> &BTreeSet<DeviceId> {
        &self.members
    }

    /// 마지막 갱신 뒤의 사슬 끝(깊이·세대는 0).
    pub fn tip(&self) -> super::ChainTip {
        super::ChainTip {
            seq: self.seq,
            root_commit: self.root_commit,
            depth: 0,
            generation: 0,
        }
    }

    pub fn set_last_event(&mut self, id: EventId) {
        self.last_event = Some(id);
    }

    /// 크루 갱신 뒤 멤버(크루 기기 − `excluded`)를 맞춥니다. 빠진 기기가 있거나 크루가 전체 재키·트리 재구성을 했으면
    /// 새 T·R을 덮개 전체에(`Rotate`/`Refresh`), 들어온 기기만 있으면 T' = advance(T)를 들어온 기기를 덮는 노드에만
    /// (`Advance`) 감쌉니다. 처음 갱신(아직 멤버 없음)은 기기 목록 없는 `Refresh`입니다(멤버 = 크루 − 제외라 목록이
    /// 필요 없고, 4만 대 크루의 목록 1.4MB를 싣지 않음). 바뀐 것이 없으면 `None`. 크루가 직접 인코딩이면 감싸기 대신
    /// 봉인 상자를 씁니다. 크루가 트리일 때 표 보유 기기를 빼려면 크루 전체 재키가 먼저입니다.
    pub fn update(
        &mut self,
        excluded: &BTreeSet<DeviceId>,
        cx: &CommitCtx<'_>,
    ) -> Result<Option<KeyUpdateOutput>> {
        self.change(excluded, cx, false)
    }

    /// 시간 회전: 새 T·R(멤버 변화도 함께 반영).
    pub fn refresh(
        &mut self,
        excluded: &BTreeSet<DeviceId>,
        cx: &CommitCtx<'_>,
    ) -> Result<KeyUpdateOutput> {
        Ok(self
            .change(excluded, cx, true)?
            .expect("강제 회전은 늘 갱신을 냄"))
    }

    /// R만 새로. 크루 재키 뒤에 머물러 있으면 T까지 새로 냅니다(넘긴 표를 가졌던 기기가 옛 T로 새 R을 열지 못하게).
    pub fn refresh_r(
        &mut self,
        excluded: &BTreeSet<DeviceId>,
        cx: &CommitCtx<'_>,
    ) -> Result<KeyUpdateOutput> {
        if self.recovering {
            return Err(KeyUpdateError::NeedFullRekey);
        }
        let crew = crew_of(&self.space, cx)?;
        if self.stale(crew) {
            return self.refresh(excluded, cx);
        }
        let seq = self.seq + 1;
        let salt = random_salt();
        let r = Secret32::random();
        let r_ct = keysched::seal_r(&self.t, &self.space, &self.group, seq, &salt, &r);
        let t = self.t.clone();
        Ok(self.finish(Done {
            seq,
            salt,
            kind: KeyUpdateKind::RRefresh,
            crew: anchor(crew),
            t,
            r: Some(r),
            r_ct: Some(r_ct),
            wraps: Vec::new(),
            sealed: Vec::new(),
        }))
    }

    /// 크루 재키(전체 재키·트리 재구성) 뒤에 머물러 있는지.
    fn stale(&self, crew: &KeyTree) -> bool {
        !self.members.is_empty()
            && (crew.generation() != self.crew_generation || crew.rekey_epoch != self.crew_epoch)
    }

    fn change(
        &mut self,
        excluded: &BTreeSet<DeviceId>,
        cx: &CommitCtx<'_>,
        force: bool,
    ) -> Result<Option<KeyUpdateOutput>> {
        let crew = crew_of(&self.space, cx)?;
        let now: BTreeSet<DeviceId> = crew
            .devices()
            .into_iter()
            .filter(|d| !excluded.contains(d))
            .collect();
        let removed: Vec<DeviceId> = self.members.difference(&now).copied().collect();
        let joined: Vec<DeviceId> = now.difference(&self.members).copied().collect();
        // 크루 트리에 남은 표 보유 기기는 leaf_master로 모든 크루 노드 키를 계산하므로, 덮개에서 빼도 새 T를 엽니다.
        if crew.is_tree()
            && let Some(d) = removed.iter().find(|d| crew.holders.contains(d))
        {
            return Err(KeyUpdateError::HolderNeedsFullRekey(*d));
        }
        let first = self.members.is_empty();
        let rotate = force || first || self.stale(crew) || self.recovering || !removed.is_empty();
        if !rotate && joined.is_empty() {
            return Ok(None);
        }
        let seq = self.seq + 1;
        let salt = random_salt();
        let (kind, t, targets) = if rotate {
            let kind = if first || (removed.is_empty() && joined.is_empty()) {
                KeyUpdateKind::Refresh
            } else {
                KeyUpdateKind::Rotate {
                    removed,
                    joined: joined.clone(),
                }
            };
            (kind, Secret32::random(), now.clone())
        } else {
            let t = keysched::advance(&self.t, &self.group, seq, &keysched::joined_hash(&joined));
            let targets = joined.iter().copied().collect();
            (KeyUpdateKind::Advance { joined }, t, targets)
        };
        let r = rotate.then(Secret32::random);
        let crew_ref = anchor(crew);
        let wcx = WrapCx {
            space: &self.space,
            group: &self.group,
            seq,
            salt: &salt,
            crew: crew_ref.as_ref(),
        };
        let commit = keysched::root_commit(
            &self.space,
            &self.group,
            seq,
            &t,
            r.as_ref().unwrap_or(&self.r),
        );
        let mut wraps = Vec::new();
        let mut sealed = Vec::new();
        #[cfg(test)]
        let mut sites = Vec::new();
        let r_ct = match crew.tree() {
            None => {
                // 크루가 직접 인코딩이면 받을 기기마다 봉인(가입만이면 R도 상자에).
                let to = recipients(cx.boxes, &targets)?;
                if !to.is_empty() {
                    let bundle = Bundle::Direct {
                        t: t.clone(),
                        r: (!rotate).then(|| self.r.clone()),
                    };
                    sealed.push(SealedBox(seal_to(
                        &to,
                        &direct_ad(&self.space, &self.group, seq, &commit),
                        &bundle.encode(),
                    )));
                }
                r.as_ref()
                    .map(|r| keysched::seal_r(&t, &self.space, &self.group, seq, &salt, r))
            }
            Some(tree) => {
                let slots: Vec<u32> = targets.iter().filter_map(|d| crew.slot_of(d)).collect();
                for node in exclusion_cover(tree.depth, &tree.slots, excluded) {
                    if !rotate && !slots.iter().any(|&s| node.covers(s)) {
                        continue;
                    }
                    let k = tree.get(node).expect("덮개 노드 아래에는 기기가 있음");
                    let site = WrapSite {
                        kind: WrapKind::Cover,
                        node,
                        version: k.version,
                    };
                    wraps.push(wcx.seal(&k.key, site, &t));
                    #[cfg(test)]
                    sites.push(site);
                }
                // 가입만이어도 가입자는 R을 r_ct로 받습니다(같은 R).
                Some(keysched::seal_r(
                    &t,
                    &self.space,
                    &self.group,
                    seq,
                    &salt,
                    r.as_ref().unwrap_or(&self.r),
                ))
            }
        };
        self.members = now;
        if rotate {
            self.crew_generation = crew.generation();
            self.crew_epoch = crew.rekey_epoch;
        }
        #[cfg(test)]
        {
            self.last_sites = sites;
        }
        Ok(Some(self.finish(Done {
            seq,
            salt,
            kind,
            crew: crew_ref,
            t,
            r,
            r_ct,
            wraps,
            sealed,
        })))
    }

    fn finish(&mut self, d: Done) -> KeyUpdateOutput {
        let root_commit = keysched::root_commit(
            &self.space,
            &self.group,
            d.seq,
            &d.t,
            d.r.as_ref().unwrap_or(&self.r),
        );
        let payload = KeyUpdatePayload {
            group: self.group,
            seq: d.seq,
            prev: self.last_event.take(),
            kind: d.kind,
            root_commit,
            prev_root_commit: self.root_commit,
            crew: d.crew,
            bundle_root: None,
            shards: None,
            depth: 0,
            salt: d.salt,
            layout: Vec::new(),
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
        self.recovering = false;
        KeyUpdateOutput {
            payload,
            bundles: Vec::new(),
        }
    }
}

/// 덮개 갱신 하나에서 만든 것.
struct Done {
    seq: u64,
    salt: [u8; 24],
    kind: KeyUpdateKind,
    crew: Option<CrewRef>,
    t: Secret32,
    r: Option<Secret32>,
    r_ct: Option<RCt>,
    wraps: Vec<Wrap>,
    sealed: Vec<SealedBox>,
}

fn anchor(crew: &KeyTree) -> Option<CrewRef> {
    Some(CrewRef {
        seq: crew.seq(),
        root_commit: *crew.root_commit(),
    })
}

/// 크루 트리 기준 덮개(시험·진단용).
impl KeyTree {
    pub fn cover(&self, excluded: &BTreeSet<DeviceId>) -> Option<Vec<NodeIdx>> {
        let t = self.tree()?;
        Some(exclusion_cover(t.depth, &t.slots, excluded))
    }
}
