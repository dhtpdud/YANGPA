//! 체크포인트 사슬 검증(새 멤버·재시작)과 기기 키 증명 확인.
//!
//! 해소기([`crate::state`])는 앞 상태로 체크포인트를 끝까지 확인하지만, 체크포인트로 들어오는 멤버는 앞 상태가 없습니다.
//! 여기서는 본문만으로 볼 수 있는 것을 봅니다: 초대 링크의 고정에서 이어지는지, 서명, 해소기 판, 소유자 연속성(앞
//! 체크포인트에 묶인 소유권 인증서), 소유자 기기 목록이 폐기를 되돌리지 않는지, 쓴 기기가 **앞** 체크포인트 기준으로
//! 자격이 있는지(활성 소유자 기기, 앞 체크포인트가 밝힌 위임, 새 소유자 기기의 잎 증명), 폐기 기록 해시 사슬, 소유자
//! 증명, 마지막 스냅숏의 해시. 스냅숏 상태가 본문(해시·트리 뿌리·소유자 기록)과 맞는지는
//! [`crate::Resolver::from_checkpoint`]가 봅니다.

use std::collections::HashSet;

use thiserror::Error;
use yangpa_core::checkpoint::{
    self as ckpt, CheckpointPin, DeviceLeafProof, RESOLVER_VERSION, SignedCheckpoint, merkle,
};
use yangpa_core::device::{DeviceRecord, MemberDevices};
use yangpa_core::ids::{CheckpointId, DeviceId, SpaceId, UserId};
use yangpa_core::wire::DeviceKeysItem;

use crate::state::{Machine, check_owner_proof};

/// 체크포인트 사슬을 받아들일 수 없는 이유.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ChainError {
    #[error("빈 체크포인트 사슬")]
    Empty,
    #[error("고정한 체크포인트와 다름")]
    PinMismatch,
    #[error("더 새 해소기 판의 체크포인트(업데이트 필요)")]
    NewerResolver,
    #[error("체크포인트 {seq}: {reason}")]
    Invalid { seq: u64, reason: &'static str },
    #[error("스냅숏 해시가 체크포인트와 다름")]
    Snapshot,
}

/// 고정한 체크포인트(`pin`, 초대 링크나 이미 믿는 바탕)에서 시작하는 사슬 `chain`(첫 항목이 고정한 것)을 확인하고 마지막
/// 체크포인트를 돌려줍니다. `snapshot`이 있으면 마지막 체크포인트의 스냅숏 평문인지 해시로 봅니다.
///
/// 다음 체크포인트를 쓸 자격은 늘 **앞** 체크포인트만으로 봅니다(실린 목록은 쓴 쪽이 지어낼 수 있음):
/// - 소유자가 같으면 쓴 기기가 앞 체크포인트 소유자 기기 목록에서 활성이고, 실린 목록이 앞 목록과 같거나 이어받아야
///   합니다. 소유자 신원 키만 가진 사람(폐기된 기기, 시드를 훔친 사람)이 새 기기를 지어내 서명하지 못합니다.
/// - 위임으로 쓰면 앞 체크포인트가 밝힌 위임(`delegation_hash`)이어야 합니다.
/// - 소유자가 바뀌었으면 인증서가 모두 앞 체크포인트에 묶이고(`base`), 첫 인증서의 기기는 앞 소유자 목록에서, 그 뒤
///   인증서의 기기와 쓴 기기는 앞 체크포인트 기기 트리의 잎 증명(`signer_proofs`)으로 그 사용자의 활성 기기여야 합니다.
///
/// 그래서 앞 체크포인트 뒤에 더한 기기가 쓴 체크포인트는 여기서 받지 않습니다(해소기는 받음). 그때 새 멤버는 처음부터
/// 따라잡습니다. 앞 체크포인트 때 자격이 있던 기기가 그 뒤 폐기돼도 이것만으로는 알 수 없으므로, 데몬은 다른 출처와
/// 머리를 견줍니다.
pub fn validate_chain(
    pin: &CheckpointPin,
    chain: &[SignedCheckpoint],
    snapshot: Option<&[u8]>,
) -> Result<CheckpointPin, ChainError> {
    let first = chain.first().ok_or(ChainError::Empty)?;
    if first.body.seq != pin.seq || first.id() != pin.id {
        return Err(ChainError::PinMismatch);
    }
    check_one(first)?;
    // 사슬에서 본 폐기 기기(고정한 체크포인트 앞의 폐기는 그 소유자 기기 목록·기기 트리에 폐기로 남아 있음).
    let mut revoked: HashSet<DeviceId> = first.body.revoked.iter().copied().collect();
    let mut prev = first;
    let mut prev_id = pin.id;
    for c in &chain[1..] {
        check_one(c)?;
        check_link(prev, prev_id, c, &revoked)?;
        revoked.extend(c.body.revoked.iter().copied());
        prev = c;
        prev_id = c.id();
    }
    if let Some(s) = snapshot
        && ckpt::state_hash(s) != prev.body.state_hash
    {
        return Err(ChainError::Snapshot);
    }
    Ok(CheckpointPin {
        seq: prev.body.seq,
        id: prev_id,
    })
}

/// 다른 출처가 알려 준 체크포인트 머리가 내 머리와 어떤 관계인지([`compare_heads`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadAgreement {
    /// 같은 머리.
    Same,
    /// 내 머리에서 확인되는 사슬로 이어지는 더 새 머리.
    Extends,
    /// 내 순번에 서명이 맞는 다른 체크포인트가 있음(갈라진 사슬: 처음부터 따라잡아야 함).
    Conflict,
    /// 판정할 수 없음(뒤처진 출처, 확인되지 않는 사슬).
    Unknown,
}

/// 새 멤버가 받아 준 노드에게서 받아 설치한 머리 `mine`을 다른 출처의 답(`head`와 내 순번부터의 사슬 `chain`)과
/// 견줍니다. 받아 준 노드 하나는 링크 고정 뒤 소유권 이전을 숨긴 사슬(예: 소유권을 넘긴 옛 소유자가 지어낸 것)을 보일
/// 수 있으므로, 같은 순번에 서명이 맞는 다른 체크포인트가 보이면 갈라진 것으로 봅니다. 서명이 맞지 않는 답은 무시합니다
/// (아무나 새 멤버를 처음부터 따라잡게 만들지 못하게).
pub fn compare_heads(
    mine: &CheckpointPin,
    space: &SpaceId,
    head: &CheckpointPin,
    chain: &[SignedCheckpoint],
) -> HeadAgreement {
    if head == mine {
        return HeadAgreement::Same;
    }
    if head.seq < mine.seq {
        return HeadAgreement::Unknown;
    }
    let Some(first) = chain.first() else {
        return HeadAgreement::Unknown;
    };
    let id = first.id();
    if first.body.seq != mine.seq || first.body.space != *space || check_one(first).is_err() {
        return HeadAgreement::Unknown;
    }
    if id != mine.id {
        return HeadAgreement::Conflict;
    }
    match validate_chain(mine, chain, None) {
        Ok(end) if end == *head => HeadAgreement::Extends,
        _ => HeadAgreement::Unknown,
    }
}

/// 앞 체크포인트 `p`(ID `prev_id`)만으로 다음 체크포인트 `c`를 확인합니다([`validate_chain`]).
fn check_link(
    p: &SignedCheckpoint,
    prev_id: CheckpointId,
    c: &SignedCheckpoint,
    revoked: &HashSet<DeviceId>,
) -> Result<(), ChainError> {
    let (b, pb) = (&c.body, &p.body);
    let bad = |reason| ChainError::Invalid { seq: b.seq, reason };
    if b.space != pb.space {
        return Err(bad("다른 공간"));
    }
    if b.prev != Some(prev_id) || b.seq != pb.seq + 1 {
        return Err(bad("앞 체크포인트를 잇지 않음"));
    }
    if b.revocation_root != ckpt::revocation_root(&pb.revocation_root, b.seq, &b.revoked) {
        return Err(bad("폐기 기록 사슬"));
    }
    let old = MemberDevices::from_list(&pb.owner_devices.list);
    // 앞 체크포인트 기기 트리에서 `user`의 활성 기기인지(잎 증명).
    let active_at_prev = |device: &DeviceId, user: &UserId| {
        b.signer_proofs.iter().any(|x| {
            x.checkpoint == prev_id
                && x.user == *user
                && x.record.device == *device
                && x.record.revoked_at.is_none()
                && x.verify(&pb.device_root, pb.devices)
        })
    };
    match b.owner_chain.as_slice() {
        [] => {
            if b.owner != pb.owner {
                return Err(bad("인증서 없이 바뀐 소유자"));
            }
            let new = MemberDevices::from_list(&b.owner_devices.list);
            // 폐기를 되돌리거나 키를 바꾼 목록은 받지 않습니다(같은 판이면 같은 목록이어야 함).
            if new != old && !new.extends(&old) {
                return Err(bad("소유자 기기 목록이 앞 것을 잇지 않음"));
            }
            match &b.delegation {
                None => {
                    if !old.contains_active(&b.author) {
                        return Err(bad("앞 체크포인트 소유자의 활성 기기가 쓰지 않음"));
                    }
                }
                Some(d) => {
                    if pb.delegation_hash != Some(d.hash()) {
                        return Err(bad("앞 체크포인트가 밝히지 않은 위임"));
                    }
                }
            }
        }
        certs => {
            if b.delegation.is_some() {
                // 위임은 소유자의 것이라 소유권이 바뀌면 사라집니다(해소기도 받지 않음).
                return Err(bad("소유자가 바뀐 구간의 위임"));
            }
            if certs.iter().any(|x| x.base != Some(prev_id)) {
                return Err(bad("앞 체크포인트에 묶이지 않은 소유권 인증서"));
            }
            if certs[0].from != pb.owner || !old.contains_active(&certs[0].device) {
                return Err(bad("첫 소유권 이전의 기기"));
            }
            if certs.windows(2).any(|w| w[0].to != w[1].from)
                || certs.last().is_some_and(|x| x.to != b.owner)
            {
                return Err(bad("소유권 인증서 사슬"));
            }
            if certs[1..]
                .iter()
                .any(|x| !active_at_prev(&x.device, &x.from))
            {
                return Err(bad("소유권 이전 기기의 증명"));
            }
            if !active_at_prev(&b.author, &b.owner) {
                return Err(bad("새 소유자 기기의 증명"));
            }
            if certs.iter().any(|x| revoked.contains(&x.device)) {
                return Err(bad("폐기된 기기의 소유권 이전"));
            }
        }
    }
    let cosigner = b.delegation.as_ref().map_or(b.author, |d| d.cosig.device);
    if revoked.contains(&b.author)
        || revoked.contains(&cosigner)
        || b.revoked.contains(&b.author)
        || b.revoked.contains(&cosigner)
    {
        return Err(bad("폐기된 기기가 씀"));
    }
    Ok(())
}

/// 체크포인트 하나만으로 볼 수 있는 것: 판, 모양·서명, 쓴(위임이면 위임한) 기기가 실린 소유자 기기 목록에서 활성인지,
/// 소유자 증명. 실린 목록은 쓴 쪽이 만들 수 있으므로 쓴 기기의 자격은 앞 체크포인트로 다시 봅니다([`check_link`]; 사슬의
/// 첫 항목은 고정으로 믿음).
fn check_one(c: &SignedCheckpoint) -> Result<(), ChainError> {
    let b = &c.body;
    let bad = |reason| ChainError::Invalid { seq: b.seq, reason };
    if b.resolver_version > RESOLVER_VERSION {
        return Err(ChainError::NewerResolver);
    }
    c.verify().map_err(|_| bad("서명·모양"))?;
    let list = MemberDevices::from_list(&b.owner_devices.list);
    let cosigner = b.delegation.as_ref().map_or(b.author, |d| d.cosig.device);
    if !list.contains_active(&cosigner) {
        return Err(bad("소유자의 활성 기기가 서명하지 않음"));
    }
    if !check_owner_proof(c) {
        return Err(bad("소유자 증명"));
    }
    Ok(())
}

/// 체크포인트 기기 트리 확인에 쓰는 값.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceTree {
    pub checkpoint: CheckpointId,
    pub root: [u8; 32],
    pub devices: u64,
}

impl DeviceTree {
    pub fn of(c: &SignedCheckpoint) -> Self {
        DeviceTree {
            checkpoint: c.id(),
            root: c.body.device_root,
            devices: c.body.devices,
        }
    }
}

/// `GetDeviceKeys` 답의 항목을 받아들일지. 키가 기기 ID와 맞고, 상태의 기기 기록(`known`)의 키 약속과 같거나, 아는
/// 체크포인트 기기 트리(`tree`)의 잎 증명이 맞고 그 잎의 키 약속과 같아야 합니다. 답한 쪽은 믿지 않습니다(남의 키를
/// 바꿔 끼우면 봉인 상자를 엿볼 수 있음).
pub fn accept_device_keys(
    item: &DeviceKeysItem,
    known: Option<&DeviceRecord>,
    tree: Option<&DeviceTree>,
) -> bool {
    let keys = &item.keys;
    if !keys.keys_match() {
        return false;
    }
    let commit = keys.key_commit();
    if let Some(r) = known
        && r.device == keys.device
    {
        return r.key_commit == commit;
    }
    match (&item.proof, tree) {
        (Some(p), Some(t)) => {
            p.checkpoint == t.checkpoint
                && p.record.device == keys.device
                && p.record.key_commit == commit
                && p.verify(&t.root, t.devices)
        }
        _ => false,
    }
}

/// 바탕 체크포인트의 기기 트리(답하는 쪽이 잎 증명을 만들 때). 층을 한 번 세어 두므로 증명 하나가 트리 높이만큼입니다.
/// 바탕이 바뀔 때만 다시 만듭니다([`crate::Resolver::device_proofs`]).
#[derive(Clone, Debug)]
pub struct DeviceProofs {
    pub tree: DeviceTree,
    /// (기기, 사용자) 순의 잎 주인과 기록.
    entries: Vec<(UserId, DeviceRecord)>,
    levels: Vec<Vec<[u8; 32]>>,
}

impl DeviceProofs {
    pub(crate) fn of(checkpoint: CheckpointId, m: &Machine) -> Self {
        let leaves = m.device_leaves();
        let hashes: Vec<[u8; 32]> = leaves.iter().map(|l| l.leaf).collect();
        let tree = DeviceTree {
            checkpoint,
            root: merkle::root(merkle::DEVICES, &hashes),
            devices: hashes.len() as u64,
        };
        DeviceProofs {
            tree,
            entries: leaves.iter().map(|l| (l.user, l.record.clone())).collect(),
            levels: merkle::levels(&hashes),
        }
    }

    /// 기기의 잎 증명(기기가 여러 사용자 기록에 있으면 첫 것; 상태가 그런 기록을 받지 않음).
    pub fn proof(&self, device: &DeviceId) -> Option<DeviceLeafProof> {
        let i = self.entries.partition_point(|(_, r)| r.device < *device);
        let (user, record) = self.entries.get(i).filter(|(_, r)| r.device == *device)?;
        Some(DeviceLeafProof {
            checkpoint: self.tree.checkpoint,
            user: *user,
            record: record.clone(),
            proof: merkle::range_proof_in(&self.levels, i, i + 1)?,
        })
    }
}
