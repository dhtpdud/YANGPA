//! 키 그룹 `KeyUpdate`의 전송 형식(암호 없는 평문 타입).
//!
//! 키 그룹은 같은 키를 나눠야 하는 기기 모음입니다(크루 그룹, 열람 규칙마다의 비공개 그룹). 그룹마다 확정 기기 한 대가
//! `KeyUpdate` 사슬(`seq`가 1씩 오름)을 씁니다. 기기 256대 이하는 기기마다 직접 봉인하고(깊이 0, "직접 인코딩"),
//! 그보다 크면 LKH 이진 트리를 씁니다("트리 인코딩"). 암호 계산은 `yangpa-crypto`의 `keysched`·`keytree`에 있고, 여기는
//! 받는 쪽이 검증 전에 크기를 묶을 수 있는 순수 데이터와 검사만 둡니다.
//!
//! - 노드 번호는 (잎에서 센 높이, 그 높이에서의 위치)입니다([`NodeIdx`]). 트리가 가득 차 위에 뿌리를 하나 얹어도 기존
//!   노드 번호가 그대로라, 힙 번호(뿌리 1)처럼 모든 번호가 밀리지 않습니다.
//! - 디코딩은 바이트 한도([`MAX_PAYLOAD_LEN`])를 먼저 보고, 모든 목록은 원소를 읽기 전에 개수 한도를 봅니다. 악의적인
//!   페이로드가 받는 쪽에 큰 할당이나 긴 반복을 시키지 못하게 하려는 것입니다.
//! - 한 갱신이 한도를 넘으면 [`KeyUpdatePayload::split`]으로 여러 조각(`part`/`parts`)으로 나누고, 받는 쪽은
//!   [`KeyUpdatePayload::merge`]로 합친 뒤 적용합니다. 기기 목록(가입·제거)도 이어진 구간으로 나눠 실으므로 어떤 조각도
//!   [`MAX_PAYLOAD_LEN`]을 넘지 않습니다. 합친 갱신의 목록 한도는 깊이에서 나오는 실제 최댓값이라, 서명된 갱신 하나가
//!   받는 쪽에 시키는 일과 메모리도 묶입니다.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::{BlobId, DeviceId, EventId};

/// 트리 깊이 상한. 칸 2^17 = 131,072개로 멤버 상한(65,536명)에 사용자당 기기 2대를 넣고도 남습니다.
pub const MAX_DEPTH: u8 = 17;
/// 직접 인코딩 상한: 이보다 많으면 트리로 바꿉니다.
pub const DIRECT_MAX: usize = 256;
/// 트리에서 직접으로 돌아오는 크기(이하). 경계에서 인코딩이 오락가락하지 않게 상한보다 낮게 둡니다.
pub const DIRECT_BACK: usize = 160;
/// 받은 페이로드 조각 하나의 바이트 한도(키 합의 레인 본문 한도와 같음).
pub const MAX_PAYLOAD_LEN: usize = crate::event::MAX_KEY_UPDATE_BODY_LEN;
/// 나눌 때 조각 하나의 목표 크기. 이벤트 본문 틀·압축 표시·Padmé 채움(이 크기에서 16KiB 단위)·AEAD 태그를 더해도
/// 한도 안에 들도록 32KiB를 남깁니다(16KiB만 남기면 목표 크기에 꼭 찬 조각이 채움 한 단위를 넘겨 태그 16B만큼 한도를
/// 넘었음).
pub const PART_TARGET_LEN: usize = MAX_PAYLOAD_LEN - 32 * 1024;
/// 감싸기 하나의 인코딩 크기(표지 8 + 암호문 32 + 태그 16).
pub const WRAP_LEN: usize = 56;
/// 기기 ID 하나의 인코딩 크기(길이 1 + 32).
pub const DEVICE_LEN: usize = 33;
/// 조각 하나에 들 수 있는 감싸기 수.
pub const MAX_WRAPS: usize = MAX_PAYLOAD_LEN / WRAP_LEN;
/// 칸 넣기·비우기 수(조각 하나). 트리 칸 수와 같습니다.
pub const MAX_LAYOUT: usize = 1 << MAX_DEPTH;
/// 합친 갱신의 가입·제거 기기 목록 길이(트리 칸 수).
pub const MAX_DEVICES: usize = 1 << MAX_DEPTH;
/// 조각 하나에 들 수 있는 기기 ID 수. 목록이 이보다 길면 [`KeyUpdatePayload::split`]이 여러 조각에 나눠 싣습니다.
pub const MAX_PART_DEVICES: usize = MAX_PAYLOAD_LEN / DEVICE_LEN;
/// 덮개 그룹(깊이 0) 합친 갱신의 감싸기 수: 덮개 노드는 크루 트리 노드 수(2^(깊이 상한 + 1))보다 많을 수 없습니다.
pub const MAX_COVER_WRAPS: usize = 1 << (MAX_DEPTH + 1);
/// 봉인 상자 수(조각 하나). 직접 인코딩은 상자 하나에 수신자 [`DIRECT_MAX`]명까지입니다.
pub const MAX_SEALED: usize = DIRECT_MAX;
/// 한 갱신의 조각 수 상한. 깊이 17에서 가능한 가장 큰 갱신(모든 칸을 비우고 다시 채우는 `Rotate`: 목록 8.4MB + 칸
/// 5.2MB + 감싸기 14.7MB)이 약 30조각이고, 받는 쪽이 모아 둘 바이트를 약 30MB로 묶습니다.
pub const MAX_PARTS: u16 = 32;

/// 키 그룹 ID(스트림 이름과 같은 타입, 만드는 법은 [`KeyGroupId::crew`]·[`KeyGroupId::private`]).
pub use crate::ids::KeyGroupId;

/// 트리 노드 번호: 잎에서 센 높이(잎 0, 뿌리 = 깊이)와 그 높이에서의 위치. 칸 `s`의 높이 `l` 조상은 `(l, s >> l)`입니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeIdx {
    pub level: u8,
    pub pos: u32,
}

impl NodeIdx {
    /// 칸 `slot`의 잎.
    pub const fn leaf(slot: u32) -> Self {
        NodeIdx {
            level: 0,
            pos: slot,
        }
    }

    /// 깊이 `depth` 트리의 뿌리.
    pub const fn root(depth: u8) -> Self {
        NodeIdx {
            level: depth,
            pos: 0,
        }
    }

    /// 칸 `slot`의 높이 `level` 조상(0이면 잎 자신).
    pub const fn ancestor(slot: u32, level: u8) -> Self {
        NodeIdx {
            level,
            pos: if level >= 32 { 0 } else { slot >> level },
        }
    }

    pub const fn parent(self) -> Self {
        NodeIdx {
            level: self.level + 1,
            pos: self.pos >> 1,
        }
    }

    /// 두 자식(왼쪽, 오른쪽). 잎에는 부르지 않습니다.
    pub const fn children(self) -> [NodeIdx; 2] {
        let level = self.level - 1;
        [
            NodeIdx {
                level,
                pos: self.pos << 1,
            },
            NodeIdx {
                level,
                pos: (self.pos << 1) | 1,
            },
        ]
    }

    /// 칸 `slot`이 이 노드 아래에 있는지.
    pub const fn covers(self, slot: u32) -> bool {
        Self::ancestor(slot, self.level).pos == self.pos
    }

    /// 깊이 `depth` 트리 안의 노드인지.
    pub const fn fits(self, depth: u8) -> bool {
        self.level <= depth && (self.pos as u64) < (1u64 << (depth - self.level))
    }

    /// AD·KDF에 넣는 고정 길이 표현(높이 1 + 위치 4, 빅엔디언).
    pub fn to_bytes(self) -> [u8; 5] {
        let mut b = [0u8; 5];
        b[0] = self.level;
        b[1..].copy_from_slice(&self.pos.to_be_bytes());
        b
    }
}

/// 대칭 감싸기: 노드 키(또는 그룹 키) 32바이트를 다른 키로 XChaCha20-Poly1305 암호화한 것.
///
/// 받는 쪽은 자기가 가진 키로 표지를 계산해 열 수 있는 감싸기를 찾으므로 트리 배치를 몰라도 됩니다. 표지는 순번이 바뀌면
/// 달라져 갱신 사이에 이어 볼 수 없습니다. 표지가 우연히 겹치면 맞는 것을 모두 열어 봅니다(AEAD가 가림).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wrap {
    pub tag: [u8; 8],
    /// 암호문 32바이트 + Poly1305 태그 16바이트.
    #[serde(with = "byte_array")]
    pub ct: [u8; 48],
}

impl fmt::Debug for Wrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Wrap({})", hex::encode(self.tag))
    }
}

/// 새 R을 새 T에서 나온 키로 암호화한 것(암호문 32 + 태그 16).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RCt {
    #[serde(with = "byte_array")]
    pub ct: [u8; 48],
}

impl fmt::Debug for RCt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RCt(..)")
    }
}

/// 봉인 상자(`yangpa_crypto::seal_to` 봉투). 직접 인코딩의 새 T(수신 기기 여럿)나 기기 하나의 묶음을 담습니다.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedBox(#[serde(with = "serde_bytes")] pub Vec<u8>);

impl fmt::Debug for SealedBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SealedBox({}B)", self.0.len())
    }
}

impl SealedBox {
    /// 상자에 든 수신 기기 목록(복호화 없이, [`sealed_recipients`]).
    pub fn recipients(&self) -> Result<Vec<DeviceId>, CoreError> {
        sealed_recipients(&self.0)
    }
}

/// 봉투 형식 판(`yangpa_crypto::seal_to`가 쓰는 값).
pub const SEALED_ENVELOPE_VERSION: u8 = 2;

/// 봉투(`yangpa_crypto::seal_to`)에 든 수신 기기 목록. 봉투는 postcard `{v, wraps: [{device, kem_ct, wrapped}], ct}`이고
/// 여기서는 앞 두 필드만 빌려 읽습니다(본문 암호문은 건드리지 않음). 해소기는 암호 크레이트 없이 직접 인코딩 상자의
/// 수신자가 그룹 멤버인지 보고, `yangpa_crypto::sealed_recipients`도 이것을 불러 두 쪽이 늘 같은 답을 냅니다.
pub fn sealed_recipients(envelope: &[u8]) -> Result<Vec<DeviceId>, CoreError> {
    #[derive(Deserialize)]
    struct WrapHead<'a> {
        device: DeviceId,
        #[serde(borrow)]
        _kem_ct: &'a [u8],
        #[serde(borrow)]
        _wrapped: &'a [u8],
    }
    #[derive(Deserialize)]
    struct EnvelopeHead<'a> {
        v: u8,
        #[serde(borrow)]
        wraps: Vec<WrapHead<'a>>,
    }
    // 판이 다르면 구조도 다르므로 첫 바이트(postcard u8)부터 봅니다.
    if envelope.first() != Some(&SEALED_ENVELOPE_VERSION) {
        return Err(CoreError::InvalidEncoding);
    }
    let (head, _ct): (EnvelopeHead<'_>, _) = postcard::take_from_bytes(envelope)?;
    debug_assert_eq!(head.v, SEALED_ENVELOPE_VERSION);
    Ok(head.wraps.into_iter().map(|w| w.device).collect())
}

/// 트리 칸 바뀜(트리 인코딩만). 칸은 확정 기기가 고릅니다(가장 왼쪽 빈 칸).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutOp {
    Fill { slot: u32, device: DeviceId },
    Clear { slot: u32 },
}

impl LayoutOp {
    pub fn slot(&self) -> u32 {
        match self {
            LayoutOp::Fill { slot, .. } | LayoutOp::Clear { slot } => *slot,
        }
    }
}

/// 갱신 종류.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyUpdateKind {
    /// 가입만: T(트리면 가입 경로의 노드)를 한 방향으로 전진합니다. R은 그대로입니다.
    Advance {
        #[serde(deserialize_with = "bounded::<_, _, MAX_PART_DEVICES>")]
        joined: Vec<DeviceId>,
    },
    /// 제거(와 같은 묶음의 가입): 제거 경로를 새로 뽑고 R도 새로. 기다리던 가입을 같은 이벤트에 실어 "제거가 가입보다
    /// 먼저" 규칙을 한 이벤트로 지킵니다.
    Rotate {
        #[serde(deserialize_with = "bounded::<_, _, MAX_PART_DEVICES>")]
        removed: Vec<DeviceId>,
        #[serde(deserialize_with = "bounded::<_, _, MAX_PART_DEVICES>")]
        joined: Vec<DeviceId>,
    },
    /// 뿌리(T)와 R만 새로(시간 회전, 새 확정 기기의 첫 회전, 덮개 그룹의 첫 갱신).
    Refresh,
    /// 전체 재키: 표 보유 기기(옛 확정 기기·예비 기기)가 아는 키를 모두 버립니다. 트리면 새 leaf_master(세대
    /// `generation`)로 모든 잎·노드를 새로 만들고 기기 몫은 페이로드 밖(Inbox·블롭)으로, 직접 인코딩(깊이 0)이면 새 T를
    /// 남는 기기 모두에게 봉인합니다(세대는 그대로). 같은 묶음의 제거·가입(앞 갱신 대비 바뀐 기기만)을 목록으로 실어,
    /// 기기 몫이 페이로드 밖에 있어도 해소기가 그룹 멤버를 따라갈 수 있게 합니다.
    FullRekey {
        generation: u64,
        #[serde(deserialize_with = "bounded::<_, _, MAX_PART_DEVICES>")]
        removed: Vec<DeviceId>,
        #[serde(deserialize_with = "bounded::<_, _, MAX_PART_DEVICES>")]
        joined: Vec<DeviceId>,
    },
    /// R만 새로(채널이 그룹을 떠나거나 기록 공개를 끌 때).
    RRefresh,
}

impl KeyUpdateKind {
    pub fn joined(&self) -> &[DeviceId] {
        match self {
            KeyUpdateKind::Advance { joined }
            | KeyUpdateKind::Rotate { joined, .. }
            | KeyUpdateKind::FullRekey { joined, .. } => joined,
            _ => &[],
        }
    }

    pub fn removed(&self) -> &[DeviceId] {
        match self {
            KeyUpdateKind::Rotate { removed, .. } | KeyUpdateKind::FullRekey { removed, .. } => {
                removed
            }
            _ => &[],
        }
    }

    /// 전체 재키인지(표 보유 기기를 비우는 유일한 갱신).
    pub fn is_full_rekey(&self) -> bool {
        matches!(self, KeyUpdateKind::FullRekey { .. })
    }

    /// R이 새로 바뀌는 종류인지(그러면 `r_ct`가 꼭 있음).
    pub fn new_r(&self) -> bool {
        !matches!(self, KeyUpdateKind::Advance { .. })
    }

    /// (가입, 제거) 목록을 떼어 내고 빈 목록을 남깁니다(나눌 때).
    fn take_lists(&mut self) -> (Vec<DeviceId>, Vec<DeviceId>) {
        match self {
            KeyUpdateKind::Advance { joined } => (std::mem::take(joined), Vec::new()),
            KeyUpdateKind::Rotate { removed, joined }
            | KeyUpdateKind::FullRekey {
                removed, joined, ..
            } => (std::mem::take(joined), std::mem::take(removed)),
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn joined_mut(&mut self) -> Option<&mut Vec<DeviceId>> {
        match self {
            KeyUpdateKind::Advance { joined }
            | KeyUpdateKind::Rotate { joined, .. }
            | KeyUpdateKind::FullRekey { joined, .. } => Some(joined),
            _ => None,
        }
    }

    fn removed_mut(&mut self) -> Option<&mut Vec<DeviceId>> {
        match self {
            KeyUpdateKind::Rotate { removed, .. } | KeyUpdateKind::FullRekey { removed, .. } => {
                Some(removed)
            }
            _ => None,
        }
    }

    /// 조각마다 같아야 하는 부분이 같은지(목록은 조각마다 다른 구간이라 보지 않음).
    pub fn same_variant(&self, other: &KeyUpdateKind) -> bool {
        match (self, other) {
            (
                KeyUpdateKind::FullRekey { generation: a, .. },
                KeyUpdateKind::FullRekey { generation: b, .. },
            ) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

/// 비공개·덮개 그룹 갱신이 기대는 크루 상태(그 크루 갱신의 순번과 뿌리 약속). 비공개 트리의 잎은 크루 잎에서, 덮개
/// 감싸기는 크루 노드 키로 만들므로, 받는 쪽은 이 순번의 크루 상태(갈래까지 같은 것)를 골라 엽니다. 감싸기 AD에도
/// 들어갑니다.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewRef {
    pub seq: u64,
    pub root_commit: [u8; 32],
}

impl fmt::Debug for CrewRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CrewRef({}, {})",
            self.seq,
            hex::encode(&self.root_commit[..4])
        )
    }
}

/// `KeyUpdate` 이벤트 본문(`EventBody::KeyUpdate`) 한 조각. 조각 잇는 법·받아들이는 규칙은 `yangpa-sync` 해소기에 있습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyUpdatePayload {
    #[serde(with = "group_raw")]
    pub group: KeyGroupId,
    /// 그룹 안 순번. 첫 갱신이 1이고 하나씩 오릅니다.
    pub seq: u64,
    /// 앞 `KeyUpdate` 이벤트(나눴으면 첫 조각). 첫 갱신과 확정 기기가 이벤트 ID를 모를 때는 없음.
    pub prev: Option<EventId>,
    pub kind: KeyUpdateKind,
    /// derive_key("yangpa/v3/key-commit", 공간 ‖ 그룹 ‖ seq ‖ T ‖ R). 받는 쪽은 계산한 T·R이 이와 같은지 봅니다(R도
    /// 묶어, 확정 기기나 중계한 멤버가 가입자에게 다른 R을 주면 드러남).
    pub root_commit: [u8; 32],
    /// 앞 갱신의 `root_commit`(첫 갱신은 0). 받는 쪽이 자기 사슬과 이어지는지 봅니다.
    pub prev_root_commit: [u8; 32],
    /// 비공개·덮개 그룹이 기대는 크루 상태. 크루 그룹은 없음.
    pub crew: Option<CrewRef>,
    /// 기기 몫 봉인 묶음(크루 트리 가입자, 크루 전체 재키)의 머클 뿌리. 묶음은 페이로드 밖(Inbox·블롭·중계)으로
    /// 가므로, 받는 쪽은 자기 묶음의 증명이 이 뿌리와 맞을 때만 엽니다(다른 멤버가 심은 묶음·다른 갈래의 묶음을 거름).
    pub bundle_root: Option<[u8; 32]>,
    /// 크루 트리 전체 재키의 기기 몫 묶음을 사용자별 조각으로 모은 내용 주소 블롭. 기기는 자기 사용자 조각만 받아
    /// `bundle_root` 증명으로 확인합니다. 묶음이 있는 갱신에만 있습니다.
    pub shards: Option<BlobId>,
    /// 이 갱신 뒤 트리 깊이. 0이면 직접 인코딩(또는 덮개 그룹).
    pub depth: u8,
    /// 이 갱신의 감싸기·`r_ct` 키에 섞는 무작위 값. 감싸기 키가 갱신마다 달라 논스를 싣지 않습니다(같은 순번의 두
    /// 갈래가 같은 키로 다른 값을 감싸도 키가 달라 논스 재사용이 생기지 않음).
    pub salt: [u8; 24],
    #[serde(deserialize_with = "bounded::<_, _, MAX_LAYOUT>")]
    pub layout: Vec<LayoutOp>,
    #[serde(deserialize_with = "bounded::<_, _, MAX_WRAPS>")]
    pub wraps: Vec<Wrap>,
    #[serde(deserialize_with = "bounded::<_, _, MAX_SEALED>")]
    pub sealed: Vec<SealedBox>,
    pub r_ct: Option<RCt>,
    /// 조각 번호(0부터)와 조각 수. 나누지 않았으면 0과 1.
    pub part: u16,
    pub parts: u16,
}

/// 목록 한도(조각 하나, 또는 합친 갱신).
struct Caps {
    layout: usize,
    wraps: usize,
    devices: usize,
}

impl KeyUpdatePayload {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("KeyUpdate 직렬화는 실패하지 않음")
    }

    pub fn encoded_len(&self) -> usize {
        postcard::serialize_with_flavor(self, postcard::ser_flavors::Size::default())
            .expect("KeyUpdate 직렬화는 실패하지 않음")
    }

    /// 받은 조각 하나를 풉니다: 바이트 한도 → 목록 개수 한도(디코딩 중) → 뒤에 남는 바이트 없음 → [`Self::validate`].
    pub fn decode(bytes: &[u8]) -> Result<Self, CoreError> {
        if bytes.len() > MAX_PAYLOAD_LEN {
            return Err(CoreError::LimitExceeded("KeyUpdate 크기"));
        }
        let (p, rest): (KeyUpdatePayload, _) = postcard::take_from_bytes(bytes)?;
        if !rest.is_empty() {
            return Err(CoreError::InvalidEncoding);
        }
        p.validate()?;
        Ok(p)
    }

    /// 조각 하나의 구조 검사(서명·암호 확인 전에). 디코딩이 이미 목록 길이를 묶었고, 여기서는 값의 범위와 서로 맞는지를
    /// 봅니다. 기기 목록은 조각마다 이어진 구간이라 조각 안에서 정렬돼 있어야 합니다.
    pub fn validate(&self) -> Result<(), CoreError> {
        self.check(&Caps {
            layout: MAX_LAYOUT,
            wraps: MAX_WRAPS,
            devices: MAX_PART_DEVICES,
        })
    }

    /// 합친 갱신([`Self::merge`])의 구조 검사. 목록 한도는 깊이에서 나오는 최댓값입니다: 감싸기는 노드마다 많아야 두
    /// 자식 몫이라 2^(깊이 + 1)개(덮개 그룹은 [`MAX_COVER_WRAPS`]), 칸 바뀜은 한 칸에 비우기·채우기가 함께 올 수 있어
    /// 2 × 2^깊이개, 기기 목록은 [`MAX_DEVICES`]. 멤버는 적용 전에 이것을 봅니다.
    pub fn validate_merged(&self) -> Result<(), CoreError> {
        if self.part != 0 || self.parts != 1 {
            return Err(CoreError::InvalidEncoding);
        }
        let (layout, wraps) = if self.depth == 0 {
            (0, MAX_COVER_WRAPS)
        } else {
            (
                2usize << self.depth.min(MAX_DEPTH),
                2usize << self.depth.min(MAX_DEPTH),
            )
        };
        self.check(&Caps {
            layout,
            wraps,
            devices: MAX_DEVICES,
        })
    }

    fn check(&self, caps: &Caps) -> Result<(), CoreError> {
        if self.depth > MAX_DEPTH {
            return Err(CoreError::LimitExceeded("KeyUpdate 트리 깊이"));
        }
        if self.parts == 0 || self.parts > MAX_PARTS || self.part >= self.parts {
            return Err(CoreError::LimitExceeded("KeyUpdate 조각"));
        }
        if self.seq == 0 {
            return Err(CoreError::InvalidEncoding);
        }
        if self.layout.len() > caps.layout
            || self.wraps.len() > caps.wraps
            || self.sealed.len() > MAX_SEALED
        {
            return Err(CoreError::LimitExceeded("KeyUpdate 목록"));
        }
        let slots = if self.depth == 0 {
            0
        } else {
            1u64 << self.depth
        };
        if self.layout.iter().any(|op| u64::from(op.slot()) >= slots) {
            return Err(CoreError::InvalidEncoding);
        }
        let (joined, removed) = (self.kind.joined(), self.kind.removed());
        if joined.len() > caps.devices || removed.len() > caps.devices {
            return Err(CoreError::LimitExceeded("KeyUpdate 기기 목록"));
        }
        // 정렬·중복 없음: 가입 해시가 목록 순서에 따라 갈리지 않고, 한 기기가 두 번 세어지지 않게. 조각마다 이어진
        // 구간이므로 조각 안에서도, 합친 뒤에도 그대로 성립합니다.
        if !strictly_sorted(joined) || !strictly_sorted(removed) {
            return Err(CoreError::InvalidEncoding);
        }
        if joined.iter().any(|d| removed.binary_search(d).is_ok()) {
            return Err(CoreError::InvalidEncoding);
        }
        let first = self.part == 0;
        if !first && self.r_ct.is_some() {
            return Err(CoreError::InvalidEncoding);
        }
        // 기기 몫 묶음은 트리 인코딩에만 있습니다.
        if self.depth == 0 && self.bundle_root.is_some() {
            return Err(CoreError::InvalidEncoding);
        }
        if self.shards.is_some() && self.bundle_root.is_none() {
            return Err(CoreError::InvalidEncoding);
        }
        match &self.kind {
            // 나눌 때 목록을 맨 앞에 실으므로 첫 조각에는 가입 기기가 적어도 하나 있습니다.
            KeyUpdateKind::Advance { joined } if first && joined.is_empty() => {
                return Err(CoreError::InvalidEncoding);
            }
            KeyUpdateKind::Rotate { removed, joined }
                if first && removed.is_empty() && joined.is_empty() =>
            {
                return Err(CoreError::InvalidEncoding);
            }
            KeyUpdateKind::Refresh | KeyUpdateKind::RRefresh if !self.layout.is_empty() => {
                return Err(CoreError::InvalidEncoding);
            }
            KeyUpdateKind::RRefresh if !self.wraps.is_empty() || !self.sealed.is_empty() => {
                return Err(CoreError::InvalidEncoding);
            }
            _ => {}
        }
        if first && self.kind.new_r() && self.r_ct.is_none() {
            return Err(CoreError::InvalidEncoding);
        }
        Ok(())
    }

    /// 인코딩이 `max_len`바이트를 넘으면 조각으로 나눕니다. 가입·제거 목록, 칸 바뀜, 감싸기, 봉인 상자를 이 순서로
    /// 이어진 구간씩 담고, `r_ct`는 첫 조각에만 둡니다. 나머지 머리(그룹·순번·약속·깊이·salt·크루 상태·묶음 뿌리)는
    /// 모든 조각이 같습니다. 조각이 [`MAX_PARTS`]개를 넘거나 원소 하나가 조각 하나에 들지 않으면 오류입니다(그런
    /// 갱신은 받는 쪽이 받지 못하므로 발행하면 안 됨).
    pub fn split(mut self, max_len: usize) -> Result<Vec<KeyUpdatePayload>, CoreError> {
        self.part = 0;
        self.parts = 1;
        if self.encoded_len() <= max_len {
            return Ok(vec![self]);
        }
        let (joined, removed) = self.kind.take_lists();
        let layout = std::mem::take(&mut self.layout);
        let wraps = std::mem::take(&mut self.wraps);
        let sealed = std::mem::take(&mut self.sealed);
        let mut rest = self.clone();
        rest.r_ct = None;
        // 목록 길이 접두(varint)가 늘 수 있는 몫(목록 5개 × 2바이트)과 여유를 남깁니다.
        let room = |head: &KeyUpdatePayload| max_len.saturating_sub(head.encoded_len() + 16);
        let mut parts = vec![self];
        let mut left = room(&parts[0]);
        let push = |parts: &mut Vec<KeyUpdatePayload>, left: &mut usize, size: usize| {
            if size > *left && !is_empty_part(parts.last().expect("조각 하나는 있음")) {
                parts.push(rest.clone());
                *left = room(&rest);
            }
            *left = left.saturating_sub(size);
        };
        for d in joined {
            push(&mut parts, &mut left, DEVICE_LEN);
            let last = parts.last_mut().expect("조각");
            last.kind.joined_mut().expect("목록이 있는 종류").push(d);
        }
        for d in removed {
            push(&mut parts, &mut left, DEVICE_LEN);
            let last = parts.last_mut().expect("조각");
            last.kind.removed_mut().expect("목록이 있는 종류").push(d);
        }
        for op in layout {
            push(&mut parts, &mut left, postcard_len(&op));
            parts.last_mut().expect("조각").layout.push(op);
        }
        for w in wraps {
            push(&mut parts, &mut left, WRAP_LEN);
            parts.last_mut().expect("조각").wraps.push(w);
        }
        for s in sealed {
            push(&mut parts, &mut left, s.0.len() + 3);
            parts.last_mut().expect("조각").sealed.push(s);
        }
        if parts.len() > usize::from(MAX_PARTS) {
            return Err(CoreError::LimitExceeded("KeyUpdate 조각 수"));
        }
        let n = parts.len() as u16;
        for (i, p) in parts.iter_mut().enumerate() {
            p.part = i as u16;
            p.parts = n;
        }
        if parts.iter().any(|p| p.encoded_len() > max_len) {
            return Err(CoreError::LimitExceeded("KeyUpdate 조각 크기"));
        }
        Ok(parts)
    }

    /// 조각마다 같아야 하는 머리(그룹·순번·앞 이벤트·약속·크루 상태·묶음 뿌리·깊이·salt·조각 수·종류)가 `other`와
    /// 같은지. 해소기는 이것으로 같은 갱신의 조각인지 가립니다.
    pub fn same_update(&self, other: &KeyUpdatePayload) -> bool {
        self.parts == other.parts
            && self.group == other.group
            && self.seq == other.seq
            && self.prev == other.prev
            && self.root_commit == other.root_commit
            && self.prev_root_commit == other.prev_root_commit
            && self.crew == other.crew
            && self.bundle_root == other.bundle_root
            && self.shards == other.shards
            && self.depth == other.depth
            && self.salt == other.salt
            && self.kind.same_variant(&other.kind)
    }

    /// 한 갱신의 조각을 모두 모아 합칩니다(순서는 상관없음). 조각마다 [`Self::validate`]를 이미 통과했어야 하고(디코딩이
    /// 함), 합친 것은 [`Self::validate_merged`]까지 통과해야 돌려줍니다.
    pub fn merge(mut parts: Vec<KeyUpdatePayload>) -> Result<KeyUpdatePayload, CoreError> {
        parts.sort_by_key(|p| p.part);
        let n = parts.len();
        let first = parts.first().ok_or(CoreError::InvalidEncoding)?;
        if usize::from(first.parts) != n || n > usize::from(MAX_PARTS) {
            return Err(CoreError::InvalidEncoding);
        }
        for (i, p) in parts.iter().enumerate() {
            if p.part as usize != i || !p.same_update(first) {
                return Err(CoreError::InvalidEncoding);
            }
        }
        let mut it = parts.into_iter();
        let mut out = it.next().expect("조각 하나는 있음");
        for mut p in it {
            let (joined, removed) = p.kind.take_lists();
            if let Some(v) = out.kind.joined_mut() {
                v.extend(joined);
            }
            if let Some(v) = out.kind.removed_mut() {
                v.extend(removed);
            }
            out.layout.extend(p.layout);
            out.wraps.extend(p.wraps);
            out.sealed.extend(p.sealed);
        }
        out.part = 0;
        out.parts = 1;
        out.validate_merged()?;
        Ok(out)
    }
}

fn is_empty_part(p: &KeyUpdatePayload) -> bool {
    p.layout.is_empty()
        && p.wraps.is_empty()
        && p.sealed.is_empty()
        && p.kind.joined().is_empty()
        && p.kind.removed().is_empty()
}

fn postcard_len<T: Serialize>(v: &T) -> usize {
    postcard::serialize_with_flavor(v, postcard::ser_flavors::Size::default()).unwrap_or(0)
}

fn strictly_sorted(v: &[DeviceId]) -> bool {
    v.windows(2).all(|w| w[0] < w[1])
}

/// 원소를 읽기 전에 개수를 보는 목록 디코딩. postcard는 길이를 먼저 알려 주므로(`size_hint`) 한도를 넘는 길이는 원소
/// 하나 읽지 않고 거부하고, 길이를 모르는 형식이어도 한도에서 멈춥니다.
fn bounded<'de, D, T, const N: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T, const N: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for V<T, N> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "원소 {N}개 이하의 목록")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let hint = seq.size_hint().unwrap_or(0);
            if hint > N {
                return Err(de::Error::invalid_length(hint, &self));
            }
            // 미리 잡는 크기도 한도와 작은 상수로 묶습니다(길이를 속여도 실제 원소만큼만 자람).
            let mut out = Vec::with_capacity(hint.min(1024));
            while let Some(x) = seq.next_element()? {
                if out.len() == N {
                    return Err(de::Error::invalid_length(N + 1, &self));
                }
                out.push(x);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(V::<T, N>(PhantomData))
}

/// 길이 접두 없이 바이트를 그대로 싣는 고정 길이 배열(serde는 32바이트 넘는 배열을 기본으로 다루지 않음).
mod byte_array {
    use std::fmt;

    use serde::de::{self, SeqAccess, Visitor};
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(a: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(N)?;
        for b in a {
            t.serialize_element(b)?;
        }
        t.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        struct V<const N: usize>;
        impl<'de, const N: usize> Visitor<'de> for V<N> {
            type Value = [u8; N];

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{N}바이트")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
                let mut out = [0u8; N];
                for (i, b) in out.iter_mut().enumerate() {
                    *b = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(i, &self))?;
                }
                Ok(out)
            }
        }
        d.deserialize_tuple(N, V::<N>)
    }
}

/// 페이로드의 키 그룹 ID는 16바이트 그대로(길이 접두 없이) 싣습니다. `ids`의 ID 직렬화는 바이트열(길이 1B 접두)이라
/// 그대로 쓰면 가입 갱신 본문(252B)이 Padmé 경계(4 + 256B)를 넘어 크루 기기마다 16B를 더 받습니다.
mod group_raw {
    use serde::{Deserializer, Serializer};

    use super::{KeyGroupId, byte_array};

    pub fn serialize<S: Serializer>(g: &KeyGroupId, s: S) -> Result<S::Ok, S::Error> {
        byte_array::serialize(&g.0, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<KeyGroupId, D::Error> {
        byte_array::deserialize(d).map(KeyGroupId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(b: u8) -> DeviceId {
        DeviceId([b; 32])
    }

    fn devn(i: u32) -> DeviceId {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&i.to_be_bytes());
        DeviceId(b)
    }

    fn sample() -> KeyUpdatePayload {
        KeyUpdatePayload {
            group: KeyGroupId([7; 16]),
            seq: 3,
            prev: Some(EventId([9; 32])),
            kind: KeyUpdateKind::Rotate {
                removed: vec![dev(1), dev(2)],
                joined: vec![dev(3)],
            },
            root_commit: [1; 32],
            prev_root_commit: [2; 32],
            crew: Some(CrewRef {
                seq: 11,
                root_commit: [8; 32],
            }),
            bundle_root: Some([4; 32]),
            shards: None,
            depth: 4,
            salt: [5; 24],
            layout: vec![
                LayoutOp::Clear { slot: 1 },
                LayoutOp::Clear { slot: 2 },
                LayoutOp::Fill {
                    slot: 1,
                    device: dev(3),
                },
            ],
            wraps: (0..20u8)
                .map(|i| Wrap {
                    tag: [i; 8],
                    ct: [i; 48],
                })
                .collect(),
            sealed: vec![SealedBox(vec![4; 100])],
            r_ct: Some(RCt { ct: [6; 48] }),
            part: 0,
            parts: 1,
        }
    }

    fn wraps(n: u32) -> Vec<Wrap> {
        (0..n)
            .map(|i| Wrap {
                tag: i.to_be_bytes().repeat(2).try_into().unwrap(),
                ct: [(i % 251) as u8; 48],
            })
            .collect()
    }

    /// 나누고 → 조각마다 인코딩 한도·디코딩 → 섞어서 합치면 원래와 같은지.
    fn roundtrip_split(p: &KeyUpdatePayload, cap: usize) -> Vec<KeyUpdatePayload> {
        let parts = p.clone().split(cap).expect("나누기");
        let mut decoded = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let b = part.encode();
            assert!(b.len() <= cap, "조각 {i}: {}B", b.len());
            let back = KeyUpdatePayload::decode(&b).expect("조각 디코딩");
            assert_eq!(back.part as usize, i);
            assert_eq!(back.parts as usize, parts.len());
            decoded.push(back);
        }
        decoded.reverse();
        assert_eq!(&KeyUpdatePayload::merge(decoded).expect("합치기"), p);
        parts
    }

    #[test]
    fn node_index_is_stable_and_navigates() {
        let n = NodeIdx::ancestor(13, 2);
        assert_eq!(n, NodeIdx { level: 2, pos: 3 });
        assert_eq!(n.parent(), NodeIdx { level: 3, pos: 1 });
        assert_eq!(
            n.children(),
            [NodeIdx { level: 1, pos: 6 }, NodeIdx { level: 1, pos: 7 }]
        );
        assert!(n.covers(12) && n.covers(15) && !n.covers(11));
        assert!(n.fits(4) && !n.fits(3) && !NodeIdx { level: 2, pos: 4 }.fits(4));
        // 위에 뿌리를 얹어도(깊이 3 → 4) 기존 노드 번호가 그대로입니다.
        assert_eq!(NodeIdx::root(3).parent(), NodeIdx::root(4));
        assert_eq!(NodeIdx::ancestor(13, 0), NodeIdx::leaf(13));
    }

    #[test]
    fn wrap_is_exactly_56_bytes_and_roundtrips() {
        let w = Wrap {
            tag: [1; 8],
            ct: [2; 48],
        };
        let b = postcard::to_stdvec(&w).unwrap();
        assert_eq!(b.len(), WRAP_LEN);
        assert_eq!(postcard::from_bytes::<Wrap>(&b).unwrap(), w);
        let r = postcard::to_stdvec(&RCt { ct: [3; 48] }).unwrap();
        assert_eq!(r.len(), 48);
        assert_eq!(postcard::to_stdvec(&dev(1)).unwrap().len(), DEVICE_LEN);
        // 사람이 읽는 형식에서도 왕복.
        let j = serde_json::to_string(&w).unwrap();
        assert_eq!(serde_json::from_str::<Wrap>(&j).unwrap(), w);
    }

    #[test]
    fn payload_roundtrip_and_validate() {
        let p = sample();
        let b = p.encode();
        assert_eq!(b.len(), p.encoded_len());
        assert_eq!(KeyUpdatePayload::decode(&b).unwrap(), p);
        // 뒤에 남는 바이트는 거부.
        let mut extra = b.clone();
        extra.push(0);
        assert!(KeyUpdatePayload::decode(&extra).is_err());
        // 정렬되지 않은 목록, 겹치는 가입·제거, 칸 범위 밖, 깊이 상한, 조각 번호, 빠진 r_ct.
        let mut bad = p.clone();
        bad.kind = KeyUpdateKind::Rotate {
            removed: vec![dev(2), dev(1)],
            joined: Vec::new(),
        };
        assert!(bad.validate().is_err());
        bad.kind = KeyUpdateKind::Rotate {
            removed: vec![dev(1)],
            joined: vec![dev(1)],
        };
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.layout.push(LayoutOp::Clear { slot: 16 });
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.depth = MAX_DEPTH + 1;
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.part = 1;
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.r_ct = None;
        assert!(bad.validate().is_err());
        // 직접 인코딩의 전체 재키(깊이 0)는 칸 바뀜·묶음 없이 됩니다.
        let mut direct = p.clone();
        direct.kind = KeyUpdateKind::FullRekey {
            generation: 1,
            removed: vec![dev(1)],
            joined: vec![dev(3)],
        };
        direct.depth = 0;
        direct.layout.clear();
        direct.bundle_root = None;
        direct.validate().unwrap();
        let mut bad = direct.clone();
        bad.r_ct = None;
        assert!(bad.validate().is_err());
        let mut bad = direct.clone();
        bad.layout.push(LayoutOp::Clear { slot: 0 });
        assert!(bad.validate().is_err());
        let mut bad = direct;
        bad.kind = KeyUpdateKind::FullRekey {
            generation: 1,
            removed: vec![dev(1)],
            joined: vec![dev(1)],
        };
        assert!(bad.validate().is_err());
        // 직접 인코딩에는 기기 몫 묶음이 없음.
        let mut bad = p.clone();
        bad.depth = 0;
        bad.layout.clear();
        assert!(bad.validate().is_err());
        // 나중 조각의 r_ct.
        let mut bad = p.clone();
        bad.parts = 2;
        bad.part = 1;
        assert!(bad.validate().is_err());
        let mut adv = p;
        adv.kind = KeyUpdateKind::Advance {
            joined: vec![dev(3)],
        };
        adv.r_ct = None;
        assert!(adv.validate().is_ok());
    }

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn enc<T: Serialize>(v: &T) -> Vec<u8> {
        postcard::to_stdvec(v).unwrap()
    }

    /// 한도를 넘는 길이를 적은 목록은 원소를 읽기 전에 거부하고, 바이트 한도를 넘는 입력은 풀지 않습니다.
    /// (postcard의 구조체 인코딩은 필드 인코딩을 차례로 이은 것이라 앞 필드를 이어 붙여 적대적 입력을 만듭니다.)
    #[test]
    fn decode_caps_reject_oversized_payloads() {
        let p = sample();
        let head = [
            enc(&p.group.0),
            enc(&p.seq),
            enc(&p.prev),
            enc(&p.kind),
            enc(&p.root_commit),
            enc(&p.prev_root_commit),
            enc(&p.crew),
            enc(&p.bundle_root),
            enc(&p.shards),
            enc(&p.depth),
            enc(&p.salt),
            enc(&p.layout),
        ]
        .concat();
        // 머리 뒤에 감싸기·봉인·r_ct·조각을 이으면 원래 인코딩과 같아야(필드 순서 확인).
        let tail = [
            enc(&p.wraps),
            enc(&p.sealed),
            enc(&p.r_ct),
            enc(&p.part),
            enc(&p.parts),
        ]
        .concat();
        assert_eq!([head.clone(), tail].concat(), p.encode());
        // 감싸기 개수를 한도 + 1로 적고 원소는 하나만: 원소를 읽기 전에 거부.
        let mut forged = head.clone();
        forged.extend(varint(MAX_WRAPS as u64 + 1));
        forged.extend(enc(&p.wraps[0]));
        let err = KeyUpdatePayload::decode(&forged).unwrap_err();
        assert!(matches!(err, CoreError::Codec(_)), "{err:?}");
        // 봉인 상자 수(직접 인코딩 257개 이상)도.
        let mut forged = head.clone();
        forged.extend(enc(&p.wraps));
        forged.extend(varint(MAX_SEALED as u64 + 1));
        assert!(KeyUpdatePayload::decode(&forged).is_err());
        // 기기 목록에 터무니없는 길이(2^40)를 적어도 할당하지 않고 실패합니다.
        let mut forged = [enc(&p.group.0), enc(&p.seq), enc(&p.prev)].concat();
        forged.extend(varint(0)); // Advance
        forged.extend(varint(1 << 40));
        forged.extend(enc(&DeviceId([1; 32])));
        assert!(KeyUpdatePayload::decode(&forged).is_err());
        // 조각 하나에 들 수 없는 기기 수(한도 + 1)도 원소를 읽기 전에.
        let mut forged = [enc(&p.group.0), enc(&p.seq), enc(&p.prev)].concat();
        forged.extend(varint(0));
        forged.extend(varint(MAX_PART_DEVICES as u64 + 1));
        forged.extend(enc(&DeviceId([1; 32])));
        let err = KeyUpdatePayload::decode(&forged).unwrap_err();
        assert!(matches!(err, CoreError::Codec(_)), "{err:?}");
        // 칸 바뀜 수 한도.
        let mut forged = [
            enc(&p.group.0),
            enc(&p.seq),
            enc(&p.prev),
            enc(&p.kind),
            enc(&p.root_commit),
            enc(&p.prev_root_commit),
            enc(&p.crew),
            enc(&p.bundle_root),
            enc(&p.shards),
            enc(&p.depth),
            enc(&p.salt),
        ]
        .concat();
        forged.extend(varint(MAX_LAYOUT as u64 + 1));
        assert!(KeyUpdatePayload::decode(&forged).is_err());
        // 바이트 한도.
        assert_eq!(
            KeyUpdatePayload::decode(&vec![0u8; MAX_PAYLOAD_LEN + 1]),
            Err(CoreError::LimitExceeded("KeyUpdate 크기"))
        );
    }

    #[test]
    fn split_and_merge_keep_everything_under_the_cap() {
        let mut p = sample();
        p.wraps = wraps(5_000);
        p.layout = (0..2_000u32).map(|slot| LayoutOp::Clear { slot }).collect();
        p.depth = 12;
        let parts = roundtrip_split(&p, 64 * 1024);
        assert!(parts.len() > 1);
        // r_ct는 첫 조각에만, 머리는 모두 같음.
        assert!(
            parts[1..]
                .iter()
                .all(|q| q.r_ct.is_none() && q.crew == p.crew && q.bundle_root == p.bundle_root)
        );
        // 빠진 조각·섞인 갱신은 거부.
        assert!(KeyUpdatePayload::merge(parts[1..].to_vec()).is_err());
        let mut other = parts.clone();
        other[1].seq += 1;
        assert!(KeyUpdatePayload::merge(other).is_err());
        let mut other = parts.clone();
        other[1].bundle_root = None;
        assert!(KeyUpdatePayload::merge(other).is_err());
        let mut other = parts.clone();
        other[1].crew = None;
        assert!(KeyUpdatePayload::merge(other).is_err());
        // 한도 안이면 그대로 하나.
        assert_eq!(sample().split(MAX_PAYLOAD_LEN).unwrap().len(), 1);
    }

    /// 기기 목록이 조각 하나에 들지 않는 갱신(크루 크기 비공개 그룹에 3만 대 넘게 들어옴, 4만 대 밴 뒤 트리 → 직접)도
    /// 목록을 이어진 구간으로 나눠, 모든 조각이 한도 안이고 합치면 원래와 같습니다.
    #[test]
    fn long_device_lists_are_split_across_parts() {
        let mut adv = sample();
        adv.kind = KeyUpdateKind::Advance {
            joined: (0..31_400).map(devn).collect(),
        };
        adv.r_ct = None;
        adv.depth = 16;
        adv.layout = (0..31_400u32)
            .map(|slot| LayoutOp::Fill {
                slot,
                device: devn(slot),
            })
            .collect();
        adv.wraps = wraps(40_000);
        let parts = roundtrip_split(&adv, PART_TARGET_LEN);
        assert!(parts.len() >= 4, "{}", parts.len());
        assert!(parts.iter().all(|q| q.encoded_len() <= PART_TARGET_LEN));
        // 첫 조각은 목록으로 차고, 목록은 조각마다 정렬된 구간.
        assert!(parts[0].kind.joined().len() > 20_000);
        assert!(parts[1].kind.joined().len() > 1_000);

        let mut rot = sample();
        rot.kind = KeyUpdateKind::Rotate {
            removed: (0..40_000).map(devn).collect(),
            joined: (50_000..55_000).map(devn).collect(),
        };
        rot.depth = 0;
        rot.layout.clear();
        rot.wraps.clear();
        rot.bundle_root = None;
        rot.sealed = vec![SealedBox(vec![7; 300_000])];
        let parts = roundtrip_split(&rot, PART_TARGET_LEN);
        // 목록 1.5MB + 봉인 상자 0.3MB: 첫 조각은 제거 목록만, 둘째 조각에 나머지 목록과 상자.
        assert_eq!(parts.len(), 2);
        assert!(parts[0].sealed.is_empty() && parts[1].sealed.len() == 1);
        // 합친 목록의 순서가 깨지면(조각을 바꿔 끼움) 거부.
        let mut swapped = parts.clone();
        let (a, b) = (swapped[0].kind.clone(), swapped[1].kind.clone());
        swapped[0].kind = b;
        swapped[1].kind = a;
        assert!(KeyUpdatePayload::merge(swapped).is_err());
    }

    /// 조각 하나에 들지 않는 원소나 조각 수 한도를 넘는 갱신은 나누지 않고 오류입니다(발행하면 받는 쪽이 못 받음).
    #[test]
    fn split_refuses_what_cannot_be_delivered() {
        let mut p = sample();
        p.sealed = vec![SealedBox(vec![1; 70_000])];
        assert_eq!(
            p.split(64 * 1024),
            Err(CoreError::LimitExceeded("KeyUpdate 조각 크기"))
        );
        let mut p = sample();
        p.depth = 17;
        p.wraps = wraps(40_000);
        assert_eq!(
            p.split(64 * 1024),
            Err(CoreError::LimitExceeded("KeyUpdate 조각 수"))
        );
    }

    /// 조각 하나의 감싸기 한도를 넘는 갱신(큰 밴 물결)도 나눠 보내고 합쳐 검사할 수 있는지.
    #[test]
    fn merged_update_may_exceed_one_part_list_cap() {
        let mut p = sample();
        p.depth = 16;
        p.wraps = wraps(MAX_WRAPS as u32 + 100);
        assert!(p.validate().is_err());
        let parts = p.clone().split(PART_TARGET_LEN).unwrap();
        assert_eq!(parts.len(), 2);
        let decoded: Vec<_> = parts
            .iter()
            .map(|q| KeyUpdatePayload::decode(&q.encode()).unwrap())
            .collect();
        let merged = KeyUpdatePayload::merge(decoded).unwrap();
        assert_eq!(merged, p);
        merged.validate_merged().unwrap();
        // 합친 것은 조각 번호가 0/1이어야 합니다.
        assert!(parts[1].validate_merged().is_err());
    }

    /// 합친 갱신의 한도는 깊이에서 나옵니다: 트리 감싸기 2^(깊이 + 1), 덮개(깊이 0) 2^18, 칸 바뀜 2 × 2^깊이. 서명된
    /// 갱신 하나로 받는 쪽에 수백만 개의 감싸기를 열어 보게 하지 못합니다.
    #[test]
    fn merged_caps_follow_the_depth() {
        let mut p = sample();
        p.depth = 4;
        p.wraps = wraps(32);
        p.validate_merged().unwrap();
        p.wraps = wraps(33);
        assert!(p.validate_merged().is_err());
        p.wraps.clear();
        p.layout = (0..16u32)
            .flat_map(|slot| {
                [
                    LayoutOp::Clear { slot },
                    LayoutOp::Fill {
                        slot,
                        device: devn(slot),
                    },
                ]
            })
            .collect();
        p.validate_merged().unwrap();
        p.layout.push(LayoutOp::Clear { slot: 0 });
        assert!(p.validate_merged().is_err());
        // 깊이 17의 가장 큰 트리 갱신보다 하나 많은 감싸기.
        let mut big = sample();
        big.depth = MAX_DEPTH;
        big.wraps = wraps((2u32 << MAX_DEPTH) + 1);
        assert!(big.validate_merged().is_err());
        big.depth = 0;
        big.layout.clear();
        big.bundle_root = None;
        big.wraps = wraps(MAX_COVER_WRAPS as u32 + 1);
        assert!(big.validate_merged().is_err());
        big.wraps.truncate(MAX_COVER_WRAPS);
        big.validate_merged().unwrap();
    }

    /// 전체 재키의 목록도 다른 목록처럼 조각에 나눠 싣고, 합치면 원래와 같습니다. 세대가 다른 조각은 섞이지 않습니다.
    #[test]
    fn full_rekey_lists_split_and_merge() {
        let mut p = sample();
        p.kind = KeyUpdateKind::FullRekey {
            generation: 4,
            removed: (0..20_000).map(devn).collect(),
            joined: (30_000..50_000).map(devn).collect(),
        };
        p.layout.clear();
        p.depth = 16;
        let parts = roundtrip_split(&p, PART_TARGET_LEN);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].kind.is_full_rekey() && parts[1].kind.is_full_rekey());
        let mut other = parts.clone();
        other[1].kind = KeyUpdateKind::FullRekey {
            generation: 5,
            removed: Vec::new(),
            joined: Vec::new(),
        };
        assert!(KeyUpdatePayload::merge(other).is_err());
        assert!(parts[0].same_update(&parts[1]));
    }

    /// 봉투 수신자 목록: 봉투 형식(판, 감싸기 목록, 암호문)에서 기기 ID만 읽고, 다른 판·잘린 봉투는 거부합니다.
    #[test]
    fn sealed_recipients_reads_the_envelope_head() {
        #[derive(Serialize)]
        struct W {
            device: DeviceId,
            #[serde(with = "serde_bytes")]
            kem_ct: Vec<u8>,
            #[serde(with = "serde_bytes")]
            wrapped: Vec<u8>,
        }
        #[derive(Serialize)]
        struct E {
            v: u8,
            wraps: Vec<W>,
            #[serde(with = "serde_bytes")]
            ct: Vec<u8>,
        }
        let env = enc(&E {
            v: SEALED_ENVELOPE_VERSION,
            wraps: vec![
                W {
                    device: dev(1),
                    kem_ct: vec![1; 1120],
                    wrapped: vec![2; 48],
                },
                W {
                    device: dev(2),
                    kem_ct: vec![3; 1120],
                    wrapped: vec![4; 48],
                },
            ],
            ct: vec![5; 99],
        });
        assert_eq!(
            SealedBox(env.clone()).recipients().unwrap(),
            [dev(1), dev(2)]
        );
        let mut old = env.clone();
        old[0] = 1;
        assert!(sealed_recipients(&old).is_err());
        assert!(sealed_recipients(&env[..1200]).is_err());
        assert!(sealed_recipients(&[]).is_err());
        // 개수를 크게 속여도 실제 바이트가 없으면 실패합니다.
        let mut forged = vec![SEALED_ENVELOPE_VERSION];
        forged.extend(varint(1 << 40));
        assert!(sealed_recipients(&forged).is_err());
    }

    /// 키 갱신 형식과 스트림 이름은 같은 ID 타입·유도를 씁니다(`ids::KeyGroupId`).
    #[test]
    fn group_ids() {
        use crate::ids::SpaceId;
        let s = SpaceId([3; 16]);
        assert_eq!(KeyGroupId::crew(&s).0, s.0);
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/key-group");
        h.update(&s.0);
        h.update(b"a");
        assert_eq!(
            KeyGroupId::private(&s, b"a").0[..],
            h.finalize().as_bytes()[..16]
        );
        assert_ne!(KeyGroupId::private(&s, b"a"), KeyGroupId::private(&s, b"b"));
        assert_ne!(
            KeyGroupId::private(&s, b"a"),
            KeyGroupId::private(&SpaceId([4; 16]), b"a")
        );
        // 페이로드에는 길이 접두 없이 16바이트 그대로(첫 필드).
        let p = sample();
        let bytes = p.encode();
        assert_eq!(&bytes[..16], &[7u8; 16]);
        assert_eq!(bytes[16], 3, "다음은 seq");
        assert_eq!(KeyUpdatePayload::decode(&bytes).unwrap(), p);
    }
}
