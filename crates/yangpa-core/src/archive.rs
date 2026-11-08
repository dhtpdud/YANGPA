//! 보관 전용 노드(protocol.md "보관 전용 노드"): 지정 정보, 서명된 멤버 명단, 노드 간 요청.
//!
//! 보관 노드는 크루의 멤버가 아니고 어떤 키도 받지 않습니다. 그래서 상태 이벤트(상태 키로 암호화)를 읽지 못하고,
//! 요청자를 확인하는 데 필요한 멤버 명단만 권한자 기기가 서명해 따로 보냅니다.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::{BlobId, ChannelId, DeviceId, EventId, LinkToken, SpaceId, UserId};
use crate::links::ContactInfo;
use crate::sign::{Sig, SignDomain, SigningKeypair, verify};
use crate::time::Timestamp;
use crate::wire::PeerAuth;

/// 크루마다 지정할 수 있는 보관 노드 수.
pub const MAX_ARCHIVES: usize = 8;
/// 지정 하나에 넣을 수 있는 비공개 채널 수.
pub const MAX_PRIVATE_CHANNELS: usize = 64;
/// 보관 노드 표시 이름 길이(글자).
pub const MAX_NAME_CHARS: usize = 64;
/// 명단 상한(보관 노드가 받는 크기를 묶음).
pub const MAX_ROSTER_MEMBERS: usize = 65_536;
pub const MAX_ROSTER_DEVICES: usize = 64;
pub const MAX_ROSTER_CHANNELS: usize = 1_000;
/// 명단의 `anchor`는 보관 노드가 가진 최대 논리 시계보다 이만큼까지만 앞설 수 있습니다(부풀린 명단이 이후 명단을
/// 모두 막지 못하게).
pub const MAX_ANCHOR_AHEAD: u64 = 1 << 20;
/// 첨부 알림 한 번에 실을 메시지 수와 메시지마다 블롭 수.
pub const MAX_REF_EVENTS: usize = 256;
pub const MAX_REF_BLOBS: usize = 16;
/// 삭제 반영 한 번에 실을 이벤트 수.
pub const MAX_PRUNE_EVENTS: usize = 512;
pub const ROSTER_VERSION: u8 = 1;

/// `ArchiveGrant` 상태 이벤트의 본문. 보관 노드의 기기·사용자와 접속 정보, 함께 보관할 비공개 채널.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveGrant {
    pub device: DeviceId,
    pub user: UserId,
    #[serde(default)]
    pub name: String,
    pub contact: ContactInfo,
    /// 기본으로는 보관하지 않는 비공개 채널(@everyone 보기 거부) 중 함께 보관할 것.
    #[serde(default)]
    pub private_channels: Vec<ChannelId>,
}

/// 상태에 남는 지정.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub grant: ArchiveGrant,
    pub granted_by: UserId,
    pub granted_at: Timestamp,
}

/// 명단의 멤버 한 명.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterMember {
    pub user: UserId,
    /// 활성 기기(폐기된 기기는 빠짐).
    pub devices: Vec<DeviceId>,
    /// 가장 최근에 멤버가 된 논리 시계. 처음 멤버면 `None`(입장 이전 기록 검사용).
    pub since: Option<u64>,
}

/// 명단의 보관 채널 하나.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterChannel {
    pub channel: ChannelId,
    /// 과거 기록 공개(나중에 들어온 멤버도 입장 이전 기록을 받음).
    pub history_public: bool,
    /// 볼 수 있는 사용자. 멤버 모두면 `None`.
    pub readers: Option<Vec<UserId>>,
    /// 채팅 수명(받은 뒤 N초에 본문 삭제). 없으면 `None`.
    pub ttl_secs: Option<u32>,
}

/// 보관 노드가 요청자를 확인하는 데 쓰는 명단. 권한자 기기가 서명합니다([`SignedArchiveRoster`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRoster {
    pub v: u8,
    pub space: SpaceId,
    pub genesis: EventId,
    /// 이 명단을 받을 보관 기기(다른 보관 노드에 다시 쓰지 못하게).
    pub archive: DeviceId,
    /// 서명 기기가 가진 그 공간 **공간 스트림** 이벤트의 가장 큰 논리 시계(명단의 순서). 명단 내용은 모두 공간
    /// 스트림(상태)에서 나오고 공간 스트림은 멤버 모두가 같이 받으므로, 비공개 채널을 볼 수 있는지와 무관하게 견줄 수 있습니다.
    pub anchor: u64,
    pub issued_at: Timestamp,
    pub owner: UserId,
    /// 소유자와 `MANAGE_SPACE`가 있는 사용자(명단을 낼 수 있음).
    pub authorities: Vec<UserId>,
    /// `MANAGE_MESSAGES`가 있는 사용자(남의 메시지 삭제를 반영할 수 있음).
    pub moderators: Vec<UserId>,
    /// `KICK_MEMBERS`·`BAN_MEMBERS`가 있는 사용자. 권한자가 아니어도 권한자가 아닌 멤버를 빼는 명단을 낼 수 있습니다.
    pub wardens: Vec<UserId>,
    pub members: Vec<RosterMember>,
    pub channels: Vec<RosterChannel>,
}

impl ArchiveRoster {
    pub fn member(&self, user: &UserId) -> Option<&RosterMember> {
        self.members.iter().find(|m| m.user == *user)
    }

    /// 사용자가 멤버이고 그 기기가 활성인지.
    pub fn has_device(&self, user: &UserId, device: &DeviceId) -> bool {
        self.member(user)
            .is_some_and(|m| m.devices.contains(device))
    }

    /// 기기의 사용자(멤버 기기일 때).
    pub fn user_of(&self, device: &DeviceId) -> Option<UserId> {
        self.members
            .iter()
            .find(|m| m.devices.contains(device))
            .map(|m| m.user)
    }

    pub fn is_authority(&self, user: &UserId) -> bool {
        self.authorities.contains(user)
    }

    pub fn is_moderator(&self, user: &UserId) -> bool {
        self.moderators.contains(user) || self.is_authority(user)
    }

    pub fn is_warden(&self, user: &UserId) -> bool {
        self.wardens.contains(user)
    }

    pub fn channel(&self, channel: &ChannelId) -> Option<&RosterChannel> {
        self.channels.iter().find(|c| c.channel == *channel)
    }

    /// 권한자(지금 명단 기준)의 활성 기기인지.
    pub fn is_authority_device(&self, device: &DeviceId) -> bool {
        self.user_of(device).is_some_and(|u| self.is_authority(&u))
    }

    /// 크기 상한 검사.
    pub fn check_limits(&self) -> Result<(), CoreError> {
        if self.v != ROSTER_VERSION {
            return Err(CoreError::UnsupportedVersion(self.v));
        }
        if self.members.len() > MAX_ROSTER_MEMBERS
            || self.authorities.len() > MAX_ROSTER_MEMBERS
            || self.moderators.len() > MAX_ROSTER_MEMBERS
            || self.wardens.len() > MAX_ROSTER_MEMBERS
        {
            return Err(CoreError::LimitExceeded("명단 멤버 수"));
        }
        if self
            .members
            .iter()
            .any(|m| m.devices.len() > MAX_ROSTER_DEVICES)
        {
            return Err(CoreError::LimitExceeded("명단 기기 수"));
        }
        if self.channels.len() > MAX_ROSTER_CHANNELS {
            return Err(CoreError::LimitExceeded("명단 채널 수"));
        }
        if self.channels.iter().any(|c| {
            c.readers
                .as_ref()
                .is_some_and(|r| r.len() > MAX_ROSTER_MEMBERS)
        }) {
            return Err(CoreError::LimitExceeded("채널 읽는 사람 수"));
        }
        Ok(())
    }

    pub fn sign(self, device_key: &SigningKeypair) -> Result<SignedArchiveRoster, CoreError> {
        let bytes = postcard::to_stdvec(&self)?;
        let sig = device_key.sign(SignDomain::ArchiveRoster, &bytes);
        Ok(SignedArchiveRoster {
            roster: self,
            signer: DeviceId(device_key.public()),
            sig,
        })
    }
}

/// 서명 기기의 기기 키로 서명한 명단.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedArchiveRoster {
    pub roster: ArchiveRoster,
    pub signer: DeviceId,
    pub sig: Sig,
}

impl SignedArchiveRoster {
    /// 서명과 크기만 확인합니다. 서명 기기를 믿을지는 받는 쪽(`yangpa_sync::archive::accept_roster`)이 정합니다.
    pub fn verify(&self) -> Result<(), CoreError> {
        self.roster.check_limits()?;
        let bytes = postcard::to_stdvec(&self.roster)?;
        verify(
            self.signer.as_bytes(),
            SignDomain::ArchiveRoster,
            &bytes,
            &self.sig,
        )
    }
}

/// 명단 차이 하나에 실을 더하기·빼기 수.
pub const MAX_DELTA_CHANGES: usize = 512;
/// 보관 노드가 바탕 명단마다 모아 두는 차이 수(넘으면 바탕 명단을 다시 받을 때까지 새 차이를 받지 않음). 권한자 차이와
/// 그 밖의 멤버 차이를 따로 셉니다(멤버가 권한자 몫을 써 버리지 못하게).
pub const MAX_DELTAS: usize = 4096;
/// 권한자가 아닌 멤버 기기 하나가 바탕 명단마다 보낼 수 있는 차이 수(한 멤버가 [`MAX_DELTAS`]를 다 쓰지 못하게).
pub const MAX_MEMBER_DELTAS: u64 = 16;

/// 명단 해시(차이가 가리키는 바탕).
pub fn roster_hash(r: &ArchiveRoster) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/archive-roster");
    h.update(&postcard::to_stdvec(r).expect("직렬화 실패 없음"));
    *h.finalize().as_bytes()
}

/// 바탕 명단(체크포인트 때 권한자가 서명한 통째 명단) 위의 차이. 바뀔 때마다 명단 전체(3만 명이면 2.7MB)를 다시 보내지
/// 않으려고 변경당 약 100B만 보냅니다. 보관 노드는 차이를 (서명 기기, 순번) 집합으로 모아, 같은 기기의 차이는 앞 차이
/// 해시(`prev_hash`)로 잇습니다. 여러 기기가 동시에 써도 사슬이 갈라지지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRosterDelta {
    pub v: u8,
    pub space: SpaceId,
    pub archive: DeviceId,
    /// 바탕 명단의 [`roster_hash`].
    pub base: [u8; 32],
    /// 서명 기기가 이 바탕 위에 낸 차이 순번(1부터).
    pub seq: u64,
    /// 같은 서명 기기의 앞 차이 해시(첫 차이면 바탕 해시).
    pub prev_hash: [u8; 32],
    /// 서명 기기가 본 공간 스트림 최대 논리 시계.
    pub anchor: u64,
    pub issued_at: Timestamp,
    pub add: Vec<RosterMember>,
    pub remove: Vec<UserId>,
}

impl ArchiveRosterDelta {
    pub fn check_limits(&self) -> Result<(), CoreError> {
        if self.v != ROSTER_VERSION {
            return Err(CoreError::UnsupportedVersion(self.v));
        }
        if self.seq == 0 || self.add.len() + self.remove.len() > MAX_DELTA_CHANGES {
            return Err(CoreError::LimitExceeded("명단 차이 크기"));
        }
        if self
            .add
            .iter()
            .any(|m| m.devices.len() > MAX_ROSTER_DEVICES)
        {
            return Err(CoreError::LimitExceeded("명단 기기 수"));
        }
        Ok(())
    }

    /// 이 차이의 해시(다음 차이의 `prev_hash`).
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/archive-roster-delta");
        h.update(&postcard::to_stdvec(self).expect("직렬화 실패 없음"));
        *h.finalize().as_bytes()
    }

    pub fn sign(self, device_key: &SigningKeypair) -> Result<SignedArchiveRosterDelta, CoreError> {
        let bytes = postcard::to_stdvec(&self)?;
        let sig = device_key.sign(SignDomain::ArchiveRosterDelta, &bytes);
        Ok(SignedArchiveRosterDelta {
            delta: self,
            signer: DeviceId(device_key.public()),
            sig,
        })
    }
}

/// 서명 기기의 기기 키로 서명한 명단 차이.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedArchiveRosterDelta {
    pub delta: ArchiveRosterDelta,
    pub signer: DeviceId,
    pub sig: Sig,
}

impl SignedArchiveRosterDelta {
    pub fn verify(&self) -> Result<(), CoreError> {
        self.delta.check_limits()?;
        let bytes = postcard::to_stdvec(&self.delta)?;
        verify(
            self.signer.as_bytes(),
            SignDomain::ArchiveRosterDelta,
            &bytes,
            &self.sig,
        )
    }
}

/// 지정 요청(관리자 → 보관 노드).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveDesignate {
    /// 제안 링크의 1회용 토큰.
    pub token: LinkToken,
    /// 이 연결에 묶인 관리자 기기 증명(인증 전에도 받는 요청).
    pub auth: PeerAuth,
    /// 지정이 적용된 것으로 계산한 첫 명단(서명 기기 = `auth.device`).
    pub roster: SignedArchiveRoster,
}

/// 첨부 알림 하나: 보관 범위 채널의 메시지와 거기 실린 블롭.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveBlobRef {
    pub event: EventId,
    pub blobs: Vec<BlobId>,
}

/// 보관 노드와 주고받는 요청(`YpRequest::Archive`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveRequest {
    /// 지정 수락 요청. 인증 전에도 받습니다.
    Designate(Box<ArchiveDesignate>),
    /// 새 명단. 권한자 기기는 통째로, 그 밖의 멤버 기기는 멤버를 빼는 데만 씁니다(`yangpa_sync::archive::accept_roster`).
    /// 받으면 `Ok`, 오래됐거나 일부만 반영했으면 `Expired`(따라잡은 뒤 다시 보냄), 거절이면 `Forbidden`.
    Roster(Box<SignedArchiveRoster>),
    /// 첨부 알림. 받은 메시지는 `YpResponse::Ids`로 돌려줍니다.
    BlobRefs {
        space: SpaceId,
        refs: Vec<ArchiveBlobRef>,
    },
    /// 지워진 메시지(본문을 지우고 삭제 표시를 남김).
    Prune {
        space: SpaceId,
        events: Vec<EventId>,
    },
    /// 지정 해제: 그 공간의 모든 기록을 지움. 인증 전에도 받습니다(이 연결에 묶인 권한자 기기 증명).
    Release { space: SpaceId, auth: Box<PeerAuth> },
    /// 바탕 명단 위의 차이(`yangpa_sync::archive::accept_delta`). 받으면 `Ok`, 바탕이 다르거나 앞 차이가 없으면 `Expired`
    /// (보낸 쪽이 바탕 명단이나 빠진 차이를 다시 보냄), 거절이면 `Forbidden`.
    RosterDelta(Box<SignedArchiveRosterDelta>),
}

impl ArchiveRequest {
    /// 대상 공간.
    pub fn space(&self) -> SpaceId {
        match self {
            ArchiveRequest::Designate(d) => d.roster.roster.space,
            ArchiveRequest::Roster(r) => r.roster.space,
            ArchiveRequest::RosterDelta(d) => d.delta.space,
            ArchiveRequest::BlobRefs { space, .. }
            | ArchiveRequest::Prune { space, .. }
            | ArchiveRequest::Release { space, .. } => *space,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(signer_user: UserId, signer: DeviceId) -> ArchiveRoster {
        ArchiveRoster {
            v: ROSTER_VERSION,
            space: SpaceId([1; 16]),
            genesis: EventId([2; 32]),
            archive: DeviceId([3; 32]),
            anchor: 10,
            issued_at: Timestamp(1),
            owner: signer_user,
            authorities: vec![signer_user],
            moderators: vec![],
            wardens: vec![],
            members: vec![RosterMember {
                user: signer_user,
                devices: vec![signer],
                since: None,
            }],
            channels: vec![],
        }
    }

    #[test]
    fn signed_roster_round_trip_and_tamper() {
        let key = SigningKeypair::generate();
        let user = UserId([9; 32]);
        let signed = roster(user, DeviceId(key.public())).sign(&key).unwrap();
        signed.verify().unwrap();
        assert!(signed.roster.has_device(&user, &DeviceId(key.public())));
        assert!(signed.roster.is_authority_device(&DeviceId(key.public())));

        let bytes = postcard::to_stdvec(&signed).unwrap();
        let back: SignedArchiveRoster = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, signed);

        let mut forged = signed.clone();
        forged.roster.anchor += 1;
        assert!(forged.verify().is_err());
    }

    #[test]
    fn offer_link_round_trip() {
        use crate::links::ArchiveOfferLink;
        let contact = ContactInfo {
            peer_id: vec![1, 2, 3],
            relay_addrs: vec!["/dns4/relay.example/tcp/4001/p2p/12D3KooW/p2p-circuit".into()],
            onion: None,
        };
        let l = ArchiveOfferLink::new(UserId([1; 32]), DeviceId([2; 32]), contact, None);
        let url = l.to_url().unwrap();
        assert!(url.starts_with(&format!("{}#archive/", crate::links::WEB_BASE)));
        assert_eq!(ArchiveOfferLink::parse(&url).unwrap(), l);
        // 다른 종류의 링크로는 읽히지 않음.
        assert!(crate::links::FriendLink::parse(&url).is_err());
        let mut expired = l.clone();
        expired.expires_at = Some(Timestamp(1));
        let url = expired.to_url().unwrap();
        assert!(matches!(
            ArchiveOfferLink::parse(&url),
            Err(CoreError::Expired)
        ));
    }

    #[test]
    fn roster_limits() {
        let key = SigningKeypair::generate();
        let mut r = roster(UserId([9; 32]), DeviceId(key.public()));
        r.members[0].devices = (0..=MAX_ROSTER_DEVICES as u8)
            .map(|i| DeviceId([i; 32]))
            .collect();
        assert!(r.sign(&key).unwrap().verify().is_err());
    }
}
