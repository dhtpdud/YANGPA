//! 해소기 DAG를 따라가는 조상 검사(크루 키 갱신의 가입 기기, 키 그룹 사슬의 기준점).

use std::collections::HashSet;

use yangpa_core::ids::EventId;

/// 조상 검사 한 번이 따라가는 마디 수 상한. 넘으면 '닿지 않음'으로 끝내 서명된 이벤트 하나가 시키는 일을 묶습니다.
/// 답은 DAG 모양만의 함수라 같은 항목을 가진 노드에서는 같습니다.
pub(crate) const MAX_ANCESTRY_VISITS: usize = 1 << 20;

/// 조상 검사가 보는 DAG: ID → (논리 시계, 부모).
pub(crate) trait Dag {
    fn node(&self, id: &EventId) -> Option<(u64, &[EventId])>;
}

/// 아무 마디도 모르는 DAG(시험용).
#[cfg(test)]
pub(crate) struct NoDag;

#[cfg(test)]
impl Dag for NoDag {
    fn node(&self, _: &EventId) -> Option<(u64, &[EventId])> {
        None
    }
}

/// `targets`가 모두 `from` 가운데 하나이거나 그 조상인지. 부모의 논리 시계는 자식보다 작으므로 가장 작은 목표보다
/// 논리 시계가 작거나 같은 마디 위로는 따라가지 않습니다(목표가 그 조상일 수 없음). 모르는 목표가 있으면 거짓입니다.
pub(crate) fn reaches(dag: &dyn Dag, from: &[EventId], targets: &[EventId]) -> bool {
    let mut left: HashSet<EventId> = targets.iter().copied().collect();
    for f in from {
        left.remove(f);
    }
    if left.is_empty() {
        return true;
    }
    let mut floor = u64::MAX;
    for t in &left {
        match dag.node(t) {
            Some((l, _)) => floor = floor.min(l),
            None => return false,
        }
    }
    let mut seen: HashSet<EventId> = from.iter().copied().collect();
    let mut stack: Vec<EventId> = from.to_vec();
    let mut visits = 0usize;
    while let Some(id) = stack.pop() {
        let Some((_, parents)) = dag.node(&id) else {
            continue;
        };
        visits += 1;
        if visits > MAX_ANCESTRY_VISITS {
            return false;
        }
        for p in parents {
            if left.remove(p) && left.is_empty() {
                return true;
            }
            if seen.insert(*p) && dag.node(p).is_some_and(|(l, _)| l > floor) {
                stack.push(*p);
            }
        }
    }
    false
}

/// `targets` 가운데 `from`(또는 그 조상)인 것들. [`reaches`]와 같이 한 번만 따라가며, 상한을 넘으면 그때까지 찾은
/// 것만 돌려줍니다(덜 찾는 쪽이라 안전: 찾지 못한 목표는 다음에 다시 봄).
pub(crate) fn reached(dag: &dyn Dag, from: &[EventId], targets: &[EventId]) -> HashSet<EventId> {
    let mut left: HashSet<EventId> = targets.iter().copied().collect();
    let mut found: HashSet<EventId> = HashSet::new();
    for f in from {
        if left.remove(f) {
            found.insert(*f);
        }
    }
    let mut floor = u64::MAX;
    left.retain(|t| match dag.node(t) {
        Some((l, _)) => {
            floor = floor.min(l);
            true
        }
        None => false,
    });
    if left.is_empty() {
        return found;
    }
    let mut seen: HashSet<EventId> = from.iter().copied().collect();
    let mut stack: Vec<EventId> = from.to_vec();
    let mut visits = 0usize;
    while let Some(id) = stack.pop() {
        let Some((_, parents)) = dag.node(&id) else {
            continue;
        };
        visits += 1;
        if visits > MAX_ANCESTRY_VISITS {
            break;
        }
        for p in parents {
            if left.remove(p) {
                found.insert(*p);
                if left.is_empty() {
                    return found;
                }
            }
            if seen.insert(*p) && dag.node(p).is_some_and(|(l, _)| l > floor) {
                stack.push(*p);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Map(HashMap<EventId, (u64, Vec<EventId>)>);

    impl Dag for Map {
        fn node(&self, id: &EventId) -> Option<(u64, &[EventId])> {
            self.0.get(id).map(|(l, p)| (*l, p.as_slice()))
        }
    }

    fn id(n: u8) -> EventId {
        EventId([n; 32])
    }

    #[test]
    fn reaches_follows_parents_and_prunes_by_lamport() {
        // 1 ← 2 ← 4, 1 ← 3 (3과 4는 동시)
        let mut m = HashMap::new();
        m.insert(id(1), (1, vec![]));
        m.insert(id(2), (2, vec![id(1)]));
        m.insert(id(3), (2, vec![id(1)]));
        m.insert(id(4), (3, vec![id(2)]));
        let d = Map(m);
        assert!(reaches(&d, &[id(4)], &[id(1), id(2)]));
        assert!(reaches(&d, &[id(4)], &[id(4)]));
        assert!(
            !reaches(&d, &[id(4)], &[id(3)]),
            "동시 이벤트는 조상이 아님"
        );
        assert!(reaches(&d, &[id(4), id(3)], &[id(3), id(1)]));
        assert!(!reaches(&d, &[id(4)], &[id(9)]), "모르는 목표");
        assert!(!reaches(&NoDag, &[id(4)], &[id(1)]));
    }

    #[test]
    fn reached_returns_only_ancestors() {
        // 1 ← 2 ← 4, 1 ← 3 (3과 4는 동시)
        let mut m = HashMap::new();
        m.insert(id(1), (1, vec![]));
        m.insert(id(2), (2, vec![id(1)]));
        m.insert(id(3), (2, vec![id(1)]));
        m.insert(id(4), (3, vec![id(2)]));
        let d = Map(m);
        let got = reached(&d, &[id(4)], &[id(1), id(3), id(4), id(9)]);
        assert_eq!(got, HashSet::from([id(1), id(4)]));
        assert!(reached(&NoDag, &[id(4)], &[id(1)]).is_empty());
    }
}
