//! 음성·화면공유: 통화 참가자, 프레임 E2EE 키, WebRTC 신호 중계.
//!
//! - 참가자 목록은 RAM에만 둡니다(공간·채널별). 참가하면 채널을 볼 수 있는 연결된 기기 모두에게 `Join`을 보내고,
//!   이미 참가 중인 기기는 새 참가자에게 자기 상태를 `Join{reply}`로 돌려준 뒤 답합니다. 그래서 참가 요청을 다
//!   보낸 때에는 참가자 전원을 압니다. 나간 기기(`Leave`)·끊긴 노드의 기기는 목록에서 뺍니다.
//! - 프레임 키: 참가자 중 기기 ID가 가장 작은 기기(키 관리 기기)가 참가자가 바뀔 때마다 무작위 32바이트 키를 새
//!   세대로 만들어 다른 참가자의 상자 키로 봉인해 보냅니다(`MediaKey`). 받는 쪽은 **자기 목록의 관리 기기**가 보낸,
//!   그 기기에게서 받은 것보다 큰 세대만 씁니다. 다른 참가자가 보낸 키는 그 기기가 관리 기기가 될 때까지 따로 둡니다
//!   (참가자 목록이 잠깐 엇갈린 경우). 관리 기기가 나가면 다음 관리 기기가 새 세대를 만듭니다(protocol.md "음성·화면공유").
//! - 공간 상태가 바뀌면(추방·기기 폐기·권한 변경·채널 삭제) 자격을 잃은 참가자를 빼고 새 키를 만듭니다.
//! - 신호(`Offer`·`Answer`·`Ice`·`WatchScreen`)는 지금 참가자 사이에서만 오가고, 데몬은 받은 것을 그대로 앱에 넘깁니다.
//! - 토폴로지: 키 관리 기기가 참가자 수(히스테리시스: `mesh_max`를 넘으면 SFU, `mesh_min` 이하면 메시)와 후보의
//!   조건(`Join`·`State`의 `sfu`, 참가하지 않는 호스트 노드의 `SfuHost`)으로 SFU 호스트를 정해 `SfuPlan`으로 알립니다.
//!   받는 쪽은 자기 목록의 관리 기기가 보낸, 더 큰 번호의 결정만 따릅니다(키와 같은 규칙). 관리 기기는 결정을
//!   못 보낸 상대에게 잠깐씩 다시 보내고, 20초마다 지금 결정을 한 번 더 보냅니다(결정을 놓친 호스트가 통화방을
//!   열지 않은 채 남지 않게). SFU 신호(`Sfu*`)는 앱 → 이 데몬 → 호스트 데몬으로 가고, 호스트는 참가자가 보낸 것만
//!   엔진(`sfu`)에 넘깁니다. 참가하지 않고 호스트만 맡는 노드는 그 채널에 `manage_voice`가 있어야 후보가 됩니다
//!   (호스트는 참가자의 IP·말하는 때를 알게 되므로 관리자가 정한 노드만).
//! - 통화 ID는 기기마다 앱에 보여 주는 이름일 뿐이고, 데몬끼리는 (공간, 채널)로 통화를 찾습니다.
//! - WebRTC는 직접 연결이라 참여자에게 IP가 보입니다. Tor로 도는 기기의 일반 채널(P2P 전용이 아닌 채널) 통화는
//!   `Topology::Tor`로, 앱이 봉인한 소리 패킷을 Tor 연결로 나릅니다(`media`). 그 통화에서는 SFU를 고르지 않고, WebRTC는
//!   서로 신뢰한(신뢰 스위치) 참가자끼리만 잇습니다(`VoiceParticipant.direct`, 익명 채널 제외). P2P 전용 채널은 참가해야
//!   통화하고(참가하면 직접 연결, WebRTC). 데몬은 내 STUN
//!   서버와 쓰기로 한 그 크루의 STUN 서버를 `VoiceJoinResult.ice_servers`로 내줍니다(`stun::call_ice`).

pub(crate) mod media;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine;
use futures::future::join_all;
use parking_lot::Mutex;
use rand::RngCore;
use tokio::sync::broadcast;
use tracing::{debug, warn};
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{CallId, ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, Permissions, SpaceState};
use yangpa_core::wire::{SfuCaps, SignalPayload, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::PeerId;
use yangpa_rpc::api::serde_util::SecretString;
use yangpa_rpc::api::{
    ANONYMOUS_USER, ChannelView, Notification, Topology, VoiceJoinResult, VoiceParticipant,
};
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{peers, sfu, tor};

/// 메시에서 SFU로 넘어가는 참가자 수의 기본값(decisions.md: P4 측정 전 기본값). 실제 값은 `NodeConfig::sfu`.
#[cfg(test)]
pub(crate) const MESH_MAX: usize = 6;
/// 한 통화에서 기억하는 참가하지 않는 호스트 노드 수.
const MAX_HOSTS: usize = 16;
/// 참가할 때 키 관리 기기의 키를 기다리는 시간.
const KEY_WAIT: Duration = Duration::from_secs(5);
/// 새 참가자에게 자기 상태를 돌려줄 때 기다리는 시간.
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// 통화 중인 노드와의 연결이 쉬어서 끊기지 않게 확인하는 간격.
const KEEPALIVE: Duration = Duration::from_secs(20);
/// 신호 크기 한도.
const MAX_SDP: usize = 64 * 1024;
const MAX_CANDIDATE: usize = 4 * 1024;
const MAX_MID: usize = 64;
/// 앱이 SFU 제안·응답에 싣는 트랙 표 크기 상한.
const MAX_SFU_TRACKS: usize = 8;
/// 한 통화의 참가자 상한(봉인 상자 크기·신호 수를 묶음).
const MAX_PARTICIPANTS: usize = 64;
/// 받아들이는 키 세대의 상한. 앱은 세대를 JSON 숫자로 다루므로 2^53보다 훨씬 작게 둡니다.
pub(crate) const GEN_LIMIT: u64 = 1 << 40;
/// 새 관리 기기가 이어 받는 세대(`Join`이 알린 세대, 남이 만든 키의 세대)의 상한. 넘는 값은 무시해, 참가자가 큰 값을
/// 알려도 관리 기기가 세대를 더 올릴 여유(2^39번)가 남게 합니다.
const CLAIM_LIMIT: u64 = 1 << 39;
/// `MediaKey` 봉인 상자 크기 상한(참가자 64명분보다 넉넉히).
const MAX_SEALED: usize = 16 * 1024;
/// 막 인증한 상대가 아직 이쪽 인증을 마치지 못해 통화 알림을 거절하면 다시 보내는 간격.
const ANNOUNCE_RETRY: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_millis(300),
    Duration::from_secs(1),
    Duration::from_secs(3),
];

/// 음성 영역의 노드 수준 상태(`Node::voice`).
#[derive(Default)]
pub struct VoiceState {
    calls: Mutex<HashMap<(SpaceId, ChannelId), Call>>,
    key_changed: tokio::sync::Notify,
    /// SFU 호스트 실행기.
    pub(crate) sfu: sfu::SfuState,
    /// Tor 통화 소리 패킷의 보낸 기기별 빈도 제한.
    pub(crate) media_rate: Mutex<media::Buckets>,
}

struct Call {
    id: CallId,
    parts: BTreeMap<DeviceId, Part>,
    /// 이 기기가 참가 중인지(`parts`에도 들어 있음).
    me_in: bool,
    /// 참가 알림을 돌리는 중(그동안 받은 답마다 키를 새로 만들지 않게).
    joining: bool,
    key: Option<MediaKey>,
    /// 참가자들이 `Join`으로 알린 세대 중 가장 큰 것(`CLAIM_LIMIT` 이하만). 새 관리 기기가 그보다 큰 세대로 시작하게.
    max_gen: u64,
    /// 이 기기가 관리 기기로서 만든 가장 큰 세대(역할을 잠시 잃었다 되찾아도 받는 쪽이 받아들이게 이어 올림).
    issued: u64,
    /// 관리 기기별로 받아들인 가장 큰 세대(늦게 온 옛 키를 버림). 그 기기가 참가자에서 빠지면 지웁니다.
    seen: BTreeMap<DeviceId, u64>,
    /// 관리 기기가 아닌 참가자가 보낸 키(보낸 기기마다 가장 큰 세대 하나). 그 기기가 관리 기기가 되면 씁니다.
    pending: BTreeMap<DeviceId, MediaKey>,
    /// 화면공유 기기 → 그 화면을 보는 기기(이 기기가 아는 범위).
    viewers: BTreeMap<DeviceId, BTreeSet<DeviceId>>,
    /// 참가자가 알린 SFU 호스트 조건(맡을 수 없는 기기는 없음). 내 것도 들어 있음.
    caps: BTreeMap<DeviceId, SfuCaps>,
    /// 참가하지 않고 SFU만 맡겠다고 알린 노드의 기기(그리고 결정에 나온, 아직 모르는 호스트).
    hosts: BTreeMap<DeviceId, HostOnly>,
    /// 따르는 토폴로지 결정.
    plan: Plan,
    /// 관리 기기별로 받아들인 가장 큰 결정 번호. 그 기기가 참가자에서 빠지면 지웁니다.
    plan_seen: BTreeMap<DeviceId, u64>,
    /// 관리 기기가 아닌 참가자가 보낸 결정(그 기기가 관리 기기가 되면 쓰고, 내가 관리 기기가 되면 지금 호스트의 실마리).
    plan_pending: BTreeMap<DeviceId, Plan>,
    /// 내가 관리 기기로서 낸 가장 큰 결정 번호.
    plan_issued: u64,
    /// 참가하지 않은 채 호스트 후보로 알렸음(`SfuHost`).
    announced: bool,
    /// 마지막으로 알린 호스트 후보 조건(바뀌면 다시 알림).
    announced_caps: Option<SfuCaps>,
    /// 익명 채널에서 보여 줄 참가자 번호(이 기기가 본 들어온 순서, 1부터). 통화가 비면 처음부터 다시 셉니다.
    anon: BTreeMap<DeviceId, u32>,
    /// Tor 통화에서 서로 신뢰해 WebRTC로 바로 잇는 참가자(마지막으로 앱에 알린 것, [`refresh_direct`]).
    direct: BTreeSet<DeviceId>,
}

/// SFU만 맡겠다는 노드(통화 참가자가 아님). 프레임 키는 받지 않습니다.
struct HostOnly {
    peer: Option<PeerId>,
    /// `SfuHost`로 나선 사용자(자격을 다시 볼 때 씀). 결정으로만 안 호스트면 없음.
    user: Option<UserId>,
    /// 앱에 보여 줄 호스트 사용자(결정으로만 안 호스트는 공간 상태에서 찾음).
    owner: Option<UserId>,
    caps: Option<SfuCaps>,
}

/// 토폴로지 결정(키 관리 기기가 냄). `host`가 있으면 SFU, 없으면 메시.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Plan {
    seq: u64,
    host: Option<DeviceId>,
}

/// 메시·SFU 전환 기준(`NodeConfig::sfu`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Thresholds {
    pub mesh_max: usize,
    pub mesh_min: usize,
}

impl Thresholds {
    fn of(node: &Node) -> Self {
        Thresholds {
            mesh_max: node.cfg.sfu.mesh_max,
            mesh_min: node.cfg.sfu.mesh_min,
        }
    }
}

/// 토폴로지를 정합니다(순수 함수). `n`은 참가자 수, `current`는 지금 호스트(메시면 `None`), `candidates`는 맡을 수
/// 있는 기기와 그 조건입니다.
///
/// - 히스테리시스: 메시는 `mesh_max`를 넘어야 SFU로, SFU는 `mesh_min` 이하로 줄어야 메시로 바꿉니다.
/// - 지금 호스트가 계속 맡을 수 있으면(후보이고 수용 인원이 충분) 바꾸지 않습니다(넘겨주기는 비쌈). 다만 먼저
///   맡겠다는 후보(시더)가 늦게 나타났고 지금 호스트는 그렇지 않으면 한 번 넘깁니다(시더의 알림은 참가 알림보다
///   늦게 닿을 수 있고, 다시 켜진 시더가 데스크톱의 짐을 도로 맡게).
/// - 아니면 (먼저 맡겠다고 함, 공인 주소, 릴레이로만 닿는 참가자 수, RTT 묶음, 기기 ID) 순으로 가장 좋은 후보.
/// - 맡을 후보가 없으면 메시로 둡니다(무겁지만 통화는 됨).
pub(crate) fn decide(
    n: usize,
    current: Option<DeviceId>,
    candidates: &[(DeviceId, SfuCaps)],
    t: Thresholds,
) -> Option<DeviceId> {
    let mesh_min = t.mesh_min.min(t.mesh_max);
    let want_sfu = if current.is_some() {
        n > mesh_min
    } else {
        n > t.mesh_max
    };
    if !want_sfu {
        return None;
    }
    let fits = |c: &SfuCaps| usize::from(c.capacity) >= n;
    let best = candidates
        .iter()
        .filter(|(_, c)| fits(c))
        .min_by_key(|(d, c)| rank(d, c));
    if let Some(h) = current
        && let Some((_, cur)) = candidates.iter().find(|(d, c)| *d == h && fits(c))
        && (cur.preferred || !best.is_some_and(|(_, b)| b.preferred))
    {
        return Some(h);
    }
    best.map(|(d, _)| *d)
}

fn rank(d: &DeviceId, c: &SfuCaps) -> (bool, bool, u16, u32, DeviceId) {
    (
        !c.preferred,
        !c.public,
        c.relayed,
        c.rtt_ms.map_or(u32::MAX, |r| r / sfu::RTT_BUCKET_MS),
        *d,
    )
}

/// 호스트 선출에서 같은 조건인지(RTT는 묶음으로 비교해 조금씩 흔들려도 다시 알리지 않음).
fn same_caps(a: Option<SfuCaps>, b: Option<SfuCaps>) -> bool {
    let norm = |c: Option<SfuCaps>| {
        c.map(|c| SfuCaps {
            rtt_ms: c.rtt_ms.map(|r| r / sfu::RTT_BUCKET_MS),
            ..c
        })
    };
    norm(a) == norm(b)
}

impl Call {
    fn new(id: CallId) -> Self {
        Call {
            id,
            parts: BTreeMap::new(),
            me_in: false,
            joining: false,
            key: None,
            max_gen: 0,
            issued: 0,
            seen: BTreeMap::new(),
            pending: BTreeMap::new(),
            viewers: BTreeMap::new(),
            caps: BTreeMap::new(),
            hosts: BTreeMap::new(),
            plan: Plan::default(),
            plan_seen: BTreeMap::new(),
            plan_pending: BTreeMap::new(),
            plan_issued: 0,
            announced: false,
            announced_caps: None,
            anon: BTreeMap::new(),
            direct: BTreeSet::new(),
        }
    }

    /// 참가자에게 익명 번호를 줍니다(이미 있으면 그대로).
    fn number(&mut self, device: DeviceId) {
        let next = u32::try_from(self.anon.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        self.anon.entry(device).or_insert(next);
    }

    fn manager(&self) -> Option<DeviceId> {
        self.parts.keys().next().copied()
    }

    fn generation(&self) -> u64 {
        self.key.as_ref().map_or(0, |k| k.generation)
    }

    /// 참가자가 알린 세대를 기억합니다(상한을 넘는 값은 무시).
    fn note_claim(&mut self, generation: u64) {
        if generation <= CLAIM_LIMIT {
            self.max_gen = self.max_gen.max(generation);
        }
    }

    /// 관리 기기가 만들 다음 세대: 내가 만든 것, 알려진 세대, 지금 키 세대(상한 이하만) 중 가장 큰 것 + 1.
    fn next_generation(&self) -> u64 {
        let inherited = self.generation().min(CLAIM_LIMIT);
        self.issued
            .max(self.max_gen)
            .max(inherited)
            .saturating_add(1)
    }

    /// 관리 기기가 보낸 키를 받아들일지: 그 기기에게서 받은 것보다 큰 세대만.
    fn accepts_from(&self, issuer: &DeviceId, generation: u64) -> bool {
        generation > self.seen.get(issuer).copied().unwrap_or(0)
    }

    fn install(&mut self, key: MediaKey) {
        self.seen.insert(key.issuer, key.generation);
        self.pending.remove(&key.issuer);
        self.key = Some(key);
    }

    /// 참가자 목록이 바뀐 뒤: 새 관리 기기가 미리 보낸 키가 있으면 씁니다. 바꿨으면 `true`.
    fn adopt_pending(&mut self, my_device: &DeviceId) -> bool {
        let Some(manager) = self.manager() else {
            return false;
        };
        if !self.me_in || manager == *my_device {
            return false;
        }
        match self.pending.remove(&manager) {
            Some(k) if self.accepts_from(&manager, k.generation) => {
                self.install(k);
                true
            }
            _ => false,
        }
    }

    fn remove(&mut self, device: &DeviceId) -> bool {
        self.viewers.remove(device);
        for v in self.viewers.values_mut() {
            v.remove(device);
        }
        self.seen.remove(device);
        self.pending.remove(device);
        self.caps.remove(device);
        self.plan_seen.remove(device);
        self.plan_pending.remove(device);
        let removed = self.parts.remove(device).is_some();
        if self.parts.is_empty() {
            self.anon.clear();
        }
        removed
    }

    /// 호스트를 맡을 수 있는 참가자와 호스트 노드.
    fn candidates(&self) -> Vec<(DeviceId, SfuCaps)> {
        let mut v: Vec<(DeviceId, SfuCaps)> = self
            .parts
            .keys()
            .filter_map(|d| self.caps.get(d).map(|c| (*d, *c)))
            .collect();
        v.extend(
            self.hosts
                .iter()
                .filter(|(d, _)| !self.parts.contains_key(*d))
                .filter_map(|(d, h)| h.caps.map(|c| (*d, c))),
        );
        v
    }

    /// 결정을 받습니다. 자기 목록의 관리 기기가 보낸, 그 기기에게서 받은 것보다 큰 번호만 쓰고, 다른 참가자의 결정은
    /// 따로 둡니다. 따르는 호스트가 바뀌었으면 `true`.
    fn accept_plan(&mut self, from: DeviceId, plan: Plan, my_device: &DeviceId) -> bool {
        if self.manager() == Some(from) && from != *my_device {
            if plan.seq <= self.plan_seen.get(&from).copied().unwrap_or(0) {
                return false;
            }
            self.plan_seen.insert(from, plan.seq);
            self.plan_pending.remove(&from);
            self.follow(plan)
        } else {
            if self
                .plan_pending
                .get(&from)
                .is_none_or(|p| p.seq < plan.seq)
            {
                self.plan_pending.insert(from, plan);
            }
            false
        }
    }

    /// 참가자 목록이 바뀐 뒤: 새 관리 기기가 미리 보낸 결정이 있으면 따릅니다. 호스트가 바뀌었으면 `true`.
    fn adopt_plan_pending(&mut self, my_device: &DeviceId) -> bool {
        let Some(manager) = self.manager().filter(|m| m != my_device) else {
            return false;
        };
        match self.plan_pending.remove(&manager) {
            Some(p) if p.seq > self.plan_seen.get(&manager).copied().unwrap_or(0) => {
                self.plan_seen.insert(manager, p.seq);
                self.follow(p)
            }
            _ => false,
        }
    }

    fn follow(&mut self, plan: Plan) -> bool {
        if let Some(h) = plan.host
            && !self.parts.contains_key(&h)
            && self.hosts.len() < MAX_HOSTS
        {
            // 아직 `SfuHost`를 받지 못한 호스트 노드: 결정을 믿고 노드는 저장된 기록으로 찾습니다.
            self.hosts.entry(h).or_insert(HostOnly {
                peer: None,
                user: None,
                owner: None,
                caps: None,
            });
        }
        let changed = self.plan.host != plan.host;
        self.plan = plan;
        changed
    }

    /// 따르는 결정이 없을 때 쓸 호스트 실마리(막 관리 기기가 된 새 참가자): 다른 참가자가 알려 준 가장 새 결정.
    fn plan_hint(&self) -> Option<DeviceId> {
        self.plan_pending
            .values()
            .filter(|p| p.host.is_some())
            .max_by_key(|p| p.seq)
            .and_then(|p| p.host)
    }

    /// 호스트가 아직 있는지(참가자이거나 알려진 호스트 노드).
    fn host_present(&self, h: &DeviceId) -> bool {
        self.parts.contains_key(h) || self.hosts.contains_key(h)
    }

    /// 참가에서 빠질 때 결정 상태를 비웁니다(보기만 하는 동안은 따르지 않음).
    fn reset_plan(&mut self, my_device: &DeviceId) {
        self.plan = Plan::default();
        self.plan_seen.clear();
        self.plan_pending.clear();
        self.caps.remove(my_device);
    }
}

#[derive(Clone)]
struct Part {
    user: UserId,
    /// 이 기기의 노드(내 기기면 `None`).
    peer: Option<PeerId>,
    muted: bool,
    deafened: bool,
    screen: bool,
    /// 그 기기가 `Join`에 실은 참가 번호(0은 모름).
    session: u64,
}

struct MediaKey {
    generation: u64,
    /// 키를 만든(봉인해 보낸) 관리 기기.
    issuer: DeviceId,
    key: Zeroizing<[u8; 32]>,
}

fn is_voice(state: &SpaceState, channel: &ChannelId) -> bool {
    state
        .channels
        .get(channel)
        .is_some_and(|c| c.kind == ChannelKind::Voice)
}

fn can_connect(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    state.is_member(user)
        && state
            .channel_permissions(user, channel)
            .contains(Permissions(
                Permissions::VIEW_CHANNEL.0 | Permissions::CONNECT_VOICE.0,
            ))
}

fn can_view(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    state.is_member(user)
        && state
            .channel_permissions(user, channel)
            .contains(Permissions::VIEW_CHANNEL)
}

fn has(state: &SpaceState, user: &UserId, channel: &ChannelId, p: Permissions) -> bool {
    state.channel_permissions(user, channel).contains(p)
}

/// 이 기기가 지금 통화에 있어도 되는지(멤버의 활성 기기이고 채널을 보고 접속할 권한이 있음).
fn allowed(state: &SpaceState, user: &UserId, device: &DeviceId, channel: &ChannelId) -> bool {
    is_voice(state, channel)
        && state.device_belongs(user, device)
        && can_connect(state, user, channel)
}

/// 참가하지 않고 그 통화의 SFU 호스트만 맡을 수 있는지: 접속 권한에 더해 `manage_voice`(관리자가 준 권한)가
/// 있어야 합니다. 호스트는 참가자의 IP·말하는 때·트래픽을 알게 되므로, 아무 멤버나 통화 밖에서 그 자리를 차지해
/// 엿보지 못하게 합니다(참가자는 메시에서도 같은 것을 알므로 따로 묻지 않음).
fn may_host_unjoined(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    can_connect(state, user, channel) && has(state, user, channel, Permissions::MANAGE_VOICE)
}

/// 이 통화를 Tor로 나르는지(`Topology::Tor`): Tor로 도는(`tor::tor_active`) 기기의 P2P 전용이 아닌 채널. WebRTC 직접
/// 연결은 실제 IP를 드러내므로 이 통화에서는 서로 신뢰한 참가자끼리만 씁니다([`refresh_direct`]). 직접 연결 주
/// 스웜(시더·시험 노드)은 그대로 WebRTC입니다.
pub(crate) fn tor_call(node: &Node, state: &SpaceState, channel: &ChannelId) -> bool {
    tor::tor_active(node) && !state.is_p2p(channel)
}

/// Tor 통화에서 이 기기와 WebRTC 신호를 주고받아도 되는지: 내가 그 사람을 신뢰함(내 IP를 보여도 된다는 동의). 앱은 서로
/// 신뢰할 때만 잇지만([`refresh_direct`]), 상대의 신뢰 안내가 이쪽에 늦게 닿는 동안 먼저 온 제안도 받게 내 동의만 봅니다.
/// 익명 채널은 늘 Tor입니다(직접 연결은 가명 뒤의 사람이 내가 신뢰한 사람임을 알려 주고 IP를 줌).
fn signal_ok(node: &Node, state: &SpaceState, channel: &ChannelId, user: &UserId) -> bool {
    !state.is_anonymous(channel) && crate::trust::trusts(node, user)
}

/// Tor 통화에서 WebRTC로 바로 잇는 참가자(서로 신뢰, 익명 채널 아님)를 다시 셉니다. 바뀌었으면 참입니다. 신뢰는 통화
/// 잠금 밖에서 봅니다(신뢰 목록·공간 상태 잠금을 통화 잠금 안에서 잡지 않게).
fn refresh_direct(node: &Node, space: SpaceId, channel: ChannelId) -> bool {
    let tor = node
        .space_state(&space)
        .filter(|st| tor_call(node, st, &channel) && !st.is_anonymous(&channel));
    let parts: Vec<(DeviceId, UserId)> = match node.voice.calls.lock().get(&(space, channel)) {
        Some(c) if tor.is_some() => c.parts.iter().map(|(d, p)| (*d, p.user)).collect(),
        _ => Vec::new(),
    };
    let direct: BTreeSet<DeviceId> = parts
        .into_iter()
        .filter(|(d, u)| crate::p2p::mutual(node, u, d))
        .map(|(d, _)| d)
        .collect();
    let mut calls = node.voice.calls.lock();
    let Some(call) = calls.get_mut(&(space, channel)) else {
        return false;
    };
    if call.direct == direct {
        return false;
    }
    call.direct = direct;
    true
}

/// 신뢰나 상대의 신뢰 안내가 바뀐 뒤(`p2p::reconcile`): 직접 잇는 상대가 바뀐 통화마다 참가자 알림을 다시 보냅니다.
pub(crate) fn refresh_direct_all(node: &Arc<Node>) {
    let joined: Vec<(SpaceId, ChannelId)> = node
        .voice
        .calls
        .lock()
        .iter()
        .filter(|(_, c)| c.me_in)
        .map(|(k, _)| *k)
        .collect();
    for (space, channel) in joined {
        if refresh_direct(node, space, channel) {
            notify_roster(node, space, channel);
        }
    }
}

/// 이 채널에서 지금 통화하면 안 되는 이유. P2P 전용 채널은 참가해야 합니다(참가하면 Tor로 도는 기기도 직접 연결).
/// 그 밖의 채널은 Tor로 도는 기기면 Tor 통화로 합니다.
fn voice_block(
    node: &Node,
    state: &SpaceState,
    space: &SpaceId,
    channel: &ChannelId,
) -> Option<Error> {
    if state.is_p2p(channel) {
        return (!crate::p2p::p2p_joined(node, space, channel)).then(|| {
            Error::forbidden(
                "P2P 전용 채널에 먼저 참가해야 통화할 수 있습니다(참가하면 참여자에게 IP가 보입니다)",
            )
        });
    }
    None
}

/// 프레임 키 봉인의 연관 데이터: 어느 통화의 몇 세대를 누가 만들었는지.
pub(crate) fn media_ad(
    space: &SpaceId,
    channel: &ChannelId,
    generation: u64,
    from: &DeviceId,
) -> Vec<u8> {
    let mut ad = b"yangpa/v1/media-key".to_vec();
    ad.extend_from_slice(space.as_bytes());
    ad.extend_from_slice(channel.as_bytes());
    ad.extend_from_slice(&generation.to_le_bytes());
    ad.extend_from_slice(from.as_bytes());
    ad
}

/// 앱에 알리는 토폴로지와 SFU 호스트(기기, 그 사용자). SFU인데 호스트가 빠졌으면(넘겨받는 중) 호스트 없이 SFU입니다.
/// Tor 통화면 다른 기기의 결정과 상관없이 `Tor`입니다(앱이 SFU·메시 WebRTC로 붙지 않게).
fn topology(call: &Call, tor: bool) -> (Topology, Option<DeviceId>, Option<UserId>) {
    if tor {
        return (Topology::Tor, None, None);
    }
    match call.plan.host {
        Some(h) if call.host_present(&h) => {
            let user = call
                .parts
                .get(&h)
                .map(|p| p.user)
                .or_else(|| call.hosts.get(&h).and_then(|x| x.user.or(x.owner)));
            (Topology::Sfu, Some(h), user)
        }
        Some(_) => (Topology::Sfu, None, None),
        None => (Topology::Mesh, None, None),
    }
}

/// 앱에 알리는 토폴로지. 익명 채널이면 SFU 호스트의 사용자를 알리지 않습니다.
fn shown_topology(
    call: &Call,
    anonymous: bool,
    tor: bool,
) -> (Topology, Option<DeviceId>, Option<UserId>) {
    let (t, host, user) = topology(call, tor);
    (t, host, user.filter(|_| !anonymous))
}

/// 앱에 보여 줄 참가자. 익명 채널이면 사용자를 가리고 번호를 붙입니다(통화 연결에 쓰는 기기 ID는 그대로).
fn view_of(
    (space, channel): (SpaceId, ChannelId),
    call: &Call,
    anonymous: bool,
    my_device: Option<DeviceId>,
) -> Vec<VoiceParticipant> {
    call.parts
        .iter()
        .map(|(d, p)| VoiceParticipant {
            user: if anonymous { ANONYMOUS_USER } else { p.user },
            device: *d,
            anon: anonymous.then(|| call.anon.get(d).copied().unwrap_or(0)),
            anon_name: anonymous.then(|| crate::anon::name(&space, &channel, &p.user)),
            mine: Some(*d) == my_device,
            muted: p.muted,
            deafened: p.deafened,
            screen: p.screen,
            viewers: call
                .viewers
                .get(d)
                .map(|v| v.iter().copied().collect())
                .unwrap_or_default(),
            direct: call.direct.contains(d),
        })
        .collect()
}

fn encode_key(k: &MediaKey) -> SecretString {
    SecretString::new(base64::engine::general_purpose::STANDARD.encode(&k.key[..]))
}

/// 참가자 목록 알림. 참가 중이면 지금 세대 키를, 아니면 세대 0과 빈 키를 실어 보냅니다. 맡은 SFU 통화방도 맞춥니다.
fn notify_roster(node: &Arc<Node>, space: SpaceId, channel: ChannelId) {
    refresh_direct(node, space, channel);
    let anonymous = crate::messages::channel_anonymous(node, &space, &channel);
    let my_device = node.me().ok().map(|m| m.device_id());
    let tor = node
        .space_state(&space)
        .is_some_and(|st| tor_call(node, &st, &channel));
    let n = {
        let calls = node.voice.calls.lock();
        let (participants, generation, media_key, (topology, sfu_host, sfu_host_user)) =
            match calls.get(&(space, channel)) {
                Some(call) => {
                    let (generation, key) = match (&call.key, call.me_in) {
                        (Some(k), true) => (k.generation, encode_key(k)),
                        _ => (0, SecretString::default()),
                    };
                    (
                        view_of((space, channel), call, anonymous, my_device),
                        generation,
                        key,
                        shown_topology(call, anonymous, tor),
                    )
                }
                None => (
                    Vec::new(),
                    0,
                    SecretString::default(),
                    (if tor { Topology::Tor } else { Topology::Mesh }, None, None),
                ),
            };
        Notification::VoiceParticipants {
            space,
            channel,
            participants,
            generation,
            media_key,
            topology,
            sfu_host,
            sfu_host_user,
        }
    };
    node.notify.publish(n);
    sync_sfu(node, space, channel);
}

/// 결정이 이 기기를 호스트로 정했으면 SFU 통화방을 열고(들어올 수 있는 기기 = 참가자), 아니면 닫습니다.
fn sync_sfu(node: &Arc<Node>, space: SpaceId, channel: ChannelId) {
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let allowed = {
        let calls = node.voice.calls.lock();
        calls.get(&(space, channel)).and_then(|c| {
            let hosting = c.plan.host == Some(my_device)
                && (c.me_in || (c.announced && sfu::prefers_hosting(node)));
            hosting.then(|| c.parts.keys().copied().collect::<BTreeSet<DeviceId>>())
        })
    };
    if allowed.is_some() || sfu::is_hosting(node, &(space, channel)) {
        sfu::sync_room(node, (space, channel), allowed);
    }
}

pub(crate) fn start(node: &Arc<Node>) {
    node.tasks.spawn(keepalive(Arc::downgrade(node)));
    node.tasks
        .spawn(watch_state(Arc::downgrade(node), node.notify.subscribe()));
}

/// 아는 통화(참가 중이든 보기만 하든)의 참가자 노드에 주기적으로 요청을 보내 연결이 쉬어서 닫히지 않게 합니다
/// (닫히면 참가자에서 빠짐). 상태 알림을 놓쳤을 때를 위해 참가 자격도 다시 봅니다.
async fn keepalive(weak: Weak<Node>) {
    loop {
        tokio::time::sleep(KEEPALIVE).await;
        let Some(node) = weak.upgrade() else { return };
        recheck(&node).await;
        let targets: BTreeSet<PeerId> = node
            .voice
            .calls
            .lock()
            .values()
            .flat_map(|c| c.parts.values().filter_map(|p| p.peer))
            .collect();
        let pings = targets
            .into_iter()
            .map(|p| node.net.request(p, YpRequest::Ping));
        join_all(pings).await;
    }
}

/// `keepalive`마다 하는 다시 보기: 참가 자격, 내 SFU 호스트 조건, 관리 기기면 지금 토폴로지 결정 다시 보내기.
pub(crate) async fn recheck(node: &Arc<Node>) {
    sweep(node, None);
    refresh_caps(node).await;
    resend_plans(node);
}

/// 공간 상태가 바뀔 때마다(`space.updated`) 그 공간의 통화 참가 자격을 다시 봅니다. 알림을 놓치면 전부 봅니다.
async fn watch_state(weak: Weak<Node>, mut rx: broadcast::Receiver<Notification>) {
    loop {
        let only = match rx.recv().await {
            Ok(Notification::SpaceUpdated { space }) => Some(space),
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => None,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let Some(node) = weak.upgrade() else { return };
        sweep(&node, only);
    }
}

/// 통화 하나에서 자격 검사로 바뀐 것.
struct Swept {
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    /// 검사 전에 내가 참가 중이었는지.
    me_in: bool,
    /// 뺀 다른 기기.
    gone: Vec<DeviceId>,
    /// 내가 자격을 잃었음(퇴장 알림을 보냄).
    i_leave: bool,
    /// 자격을 잃은 호스트 노드를 뺐음.
    hosts_gone: bool,
}

/// 추방·기기 폐기·권한 변경·채널 삭제로 자격을 잃은 참가자를 빼고(내가 잃었으면 나옴), 남은 참가자 키를 바꿉니다.
/// 채널을 더는 볼 수 없거나 공간이 없어졌으면 그 통화를 통째로 잊습니다.
fn sweep(node: &Arc<Node>, only: Option<SpaceId>) {
    let Ok(me) = node.me() else { return };
    let (my_user, my_device) = (me.user(), me.device_id());
    let keys: Vec<(SpaceId, ChannelId)> = node
        .voice
        .calls
        .lock()
        .keys()
        .filter(|(s, _)| only.is_none_or(|o| o == *s))
        .copied()
        .collect();
    if keys.is_empty() {
        return;
    }
    // 공간 잠금을 통화 잠금 안에서 잡지 않게 상태를 먼저 읽습니다.
    let states: HashMap<SpaceId, Option<Arc<SpaceState>>> = keys
        .iter()
        .map(|(s, _)| (*s, node.space_state(s)))
        .collect();
    let mut changes = Vec::new();
    {
        let mut calls = node.voice.calls.lock();
        for key @ (space, channel) in keys {
            let state = states.get(&space).cloned().flatten();
            let Some(call) = calls.get_mut(&key) else {
                continue;
            };
            let visible = state
                .as_ref()
                .is_some_and(|st| is_voice(st, &channel) && can_view(st, &my_user, &channel));
            let Some(st) = state.filter(|_| visible) else {
                // 볼 수 없는 통화는 들고 있지 않습니다(내 자격도 없으므로 상대들이 나를 뺌).
                let gone: Vec<DeviceId> = call
                    .parts
                    .keys()
                    .filter(|d| **d != my_device)
                    .copied()
                    .collect();
                changes.push(Swept {
                    space,
                    channel,
                    call: call.id,
                    me_in: call.me_in,
                    gone,
                    i_leave: false,
                    hosts_gone: false,
                });
                calls.remove(&key);
                continue;
            };
            let gone: Vec<DeviceId> = call
                .parts
                .iter()
                .filter(|(d, p)| **d != my_device && !allowed(&st, &p.user, d, &channel))
                .map(|(d, _)| *d)
                .collect();
            let i_leave = call.me_in && !allowed(&st, &my_user, &my_device, &channel);
            let before = call.hosts.len();
            call.hosts.retain(|d, h| {
                h.user.is_none_or(|u| {
                    st.device_belongs(&u, d) && may_host_unjoined(&st, &u, &channel)
                })
            });
            let hosts_gone = call.hosts.len() != before;
            if gone.is_empty() && !i_leave && !hosts_gone {
                continue;
            }
            for d in &gone {
                call.remove(d);
            }
            if !i_leave {
                call.adopt_pending(&my_device);
                call.adopt_plan_pending(&my_device);
            }
            changes.push(Swept {
                space,
                channel,
                call: call.id,
                me_in: call.me_in,
                gone,
                i_leave,
                hosts_gone,
            });
            if call.parts.is_empty() {
                calls.remove(&key);
            }
        }
    }
    for c in changes {
        debug!(
            removed = c.gone.len(),
            left = c.i_leave,
            "통화 참가 자격 정리"
        );
        // 참가 중 키를 기다리던 `voice.join`이 바뀐 통화를 다시 보게 합니다.
        node.voice.key_changed.notify_waiters();
        notify_roster(node, c.space, c.channel);
        if c.me_in {
            for from in &c.gone {
                node.notify.publish(Notification::VoiceSignal {
                    space: c.space,
                    channel: c.channel,
                    call: c.call,
                    from: *from,
                    payload: SignalPayload::Leave,
                });
            }
        }
        if c.i_leave {
            let (space, channel) = (c.space, c.channel);
            node.spawn_detached(move |node| async move {
                let _ = leave(&node, space, channel).await;
            });
        } else if c.me_in {
            if !c.gone.is_empty() {
                maybe_rekey(node, c.space, c.channel);
            }
            if !c.gone.is_empty() || c.hosts_gone {
                maybe_replan(node, c.space, c.channel);
            }
        }
    }
}

/// 채널 보기 채우기(통화 채널의 지금 참가자). `volatile::fill_channel_view` 다음에 불립니다.
pub(crate) fn fill_channel_view(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    view: &mut ChannelView,
) {
    if !is_voice(state, &view.channel) {
        return;
    }
    let calls = node.voice.calls.lock();
    let call = calls.get(&(*space, view.channel));
    let mut users: Vec<UserId> = Vec::new();
    for p in call.iter().flat_map(|c| c.parts.values()) {
        if !users.contains(&p.user) {
            users.push(p.user);
        }
    }
    // 익명 채널은 몇 명인지만 보여 줍니다.
    if state.is_anonymous(&view.channel) {
        users.fill(ANONYMOUS_USER);
    }
    view.participants = Some(users);
    view.joined = Some(call.is_some_and(|c| c.me_in));
}

// ───────────── 보내기 ─────────────

/// 기기 하나에 신호를 보내고 응답을 받습니다(인증 후). 노드를 모르면 저장된 주소 기록으로 찾습니다.
async fn request_signal(
    node: &Arc<Node>,
    device: DeviceId,
    hint: Option<PeerId>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    payload: SignalPayload,
) -> Result<YpResponse> {
    let me = node.me()?;
    let peer = hint
        .filter(|p| node.peers.identity_of(p).is_some_and(|(_, d)| d == device))
        .or_else(|| peers::peer_for_device(node, &device))
        .ok_or_else(|| Error::Network("기기의 노드를 모름".into()))?;
    let req = YpRequest::Signal {
        space,
        channel,
        call,
        from: me.device_id(),
        payload,
    };
    if let Some(r) = crate::p2p::request_call_signal(node, &space, &channel, &device, &req).await {
        return Ok(r);
    }
    peers::ensure_auth(node, peer).await?;
    Ok(node.net.request(peer, req).await?)
}

/// 기기 하나에 신호를 보냅니다(거절도 오류).
async fn send_to(
    node: &Arc<Node>,
    device: DeviceId,
    hint: Option<PeerId>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    payload: SignalPayload,
) -> Result<()> {
    match request_signal(node, device, hint, space, channel, call, payload).await? {
        YpResponse::Ok => Ok(()),
        YpResponse::Error { code, .. } => {
            Err(Error::Network(format!("상대가 신호를 거절함: {code:?}")))
        }
        _ => Err(Error::Network("예상하지 못한 응답".into())),
    }
}

/// 채널을 볼 수 있는 연결된 기기와 지금 참가자 모두에게 보냅니다(끝날 때까지 기다림, 실패는 무시).
async fn broadcast(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    payload: SignalPayload,
) {
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let mut targets: BTreeMap<DeviceId, Option<PeerId>> = BTreeMap::new();
    for (peer, user, device) in node.peers.authed_peers() {
        if device != my_device
            && state.device_belongs(&user, &device)
            && can_view(&state, &user, &channel)
        {
            targets.insert(device, Some(peer));
        }
    }
    if let Some(c) = node.voice.calls.lock().get(&(space, channel)) {
        for (d, p) in &c.parts {
            if *d != my_device {
                targets.entry(*d).or_insert(p.peer);
            }
        }
    }
    let sends = targets.into_iter().map(|(device, hint)| {
        let payload = payload.clone();
        async move {
            if let Err(e) = send_to(node, device, hint, space, channel, call, payload).await {
                debug!(device = %device.short(), error = %e, "통화 신호 전달 실패");
            }
        }
    });
    join_all(sends).await;
}

/// 키를 받을 다른 참가자(지금 상태에서 통화 자격이 있는 기기만)와 그 상자 키.
fn key_recipients(
    node: &Node,
    state: &SpaceState,
    call: &Call,
    channel: &ChannelId,
    my_device: &DeviceId,
) -> Vec<(DeviceId, Option<PeerId>, BoxPublicKey)> {
    // 상자 키는 참가자 수만큼만 기기 키 표에서 찾습니다(상태 기록의 키 약속으로).
    call.parts
        .iter()
        .filter(|(d, p)| *d != my_device && allowed(state, &p.user, d, channel))
        .filter_map(|(d, p)| {
            crate::devkeys::device_box_key(node, state, &p.user, d).map(|k| (*d, p.peer, k))
        })
        .collect()
}

/// 보낼 토폴로지 결정.
struct PlanSend {
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    plan: Plan,
    /// 고른 호스트(먼저 보냄, 참가자가 제안을 보낼 때 통화방을 열어 두게).
    host: Option<(DeviceId, Option<PeerId>)>,
    others: Vec<(DeviceId, Option<PeerId>)>,
}

/// 내가 키 관리 기기면 토폴로지를 다시 정합니다. 호스트가 바뀌었으면 상태를 바꾸고 보낼 결정을 돌려줍니다.
fn replan(node: &Node, space: SpaceId, channel: ChannelId) -> Option<PlanSend> {
    let my_device = node.me().ok()?.device_id();
    let t = Thresholds::of(node);
    let mut calls = node.voice.calls.lock();
    let call = calls.get_mut(&(space, channel))?;
    if !call.me_in || call.joining || call.manager() != Some(my_device) {
        return None;
    }
    let current = call
        .plan
        .host
        .or_else(|| call.plan_hint())
        .filter(|_| call.plan.host.is_some() || call.plan.seq == 0);
    // Tor 통화에는 SFU를 고르지 않습니다(호스트에 WebRTC로 붙으면 IP가 드러남).
    let tor = node
        .space_state(&space)
        .is_some_and(|st| tor_call(node, &st, &channel));
    let host = if tor {
        None
    } else {
        decide(call.parts.len(), current, &call.candidates(), t)
    };
    if host == call.plan.host {
        return None;
    }
    let seq = call.plan_issued.max(call.plan.seq).saturating_add(1);
    if seq > GEN_LIMIT {
        return None;
    }
    call.plan_issued = seq;
    let plan = Plan { seq, host };
    let handover = call.plan.host.is_some() && host.is_some();
    call.follow(plan);
    debug!(
        sfu = host.is_some(),
        seq,
        handover,
        host = %host.map(|h| h.short()).unwrap_or_default(),
        "통화 토폴로지 결정"
    );
    let mut targets: Vec<(DeviceId, Option<PeerId>)> = call
        .parts
        .iter()
        .filter(|(d, _)| **d != my_device)
        .map(|(d, p)| (*d, p.peer))
        .collect();
    targets.extend(
        call.hosts
            .iter()
            .filter(|(d, _)| !call.parts.contains_key(*d))
            .map(|(d, h)| (*d, h.peer)),
    );
    let host_target = host
        .and_then(|h| targets.iter().position(|(d, _)| *d == h))
        .map(|i| targets.remove(i));
    Some(PlanSend {
        space,
        channel,
        call: call.id,
        plan,
        host: host_target,
        others: targets,
    })
}

/// 결정을 보냅니다(호스트 먼저, `skip`은 이미 보낸 기기). 못 보내면(일시적인 스트림 오류, 상대가 아직 나를
/// 참가자로 모름) 잠깐씩 물러났다 다시 보냅니다. 결정을 놓친 호스트는 통화방을 열지 않아 모두가 거절당하므로.
fn spawn_plan(node: &Arc<Node>, s: PlanSend, skip: Option<DeviceId>) {
    node.spawn_detached(move |node| async move {
        let (space, channel, call, plan) = (s.space, s.channel, s.call, s.plan);
        if let Some((d, hint)) = s.host.filter(|(d, _)| Some(*d) != skip) {
            send_plan(&node, d, hint, space, channel, call, plan).await;
        }
        let sends = s
            .others
            .into_iter()
            .filter(|(d, _)| Some(*d) != skip)
            .map(|(d, hint)| send_plan(&node, d, hint, space, channel, call, plan));
        join_all(sends).await;
    });
}

/// 결정 하나를 기기 하나에 보냅니다. 실패하면 `ANNOUNCE_RETRY` 간격으로 다시 보내되, 그사이 결정이 바뀌었거나 내가
/// 관리 기기가 아니게 됐으면 그만둡니다.
async fn send_plan(
    node: &Arc<Node>,
    device: DeviceId,
    hint: Option<PeerId>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    plan: Plan,
) {
    let mut waits = ANNOUNCE_RETRY.iter();
    loop {
        let payload = SignalPayload::SfuPlan {
            seq: plan.seq,
            host: plan.host,
        };
        let Err(e) = send_to(node, device, hint, space, channel, call, payload).await else {
            return;
        };
        let Some(wait) = waits.next() else {
            debug!(device = %device.short(), error = %e, "토폴로지 결정 전달 실패");
            return;
        };
        tokio::time::sleep(*wait).await;
        if !plan_current(node, space, channel, plan.seq) {
            return;
        }
    }
}

/// 내가 관리 기기로서 낸 결정 `seq`가 아직 지금 결정인지.
fn plan_current(node: &Node, space: SpaceId, channel: ChannelId, seq: u64) -> bool {
    let Ok(me) = node.me() else { return false };
    let calls = node.voice.calls.lock();
    calls
        .get(&(space, channel))
        .is_some_and(|c| c.me_in && c.manager() == Some(me.device_id()) && c.plan.seq == seq)
}

/// 관리 기기로서 낸 지금 결정을 참가자와 호스트 후보에게 한 번 더 보냅니다(`keepalive`마다). 받는 쪽은 이미 받은
/// 번호면 무시하므로, 결정을 놓친 기기만 따라잡습니다.
pub(crate) fn resend_plans(node: &Arc<Node>) {
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let sends: Vec<PlanSend> = {
        let calls = node.voice.calls.lock();
        calls
            .iter()
            .filter(|(_, c)| {
                c.me_in && !c.joining && c.manager() == Some(my_device) && c.plan.seq > 0
            })
            .map(|(&(space, channel), c)| {
                let mut others: Vec<(DeviceId, Option<PeerId>)> = c
                    .parts
                    .iter()
                    .filter(|(d, _)| **d != my_device)
                    .map(|(d, p)| (*d, p.peer))
                    .collect();
                others.extend(
                    c.hosts
                        .iter()
                        .filter(|(d, _)| !c.parts.contains_key(*d))
                        .map(|(d, h)| (*d, h.peer)),
                );
                PlanSend {
                    space,
                    channel,
                    call: c.id,
                    plan: c.plan,
                    host: None,
                    others,
                }
            })
            .collect()
    };
    for s in sends {
        node.spawn_detached(move |node| async move {
            let payload = SignalPayload::SfuPlan {
                seq: s.plan.seq,
                host: s.plan.host,
            };
            let sends = s.others.into_iter().map(|(d, hint)| {
                let (node, payload) = (node.clone(), payload.clone());
                async move {
                    let _ = send_to(&node, d, hint, s.space, s.channel, s.call, payload).await;
                }
            });
            join_all(sends).await;
        });
    }
}

/// 내가 키 관리 기기면 토폴로지를 다시 정하고, 바뀌었으면 알립니다.
fn maybe_replan(node: &Arc<Node>, space: SpaceId, channel: ChannelId) {
    if let Some(s) = replan(node, space, channel) {
        notify_roster(node, space, channel);
        spawn_plan(node, s, None);
    }
}

/// 이 노드의 SFU 호스트 조건을 다시 재고, 바뀌었으면 참가 중인 통화에는 `State`로, 참가하지 않은 통화에는
/// `SfuHost`로 알립니다(Tor로 도는 노드는 늘 `None`). 시더는 나중에 `manage_voice`를 받은
/// 통화에도 여기서 나섭니다(참가 알림만 기다리면 이미 있던 참가자는 끝까지 모름).
async fn refresh_caps(node: &Arc<Node>) {
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let prefer = sfu::prefers_hosting(node);
    type Known = ((SpaceId, ChannelId), bool, BTreeSet<PeerId>);
    let known: Vec<Known> = node
        .voice
        .calls
        .lock()
        .iter()
        .filter(|(_, c)| c.me_in || c.announced || (prefer && !c.parts.is_empty()))
        .map(|(k, c)| {
            (
                *k,
                c.me_in,
                c.parts.values().filter_map(|p| p.peer).collect(),
            )
        })
        .collect();
    for ((space, channel), me_in, peers) in known {
        let mut caps = sfu::caps(node, (space, channel), &peers).await;
        if !me_in
            && !node
                .space_state(&space)
                .is_some_and(|st| may_host_unjoined(&st, &me.user(), &channel))
        {
            // 통화 밖 호스트 자격(`manage_voice`)을 잃었으면 후보에서 물러납니다.
            caps = None;
        }
        if me_in {
            let update = {
                let mut calls = node.voice.calls.lock();
                let Some(call) = calls.get_mut(&(space, channel)).filter(|c| c.me_in) else {
                    continue;
                };
                if same_caps(call.caps.get(&my_device).copied(), caps) {
                    continue;
                }
                match caps {
                    Some(c) => call.caps.insert(my_device, c),
                    None => call.caps.remove(&my_device),
                };
                let Some(p) = call.parts.get(&my_device) else {
                    continue;
                };
                let payload = SignalPayload::State {
                    muted: p.muted,
                    deafened: p.deafened,
                    screen: p.screen,
                    sfu: caps,
                };
                (call.id, payload)
            };
            maybe_replan(node, space, channel);
            let (call_id, payload) = update;
            node.spawn_detached(move |node| async move {
                broadcast(&node, space, channel, call_id, payload).await;
            });
        } else {
            // 참가하지 않은 통화: 새로 맡을 수 있게 됐으면 나서고, 맡을 수 없게 됐으면 물러나고, 조건(남은 수용
            // 인원 등)이 바뀌었으면 다시 알립니다.
            let (call_id, targets) = {
                let mut calls = node.voice.calls.lock();
                let Some(call) = calls.get_mut(&(space, channel)).filter(|c| !c.me_in) else {
                    continue;
                };
                if same_caps(call.announced_caps, caps) {
                    continue;
                }
                call.announced = caps.is_some();
                call.announced_caps = caps;
                let targets: Vec<(DeviceId, Option<PeerId>)> =
                    call.parts.iter().map(|(d, p)| (*d, p.peer)).collect();
                (call.id, targets)
            };
            if caps.is_none() {
                notify_roster(node, space, channel);
            }
            for (d, hint) in targets {
                node.spawn_detached(move |node| async move {
                    let payload = SignalPayload::SfuHost { caps };
                    let _ = send_to(&node, d, hint, space, channel, call_id, payload).await;
                });
            }
        }
    }
}

/// 맡을 수 있는지(소켓을 열지 못함)나 남은 수용 인원(맡은 통화방이 바뀜)이 바뀌었을 수 있음: 조건을 다시 재서
/// 바뀐 통화에 알려 키 관리 기기가 다시 정하게 합니다.
pub(crate) fn caps_changed(node: &Arc<Node>) {
    node.spawn_detached(|node| async move { refresh_caps(&node).await });
}

/// 참가하지 않는 호스트 노드가 새 참가자(`to`)를 보고 호스트 후보로 나섭니다.
fn announce_host(node: &Arc<Node>, space: SpaceId, channel: ChannelId, to: DeviceId, peer: PeerId) {
    node.spawn_detached(move |node| async move {
        let Ok(me) = node.me() else { return };
        if !node
            .space_state(&space)
            .is_some_and(|st| may_host_unjoined(&st, &me.user(), &channel))
        {
            // 관리자가 `manage_voice`를 주지 않은 채널에서는 나서지 않습니다(참가자 쪽도 받지 않음).
            return;
        }
        let Some((call_id, peers)) = node.voice.calls.lock().get(&(space, channel)).map(|c| {
            let peers: BTreeSet<PeerId> = c.parts.values().filter_map(|p| p.peer).collect();
            (c.id, peers)
        }) else {
            return;
        };
        let Some(caps) = sfu::caps(&node, (space, channel), &peers).await else {
            return;
        };
        // 처음 나서거나 조건이 바뀌었으면 모든 참가자에게(키 관리 기기가 먼저 온 참가자일 수 있음), 아니면 새
        // 참가자에게만 알립니다.
        let targets: Vec<(DeviceId, Option<PeerId>)> = {
            let mut calls = node.voice.calls.lock();
            let Some(c) = calls.get_mut(&(space, channel)).filter(|c| !c.me_in) else {
                return;
            };
            let all = !c.announced || !same_caps(c.announced_caps, Some(caps));
            c.announced = true;
            c.announced_caps = Some(caps);
            if all {
                c.parts
                    .iter()
                    .map(|(d, p)| (*d, if *d == to { Some(peer) } else { p.peer }))
                    .collect()
            } else {
                vec![(to, Some(peer))]
            }
        };
        let payload = SignalPayload::SfuHost { caps: Some(caps) };
        let sends = targets.into_iter().map(|(d, hint)| {
            let (node, payload) = (node.clone(), payload.clone());
            async move {
                if let Err(e) = send_to(&node, d, hint, space, channel, call_id, payload).await {
                    debug!(device = %d.short(), error = %e, "SFU 호스트 알림 실패");
                }
            }
        });
        join_all(sends).await;
    });
}

/// 내가 키 관리 기기면 새 세대 키를 만들어 다른 참가자에게 봉인해 보냅니다.
fn maybe_rekey(node: &Arc<Node>, space: SpaceId, channel: ChannelId) {
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let Some(state) = node.space_state(&space) else {
        return;
    };
    // 내가 자격을 잃었으면 키를 만들지 않습니다(곧 통화에서 나옴).
    if !allowed(&state, &me.user(), &my_device, &channel) {
        return;
    }
    let made = {
        let mut calls = node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(space, channel)) else {
            return;
        };
        if !call.me_in || call.joining || call.manager() != Some(my_device) {
            return;
        }
        let generation = call.next_generation();
        if generation > GEN_LIMIT {
            // 2^39번 넘게 키를 바꿔야 닿는 값이라 실제로는 오지 않습니다.
            warn!("통화 키 세대가 상한에 닿아 더 바꾸지 못함");
            return;
        }
        let mut key = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(&mut key[..]);
        call.issued = generation;
        let others = key_recipients(node, &state, call, &channel, &my_device);
        let copy = Zeroizing::new(*key);
        call.key = Some(MediaKey {
            generation,
            issuer: my_device,
            key,
        });
        (generation, copy, others, call.id)
    };
    node.voice.key_changed.notify_waiters();
    notify_roster(node, space, channel);
    let (generation, key, others, call_id) = made;
    if others.is_empty() {
        return;
    }
    debug!(
        generation,
        recipients = others.len(),
        "통화 키 새 세대 배포"
    );
    let ad = media_ad(&space, &channel, generation, &my_device);
    for (device, hint, box_key) in others {
        // 받는 기기마다 따로 봉인합니다. 양자 내성 캡슐문이 수신자마다 1KiB를 넘어, 모두에게 같은 봉투를 보내면
        // 참가자 수의 제곱만큼 보내게 됩니다.
        let payload = SignalPayload::MediaKey {
            generation,
            sealed: yangpa_crypto::seal_to(&[(device, box_key)], &ad, &key[..]),
        };
        node.spawn_detached(move |node| async move {
            if let Err(e) = send_to(&node, device, hint, space, channel, call_id, payload).await {
                debug!(device = %device.short(), error = %e, "통화 키 전달 실패");
            }
        });
    }
}

/// 관리 기기가 지금 키를 참가자 하나에게 다시 보냅니다(다시 연결한 참가자가 키를 잃었을 때). 지금 키가 내가 만든
/// 것이 아니면(막 관리 기기가 됨) 새로 만듭니다.
fn resend_key(node: &Arc<Node>, space: SpaceId, channel: ChannelId, device: DeviceId) {
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let found = {
        let calls = node.voice.calls.lock();
        let Some(call) = calls.get(&(space, channel)) else {
            return;
        };
        if !call.me_in || call.manager() != Some(my_device) {
            return;
        }
        match &call.key {
            Some(k) if k.issuer == my_device => {
                let Some((_, hint, box_key)) =
                    key_recipients(node, &state, call, &channel, &my_device)
                        .into_iter()
                        .find(|(d, _, _)| *d == device)
                else {
                    return;
                };
                Some((k.generation, Zeroizing::new(*k.key), hint, box_key, call.id))
            }
            _ => None,
        }
    };
    let Some((generation, key, hint, box_key, call_id)) = found else {
        maybe_rekey(node, space, channel);
        return;
    };
    let sealed = yangpa_crypto::seal_to(
        &[(device, box_key)],
        &media_ad(&space, &channel, generation, &my_device),
        &key[..],
    );
    let payload = SignalPayload::MediaKey { generation, sealed };
    node.spawn_detached(move |node| async move {
        let _ = send_to(&node, device, hint, space, channel, call_id, payload).await;
    });
}

// ───────────── 로컬 API ─────────────

/// 통화에 참가합니다(이미 참가 중이면 지금 상태). 다른 통화에 있으면 먼저 나옵니다.
pub(crate) async fn join(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
) -> Result<VoiceJoinResult> {
    let me = node.me()?;
    let my_device = me.device_id();
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    if !is_voice(&state, &channel) {
        return Err(Error::not_found("통화 채널이 아님"));
    }
    if !can_connect(&state, &me.user(), &channel) {
        return Err(Error::forbidden("이 통화 채널에 참가할 권한이 없음"));
    }
    if let Some(e) = voice_block(node, &state, &space, &channel) {
        return Err(e);
    }
    let others: Vec<(SpaceId, ChannelId)> = node
        .voice
        .calls
        .lock()
        .iter()
        .filter(|(k, c)| c.me_in && **k != (space, channel))
        .map(|(k, _)| *k)
        .collect();
    for (s, c) in others {
        leave(node, s, c).await?;
    }
    // 호스트 조건(이 통화 참가자 노드와의 연결로 잼).
    let call_peers: BTreeSet<PeerId> = node
        .voice
        .calls
        .lock()
        .get(&(space, channel))
        .map(|c| c.parts.values().filter_map(|p| p.peer).collect())
        .unwrap_or_default();
    let my_caps = sfu::caps(node, (space, channel), &call_peers).await;
    let muted = !has(&state, &me.user(), &channel, Permissions::SPEAK);
    let start = {
        let mut calls = node.voice.calls.lock();
        let call = calls
            .entry((space, channel))
            .or_insert_with(|| Call::new(CallId::random()));
        if call.me_in {
            None
        } else {
            if call.parts.len() >= MAX_PARTICIPANTS {
                return Err(Error::conflict("통화 인원이 가득 참"));
            }
            if call.parts.is_empty() {
                call.id = CallId::random();
                call.key = None;
            }
            call.me_in = true;
            call.joining = true;
            // 0은 '모름'이라 피합니다.
            let session = rand::rngs::OsRng.next_u64().max(1);
            call.parts.insert(
                my_device,
                Part {
                    user: me.user(),
                    peer: None,
                    muted,
                    deafened: false,
                    screen: false,
                    session,
                },
            );
            call.number(my_device);
            call.hosts.remove(&my_device);
            match my_caps {
                Some(c) => call.caps.insert(my_device, c),
                None => call.caps.remove(&my_device),
            };
            Some((call.id, call.max_gen, session))
        }
    };
    if let Some((call_id, generation, session)) = start {
        notify_roster(node, space, channel);
        let payload = SignalPayload::Join {
            muted,
            deafened: false,
            screen: false,
            generation,
            reply: false,
            sfu: my_caps,
            session,
        };
        broadcast(node, space, channel, call_id, payload).await;
        if let Some(c) = node.voice.calls.lock().get_mut(&(space, channel)) {
            c.joining = false;
        }
        maybe_rekey(node, space, channel);
        maybe_replan(node, space, channel);
        wait_for_key(node, space, channel).await;
    }
    // 참가하는 동안 P2P 참가를 그만뒀거나 Tor처럼 보게 됐으면(바뀜은 참가 검사 뒤에 올 수 있음) 나옵니다.
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    if let Some(e) = voice_block(node, &state, &space, &channel) {
        leave(node, space, channel).await?;
        return Err(e);
    }
    let tor = tor_call(node, &state, &channel);
    // Tor 통화는 서로 신뢰한 상대와만 WebRTC로 잇습니다. 켜 둔 신뢰가 없거나 익명 채널이면 STUN 서버를 내주지
    // 않습니다. shortcut: 통화 중에 처음 켠 신뢰는 다시 참가해야 STUN을 씀(그 전엔 같은 망 후보만, 못
    // 이으면 소리는 Tor로 감). 자주 겪으면 참가자 알림에 ICE 서버를 싣습니다.
    let ice_servers = if tor && (state.is_anonymous(&channel) || !crate::trust::any(node)) {
        Vec::new()
    } else {
        crate::stun::call_ice(node, &space)
    };
    refresh_direct(node, space, channel);
    let calls = node.voice.calls.lock();
    let call = calls
        .get(&(space, channel))
        .filter(|c| c.me_in)
        .ok_or_else(|| Error::conflict("참가하는 동안 통화에서 빠짐"))?;
    let (media_key, generation) = match &call.key {
        Some(k) => (encode_key(k), k.generation),
        None => {
            // 관리 기기의 키가 아직 오지 않았습니다. 받는 대로 참가자 알림으로 새 세대가 갑니다.
            let mut junk = Zeroizing::new([0u8; 32]);
            rand::rngs::OsRng.fill_bytes(&mut junk[..]);
            let k = MediaKey {
                generation: 0,
                issuer: my_device,
                key: junk,
            };
            (encode_key(&k), 0)
        }
    };
    let anonymous = state.is_anonymous(&channel);
    let (topology, sfu_host, sfu_host_user) = shown_topology(call, anonymous, tor);
    Ok(VoiceJoinResult {
        call: call.id,
        participants: view_of((space, channel), call, anonymous, Some(my_device)),
        media_key,
        generation,
        topology,
        sfu_host,
        sfu_host_user,
        ice_servers,
    })
}

/// 키 관리 기기의 키를 잠깐 기다립니다.
async fn wait_for_key(node: &Node, space: SpaceId, channel: ChannelId) {
    let deadline = tokio::time::Instant::now() + KEY_WAIT;
    loop {
        let notified = node.voice.key_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let ready = node
            .voice
            .calls
            .lock()
            .get(&(space, channel))
            .is_none_or(|c| !c.me_in || c.key.is_some());
        if ready {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return;
        }
    }
}

/// 통화에서 나옵니다(참가 중이 아니면 아무것도 하지 않음).
pub(crate) async fn leave(node: &Arc<Node>, space: SpaceId, channel: ChannelId) -> Result<()> {
    let me = node.me()?;
    let left = {
        let mut calls = node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(space, channel)) else {
            return Ok(());
        };
        if !call.me_in {
            return Ok(());
        }
        call.me_in = false;
        call.joining = false;
        call.key = None;
        call.reset_plan(&me.device_id());
        call.remove(&me.device_id());
        let id = call.id;
        if call.parts.is_empty() {
            calls.remove(&(space, channel));
        }
        id
    };
    notify_roster(node, space, channel);
    broadcast(node, space, channel, left, SignalPayload::Leave).await;
    Ok(())
}

/// 해소된 상태가 바뀐 직후(`crews::on_state_applied`): 익명이 꺼진 채널, Tor로 도는 기기에서 P2P 전용이 바뀐 채널의
/// 통화에서 나옵니다. 모든 참가자의 기기가 각자 이렇게 나오므로, 익명으로 들어온 참가자가 이름과 함께 통화에 남지
/// 않고, 일반 채널이 된 통화에 WebRTC 연결이 남지 않습니다.
pub(crate) fn on_state_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    old: Option<&SpaceState>,
    new: &SpaceState,
) {
    let Some(old) = old else { return };
    // 익명이 켜졌거나 멤버가 바뀌었으면(크루 신뢰) 직접 잇는 상대가 달라집니다. 상태를 다 바꾼 뒤에 셉니다.
    node.spawn_detached(|node| async move { refresh_direct_all(&node) });
    let tor = tor::tor_active(node);
    let off: Vec<ChannelId> = {
        let calls = node.voice.calls.lock();
        calls
            .iter()
            .filter(|((s, _), call)| s == space && call.me_in)
            .map(|((_, c), _)| *c)
            // 익명이 꺼졌거나, Tor로 도는 기기에서 P2P 전용이 켜지거나 꺼진 채널(WebRTC ↔ Tor 통화로 나르는 길이
            // 바뀌어 지금 연결을 그대로 쓸 수 없음. 꺼질 때 남은 WebRTC는 IP를 계속 드러냄).
            .filter(|c| {
                (old.is_anonymous(c) && !new.is_anonymous(c))
                    || (tor && old.is_p2p(c) != new.is_p2p(c))
            })
            .collect()
    };
    for channel in off {
        let space = *space;
        node.spawn_detached(move |node| async move {
            if let Err(e) = leave(&node, space, channel).await {
                debug!(error = %e, "익명이 꺼진 통화에서 나오지 못함");
            }
        });
    }
}

/// 이제 통화하면 안 되는 통화에서 나옵니다: 참가를 그만둔(또는 Tor로 돌아간) P2P 전용 채널 통화. WebRTC가 실제 IP를
/// 드러내지 않게 합니다. 일반 채널 통화는 Tor로 바뀌면 `Topology::Tor`로 알려(참가자 알림) 앱이 WebRTC를 닫습니다.
pub(crate) async fn leave_blocked(node: &Arc<Node>) {
    let joined: Vec<(SpaceId, ChannelId)> = node
        .voice
        .calls
        .lock()
        .iter()
        .filter(|(_, c)| c.me_in)
        .map(|(k, _)| *k)
        .collect();
    for (s, c) in joined {
        let blocked = node
            .space_state(&s)
            .is_none_or(|st| voice_block(node, &st, &s, &c).is_some());
        if blocked {
            let _ = leave(node, s, c).await;
        }
    }
}

/// 망 설정(시험의 `simulate_tor` 등)이 바뀐 뒤 통화 정책을 맞춥니다.
pub(crate) async fn enforce_policy(node: &Arc<Node>) {
    leave_blocked(node).await;
    // Tor에서는 SFU를 맡지 않습니다(참가자에게 실제 IP가 드러남). 남은 통화에는 못 맡는다고 알립니다.
    if !sfu::can_host(node) {
        sfu::stop(node).await;
    }
    refresh_caps(node).await;
}

/// 음소거·화면공유 상태를 바꿉니다.
pub(crate) async fn set_state(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    muted: bool,
    deafened: bool,
    screen: bool,
) -> Result<()> {
    let me = node.me()?;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    if screen && !has(&state, &me.user(), &channel, Permissions::STREAM) {
        return Err(Error::forbidden("화면공유 권한이 없음"));
    }
    let muted = muted || !has(&state, &me.user(), &channel, Permissions::SPEAK);
    let call_id = {
        let mut calls = node.voice.calls.lock();
        let call = calls
            .get_mut(&(space, channel))
            .filter(|c| c.me_in)
            .ok_or_else(|| Error::invalid("이 통화에 참가 중이 아님"))?;
        let part = call
            .parts
            .get_mut(&me.device_id())
            .ok_or_else(|| Error::internal("참가자 목록에 내가 없음"))?;
        part.muted = muted;
        part.deafened = deafened;
        part.screen = screen;
        if !screen {
            call.viewers.remove(&me.device_id());
        }
        (call.id, call.caps.get(&me.device_id()).copied())
    };
    let (call_id, caps) = call_id;
    notify_roster(node, space, channel);
    let payload = SignalPayload::State {
        muted,
        deafened,
        screen,
        sfu: caps,
    };
    broadcast(node, space, channel, call_id, payload).await;
    Ok(())
}

/// 앱이 보낸 WebRTC 신호를 참가자 하나에게 전합니다.
pub(crate) async fn relay_from_app(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    to: DeviceId,
    payload: SignalPayload,
) -> Result<()> {
    validate_app_payload(&payload)?;
    let me = node.me()?;
    if let Some(st) = node
        .space_state(&space)
        .filter(|st| tor_call(node, st, &channel))
    {
        // Tor 통화의 WebRTC 신호(후보에 IP가 실림)는 내가 신뢰한 참가자에게만. SFU는 쓰지 않습니다.
        let user = node
            .voice
            .calls
            .lock()
            .get(&(space, channel))
            .and_then(|c| c.parts.get(&to))
            .map(|p| p.user);
        let webrtc = matches!(
            payload,
            SignalPayload::Offer { .. } | SignalPayload::Answer { .. } | SignalPayload::Ice { .. }
        );
        if !webrtc || !user.is_some_and(|u| signal_ok(node, &st, &channel, &u)) {
            return Err(Error::conflict(
                "Tor 통화에서는 서로 신뢰한 참가자에게만 WebRTC 신호를 보냅니다(소리는 voice.media로)",
            ));
        }
    }
    if is_sfu_media(&payload) {
        return relay_sfu(node, space, channel, call, to, payload, me.device_id()).await;
    }
    let hint = {
        let mut calls = node.voice.calls.lock();
        let c = calls
            .get_mut(&(space, channel))
            .filter(|c| c.me_in)
            .ok_or_else(|| Error::invalid("이 통화에 참가 중이 아님"))?;
        if c.id != call {
            return Err(Error::invalid("통화 ID가 다름"));
        }
        if to == me.device_id() {
            return Err(Error::invalid("자기 자신에게는 보낼 수 없음"));
        }
        let hint = c
            .parts
            .get(&to)
            .ok_or_else(|| Error::not_found("통화 참가자가 아님"))?
            .peer;
        if let SignalPayload::WatchScreen { watching } = payload {
            set_watching(c, to, me.device_id(), watching);
        }
        hint
    };
    send_to(node, to, hint, space, channel, call, payload).await
}

fn validate_app_payload(p: &SignalPayload) -> Result<()> {
    match p {
        SignalPayload::SfuPlan { .. } | SignalPayload::SfuHost { .. } => {
            return Err(Error::invalid("SFU 호스트 결정은 데몬끼리만 주고받습니다"));
        }
        SignalPayload::Join { .. }
        | SignalPayload::Leave
        | SignalPayload::State { .. }
        | SignalPayload::MediaKey { .. } => {
            return Err(Error::invalid(
                "참가·퇴장·상태는 voice.join·voice.leave·voice.state로 보내세요",
            ));
        }
        _ => {}
    }
    if media_payload_ok(p) {
        Ok(())
    } else {
        Err(Error::invalid("신호가 너무 큼"))
    }
}

/// 미디어 신호(WebRTC·SFU)의 크기 한도. 앱이 보낸 신호와 다른 데몬이 보낸 신호에 똑같이 씁니다(고친 데몬이 큰
/// SDP를 되풀이해 보내 호스트의 SFU 실행기나 앱을 붙잡지 못하게). 제어 신호는 각자 따로 검사합니다.
fn media_payload_ok(p: &SignalPayload) -> bool {
    match p {
        SignalPayload::Offer { sdp } | SignalPayload::Answer { sdp } => sdp.len() <= MAX_SDP,
        SignalPayload::Ice {
            candidate, sdp_mid, ..
        }
        | SignalPayload::SfuIce {
            candidate, sdp_mid, ..
        } => {
            candidate.len() <= MAX_CANDIDATE && sdp_mid.as_ref().is_none_or(|m| m.len() <= MAX_MID)
        }
        SignalPayload::SfuOffer { sdp, tracks, .. }
        | SignalPayload::SfuAnswer { sdp, tracks, .. } => {
            sdp.len() <= MAX_SDP
                && tracks.len() <= MAX_SFU_TRACKS
                && tracks.iter().all(|t| t.mid.len() <= MAX_MID)
        }
        SignalPayload::WatchScreen { .. }
        | SignalPayload::SfuSelect { .. }
        | SignalPayload::SfuClose { .. }
        | SignalPayload::SfuPlan { .. }
        | SignalPayload::SfuHost { .. }
        | SignalPayload::Join { .. }
        | SignalPayload::Leave
        | SignalPayload::State { .. }
        | SignalPayload::MediaKey { .. } => true,
    }
}

/// 앱 ↔ SFU 호스트 사이의 신호인지.
fn is_sfu_media(p: &SignalPayload) -> bool {
    matches!(
        p,
        SignalPayload::SfuOffer { .. }
            | SignalPayload::SfuAnswer { .. }
            | SignalPayload::SfuIce { .. }
            | SignalPayload::SfuSelect { .. }
            | SignalPayload::SfuClose { .. }
    )
}

fn sfu_error(e: yangpa_sfu::SfuError) -> Error {
    use yangpa_sfu::SfuError as E;
    match e {
        E::NoRoom => Error::not_found(e.to_string()),
        E::NotAllowed => Error::forbidden(e.to_string()),
        E::Full => Error::conflict(e.to_string()),
        _ => Error::invalid(e.to_string()),
    }
}

fn sfu_wire_error(e: &yangpa_sfu::SfuError) -> YpResponse {
    use yangpa_sfu::SfuError as E;
    let code = match e {
        E::NoRoom => WireErrorCode::NotFound,
        E::NotAllowed | E::Full => WireErrorCode::Forbidden,
        _ => WireErrorCode::BadRequest,
    };
    YpResponse::error(code, e.to_string())
}

/// 앱의 SFU 신호를 지금 호스트에게 보냅니다(내가 호스트면 이 데몬의 엔진으로).
async fn relay_sfu(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    to: DeviceId,
    payload: SignalPayload,
    my_device: DeviceId,
) -> Result<()> {
    let hint = {
        let calls = node.voice.calls.lock();
        let c = calls
            .get(&(space, channel))
            .filter(|c| c.me_in)
            .ok_or_else(|| Error::invalid("이 통화에 참가 중이 아님"))?;
        if c.id != call {
            return Err(Error::invalid("통화 ID가 다름"));
        }
        if c.plan.host != Some(to) || !c.host_present(&to) {
            return Err(Error::conflict("지금 SFU 호스트가 아님"));
        }
        c.parts
            .get(&to)
            .and_then(|p| p.peer)
            .or_else(|| c.hosts.get(&to).and_then(|h| h.peer))
    };
    if to == my_device {
        return sfu::handle_signal(node, (space, channel), my_device, payload)
            .await
            .map_err(sfu_error);
    }
    send_to(node, to, hint, space, channel, call, payload).await
}

/// SFU 엔진이 참가자에게 보내는 신호(`sfu` 실행기가 기기마다 순서대로 부름). 내 기기면 앱에 바로 알립니다.
pub(crate) async fn sfu_deliver(
    node: &Arc<Node>,
    (space, channel): yangpa_sfu::RoomId,
    to: DeviceId,
    payload: SignalPayload,
) {
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let Some((call_id, hint, me_in)) = node
        .voice
        .calls
        .lock()
        .get(&(space, channel))
        .map(|c| (c.id, c.parts.get(&to).and_then(|p| p.peer), c.me_in))
    else {
        return;
    };
    if to == my_device {
        if me_in {
            node.notify.publish(Notification::VoiceSignal {
                space,
                channel,
                call: call_id,
                from: my_device,
                payload,
            });
        }
        return;
    }
    if let Err(e) = send_to(node, to, hint, space, channel, call_id, payload).await {
        debug!(device = %to.short(), error = %e, "SFU 신호 전달 실패");
    }
}

fn set_watching(call: &mut Call, sharer: DeviceId, viewer: DeviceId, watching: bool) {
    if watching {
        call.viewers.entry(sharer).or_default().insert(viewer);
    } else if let Some(v) = call.viewers.get_mut(&sharer) {
        v.remove(&viewer);
        if v.is_empty() {
            call.viewers.remove(&sharer);
        }
    }
}

/// 다른 참가자의 화면공유를 보기 시작하거나 그만둡니다.
pub(crate) async fn watch(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    device: DeviceId,
    watching: bool,
) -> Result<()> {
    let me = node.me()?;
    let (call_id, hint) = {
        let mut calls = node.voice.calls.lock();
        let c = calls
            .get_mut(&(space, channel))
            .filter(|c| c.me_in)
            .ok_or_else(|| Error::invalid("이 통화에 참가 중이 아님"))?;
        if device == me.device_id() {
            return Err(Error::invalid("자기 화면은 볼 수 없음"));
        }
        let part = c
            .parts
            .get(&device)
            .ok_or_else(|| Error::not_found("통화 참가자가 아님"))?;
        if watching && !part.screen {
            return Err(Error::invalid("화면공유 중이 아님"));
        }
        let hint = part.peer;
        set_watching(c, device, me.device_id(), watching);
        (c.id, hint)
    };
    notify_roster(node, space, channel);
    send_to(
        node,
        device,
        hint,
        space,
        channel,
        call_id,
        SignalPayload::WatchScreen { watching },
    )
    .await
}

// ───────────── 받기 ─────────────

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_signal(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    from: DeviceId,
    payload: SignalPayload,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    if device != from {
        return YpResponse::error(WireErrorCode::BadRequest, "보낸 기기가 연결과 다름");
    }
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if !is_voice(&state, &channel) || !can_view(&state, &me.user(), &channel) {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 통화 채널");
    }
    if !matches!(payload, SignalPayload::Leave) && !can_connect(&state, &user, &channel) {
        return YpResponse::error(WireErrorCode::Forbidden, "통화 권한 없음");
    }
    if !media_payload_ok(&payload) {
        return YpResponse::error(WireErrorCode::BadRequest, "신호가 너무 큼");
    }
    let ctx = Inbound {
        node,
        peer,
        space,
        channel,
        user,
        from,
        my_device: me.device_id(),
    };
    match payload {
        SignalPayload::Join {
            muted,
            deafened,
            screen,
            generation,
            reply,
            sfu,
            session,
        } => {
            let muted = muted || !has(&state, &user, &channel, Permissions::SPEAK);
            let screen = screen && has(&state, &user, &channel, Permissions::STREAM);
            on_join(
                &ctx,
                call,
                (muted, deafened, screen),
                generation,
                reply,
                sfu,
                session,
            )
            .await
        }
        SignalPayload::Leave => on_leave(&ctx),
        SignalPayload::State {
            muted,
            deafened,
            screen,
            sfu,
        } => {
            let muted = muted || !has(&state, &user, &channel, Permissions::SPEAK);
            let screen = screen && has(&state, &user, &channel, Permissions::STREAM);
            on_state(&ctx, muted, deafened, screen, sfu)
        }
        SignalPayload::MediaKey { generation, sealed } => {
            on_media_key(&ctx, &me, generation, &sealed)
        }
        SignalPayload::SfuPlan { seq, host } => on_sfu_plan(&ctx, &state, seq, host),
        SignalPayload::SfuHost { caps } => on_sfu_host(&ctx, &state, caps),
        // Tor 통화에서는 SFU를 쓰지 않고, 내가 신뢰하지 않는 참가자의 WebRTC 신호는 앱에 넘기지 않습니다(앱이 응답하면
        // IP 후보가 나감).
        SignalPayload::SfuOffer { .. }
        | SignalPayload::SfuAnswer { .. }
        | SignalPayload::SfuIce { .. }
            if tor_call(node, &state, &channel) =>
        {
            YpResponse::error(WireErrorCode::Forbidden, "Tor 통화에서는 SFU를 쓰지 않음")
        }
        SignalPayload::Offer { .. } | SignalPayload::Answer { .. } | SignalPayload::Ice { .. }
            if tor_call(node, &state, &channel) && !signal_ok(node, &state, &channel, &user) =>
        {
            YpResponse::error(
                WireErrorCode::Forbidden,
                "Tor 통화에서는 서로 신뢰한 참가자와만 WebRTC로 이음",
            )
        }
        p @ (SignalPayload::SfuOffer { .. }
        | SignalPayload::SfuAnswer { .. }
        | SignalPayload::SfuIce { .. }
        | SignalPayload::SfuSelect { .. }
        | SignalPayload::SfuClose { .. }) => on_sfu_media(&ctx, p).await,
        p @ (SignalPayload::Offer { .. }
        | SignalPayload::Answer { .. }
        | SignalPayload::Ice { .. }
        | SignalPayload::WatchScreen { .. }) => on_webrtc(&ctx, p),
    }
}

struct Inbound<'a> {
    node: &'a Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
    user: UserId,
    from: DeviceId,
    my_device: DeviceId,
}

/// 이미 참가자인 기기(`old`는 그 기기가 지난번에 알린 참가 번호)가 새 번호로 알렸는지. 0은 모름(예전 빌드)이라 따지지 않습니다.
fn is_rejoin(old: Option<u64>, session: u64) -> bool {
    old.is_some_and(|o| o != 0 && session != 0 && o != session)
}

async fn on_join(
    c: &Inbound<'_>,
    call: CallId,
    (muted, deafened, screen): (bool, bool, bool),
    generation: u64,
    reply: bool,
    sfu_caps: Option<SfuCaps>,
    session: u64,
) -> YpResponse {
    let (new, rejoined, me_in, mine, adopted, was_manager) = {
        let mut calls = c.node.voice.calls.lock();
        let entry = calls
            .entry((c.space, c.channel))
            .or_insert_with(|| Call::new(call));
        if entry.parts.is_empty() {
            entry.id = call;
        }
        if !entry.parts.contains_key(&c.from) && entry.parts.len() >= MAX_PARTICIPANTS {
            return YpResponse::error(WireErrorCode::Forbidden, "통화 인원이 가득 참");
        }
        let was_manager = entry.me_in && entry.manager() == Some(c.my_device);
        let old = entry.parts.get(&c.from);
        let new = old.is_none_or(|p| p.peer != Some(c.peer));
        // 이미 참가자인 기기가 다른 참가 번호로 알림: 다시 참가함(앱을 다시 켜 노드는 같은 연결로 돌아옴). 앱은 지난 WebRTC
        // 연결을 붙든 채 새 연결을 기다리다 늦거나 끊기므로, 나감을 먼저 알려 새로 잇게 합니다. 다시 알림(재연결)은 번호가 같음.
        let rejoined = !reply && is_rejoin(old.map(|p| p.session), session);
        entry.parts.insert(
            c.from,
            Part {
                user: c.user,
                peer: Some(c.peer),
                muted,
                deafened,
                screen,
                session,
            },
        );
        if rejoined {
            for v in entry.viewers.values_mut() {
                v.remove(&c.from);
            }
        }
        entry.number(c.from);
        if !screen {
            entry.viewers.remove(&c.from);
        }
        match sfu_caps {
            Some(cap) => entry.caps.insert(c.from, cap),
            None => entry.caps.remove(&c.from),
        };
        entry.hosts.remove(&c.from);
        entry.note_claim(generation);
        let adopted = entry.adopt_pending(&c.my_device);
        entry.adopt_plan_pending(&c.my_device);
        let my_caps = entry.caps.get(&c.my_device).copied();
        let mine = entry
            .parts
            .get(&c.my_device)
            .filter(|_| entry.me_in)
            .map(|p| {
                (
                    p.muted,
                    p.deafened,
                    p.screen,
                    entry.generation(),
                    entry.id,
                    my_caps,
                    p.session,
                )
            });
        (new, rejoined, entry.me_in, mine, adopted, was_manager)
    };
    if adopted {
        c.node.voice.key_changed.notify_waiters();
    }
    if rejoined && let Some((.., my_call, _, _)) = mine {
        c.node.notify.publish(Notification::VoiceSignal {
            space: c.space,
            channel: c.channel,
            call: my_call,
            from: c.from,
            payload: SignalPayload::Leave,
        });
    }
    notify_roster(c.node, c.space, c.channel);
    if !me_in {
        if !reply && sfu::prefers_hosting(c.node) {
            announce_host(c.node, c.space, c.channel, c.from, c.peer);
        }
        return YpResponse::Ok;
    }
    if !reply
        && let Some((muted, deafened, screen, generation, my_call, my_caps, my_session)) = mine
    {
        // 새 참가자가 참가 알림을 다 돌렸을 때 나를 알고 있게, 답하기 전에 내 상태를 돌려줍니다.
        let payload = SignalPayload::Join {
            muted,
            deafened,
            screen,
            generation,
            reply: true,
            sfu: my_caps,
            session: my_session,
        };
        let send = send_to(
            c.node,
            c.from,
            Some(c.peer),
            c.space,
            c.channel,
            my_call,
            payload,
        );
        if let Ok(Err(e)) | Err(e) = tokio::time::timeout(REPLY_WAIT, send)
            .await
            .map_err(|_| Error::Network("시간 초과".into()))
        {
            debug!(error = %e, "참가 답장 실패");
        }
    }
    if reply {
        maybe_replan(c.node, c.space, c.channel);
    } else {
        tell_plan(c, was_manager).await;
    }
    if new || rejoined {
        maybe_rekey(c.node, c.space, c.channel);
    } else {
        resend_key(c.node, c.space, c.channel, c.from);
    }
    YpResponse::Ok
}

/// 새(또는 다시 알린) 참가자에게 토폴로지를 알립니다. 관리 기기면 다시 정하고, 참가 알림에 답하기 전에 지금 결정을
/// 보내 참가를 마칠 때 토폴로지를 알게 합니다. 막 관리 기기 자리를 넘긴 기기도 지금 결정을 실마리로 보냅니다.
async fn tell_plan(c: &Inbound<'_>, was_manager: bool) {
    let sends = replan(c.node, c.space, c.channel);
    if sends.is_some() {
        notify_roster(c.node, c.space, c.channel);
    }
    let current = {
        let calls = c.node.voice.calls.lock();
        calls
            .get(&(c.space, c.channel))
            .filter(|call| call.me_in && (was_manager || call.manager() == Some(c.my_device)))
            .filter(|call| call.plan.seq > 0)
            .map(|call| (call.id, call.plan))
    };
    if let Some(s) = &sends
        && let Some((d, hint)) = s.host.filter(|(d, _)| *d != c.from)
    {
        let payload = SignalPayload::SfuPlan {
            seq: s.plan.seq,
            host: s.plan.host,
        };
        let send = send_to(c.node, d, hint, c.space, c.channel, s.call, payload);
        let _ = tokio::time::timeout(REPLY_WAIT, send).await;
    }
    if let Some((call_id, plan)) = current {
        let payload = SignalPayload::SfuPlan {
            seq: plan.seq,
            host: plan.host,
        };
        let send = send_to(
            c.node,
            c.from,
            Some(c.peer),
            c.space,
            c.channel,
            call_id,
            payload,
        );
        if let Ok(Err(e)) | Err(e) = tokio::time::timeout(REPLY_WAIT, send)
            .await
            .map_err(|_| Error::Network("시간 초과".into()))
        {
            debug!(error = %e, "토폴로지 결정 전달 실패");
        }
    }
    if let Some(mut s) = sends {
        // 호스트와 새 참가자에게는 위에서 보냈습니다.
        s.host = None;
        spawn_plan(c.node, s, Some(c.from));
    }
}

fn on_leave(c: &Inbound<'_>) -> YpResponse {
    let (removed, me_in, my_call, adopted) = {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(c.space, c.channel)) else {
            return YpResponse::Ok;
        };
        let removed = call.remove(&c.from);
        let adopted = removed && call.adopt_pending(&c.my_device);
        if removed {
            call.adopt_plan_pending(&c.my_device);
        }
        let (me_in, id) = (call.me_in, call.id);
        if call.parts.is_empty() {
            calls.remove(&(c.space, c.channel));
        }
        (removed, me_in, id, adopted)
    };
    if !removed {
        return YpResponse::Ok;
    }
    if adopted {
        c.node.voice.key_changed.notify_waiters();
    }
    notify_roster(c.node, c.space, c.channel);
    if me_in {
        c.node.notify.publish(Notification::VoiceSignal {
            space: c.space,
            channel: c.channel,
            call: my_call,
            from: c.from,
            payload: SignalPayload::Leave,
        });
        maybe_rekey(c.node, c.space, c.channel);
        maybe_replan(c.node, c.space, c.channel);
    }
    YpResponse::Ok
}

fn on_state(
    c: &Inbound<'_>,
    muted: bool,
    deafened: bool,
    screen: bool,
    sfu_caps: Option<SfuCaps>,
) -> YpResponse {
    let caps_changed = {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(c.space, c.channel)) else {
            return YpResponse::error(WireErrorCode::NotFound, "통화 참가자가 아님");
        };
        let Some(part) = call.parts.get_mut(&c.from) else {
            return YpResponse::error(WireErrorCode::NotFound, "통화 참가자가 아님");
        };
        part.muted = muted;
        part.deafened = deafened;
        part.screen = screen;
        if !screen {
            call.viewers.remove(&c.from);
        }
        let changed = !same_caps(call.caps.get(&c.from).copied(), sfu_caps);
        match sfu_caps {
            Some(cap) => call.caps.insert(c.from, cap),
            None => call.caps.remove(&c.from),
        };
        changed
    };
    notify_roster(c.node, c.space, c.channel);
    if caps_changed {
        maybe_replan(c.node, c.space, c.channel);
    }
    YpResponse::Ok
}

/// 키 관리 기기의 토폴로지 결정. 호스트는 그 통화에 접속할 수 있는 멤버의 활성 기기여야 합니다.
fn on_sfu_plan(
    c: &Inbound<'_>,
    state: &SpaceState,
    seq: u64,
    host: Option<DeviceId>,
) -> YpResponse {
    if seq == 0 || seq > GEN_LIMIT {
        return YpResponse::error(WireErrorCode::BadRequest, "결정 번호가 범위 밖");
    }
    // 호스트의 사용자(앱에 누가 중계하는지 보여 줌).
    let mut owner = None;
    if let Some(h) = host {
        owner = state
            .active_devices()
            .find(|(u, d)| d.device == h && can_connect(state, u, &c.channel))
            .map(|(u, _)| *u);
        if owner.is_none() {
            return YpResponse::error(WireErrorCode::BadRequest, "호스트가 통화 자격이 없는 기기");
        }
    }
    let changed = {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(c.space, c.channel)) else {
            return YpResponse::error(WireErrorCode::NotFound, "모르는 통화");
        };
        if !call.parts.contains_key(&c.from) {
            return YpResponse::error(WireErrorCode::Forbidden, "통화 참가자가 아님");
        }
        let changed = call.accept_plan(c.from, Plan { seq, host }, &c.my_device);
        if let Some(h) = host
            && let Some(x) = call.hosts.get_mut(&h)
        {
            x.owner = x.owner.or(owner);
        }
        changed
    };
    if changed {
        notify_roster(c.node, c.space, c.channel);
    }
    YpResponse::Ok
}

/// 참가하지 않는 노드가 SFU 호스트 후보로 나서거나(`Some`) 그만둠(`None`). 나서려면 그 채널에 `manage_voice`가
/// 있어야 합니다.
fn on_sfu_host(c: &Inbound<'_>, state: &SpaceState, caps: Option<SfuCaps>) -> YpResponse {
    if caps.is_some() && !may_host_unjoined(state, &c.user, &c.channel) {
        return YpResponse::error(
            WireErrorCode::Forbidden,
            "통화 밖에서 호스트를 맡으려면 음성 관리(manage_voice) 권한이 필요합니다",
        );
    }
    {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(c.space, c.channel)) else {
            return YpResponse::Ok;
        };
        if call.parts.contains_key(&c.from) {
            return YpResponse::error(
                WireErrorCode::BadRequest,
                "참가자는 참가·상태 알림으로 호스트 조건을 알립니다",
            );
        }
        match caps {
            Some(cap) => {
                if !call.hosts.contains_key(&c.from) && call.hosts.len() >= MAX_HOSTS {
                    return YpResponse::error(WireErrorCode::Forbidden, "호스트 후보가 너무 많음");
                }
                call.hosts.insert(
                    c.from,
                    HostOnly {
                        peer: Some(c.peer),
                        user: Some(c.user),
                        owner: Some(c.user),
                        caps: Some(cap),
                    },
                );
            }
            None => {
                call.hosts.remove(&c.from);
            }
        }
    }
    debug!(device = %c.from.short(), on = caps.is_some(), "SFU 호스트 후보 알림 받음");
    notify_roster(c.node, c.space, c.channel);
    maybe_replan(c.node, c.space, c.channel);
    YpResponse::Ok
}

/// SFU 세션 신호. 내가 호스트면 참가자가 보낸 것만 엔진에, 내가 참가자면 지금 호스트가 보낸 것만 앱에 넘깁니다.
async fn on_sfu_media(c: &Inbound<'_>, payload: SignalPayload) -> YpResponse {
    let to_app = {
        let calls = c.node.voice.calls.lock();
        let Some(call) = calls.get(&(c.space, c.channel)) else {
            return YpResponse::error(WireErrorCode::NotFound, "모르는 통화");
        };
        if call.plan.host == Some(c.my_device) && call.parts.contains_key(&c.from) {
            None
        } else if call.me_in && call.plan.host == Some(c.from) {
            Some(call.id)
        } else {
            return YpResponse::error(WireErrorCode::Forbidden, "SFU 신호를 받을 수 없음");
        }
    };
    match to_app {
        None => match sfu::handle_signal(c.node, (c.space, c.channel), c.from, payload).await {
            Ok(()) => YpResponse::Ok,
            Err(e) => sfu_wire_error(&e),
        },
        Some(call) => {
            c.node.notify.publish(Notification::VoiceSignal {
                space: c.space,
                channel: c.channel,
                call,
                from: c.from,
                payload,
            });
            YpResponse::Ok
        }
    }
}

/// 받은 프레임 키. 자기 목록의 관리 기기가 보낸 키만 쓰고(그 기기에게서 받은 것보다 큰 세대), 다른 참가자의 키는
/// 그 기기가 관리 기기가 될 때까지 따로 둡니다. 내가 관리 기기인데 키를 받았으면 목록이 엇갈린 것이라 새로 만듭니다.
fn on_media_key(
    c: &Inbound<'_>,
    me: &crate::identity::Me,
    generation: u64,
    sealed: &[u8],
) -> YpResponse {
    if generation == 0 || generation > GEN_LIMIT {
        return YpResponse::error(WireErrorCode::BadRequest, "통화 키 세대가 범위 밖");
    }
    if sealed.len() > MAX_SEALED {
        return YpResponse::error(WireErrorCode::BadRequest, "통화 키 상자가 너무 큼");
    }
    let rekey = {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(c.space, c.channel)) else {
            return YpResponse::error(WireErrorCode::NotFound, "참가 중인 통화가 아님");
        };
        if !call.me_in || !call.parts.contains_key(&c.from) {
            return YpResponse::error(WireErrorCode::Forbidden, "통화 참가자가 아님");
        }
        if call.manager() == Some(c.my_device) {
            // 참가자 목록이 잠깐 엇갈려 다른 기기도 키를 만들었습니다. 관리 기기가 더 큰 세대로 정리합니다.
            call.note_claim(generation);
            generation >= call.generation()
        } else {
            false
        }
    };
    if rekey {
        maybe_rekey(c.node, c.space, c.channel);
        return YpResponse::Ok;
    }
    let ad = media_ad(&c.space, &c.channel, generation, &c.from);
    let Ok(plain) = yangpa_crypto::open_sealed(&c.my_device, &me.device.boxkey, &ad, sealed) else {
        return YpResponse::error(WireErrorCode::BadRequest, "통화 키를 열 수 없음");
    };
    let Ok(bytes) = <[u8; 32]>::try_from(plain.as_slice()) else {
        return YpResponse::error(WireErrorCode::BadRequest, "통화 키 길이");
    };
    let key = MediaKey {
        generation,
        issuer: c.from,
        key: Zeroizing::new(bytes),
    };
    {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls.get_mut(&(c.space, c.channel)) else {
            return YpResponse::Ok;
        };
        if !call.me_in || !call.parts.contains_key(&c.from) {
            return YpResponse::Ok;
        }
        let manager = call.manager();
        if manager == Some(c.from) {
            if !call.accepts_from(&c.from, generation) {
                return YpResponse::Ok;
            }
            call.install(key);
        } else {
            if manager != Some(c.my_device)
                && call
                    .pending
                    .get(&c.from)
                    .is_none_or(|k| k.generation < generation)
            {
                call.pending.insert(c.from, key);
            }
            return YpResponse::Ok;
        }
    }
    c.node.voice.key_changed.notify_waiters();
    notify_roster(c.node, c.space, c.channel);
    YpResponse::Ok
}

fn on_webrtc(c: &Inbound<'_>, payload: SignalPayload) -> YpResponse {
    let my_call = {
        let mut calls = c.node.voice.calls.lock();
        let Some(call) = calls
            .get_mut(&(c.space, c.channel))
            .filter(|call| call.me_in && call.parts.contains_key(&c.from))
        else {
            return YpResponse::error(WireErrorCode::Forbidden, "같은 통화에 참가 중이 아님");
        };
        if let SignalPayload::WatchScreen { watching } = payload {
            let sharing = call.parts.get(&c.my_device).is_some_and(|p| p.screen);
            if sharing || !watching {
                set_watching(call, c.my_device, c.from, watching);
            }
        }
        call.id
    };
    if matches!(payload, SignalPayload::WatchScreen { .. }) {
        notify_roster(c.node, c.space, c.channel);
    }
    c.node.notify.publish(Notification::VoiceSignal {
        space: c.space,
        channel: c.channel,
        call: my_call,
        from: c.from,
        payload,
    });
    YpResponse::Ok
}

/// 노드가 끊기면 그 노드의 기기를 참가자에서 뺍니다(관리 기기면 새 키).
pub(crate) fn on_peer_disconnected(node: &Arc<Node>, peer: PeerId) {
    let my_device = node.me().ok().map(|m| m.device_id());
    let mut changed = Vec::new();
    {
        let mut calls = node.voice.calls.lock();
        for (key, call) in calls.iter_mut() {
            let gone: Vec<DeviceId> = call
                .parts
                .iter()
                .filter(|(_, p)| p.peer == Some(peer))
                .map(|(d, _)| *d)
                .collect();
            let hosts_before = call.hosts.len();
            call.hosts.retain(|_, h| h.peer != Some(peer));
            if gone.is_empty() && call.hosts.len() == hosts_before {
                continue;
            }
            for d in &gone {
                call.remove(d);
            }
            if let Some(me) = &my_device {
                call.adopt_pending(me);
                call.adopt_plan_pending(me);
            }
            changed.push((*key, call.me_in, call.id, gone));
        }
        calls.retain(|_, c| !c.parts.is_empty());
    }
    if !changed.is_empty() {
        node.voice.key_changed.notify_waiters();
    }
    for ((space, channel), me_in, call_id, gone) in changed {
        notify_roster(node, space, channel);
        if me_in {
            for from in gone {
                node.notify.publish(Notification::VoiceSignal {
                    space,
                    channel,
                    call: call_id,
                    from,
                    payload: SignalPayload::Leave,
                });
            }
            maybe_rekey(node, space, channel);
            maybe_replan(node, space, channel);
        }
    }
}

/// 인증된 피어(다시 연결된 참가자 포함)에게, 내가 참가 중이고 그 사용자가 볼 수 있는 통화를 알립니다.
pub(crate) fn on_peer_authed(node: &Arc<Node>, peer: PeerId, user: UserId, device: DeviceId) {
    let announce: Vec<(SpaceId, ChannelId, CallId, SignalPayload)> = {
        let Ok(me) = node.me() else { return };
        let my_device = me.device_id();
        if device == my_device {
            return;
        }
        let calls = node.voice.calls.lock();
        calls
            .iter()
            .filter(|(_, c)| c.me_in)
            .filter_map(|((space, channel), c)| {
                let state = node.space_state(space)?;
                if !state.device_belongs(&user, &device) || !can_view(&state, &user, channel) {
                    return None;
                }
                let p = c.parts.get(&my_device)?;
                Some((
                    *space,
                    *channel,
                    c.id,
                    SignalPayload::Join {
                        muted: p.muted,
                        deafened: p.deafened,
                        screen: p.screen,
                        generation: c.generation(),
                        reply: false,
                        sfu: c.caps.get(&my_device).copied(),
                        // 다시 알림이라 참가할 때의 번호 그대로(받는 쪽이 새 참가로 보지 않게).
                        session: p.session,
                    },
                ))
            })
            .collect()
    };
    for (space, channel, call, payload) in announce {
        node.spawn_detached(move |node| async move {
            announce_to(&node, device, peer, space, channel, call, payload).await;
        });
    }
}

/// 인증 직후의 통화 알림. 상대를 인증한 쪽이 받는 쪽이면(상대가 건 `Auth`에 답하는 중) 상대는 아직 이쪽을
/// 인증하지 않아 `Unauthenticated`로 거절하므로, 잠깐 물러났다 다시 보냅니다.
async fn announce_to(
    node: &Arc<Node>,
    device: DeviceId,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    payload: SignalPayload,
) {
    let mut waits = ANNOUNCE_RETRY.iter();
    loop {
        let r = request_signal(
            node,
            device,
            Some(peer),
            space,
            channel,
            call,
            payload.clone(),
        )
        .await;
        let unauthenticated = matches!(
            r,
            Ok(YpResponse::Error {
                code: WireErrorCode::Unauthenticated,
                ..
            })
        );
        let Some(wait) = waits.next().filter(|_| unauthenticated) else {
            if let Ok(YpResponse::Error { code, .. }) = r {
                debug!(device = %device.short(), ?code, "통화 알림 거절");
            }
            return;
        };
        tokio::time::sleep(*wait).await;
    }
}

/// 뱃지(`badges::tick`): (다른 사람이 있는 통화에 참가 중인지, 그 통화에서 내 화면을 공유 중인지).
pub(crate) fn talking(node: &Node) -> (bool, bool) {
    let Ok(me) = node.me() else {
        return (false, false);
    };
    let calls = node.voice.calls.lock();
    calls
        .values()
        .filter(|c| c.me_in && c.parts.values().any(|p| p.user != me.user()))
        .fold((false, false), |(_, screen), c| {
            let mine = c.parts.get(&me.device_id()).is_some_and(|p| p.screen);
            (true, screen || mine)
        })
}

/// 시험용 들여다보기: 키 자체는 내보내지 않고 세대와 지문만.
pub(crate) mod inspect {
    use super::*;

    /// (참가 중인지, 참가자 기기, 지금 세대, 키 지문(BLAKE3 앞 8바이트)).
    pub(crate) type CallInfo = (bool, Vec<DeviceId>, u64, Option<[u8; 8]>);

    /// 키 지문(BLAKE3 앞 8바이트).
    pub(crate) fn fingerprint(key: &[u8; 32]) -> [u8; 8] {
        let h = blake3::hash(key);
        let mut out = [0u8; 8];
        out.copy_from_slice(&h.as_bytes()[..8]);
        out
    }

    /// (앱에 알리는 토폴로지, 알리는 호스트, 따르는 결정의 호스트, 결정 번호).
    pub(crate) type PlanInfo = (Topology, Option<DeviceId>, Option<DeviceId>, u64);

    pub(crate) fn plan(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<PlanInfo> {
        let tor = node
            .space_state(space)
            .is_some_and(|st| super::tor_call(node, &st, channel));
        let calls = node.voice.calls.lock();
        let c = calls.get(&(*space, *channel))?;
        let (t, h, _) = topology(c, tor);
        Some((t, h, c.plan.host, c.plan.seq))
    }

    /// 앱에 알리는 SFU 호스트의 사용자.
    pub(crate) fn host_user(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<UserId> {
        let calls = node.voice.calls.lock();
        topology(calls.get(&(*space, *channel))?, false).2
    }

    /// 참가하지 않은 통화에 호스트 후보로 나섰는지.
    pub(crate) fn announced(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
        let calls = node.voice.calls.lock();
        calls.get(&(*space, *channel)).is_some_and(|c| c.announced)
    }

    /// 시험용: 이 노드가 따르던 토폴로지 결정을 잊습니다(결정을 받지 못한 호스트 흉내). 맡던 통화방도 닫습니다.
    pub(crate) fn forget_plan(node: &Arc<Node>, space: SpaceId, channel: ChannelId) {
        {
            let mut calls = node.voice.calls.lock();
            let Some(c) = calls.get_mut(&(space, channel)) else {
                return;
            };
            c.plan = Plan::default();
            c.plan_seen.clear();
            c.plan_pending.clear();
        }
        notify_roster(node, space, channel);
    }

    /// 참가 중인 통화의 ID.
    pub(crate) fn call_id(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<CallId> {
        let calls = node.voice.calls.lock();
        calls
            .get(&(*space, *channel))
            .filter(|c| c.me_in)
            .map(|c| c.id)
    }

    /// 마지막으로 앱에 알린, WebRTC로 바로 잇는 참가자.
    pub(crate) fn direct(node: &Node, space: &SpaceId, channel: &ChannelId) -> Vec<DeviceId> {
        let calls = node.voice.calls.lock();
        calls
            .get(&(*space, *channel))
            .map(|c| c.direct.iter().copied().collect())
            .unwrap_or_default()
    }

    /// 이 기기가 그 통화에 알린 SFU 호스트 조건.
    pub(crate) fn my_caps(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<SfuCaps> {
        let me = node.me().ok()?;
        let calls = node.voice.calls.lock();
        calls
            .get(&(*space, *channel))?
            .caps
            .get(&me.device_id())
            .copied()
    }

    pub(crate) fn call(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<CallInfo> {
        let calls = node.voice.calls.lock();
        let c = calls.get(&(*space, *channel))?;
        let fp = c.key.as_ref().map(|k| fingerprint(&k.key));
        Some((
            c.me_in,
            c.parts.keys().copied().collect(),
            c.generation(),
            fp,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_is_smallest_device_and_topology_switches() {
        let mut c = Call::new(CallId::random());
        for i in [5u8, 2, 9] {
            c.parts.insert(
                DeviceId([i; 32]),
                Part {
                    user: UserId([i; 32]),
                    peer: None,
                    muted: false,
                    deafened: false,
                    screen: false,
                    session: 0,
                },
            );
        }
        assert_eq!(c.manager(), Some(DeviceId([2; 32])));
        assert_eq!(topology(&c, false), (Topology::Mesh, None, None));
        for i in 10u8..14 {
            c.parts.insert(
                DeviceId([i; 32]),
                Part {
                    user: UserId([i; 32]),
                    peer: None,
                    muted: false,
                    deafened: false,
                    screen: false,
                    session: 0,
                },
            );
        }
        // 참가자 수만으로는 바뀌지 않고, 관리 기기의 결정을 따릅니다.
        assert_eq!(topology(&c, false), (Topology::Mesh, None, None));
        assert!(c.accept_plan(
            DeviceId([2; 32]),
            Plan {
                seq: 1,
                host: Some(DeviceId([10; 32]))
            },
            &DeviceId([9; 32])
        ));
        // 참가자 호스트는 그 사용자도 알림.
        assert_eq!(
            topology(&c, false),
            (
                Topology::Sfu,
                Some(DeviceId([10; 32])),
                Some(UserId([10; 32]))
            )
        );
        // 호스트가 빠지면 넘겨받는 동안 호스트 없이 SFU.
        c.remove(&DeviceId([10; 32]));
        assert_eq!(topology(&c, false), (Topology::Sfu, None, None));
        // 나간 기기는 시청 관계에서도 빠진다.
        set_watching(&mut c, DeviceId([5; 32]), DeviceId([9; 32]), true);
        c.remove(&DeviceId([9; 32]));
        assert!(
            c.viewers
                .get(&DeviceId([5; 32]))
                .is_none_or(|v| v.is_empty())
        );
    }

    #[test]
    fn rejoin_needs_a_known_different_session() {
        assert!(is_rejoin(Some(1), 2));
        assert!(!is_rejoin(Some(1), 1), "다시 알림(재연결)");
        assert!(!is_rejoin(None, 2), "처음 참가");
        assert!(!is_rejoin(Some(0), 2), "예전 빌드의 참가");
        assert!(!is_rejoin(Some(1), 0), "예전 빌드의 알림");
    }

    fn part(i: u8) -> Part {
        Part {
            user: UserId([i; 32]),
            peer: None,
            muted: false,
            deafened: false,
            screen: false,
            session: 0,
        }
    }

    fn key(issuer: u8, generation: u64) -> MediaKey {
        MediaKey {
            generation,
            issuer: DeviceId([issuer; 32]),
            key: Zeroizing::new([issuer; 32]),
        }
    }

    #[test]
    fn generation_claims_are_bounded() {
        let mut c = Call::new(CallId::random());
        c.note_claim(u64::MAX);
        c.note_claim(CLAIM_LIMIT + 1);
        assert_eq!(c.next_generation(), 1, "상한을 넘는 알림은 무시");
        c.note_claim(CLAIM_LIMIT);
        assert_eq!(c.next_generation(), CLAIM_LIMIT + 1);
        // 받아들인 키의 세대가 상한 근처여도 다음 세대는 상한 안에 머뭅니다.
        c.install(key(2, GEN_LIMIT));
        assert!(c.next_generation() <= GEN_LIMIT);
        c.issued = u64::MAX;
        assert_eq!(c.next_generation(), u64::MAX, "넘치지 않음");
    }

    #[test]
    fn keys_from_non_manager_wait_until_it_manages() {
        let me = DeviceId([9; 32]);
        let mut c = Call::new(CallId::random());
        for i in [2u8, 5, 9] {
            c.parts.insert(DeviceId([i; 32]), part(i));
        }
        c.me_in = true;
        c.install(key(2, 3));
        // 5가 미리 보낸 키는 쓰지 않고 두었다가, 2가 빠져 5가 관리 기기가 되면 씁니다.
        c.pending.insert(DeviceId([5; 32]), key(5, 7));
        assert!(!c.adopt_pending(&me));
        assert_eq!(c.key.as_ref().map(|k| k.issuer), Some(DeviceId([2; 32])));
        c.remove(&DeviceId([2; 32]));
        assert!(c.adopt_pending(&me));
        assert_eq!(c.generation(), 7);
        // 같은 관리 기기의 낡은 세대는 받지 않고, 빠진 기기의 기록은 지웁니다.
        assert!(!c.accepts_from(&DeviceId([5; 32]), 6));
        assert!(c.accepts_from(&DeviceId([5; 32]), 8));
        assert!(!c.seen.contains_key(&DeviceId([2; 32])));
        // 내가 관리 기기가 되면 남이 보낸 키를 쓰지 않습니다.
        c.pending.insert(DeviceId([12; 32]), key(12, 20));
        c.remove(&DeviceId([5; 32]));
        assert!(!c.adopt_pending(&me));
        assert_eq!(c.generation(), 7);
    }

    #[test]
    fn app_cannot_send_control_payloads() {
        assert!(validate_app_payload(&SignalPayload::Leave).is_err());
        assert!(
            validate_app_payload(&SignalPayload::MediaKey {
                generation: 1,
                sealed: vec![]
            })
            .is_err()
        );
        assert!(
            validate_app_payload(&SignalPayload::Offer {
                sdp: "x".repeat(MAX_SDP + 1)
            })
            .is_err()
        );
        assert!(validate_app_payload(&SignalPayload::Offer { sdp: "v=0".into() }).is_ok());
    }

    fn caps(preferred: bool, public: bool, relayed: u16, rtt: Option<u32>) -> SfuCaps {
        SfuCaps {
            preferred,
            public,
            relayed,
            rtt_ms: rtt,
            capacity: 25,
        }
    }

    const T: Thresholds = Thresholds {
        mesh_max: MESH_MAX,
        mesh_min: 4,
    };

    #[test]
    fn election_prefers_opted_in_then_connectivity_then_device_id() {
        let d = |i: u8| DeviceId([i; 32]);
        let plain = caps(false, false, 0, Some(20));
        // 같은 조건이면 기기 ID가 가장 작은 쪽(어느 기기가 정해도 같은 답).
        let cands = [(d(9), plain), (d(3), plain), (d(5), plain)];
        assert_eq!(decide(7, None, &cands, T), Some(d(3)));
        // 먼저 맡겠다고 한 노드(헤드리스 시더)가 연결이 나빠도 먼저.
        let cands = [
            (d(3), caps(false, true, 0, Some(5))),
            (d(9), caps(true, false, 2, None)),
        ];
        assert_eq!(decide(7, None, &cands, T), Some(d(9)));
        // 공인 주소 > 릴레이로만 닿는 참가자가 적음 > RTT 묶음(50ms).
        let cands = [
            (d(1), caps(false, false, 0, Some(10))),
            (d(2), caps(false, true, 3, Some(300))),
        ];
        assert_eq!(decide(7, None, &cands, T), Some(d(2)));
        let cands = [
            (d(1), caps(false, false, 2, Some(10))),
            (d(2), caps(false, false, 1, Some(300))),
        ];
        assert_eq!(decide(7, None, &cands, T), Some(d(2)));
        let cands = [
            (d(1), caps(false, false, 0, Some(120))),
            (d(2), caps(false, false, 0, Some(40))),
            (d(3), caps(false, false, 0, None)),
        ];
        assert_eq!(decide(7, None, &cands, T), Some(d(2)));
        // 같은 RTT 묶음(0~49ms)이면 기기 ID.
        let cands = [
            (d(4), caps(false, false, 0, Some(10))),
            (d(2), caps(false, false, 0, Some(45))),
        ];
        assert_eq!(decide(7, None, &cands, T), Some(d(2)));
        // 수용 인원이 모자란 후보는 고르지 않고, 맡을 후보가 없으면 메시로 둡니다.
        let small = SfuCaps {
            capacity: 6,
            ..caps(true, true, 0, Some(1))
        };
        let cands = [(d(1), small), (d(2), plain)];
        assert_eq!(decide(7, None, &cands, T), Some(d(2)));
        assert_eq!(decide(7, None, &[(d(1), small)], T), None);
        assert_eq!(decide(7, None, &[], T), None);
    }

    #[test]
    fn hysteresis_around_mesh_max_and_sticky_host() {
        let d = |i: u8| DeviceId([i; 32]);
        let cands = [
            (d(1), caps(false, false, 0, Some(10))),
            (d(2), caps(false, false, 0, Some(10))),
        ];
        // 메시: MESH_MAX(6)까지는 메시, 넘으면 SFU.
        for n in 1..=MESH_MAX {
            assert_eq!(decide(n, None, &cands, T), None, "{n}명은 메시");
        }
        assert_eq!(decide(MESH_MAX + 1, None, &cands, T), Some(d(1)));
        // SFU: mesh_min(4) 이하로 줄어야 메시로(5~6명은 지금 토폴로지 유지).
        for n in [7, 6, 5] {
            assert_eq!(
                decide(n, Some(d(1)), &cands, T),
                Some(d(1)),
                "{n}명은 SFU 유지"
            );
        }
        assert_eq!(decide(4, Some(d(1)), &cands, T), None);
        // 한 명씩 오르내려도 경계에서 깜빡이지 않음: 7 → 6 → 7 → 5 → 6 → 4 → 5.
        let mut host = None;
        let mut flips = 0;
        for n in [7, 6, 7, 5, 6, 4, 5] {
            let next = decide(n, host, &cands, T);
            if next.is_some() != host.is_some() {
                flips += 1;
            }
            host = next;
        }
        assert_eq!(flips, 2, "SFU로 한 번, 메시로 한 번");
        // 지금 호스트보다 연결이 나은 후보가 와도 바꾸지 않음(넘겨주기는 비쌈).
        let better = [
            (d(2), caps(false, false, 0, Some(10))),
            (d(1), caps(false, true, 0, Some(1))),
        ];
        assert_eq!(decide(8, Some(d(2)), &better, T), Some(d(2)));
        // 호스트가 빠지거나 맡을 수 없게 되면(조건 없음) 다음 후보로 넘겨줌.
        assert_eq!(decide(8, Some(d(3)), &better, T), Some(d(1)));
        // 먼저 맡겠다는 후보(시더)가 늦게 나타나면 그쪽으로 한 번 넘기고, 그 뒤로는 다른 시더가 와도 유지.
        let seeders = [
            (d(2), caps(false, false, 0, Some(10))),
            (d(5), caps(true, false, 1, Some(80))),
            (d(4), caps(true, true, 0, Some(1))),
        ];
        assert_eq!(decide(8, Some(d(2)), &seeders, T), Some(d(4)));
        assert_eq!(decide(8, Some(d(5)), &seeders, T), Some(d(5)));
        // 수용 인원이 모자란 시더는 지금 호스트를 밀어내지 않음.
        let small_seeder = [
            (d(2), caps(false, false, 0, Some(10))),
            (
                d(4),
                SfuCaps {
                    capacity: 7,
                    ..caps(true, true, 0, Some(1))
                },
            ),
        ];
        assert_eq!(decide(8, Some(d(2)), &small_seeder, T), Some(d(2)));
        let full = SfuCaps {
            capacity: 7,
            ..caps(false, false, 0, Some(10))
        };
        assert_eq!(
            decide(
                8,
                Some(d(2)),
                &[(d(2), full), (d(1), caps(false, false, 0, None))],
                T
            ),
            Some(d(1))
        );
        // 잘못된 설정(mesh_min > mesh_max)도 깜빡이지 않게 mesh_max로 묶음.
        let odd = Thresholds {
            mesh_max: 3,
            mesh_min: 9,
        };
        assert_eq!(decide(4, None, &cands, odd), Some(d(1)));
        assert_eq!(decide(4, Some(d(1)), &cands, odd), Some(d(1)));
        assert_eq!(decide(3, Some(d(1)), &cands, odd), None);
    }

    #[test]
    fn plans_only_from_manager_and_pending_adopted_on_handover() {
        let me = DeviceId([9; 32]);
        let mut c = Call::new(CallId::random());
        for i in [2u8, 5, 9] {
            c.parts.insert(DeviceId([i; 32]), part(i));
        }
        c.me_in = true;
        let plan = |seq, h: Option<u8>| Plan {
            seq,
            host: h.map(|i| DeviceId([i; 32])),
        };
        // 관리 기기(2)가 아닌 5의 결정은 따로 둠.
        assert!(!c.accept_plan(DeviceId([5; 32]), plan(3, Some(5)), &me));
        assert_eq!(c.plan.host, None);
        assert!(c.accept_plan(DeviceId([2; 32]), plan(1, Some(2)), &me));
        // 같은 관리 기기의 늦게 온 옛 결정은 버림.
        assert!(!c.accept_plan(DeviceId([2; 32]), plan(1, None), &me));
        assert_eq!(c.plan.host, Some(DeviceId([2; 32])));
        // 2가 빠지면 5가 관리 기기: 미리 보낸 결정을 따름.
        c.remove(&DeviceId([2; 32]));
        assert!(c.adopt_plan_pending(&me));
        assert_eq!(c.plan, plan(3, Some(5)));
        // 결정에 나온 모르는 호스트(참가하지 않는 호스트 노드)는 있는 것으로 봄.
        assert!(c.accept_plan(DeviceId([5; 32]), plan(4, Some(40)), &me));
        assert_eq!(
            topology(&c, false),
            (Topology::Sfu, Some(DeviceId([40; 32])), None)
        );
        // 공간 상태로 찾은 그 기기의 사용자는 보여 주기만 함(통화 밖 호스트 자격 검사에는 쓰지 않음).
        c.hosts.get_mut(&DeviceId([40; 32])).expect("호스트").owner = Some(UserId([40; 32]));
        assert_eq!(
            topology(&c, false),
            (
                Topology::Sfu,
                Some(DeviceId([40; 32])),
                Some(UserId([40; 32]))
            )
        );
        // 나가면 결정 상태를 비움.
        c.reset_plan(&me);
        assert_eq!(c.plan, Plan::default());
        assert!(c.plan_seen.is_empty());
        // 막 관리 기기가 된 새 참가자는 다른 참가자가 알려 준 결정을 실마리로 씀.
        c.plan_pending.insert(DeviceId([5; 32]), plan(7, Some(5)));
        assert_eq!(c.plan_hint(), Some(DeviceId([5; 32])));
    }

    #[test]
    fn caps_comparison_ignores_small_rtt_changes() {
        let a = caps(false, true, 0, Some(12));
        assert!(same_caps(Some(a), Some(caps(false, true, 0, Some(40)))));
        assert!(!same_caps(Some(a), Some(caps(false, true, 0, Some(60)))));
        assert!(!same_caps(Some(a), Some(caps(false, true, 1, Some(12)))));
        assert!(!same_caps(Some(a), None));
        assert!(same_caps(None, None));
    }

    #[test]
    fn app_cannot_send_sfu_control_payloads() {
        assert!(validate_app_payload(&SignalPayload::SfuPlan { seq: 1, host: None }).is_err());
        assert!(validate_app_payload(&SignalPayload::SfuHost { caps: None }).is_err());
        assert!(
            validate_app_payload(&SignalPayload::SfuOffer {
                session: 1,
                sdp: "x".repeat(MAX_SDP + 1),
                tracks: Vec::new(),
            })
            .is_err()
        );
        let many = (0..=MAX_SFU_TRACKS)
            .map(|i| yangpa_core::wire::SfuTrack {
                mid: i.to_string(),
                device: None,
                source: yangpa_core::wire::TrackSource::Audio,
            })
            .collect();
        assert!(
            validate_app_payload(&SignalPayload::SfuOffer {
                session: 1,
                sdp: "v=0".into(),
                tracks: many,
            })
            .is_err()
        );
        assert!(
            validate_app_payload(&SignalPayload::SfuSelect {
                device: DeviceId([1; 32]),
                source: yangpa_core::wire::TrackSource::Screen,
                layer: yangpa_core::wire::SfuLayer::Low,
            })
            .is_ok()
        );
        assert!(is_sfu_media(&SignalPayload::SfuClose { session: 1 }));
        assert!(!is_sfu_media(&SignalPayload::Offer { sdp: "v=0".into() }));
    }

    /// 다른 데몬이 보낸 미디어 신호에도 앱 신호와 같은 크기 한도를 씁니다(`handle_signal`이 엔진·앱에 넘기기 전에).
    #[test]
    fn inbound_media_payloads_are_bounded() {
        let big = "x".repeat(MAX_SDP + 1);
        for p in [
            SignalPayload::SfuOffer {
                session: 1,
                sdp: big.clone(),
                tracks: Vec::new(),
            },
            SignalPayload::SfuAnswer {
                session: 1,
                sdp: big.clone(),
                tracks: Vec::new(),
            },
            SignalPayload::SfuIce {
                session: 1,
                candidate: "c".repeat(MAX_CANDIDATE + 1),
                sdp_mid: None,
                sdp_mline_index: None,
            },
            SignalPayload::Offer { sdp: big.clone() },
            SignalPayload::Ice {
                candidate: "c".repeat(MAX_CANDIDATE + 1),
                sdp_mid: None,
                sdp_mline_index: None,
            },
        ] {
            assert!(!media_payload_ok(&p), "{p:?}");
        }
        assert!(media_payload_ok(&SignalPayload::SfuOffer {
            session: 1,
            sdp: "v=0".into(),
            tracks: Vec::new(),
        }));
        assert!(media_payload_ok(&SignalPayload::Leave));
        assert!(media_payload_ok(&SignalPayload::SfuPlan {
            seq: 1,
            host: None
        }));
    }

    #[test]
    fn media_ad_binds_generation_and_sender() {
        let s = SpaceId::random();
        let c = ChannelId::random();
        let d = DeviceId([1; 32]);
        assert_ne!(media_ad(&s, &c, 1, &d), media_ad(&s, &c, 2, &d));
        assert_ne!(
            media_ad(&s, &c, 1, &d),
            media_ad(&s, &c, 1, &DeviceId([2; 32]))
        );
    }
}
