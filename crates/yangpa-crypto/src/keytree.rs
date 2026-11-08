//! 키 그룹(`KeyUpdate`)의 암호 계산: 확정 기기 쪽 [`KeyTree`]·[`CoverGroup`]과 멤버 쪽 [`MemberKeys`].
//!
//! 전송 형식은 [`yangpa_core::keyupdate`], 키 일정(T·R에서 나오는 키)은 [`crate::keysched`]에 있습니다.
//!
//! # 인코딩
//!
//! - **직접(깊이 0)**: 기기 [`DIRECT_MAX`](=256)대 이하. 제거·시간 회전이면 새 T를 남은 기기 모두에게 봉인 상자 하나
//!   (수신자 여럿, X-Wing)로 보내고, 가입이면 T' = [`crate::keysched::advance`]\(T)를 가입 기기에만 봉인합니다.
//! - **트리**: 256대 초과(그러다 [`DIRECT_BACK`](=160)대 이하가 되면 직접으로). 깊이 D가 고정된 이진 트리(칸 2^D개)에
//!   확정 기기가 가장 왼쪽 빈 칸으로 넣고, 가득 차면 위에 뿌리를 하나 얹습니다(감싸기 1개). 노드 키는 새 무작위 값이고
//!   노드마다 판(그 키를 정한 `seq`)이 있습니다. 뿌리 키가 T입니다.
//!   - 크루 잎 L_d = derive_key("yangpa/v3/lkh-leaf", leaf_master ‖ 그룹 ‖ 기기 ‖ 세대). 가입 기기는 잎과 경로를 X-Wing으로
//!     한 번 받습니다([`DeviceBundle`], Inbox).
//!   - 비공개 그룹 잎 = derive_key("yangpa/v3/lkh-private-leaf", 크루 잎 ‖ 그룹 ‖ 크루 세대). 비공개 그룹에 넣을 때 X-Wing이
//!     필요 없고, 경로는 잎으로 감싸 페이로드에 싣습니다. 크루가 직접 인코딩이면 비공개 그룹도 직접입니다.
//!   - "이 몇 기기만 빼고 크루 전체" 그룹은 자기 트리 없이 크루 트리 위의 제외 덮개([`exclusion_cover`])에 그룹 키를
//!     감쌉니다([`CoverGroup`]).
//!
//! # 갱신
//!
//! | 작업 | 종류 | 트리 | 직접 |
//! | --- | --- | --- | --- |
//! | 가입 | `Advance{joined}` | 가입 경로의 노드를 한 방향 전진(KDF(노드 키, 그룹 ‖ seq ‖ 노드 ‖ 가입 해시)), 비어 있던 노드는 새로 | T' = KDF(T), 가입 기기에만 봉인 |
//! | 제거(+같은 묶음의 가입) | `Rotate{removed, joined}` | 제거 경로를 새로 뽑아 남은 자식 키로 감쌈(빈 하위 트리는 건너뜀) | 새 T를 남은 기기마다 |
//! | 시간 회전 | `Refresh` | 새 뿌리를 두 자식 키로 감쌈 | 새 T를 모두에게 |
//! | 전체 재키 | `FullRekey{generation, removed, joined}` | 새 leaf_master, 모든 노드 새로, 기기마다 잎 + 경로 봉인 | 새 T를 남은 기기 모두에게(깊이 0, 세대 그대로) |
//! | R만 | `RRefresh` | — | — |
//!
//! 가입·제거 목록은 앞 갱신 대비 바뀐 기기만 싣습니다(`FullRekey`도). 해소기는 이 목록만으로 그룹 멤버를 따라가며, 목록이
//! 없는 예외는 덮개 그룹의 첫 `Refresh`(멤버 = 크루 − 제외)뿐입니다. 전체 재키는 직접 인코딩에서도 `FullRekey`라서
//! 해소기가 표 보유 기기를 비우는 재키를 같은 모양의 보통 회전과 가릴 수 있습니다.
//!
//! R은 T와 따로 새로 뽑아 `r_ct`(새 T에서 나온 키로 암호화)에 싣습니다. 가입만 있는 갱신은 R이 그대로이고, 가입자는 R을
//! 봉인 묶음에서(크루·직접) 또는 같은 R을 다시 암호화한 `r_ct`에서(비공개 트리·덮개) 받습니다. 뿌리 약속
//! `root_commit`이 T와 R을 함께 묶으므로([`crate::keysched::root_commit`]) 어느 길로 받았든 그룹 모두와 같은 값인지
//! 확인됩니다.
//!
//! **감싸기**(56B = 표지 8 + 암호문 32 + 태그 16): XChaCha20-Poly1305(키 = KDF(감싸는 키 ‖ AD ‖ salt), 고정 논스,
//! 평문 = 32바이트 키). 트리 감싸기는 AD = "yangpa/v3/lkh-wrap" ‖ 공간 ‖ 그룹 ‖ seq ‖ 노드 ‖ 판(노드 = 감싸이는 노드,
//! 판 = seq), 덮개 감싸기는 "yangpa/v3/lkh-cover" ‖ … ‖ 감싸는 크루 노드 ‖ 그 노드의 판이고, 비공개·덮개 그룹은 기대는
//! 크루 상태(`CrewRef`)도 붙습니다. 다른 자리·판·갱신으로 옮기면 열리지 않습니다. 표지 = KDF(감싸는 키, 그룹 ‖ seq ‖
//! 노드)의 앞 8바이트라 받는 쪽은 배치를 몰라도 자기 키로 찾고, 갱신마다 달라 이어 볼 수 없습니다(겹치면 모두 열어
//! 봄). 감싸기마다 무작위 논스(24바이트)를 싣는 대신 갱신마다 무작위 `salt` 하나를 키에 섞어 80B를 56B로 줄였습니다
//! (같은 순번의 두 갈래가 같은 키로 다른 값을 감싸도 salt가 달라 키가 다름).
//!
//! **기기 몫 묶음**([`DeviceBundle`]): 크루 트리 가입자와 크루 전체 재키의 잎·경로·R은 페이로드 밖(Inbox, 사용자별
//! 블롭 조각, 멤버 중계)으로 갑니다. 봉투는 익명 X-Wing이라 누가 만들었는지 모르므로, 페이로드의 `bundle_root`(봉투
//! 해시들의 머클 뿌리, 서명된 이벤트 안)와 묶음의 증명이 맞을 때만 엽니다. 그래서 다른 멤버가 심은 묶음이나 같은
//! 순번 다른 갈래의 묶음은 열기 전에 [`KeyUpdateError::BadBundle`]로 걸러지고, 어느 길로 받아도 됩니다. 봉투 AD도
//! (공간, 그룹, seq, 기기, `root_commit`)을 묶습니다.
//!
//! # 확정 기기 쪽 사용(데몬)
//!
//! ```text
//! let cx = CommitCtx { boxes: &디렉터리, crew: None };
//! let mut crew = KeyTree::crew(space);
//! crew.add_holders(해소기가_센_표_보유_기기);                 // 이 기기 말고 표를 가진 기기(넘겨준 옛 확정 기기·예비 기기)
//! let mut next = crew.clone();                                 // 받아들여질 때까지 옛 상태를 남겨 둠
//! let out = next.update(&removed, &joined, &cx)?;              // 제거가 있으면 가입도 같은 Rotate에
//! for part in out.payload.clone().split(PART_TARGET_LEN)? { /* KeyUpdate 이벤트로 발행(본문은 갱신 전 상태 키로 암호화) */ }
//! next.set_last_event(첫 조각 이벤트 ID);
//! for (user, bundles) in by_user(out.bundles, |d| 디렉터리.user_of(d)) { /* 가입자 Inbox, 전체 재키는 사용자별 블롭 조각 */ }
//! crew = next;                                                 // 해소기가 받아들인 뒤에만(진 갈래·발행 실패면 버림)
//! // 같은 묶음에서 크루 다음에(크루 잎·크루 노드·크루 순번을 씀):
//! let cx = CommitCtx { boxes: &디렉터리, crew: Some(&crew) };
//! private.update(&removed_p, &joined_p, &cx)?;                // 크루가 전체 재키했으면 바뀐 기기가 없어도 새 T
//! cover.update(&excluded, &cx)?;                               // 바뀐 것이 없으면 None
//! ```
//!
//! - 작업은 상태를 제자리에서 바꾸므로, 발행한 갱신이 받아들여질 때까지는 작업 전에 만든 복제본([`KeyTree`]·
//!   [`CoverGroup`]은 `Clone`)을 남겨 두고, 진 갈래·발행 실패면 그 복제본으로 돌아갑니다. 새 T·R로 보내는 것도
//!   받아들여진 뒤부터입니다.
//! - **표 보유 기기**(이 기기 말고 leaf_master·노드 키 표를 가진 기기): [`seal_handover`]·[`open_handover`]가 스스로
//!   기록하고, 해소기가 아는 나머지는 [`KeyTree::add_holders`]로 알려 줍니다. 보유 기기를 크루에서 빼는 것은
//!   `crew.full_rekey`로만 되고([`KeyTree::update`]는 [`KeyUpdateError::HolderNeedsFullRekey`]), 크루에 남은 보유 기기를
//!   트리 인코딩 비공개 그룹이나 (크루가 트리일 때) 덮개 그룹에서 빼는 것도 크루 전체 재키가 먼저입니다(보유 기기는
//!   leaf_master로 모든 크루 잎·노드를 계산하므로). 크루 전체 재키는 보유 기기 목록을 비우고 "재키 판"을 올려, 같은
//!   공간의 모든 비공개·덮개 그룹이 다음 작업(바뀐 기기가 없는 `update`, `refresh_r` 포함)에서 새 T를 냅니다(넘긴 표에
//!   직접 인코딩 그룹의 T도 들어 있으므로). 그래서 순서는 `crew.full_rekey` → 모든 비공개 그룹 `update` → 모든 덮개 그룹
//!   `update`(한 묶음)입니다.
//! - 확정 기기를 넘길 때 [`seal_handover`]로 표 전체를 서명·봉인해 넘기고, 새 기기는 [`open_handover`]로 보낸 기기의
//!   서명과 표가 스스로 맞는지(약속·트리 모양·잎)를 확인한 뒤, 자기가 적용한 사슬 끝([`KeyTree::tip`])과 같은지 봅니다.
//!   넘겨받지 못했으면 마지막 갱신의 공개 끝([`ChainTip`])과 멤버로 [`KeyTree::recover`]·[`CoverGroup::recover`]한 뒤
//!   전체 재키부터 합니다.
//! - 인코딩 전환은 자동입니다: 직접 → 트리는 `FullRekey`(크루 몫은 기기마다 봉인 묶음), 트리 → 직접은 `Rotate`.
//!
//! # 멤버 쪽 사용
//!
//! ```text
//! let p = KeyUpdatePayload::merge(조각들)?;                         // 조각마다 decode를 거친 것
//! let cx = MemberCtx { box_keys: Some(&상자 키), bundle: 내_묶음.as_ref(), crew: 크루_상태(p.crew) };
//! let next = mine.apply(&p, &cx)?;                                   // 처음이면 MemberKeys::join
//! let s = next.state_key(); let root = next.private_root(&channel);
//! ```
//!
//! [`MemberKeys::apply`]는 새 값을 돌려주므로 부르는 쪽이 seq마다 상태를 보관했다가 이긴 갈래로 옮길 수 있습니다.
//! 순번이 이어지지 않으면 [`KeyUpdateError::SeqGap`], 앞 약속이 다르면 [`KeyUpdateError::ChainMismatch`](다른 갈래),
//! 계산한 T·R이 약속과 다르면 [`KeyUpdateError::RootCommitMismatch`](확정 기기가 기기마다 다른 키를 줬다는 증거)입니다.
//! 비공개·덮개 그룹 페이로드는 기대는 크루 상태(`p.crew` = 크루 순번·약속)를 적으므로, 데몬은 크루 상태도 순번마다
//! 몇 개 보관했다가 그 순번의 것을 `crew`로 넘깁니다(다르면 [`KeyUpdateError::CrewMismatch`]). 페이로드는 해소기가
//! 정한 확정 기기가 서명한 이벤트에서 온 것이어야 합니다(이 모듈은 서명을 보지 않음).

mod committer;
mod cover;
mod member;
mod snapshot;
mod tree;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use yangpa_core::body::Secret32;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{DeviceId, SpaceId, UserId};
use yangpa_core::keyupdate::{CrewRef, KeyGroupId, MAX_DEPTH, NodeIdx, Wrap};
use zeroize::Zeroizing;

pub use committer::{ChainTip, KeyTree};
pub use cover::{CoverGroup, exclusion_cover};
pub use member::{MemberCtx, MemberKeys};
pub use snapshot::{
    SealedHandover, check_private_against, decode_table, encode_table, open_handover, seal_handover,
};
pub use yangpa_core::keyupdate::{DIRECT_BACK, DIRECT_MAX, PART_TARGET_LEN};

use crate::error::CryptoError;
use crate::keysched::{ct_eq, kdf, open_once, seal_once};

/// 키 그룹 종류. 잎을 어디서 얻는지가 다릅니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupKind {
    /// 크루 그룹: 잎은 leaf_master에서, 가입 기기는 X-Wing 봉인으로 받음.
    Crew,
    /// 비공개 그룹(자기 트리): 잎은 크루 잎에서.
    Private,
    /// 제외 덮개 그룹: 자기 트리 없이 크루 트리의 덮개 노드로 그룹 키를 감쌈.
    Cover,
}

/// 기기의 상자 공개키(X-Wing)를 찾는 곳. 데몬은 디렉터리(기기 목록)로 구현합니다.
pub trait BoxKeys {
    fn box_key(&self, device: &DeviceId) -> Option<&BoxPublicKey>;

    /// 기기의 주인 사용자(알면). 트리에 넣을 때 한 사용자의 기기를 이웃 칸에 두어, 그 사용자가 나갈 때 함께 바뀌는
    /// 경로 노드가 겹치게 합니다(제거 갱신의 감싸기 수가 줄어듦).
    fn user_of(&self, _device: &DeviceId) -> Option<UserId> {
        None
    }
}

impl<S: std::hash::BuildHasher> BoxKeys for HashMap<DeviceId, BoxPublicKey, S> {
    fn box_key(&self, device: &DeviceId) -> Option<&BoxPublicKey> {
        self.get(device)
    }
}

impl BoxKeys for BTreeMap<DeviceId, BoxPublicKey> {
    fn box_key(&self, device: &DeviceId) -> Option<&BoxPublicKey> {
        self.get(device)
    }
}

/// 확정 기기 작업의 문맥.
#[derive(Clone, Copy)]
pub struct CommitCtx<'a> {
    pub boxes: &'a dyn BoxKeys,
    /// 비공개·덮개 그룹이면 같은 공간의 크루 그룹(같은 묶음의 크루 갱신을 먼저 적용한 것). 크루 그룹 작업은 `None`.
    pub crew: Option<&'a KeyTree>,
}

/// 기기 하나에게만 가는 봉인 묶음(가입자의 잎·경로, 전체 재키 몫). 페이로드 밖(Inbox·블롭 조각·멤버 중계)으로 보내고,
/// 받는 쪽은 `index`·`proof`로 페이로드의 `bundle_root`와 맞는지 본 뒤에 엽니다.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBundle {
    pub device: DeviceId,
    /// `seal_to` 봉투(수신자 이 기기 하나).
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
    /// 머클 트리 안 자리와 형제 해시(잎에서 뿌리 쪽으로).
    pub index: u32,
    pub proof: Vec<[u8; 32]>,
}

impl std::fmt::Debug for DeviceBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DeviceBundle({:?}, {}B, 증명 {})",
            self.device,
            self.envelope.len(),
            self.proof.len()
        )
    }
}

/// 묶음 하나의 인코딩 상한(봉투 약 1.9KB + 증명 17 × 32B에 넉넉히).
pub const MAX_BUNDLE_LEN: usize = 16 * 1024;
/// 묶음 증명 길이 상한: 잎이 많아야 2^17개(트리 칸 수).
const MAX_PROOF: usize = MAX_DEPTH as usize;

impl DeviceBundle {
    /// 전송 인코딩(postcard).
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("묶음 직렬화는 실패하지 않음")
    }

    /// 받은 묶음을 풉니다(바이트·증명 길이 한도). 증명 확인은 [`MemberKeys`]가 페이로드와 함께 합니다.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BUNDLE_LEN {
            return Err(KeyUpdateError::Malformed("봉인 묶음 크기"));
        }
        let (b, rest): (DeviceBundle, _) =
            postcard::take_from_bytes(bytes).map_err(|_| KeyUpdateError::Malformed("봉인 묶음"))?;
        if !rest.is_empty() || b.proof.len() > MAX_PROOF {
            return Err(KeyUpdateError::Malformed("봉인 묶음"));
        }
        Ok(b)
    }

    /// 이 묶음이 `root`(페이로드의 `bundle_root`)에 든 것인지.
    pub fn verify(&self, root: &[u8; 32]) -> bool {
        if self.proof.len() > MAX_PROOF || u64::from(self.index) >> self.proof.len() != 0 {
            return false;
        }
        let mut h = bundle_leaf(&self.device, &self.envelope);
        let mut i = self.index;
        for s in &self.proof {
            h = if i & 1 == 0 {
                bundle_node(&h, s)
            } else {
                bundle_node(s, &h)
            };
            i >>= 1;
        }
        ct_eq(&h, root)
    }
}

fn bundle_leaf(device: &DeviceId, envelope: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/bundle-leaf");
    h.update(device.as_bytes());
    h.update(envelope);
    *h.finalize().as_bytes()
}

fn bundle_node(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/bundle-node");
    h.update(a);
    h.update(b);
    *h.finalize().as_bytes()
}

/// 봉투들로 머클 트리(잎 수를 2의 거듭제곱까지 0으로 채움)를 만들어 뿌리와 묶음들을 돌려줍니다. 잎·노드 해시의 라벨이
/// 달라 노드를 잎으로 내밀 수 없습니다.
pub(crate) fn bundle_tree(
    items: Vec<(DeviceId, Vec<u8>)>,
) -> (Option<[u8; 32]>, Vec<DeviceBundle>) {
    if items.is_empty() {
        return (None, Vec::new());
    }
    let width = items.len().next_power_of_two();
    let mut level: Vec<[u8; 32]> = items.iter().map(|(d, e)| bundle_leaf(d, e)).collect();
    level.resize(width, [0; 32]);
    let mut levels = vec![level];
    while levels.last().expect("잎 단").len() > 1 {
        let up = levels
            .last()
            .expect("잎 단")
            .chunks(2)
            .map(|c| bundle_node(&c[0], &c[1]))
            .collect();
        levels.push(up);
    }
    let root = levels.last().expect("뿌리")[0];
    let bundles = items
        .into_iter()
        .enumerate()
        .map(|(i, (device, envelope))| DeviceBundle {
            device,
            envelope,
            index: i as u32,
            proof: levels[..levels.len() - 1]
                .iter()
                .enumerate()
                .map(|(l, v)| v[(i >> l) ^ 1])
                .collect(),
        })
        .collect();
    (Some(root), bundles)
}

/// 확정 기기 작업 하나의 결과.
#[derive(Clone, Debug)]
pub struct KeyUpdateOutput {
    /// 그룹 전체가 받는 갱신(합친 것). 발행 전에 [`yangpa_core::keyupdate::KeyUpdatePayload::split`]으로 나눕니다.
    pub payload: yangpa_core::keyupdate::KeyUpdatePayload,
    /// 기기마다의 봉인 묶음(증명 포함).
    pub bundles: Vec<DeviceBundle>,
}

/// 묶음을 사용자별로 모읍니다(전체 재키 몫을 사용자 해시로 나눈 내용 주소 블롭으로 낼 때). 사용자를 모르는 기기는
/// 빠집니다.
pub fn by_user(
    bundles: Vec<DeviceBundle>,
    user_of: impl Fn(&DeviceId) -> Option<UserId>,
) -> BTreeMap<UserId, Vec<DeviceBundle>> {
    let mut out: BTreeMap<UserId, Vec<DeviceBundle>> = BTreeMap::new();
    for b in bundles {
        if let Some(u) = user_of(&b.device) {
            out.entry(u).or_default().push(b);
        }
    }
    out
}

#[derive(Debug, Error)]
pub enum KeyUpdateError {
    #[error("잘못된 KeyUpdate: {0}")]
    Malformed(&'static str),
    #[error("다른 그룹의 KeyUpdate")]
    WrongGroup,
    #[error("KeyUpdate 순번이 이어지지 않음: 지금 {have}, 받은 것 {got}")]
    SeqGap { have: u64, got: u64 },
    /// 앞 갱신의 뿌리 약속이 내 것과 다름: 다른 갈래의 갱신입니다.
    #[error("앞 KeyUpdate의 뿌리 약속이 내 사슬과 다름(다른 갈래)")]
    ChainMismatch,
    /// 계산한 T·R이 `root_commit`과 다름: 확정 기기가 기기마다 다른 키를 줬을 수 있습니다(증거로 남김).
    #[error("뿌리 약속 불일치: 확정 기기가 기기마다 다른 키를 줬을 수 있음")]
    RootCommitMismatch,
    /// 가입 갱신의 `r_ct`가 내 R과 다름(확정 기기가 가입자에게 다른 R을 줌).
    #[error("가입 갱신의 R이 내 R과 다름")]
    RMismatch,
    /// 봉인 묶음이 페이로드의 `bundle_root`에 들어 있지 않음: 다른 갈래의 묶음이거나 누군가 심은 묶음입니다(열지 않음).
    #[error("봉인 묶음이 이 갱신의 것이 아님(증명 불일치)")]
    BadBundle,
    /// 넘겨받은 상태가 다른 크루 상태에 기대는 갱신: 그 순번의 크루 상태로 다시 적용해야 합니다.
    #[error("이 갱신은 크루 순번 {want}의 상태가 필요함(받은 것 {have:?})")]
    CrewMismatch { want: u64, have: Option<u64> },
    #[error("이 기기가 그룹에서 제거됨")]
    Removed,
    #[error("이 기기를 위한 키가 없음(봉인 묶음이나 감싸기를 못 찾음)")]
    NoKeyForMe,
    #[error("그룹에 없는 기기: {0:?}")]
    UnknownDevice(DeviceId),
    #[error("이미 그룹에 있는 기기: {0:?}")]
    AlreadyMember(DeviceId),
    #[error("바뀌는 기기가 없음")]
    NothingToDo,
    #[error("상자 공개키를 모르는 기기: {0:?}")]
    MissingBoxKey(DeviceId),
    #[error("기기 수 한도를 넘음")]
    TooManyDevices,
    #[error("키를 모르는 이어받은 상태: 전체 재키가 먼저")]
    NeedFullRekey,
    /// 표 보유 기기(leaf_master를 가진 기기)를 빼려 함: 크루 전체 재키로만 뺄 수 있습니다.
    #[error("표 보유 기기 {0:?}를 빼려면 크루 전체 재키가 먼저")]
    HolderNeedsFullRekey(DeviceId),
    #[error("같은 공간의 크루 그룹 상태가 필요함")]
    NeedCrewTree,
    #[error("넘겨받은 키 표가 맞지 않음: {0}")]
    BadHandover(&'static str),
    #[error("암호 오류: {0}")]
    Crypto(#[from] CryptoError),
    #[error("형식 오류: {0}")]
    Core(#[from] yangpa_core::CoreError),
}

pub type Result<T> = std::result::Result<T, KeyUpdateError>;

/// 노드 키와 판(그 키를 정한 `seq`. 잎은 세대).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Node {
    pub key: Secret32,
    pub version: u64,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node(v{})", self.version)
    }
}

/// 비밀이 든 값을 정확한 크기로 한 번에 직렬화합니다(버퍼가 자라며 옮겨질 때 옛 버퍼에 평문이 남지 않게).
pub(crate) fn encode_secret<T: Serialize>(v: &T) -> Zeroizing<Vec<u8>> {
    let n = postcard::serialize_with_flavor(v, postcard::ser_flavors::Size::default())
        .expect("직렬화는 실패하지 않음");
    let out = postcard::to_extend(v, Vec::with_capacity(n)).expect("직렬화는 실패하지 않음");
    debug_assert_eq!(out.capacity(), n, "다시 할당하지 않음");
    Zeroizing::new(out)
}

/// 봉인 상자 안의 평문(기기 하나 또는 직접 인코딩 수신자 모두).
#[derive(Serialize, Deserialize)]
pub(crate) enum Bundle {
    Direct {
        t: Secret32,
        /// 갱신에 `r_ct`가 없을 때(가입만)의 지금 R.
        r: Option<Secret32>,
    },
    Tree {
        slot: u32,
        generation: u64,
        leaf: Secret32,
        /// 높이 1..=깊이의 경로 키(마지막이 뿌리 = T).
        path: Vec<Secret32>,
        r: Option<Secret32>,
    },
}

impl Bundle {
    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        encode_secret(self)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let b: Bundle =
            postcard::from_bytes(bytes).map_err(|_| KeyUpdateError::Malformed("봉인 묶음"))?;
        if let Bundle::Tree { path, .. } = &b
            && (path.is_empty() || path.len() > usize::from(MAX_DEPTH))
        {
            return Err(KeyUpdateError::Malformed("봉인 묶음 경로 길이"));
        }
        Ok(b)
    }
}

/// 직접 인코딩 봉인 상자의 AD(수신자 여럿이 같은 상자). 뿌리 약속까지 묶어 같은 순번 다른 갈래의 상자와 섞이지 않게.
pub(crate) fn direct_ad(
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    root_commit: &[u8; 32],
) -> Vec<u8> {
    let mut ad = b"yangpa/v3/key-direct".to_vec();
    ad.extend_from_slice(space.as_bytes());
    ad.extend_from_slice(group.as_bytes());
    ad.extend_from_slice(&seq.to_be_bytes());
    ad.extend_from_slice(root_commit);
    ad
}

/// 기기 하나에게 가는 봉인 묶음의 AD(갈래까지 묶음).
pub(crate) fn bundle_ad(
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    device: &DeviceId,
    root_commit: &[u8; 32],
) -> Vec<u8> {
    let mut ad = b"yangpa/v3/key-bundle".to_vec();
    ad.extend_from_slice(space.as_bytes());
    ad.extend_from_slice(group.as_bytes());
    ad.extend_from_slice(&seq.to_be_bytes());
    ad.extend_from_slice(device.as_bytes());
    ad.extend_from_slice(root_commit);
    ad
}

/// 크루 잎 L_d.
pub(crate) fn leaf_key(
    leaf_master: &Secret32,
    group: &KeyGroupId,
    device: &DeviceId,
    generation: u64,
) -> Secret32 {
    kdf(
        "yangpa/v3/lkh-leaf",
        leaf_master,
        &[
            group.as_bytes(),
            device.as_bytes(),
            &generation.to_be_bytes(),
        ],
    )
}

/// 비공개 그룹 잎 = KDF(크루 잎, 그룹 ‖ 크루 세대).
pub(crate) fn private_leaf(crew_leaf: &Secret32, group: &KeyGroupId, generation: u64) -> Secret32 {
    kdf(
        "yangpa/v3/lkh-private-leaf",
        crew_leaf,
        &[group.as_bytes(), &generation.to_be_bytes()],
    )
}

/// 트리 노드의 가입 전진(한 방향).
pub(crate) fn advance_node(
    key: &Secret32,
    group: &KeyGroupId,
    seq: u64,
    node: NodeIdx,
    joined_hash: &[u8; 32],
) -> Secret32 {
    kdf(
        "yangpa/v3/lkh-advance",
        key,
        &[
            group.as_bytes(),
            &seq.to_be_bytes(),
            &node.to_bytes(),
            joined_hash,
        ],
    )
}

/// 감싸기 종류: 트리(노드 = 감싸이는 노드, 판 = seq), 덮개(노드 = 감싸는 크루 노드, 판 = 그 크루 노드의 판).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapKind {
    Tree,
    Cover,
}

/// 감싸기 하나의 자리(AD·표지에 들어가는 것).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WrapSite {
    pub kind: WrapKind,
    pub node: NodeIdx,
    pub version: u64,
}

/// 한 갱신의 감싸기 문맥.
#[derive(Clone, Copy)]
pub(crate) struct WrapCx<'a> {
    pub space: &'a SpaceId,
    pub group: &'a KeyGroupId,
    pub seq: u64,
    pub salt: &'a [u8; 24],
    /// 비공개·덮개 그룹이 기대는 크루 상태(페이로드의 `crew`).
    pub crew: Option<&'a CrewRef>,
}

impl WrapCx<'_> {
    fn ad(&self, site: WrapSite) -> Vec<u8> {
        let label: &[u8] = match site.kind {
            WrapKind::Tree => b"yangpa/v3/lkh-wrap",
            WrapKind::Cover => b"yangpa/v3/lkh-cover",
        };
        let mut ad = Vec::with_capacity(label.len() + 16 + 16 + 8 + 5 + 8 + 1 + 8 + 32);
        ad.extend_from_slice(label);
        ad.extend_from_slice(self.space.as_bytes());
        ad.extend_from_slice(self.group.as_bytes());
        ad.extend_from_slice(&self.seq.to_be_bytes());
        ad.extend_from_slice(&site.node.to_bytes());
        ad.extend_from_slice(&site.version.to_be_bytes());
        match self.crew {
            Some(c) => {
                ad.push(1);
                ad.extend_from_slice(&c.seq.to_be_bytes());
                ad.extend_from_slice(&c.root_commit);
            }
            None => ad.push(0),
        }
        ad
    }

    /// 감싸는 키 `k`가 이 자리에서 만드는 표지.
    pub(crate) fn tag(&self, k: &Secret32, site: WrapSite) -> [u8; 8] {
        let label = match site.kind {
            WrapKind::Tree => "yangpa/v3/lkh-tag",
            WrapKind::Cover => "yangpa/v3/lkh-cover-tag",
        };
        let t = kdf(
            label,
            k,
            &[
                self.group.as_bytes(),
                &self.seq.to_be_bytes(),
                &site.node.to_bytes(),
            ],
        );
        let mut tag = [0u8; 8];
        tag.copy_from_slice(&t.0[..8]);
        tag
    }

    fn key(&self, k: &Secret32, ad: &[u8]) -> Secret32 {
        kdf("yangpa/v3/lkh-wrap-key", k, &[ad, self.salt])
    }

    pub(crate) fn seal(&self, wrapping: &Secret32, site: WrapSite, plaintext: &Secret32) -> Wrap {
        let ad = self.ad(site);
        Wrap {
            tag: self.tag(wrapping, site),
            ct: seal_once(&self.key(wrapping, &ad), &ad, plaintext),
        }
    }

    pub(crate) fn open(&self, wrapping: &Secret32, site: WrapSite, w: &Wrap) -> Option<Secret32> {
        let ad = self.ad(site);
        open_once(&self.key(wrapping, &ad), &ad, &w.ct)
    }
}

/// 표지가 맞는 감싸기를 모두 열어 봅니다(AEAD가 가림). 색인을 만들지 않고 훑으므로 메모리가 페이로드 크기에 따라 늘지
/// 않습니다(한 기기가 찾는 표지는 많아야 깊이 + 1개, 합친 감싸기는 2^(깊이 + 1)개 이하).
pub(crate) fn find_wrap(
    cx: &WrapCx<'_>,
    wraps: &[Wrap],
    k: &Secret32,
    site: WrapSite,
) -> Option<Secret32> {
    let tag = cx.tag(k, site);
    wraps
        .iter()
        .filter(|w| w.tag == tag)
        .find_map(|w| cx.open(k, site, w))
}

/// 수신자들의 상자 공개키(하나라도 모르면 실패).
pub(crate) fn recipients<'d>(
    boxes: &dyn BoxKeys,
    devices: impl IntoIterator<Item = &'d DeviceId>,
) -> Result<Vec<(DeviceId, BoxPublicKey)>> {
    devices
        .into_iter()
        .map(|d| {
            boxes
                .box_key(d)
                .map(|pk| (*d, pk.clone()))
                .ok_or(KeyUpdateError::MissingBoxKey(*d))
        })
        .collect()
}
