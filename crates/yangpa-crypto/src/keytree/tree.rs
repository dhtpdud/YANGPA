//! 확정 기기 쪽 트리 배치(칸 → 기기)와 노드 키 표.

use std::collections::{BTreeSet, HashMap};

use yangpa_core::body::Secret32;
use yangpa_core::ids::DeviceId;
use yangpa_core::keyupdate::NodeIdx;

use super::Node;

/// 깊이가 고정된 이진 트리. `levels[l][p]`가 노드 (l, p)의 키이고, 아래에 기기가 하나도 없으면 `None`입니다.
/// (`Clone`은 확정 기기가 받아들여지기 전 상태를 남겨 둘 때 씁니다. 키는 `Secret32`라 복제본도 버릴 때 지워짐.)
#[derive(Clone)]
pub(crate) struct Tree {
    pub depth: u8,
    pub slots: Vec<Option<DeviceId>>,
    pub dev_slot: HashMap<DeviceId, u32>,
    /// 빈 칸(가장 왼쪽부터 채움).
    free: BTreeSet<u32>,
    pub levels: Vec<Vec<Option<Node>>>,
    /// 크루 트리의 잎 원천. 비공개 트리는 `None`(잎이 크루 잎에서 나옴).
    pub leaf_master: Option<Secret32>,
}

/// `n`대를 담는 가장 얕은 깊이(최소 1).
pub(crate) fn depth_for(n: usize) -> u8 {
    let mut d = 1u8;
    while (1usize << d) < n {
        d += 1;
    }
    d
}

impl Tree {
    pub fn new(depth: u8, leaf_master: Option<Secret32>) -> Self {
        let n = 1usize << depth;
        Tree {
            depth,
            slots: vec![None; n],
            dev_slot: HashMap::new(),
            free: (0..n as u32).collect(),
            levels: (0..=depth).map(|l| vec![None; n >> l]).collect(),
            leaf_master,
        }
    }

    /// 저장된 배치·키 표에서 다시 만듭니다(넘겨받은 상태). 모양이 맞지 않으면 `None`.
    pub fn from_parts(
        depth: u8,
        slots: Vec<Option<DeviceId>>,
        levels: Vec<Vec<Option<Node>>>,
        leaf_master: Option<Secret32>,
    ) -> Option<Self> {
        let n = 1usize << depth;
        if slots.len() != n
            || levels.len() != usize::from(depth) + 1
            || levels.iter().enumerate().any(|(l, v)| v.len() != n >> l)
        {
            return None;
        }
        let mut dev_slot = HashMap::new();
        let mut free = BTreeSet::new();
        for (s, d) in slots.iter().enumerate() {
            match d {
                Some(d) => {
                    if dev_slot.insert(*d, s as u32).is_some() || levels[0][s].is_none() {
                        return None;
                    }
                }
                None => {
                    free.insert(s as u32);
                }
            }
        }
        Some(Tree {
            depth,
            slots,
            dev_slot,
            free,
            levels,
            leaf_master,
        })
    }

    pub fn len(&self) -> usize {
        self.dev_slot.len()
    }

    pub fn get(&self, n: NodeIdx) -> Option<&Node> {
        self.levels
            .get(usize::from(n.level))?
            .get(n.pos as usize)?
            .as_ref()
    }

    pub fn present(&self, n: NodeIdx) -> bool {
        self.get(n).is_some()
    }

    pub fn set(&mut self, n: NodeIdx, node: Option<Node>) {
        self.levels[usize::from(n.level)][n.pos as usize] = node;
    }

    pub fn root(&self) -> NodeIdx {
        NodeIdx::root(self.depth)
    }

    /// 위에 뿌리를 하나 얹습니다. 옛 뿌리가 새 뿌리의 왼쪽 자식이 되고 노드 번호는 그대로입니다.
    fn grow(&mut self) {
        let old = 1usize << self.depth;
        self.depth += 1;
        let n = old << 1;
        self.slots.resize(n, None);
        for (l, v) in self.levels.iter_mut().enumerate() {
            // `resize`로 늘리면 키가 새 버퍼로 비트 복사되고 옛 버퍼는 지워지지 않은 채 풀립니다. 새 버퍼에 복제한 뒤
            // 옛 것을 버려 키마다 0으로 덮이게 합니다.
            let mut grown = Vec::with_capacity(n >> l);
            grown.extend(v.iter().cloned());
            grown.resize(n >> l, None);
            *v = grown;
        }
        self.levels.push(vec![None]);
        self.free.extend(old as u32..n as u32);
    }

    /// 넘겨받은 표의 모양 검사: 잎은 칸에 기기가 있을 때만, 내부 노드는 아래에 기기가 있을 때만 키가 있어야 합니다.
    pub fn well_formed(&self) -> bool {
        let leaves_ok = self
            .slots
            .iter()
            .zip(&self.levels[0])
            .all(|(d, k)| d.is_some() == k.is_some());
        leaves_ok
            && (1..=self.depth).all(|l| {
                let below = &self.levels[usize::from(l) - 1];
                self.levels[usize::from(l)]
                    .iter()
                    .enumerate()
                    .all(|(p, k)| {
                        k.is_some() == (below[2 * p].is_some() || below[2 * p + 1].is_some())
                    })
            })
    }

    /// 가장 왼쪽 빈 칸(없으면 트리를 키움).
    pub fn take_slot(&mut self) -> u32 {
        if self.free.is_empty() {
            self.grow();
        }
        self.free.pop_first().expect("키운 뒤에는 빈 칸이 있음")
    }

    /// `near` 칸과 같은 작은 블록(2·4·8칸) 안의 빈 칸, 없으면 가장 왼쪽 빈 칸. 한 사용자의 기기를 이웃 칸에 둡니다.
    pub fn take_slot_near(&mut self, near: Option<u32>) -> u32 {
        if let Some(s) = near {
            for k in 1..=3u32 {
                let start = s & !((1u32 << k) - 1);
                let end = start.saturating_add(1 << k);
                if let Some(&f) = self.free.range(start..end).next() {
                    self.free.remove(&f);
                    return f;
                }
            }
        }
        self.take_slot()
    }

    pub fn place(&mut self, device: DeviceId, slot: u32, leaf: Node) {
        self.free.remove(&slot);
        self.slots[slot as usize] = Some(device);
        self.dev_slot.insert(device, slot);
        self.levels[0][slot as usize] = Some(leaf);
    }

    /// 기기를 빼고 칸을 비웁니다(노드 키는 부르는 쪽이 다시 정함).
    pub fn clear(&mut self, device: &DeviceId) -> Option<u32> {
        let slot = self.dev_slot.remove(device)?;
        self.slots[slot as usize] = None;
        self.levels[0][slot as usize] = None;
        self.free.insert(slot);
        Some(slot)
    }

    /// 칸 `slot`의 높이 1..=깊이 경로 키.
    pub fn path(&self, slot: u32) -> Vec<Secret32> {
        (1..=self.depth)
            .map(|l| {
                self.get(NodeIdx::ancestor(slot, l))
                    .expect("기기가 있는 칸의 조상에는 키가 있음")
                    .key
                    .clone()
            })
            .collect()
    }

    /// 칸 순서대로의 기기.
    pub fn devices(&self) -> impl Iterator<Item = (u32, &DeviceId)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(s, d)| d.as_ref().map(|d| (s as u32, d)))
    }
}
