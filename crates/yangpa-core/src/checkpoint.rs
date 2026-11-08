//! 소유자가 함께 서명한 상태 체크포인트(`StateCheckpoint`, protocol.md "체크포인트")와 그 주변 인증서·머클 증명.
//!
//! - 체크포인트는 상태 레인 이벤트입니다. 해소기는 그 조상까지의 상태를 정규 인코딩해 해시(`state_hash`)를 견주고, 맞으면
//!   그 상태를 새 바탕으로 삼습니다. 그 밖의 이벤트는 모두 체크포인트 뒤에 놓입니다(경계).
//! - 새 멤버는 이벤트 기록 대신 체크포인트 사슬(평문 본문)과 스냅숏을 받습니다. 옛 체크포인트 이벤트는 이미 은퇴한 상태 키로
//!   암호화돼 있어 열 수 없으므로, 본문 자체가 서명으로 스스로 검증되고([`SignedCheckpoint`]) 사슬은 이벤트 ID가 아니라
//!   본문 해시([`CheckpointId`])로 잇습니다.
//! - 소유자 신원 키는 소유자의 모든 기기에 있으므로 신원 서명만으로는 폐기된 기기를 막지 못합니다. 그래서
//!   쓴 기기가 기기 키로 함께 서명하고, 그 기기는 본문에 실린 소유자 기기 목록에서 활성이어야 하며, 그 목록은 앞
//!   체크포인트의 목록을 이어받아야 합니다(폐기를 되살리지 못함).

use serde::{Deserialize, Serialize};

use crate::device::{DeviceCosig, DeviceRecord, MemberDevices, SignedDeviceList};
use crate::error::CoreError;
use crate::ids::{
    BlobId, CheckpointId, DeviceId, EventId, KeyGroupId, SpaceId, StateKeyId, UserId,
};
use crate::sign::{Sig, SignDomain, SigningKeypair, verify};
use crate::time::Timestamp;

/// 해소기 판. 체크포인트가 더 새 판으로 만든 것이면 따르지 않고 업데이트를 요청합니다(판이 다른 해소기가 같은 체크포인트를
/// 따르면 영영 갈라짐). 2판: 이모지 한도(`EmojiLimitsSet`)를 적용하고 공간 상태에 이모지 한도·올린 사람이 더해짐.
pub const RESOLVER_VERSION: u32 = 2;
/// 경계 바로 아래에 남기는 논리 시계 폭: 체크포인트 조상 가운데 (가장 큰 논리 시계 - 이 값) 이상은 ID를 본문에 싣습니다.
/// 논리 시계가 그 아래인 늦은 이벤트는 모든 노드가 거부하고 작성자가 지금 머리에 다시 서명합니다(그 아래 이벤트를
/// 부모로만 둔 이벤트는 받음: 그 부모는 경계 뒤에서 효과가 없어, 부모를 아는지와 무관하게 같은 판정이 나옴).
pub const FRONTIER_DEPTH: u64 = 32;
/// 본문에 싣는 경계 ID 상한. 넘으면 작성자가 경계(`frontier_lamport`)를 올립니다.
pub const MAX_FRONTIER: usize = 512;
/// 체크포인트 뒤 이벤트가 쓸 수 있는 상태 키 수 상한.
pub const MAX_VALID_STATE_KEYS: usize = 64;
/// 체크포인트 하나에 싣는 새 폐기 기기 수 상한(넘는 몫은 다음 체크포인트로 넘어감).
pub const MAX_CHECKPOINT_REVOKED: usize = 1024;
/// 체크포인트 하나에 싣는 소유권 이전 인증서 수 상한.
pub const MAX_OWNER_CHAIN: usize = 16;
/// 키 그룹 머리 수 상한(크루 그룹 + 채널 상한).
pub const MAX_KEY_HEADS: usize = 1024;
/// 스냅숏(암호문) 크기 상한.
pub const MAX_SNAPSHOT_LEN: u64 = 32 * 1024 * 1024;
/// 머클 증명 경로 길이 상한(잎 2^32개까지).
pub const MAX_PROOF_PATH: usize = 2 * 32 + 2;
/// 응답 하나에 싣는 체크포인트 수 상한.
pub const MAX_CHAIN_PER_REPLY: usize = 32;

/// 키 그룹 사슬의 머리: (그룹, 순번, 그 갱신의 뿌리 약속 `root_commit`). 크루 그룹(공간 스트림)은 해소기의 크루 사슬
/// 끝이라 모든 노드가 같은 값을 확인하고, 비공개·덮개 그룹(`KeyGroup` 스트림)은 쓴 기기가 아는 이긴 사슬 끝입니다(그
/// 그룹을 볼 수 없는 멤버는 확인할 수 없어 해소기는 크루 것만 봄). 새로 자격을 얻은 기기는 이 머리에서 그룹 사슬을
/// 시작합니다(protocol.md "키 그룹과 체크포인트").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KeyHead {
    pub group: KeyGroupId,
    pub seq: u64,
    pub root_commit: [u8; 32],
    /// 비공개·덮개 그룹 머리의 사슬 정보(크루 그룹은 `None`). 모든 노드가 이 머리에서 같은 바닥을 세웁니다.
    pub chain: Option<HeadChain>,
}

/// 비공개·덮개 그룹 머리 갱신의 사슬 정보: 그 사슬을 처음부터 아는 노드가 머리에서 가진 값 그대로입니다. 머리에서
/// 시작한 노드(새 멤버·다시 시작한 노드·뒤처진 노드)도 이것으로 다음 갱신을 같은 규칙(앞 갱신 잇기, 새 T 먼저, 정확한
/// 제거 목록, 작성자 자격)으로 판정합니다. 멤버 목록 자체는 싣지 않고 요약만 싣습니다: 목록은 그룹 멤버가 중계해 주고,
/// 받는 쪽은 요약과 맞는 목록만 씁니다(누가 답했든 같음).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HeadChain {
    /// 그 갱신의 첫 조각(다음 갱신의 `prev`).
    pub head: EventId,
    /// 그 갱신이 기댄 크루 순번.
    pub crew_seq: u64,
    /// 마지막으로 새 T를 낸 갱신이 기댄 크루 순번.
    pub fresh_crew: u64,
    /// 그 갱신 뒤 멤버 요약([`members_digest`]).
    pub members: [u8; 32],
    /// 작성자(사용자, 기기). 지금도 표를 가질 자격이 있는지(보낼 때 쓸 수 있는지)를 모든 노드가 같이 셉니다.
    pub user: UserId,
    pub device: DeviceId,
}

/// 그룹 멤버 목록 요약. `members`는 오름차순이어야 합니다(`imbl::OrdSet`·`BTreeSet` 순서).
pub fn members_digest<'a>(
    group: &KeyGroupId,
    members: impl ExactSizeIterator<Item = &'a DeviceId>,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/group-members");
    h.update(group.as_bytes());
    h.update(&(members.len() as u64).to_le_bytes());
    for d in members {
        h.update(d.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// 경계 위 조상 하나(ID, 논리 시계). 새 멤버는 이것으로 경계 위 조상을 압니다(자식의 논리 시계 검사, 다음 체크포인트의
/// 경계 위 조상 계산).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FrontierEntry {
    pub id: EventId,
    pub lamport: u64,
}

/// 소유권 이전 인증서: 앞 소유자 신원과, 이전 이벤트를 쓴 그 사람의 기기가 함께 서명합니다. 체크포인트 사슬을 검증하는
/// 새 멤버는 이전 이벤트(옛 상태 키로 암호화)를 열 수 없으므로 이것으로 소유자가 바뀐 것을 확인합니다.
///
/// `base`는 이전할 때의 바탕 체크포인트입니다(서명 대상). 이것이 없으면 소유권이 돌아온 뒤에도 옛 인증서를 다음
/// 체크포인트 구간에 다시 실어 새 멤버에게 옛 이전을 보일 수 있습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipCert {
    pub space: SpaceId,
    pub base: Option<CheckpointId>,
    pub from: UserId,
    pub to: UserId,
    pub at: Timestamp,
    pub device: DeviceId,
    /// 앞 소유자 신원 서명.
    pub sig: Sig,
    /// `device`의 기기 키 서명.
    pub device_sig: Sig,
}

impl OwnershipCert {
    fn message(
        space: &SpaceId,
        base: &Option<CheckpointId>,
        from: &UserId,
        to: &UserId,
        at: Timestamp,
        device: &DeviceId,
    ) -> Vec<u8> {
        let mut m = Vec::with_capacity(16 + 33 + 32 * 3 + 8);
        m.extend_from_slice(space.as_bytes());
        match base {
            Some(b) => {
                m.push(1);
                m.extend_from_slice(&b.0);
            }
            None => m.push(0),
        }
        m.extend_from_slice(from.as_bytes());
        m.extend_from_slice(to.as_bytes());
        m.extend_from_slice(&at.0.to_le_bytes());
        m.extend_from_slice(device.as_bytes());
        m
    }

    /// 지금 소유자(`identity`)가 기기(`device`)로 `to`에게 넘기는 인증서. `base`는 지금 바탕 체크포인트입니다.
    pub fn create(
        space: SpaceId,
        base: Option<CheckpointId>,
        to: UserId,
        at: Timestamp,
        identity: &SigningKeypair,
        device: &SigningKeypair,
    ) -> Self {
        let from = UserId(identity.public());
        let dev = DeviceId(device.public());
        let msg = Self::message(&space, &base, &from, &to, at, &dev);
        OwnershipCert {
            space,
            base,
            from,
            to,
            at,
            device: dev,
            sig: identity.sign(SignDomain::OwnershipCert, &msg),
            device_sig: device.sign(SignDomain::OwnershipCert, &msg),
        }
    }

    /// 두 서명을 확인합니다(기기가 앞 소유자의 활성 기기였는지는 부르는 쪽이 봄).
    pub fn verify(&self) -> Result<(), CoreError> {
        let msg = Self::message(
            &self.space,
            &self.base,
            &self.from,
            &self.to,
            self.at,
            &self.device,
        );
        verify(
            self.from.as_bytes(),
            SignDomain::OwnershipCert,
            &msg,
            &self.sig,
        )?;
        verify(
            self.device.as_bytes(),
            SignDomain::OwnershipCert,
            &msg,
            &self.device_sig,
        )
    }
}

/// 체크포인트 작성 위임: 소유자 신원이 서명하고 소유자 기기가 함께 서명합니다. `until_seq`까지(포함)의 체크포인트를
/// `device`가 쓸 수 있습니다. 위임한 소유자 기기가 폐기되면 위임도 효력이 없습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationCert {
    pub space: SpaceId,
    pub owner: UserId,
    pub device: DeviceId,
    pub until_seq: u64,
    pub sig: Sig,
    pub cosig: DeviceCosig,
}

impl DelegationCert {
    fn message(space: &SpaceId, owner: &UserId, device: &DeviceId, until_seq: u64) -> Vec<u8> {
        let mut m = Vec::with_capacity(16 + 64 + 8);
        m.extend_from_slice(space.as_bytes());
        m.extend_from_slice(owner.as_bytes());
        m.extend_from_slice(device.as_bytes());
        m.extend_from_slice(&until_seq.to_le_bytes());
        m
    }

    pub fn create(
        space: SpaceId,
        device: DeviceId,
        until_seq: u64,
        identity: &SigningKeypair,
        owner_device: &SigningKeypair,
    ) -> Self {
        let owner = UserId(identity.public());
        let msg = Self::message(&space, &owner, &device, until_seq);
        DelegationCert {
            space,
            owner,
            device,
            until_seq,
            sig: identity.sign(SignDomain::CheckpointDelegation, &msg),
            cosig: DeviceCosig {
                device: DeviceId(owner_device.public()),
                sig: owner_device.sign(SignDomain::CheckpointDelegation, &msg),
            },
        }
    }

    /// 인증서 전체의 해시. 체크포인트 본문이 다음 체크포인트를 쓸 수 있는 위임을 이것으로 밝힙니다
    /// ([`CheckpointBody::delegation_hash`]).
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/checkpoint-delegation");
        h.update(&postcard::to_stdvec(self).expect("직렬화 실패 없음"));
        *h.finalize().as_bytes()
    }

    /// 두 서명을 확인합니다(함께 서명한 기기가 소유자의 활성 기기인지는 부르는 쪽이 봄).
    pub fn verify(&self) -> Result<(), CoreError> {
        let msg = Self::message(&self.space, &self.owner, &self.device, self.until_seq);
        verify(
            self.owner.as_bytes(),
            SignDomain::CheckpointDelegation,
            &msg,
            &self.sig,
        )?;
        verify(
            self.cosig.device.as_bytes(),
            SignDomain::CheckpointDelegation,
            &msg,
            &self.cosig.sig,
        )
    }
}

/// 정렬 머클 트리의 구간 증명: 잎 `start..start+k`(값은 따로 줌)를 뿌리까지 올리는 형제 해시. 잎 하나의 포함 증명은
/// `k = 1`, 없음 증명은 이웃한 두 잎(또는 맨 앞·맨 뒤 잎 하나)의 구간 증명입니다([`merkle`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleProof {
    pub start: u64,
    pub path: Vec<[u8; 32]>,
}

/// 체크포인트 본문(서명 대상). 크기를 묶는 한도는 [`CheckpointBody::check_shape`]가 봅니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointBody {
    pub space: SpaceId,
    /// 1부터 1씩 오름.
    pub seq: u64,
    /// 앞 체크포인트(첫 체크포인트면 `None`).
    pub prev: Option<CheckpointId>,
    /// 이 체크포인트 이벤트의 부모(정렬). 해소기는 헤더의 부모와 같은지 봅니다.
    pub heads: Vec<EventId>,
    /// 경계 F. 조상 가운데 논리 시계가 F 이상인 것은 모두 `frontier`에 있습니다.
    pub frontier_lamport: u64,
    /// 경계 위 조상(ID 순).
    pub frontier: Vec<FrontierEntry>,
    /// 해소기 상태 전체의 정규 인코딩(스냅숏 평문) 해시.
    pub state_hash: [u8; 32],
    /// 사용자 기준 정렬 머클 트리의 뿌리와 잎 수.
    pub user_root: [u8; 32],
    pub users: u64,
    /// 기기 기준 정렬 머클 트리의 뿌리와 잎 수.
    pub device_root: [u8; 32],
    pub devices: u64,
    /// 암호화한 스냅숏 블롭과 그 크기.
    pub snapshot: BlobId,
    pub snapshot_len: u64,
    /// 키 그룹 머리(그룹 순).
    pub key_heads: Vec<KeyHead>,
    /// 이 체크포인트 뒤 이벤트가 쓸 수 있는 옛 상태 키(정렬). 여기 없고 체크포인트 전에 쓰인 키는 은퇴합니다.
    pub valid_state_keys: Vec<StateKeyId>,
    pub owner: UserId,
    /// 소유자의 서명된 기기 목록과 사용자 트리에서의 포함 증명.
    pub owner_devices: SignedDeviceList,
    pub owner_proof: MerkleProof,
    /// 앞 체크포인트 뒤 새로 폐기된 기기(정렬)와, 이것까지 이은 해시 사슬 폐기 기록의 뿌리.
    pub revoked: Vec<DeviceId>,
    pub revocation_root: [u8; 32],
    /// 앞 체크포인트 뒤의 소유권 이전 인증서(순서대로).
    pub owner_chain: Vec<OwnershipCert>,
    pub resolver_version: u32,
    /// 쓴 기기(이벤트 작성 기기와 같음).
    pub author: DeviceId,
    /// 위임받은 기기가 썼으면 그 위임(그때는 소유자 신원 서명이 없음). 앞 체크포인트가 `delegation_hash`로 밝힌
    /// 위임이어야 합니다.
    pub delegation: Option<DelegationCert>,
    /// 이 체크포인트 때 상태에 있는 위임의 해시([`DelegationCert::hash`]): 위임받은 기기와 위임한 소유자 기기가 둘 다
    /// 활성일 때만. 다음 체크포인트는 이 위임으로만 위임받아 쓸 수 있습니다(바꾼 위임·쫓겨난 기기의 옛 인증서로 새 멤버
    /// 사슬을 만들지 못하게).
    pub delegation_hash: Option<[u8; 32]>,
    /// 소유자가 바뀐 구간에서 서명 기기의 증명: 두 번째 이후 인증서의 기기와 이 체크포인트를 쓴 기기가 앞 체크포인트
    /// 기기 트리에서 그 사용자의 활성 기기라는 잎 증명(`checkpoint`가 앞 체크포인트). 새 멤버는 앞 체크포인트만으로 새
    /// 소유자의 기기를 확인할 수 없어 이것을 봅니다.
    pub signer_proofs: Vec<DeviceLeafProof>,
}

/// 서명된 체크포인트(`EventBody::StateCheckpoint`의 내용).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCheckpoint {
    pub body: CheckpointBody,
    /// 소유자 신원 서명(자기 검증형). 위임이면 `None`.
    pub owner_sig: Option<Sig>,
    /// 쓴 기기의 서명.
    pub device_sig: Sig,
}

/// 해시 사슬 폐기 기록의 다음 뿌리.
pub fn revocation_root(prev: &[u8; 32], seq: u64, revoked: &[DeviceId]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/revocation-log");
    h.update(prev);
    h.update(&seq.to_le_bytes());
    h.update(&(revoked.len() as u64).to_le_bytes());
    for d in revoked {
        h.update(d.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// 스냅숏 평문(정규 인코딩)의 해시.
pub fn state_hash(snapshot_plain: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/checkpoint-state");
    h.update(snapshot_plain);
    *h.finalize().as_bytes()
}

fn sorted_unique<T: Ord>(v: &[T]) -> bool {
    v.windows(2).all(|w| w[0] < w[1])
}

impl CheckpointBody {
    /// 서명하는 바이트.
    pub fn signed_bytes(&self) -> Result<Vec<u8>, CoreError> {
        Ok(postcard::to_stdvec(self)?)
    }

    /// 그룹 `group`의 키 머리(없으면 `None`: 그 그룹에 갱신이 없거나 쓴 기기가 모르는 그룹).
    pub fn key_head(&self, group: &KeyGroupId) -> Option<&KeyHead> {
        self.key_heads
            .binary_search_by(|k| k.group.cmp(group))
            .ok()
            .map(|i| &self.key_heads[i])
    }

    /// 상태와 무관한 모양 검사: 한도, 정렬, 경계 ID가 경계 위인지, 위임과 서명 종류.
    pub fn check_shape(&self) -> Result<(), CoreError> {
        if self.seq == 0 || (self.seq == 1) != self.prev.is_none() {
            return Err(CoreError::InvalidEncoding);
        }
        if self.heads.is_empty()
            || self.heads.len() > crate::event::MAX_PARENTS
            || !sorted_unique(&self.heads)
        {
            return Err(CoreError::InvalidEncoding);
        }
        if self.frontier.len() > MAX_FRONTIER {
            return Err(CoreError::LimitExceeded("체크포인트 경계 수"));
        }
        let ids: Vec<EventId> = self.frontier.iter().map(|f| f.id).collect();
        if !sorted_unique(&ids)
            || self
                .frontier
                .iter()
                .any(|f| f.lamport < self.frontier_lamport)
        {
            return Err(CoreError::InvalidEncoding);
        }
        if self.key_heads.len() > MAX_KEY_HEADS
            || !self.key_heads.windows(2).all(|w| w[0].group < w[1].group)
            || self.key_heads.iter().any(|k| k.seq == 0)
        {
            return Err(CoreError::LimitExceeded("키 그룹 머리 수"));
        }
        // 크루 머리는 해소기가 확인하므로 사슬 정보가 없고, 비공개·덮개 그룹 머리에는 꼭 있습니다.
        let crew = KeyGroupId::crew(&self.space);
        if self
            .key_heads
            .iter()
            .any(|k| (k.group == crew) == k.chain.is_some())
        {
            return Err(CoreError::InvalidEncoding);
        }
        if self.valid_state_keys.is_empty()
            || self.valid_state_keys.len() > MAX_VALID_STATE_KEYS
            || !sorted_unique(&self.valid_state_keys)
        {
            return Err(CoreError::LimitExceeded("유효한 상태 키 수"));
        }
        if self.revoked.len() > MAX_CHECKPOINT_REVOKED || !sorted_unique(&self.revoked) {
            return Err(CoreError::LimitExceeded("폐기 기기 수"));
        }
        if self.owner_chain.len() > MAX_OWNER_CHAIN {
            return Err(CoreError::LimitExceeded("소유권 인증서 수"));
        }
        if self.owner_proof.path.len() > MAX_PROOF_PATH
            || self
                .signer_proofs
                .iter()
                .any(|p| p.proof.path.len() > MAX_PROOF_PATH)
        {
            return Err(CoreError::LimitExceeded("증명 길이"));
        }
        if self.signer_proofs.len() > MAX_OWNER_CHAIN + 1 {
            return Err(CoreError::LimitExceeded("서명 기기 증명 수"));
        }
        if self.snapshot_len > MAX_SNAPSHOT_LEN {
            return Err(CoreError::LimitExceeded("스냅숏 크기"));
        }
        if self.owner_devices.list.user != self.owner {
            return Err(CoreError::InvalidEncoding);
        }
        if let Some(d) = &self.delegation
            && (d.space != self.space || d.device != self.author || d.owner != self.owner)
        {
            return Err(CoreError::InvalidEncoding);
        }
        Ok(())
    }
}

impl SignedCheckpoint {
    /// 소유자 신원과 쓴 기기가 서명합니다(소유자 경로).
    pub fn sign_owner(
        body: CheckpointBody,
        identity: &SigningKeypair,
        device: &SigningKeypair,
    ) -> Result<Self, CoreError> {
        let bytes = body.signed_bytes()?;
        Ok(SignedCheckpoint {
            owner_sig: Some(identity.sign(SignDomain::Checkpoint, &bytes)),
            device_sig: device.sign(SignDomain::CheckpointDevice, &bytes),
            body,
        })
    }

    /// 위임받은 기기만 서명합니다(`body.delegation`이 있어야 함).
    pub fn sign_delegated(
        body: CheckpointBody,
        device: &SigningKeypair,
    ) -> Result<Self, CoreError> {
        let bytes = body.signed_bytes()?;
        Ok(SignedCheckpoint {
            owner_sig: None,
            device_sig: device.sign(SignDomain::CheckpointDevice, &bytes),
            body,
        })
    }

    /// 본문 해시(사슬을 잇는 ID). 서명은 FN-DSA 부분의 지문만 넣습니다(이벤트 ID와 같은 이유).
    pub fn id(&self) -> CheckpointId {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/checkpoint-id");
        h.update(&postcard::to_stdvec(&self.body).expect("직렬화 실패 없음"));
        match &self.owner_sig {
            Some(s) => {
                h.update(&[1]);
                h.update(&s.fingerprint());
            }
            None => {
                h.update(&[0]);
            }
        }
        h.update(&self.device_sig.fingerprint());
        CheckpointId(*h.finalize().as_bytes())
    }

    /// 모양과 서명(소유자 신원 또는 위임, 쓴 기기)을 확인합니다. 쓴 기기가 소유자 기기 목록에서 활성인지, 그 목록이 앞
    /// 체크포인트의 것을 이어받는지, 위임한 기기가 활성인지는 부르는 쪽이 봅니다(앞 상태가 필요함).
    pub fn verify(&self) -> Result<(), CoreError> {
        self.body.check_shape()?;
        let bytes = self.body.signed_bytes()?;
        match (&self.owner_sig, &self.body.delegation) {
            (Some(sig), None) => verify(
                self.body.owner.as_bytes(),
                SignDomain::Checkpoint,
                &bytes,
                sig,
            )?,
            (None, Some(d)) => {
                d.verify()?;
                if d.until_seq < self.body.seq {
                    return Err(CoreError::Expired);
                }
            }
            _ => return Err(CoreError::InvalidEncoding),
        }
        verify(
            self.body.author.as_bytes(),
            SignDomain::CheckpointDevice,
            &bytes,
            &self.device_sig,
        )?;
        self.body.owner_devices.verify()?;
        for c in &self.body.owner_chain {
            if c.space != self.body.space {
                return Err(CoreError::InvalidEncoding);
            }
            c.verify()?;
        }
        Ok(())
    }
}

/// 초대 링크에 고정하는 체크포인트.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointPin {
    pub seq: u64,
    pub id: CheckpointId,
}

/// 정렬 머클 트리(BLAKE3). 잎은 (키, 값 해시)를 트리 이름과 함께 묶은 해시이고 키 순으로 둡니다. 한 층을 올릴 때 짝이
/// 없는 마지막 마디는 그대로 올라갑니다. 정렬돼 있으므로 이웃한 두 잎의 구간 증명이 그 사이 키의 없음 증명입니다.
pub mod merkle {
    use super::MerkleProof;

    /// 사용자 트리 이름.
    pub const USERS: &[u8] = b"users";
    /// 기기 트리 이름.
    pub const DEVICES: &[u8] = b"devices";

    pub fn leaf(tree: &[u8], key: &[u8], value: &[u8; 32]) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/merkle-leaf");
        h.update(&(tree.len() as u64).to_le_bytes());
        h.update(tree);
        h.update(&(key.len() as u64).to_le_bytes());
        h.update(key);
        h.update(value);
        *h.finalize().as_bytes()
    }

    fn node(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/merkle-node");
        h.update(l);
        h.update(r);
        *h.finalize().as_bytes()
    }

    fn empty(tree: &[u8]) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/merkle-empty");
        h.update(tree);
        *h.finalize().as_bytes()
    }

    fn up(level: &[[u8; 32]]) -> Vec<[u8; 32]> {
        level
            .chunks(2)
            .map(|c| {
                if c.len() == 2 {
                    node(&c[0], &c[1])
                } else {
                    c[0]
                }
            })
            .collect()
    }

    /// 잎 해시들(키 순)의 뿌리.
    pub fn root(tree: &[u8], leaves: &[[u8; 32]]) -> [u8; 32] {
        if leaves.is_empty() {
            return empty(tree);
        }
        let mut level = leaves.to_vec();
        while level.len() > 1 {
            level = up(&level);
        }
        level[0]
    }

    /// 잎부터 뿌리까지의 모든 층. 같은 트리에서 증명을 여럿 만들 때 한 번만 셉니다([`range_proof_in`]).
    pub fn levels(leaves: &[[u8; 32]]) -> Vec<Vec<[u8; 32]>> {
        let mut out = vec![leaves.to_vec()];
        while let Some(last) = out.last()
            && last.len() > 1
        {
            let next = up(last);
            out.push(next);
        }
        out
    }

    /// 잎 `start..end`의 구간 증명.
    pub fn range_proof(leaves: &[[u8; 32]], start: usize, end: usize) -> Option<MerkleProof> {
        range_proof_in(&levels(leaves), start, end)
    }

    /// 미리 센 층([`levels`])에서 잎 `start..end`의 구간 증명.
    pub fn range_proof_in(
        levels: &[Vec<[u8; 32]>],
        start: usize,
        end: usize,
    ) -> Option<MerkleProof> {
        let n = levels.first()?.len();
        if start >= end || end > n {
            return None;
        }
        let mut path = Vec::new();
        let (mut lo, mut hi) = (start, end);
        for level in levels.iter().take_while(|l| l.len() > 1) {
            if lo % 2 == 1 {
                path.push(level[lo - 1]);
            }
            if hi % 2 == 1 && hi < level.len() {
                path.push(level[hi]);
            }
            lo /= 2;
            hi = hi.div_ceil(2);
        }
        Some(MerkleProof {
            start: start as u64,
            path,
        })
    }

    /// 잎 `proof.start..proof.start + values.len()`가 잎 `n`개 트리의 뿌리 `root`에 들어 있는지.
    pub fn verify_range(root: &[u8; 32], n: u64, values: &[[u8; 32]], proof: &MerkleProof) -> bool {
        let k = values.len() as u64;
        if k == 0 || proof.path.len() > super::MAX_PROOF_PATH {
            return false;
        }
        let Some(end) = proof.start.checked_add(k) else {
            return false;
        };
        if end > n {
            return false;
        }
        let mut lo = proof.start;
        let mut hi = end;
        let mut len = n;
        let mut cur: Vec<[u8; 32]> = values.to_vec();
        let mut path = proof.path.iter();
        while len > 1 {
            if lo % 2 == 1 {
                let Some(s) = path.next() else { return false };
                cur.insert(0, *s);
                lo -= 1;
            }
            if hi % 2 == 1 && hi < len {
                let Some(s) = path.next() else { return false };
                cur.push(*s);
                hi += 1;
            }
            cur = up(&cur);
            lo /= 2;
            hi = hi.div_ceil(2);
            len = len.div_ceil(2);
        }
        path.next().is_none() && cur.len() == 1 && cur[0] == *root
    }

    /// 없음 증명 확인: `neighbors`(키, 값 해시)가 증명의 구간 잎이고, `key`가 그 사이(또는 맨 앞 앞·맨 뒤 뒤)에 있어 트리에
    /// 없음을 보입니다. 키는 트리마다 정한 바이트 순입니다.
    pub fn verify_absence(
        tree: &[u8],
        root: &[u8; 32],
        n: u64,
        key: &[u8],
        neighbors: &[(Vec<u8>, [u8; 32])],
        proof: &MerkleProof,
    ) -> bool {
        if n == 0 {
            return neighbors.is_empty() && *root == empty(tree);
        }
        let leaves: Vec<[u8; 32]> = neighbors.iter().map(|(k, v)| leaf(tree, k, v)).collect();
        if !verify_range(root, n, &leaves, proof) {
            return false;
        }
        match neighbors {
            [only] if proof.start == 0 && n >= 1 && key < only.0.as_slice() => true,
            [only] if proof.start + 1 == n && key > only.0.as_slice() => true,
            [a, b] => a.0.as_slice() < key && key < b.0.as_slice(),
            _ => false,
        }
    }
}

/// 사용자 트리 잎의 값: 그 사용자의 간추린 기기 기록(없으면 빈 값).
pub fn user_value(record: Option<&MemberDevices>) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/user-leaf");
    h.update(&postcard::to_stdvec(&record).expect("직렬화 실패 없음"));
    *h.finalize().as_bytes()
}

/// 기기 트리의 키: (기기, 사용자). 한 기기가 두 사용자 기록에 있으면 이웃한 잎이 되어 구간 증명으로 드러납니다.
pub fn device_key(device: &DeviceId, user: &UserId) -> Vec<u8> {
    let mut k = Vec::with_capacity(64);
    k.extend_from_slice(device.as_bytes());
    k.extend_from_slice(user.as_bytes());
    k
}

/// 기기 트리 잎의 값: 간추린 기기 기록(키 약속 포함).
pub fn device_value(record: &DeviceRecord) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/device-leaf");
    h.update(&postcard::to_stdvec(record).expect("직렬화 실패 없음"));
    *h.finalize().as_bytes()
}

/// 기기 키 하나에 붙인 증명: 체크포인트 기기 트리의 잎(사용자, 기록)과 그 포함 증명. 받는 쪽은 아는 체크포인트의
/// `device_root`로 확인하고, 키가 기록의 키 약속과 맞는지 봅니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLeafProof {
    pub checkpoint: CheckpointId,
    pub user: UserId,
    pub record: DeviceRecord,
    pub proof: MerkleProof,
}

impl DeviceLeafProof {
    /// 잎 `devices`개의 기기 트리 뿌리 `root`에 이 잎이 있는지.
    pub fn verify(&self, root: &[u8; 32], devices: u64) -> bool {
        let leaf = merkle::leaf(
            merkle::DEVICES,
            &device_key(&self.record.device, &self.user),
            &device_value(&self.record),
        );
        merkle::verify_range(root, devices, &[leaf], &self.proof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<(Vec<u8>, [u8; 32])> {
        (0..n)
            .map(|i| {
                (
                    vec![(i * 2) as u8 + 1],
                    *blake3::hash(&[i as u8]).as_bytes(),
                )
            })
            .collect()
    }

    #[test]
    fn range_and_absence_proofs_cover_every_shape() {
        for n in 1..=17usize {
            let kv = leaves(n);
            let hs: Vec<[u8; 32]> = kv
                .iter()
                .map(|(k, v)| merkle::leaf(merkle::USERS, k, v))
                .collect();
            let root = merkle::root(merkle::USERS, &hs);
            for a in 0..n {
                for b in a + 1..=n {
                    let p = merkle::range_proof(&hs, a, b).unwrap();
                    assert!(
                        merkle::verify_range(&root, n as u64, &hs[a..b], &p),
                        "{n} {a}..{b}"
                    );
                    let mut bad = hs[a..b].to_vec();
                    bad[0][0] ^= 1;
                    assert!(!merkle::verify_range(&root, n as u64, &bad, &p));
                    // 다른 위치라고 우기면 실패.
                    if b < n {
                        let mut moved = p.clone();
                        moved.start += 1;
                        assert!(!merkle::verify_range(&root, n as u64, &hs[a..b], &moved));
                    }
                }
            }
            // 짝수 키는 없음: 이웃한 두 잎으로 증명.
            for i in 0..n - 1 {
                let p = merkle::range_proof(&hs, i, i + 2).unwrap();
                let key = [(i * 2 + 2) as u8];
                assert!(merkle::verify_absence(
                    merkle::USERS,
                    &root,
                    n as u64,
                    &key,
                    &kv[i..i + 2],
                    &p
                ));
                // 있는 키는 없다고 증명할 수 없음.
                assert!(!merkle::verify_absence(
                    merkle::USERS,
                    &root,
                    n as u64,
                    &kv[i].0,
                    &kv[i..i + 2],
                    &p
                ));
            }
            let first = merkle::range_proof(&hs, 0, 1).unwrap();
            assert!(merkle::verify_absence(
                merkle::USERS,
                &root,
                n as u64,
                &[0],
                &kv[..1],
                &first
            ));
            let last = merkle::range_proof(&hs, n - 1, n).unwrap();
            assert!(merkle::verify_absence(
                merkle::USERS,
                &root,
                n as u64,
                &[255],
                &kv[n - 1..],
                &last
            ));
        }
        assert!(merkle::verify_absence(
            merkle::USERS,
            &merkle::root(merkle::USERS, &[]),
            0,
            &[1],
            &[],
            &MerkleProof::default()
        ));
    }

    #[test]
    fn certificates_bind_their_fields() {
        let space = SpaceId([1; 16]);
        let owner = SigningKeypair::from_seed(&[1; 32]);
        let dev = SigningKeypair::from_seed(&[2; 32]);
        let to = UserId([3; 32]);
        let base = Some(CheckpointId([6; 32]));
        let c = OwnershipCert::create(space, base, to, Timestamp(5), &owner, &dev);
        c.verify().unwrap();
        let mut bad = c.clone();
        bad.to = UserId([4; 32]);
        assert!(bad.verify().is_err());
        // 바탕도 서명 대상: 다른 구간에 옮겨 실을 수 없습니다.
        let mut moved = c.clone();
        moved.base = Some(CheckpointId([7; 32]));
        assert!(moved.verify().is_err());
        moved.base = None;
        assert!(moved.verify().is_err());
        let d = DelegationCert::create(space, DeviceId([9; 32]), 7, &owner, &dev);
        d.verify().unwrap();
        let mut bad = d.clone();
        bad.until_seq = 8;
        assert!(bad.verify().is_err());
    }

    #[test]
    fn revocation_root_chains() {
        let a = revocation_root(&[0; 32], 1, &[DeviceId([1; 32])]);
        let b = revocation_root(&a, 2, &[]);
        assert_ne!(a, b);
        assert_ne!(a, revocation_root(&[0; 32], 1, &[]));
    }
}
