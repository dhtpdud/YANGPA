//! 해소기가 들고 있는 이벤트([`Stored`])와, 기기 목록이 실린 본문에서 뽑은 사실.
//!
//! 해소기는 다시 해소할 때를 위해 받은 상태 이벤트를 모두 들고 있습니다. 가입(`MemberJoin`) 본문 하나는 서명된 기기
//! 목록(상자 키 1,216B·FN-DSA 공개키 897B·혼합 서명 둘), 프로필, 초대 증명까지 수 KB라 3만 명이면 수백 MB입니다. 그래서 가입·기기 목록 갱신은 넣을 때 한 번만 검사해, 상태와 무관한 검사 결과(모양·서명·프로필)와
//! 간추린 기록만 남기고 본문은 버립니다. 남긴 것만으로 판정이 본문을 들고 있을 때와 같습니다:
//! 상태에 따라 갈리는 검사(멤버·판·이어받기·기기 주인·초대 키)는 간추린 기록과 서명자 키 ID로 다시 할 수 있습니다.
//!
//! 키 갱신(`KeyUpdate`) 조각도 같습니다: 본문은 960KiB까지라 감싸기·봉인 상자는 버리고, 해소기가 판정에 쓰는 머리와
//! 기기 목록(가입·제거·칸 채우기·봉인 수신자)만 남깁니다([`KeyFacts`]).

use std::sync::Arc;

use yangpa_core::body::{EventBody, InviteProof};
use yangpa_core::device::{MemberDevices, SignedDeviceList, SignedProfile};
use yangpa_core::event::{EventHeader, EventKind};
use yangpa_core::ids::{DeviceId, EventId, KeyGroupId, SpaceId, UserId};
use yangpa_core::keyupdate::{CrewRef, KeyUpdateKind, KeyUpdatePayload, LayoutOp};
use yangpa_core::model::Member;
use yangpa_core::time::Timestamp;

use super::DecodedEvent;
use super::machine::check_device_list_shape;

/// 해소기가 들고 있는 이벤트. 필드 이름은 [`DecodedEvent`]와 같습니다.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub id: EventId,
    pub header: EventHeader,
    pub body: StoredBody,
}

impl Stored {
    /// 받은 이벤트를 해소기에 둘 모양으로. 가입·기기 목록 갱신은 여기서 서명을 확인합니다(예전에는 처음 필요할 때 했지만,
    /// 본문을 버리려면 미룰 수 없음. 받기 전에 작성 기기의 서명·상태 키·속도 제한을 이미 통과한 이벤트들입니다).
    pub fn new(e: DecodedEvent) -> Self {
        let DecodedEvent { id, header, body } = e;
        let body = match body {
            EventBody::MemberJoin {
                member,
                devices,
                profile,
                invite,
            } => StoredBody::Join(Box::new(JoinFacts::of(
                &header.space,
                member,
                &devices,
                profile.as_ref(),
                invite.as_ref(),
            ))),
            EventBody::DeviceListUpdate { devices } => {
                StoredBody::Devices(Box::new(DevicesFacts::of(&devices)))
            }
            EventBody::KeyUpdate(p) => StoredBody::Key(Box::new(KeyFacts::of(&p))),
            b => StoredBody::Full(Box::new(b)),
        };
        Stored { id, header, body }
    }
}

/// 해소기가 들고 있는 본문. 본문 enum은 600B 남짓이라 상자에 둡니다(간추린 가입 기록마다 그만큼 비지 않게).
#[derive(Clone, Debug)]
pub(crate) enum StoredBody {
    Full(Box<EventBody>),
    Join(Box<JoinFacts>),
    Devices(Box<DevicesFacts>),
    Key(Box<KeyFacts>),
}

impl StoredBody {
    pub fn kind(&self) -> EventKind {
        match self {
            StoredBody::Full(b) => b.kind(),
            StoredBody::Join(_) => EventKind::MemberJoin,
            StoredBody::Devices(_) => EventKind::DeviceListUpdate,
            StoredBody::Key(_) => EventKind::KeyUpdate,
        }
    }
}

/// 서명된 기기 목록에서 뽑은 것.
#[derive(Clone, Debug)]
pub(crate) struct ListFacts {
    pub user: UserId,
    pub version: u64,
    pub cosigner: Option<DeviceId>,
    /// 간추린 기록. 적용하면 같은 `Arc`를 상태에 넣습니다(키 약속을 적용마다 다시 계산하지 않음).
    pub record: Arc<MemberDevices>,
    /// 모양 검사(항목 수·중복·이름·기기 ID와 서명 키)를 통과했는지.
    pub shape_ok: bool,
    /// 신원 서명·공동 서명(·OTP 서명)이 맞는지(모양이 맞을 때만 확인, 아니면 거짓).
    pub sig_ok: bool,
    /// 계정 OTP 키의 서명이 있는지(맞는지는 `sig_ok`).
    pub otp_signed: bool,
}

impl ListFacts {
    pub fn of(sdl: &SignedDeviceList) -> Self {
        let shape_ok = check_device_list_shape(sdl).is_ok();
        ListFacts {
            user: sdl.list.user,
            version: sdl.list.version,
            cosigner: sdl.cosigner(),
            record: Arc::new(MemberDevices::from_list(&sdl.list)),
            shape_ok,
            sig_ok: shape_ok && sdl.verify().is_ok(),
            otp_signed: sdl.otp_sig.is_some(),
        }
    }
}

/// 초대 증명에서 뽑은 것.
#[derive(Clone, Debug)]
pub(crate) struct InviteFacts {
    pub token_hash: [u8; 32],
    pub at: Timestamp,
    /// 증명 서명에 실린 공개키의 키 ID. 혼합 서명은 실린 키로만 확인되므로([`yangpa_core::sign::verify`]) 다른 키로는
    /// 늘 실패합니다. 그래서 초대 키가 무엇이든 (서명자 = 초대 키 && 서명이 맞음)이 확인 결과와 같습니다.
    signer: [u8; 32],
    sig_ok: bool,
}

impl InviteFacts {
    fn of(space: &SpaceId, proof: &InviteProof, devices: &SignedDeviceList) -> Self {
        // 짧은 서명이면 실린 키가 없어 어떤 초대 키로도 확인되지 않습니다(빈 서명자 + 실패).
        let signer = proof.sig.signer();
        InviteFacts {
            token_hash: proof.token_hash,
            at: proof.at,
            signer: signer.unwrap_or_default(),
            sig_ok: signer.is_some_and(|s| proof.verify(&s, space, devices).is_ok()),
        }
    }

    /// 초대 공개 키(`InviteInfo::verify_key`)로 확인되는지.
    pub fn verifies_with(&self, key: &[u8; 32]) -> bool {
        self.signer == *key && self.sig_ok
    }
}

/// 가입 본문에서 뽑은 것.
#[derive(Clone, Debug)]
pub(crate) struct JoinFacts {
    pub member: Member,
    pub list: ListFacts,
    /// 프로필이 없거나, 가입자의 것이고 서명이 맞는지.
    pub profile_ok: bool,
    pub invite: Option<InviteFacts>,
}

impl JoinFacts {
    pub fn of(
        space: &SpaceId,
        member: Member,
        devices: &SignedDeviceList,
        profile: Option<&SignedProfile>,
        invite: Option<&InviteProof>,
    ) -> Self {
        let profile_ok = profile.is_none_or(|p| p.user == member.user && p.verify().is_ok());
        JoinFacts {
            list: ListFacts::of(devices),
            profile_ok,
            invite: invite.map(|p| InviteFacts::of(space, p, devices)),
            member,
        }
    }
}

/// 기기 목록 갱신 본문에서 뽑은 것.
#[derive(Clone, Debug)]
pub(crate) struct DevicesFacts {
    pub list: ListFacts,
}

impl DevicesFacts {
    pub fn of(devices: &SignedDeviceList) -> Self {
        DevicesFacts {
            list: ListFacts::of(devices),
        }
    }
}

/// 갱신 종류(목록을 뺀 것). 모으는 중인 크루 갱신이 정규 인코딩에 들어가므로 직렬화합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UpdateKind {
    Advance,
    Rotate,
    Refresh,
    FullRekey { generation: u64 },
    RRefresh,
}

impl UpdateKind {
    pub fn of(k: &KeyUpdateKind) -> Self {
        match k {
            KeyUpdateKind::Advance { .. } => UpdateKind::Advance,
            KeyUpdateKind::Rotate { .. } => UpdateKind::Rotate,
            KeyUpdateKind::Refresh => UpdateKind::Refresh,
            KeyUpdateKind::FullRekey { generation, .. } => UpdateKind::FullRekey {
                generation: *generation,
            },
            KeyUpdateKind::RRefresh => UpdateKind::RRefresh,
        }
    }

    /// 제거 목록을 실을 수 있는 종류인지.
    pub fn removes(self) -> bool {
        matches!(self, UpdateKind::Rotate | UpdateKind::FullRekey { .. })
    }

    pub fn is_full_rekey(self) -> bool {
        matches!(self, UpdateKind::FullRekey { .. })
    }
}

/// `KeyUpdate` 조각 하나에서 해소기가 보는 것. 목록은 이 조각의 구간이고(정렬됨), 합친 갱신은 조각들의 합입니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyFacts {
    pub group: KeyGroupId,
    pub seq: u64,
    pub prev: Option<EventId>,
    pub kind: UpdateKind,
    pub root_commit: [u8; 32],
    pub prev_root_commit: [u8; 32],
    pub crew: Option<CrewRef>,
    pub depth: u8,
    pub part: u16,
    pub parts: u16,
    /// 조각마다 같아야 하는 머리([`KeyUpdatePayload::same_update`]가 보는 것)의 해시. 이어지는 조각이 같은 갱신의
    /// 것인지 이것으로 봅니다.
    pub digest: [u8; 32],
    pub joined: Vec<DeviceId>,
    pub removed: Vec<DeviceId>,
    /// 트리 칸을 채운 기기(정렬, 중복 없음).
    pub fills: Vec<DeviceId>,
    /// 봉인 상자 수신 기기(정렬, 중복 없음).
    pub recipients: Vec<DeviceId>,
    /// 수신자를 읽을 수 없는 봉인 상자가 있음.
    pub unreadable: bool,
    pub wraps: usize,
}

impl KeyFacts {
    pub fn of(p: &KeyUpdatePayload) -> Self {
        let mut fills: Vec<DeviceId> = p
            .layout
            .iter()
            .filter_map(|op| match op {
                LayoutOp::Fill { device, .. } => Some(*device),
                LayoutOp::Clear { .. } => None,
            })
            .collect();
        fills.sort_unstable();
        fills.dedup();
        let mut recipients = Vec::new();
        let mut unreadable = false;
        for b in &p.sealed {
            match b.recipients() {
                Ok(r) => recipients.extend(r),
                Err(_) => unreadable = true,
            }
        }
        recipients.sort_unstable();
        recipients.dedup();
        // 형식 검사를 거친 목록은 이미 정렬돼 있지만, 겹침 검사(이진 탐색)가 기대므로 여기서도 맞춥니다.
        let mut joined = p.kind.joined().to_vec();
        let mut removed = p.kind.removed().to_vec();
        joined.sort_unstable();
        removed.sort_unstable();
        KeyFacts {
            group: p.group,
            seq: p.seq,
            prev: p.prev,
            kind: UpdateKind::of(&p.kind),
            root_commit: p.root_commit,
            prev_root_commit: p.prev_root_commit,
            crew: p.crew,
            depth: p.depth,
            part: p.part,
            parts: p.parts,
            digest: head_digest(p),
            joined,
            removed,
            fills,
            recipients,
            unreadable,
            wraps: p.wraps.len(),
        }
    }
}

/// 조각마다 같아야 하는 머리의 해시: 목록·감싸기·상자·`r_ct`·조각 번호를 비운 페이로드의 인코딩.
fn head_digest(p: &KeyUpdatePayload) -> [u8; 32] {
    let kind = match &p.kind {
        KeyUpdateKind::Advance { .. } => KeyUpdateKind::Advance { joined: Vec::new() },
        KeyUpdateKind::Rotate { .. } => KeyUpdateKind::Rotate {
            removed: Vec::new(),
            joined: Vec::new(),
        },
        KeyUpdateKind::FullRekey { generation, .. } => KeyUpdateKind::FullRekey {
            generation: *generation,
            removed: Vec::new(),
            joined: Vec::new(),
        },
        k => k.clone(),
    };
    let head = KeyUpdatePayload {
        group: p.group,
        seq: p.seq,
        prev: p.prev,
        kind,
        root_commit: p.root_commit,
        prev_root_commit: p.prev_root_commit,
        crew: p.crew,
        bundle_root: p.bundle_root,
        shards: p.shards,
        depth: p.depth,
        salt: p.salt,
        layout: Vec::new(),
        wraps: Vec::new(),
        sealed: Vec::new(),
        r_ct: None,
        part: 0,
        parts: p.parts,
    };
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/key-update-head");
    h.update(&head.encode());
    *h.finalize().as_bytes()
}
