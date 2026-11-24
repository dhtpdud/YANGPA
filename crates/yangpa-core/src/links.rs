//! 친구 추가 링크, 크루 초대 링크, 보관 노드 제안 링크, 기기 연결 링크.
//!
//! 링크에는 IP 주소를 넣지 않습니다. 접속 정보는 네트워크 키(PeerId), 릴레이 경유 주소,
//! onion 주소만 담고, 실제 주소는 DHT·로컬 탐색으로 찾습니다. 기기 연결 링크만 예외입니다: 내 기기끼리 쓰는 일회용
//! 링크라 직접 연결 주소([`DirectContact`])를 함께 담습니다.
//! 링크 스킴은 가칭 `yangpa://`입니다(정식 이름 미결정).
//!
//! 만드는 링크는 웹 초대 페이지 주소(`{WEB_BASE}#invite/…`)입니다. 앱이 없는 사람도 눌러서 내려받기로 가고, 있는
//! 사람은 페이지가 `yangpa://invite/…`로 앱을 엽니다. `#` 뒤는 브라우저가 서버로 보내지 않으므로 페이지 서버는
//! 링크 내용을 모릅니다. 읽을 때는 두 형식을 다 받습니다.
//!
//! 사람에게 주는 연락처·초대 링크는 짧은 링크([`ShortLink`])입니다. 여기서 만드는 긴 링크는 만든 기기가 갖고 있다가
//! 짧은 링크로 온 기기에 내줍니다(`YpRequest::GetLink`). onion이 없는 노드(직접 연결)는 긴 링크를 그대로 줍니다.

use serde::{Deserialize, Serialize};

use crate::checkpoint::CheckpointPin;
use crate::error::CoreError;
use crate::ids::{DeviceId, EventId, LinkCode, LinkToken, SpaceId, UserId};
use crate::sign::SigningKeypair;
use crate::time::Timestamp;

pub const SCHEME: &str = "yangpa://";
/// 웹 초대 페이지(정적 페이지, `web/index.html`). 앱의 `apps/desktop/src/lib/links.ts`와 같게 둡니다.
pub const WEB_BASE: &str = "https://dhtpdud.github.io/YANGPA/";
const FRIEND_PATH: &str = "friend/";
const INVITE_PATH: &str = "invite/";
const ARCHIVE_PATH: &str = "archive/";
const DEVICE_PATH: &str = "device/";
const LINK_VERSION: u8 = 1;
/// 기기 연결 링크에 담는 직접 주소 상한(QR이 찍기 어렵게 커지지 않게).
pub const MAX_DIRECT_ADDRS: usize = 6;
/// 초대 링크 판. 2판: 끝에 고정 체크포인트([`InviteLink::checkpoint`])가 붙습니다.
const INVITE_LINK_VERSION: u8 = 2;
/// 짧은 링크 판(긴 링크의 판과 겹치지 않게 둠).
const SHORT_LINK_VERSION: u8 = 3;
/// 짧은 링크의 바이트 수: 판, onion 공개키, 확인값.
const SHORT_LINK_LEN: usize = 1 + 32 + LinkCode::LEN;
/// 긴 링크 바이트 상한.
const MAX_LINK_BYTES: usize = 4096;
/// `GetLink`의 답으로 받는 긴 링크 글자 상한([`MAX_LINK_BYTES`]의 base58에 넉넉히).
pub const MAX_LINK_CHARS: usize = 6000;

/// 링크 만든 노드에 닿는 방법.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContactInfo {
    /// 링크 만든 노드의 libp2p PeerId(바이트). 신원·기기 키와 분리된 네트워크 키입니다.
    #[serde(with = "serde_bytes")]
    pub peer_id: Vec<u8>,
    /// 릴레이 경유 주소(`/p2p/<relay>/p2p-circuit` 형태, 직접 연결 노드가 만든 링크만). 자기 IP는 담지 않습니다.
    #[serde(default)]
    pub relay_addrs: Vec<String>,
    /// 링크 만든 기기의 onion 주소(`xxxx.onion:port`). Tor로 도는 기기(앱 사용자 기기)만 넣습니다.
    #[serde(default)]
    pub onion: Option<String>,
}

impl ContactInfo {
    /// IP 주소가 직접 들어간 multiaddr가 없는지 검사합니다(릴레이 경유 주소는 릴레이의
    /// 주소만 담기므로 허용하되, 회로 뒤쪽에 IP가 오면 거부).
    pub fn check_no_own_ip(&self) -> Result<(), CoreError> {
        for a in &self.relay_addrs {
            let Some((_, after)) = a.split_once("/p2p-circuit") else {
                return Err(CoreError::InvalidLink("릴레이 경유 주소만 허용".into()));
            };
            if after.contains("/ip4/") || after.contains("/ip6/") || after.contains("/dns") {
                return Err(CoreError::InvalidLink(
                    "링크에 자기 IP를 넣을 수 없음".into(),
                ));
            }
        }
        Ok(())
    }
}

/// 친구 추가 링크.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLink {
    pub v: u8,
    pub user: UserId,
    pub token: LinkToken,
    pub expires_at: Option<Timestamp>,
    pub single_use: bool,
    pub contact: ContactInfo,
    /// 받는 사람 화면에 보여줄 이름 힌트(검증되지 않음).
    #[serde(default)]
    pub name_hint: String,
}

/// 기기 연결 링크: 내 기기가 그 자리에서 QR·글자로 띄웁니다. 토큰은 띄운 기기의 RAM에만 있고 곧 끝납니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLink {
    pub v: u8,
    pub user: UserId,
    pub token: LinkToken,
    pub contact: ContactInfo,
    /// 띄운 기기의 직접 스웜. 없으면 Tor로만 닿습니다(오프라인 모드, 모두 TURN으로 보내는 설정).
    pub direct: Option<DirectContact>,
}

/// 기기 연결 링크를 띄운 기기의 직접 스웜(Tor 노드와 다른 임시 키)과 주소. 같은 망이면 Tor를 거치지 않고 바로 닿습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectContact {
    #[serde(with = "serde_bytes")]
    pub peer_id: Vec<u8>,
    /// multiaddr 바이트 최대 [`MAX_DIRECT_ADDRS`]개(글자로 담으면 QR이 세 배 촘촘해져 찍기 어려움).
    pub addrs: Vec<serde_bytes::ByteBuf>,
}

/// 크루 초대 링크.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteLink {
    pub v: u8,
    pub space: SpaceId,
    /// 공간의 첫 이벤트(`SpaceCreate`) ID. 가입자는 이 값으로 첫 이벤트를 고정해, 초대를 받아 준 멤버가
    /// 가짜 공간 상태를 꾸며 보내도 받아들이지 않습니다.
    pub genesis: EventId,
    pub token: LinkToken,
    pub expires_at: Option<Timestamp>,
    pub max_uses: Option<u32>,
    pub contact: ContactInfo,
    #[serde(default)]
    pub name_hint: String,
    /// 링크를 만들 때의 최신 체크포인트(약 40B). 가입자는 이 체크포인트에서 `prev`로 이어진 사슬만 받습니다. 없으면
    /// 시작 이벤트의 소유자부터 사슬을 확인합니다.
    pub checkpoint: Option<CheckpointPin>,
}

/// 보관 노드 제안 링크(`archive.offer`). 관리자가 이 링크로 보관 노드를 지정합니다(protocol.md "보관 전용 노드").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveOfferLink {
    pub v: u8,
    pub user: UserId,
    pub device: DeviceId,
    /// 1회용 토큰(보관 노드는 해시만 저장).
    pub token: LinkToken,
    pub expires_at: Option<Timestamp>,
    pub contact: ContactInfo,
    #[serde(default)]
    pub name_hint: String,
}

impl ArchiveOfferLink {
    pub fn new(
        user: UserId,
        device: DeviceId,
        contact: ContactInfo,
        expires_at: Option<Timestamp>,
    ) -> Self {
        ArchiveOfferLink {
            v: LINK_VERSION,
            user,
            device,
            token: LinkToken::random(),
            expires_at,
            contact,
            name_hint: String::new(),
        }
    }

    pub fn to_url(&self) -> Result<String, CoreError> {
        self.contact.check_no_own_ip()?;
        encode(ARCHIVE_PATH, self)
    }

    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let l: ArchiveOfferLink = decode(ARCHIVE_PATH, s)?;
        if l.v != LINK_VERSION {
            return Err(CoreError::UnsupportedVersion(l.v));
        }
        l.contact.check_no_own_ip()?;
        if l.expires_at.is_some_and(|t| t.is_past()) {
            return Err(CoreError::Expired);
        }
        Ok(l)
    }
}

fn encode<T: Serialize>(path: &str, v: &T) -> Result<String, CoreError> {
    let bytes = postcard::to_stdvec(v)?;
    Ok(format!(
        "{WEB_BASE}#{path}{}",
        bs58::encode(bytes).into_string()
    ))
}

/// 링크에서 스킴·웹 주소를 뗀 나머지(`invite/…`).
fn fragment(s: &str) -> Option<&str> {
    let s = s.trim();
    // 웹 주소는 호스트를 따지지 않습니다. 내용이 스스로 검증되므로(서명·첫 이벤트 고정), 페이지를 옮겨도 예전 링크가 통합니다.
    s.strip_prefix(SCHEME).or_else(|| {
        s.split_once('#')
            .filter(|(base, _)| base.starts_with("https://"))
            .map(|(_, frag)| frag)
    })
}

fn decode_bytes(path: &str, s: &str) -> Result<Vec<u8>, CoreError> {
    let rest = fragment(s)
        .and_then(|r| r.strip_prefix(path))
        .ok_or_else(|| CoreError::InvalidLink("링크 형식이 아님".into()))?;
    let bytes = bs58::decode(rest)
        .into_vec()
        .map_err(|_| CoreError::InvalidLink("base58 오류".into()))?;
    if bytes.len() > MAX_LINK_BYTES {
        return Err(CoreError::LimitExceeded("링크 길이"));
    }
    Ok(bytes)
}

fn decode<T: for<'de> Deserialize<'de>>(path: &str, s: &str) -> Result<T, CoreError> {
    postcard::from_bytes(&decode_bytes(path, s)?).map_err(|e| CoreError::InvalidLink(e.to_string()))
}

/// 긴 링크의 확인값. 주소의 호스트는 빼고 내용(`invite/…`)만 셉니다.
pub fn link_code(long: &str) -> LinkCode {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v1/short-link");
    h.update(fragment(long).unwrap_or(long).as_bytes());
    let mut out = [0u8; LinkCode::LEN];
    out.copy_from_slice(&h.finalize().as_bytes()[..LinkCode::LEN]);
    LinkCode(out)
}

/// 짧은 연락처·초대 링크: 만든 기기의 onion 공개키와 긴 링크의 확인값([`link_code`])만 담습니다. 받는 쪽은 그 onion에서
/// 긴 링크를 받아 확인값과 맞는 것만 씁니다. onion 주소가 만든 기기임을, 확인값이 긴 링크의 내용(토큰·시작 이벤트·
/// 체크포인트)을 고정하므로 긴 링크를 그대로 건네받은 것과 같습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortLink {
    pub onion: [u8; 32],
    pub code: LinkCode,
}

impl ShortLink {
    /// 긴 링크를 줄인 주소. 종류(`friend/`·`invite/`)는 그대로입니다.
    pub fn url_of(long: &str, onion: [u8; 32]) -> Option<String> {
        let (path, _) = fragment(long)?.split_once('/')?;
        let mut bytes = vec![SHORT_LINK_VERSION];
        bytes.extend_from_slice(&onion);
        bytes.extend_from_slice(link_code(long).as_bytes());
        Some(format!(
            "{WEB_BASE}#{path}/{}",
            bs58::encode(bytes).into_string()
        ))
    }

    /// 짧은 링크면 읽습니다. 긴 링크거나 다른 종류면 `None`.
    fn parse(path: &str, s: &str) -> Option<Self> {
        let b = decode_bytes(path, s).ok()?;
        if b.len() != SHORT_LINK_LEN || b[0] != SHORT_LINK_VERSION {
            return None;
        }
        Some(ShortLink {
            onion: b[1..33].try_into().ok()?,
            code: LinkCode::from_slice(&b[33..]).ok()?,
        })
    }
}

/// URL의 `#` 뒤가 친구·초대·보관 노드 링크인지(링크 미리보기에서 뺄 때).
pub fn is_link_fragment(frag: &str) -> bool {
    [FRIEND_PATH, INVITE_PATH, ARCHIVE_PATH]
        .iter()
        .any(|p| frag.starts_with(p))
}

impl DeviceLink {
    pub fn new(user: UserId, contact: ContactInfo, direct: Option<DirectContact>) -> Self {
        DeviceLink {
            v: LINK_VERSION,
            user,
            token: LinkToken::random(),
            contact,
            direct,
        }
    }

    /// 웹 페이지를 거치지 않는 앱 전용 주소입니다(휴대폰 카메라로 찍어도 브라우저로 가지 않게).
    pub fn to_url(&self) -> Result<String, CoreError> {
        self.contact.check_no_own_ip()?;
        let bytes = postcard::to_stdvec(self)?;
        Ok(format!(
            "{SCHEME}{DEVICE_PATH}{}",
            bs58::encode(bytes).into_string()
        ))
    }

    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let l: DeviceLink = decode(DEVICE_PATH, s)?;
        if l.v != LINK_VERSION {
            return Err(CoreError::UnsupportedVersion(l.v));
        }
        l.contact.check_no_own_ip()?;
        Ok(l)
    }
}

impl FriendLink {
    pub fn new(
        user: UserId,
        contact: ContactInfo,
        expires_at: Option<Timestamp>,
        single_use: bool,
    ) -> Self {
        FriendLink {
            v: LINK_VERSION,
            user,
            token: LinkToken::random(),
            expires_at,
            single_use,
            contact,
            name_hint: String::new(),
        }
    }

    pub fn to_url(&self) -> Result<String, CoreError> {
        self.contact.check_no_own_ip()?;
        encode(FRIEND_PATH, self)
    }

    /// 짧은 연락처 링크면 읽습니다.
    pub fn parse_short(s: &str) -> Option<ShortLink> {
        ShortLink::parse(FRIEND_PATH, s)
    }

    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let l: FriendLink = decode(FRIEND_PATH, s)?;
        if l.v != LINK_VERSION {
            return Err(CoreError::UnsupportedVersion(l.v));
        }
        l.contact.check_no_own_ip()?;
        if l.expires_at.is_some_and(|t| t.is_past()) {
            return Err(CoreError::Expired);
        }
        Ok(l)
    }
}

impl InviteLink {
    pub fn to_url(&self) -> Result<String, CoreError> {
        self.contact.check_no_own_ip()?;
        encode(INVITE_PATH, self)
    }

    /// 짧은 초대 링크면 읽습니다.
    pub fn parse_short(s: &str) -> Option<ShortLink> {
        ShortLink::parse(INVITE_PATH, s)
    }

    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let l: InviteLink = decode(INVITE_PATH, s)?;
        if l.v != INVITE_LINK_VERSION {
            return Err(CoreError::UnsupportedVersion(l.v));
        }
        l.contact.check_no_own_ip()?;
        if l.expires_at.is_some_and(|t| t.is_past()) {
            return Err(CoreError::Expired);
        }
        Ok(l)
    }

    pub fn new(
        space: SpaceId,
        genesis: EventId,
        contact: ContactInfo,
        expires_at: Option<Timestamp>,
        max_uses: Option<u32>,
    ) -> Self {
        InviteLink {
            v: INVITE_LINK_VERSION,
            space,
            genesis,
            token: LinkToken::random(),
            expires_at,
            max_uses,
            contact,
            name_hint: String::new(),
            checkpoint: None,
        }
    }
}

/// 초대 토큰에서 만드는 서명 키. 상태에는 공개 키([`invite_verify_key`])만 두고, 가입자는 토큰을 내보이는 대신
/// 이 키로 가입 증명(`body::InviteProof`)에 서명합니다(토큰 원문이 상태에 남으면 멤버 누구나 그 초대로 사람을 들임).
pub fn invite_keypair(token: &LinkToken) -> SigningKeypair {
    SigningKeypair::from_seed(&blake3::derive_key(
        "yangpa/v1/invite-key",
        token.as_bytes(),
    ))
}

/// 초대의 가입 증명을 확인하는 공개 키(`InviteInfo::verify_key`).
pub fn invite_verify_key(token: &LinkToken) -> [u8; 32] {
    invite_keypair(token).public()
}

/// 토큰은 원문 대신 해시로 저장·대조합니다.
pub fn token_hash(token: &LinkToken) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"yangpa/v1/link-token\0");
    h.update(token.as_bytes());
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_link_roundtrip() {
        let link = DeviceLink::new(
            UserId([3; 32]),
            ContactInfo {
                peer_id: vec![1, 2, 3],
                relay_addrs: vec![],
                onion: Some("abc.onion:443".into()),
            },
            Some(DirectContact {
                peer_id: vec![4, 5, 6],
                addrs: vec![serde_bytes::ByteBuf::from(vec![4, 192, 168, 0, 2])],
            }),
        );
        let url = link.to_url().unwrap();
        assert!(url.starts_with("yangpa://device/"));
        assert_eq!(DeviceLink::parse(&url).unwrap(), link);
        assert!(FriendLink::parse(&url).is_err());
    }

    #[test]
    fn short_link_pins_the_long_link() {
        let onion = [0xff; 32];
        let invite = || {
            let l = InviteLink::new(
                SpaceId::random(),
                EventId::random(),
                ContactInfo::default(),
                None,
                Some(10),
            );
            l.to_url().unwrap()
        };
        let frag = |u: &str| u.split_once('#').unwrap().1.to_owned();

        let long = invite();
        let url = ShortLink::url_of(&long, onion).unwrap();
        assert!(url.starts_with(&format!("{WEB_BASE}#invite/")));
        assert!(url.len() <= 100, "{}자: {url}", url.len());
        let short = InviteLink::parse_short(&url).unwrap();
        assert_eq!((short.onion, short.code), (onion, link_code(&long)));
        // 웹 페이지가 앱에 넘기는 형식도 같은 링크이고, 확인값은 주소의 호스트와 상관없습니다.
        assert_eq!(
            InviteLink::parse_short(&format!("{SCHEME}{}", frag(&url))),
            Some(short)
        );
        assert_eq!(link_code(&frag(&long)), short.code);
        // 긴 링크와 짧은 링크, 초대와 연락처를 서로 다른 것으로 읽지 않습니다.
        assert!(InviteLink::parse_short(&long).is_none());
        assert!(InviteLink::parse(&url).is_err());
        assert!(FriendLink::parse_short(&url).is_none());
        // 잘린 링크, 다른 긴 링크.
        assert!(InviteLink::parse_short(&url[..url.len() - 1]).is_none());
        assert_ne!(link_code(&invite()), short.code);

        let friend = FriendLink::new(UserId::random(), ContactInfo::default(), None, true)
            .to_url()
            .unwrap();
        let url = ShortLink::url_of(&friend, onion).unwrap();
        assert!(url.starts_with(&format!("{WEB_BASE}#friend/")));
        assert_eq!(
            FriendLink::parse_short(&url).unwrap().code,
            link_code(&friend)
        );
        assert!(InviteLink::parse_short(&url).is_none());
    }

    #[test]
    fn friend_link_roundtrip() {
        let contact = ContactInfo {
            peer_id: vec![1, 2, 3],
            relay_addrs: vec!["/ip4/1.2.3.4/udp/4001/quic-v1/p2p/12D3KooWRelay/p2p-circuit".into()],
            onion: None,
        };
        let link = FriendLink::new(UserId::random(), contact, None, true);
        let url = link.to_url().unwrap();
        assert!(url.starts_with(&format!("{WEB_BASE}#friend/")));
        assert_eq!(FriendLink::parse(&url).unwrap(), link);
        // 웹 페이지가 앱에 넘기는 형식, 다른 호스트로 옮긴 페이지 주소도 같은 링크입니다.
        let payload = url.split_once('#').unwrap().1;
        assert_eq!(
            FriendLink::parse(&format!("{SCHEME}{payload}")).unwrap(),
            link
        );
        assert_eq!(
            FriendLink::parse(&format!("https://yangpa.example/#{payload}")).unwrap(),
            link
        );
        assert!(FriendLink::parse(&format!("http://yangpa.example/#{payload}")).is_err());
    }

    #[test]
    fn rejects_own_ip_and_expired() {
        let bad = ContactInfo {
            peer_id: vec![1],
            relay_addrs: vec!["/ip4/10.0.0.2/tcp/1".into()],
            onion: None,
        };
        assert!(
            FriendLink::new(UserId::random(), bad, None, false)
                .to_url()
                .is_err()
        );
        let bad2 = ContactInfo {
            peer_id: vec![1],
            relay_addrs: vec!["/p2p/relay/p2p-circuit/ip4/10.0.0.2/tcp/1".into()],
            onion: None,
        };
        assert!(
            FriendLink::new(UserId::random(), bad2, None, false)
                .to_url()
                .is_err()
        );
        let expired = FriendLink::new(
            UserId::random(),
            ContactInfo::default(),
            Some(Timestamp(1)),
            false,
        );
        let url = expired.to_url().unwrap();
        assert_eq!(FriendLink::parse(&url), Err(CoreError::Expired));
    }

    #[test]
    fn invite_link_roundtrip() {
        let l = InviteLink::new(
            SpaceId::random(),
            EventId::random(),
            ContactInfo::default(),
            None,
            Some(10),
        );
        let url = l.to_url().unwrap();
        assert_eq!(InviteLink::parse(&url).unwrap(), l);
        assert!(FriendLink::parse(&url).is_err());
        // 고정 체크포인트는 약 40B를 더합니다.
        let mut pinned = l.clone();
        pinned.checkpoint = Some(CheckpointPin {
            seq: 300,
            id: crate::ids::CheckpointId([9; 32]),
        });
        let url2 = pinned.to_url().unwrap();
        assert_eq!(InviteLink::parse(&url2).unwrap(), pinned);
        assert!(url2.len() - url.len() <= 50, "{}", url2.len() - url.len());
    }
}
