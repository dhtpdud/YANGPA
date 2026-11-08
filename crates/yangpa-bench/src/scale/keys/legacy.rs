//! 키 그룹으로 바꾸기 전 구조(비교 모형): 키 그룹마다 MLS 그룹 하나, 확정 기기 한 대가 300ms씩 묶어 커밋합니다.
//!
//! - 제거가 있는 묶음은 경로가 있는 커밋입니다. 트리가 비어 있어 남은 모든 리프에 새 경로 비밀을 암호화하므로
//!   크기가 기기 수에 비례합니다(측정 기울기 기기당 약 1.24KB).
//! - 가입만 있는 묶음은 경로 없는 추가 커밋(약 3KB)이고, 새 에포크는 이전 에포크에서 계산됩니다.
//! - 웰컴은 트리를 담아 그룹 크기에 비례합니다(기기당 약 1.48KB).
//! - MLS 레인은 공간 스트림이라 비공개 그룹의 커밋·웰컴도 모든 크루 기기가 받습니다.
//! - 확정 기기는 커밋하려고 모든 그룹의 멤버입니다.
//!
//! 그 구조의 상태 키·기록 키 꾸러미는 모형에 넣지 않았습니다(에포크 비밀만 따라감).

use std::collections::{BTreeMap, BTreeSet};

use super::{CostClass, Cx, KeyScheme, Lane, OpCost, Pending, PendingGroup, Reach, Scope};
use crate::scale::keys::KeyId;
use crate::scale::trace::{CREW, Delta, Dev, Event, Group, Op};

#[derive(Clone, Debug, Default)]
struct MlsGroup {
    members: BTreeSet<Dev>,
    epoch: KeyId,
}

#[derive(Clone, Debug, Default)]
pub struct LegacyMls {
    groups: BTreeMap<Group, MlsGroup>,
    committer: Dev,
    pending: Pending,
    max_group: usize,
}

impl LegacyMls {
    fn reach(&self, cx: &Cx<'_>, g: Group) -> Reach {
        let own = self.groups.get(&g).map_or(0, |x| x.members.len());
        Reach {
            own,
            others: cx.world.devices().saturating_sub(own),
            target: 0,
        }
    }

    /// 묶음 하나를 커밋합니다.
    fn commit(&mut self, cx: &mut Cx<'_>, g: Group, pg: &PendingGroup, class: CostClass) -> OpCost {
        let m = cx.model;
        let committer = self.committer;
        let grp = self.groups.entry(g).or_default();
        let mut removed = false;
        for d in &pg.remove {
            // 확정 기기는 자격을 잃어도 커밋하려고 남습니다(옛 구조).
            if *d != committer && grp.members.remove(d) {
                removed = true;
            }
        }
        let joiners: Vec<Dev> = pg
            .add
            .iter()
            .filter(|d| grp.members.insert(**d))
            .copied()
            .collect();
        let n = grp.members.len();
        self.max_group = self.max_group.max(n);
        let mut c = OpCost::new(class, g);
        if !removed && joiners.is_empty() {
            return c;
        }
        let grp = self.groups.get_mut(&g).expect("방금 넣음");
        if removed {
            // 경로 비밀은 확정 기기와 가입 기기(웰컴으로 받음)를 뺀 기기마다 봉인합니다. 새 확정 기기 자신이 가입
            // 기기일 수 있어 n에서 빼서 세지 않고 실제 봉인 수를 셉니다.
            grp.epoch = cx.kn.key(Scope::Group(g), true);
            for &d in &grp.members {
                if d != committer && !joiners.contains(&d) {
                    cx.kn.seal(d, grp.epoch);
                    c.xwing += 1;
                }
            }
        } else {
            grp.epoch = cx.kn.derive(grp.epoch, Scope::Group(g), true);
        }
        for &j in &joiners {
            cx.kn.seal(j, grp.epoch);
        }
        c.xwing += joiners.len() as u64;
        let epoch = grp.epoch;
        cx.kn.settle(g, epoch);
        let reach = self.reach(cx, g);
        let body = if removed {
            (m.mls_commit_per_device * n as f64) as usize
        } else {
            m.mls_add_commit
        };
        c.event(cx.kn, m, "MLS 커밋", Lane::KeyAgreement, body, reach);
        if !joiners.is_empty() {
            let body = (m.mls_welcome_per_device * n as f64) as usize;
            c.event(cx.kn, m, "MLS 웰컴", Lane::KeyAgreement, body, reach);
        }
        c
    }

    fn flush(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        let groups = self.pending.take();
        cx.kn.begin();
        cx.kn.set_issuer(Some(self.committer));
        groups
            .iter()
            .map(|(g, pg)| self.commit(cx, *g, pg, pg.class()))
            .filter(|c| c.events > 0)
            .collect()
    }

    /// 확정 기기가 바뀜: 새 기기가 기다리던 변화와 이 변동의 변화(강등·폐기로 빠지는 기기)를 같은 커밋에 담아, 새
    /// 기기를 모든 그룹에 넣고 옛 기기는 자격 없는 그룹에서 뺍니다. 해소기가 넘긴 경우 옛 기기는 이미 없으므로
    /// 기다리던 묶음도 새 기기가 냅니다.
    fn change_committer(
        &mut self,
        cx: &mut Cx<'_>,
        new: Dev,
        mut changes: BTreeMap<Group, PendingGroup>,
    ) -> Vec<OpCost> {
        let old = std::mem::replace(&mut self.committer, new);
        cx.kn.begin();
        cx.kn.set_issuer(Some(new));
        let groups: Vec<Group> = self.groups.keys().copied().collect();
        let mut out = Vec::new();
        for g in groups {
            let mut pg = changes.remove(&g).unwrap_or_default();
            if !cx.world.group(g).contains(&old) {
                pg.remove.insert(old);
            }
            pg.add.insert(new);
            // 커밋 하나에 담기므로 기다리던 변화의 비용도 확정 기기 변경으로 셉니다.
            let c = self.commit(cx, g, &pg, CostClass::Committer);
            if c.events > 0 {
                out.push(c);
            }
        }
        out
    }
}

impl KeyScheme for LegacyMls {
    fn name(&self) -> &'static str {
        "legacy"
    }

    fn init(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        self.committer = cx.world.committer;
        cx.kn.set_issuer(Some(self.committer));
        let mut out = Vec::new();
        for g in 0..cx.world.groups.len() as Group {
            let mut members = cx.world.group(g).clone();
            members.insert(self.committer);
            let epoch = cx.kn.key(Scope::Group(g), true);
            for &d in &members {
                cx.kn.seal(d, epoch);
            }
            cx.kn.settle(g, epoch);
            let n = members.len();
            self.max_group = self.max_group.max(n);
            self.groups.insert(g, MlsGroup { members, epoch });
            let mut c = OpCost::new(CostClass::Setup, g);
            c.xwing = n as u64;
            let reach = self.reach(cx, g);
            let body = (cx.model.mls_welcome_per_device * n as f64) as usize;
            c.event(
                cx.kn,
                cx.model,
                "MLS 웰컴(처음)",
                Lane::KeyAgreement,
                body,
                reach,
            );
            out.push(c);
        }
        out
    }

    fn apply(&mut self, cx: &mut Cx<'_>, ev: &Event, d: &Delta) -> Vec<OpCost> {
        self.pending.push(d, &ev.op, ev.at_ms, cx.model, false);
        match d.committer {
            Some((_, new, _)) => {
                let changes = self.pending.take();
                self.change_committer(cx, new, changes)
            }
            None => Vec::new(),
        }
    }

    fn advance(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        if self.pending.due(cx.now).is_some() {
            return self.flush(cx);
        }
        Vec::new()
    }

    fn resync(&mut self, cx: &mut Cx<'_>, d: &Delta) -> Vec<OpCost> {
        // 두 확정 기기가 같은 에포크에서 따로 커밋했으므로 진 쪽 기기들은 갈래에 묶입니다. 이긴 쪽이 모든 그룹을 새
        // 에포크로 다시 세우고(모두에게 웰컴) 합친 멤버십을 반영합니다.
        let mut out = self.flush(cx);
        self.pending
            .push(d, &Op::PartitionEnd, cx.now, cx.model, false);
        let mut changes = self.pending.take();
        cx.kn.begin();
        cx.kn.set_issuer(Some(self.committer));
        let m = cx.model;
        let committer = self.committer;
        let groups: Vec<Group> = self.groups.keys().copied().collect();
        for g in groups {
            let pg = changes.remove(&g).unwrap_or_default();
            let grp = self.groups.get_mut(&g).expect("그룹");
            for dev in &pg.remove {
                if *dev != committer {
                    grp.members.remove(dev);
                }
            }
            grp.members.extend(pg.add.iter().copied());
            grp.epoch = cx.kn.key(Scope::Group(g), true);
            for &dev in &grp.members {
                cx.kn.seal(dev, grp.epoch);
            }
            let epoch = grp.epoch;
            let n = grp.members.len();
            cx.kn.settle(g, epoch);
            let mut c = OpCost::new(CostClass::Merge, g);
            c.xwing = n as u64;
            let reach = self.reach(cx, g);
            let body = (m.mls_commit_per_device * n as f64) as usize;
            c.event(
                cx.kn,
                m,
                "MLS 커밋(다시 세움)",
                Lane::KeyAgreement,
                body,
                reach,
            );
            let body = (m.mls_welcome_per_device * n as f64) as usize;
            c.event(
                cx.kn,
                m,
                "MLS 웰컴(다시 세움)",
                Lane::KeyAgreement,
                body,
                reach,
            );
            out.push(c);
        }
        out
    }

    fn finish(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.flush(cx)
    }

    fn boxed_clone(&self) -> Box<dyn KeyScheme + Send> {
        Box::new(self.clone())
    }

    fn notes(&self) -> Vec<(String, String)> {
        vec![
            ("MLS 그룹 수".into(), self.groups.len().to_string()),
            ("가장 큰 그룹(기기)".into(), self.max_group.to_string()),
            (
                "크루 그룹(끝)".into(),
                self.groups
                    .get(&CREW)
                    .map_or(0, |g| g.members.len())
                    .to_string(),
            ),
        ]
    }
}
