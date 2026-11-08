//! 스트림별 이벤트 DAG 색인.
//!
//! - 머리(heads): 아무 이벤트도 부모로 가리키지 않은 이벤트. 새 이벤트의 부모가 됩니다.
//! - 빠진 부모: 알고 있는 이벤트가 가리키지만 아직 받지 않은 이벤트. 오류가 아니라 '받지 않은 기록'
//!   구간이며, 나중에 다른 시더에게서 채울 수 있습니다.
//!
//! 저장소(`yangpa-store`)에 같은 정보가 있으므로 데몬은 디스크 등급 스트림에는 저장소 조회와 이 모듈의
//! 순수 함수([`choose_parents`], [`next_lamport`])를 쓰고, RAM 전용(휘발성) 스트림과 시뮬레이션에는
//! [`StreamDag`]·[`SpaceDag`]를 그대로 씁니다.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use thiserror::Error;
use yangpa_core::event::{MAX_PARENTS, StreamId};
use yangpa_core::ids::EventId;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum DagError {
    #[error("부모가 너무 많음")]
    TooManyParents,
    #[error("자기 자신을 부모로 가리킴")]
    SelfParent,
    #[error("논리 시계가 부모({0:?})보다 크지 않음")]
    LamportNotIncreasing(EventId),
}

/// 삽입 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inserted {
    New {
        /// 삽입 뒤의 머리(정렬됨).
        new_heads: Vec<EventId>,
        /// 이 이벤트가 이전에 '받지 않은 기록'이던 자리를 채웠는지.
        filled_gap: bool,
        /// 이 이벤트의 부모 중 아직 없는 것(새로 생긴 빈 구간).
        missing_parents: Vec<EventId>,
    },
    Duplicate,
}

/// 화면에 표시할 '받지 않은 기록' 구간: `after`(알고 있는 앞 이벤트, 없으면 처음부터)와
/// `before`(빠진 부모를 가리킨 가장 이른 이벤트) 사이.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    pub after: Option<EventId>,
    pub before: EventId,
    /// 이 구간에서 빠진 부모 ID(정렬됨).
    pub missing: Vec<EventId>,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    lamport: u64,
}

/// 스트림 하나의 DAG 색인.
#[derive(Clone, Debug, Default)]
pub struct StreamDag {
    nodes: HashMap<EventId, Node>,
    /// (논리 시계, ID) 순서. 구간 조회와 빈 구간 위치 계산에 씁니다.
    order: BTreeSet<(u64, EventId)>,
    heads: BTreeSet<EventId>,
    /// 빠진 부모 → 그것을 가리킨 알려진 자식들 (논리 시계, ID).
    missing: BTreeMap<EventId, BTreeSet<(u64, EventId)>>,
    max_lamport: u64,
}

impl StreamDag {
    pub fn new() -> Self {
        Self::default()
    }

    /// 이벤트를 추가합니다. 이미 있으면 [`Inserted::Duplicate`].
    ///
    /// 이미 알고 있는 부모보다 논리 시계가 크지 않으면 거부합니다. 나중에 도착한 부모와의 관계는
    /// 검사하지 않습니다(그 부모 자체는 정상일 수 있으므로).
    pub fn insert(
        &mut self,
        id: EventId,
        parents: &[EventId],
        lamport: u64,
    ) -> Result<Inserted, DagError> {
        if self.nodes.contains_key(&id) {
            return Ok(Inserted::Duplicate);
        }
        if parents.len() > MAX_PARENTS {
            return Err(DagError::TooManyParents);
        }
        let mut ps = parents.to_vec();
        ps.sort_unstable();
        ps.dedup();
        for p in &ps {
            if *p == id {
                return Err(DagError::SelfParent);
            }
            if let Some(n) = self.nodes.get(p)
                && n.lamport >= lamport
            {
                return Err(DagError::LamportNotIncreasing(*p));
            }
        }

        let filled_gap = self.missing.remove(&id).is_some();
        self.nodes.insert(id, Node { lamport });
        self.order.insert((lamport, id));
        if !filled_gap {
            self.heads.insert(id);
        }
        let mut missing_parents = Vec::new();
        for p in ps {
            if self.nodes.contains_key(&p) {
                self.heads.remove(&p);
            } else {
                self.missing.entry(p).or_default().insert((lamport, id));
                missing_parents.push(p);
            }
        }
        self.max_lamport = self.max_lamport.max(lamport);
        Ok(Inserted::New {
            new_heads: self.heads(),
            filled_gap,
            missing_parents,
        })
    }

    pub fn contains(&self, id: &EventId) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn lamport_of(&self, id: &EventId) -> Option<u64> {
        self.nodes.get(id).map(|n| n.lamport)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 이 스트림에서 본 가장 큰 논리 시계(없으면 0).
    pub fn max_lamport(&self) -> u64 {
        self.max_lamport
    }

    /// 이 스트림만 보고 정한 다음 논리 시계. 공간 전체 기준은 [`SpaceDag::next_lamport`].
    pub fn next_lamport(&self) -> u64 {
        next_lamport(self.max_lamport)
    }

    /// 머리(정렬됨).
    pub fn heads(&self) -> Vec<EventId> {
        self.heads.iter().copied().collect()
    }

    /// 머리와 논리 시계.
    pub fn heads_with_lamport(&self) -> Vec<(EventId, u64)> {
        self.heads
            .iter()
            .filter_map(|h| self.nodes.get(h).map(|n| (*h, n.lamport)))
            .collect()
    }

    /// 새 이벤트의 부모: 머리 전부, 많으면 논리 시계가 큰 순으로 최대 16개(정렬됨).
    pub fn choose_parents(&self) -> Vec<EventId> {
        choose_parents(&self.heads_with_lamport())
    }

    /// 빠진 부모(정렬됨).
    pub fn missing(&self) -> Vec<EventId> {
        self.missing.keys().copied().collect()
    }

    pub fn has_gaps(&self) -> bool {
        !self.missing.is_empty()
    }

    /// 화면에 표시할 빈 구간(앞에서부터). 같은 자리의 빠진 부모 여럿은 한 구간으로 묶습니다.
    pub fn gaps(&self) -> Vec<Gap> {
        // 가장 이른 자식 위치별로 빠진 부모를 모읍니다.
        let mut by_child: BTreeMap<(u64, EventId), Vec<EventId>> = BTreeMap::new();
        for (m, children) in &self.missing {
            if let Some(first) = children.first() {
                by_child.entry(*first).or_default().push(*m);
            }
        }
        let mut out: Vec<Gap> = Vec::with_capacity(by_child.len());
        for ((lamport, before), mut missing) in by_child {
            // 빠진 부모의 논리 시계는 자식보다 작으므로, 자식보다 확실히 앞선 마지막 이벤트가 구간 시작입니다.
            let after = self
                .order
                .range(..(lamport, EventId([0; 32])))
                .next_back()
                .map(|(_, id)| *id);
            missing.sort_unstable();
            if let Some(last) = out.last_mut()
                && last.after == after
            {
                // 같은 구간: 더 이른 `before`를 유지하고 빠진 부모를 합칩니다.
                last.missing.extend(missing);
                last.missing.sort_unstable();
                last.missing.dedup();
                continue;
            }
            out.push(Gap {
                after,
                before,
                missing,
            });
        }
        out
    }

    /// 구간 [start, end) 안의 (논리 시계, ID)를 순서대로.
    pub fn range(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, EventId)> + '_ {
        // BTreeSet::range는 시작 > 끝이면 패닉하므로 잘못된 구간은 빈 구간으로 바꿉니다.
        let end = end.max(start);
        self.order
            .range((start, EventId([0; 32]))..(end, EventId([0; 32])))
            .copied()
    }
}

/// 공간 하나의 모든 스트림과 공간 전체 논리 시계.
#[derive(Clone, Debug, Default)]
pub struct SpaceDag {
    streams: BTreeMap<StreamId, StreamDag>,
    max_lamport: u64,
}

impl SpaceDag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        stream: StreamId,
        id: EventId,
        parents: &[EventId],
        lamport: u64,
    ) -> Result<Inserted, DagError> {
        let r = self
            .streams
            .entry(stream)
            .or_default()
            .insert(id, parents, lamport)?;
        if matches!(r, Inserted::New { .. }) {
            self.max_lamport = self.max_lamport.max(lamport);
        }
        Ok(r)
    }

    /// 상대가 알려 준 머리의 논리 시계 등, 이벤트 없이 알게 된 시계 값을 반영합니다.
    pub fn observe_lamport(&mut self, lamport: u64) {
        self.max_lamport = self.max_lamport.max(lamport);
    }

    pub fn stream(&self, stream: &StreamId) -> Option<&StreamDag> {
        self.streams.get(stream)
    }

    pub fn streams(&self) -> impl Iterator<Item = (&StreamId, &StreamDag)> {
        self.streams.iter()
    }

    pub fn remove_stream(&mut self, stream: &StreamId) -> Option<StreamDag> {
        self.streams.remove(stream)
    }

    pub fn max_lamport(&self) -> u64 {
        self.max_lamport
    }

    /// 공간 전체 최댓값 + 1.
    pub fn next_lamport(&self) -> u64 {
        next_lamport(self.max_lamport)
    }

    pub fn choose_parents(&self, stream: &StreamId) -> Vec<EventId> {
        self.streams
            .get(stream)
            .map(StreamDag::choose_parents)
            .unwrap_or_default()
    }

    pub fn contains(&self, stream: &StreamId, id: &EventId) -> bool {
        self.streams.get(stream).is_some_and(|s| s.contains(id))
    }
}

/// 머리에서 새 이벤트의 부모를 고릅니다: 논리 시계가 큰 순(같으면 ID 작은 순)으로 최대
/// [`MAX_PARENTS`]개, 결과는 헤더 규칙대로 ID 오름차순.
pub fn choose_parents(heads: &[(EventId, u64)]) -> Vec<EventId> {
    let mut hs = heads.to_vec();
    hs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    hs.dedup_by_key(|h| h.0);
    let mut out: Vec<EventId> = hs.into_iter().take(MAX_PARENTS).map(|(id, _)| id).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// 공간 전체 최댓값 + 1(넘침 없이).
pub fn next_lamport(space_max: u64) -> u64 {
    space_max.saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::ids::ChannelId;

    fn id(n: u32) -> EventId {
        EventId::hash_of(&n.to_le_bytes())
    }

    #[test]
    fn linear_chain_has_single_head() {
        let mut d = StreamDag::new();
        let r = d.insert(id(1), &[], 1).unwrap();
        assert_eq!(
            r,
            Inserted::New {
                new_heads: vec![id(1)],
                filled_gap: false,
                missing_parents: vec![]
            }
        );
        d.insert(id(2), &[id(1)], 2).unwrap();
        d.insert(id(3), &[id(2)], 3).unwrap();
        assert_eq!(d.heads(), vec![id(3)]);
        assert_eq!(d.choose_parents(), vec![id(3)]);
        assert_eq!(d.next_lamport(), 4);
        assert!(!d.has_gaps());
        assert_eq!(d.insert(id(2), &[id(1)], 2).unwrap(), Inserted::Duplicate);
        assert_eq!(d.len(), 3);
    }

    #[test]
    fn concurrent_branches_merge() {
        let mut d = StreamDag::new();
        d.insert(id(1), &[], 1).unwrap();
        d.insert(id(2), &[id(1)], 2).unwrap();
        d.insert(id(3), &[id(1)], 2).unwrap();
        let mut heads = vec![id(2), id(3)];
        heads.sort();
        assert_eq!(d.heads(), heads);
        assert_eq!(d.choose_parents(), heads);
        d.insert(id(4), &heads, 3).unwrap();
        assert_eq!(d.heads(), vec![id(4)]);
    }

    #[test]
    fn rejects_bad_lamport_and_self_parent() {
        let mut d = StreamDag::new();
        d.insert(id(1), &[], 5).unwrap();
        assert_eq!(
            d.insert(id(2), &[id(1)], 5),
            Err(DagError::LamportNotIncreasing(id(1)))
        );
        assert_eq!(d.insert(id(3), &[id(3)], 9), Err(DagError::SelfParent));
        let many: Vec<EventId> = (100..117).map(id).collect();
        assert_eq!(d.insert(id(4), &many, 9), Err(DagError::TooManyParents));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn missing_parents_are_gaps_and_can_be_filled() {
        let mut d = StreamDag::new();
        d.insert(id(1), &[], 1).unwrap();
        d.insert(id(2), &[id(1)], 2).unwrap();
        // id(3)을 받지 못한 채 id(4)가 옴.
        let r = d.insert(id(4), &[id(3)], 4).unwrap();
        assert_eq!(
            r,
            Inserted::New {
                new_heads: {
                    let mut h = vec![id(2), id(4)];
                    h.sort();
                    h
                },
                filled_gap: false,
                missing_parents: vec![id(3)]
            }
        );
        assert_eq!(d.missing(), vec![id(3)]);
        let gaps = d.gaps();
        assert_eq!(
            gaps,
            vec![Gap {
                after: Some(id(2)),
                before: id(4),
                missing: vec![id(3)]
            }]
        );

        // 채우면 머리에서 빠지고 빈 구간이 사라짐.
        let r = d.insert(id(3), &[id(2)], 3).unwrap();
        assert!(matches!(
            r,
            Inserted::New {
                filled_gap: true,
                ..
            }
        ));
        assert_eq!(d.heads(), vec![id(4)]);
        assert!(d.gaps().is_empty());
    }

    #[test]
    fn gap_at_start_has_no_after() {
        let mut d = StreamDag::new();
        // 입장 이전 기록을 받지 않은 경우: 처음 받은 이벤트의 부모가 없음.
        d.insert(id(10), &[id(9)], 10).unwrap();
        d.insert(id(11), &[id(10)], 11).unwrap();
        assert_eq!(
            d.gaps(),
            vec![Gap {
                after: None,
                before: id(10),
                missing: vec![id(9)]
            }]
        );
    }

    #[test]
    fn gaps_at_same_position_merge() {
        let mut d = StreamDag::new();
        d.insert(id(1), &[], 1).unwrap();
        let mut two = vec![id(50), id(51)];
        two.sort();
        d.insert(id(5), &two, 5).unwrap();
        // 같은 논리 시계의 다른 자식: 둘 다 id(1) 뒤라서 하나로 묶임.
        d.insert(id(7), &[id(52)], 5).unwrap();
        // 더 뒤의 자식: id(5)/id(7) 다음의 별도 구간.
        d.insert(id(6), &[id(53)], 6).unwrap();
        let gaps = d.gaps();
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert_eq!(gaps[0].after, Some(id(1)));
        assert_eq!(gaps[0].before, id(5).min(id(7)));
        let mut all = vec![id(50), id(51), id(52)];
        all.sort();
        assert_eq!(gaps[0].missing, all);
        assert_eq!(gaps[1].after, Some(id(5).max(id(7))));
        assert_eq!(gaps[1].before, id(6));
        assert_eq!(gaps[1].missing, vec![id(53)]);
    }

    #[test]
    fn choose_parents_caps_at_sixteen_highest_lamport() {
        let heads: Vec<(EventId, u64)> = (0..20u32).map(|i| (id(i), u64::from(i))).collect();
        let chosen = choose_parents(&heads);
        assert_eq!(chosen.len(), MAX_PARENTS);
        for i in 4..20u32 {
            assert!(chosen.contains(&id(i)));
        }
        assert!(chosen.windows(2).all(|w| w[0] < w[1]));

        let mut d = StreamDag::new();
        for i in 0..20u32 {
            d.insert(id(i), &[], u64::from(i) + 1).unwrap();
        }
        assert_eq!(d.choose_parents().len(), MAX_PARENTS);
    }

    #[test]
    fn space_dag_uses_space_wide_clock() {
        let mut s = SpaceDag::new();
        let a = StreamId::Channel(ChannelId([1; 16]));
        let b = StreamId::Channel(ChannelId([2; 16]));
        s.insert(a, id(1), &[], 1).unwrap();
        s.insert(a, id(2), &[id(1)], 7).unwrap();
        s.insert(b, id(3), &[], 2).unwrap();
        // 채널 b의 새 이벤트도 공간 전체 최댓값 + 1.
        assert_eq!(s.next_lamport(), 8);
        assert_eq!(s.choose_parents(&b), vec![id(3)]);
        assert!(s.choose_parents(&StreamId::Space).is_empty());
        s.observe_lamport(100);
        assert_eq!(s.next_lamport(), 101);
        assert!(s.contains(&a, &id(2)));
        assert!(!s.contains(&b, &id(2)));
    }

    #[test]
    fn range_iterates_in_order() {
        let mut d = StreamDag::new();
        for i in 1..=10u32 {
            d.insert(id(i), &[], u64::from(i)).unwrap();
        }
        let got: Vec<u64> = d.range(3, 6).map(|(l, _)| l).collect();
        assert_eq!(got, vec![3, 4, 5]);
        assert_eq!(d.range(6, 3).count(), 0);
    }
}
