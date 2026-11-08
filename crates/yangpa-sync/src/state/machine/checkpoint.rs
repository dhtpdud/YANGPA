//! 체크포인트 쪽 상태 처리: 위임·이견 이벤트, 체크포인트 때의 정리(늘기만 하는 집합), 정규 인코딩, 머클 잎, 받아들이기
//! 검사. 정규 인코딩은 해소기 상태 전체입니다(쓴 초대 증명, 떠난 사람의 기기 기록, 지운 채널·역할 ID, 키 기록 등): 하나라도
//! 빠지면 체크포인트로 들어온 멤버만 다르게 판정합니다.

use super::*;
use yangpa_core::checkpoint::{RESOLVER_VERSION, merkle};

/// 정규 인코딩(쓰기). 필드 순서는 [`CanonOwned`]와 같아야 합니다.
#[derive(Serialize)]
struct CanonRef<'a> {
    v: u32,
    space: &'a SpaceId,
    genesis: &'a EventId,
    seq: u64,
    st: &'a SpaceState,
    deleted_channels: &'a imbl::OrdSet<ChannelId>,
    deleted_roles: &'a imbl::OrdSet<RoleId>,
    departed: &'a SharedMap<UserId, Arc<MemberDevices>>,
    member_since: &'a SharedMap<UserId, u64>,
    crew_name: &'a Option<String>,
    used_proofs: &'a imbl::OrdSet<([u8; 32], u64, UserId)>,
    converted_by: &'a Option<(UserId, UserId)>,
    legacy_channels: &'a BTreeSet<ChannelId>,
    history_consent: &'a BTreeMap<ChannelId, UserId>,
    recovery_relays: &'a SharedMap<(UserId, DeviceId), BTreeSet<UserId>>,
    seen_keys: &'a imbl::OrdSet<StateKeyId>,
    retired_keys: &'a imbl::OrdMap<StateKeyId, u64>,
    revoked_since: &'a imbl::OrdSet<DeviceId>,
    revocation_root: &'a [u8; 32],
    owner_certs: &'a Vec<OwnershipCert>,
    delegation: &'a Option<DelegationCert>,
    retired_invites: &'a imbl::OrdSet<[u8; 32]>,
    keys: &'a CrewKeys,
    /// 해산 표시. 거짓이면 싣지 않아, 해산하지 않은 크루의 바이트·상태 해시가 해산을 모르는 빌드와 같습니다.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    disbanded: bool,
}

/// 정규 인코딩(읽기).
#[derive(Deserialize)]
struct CanonOwned {
    v: u32,
    space: SpaceId,
    genesis: EventId,
    seq: u64,
    st: SpaceState,
    deleted_channels: imbl::OrdSet<ChannelId>,
    deleted_roles: imbl::OrdSet<RoleId>,
    departed: SharedMap<UserId, Arc<MemberDevices>>,
    member_since: SharedMap<UserId, u64>,
    crew_name: Option<String>,
    used_proofs: imbl::OrdSet<([u8; 32], u64, UserId)>,
    converted_by: Option<(UserId, UserId)>,
    legacy_channels: BTreeSet<ChannelId>,
    history_consent: BTreeMap<ChannelId, UserId>,
    recovery_relays: SharedMap<(UserId, DeviceId), BTreeSet<UserId>>,
    seen_keys: imbl::OrdSet<StateKeyId>,
    retired_keys: imbl::OrdMap<StateKeyId, u64>,
    revoked_since: imbl::OrdSet<DeviceId>,
    revocation_root: [u8; 32],
    owner_certs: Vec<OwnershipCert>,
    delegation: Option<DelegationCert>,
    retired_invites: imbl::OrdSet<[u8; 32]>,
    keys: CrewKeys,
    #[serde(deserialize_with = "trailing_flag")]
    disbanded: bool,
}

/// 정규 인코딩 끝의 선택 표시: 바이트가 끝났으면(해산하지 않은 크루) 거짓입니다. 깨진 바이트도 거짓이 되지만 상태 해시
/// 검사가 걸러 냅니다.
fn trailing_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(bool::deserialize(d).unwrap_or(false))
}

impl Machine {
    /// 적용된 상태 이벤트가 쓴 상태 키를 적어 둡니다.
    pub(crate) fn note_state_key(&mut self, k: StateKeyId) {
        if !self.seen_keys.contains(&k) {
            self.seen_keys.insert(k);
        }
    }

    /// 체크포인트가 은퇴시킨 상태 키인지.
    pub(crate) fn is_retired(&self, k: &StateKeyId) -> bool {
        self.retired_keys.contains_key(k)
    }

    /// 바탕 뒤 상태 이벤트가 쓴 상태 키 수(바탕의 유효 키 포함).
    pub(crate) fn state_keys_in_use(&self) -> usize {
        self.seen_keys.len()
    }

    /// 상태에 있는 체크포인트 작성 위임.
    pub(crate) fn delegation(&self) -> Option<&DelegationCert> {
        self.delegation.as_ref()
    }

    /// 크루 키 사슬의 머리(체크포인트 본문의 크루 `KeyHead`). 갱신이 아직 없으면 `None`.
    pub(crate) fn crew_head(&self) -> Option<KeyHead> {
        (self.keys.seq > 0).then(|| KeyHead {
            group: KeyGroupId::crew(&self.space),
            seq: self.keys.seq,
            root_commit: self.keys.root_commit,
            chain: None,
        })
    }

    /// 본문의 키 머리 가운데 크루 것이 이 상태의 크루 사슬 끝과 맞는지. 비공개·덮개 그룹 머리는 그 그룹을 볼 수 있는
    /// 기기만 알므로 해소기는 보지 않습니다(쓴 기기가 아는 사슬 끝, 새로 자격을 얻은 기기의 시작점).
    fn crew_head_matches(&self, heads: &[KeyHead]) -> bool {
        let crew = KeyGroupId::crew(&self.space);
        heads.iter().find(|k| k.group == crew).copied() == self.crew_head()
    }

    /// 경계 판정을 거치지 않은 체크포인트의 거부 이유.
    pub(super) fn stale_checkpoint(&self, c: &SignedCheckpoint) -> AuthError {
        if c.body.resolver_version > RESOLVER_VERSION {
            return AuthError::NewerResolver;
        }
        let next = match &self.base {
            Some(b) => c.body.prev == Some(b.id) && c.body.seq == b.seq + 1,
            None => c.body.prev.is_none() && c.body.seq == 1,
        };
        if next {
            AuthError::InvalidCheckpoint("확인 실패")
        } else {
            AuthError::StaleCheckpoint
        }
    }

    /// 체크포인트 작성 위임. 소유자의 활성 기기가 쓰고, 그 기기가 인증서에 함께 서명해야 합니다.
    pub(super) fn checkpoint_delegation(
        &mut self,
        h: &EventHeader,
        cert: &DelegationCert,
        cert_ok: impl FnOnce() -> bool,
    ) -> R {
        if h.user != self.st.owner {
            return Err(AuthError::NotOwner);
        }
        if cert.space != self.space || cert.owner != self.st.owner || cert.cosig.device != h.author
        {
            return Err(AuthError::Invalid("체크포인트 위임"));
        }
        if !self.device_owner.contains_key(&cert.device) {
            return Err(AuthError::UnknownDevice);
        }
        if cert.until_seq <= self.checkpoint_seq {
            return Err(AuthError::Invalid("체크포인트 위임 기간"));
        }
        if !cert_ok() {
            return Err(AuthError::Invalid("체크포인트 위임 서명"));
        }
        self.delegation = Some(cert.clone());
        Ok(Effect::none())
    }

    /// 지금 바탕 체크포인트에 대한 이견(정보용). 기기마다 한 번만 받습니다.
    pub(super) fn checkpoint_dispute(
        &mut self,
        h: &EventHeader,
        checkpoint: &CheckpointId,
        seq: u64,
    ) -> R {
        let Some(b) = &self.base else {
            return Err(AuthError::Invalid("이견을 낼 체크포인트가 없음"));
        };
        if b.id != *checkpoint || b.seq != seq {
            return Err(AuthError::StaleCheckpoint);
        }
        if self.disputes.insert(h.author).is_some() {
            return Err(AuthError::DuplicateDispute);
        }
        Ok(Effect::none())
    }

    /// 체크포인트 때의 정리. 작성자와 검증자가 같은 함수로 같은 값을 얻습니다: 폐기 기록 옮기기, 소유권 인증서 비우기,
    /// 쓰지 않을 상태 키 은퇴, 지금 초대 말고의 쓴 초대 증명 은퇴, 멤버 아닌 복구 전달 버리기, 크루 키 사슬의 조상 참조
    /// 정리([`CrewKeys::seal`]).
    pub(crate) fn seal(&mut self, seq: u64, valid: &[StateKeyId]) -> Sealed {
        let revoked: Vec<DeviceId> = self
            .revoked_since
            .iter()
            .take(MAX_CHECKPOINT_REVOKED)
            .copied()
            .collect();
        for d in &revoked {
            self.revoked_since.remove(d);
        }
        self.revocation_root = ckpt::revocation_root(&self.revocation_root, seq, &revoked);
        let owner_chain = std::mem::take(&mut self.owner_certs);
        self.disputes = imbl::OrdSet::new();

        let valid_set: imbl::OrdSet<StateKeyId> = valid.iter().copied().collect();
        for k in self.seen_keys.iter() {
            if !valid_set.contains(k) && !self.retired_keys.contains_key(k) {
                self.retired_keys.insert(*k, seq);
            }
        }
        self.seen_keys = valid_set;
        while self.retired_keys.len() > MAX_RETIRED_KEYS {
            let oldest = self
                .retired_keys
                .iter()
                .min_by_key(|(k, s)| (**s, **k))
                .map(|(k, _)| *k);
            match oldest {
                Some(k) => {
                    self.retired_keys.remove(&k);
                }
                None => break,
            }
        }

        let live: BTreeSet<[u8; 32]> = self.st.invites.iter().map(|i| i.info.token_hash).collect();
        let stale: Vec<([u8; 32], u64, UserId)> = self
            .used_proofs
            .iter()
            .filter(|(t, _, _)| !live.contains(t))
            .copied()
            .collect();
        for p in stale {
            self.used_proofs.remove(&p);
            self.retired_invites.insert(p.0);
        }

        let gone: Vec<(UserId, DeviceId)> = self
            .recovery_relays
            .keys()
            .filter(|(u, _)| !self.st.members.contains_key(u))
            .copied()
            .collect();
        for k in gone {
            self.recovery_relays.remove(&k);
        }

        self.keys.seal(self.genesis);
        self.checkpoint_seq = seq;
        Sealed {
            revoked,
            revocation_root: self.revocation_root,
            owner_chain,
            crew_head: self.crew_head(),
            delegation_hash: self.delegation_hash(seq),
        }
    }

    /// 순번 `seq` 체크포인트 다음 것을 쓸 수 있는 위임의 해시: 상태에 있는 위임 가운데 위임받은 기기와 위임한 소유자
    /// 기기가 둘 다 활성이고 다음 순번까지 유효한 것. 작성자와 검증자(스냅숏으로 들어온 멤버 포함)가 같은 값을 얻습니다.
    pub(crate) fn delegation_hash(&self, seq: u64) -> Option<[u8; 32]> {
        let d = self.delegation.as_ref()?;
        let owner = self.st.devices.get(&self.st.owner)?;
        (d.until_seq > seq
            && self.device_owner.contains_key(&d.device)
            && owner.contains_active(&d.cosig.device))
        .then(|| d.hash())
    }

    /// 정규 인코딩(스냅숏 평문). 지속 자료 구조와 `BTreeMap`은 키 순으로 직렬화되므로 같은 상태면 같은 바이트입니다.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let c = CanonRef {
            v: CANON_VERSION,
            space: &self.space,
            genesis: &self.genesis,
            seq: self.checkpoint_seq,
            st: &self.st,
            deleted_channels: &self.deleted_channels,
            deleted_roles: &self.deleted_roles,
            departed: &self.departed,
            member_since: &self.member_since,
            crew_name: &self.crew_name,
            used_proofs: &self.used_proofs,
            converted_by: &self.converted_by,
            legacy_channels: &self.legacy_channels,
            history_consent: &self.history_consent,
            recovery_relays: &self.recovery_relays,
            seen_keys: &self.seen_keys,
            retired_keys: &self.retired_keys,
            revoked_since: &self.revoked_since,
            revocation_root: &self.revocation_root,
            owner_certs: &self.owner_certs,
            delegation: &self.delegation,
            retired_invites: &self.retired_invites,
            keys: &self.keys,
            disbanded: self.st.disbanded,
        };
        postcard::to_stdvec(&c).expect("상태 직렬화는 실패하지 않음")
    }

    /// 정규 인코딩에서 되살립니다(기기 → 사용자 표는 기기 기록에서 다시 만듦). 바탕 정보는 부르는 쪽이 넣습니다.
    pub(crate) fn decode(bytes: &[u8], limits: Limits) -> Result<Machine, &'static str> {
        let c: CanonOwned = postcard::from_bytes(bytes).map_err(|_| "스냅숏 인코딩")?;
        if c.v != CANON_VERSION {
            return Err("스냅숏 판");
        }
        let mut device_owner = SharedMap::new();
        for (u, rec) in c.st.devices.iter() {
            if !c.st.members.contains_key(u) {
                return Err("멤버 아닌 기기 기록");
            }
            for d in rec.active() {
                if device_owner.insert(d.device, *u).is_some() {
                    return Err("두 사용자에 속한 기기");
                }
            }
        }
        Ok(Machine {
            space: c.space,
            genesis: c.genesis,
            st: SpaceState {
                disbanded: c.disbanded,
                ..c.st
            },
            deleted_channels: c.deleted_channels,
            deleted_roles: c.deleted_roles,
            device_owner,
            departed: c.departed,
            member_since: c.member_since,
            crew_name: c.crew_name,
            used_proofs: c.used_proofs,
            converted_by: c.converted_by,
            legacy_channels: c.legacy_channels,
            history_consent: c.history_consent,
            rank_change: RankChange::None,
            limits,
            recovery_relays: c.recovery_relays,
            checkpoint_seq: c.seq,
            seen_keys: c.seen_keys,
            retired_keys: c.retired_keys,
            revoked_since: c.revoked_since,
            revocation_root: c.revocation_root,
            owner_certs: c.owner_certs,
            delegation: c.delegation,
            disputes: imbl::OrdSet::new(),
            retired_invites: c.retired_invites,
            keys: c.keys,
            cur: c.genesis,
            base: None,
        })
    }

    /// 사용자 트리 잎(사용자 순)과 소유자 잎의 위치.
    pub(crate) fn user_leaves(&self) -> (Vec<[u8; 32]>, Option<usize>) {
        let mut owner = None;
        let leaves = self
            .st
            .members
            .keys()
            .enumerate()
            .map(|(i, u)| {
                if *u == self.st.owner {
                    owner = Some(i);
                }
                merkle::leaf(
                    merkle::USERS,
                    u.as_bytes(),
                    &ckpt::user_value(self.st.devices.get(u).map(|d| &**d)),
                )
            })
            .collect();
        (leaves, owner)
    }

    /// 기기 트리 잎((기기, 사용자) 순). 폐기된 기록도 넣습니다(옛 이벤트 검증에 필요).
    pub(crate) fn device_leaves(&self) -> Vec<DeviceLeaf<'_>> {
        let mut out = Vec::new();
        for (u, rec) in self.st.devices.iter() {
            for d in &rec.devices {
                let key = ckpt::device_key(&d.device, u);
                let leaf = merkle::leaf(merkle::DEVICES, &key, &ckpt::device_value(d));
                out.push(DeviceLeaf {
                    key,
                    leaf,
                    user: *u,
                    record: d,
                });
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// 상태 해시·트리 뿌리 계산 결과.
    pub(crate) fn digest(&self) -> Digest {
        let bytes = self.encode();
        let (users, owner) = self.user_leaves();
        let devices = self.device_leaves();
        let device_hashes: Vec<[u8; 32]> = devices.iter().map(|d| d.leaf).collect();
        Digest {
            state_hash: ckpt::state_hash(&bytes),
            user_root: merkle::root(merkle::USERS, &users),
            users: users.len() as u64,
            device_root: merkle::root(merkle::DEVICES, &device_hashes),
            devices: device_hashes.len() as u64,
            owner_proof: owner.and_then(|i| merkle::range_proof(&users, i, i + 1)),
            snapshot: bytes,
        }
    }

    /// 작성자가 맞는지: 소유자의 활성 기기(소유자 신원 서명), 또는 상태에 적용된 위임을 받은 활성 기기.
    fn check_checkpoint_author(
        &self,
        h: &EventHeader,
        c: &SignedCheckpoint,
    ) -> Result<(), AuthError> {
        let owner_devices = self
            .st
            .devices
            .get(&self.st.owner)
            .ok_or(AuthError::InvalidCheckpoint("소유자 기기 목록 없음"))?;
        match &c.body.delegation {
            None => {
                if h.user != self.st.owner {
                    return Err(AuthError::NotOwner);
                }
                if !owner_devices.contains_active(&h.author) {
                    return Err(AuthError::UnknownDevice);
                }
            }
            Some(d) => {
                if self.delegation.as_ref() != Some(d) {
                    return Err(AuthError::InvalidCheckpoint("상태에 없는 위임"));
                }
                // 바탕이 밝힌 위임이어야 합니다: 체크포인트 사슬만 보는 새 멤버가 같은 규칙으로 확인할 수 있게.
                if let Some(b) = &self.base
                    && b.delegation_hash != Some(d.hash())
                {
                    return Err(AuthError::InvalidCheckpoint("바탕이 밝히지 않은 위임"));
                }
                if d.device != h.author || !self.device_owner.contains_key(&d.device) {
                    return Err(AuthError::UnknownDevice);
                }
                // 위임한 소유자 기기가 폐기되면 위임도 끝납니다.
                if !owner_devices.contains_active(&d.cosig.device) {
                    return Err(AuthError::InvalidCheckpoint("위임한 기기가 활성이 아님"));
                }
            }
        }
        Ok(())
    }

    /// 체크포인트 조상까지 해소한 상태(`self`)에서 체크포인트 본문의 상태 쪽을 확인하고, 맞으면 그 체크포인트를 바탕으로
    /// 삼은 상태를 돌려줍니다. 경계·부모·서명 모양은 부르는 쪽(해소기)이 먼저 봅니다.
    pub(crate) fn accept_checkpoint(
        &self,
        ev: &Stored,
        c: &SignedCheckpoint,
        key: Option<StateKeyId>,
    ) -> Result<Machine, AuthError> {
        let b = &c.body;
        self.check_checkpoint_author(&ev.header, c)?;
        if b.owner != self.st.owner {
            return Err(AuthError::InvalidCheckpoint("소유자"));
        }
        let owner_rec = self
            .st
            .devices
            .get(&b.owner)
            .ok_or(AuthError::InvalidCheckpoint("소유자 기기 목록 없음"))?;
        if MemberDevices::from_list(&b.owner_devices.list) != **owner_rec {
            return Err(AuthError::InvalidCheckpoint("소유자 기기 목록"));
        }
        let mut m = self.clone();
        let sealed = m.seal(b.seq, &b.valid_state_keys);
        if sealed.revoked != b.revoked || sealed.revocation_root != b.revocation_root {
            return Err(AuthError::InvalidCheckpoint("폐기 기록"));
        }
        if sealed.owner_chain != b.owner_chain {
            return Err(AuthError::InvalidCheckpoint("소유권 인증서 사슬"));
        }
        if !m.crew_head_matches(&b.key_heads) {
            return Err(AuthError::InvalidCheckpoint("크루 키 머리"));
        }
        if sealed.delegation_hash != b.delegation_hash {
            return Err(AuthError::InvalidCheckpoint("위임 해시"));
        }
        // 서명 기기 증명은 새 멤버만 쓰지만, 실린 것은 바탕 기기 트리에 맞아야 합니다(정직한 작성자가 틀린 증명을 내지
        // 않게; 없는 증명은 새 멤버가 처음부터 따라잡게 될 뿐).
        if !b.signer_proofs.is_empty() {
            let base = self
                .base
                .as_ref()
                .ok_or(AuthError::InvalidCheckpoint("바탕 없는 서명 기기 증명"))?;
            if b.signer_proofs
                .iter()
                .any(|p| p.checkpoint != base.id || !p.verify(&base.device_root, base.devices))
            {
                return Err(AuthError::InvalidCheckpoint("서명 기기 증명"));
            }
        }
        let d = m.digest();
        if d.state_hash != b.state_hash {
            return Err(AuthError::InvalidCheckpoint("상태 해시"));
        }
        if d.user_root != b.user_root
            || d.users != b.users
            || d.device_root != b.device_root
            || d.devices != b.devices
        {
            return Err(AuthError::InvalidCheckpoint("기기 트리"));
        }
        if !check_owner_proof(c) {
            return Err(AuthError::InvalidCheckpoint("소유자 증명"));
        }
        m.base = Some(Arc::new(CheckpointBase::of(
            ev.id,
            ev.header.lamport,
            key,
            c,
        )));
        m.rank_change = RankChange::None;
        Ok(m)
    }

    /// 스냅숏에서 되살린 상태가 체크포인트 본문과 맞는지(새 멤버). 맞으면 바탕 정보를 넣습니다.
    pub(crate) fn check_snapshot(
        &mut self,
        ev_id: EventId,
        lamport: u64,
        key: Option<StateKeyId>,
        c: &SignedCheckpoint,
    ) -> Result<(), &'static str> {
        let b = &c.body;
        if self.space != b.space {
            return Err("공간");
        }
        if self.checkpoint_seq != b.seq {
            return Err("체크포인트 순번");
        }
        if self.st.owner != b.owner {
            return Err("소유자");
        }
        let owner_rec = self
            .st
            .devices
            .get(&b.owner)
            .ok_or("소유자 기기 목록 없음")?;
        if MemberDevices::from_list(&b.owner_devices.list) != **owner_rec {
            return Err("소유자 기기 목록");
        }
        if self.revocation_root != b.revocation_root {
            return Err("폐기 기록");
        }
        if !self.crew_head_matches(&b.key_heads) {
            return Err("크루 키 머리");
        }
        if self.delegation_hash(b.seq) != b.delegation_hash {
            return Err("위임 해시");
        }
        let d = self.digest();
        if d.state_hash != b.state_hash {
            return Err("상태 해시");
        }
        if d.user_root != b.user_root
            || d.users != b.users
            || d.device_root != b.device_root
            || d.devices != b.devices
        {
            return Err("기기 트리");
        }
        if !check_owner_proof(c) {
            return Err("소유자 증명");
        }
        self.base = Some(Arc::new(CheckpointBase::of(ev_id, lamport, key, c)));
        Ok(())
    }
}

/// 기기 트리의 잎 하나.
pub(crate) struct DeviceLeaf<'a> {
    pub key: Vec<u8>,
    pub leaf: [u8; 32],
    pub user: UserId,
    pub record: &'a DeviceRecord,
}

/// 상태 해시·트리 뿌리.
pub(crate) struct Digest {
    pub state_hash: [u8; 32],
    pub user_root: [u8; 32],
    pub users: u64,
    pub device_root: [u8; 32],
    pub devices: u64,
    pub owner_proof: Option<ckpt::MerkleProof>,
    /// 정규 인코딩(스냅숏 평문).
    pub snapshot: Vec<u8>,
}

/// 본문의 소유자 증명이 사용자 트리 뿌리에 맞는지.
pub(crate) fn check_owner_proof(c: &SignedCheckpoint) -> bool {
    let b = &c.body;
    let rec = MemberDevices::from_list(&b.owner_devices.list);
    let leaf = merkle::leaf(
        merkle::USERS,
        b.owner.as_bytes(),
        &ckpt::user_value(Some(&rec)),
    );
    merkle::verify_range(&b.user_root, b.users, &[leaf], &b.owner_proof)
}
