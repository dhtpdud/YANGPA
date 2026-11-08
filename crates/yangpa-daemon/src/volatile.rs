//! 휘발성 채널(B2): RAM에서만 사는 메시지·세션 키·참가자, 늦은 입장자 스냅숏, API 읽기 허용.
//!
//! 이 모듈의 데이터(암호문·세션 키·참가자·읽음 위치)는 디스크·로그·캐시로 가지 않습니다. 암호문과 세션 키는
//! [`VolatileStore`](직렬화 불가, 해제 때 0으로 덮어씀, 키는 잠긴 메모리)에, 나머지는 [`VolatileState`]에 둡니다.
//! 디스크에 남는 것은 `channel.allow_api_read` 허용 표시(공간·채널 ID뿐)입니다.
//!
//! 흐름(docs/daemon.md "휘발성 채널", docs/protocol.md "휘발성 채널"):
//! - 참가자: 입장·퇴장 방식이면 `channel.join` 하고 `VolatilePresence`를 주고받은 인증된 기기, 상시면 채널을 볼 수
//!   있는 인증된 온라인 멤버 기기(1초마다 다시 셈). 둘 다 이 기기(참가 중이면)를 포함합니다.
//! - 세션 키: 참가자가 바뀌면 참가자 중 기기 ID가 가장 작은 기기(갱신 담당)가 무작위 64비트 세대의 새 키를
//!   참가자에게 봉인한 `KeyShare`로 직접 보냅니다. 보낼 때 쓰는 키는 갱신 담당의 최신 갱신이며, 그 키를 받은
//!   기기가 모두 지금 참가자일 때만 씁니다(떠난 기기가 가진 키로 새 메시지를 암호화하지 않음). 보내기가 실패하면
//!   그 갱신이 가장 새 것이고 그 기기가 같은 연결로 참가 중인 동안 간격을 늘려 가며 다시 보냅니다(모든 참가자가 같은
//!   키로 모임). 그래도 닿지 않는 기기는 연결이 끊기면 1초 주기 작업이 참가자에서 빼고 새 갱신이 나갑니다.
//! - 이벤트는 참가자에게 `PushEvents`로만 보냅니다(gossip·시더 따라잡기로 가지 않음). 받을 때도 작성 기기가
//!   지금 참가자인 것만 받고, 입장 알림보다 먼저 온 것은 잠깐 RAM에 두었다가 알림이 오면 다시 적용합니다.
//! - 수명은 이 기기가 받은 시점 기준. 1초 주기 작업이 만료를 지우고 `message.expired`를 알립니다.
//! - 퇴장하면 내 기기의 그 채널 내용·키를 바로 지우고, 종료하면 전부 지웁니다.
//!
//! - 첨부(X1, [`attach`]): 암호문은 RAM 저장소에만 두고 실은 메시지에 묶어 함께 지웁니다. 받기·내주기는 지금
//!   참가자끼리 일반 블롭 요청으로 합니다.
//!
//! 코어가 부르는 고리: [`on_inbound_event`](적용 경로가 저장하지 않고 넘김, 적용 잠금 안이라 동기·짧게),
//! 메시지 API([`send_message`] …), [`handle_snapshot`]·[`handle_presence`], [`filter_notification`],
//! [`fill_channel_view`], [`mark_read`], [`on_peer_disconnected`], [`start`], 블롭 요청·API([`attach::serve`],
//! [`attach::find`] …).

pub(crate) mod attach;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::debug;
use yangpa_core::body::{
    EventBody, KeyShareBody, MessageBody, Secret32, SharedKey, VolatileRotationBody,
};
use yangpa_core::device::BoxPublicKey;
use yangpa_core::event::{EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::BlobId;
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelSettings, ChatTtl, HistoryVisibility, JoinMode, Permissions, SpaceKind, SpaceState,
};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    MAX_EVENTS_PER_MESSAGE, VolatileItem, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_crypto::{CryptoError, decrypt_body, open_sealed_body};
use yangpa_net::PeerId;
use yangpa_rpc::api::{
    ANONYMOUS_USER, ChannelView, Empty, EventRef, MessageEditParams, MessageListParams,
    MessageListResult, MessageRefParams, MessageSendParams, MessageView, Notification, NotifyView,
    ReactParams, ReactionView, default_notification_filter, m,
};
use yangpa_rpc::{AuthInfo, RpcError};
use yangpa_store::{StoreError, VolatileLimits, VolatileStats, VolatileStore};
use yangpa_sync::authorize_message;
use zeroize::Zeroize;

use crate::api::{self, CallCtx, R};
use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::identity::Me;
use crate::ingest::LAMPORT_SLACK;
use crate::node::Node;
use crate::{admin, messages, peers};

/// 만료·참가자 점검 주기.
const TICK: Duration = Duration::from_secs(1);
/// 나감·추방·권한 잃음으로 새 무작위 키를 내는 최소 간격(그 사이 바뀐 참가자는 다음 회전에 묶음).
const FRESH_EVERY: Duration = Duration::from_secs(1);
/// 채널마다 기다리는 회전 공지·감싸기 세대 수, 세대마다 감싸기 수, 감싸기 하나의 바이트 상한.
const MAX_PENDING_ROTATIONS: usize = 64;
const MAX_WRAPS_PER_GEN: usize = 4;
const MAX_WRAP_BYTES: usize = 4096;
/// 세대 받는 기기 기록을 두는 세대 수(키가 지워진 세대는 정리 때 빠짐).
const MAX_GEN_RECIPIENTS: usize = 256;
/// 담당이 아닌 기기가 담당의 키 없이 이만큼 지나면 새 키를 청합니다(가입 회전 왕복보다 넉넉히).
const STALE_AFTER: Duration = Duration::from_secs(3);
/// 보낼 때 세션 키를 기다리는 최대 시간. 절반이 지나도 없으면 이 기기가 직접 갱신합니다.
const KEY_WAIT: Duration = Duration::from_secs(10);
/// 지금 참가자 일부만 가진 키(막 들어온 참가자가 없는 키)로 보내기 전에 새 갱신을 기다리는 시간.
const KEY_SETTLE: Duration = Duration::from_secs(2);
/// 지운·만료된 이벤트를 기억하는 시간과 개수(늦게 온 사본·스냅숏이 되살리지 않게).
const TOMBSTONE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_TOMBSTONES: usize = 65_536;
/// 키·대상을 기다리는 이벤트를 버리기 전 시간.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// 입장 알림보다 먼저 온 이벤트를 두는 시간·개수·바이트(그 기기의 입장 알림이 다시 오기를 기다림).
const EARLY_TTL: Duration = Duration::from_secs(30);
const MAX_EARLY_EVENTS: usize = 256;
const MAX_EARLY_BYTES: usize = 4 * 1024 * 1024;
/// 스냅숏 응답에 싣는 암호문 바이트 상한(전선 메시지 8MiB 안).
const SNAPSHOT_MAX_BYTES: usize = 6 * 1024 * 1024;
/// 스냅숏을 동시에 요청하는 참가자 수(받은 응답을 RAM에 쌓아 두는 양을 묶음).
const SNAPSHOT_PARALLEL: usize = 4;
/// 세션 키 갱신을 다시 보내는 횟수와 간격(처음 간격부터 두 배, 상한까지).
const KEY_RETRIES: u32 = 6;
const KEY_RETRY_START: Duration = Duration::from_millis(200);
const KEY_RETRY_MAX: Duration = Duration::from_secs(5);
/// 휘발성 이벤트의 논리 시계가 저장된 공간 최댓값보다 앞설 수 있는 폭. 휘발성 이벤트는 저장되지 않아 공간
/// 최댓값을 올리지 않으므로, 앞선 폭을 적용 경로의 부풀리기 방어(`LAMPORT_SLACK`)보다 작게 묶어 받는 쪽이
/// 보류하지 않게 합니다.
const VOLATILE_LAMPORT_LEAD: u64 = LAMPORT_SLACK / 2;
/// 휘발성 알림 제목(작성자 이름도 남기지 않음).
const NOTIFY_TITLE: &str = "새 메시지";
/// API 읽기 허용 설정 키 접두어(`volatile.api_read.<공간>.<채널>`).
const API_READ_PREFIX: &str = "volatile.api_read.";

type ChanKey = (SpaceId, ChannelId);
/// 보기 순서: 논리 시계, 작성 시각, ID. 휘발성 논리 시계는 상한([`VOLATILE_LAMPORT_LEAD`])에 묶이면 같은 값이
/// 이어질 수 있어 작성 시각으로 가릅니다.
type Pos = (u64, Timestamp, EventId);

/// 휘발성 영역의 노드 수준 상태(`Node::volatile`). RAM 전용.
#[derive(Default)]
pub struct VolatileState {
    store: VolatileStore,
    chans: Mutex<HashMap<ChanKey, ChanRt>>,
    /// 자동화 토큰 읽기를 허용한 채널(설정에도 저장).
    api_read: Mutex<HashSet<ChanKey>>,
    /// 세션 키가 바뀌면 깨움(보내려고 키를 기다리는 호출).
    key_arrived: tokio::sync::Notify,
    /// 시험용 고장 주입(`testkit::volatile`). 시험이 아니면 늘 비어 있습니다.
    faults: Mutex<Faults>,
    /// 첨부 받기 상태(RAM 전용, 내용 없음).
    attach: attach::AttachState,
}

/// 시험에서 일부러 실패시키는 노드 간 요청.
#[derive(Default)]
struct Faults {
    /// 이 피어로 보내는 입장 알림을 이만큼 보내지 않고 실패로 처리.
    presence_to: HashMap<PeerId, u32>,
    /// 스냅숏 받아 오기를 이만큼 건너뜀.
    snapshot: u32,
    /// 이 피어로 보내는 세션 키 갱신을 이만큼 보내지 않고 실패로 처리.
    key_to: HashMap<PeerId, u32>,
    /// 받아 둔 감싸기 수(시험이 가입 회전에 감싸기가 얼마나 오가는지 셈).
    wraps_in: u64,
}

fn take_fault(slot: Option<&mut u32>) -> bool {
    match slot {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// 받은 휘발성 이벤트의 판정.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VolatileVerdict {
    Accepted,
    Rejected,
    Ignored,
}

/// 채널 하나의 RAM 상태. 이 기기가 참가하는 동안만 있습니다.
#[derive(Default)]
struct ChanRt {
    mode: JoinMode,
    /// 입장·퇴장 방식에서 `channel.join` 했는지.
    joined: bool,
    /// 상시 채널 참가 상한(`SpaceState::volatile_cap`)에 밀려 참가하지 못하면 그 상한.
    full: Option<u32>,
    /// 다른 참가 기기 → (사용자, 피어).
    present: BTreeMap<DeviceId, (UserId, PeerId)>,
    /// 입장을 알린 기기(입장·퇴장 방식). 알림이 실패하거나 다시 연결되면 빠져 1초 주기 작업이 다시 알립니다.
    announced: BTreeMap<DeviceId, PeerId>,
    /// 기기별 가장 새 세션 키 갱신.
    rotations: HashMap<DeviceId, Rotation>,
    /// 보낼 때 쓰는 세대.
    current: Option<Current>,
    /// 마지막으로 내가 갱신했을 때의 참가자.
    rotated_for: Option<BTreeSet<DeviceId>>,
    /// 키를 받은 세대(시험·정리용).
    known_gens: BTreeSet<u64>,
    /// 세대마다 그 키를 받은 기기(회전 공지의 더한·뺀 목록을 이 위에 셈).
    gen_recipients: HashMap<u64, BTreeSet<DeviceId>>,
    /// 공지보다 먼저 왔거나 공지를 기다리는 감싸기(세대 → 받은 것들, 봉인 암호문 그대로).
    wraps: HashMap<u64, Vec<WrapIn>>,
    /// 키(감싸기·부모 세대 키)를 기다리는 회전 공지.
    pending_rot: HashMap<u64, PendingRot>,
    /// 내가 마지막으로 새 무작위 키를 낸 때(나감·추방은 1초에 한 번으로 묶음).
    last_fresh: Option<Instant>,
    /// 갱신 담당인데 담당이 아닌 참가자가 갱신을 냄(그 기기가 내 키를 잃음, 예: 빨리 다시 시작함): 새 무작위 키를 냅니다.
    need_fresh: bool,
    /// 담당이 아닌데 담당의 키를 갖지 못한 때부터(오래가면 담당에게 새 키를 청함).
    stale_since: Option<Instant>,
    meta: HashMap<EventId, Meta>,
    /// 대상 메시지 → 그 메시지를 고친·반응한 이벤트(보기 순서).
    related: HashMap<EventId, BTreeSet<Pos>>,
    pending_key: HashMap<u64, Vec<EventId>>,
    pending_target: HashMap<EventId, Vec<EventId>>,
    gone: HashMap<EventId, Instant>,
    read_marker: Option<Pos>,
    /// 받아들인 가장 최근 이벤트(다음 이벤트의 부모, 논리 시계가 앞으로 가게). 확인을 마친 것만 반영합니다.
    head: Option<(u64, EventId)>,
    /// 입장·퇴장 방식에서 아직 참가자로 모르는 기기가 보낸 이벤트(입장 알림보다 먼저 온 갱신 키·메시지).
    /// 그 기기의 입장을 받으면 다시 처리하고, [`EARLY_TTL`]이 지나면 버립니다.
    early: VecDeque<Early>,
    early_bytes: usize,
    /// 나간 것을 본 기기 → 그 시각. 다시 입장을 알릴 때까지 그 기기의 늦게 온 이벤트는 잠깐 두지도 않습니다
    /// (퇴장 전에 보낸 메시지가 재입장 뒤 되살아나지 않게). [`EARLY_TTL`]이 지나면 잊습니다.
    departed: HashMap<DeviceId, Instant>,
    rekey_running: bool,
    rekey_again: bool,
    rekey_force: bool,
    /// 보기에 있는 메시지의 첨부(블롭 → 실은 메시지).
    attach: attach::Index,
}

struct Rotation {
    order: (u64, Timestamp, EventId),
    generation: u64,
    /// 봉인한 기기(이 키를 가진 기기).
    recipients: BTreeSet<DeviceId>,
}

/// 받은 감싸기(봉인 암호문, RAM 전용).
struct WrapIn {
    at: Instant,
    /// 보낸 기기. 공지를 쓴 기기가 보낸 감싸기만 씁니다(다른 참가자가 끼워 넣은 감싸기로 자리를 밀어내지 못하게).
    from: DeviceId,
    wrap: Vec<u8>,
    recipients: Option<Vec<DeviceId>>,
}

/// 키를 기다리는 회전 공지(약속값·목록만, 키 없음).
#[derive(Clone)]
struct PendingRot {
    at: Instant,
    author: DeviceId,
    lamport: u64,
    id: EventId,
    order: (u64, Timestamp, EventId),
    body: VolatileRotationBody,
}

/// 회전 공지를 적용해 본 결과.
enum RotStep {
    Applied,
    Wait,
    Reject,
}

/// 잠깐 둔 이벤트(암호문 그대로). 버릴 때 본문을 0으로 덮어씁니다.
struct Early {
    at: Instant,
    size: usize,
    ev: Option<SignedEvent>,
}

impl Drop for Early {
    fn drop(&mut self) {
        if let Some(b) = self.ev.as_mut().and_then(|e| e.body.as_mut()) {
            b.zeroize();
        }
    }
}

impl Early {
    fn author(&self) -> Option<DeviceId> {
        self.ev.as_ref().map(|e| e.header.author)
    }
}

#[derive(Clone)]
struct Current {
    generation: u64,
    recipients: BTreeSet<DeviceId>,
}

/// 이벤트 머리 정보(내용 없음).
struct Meta {
    /// 본문 종류(익명 글도 `Message`).
    kind: EventKind,
    /// 익명 채널에서 쓴 글(헤더 종류 `AnonMessage`).
    anonymous: bool,
    lamport: u64,
    user: UserId,
    device: DeviceId,
    created_at: Timestamp,
    generation: u64,
    target: Option<EventId>,
    /// 풀고 권한을 확인했는지(보기·스냅숏에는 이것만).
    ready: bool,
    /// 스냅숏으로 받은 것(새 메시지 알림을 내지 않음).
    quiet: bool,
    /// 나를 멘션한 글(`messages::mentions_me`, 풀고 권한을 확인할 때 정함).
    mentions_me: bool,
    since: Instant,
}

impl Meta {
    fn pos(&self, id: EventId) -> Pos {
        (self.lamport, self.created_at, id)
    }
}

impl ChanRt {
    fn new(mode: JoinMode) -> Self {
        ChanRt {
            mode,
            ..Default::default()
        }
    }

    fn participating(&self) -> bool {
        (self.mode == JoinMode::Always && self.full.is_none()) || self.joined
    }

    /// 이 기기에서 본 참가 기기(참가 중이면 나 포함).
    fn participants(&self, me: DeviceId) -> BTreeSet<DeviceId> {
        let mut s: BTreeSet<DeviceId> = self.present.keys().copied().collect();
        if self.participating() {
            s.insert(me);
        }
        s
    }

    /// 지금 보내는 키가 갱신 담당(참가자 중 기기 ID가 가장 작은 기기)의 가장 새 갱신인지. 내가 담당이면 참.
    fn keyed_by_rotator(&self, me: DeviceId) -> bool {
        let parts = self.participants(me);
        match parts.first() {
            None => true,
            Some(r) if *r == me => true,
            Some(r) => self
                .rotations
                .get(r)
                .zip(self.current.as_ref())
                .is_some_and(|(rot, cur)| rot.generation == cur.generation),
        }
    }

    fn peers(&self) -> Vec<PeerId> {
        let mut v: Vec<PeerId> = self.present.values().map(|(_, p)| *p).collect();
        v.sort();
        v.dedup();
        v
    }

    fn bump_head(&mut self, lamport: u64, id: EventId) {
        if self.head.is_none_or(|h| h < (lamport, id)) {
            self.head = Some((lamport, id));
        }
    }

    fn ready_message(&self, id: &EventId) -> Option<&Meta> {
        self.meta
            .get(id)
            .filter(|m| m.ready && m.kind == EventKind::Message)
    }

    /// 아직 참가자로 모르는 기기의 이벤트를 잠깐 둡니다(가득 차면 오래된 것부터 버림).
    fn hold_early(&mut self, ev: SignedEvent, now: Instant) {
        let size = ev.body.as_ref().map_or(0, Vec::len) + 512;
        if size > MAX_EARLY_BYTES {
            return;
        }
        while self.early.len() >= MAX_EARLY_EVENTS || self.early_bytes + size > MAX_EARLY_BYTES {
            let Some(old) = self.early.pop_front() else {
                break;
            };
            self.early_bytes = self.early_bytes.saturating_sub(old.size);
        }
        self.early_bytes += size;
        self.early.push_back(Early {
            at: now,
            size,
            ev: Some(ev),
        });
    }

    /// 기기가 나갔다고 보고 그 기기의 잠깐 둔 이벤트를 버립니다.
    fn mark_departed(&mut self, devices: &[DeviceId], now: Instant) {
        for d in devices {
            self.departed.insert(*d, now);
        }
        self.drop_early(|e| e.author().is_some_and(|a| devices.contains(&a)));
    }

    /// 조건에 맞는 잠깐 둔 이벤트를 버립니다(본문은 `Early`의 Drop이 덮어씀).
    fn drop_early(&mut self, mut pick: impl FnMut(&Early) -> bool) {
        let mut freed = 0usize;
        self.early.retain(|e| {
            let drop = pick(e);
            if drop {
                freed += e.size;
            }
            !drop
        });
        self.early_bytes = self.early_bytes.saturating_sub(freed);
    }

    /// 조건에 맞는 잠깐 둔 이벤트를 빼냅니다.
    fn take_early(&mut self, mut pick: impl FnMut(&Early) -> bool) -> Vec<SignedEvent> {
        let mut out = Vec::new();
        let mut keep = VecDeque::with_capacity(self.early.len());
        for mut e in self.early.drain(..) {
            if pick(&e) {
                self.early_bytes = self.early_bytes.saturating_sub(e.size);
                out.extend(e.ev.take());
            } else {
                keep.push_back(e);
            }
        }
        self.early = keep;
        out
    }
}

/// 잠금 밖에서 할 일(알림, 키 갱신, 스냅숏 요청, 입장 알림).
#[derive(Default)]
struct Effects {
    notes: Vec<Notification>,
    rekey: BTreeSet<ChanKey>,
    /// 갱신 담당의 키가 없어 담당에게 새 키를 청하는 채널(내가 새 무작위 키로 갱신).
    rekey_force: BTreeSet<ChanKey>,
    snapshots: Vec<(ChanKey, Vec<PeerId>)>,
    presence: Vec<(ChanKey, DeviceId, PeerId)>,
    keys_changed: bool,
    /// 풀어 확인한 메시지의 첨부(자동 내려받기 판단).
    attach: Vec<attach::Ready>,
}

impl Effects {
    fn run(self, node: &Arc<Node>) {
        // 받기를 먼저 정해, 새 메시지 알림을 받은 쪽이 곧바로 `blob.status`를 물어도 받는 중으로 보이게 합니다.
        if !self.attach.is_empty() {
            attach::on_ready(node, self.attach);
        }
        // 메시지가 사라졌으면 그 첨부를 기다리던 요청을 첨부 대기열 일꾼이 바로 정리하게 깨웁니다.
        if self
            .notes
            .iter()
            .any(|n| matches!(n, Notification::MessageExpired { .. }))
        {
            attach::kick(node);
        }
        for mut n in self.notes {
            // 익명 채널(지금 익명이거나 익명으로 쓴 글)의 글쓴이를 가립니다.
            if let Notification::MessageNew {
                space,
                channel,
                message,
                ..
            }
            | Notification::MessageUpdated {
                space,
                channel,
                message,
            } = &mut n
            {
                let anonymous = messages::channel_anonymous(node, space, channel);
                messages::mask(message, space, channel, anonymous);
            }
            node.notify.publish(n);
        }
        if self.keys_changed {
            node.volatile.key_arrived.notify_waiters();
        }
        for k in self.rekey {
            request_rekey(node, k, false);
        }
        for k in self.rekey_force {
            request_rekey(node, k, true);
        }
        for ((space, channel), from) in self.snapshots {
            node.spawn_detached(move |node| async move {
                fetch_snapshot(&node, space, channel, from).await;
            });
        }
        for (key, device, peer) in self.presence {
            node.spawn_detached(move |node| async move {
                announce(&node, key, device, peer).await;
            });
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    Peer,
    /// 스냅숏(남은 수명, 응답한 참가자 기기).
    Snapshot {
        remaining: Option<Duration>,
        from: DeviceId,
    },
}

enum Outcome {
    Ready,
    Pending,
    Invalid,
    Moot,
    Gone,
}

// ───────────── 작은 도우미 ─────────────

fn can_view(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    state
        .channel_permissions(user, channel)
        .contains(Permissions::VIEW_CHANNEL)
}

fn member_device(state: &SpaceState, user: &UserId, device: &DeviceId) -> bool {
    state.is_member(user) && state.device_belongs(user, device)
}

fn ttl_of(settings: &ChannelSettings) -> Option<Duration> {
    match settings.ttl {
        ChatTtl::Seconds(n) => Some(Duration::from_secs(u64::from(n))),
        ChatTtl::None | ChatTtl::OnLeave => None,
    }
}

fn body_target(body: &EventBody) -> Option<EventId> {
    match body {
        EventBody::Edit { target, .. }
        | EventBody::Delete { target }
        | EventBody::Reaction { target, .. }
        | EventBody::ReactionRemove { target, .. } => Some(*target),
        _ => None,
    }
}

/// 다 쓴 평문을 덮어씁니다(가능한 범위에서). 첨부 키는 `Secret32`가 해제될 때 덮어씁니다.
fn scrub(body: &mut EventBody) {
    match body {
        EventBody::Message(m) => {
            m.text.zeroize();
            for a in &mut m.attachments {
                a.name.zeroize();
                a.thumbnail.zeroize();
            }
        }
        EventBody::Edit { text, .. } => text.zeroize(),
        _ => {}
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// 다음 휘발성 이벤트의 추가 부모(가장 최근 휘발성 이벤트). 부모의 논리 시계는 저장된 공간 최댓값(`known`)에
/// [`VOLATILE_LAMPORT_LEAD`]를 더한 값에서 자릅니다. 휘발성 이벤트가 많이 오가도(저장 이벤트가 없어도) 논리
/// 시계가 저장된 최댓값에서 한없이 멀어지지 않아, 받는 쪽 적용 경로가 부풀리기로 보고 보류하지 않습니다.
fn volatile_parent(head: Option<(u64, EventId)>, known: u64) -> Vec<(EventId, u64)> {
    head.map(|(l, id)| (id, l.min(known.saturating_add(VOLATILE_LAMPORT_LEAD))))
        .into_iter()
        .collect()
}

/// 적용 경로를 거치지 않는 항목(스냅숏)에 적용 경로와 같은 논리 시계 부풀리기 상한을 둡니다. 저장소를 읽지
/// 못하면 받지 않습니다.
fn lamport_too_far(node: &Node, space: &SpaceId, lamport: u64) -> bool {
    match node.store.max_lamport(space) {
        Ok(known) => known > 0 && lamport > known.saturating_add(LAMPORT_SLACK),
        Err(_) => true,
    }
}

/// gossip으로 오가면 안 되는(참가자에게 직접만 보내는) 이벤트인지. 세션 키로 암호화했거나 휘발성 채널 스트림의 것.
pub(crate) fn is_direct_only(state: &SpaceState, ev: &SignedEvent) -> bool {
    matches!(ev.header.key, KeyRef::Volatile { .. })
        || ev
            .header
            .channel
            .and_then(|c| state.channels.get(&c))
            .is_some_and(|c| c.is_volatile())
}

/// 채널을 볼 수 있는 멤버의, 지금 인증된 다른 기기. P2P 전용 채널이면 함께 참가해 P2P 스웜에서 인증된 기기만
/// (Tor 피어와는 휘발성 내용도 주고받지 않음), 아니면 Tor로 인증된 기기만.
fn online_viewers(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    me: DeviceId,
) -> BTreeMap<DeviceId, (UserId, PeerId)> {
    let (space, channel) = (&key.0, &key.1);
    let peers = if state.is_p2p(channel) {
        crate::p2p::channel_peers(node, space, state, channel)
    } else {
        node.peers.authed_peers()
    };
    peers
        .into_iter()
        .filter(|(_, u, d)| *d != me && member_device(state, u, d) && can_view(state, u, channel))
        .map(|(p, u, d)| (d, (u, p)))
        .collect()
}

// ───────────── 참가자·세션 키 ─────────────

/// 받은 이벤트의 작성 기기가 지금 참가자인지(입장·퇴장 방식은 입장을 주고받은 기기, 상시는 채널을 볼 수 있는
/// 온라인 멤버 기기). 퇴장 알림이 그 기기의 마지막 이벤트보다 먼저 와도 떠난 기기의 이벤트가 남지 않고, 들어오지
/// 않은(또는 나간) 기기가 참가자 몰래 쓰거나 키를 심지 못하게 합니다.
#[allow(clippy::too_many_arguments)]
fn author_present(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    settings: &ChannelSettings,
    rt: &mut ChanRt,
    me: DeviceId,
    user: &UserId,
    device: &DeviceId,
    fx: &mut Effects,
) -> bool {
    if *device == me {
        return true;
    }
    let here = |rt: &ChanRt| rt.present.get(device).is_some_and(|(u, _)| u == user);
    if here(rt) {
        return true;
    }
    if rt.mode == JoinMode::Always {
        // 상시 채널 참가자는 1초마다 다시 셉니다. 막 연결된 기기일 수 있으니 지금 다시 셉니다.
        refresh_always(node, state, key, settings, rt, me, false, fx);
        return here(rt);
    }
    false
}

/// 참가 중인 채널의 상태. 상시 채널이면 없을 때 만들고, 입장·퇴장 채널은 들어가 있을 때만.
fn live_rt<'a>(
    node: &Node,
    chans: &'a mut HashMap<ChanKey, ChanRt>,
    state: &SpaceState,
    key: ChanKey,
    settings: &ChannelSettings,
    me: DeviceId,
    fx: &mut Effects,
) -> Option<&'a mut ChanRt> {
    // P2P 전용 채널은 이 기기가 참가한 동안만 씁니다(참가하지 않으면 잠긴 채널).
    if !p2p_open(node, key, state) {
        if let Some(old) = chans.remove(&key) {
            wipe(node, key, old, fx);
        }
        return None;
    }
    if chans
        .get(&key)
        .is_some_and(|rt| rt.mode != settings.join_mode)
        && let Some(old) = chans.remove(&key)
    {
        wipe(node, key, old, fx);
    }
    if settings.join_mode == JoinMode::Always && !chans.contains_key(&key) {
        let mut rt = ChanRt::new(JoinMode::Always);
        refresh_always(node, state, key, settings, &mut rt, me, true, fx);
        chans.insert(key, rt);
    }
    chans.get_mut(&key).filter(|rt| rt.participating())
}

/// P2P 전용이 아니거나, P2P 전용이면 이 기기가 참가 중인지.
fn p2p_open(node: &Node, key: ChanKey, state: &SpaceState) -> bool {
    !state.is_p2p(&key.1) || crate::p2p::p2p_joined(node, &key.0, &key.1)
}

/// 상시 채널 참가자를 다시 셉니다.
#[allow(clippy::too_many_arguments)]
fn refresh_always(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    settings: &ChannelSettings,
    rt: &mut ChanRt,
    me: DeviceId,
    fresh: bool,
    fx: &mut Effects,
) {
    let mut now_present = online_viewers(node, state, key, me);
    let full = trim_to_cap(&mut now_present, me, state.volatile_cap(&key.1));
    if !fresh && now_present == rt.present && full == rt.full {
        return;
    }
    rt.full = full;
    let removed: Vec<DeviceId> = rt
        .present
        .keys()
        .filter(|d| !now_present.contains_key(d))
        .copied()
        .collect();
    let was_alone = rt.present.is_empty();
    rt.present = now_present;
    after_view_change(node, key, settings, rt, me, &removed, fx);
    // 혼자였다가 다른 참가자가 보이면(시작·네트워크 복구) 남은 메시지를 받아 옵니다.
    if settings.history == HistoryVisibility::Public && was_alone && !rt.present.is_empty() {
        fx.snapshots.push((key, rt.peers()));
    }
}

/// 상시 채널 참가 기기를 상한까지 줄입니다. 모든 기기가 같은 답을 내도록 기기 ID가 작은 순서로 고르고,
/// 이 기기가 밀려나면 그 상한을 돌려줍니다(그동안 보내기·받기를 하지 않음).
fn trim_to_cap(
    present: &mut BTreeMap<DeviceId, (UserId, PeerId)>,
    me: DeviceId,
    cap: u32,
) -> Option<u32> {
    let cap_n = cap as usize;
    if present.len() < cap_n {
        return None;
    }
    let mut all: Vec<DeviceId> = present.keys().copied().collect();
    all.push(me);
    all.sort();
    all.truncate(cap_n);
    present.retain(|d, _| all.binary_search(d).is_ok());
    (all.binary_search(&me).is_err()).then_some(cap)
}

/// 입장·퇴장 채널: 연결이 끊긴·권한을 잃은 참가자를 빼고, 아직 입장을 알리지 않은 온라인 기기에 알립니다.
fn refresh_join_leave(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    settings: &ChannelSettings,
    rt: &mut ChanRt,
    me: DeviceId,
    fx: &mut Effects,
) {
    let removed: Vec<DeviceId> = rt
        .present
        .iter()
        .filter(|(d, (u, p))| {
            node.peers.identity_of(p) != Some((*u, **d))
                || !member_device(state, u, d)
                || !can_view(state, u, &key.1)
        })
        .map(|(d, _)| *d)
        .collect();
    for d in &removed {
        rt.present.remove(d);
    }
    rt.mark_departed(&removed, Instant::now());
    let online = online_viewers(node, state, key, me);
    rt.announced
        .retain(|d, p| online.get(d).is_some_and(|(_, q)| q == p));
    for (d, (_, p)) in online {
        if let std::collections::btree_map::Entry::Vacant(e) = rt.announced.entry(d) {
            e.insert(p);
            fx.presence.push((key, d, p));
        }
    }
    if !removed.is_empty() {
        after_view_change(node, key, settings, rt, me, &removed, fx);
    }
}

/// 참가자가 바뀐 뒤: 떠난 기기의 갱신 기록(수명이 '퇴장 시'면 그 기기의 이벤트도)을 지우고, 보낼 키를 다시
/// 고르고, 내가 갱신 담당이면 갱신을 요청합니다.
fn after_view_change(
    node: &Node,
    key: ChanKey,
    settings: &ChannelSettings,
    rt: &mut ChanRt,
    me: DeviceId,
    removed: &[DeviceId],
    fx: &mut Effects,
) {
    for d in removed {
        rt.rotations.remove(d);
        if settings.ttl == ChatTtl::OnLeave {
            let ids: Vec<EventId> = rt
                .meta
                .iter()
                .filter(|(_, m)| m.device == *d)
                .map(|(id, _)| *id)
                .collect();
            let me_user = node.me().map(|m| m.user()).unwrap_or_default();
            remove_events(&node.volatile.store, key, rt, &ids, me_user, true, fx);
        }
    }
    reselect_current(&node.volatile.store, key, rt, me);
    fx.keys_changed = true;
    let parts = rt.participants(me);
    if rt.participating() && parts.first() == Some(&me) {
        if rt.rotated_for.as_ref() != Some(&parts) {
            fx.rekey.insert(key);
        }
    } else {
        rt.rotated_for = None;
    }
}

/// 보낼 세대를 고릅니다: 갱신 담당의 최신 갱신, 없으면 지금 키. 둘 다 그 키를 가진 기기가 모두 지금 참가자여야
/// 합니다(떠난 기기가 가진 키는 쓰지 않음).
fn reselect_current(store: &VolatileStore, key: ChanKey, rt: &mut ChanRt, me: DeviceId) {
    if !rt.participating() {
        rt.current = None;
        return;
    }
    let parts = rt.participants(me);
    let usable = |recipients: &BTreeSet<DeviceId>, generation: u64| {
        recipients.is_subset(&parts) && store.has_key(&key.0, &key.1, generation)
    };
    let pick = parts
        .first()
        .and_then(|r| rt.rotations.get(r))
        .filter(|r| usable(&r.recipients, r.generation))
        .map(|r| Current {
            generation: r.generation,
            recipients: r.recipients.clone(),
        });
    let keep = rt
        .current
        .as_ref()
        .filter(|c| usable(&c.recipients, c.generation))
        .cloned();
    let next = pick.or(keep);
    if let Some(c) = &next {
        store.make_current(&key.0, &key.1, c.generation, Instant::now());
    }
    rt.current = next;
}

/// 세션 키 갱신을 띄웁니다(채널마다 하나씩, 도는 중이면 끝난 뒤 한 번 더).
fn request_rekey(node: &Arc<Node>, key: ChanKey, force: bool) {
    {
        let mut chans = node.volatile.chans.lock();
        let Some(rt) = chans.get_mut(&key) else {
            return;
        };
        rt.rekey_force |= force;
        if rt.rekey_running {
            rt.rekey_again = true;
            return;
        }
        rt.rekey_running = true;
    }
    node.spawn_detached(move |node| async move {
        loop {
            let force = {
                let mut chans = node.volatile.chans.lock();
                chans
                    .get_mut(&key)
                    .is_some_and(|rt| std::mem::take(&mut rt.rekey_force))
            };
            if let Err(e) = rekey_once(&node, key, force).await {
                debug!(error = %e, "휘발성 세션 키 갱신 실패");
            }
            let again = {
                let mut chans = node.volatile.chans.lock();
                match chans.get_mut(&key) {
                    Some(rt) => {
                        let again = std::mem::take(&mut rt.rekey_again);
                        rt.rekey_running = again;
                        again
                    }
                    None => false,
                }
            };
            if !again {
                break;
            }
        }
    });
}

/// 새 세대 키를 냅니다. `force`면 갱신 담당이 아니어도(보낼 키가 없을 때).
///
/// 회전 공지(`VolatileRotation`, 서명, 키 없음)는 참가자 모두에게, 키 감싸기(`VolatileKeyWrap`, 기기 하나에게 봉인)는
/// 키를 스스로 구할 수 없는 기기에게만 따로 보냅니다(예전에는 한 이벤트에 모두를 봉인해 약 213명에서 막혔음).
/// - 부모 세대(지금 보내는 키) 받는 기기가 모두 아직 참가자이면 가입만 있는 것: 새 키는 부모 키에서 유도하고
///   (`keysched::vol_join`), 가입자만 감싸기를 받습니다.
/// - 그 사이 나간·쫓겨난·권한을 잃은 기기가 있으면 새 무작위 키를 모두에게 감쌉니다. [`FRESH_EVERY`]에 한 번 이하로
///   묶습니다(그 사이 바뀐 참가자는 기다린 뒤 한 번에).
async fn rekey_once(node: &Arc<Node>, key: ChanKey, force: bool) -> Result<()> {
    let (space, channel) = key;
    let me = node.me()?;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let store = &node.volatile.store;
    loop {
        let plan = {
            let chans = node.volatile.chans.lock();
            let Some(rt) = chans.get(&key).filter(|rt| rt.participating()) else {
                return Ok(());
            };
            let parts = rt.participants(me.device_id());
            let needed = if force {
                rt.current.is_none() || !rt.keyed_by_rotator(me.device_id())
            } else {
                parts.first() == Some(&me.device_id())
                    && (rt.need_fresh || rt.rotated_for.as_ref() != Some(&parts))
            };
            if !needed {
                return Ok(());
            }
            // 부모: 지금 보내는 세대(키를 가졌을 때).
            let parent = rt
                .current
                .as_ref()
                .filter(|c| store.has_key(&space, &channel, c.generation))
                .map(|c| (c.generation, c.recipients.clone()));
            let fresh =
                force || rt.need_fresh || parent.as_ref().is_none_or(|(_, r)| !r.is_subset(&parts));
            if fresh
                && let Some(t) = rt.last_fresh
                && t.elapsed() < FRESH_EVERY
            {
                Err(FRESH_EVERY - t.elapsed())
            } else {
                let targets: Vec<(DeviceId, PeerId)> =
                    rt.present.iter().map(|(d, (_, p))| (*d, *p)).collect();
                Ok((parts, parent, fresh, targets, rt.head))
            }
        };
        let (parts, parent, fresh, targets, head) = match plan {
            Ok(p) => p,
            Err(wait) => {
                // 나감은 묶습니다: 기다린 뒤 그때 참가자로 다시 셉니다.
                tokio::time::sleep(wait).await;
                continue;
            }
        };
        let index = node.sync.index.get(&space, &state);
        let box_of = |d: &DeviceId| {
            let u = index.device_user(d)?;
            crate::devkeys::device_box_key(node, &state, &u, d)
        };
        let generation = loop {
            let g: u64 = rand::random();
            if g != 0 && parent.as_ref().is_none_or(|(p, _)| *p != g) {
                break g;
            }
        };
        // 키를 줄 기기: 새 키면 상자 키를 아는 참가자 모두, 가입이면 부모 받는 기기 + 상자 키를 아는 가입자.
        let me_d = me.device_id();
        let (secret, recipients, wrap_to, added, removed) = match (&parent, fresh) {
            (Some((pg, prec)), false) => {
                let joiners: Vec<(DeviceId, BoxPublicKey)> = parts
                    .iter()
                    .filter(|d| !prec.contains(d))
                    .filter_map(|d| box_of(d).map(|k| (*d, k)))
                    .collect();
                let added: Vec<DeviceId> = joiners.iter().map(|(d, _)| *d).collect();
                if added.is_empty() {
                    // 더할 기기가 없음(상자 키를 아직 모름): 지금 키 그대로, 다음 바뀜에 다시.
                    let mut chans = node.volatile.chans.lock();
                    if let Some(rt) = chans.get_mut(&key) {
                        rt.rotated_for = Some(parts);
                    }
                    return Ok(());
                }
                let Some(pk) = store.with_key(&space, &channel, *pg, |k| k.clone()) else {
                    continue;
                };
                let k =
                    yangpa_crypto::keysched::vol_join(&pk, &space, &channel, generation, &added);
                drop(pk);
                let mut rec = prec.clone();
                rec.extend(added.iter().copied());
                (k, rec, joiners, added, Vec::new())
            }
            _ => {
                let wrap_to: Vec<(DeviceId, BoxPublicKey)> = parts
                    .iter()
                    .filter(|d| **d != me_d)
                    .filter_map(|d| box_of(d).map(|k| (*d, k)))
                    .collect();
                let mut rec: BTreeSet<DeviceId> = wrap_to.iter().map(|(d, _)| *d).collect();
                rec.insert(me_d);
                let (added, removed) = match &parent {
                    Some((_, prec)) => (
                        rec.difference(prec).copied().collect(),
                        prec.difference(&rec).copied().collect(),
                    ),
                    None => (rec.iter().copied().collect(), Vec::new()),
                };
                (Secret32::random(), rec, wrap_to, added, removed)
            }
        };
        let rec_list: Vec<DeviceId> = recipients.iter().copied().collect();
        let commit = yangpa_crypto::keysched::vol_key_commit(&space, &channel, generation, &secret);
        let body = EventBody::VolatileRotation(VolatileRotationBody {
            generation,
            parent: parent.as_ref().map(|(g, _)| *g),
            fresh,
            added,
            removed,
            recipients_hash: yangpa_crypto::keysched::vol_recipients_hash(&rec_list),
            commit,
        });
        let known = node.store.max_lamport(&space)?;
        let extra = volatile_parent(head, known);
        let ev = author::author_event(
            node,
            &space,
            Some(channel),
            &body,
            &KeyChoice::Volatile(0, yangpa_crypto::keysched::vol_notice_key(&space, &channel)),
            &extra,
        )?;
        // 부모 받는 기기는 부모 세대 목록을 이미 압니다. 모르는 기기(가입자, 처음 세대)에게만 목록을 함께 보냅니다.
        let wraps: HashMap<DeviceId, (Vec<u8>, Option<Vec<DeviceId>>)> = wrap_to
            .iter()
            .map(|(d, k)| {
                let ad =
                    yangpa_crypto::keysched::vol_wrap_ad(&space, &channel, generation, d, &commit);
                let w = yangpa_crypto::seal_to(&[(*d, k.clone())], &ad, &secret.0);
                let knows = parent.as_ref().is_some_and(|(_, p)| p.contains(d));
                (*d, (w, (!knows).then(|| rec_list.clone())))
            })
            .collect();
        let mut fx = Effects::default();
        {
            let mut chans = node.volatile.chans.lock();
            let Some(rt) = chans.get_mut(&key).filter(|rt| rt.participating()) else {
                return Ok(());
            };
            let now = Instant::now();
            store.set_key(&space, &channel, generation, &secret, now)?;
            rt.known_gens.insert(generation);
            note_recipients(rt, generation, recipients.clone());
            let id = ev.id();
            rt.bump_head(ev.header.lamport, id);
            rt.rotations.insert(
                me_d,
                Rotation {
                    order: (ev.header.lamport, ev.header.created_at, id),
                    generation,
                    recipients: recipients.clone(),
                },
            );
            if fresh {
                rt.last_fresh = Some(now);
                rt.need_fresh = false;
            }
            rt.rotated_for = Some(parts);
            if recipients.is_subset(&rt.participants(me_d)) {
                rt.current = Some(Current {
                    generation,
                    recipients,
                });
            }
            reselect_current(store, key, rt, me_d);
            fx.keys_changed = true;
        }
        drop(secret);
        fx.run(node);
        deliver_key(node, key, generation, targets, ev, wraps);
        return Ok(());
    }
}

/// 세대 받는 기기를 적어 둡니다(오래된 것부터 [`MAX_GEN_RECIPIENTS`]까지).
fn note_recipients(rt: &mut ChanRt, generation: u64, recipients: BTreeSet<DeviceId>) {
    if rt.gen_recipients.len() >= MAX_GEN_RECIPIENTS && !rt.gen_recipients.contains_key(&generation)
    {
        let old: Vec<u64> = rt
            .gen_recipients
            .keys()
            .filter(|g| !rt.known_gens.contains(g))
            .copied()
            .collect();
        for g in old {
            rt.gen_recipients.remove(&g);
        }
        if rt.gen_recipients.len() >= MAX_GEN_RECIPIENTS
            && let Some(g) = rt.gen_recipients.keys().next().copied()
        {
            rt.gen_recipients.remove(&g);
        }
    }
    rt.gen_recipients.insert(generation, recipients);
}

/// 회전 공지와 감싸기를 참가자에게 직접 보냅니다(감싸기가 있는 기기는 감싸기 먼저). 실패하면 그 갱신이 아직 내 가장
/// 새 갱신이고 그 기기가 같은 연결로 참가 중인 동안 [`KEY_RETRIES`]번까지 다시 보냅니다. 끝내 못 보내도 참가자에서
/// 빼지는 않습니다(한쪽만 빼면 그 기기는 자기가 아직 참가자인 줄 알아 다시 알리지 않으므로 서로 다른 참가자 목록에
/// 머묾). 연결이 끊기면 1초 주기 작업이 빼고 새 갱신을 냅니다.
fn deliver_key(
    node: &Arc<Node>,
    key: ChanKey,
    generation: u64,
    targets: Vec<(DeviceId, PeerId)>,
    ev: SignedEvent,
    mut wraps: HashMap<DeviceId, (Vec<u8>, Option<Vec<DeviceId>>)>,
) {
    let events = Arc::new(vec![ev]);
    for (device, peer) in targets {
        let events = events.clone();
        let wrap = wraps.remove(&device);
        node.spawn_detached(move |node| async move {
            let mut wait = KEY_RETRY_START;
            for attempt in 0..=KEY_RETRIES {
                let faulted = take_fault(node.volatile.faults.lock().key_to.get_mut(&peer));
                let r = if faulted {
                    Err(Error::Network("시험용 실패".into()))
                } else {
                    push_rotation(&node, key, peer, generation, &wrap, &events).await
                };
                match r {
                    Ok(()) => return,
                    Err(e) => debug!(peer = %peer, attempt, error = %e, "세션 키 갱신 전달 실패"),
                }
                if attempt == KEY_RETRIES {
                    break;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(KEY_RETRY_MAX);
                if !key_delivery_needed(&node, key, generation, device, peer) {
                    return;
                }
            }
            debug!(device = %device.short(), "세션 키 갱신을 끝내 보내지 못함");
        });
    }
}

/// 감싸기(있으면)와 회전 공지를 한 피어에게.
async fn push_rotation(
    node: &Arc<Node>,
    key: ChanKey,
    peer: PeerId,
    generation: u64,
    wrap: &Option<(Vec<u8>, Option<Vec<DeviceId>>)>,
    events: &[SignedEvent],
) -> Result<()> {
    if let Some((w, recipients)) = wrap {
        peers::ensure_auth(node, peer).await?;
        let req = YpRequest::VolatileKeyWrap {
            space: key.0,
            channel: key.1,
            generation,
            wrap: w.clone(),
            recipients: recipients.clone(),
        };
        if let YpResponse::Error { code, .. } = crate::p2p::request(node, peer, req).await? {
            return Err(Error::Network(format!("상대가 감싸기를 거절함: {code:?}")));
        }
    }
    push_to(node, key.0, peer, events).await
}

/// 이 갱신을 아직 그 기기에 보내야 하는지(내 가장 새 갱신이고, 그 기기가 같은 피어로 참가 중).
fn key_delivery_needed(
    node: &Node,
    key: ChanKey,
    generation: u64,
    device: DeviceId,
    peer: PeerId,
) -> bool {
    let Ok(me) = node.me() else { return false };
    let chans = node.volatile.chans.lock();
    chans.get(&key).is_some_and(|rt| {
        rt.rotations
            .get(&me.device_id())
            .is_some_and(|r| r.generation == generation)
            && rt.present.get(&device).is_some_and(|(_, p)| *p == peer)
    })
}

// ───────────── 받은 이벤트 ─────────────

/// 휘발성 이벤트 하나. 서명·공간 일치는 이미 확인했습니다. `from`이 `None`이면 이 기기가 만든 것.
/// 적용 잠금 안에서 불리므로 동기로 짧게 끝내고, 이어지는 일(키 갱신·알림)은 띄웁니다.
pub(crate) fn on_inbound_event(
    node: &Arc<Node>,
    space: &SpaceId,
    state: &SpaceState,
    ev: SignedEvent,
    from: Option<PeerId>,
) -> VolatileVerdict {
    let origin = if from.is_some() {
        Origin::Peer
    } else {
        Origin::Local
    };
    let mut fx = Effects::default();
    let verdict = accept(node, space, state, ev, origin, &mut fx);
    fx.run(node);
    verdict
}

fn accept(
    node: &Arc<Node>,
    space: &SpaceId,
    state: &SpaceState,
    ev: SignedEvent,
    origin: Origin,
    fx: &mut Effects,
) -> VolatileVerdict {
    let h = &ev.header;
    let StreamId::Channel(channel) = h.stream() else {
        return VolatileVerdict::Rejected;
    };
    // 채널 생성이 아직 안 왔으면 받지 않습니다(휘발성은 보류할 곳이 없음).
    let Some(settings) = state.channels.get(&channel) else {
        return VolatileVerdict::Ignored;
    };
    if !settings.is_volatile() {
        return VolatileVerdict::Rejected;
    }
    if !member_device(state, &h.user, &h.author) {
        return VolatileVerdict::Ignored;
    }
    let Ok(me) = node.me() else {
        return VolatileVerdict::Ignored;
    };
    if !can_view(state, &me.user(), &channel) {
        return VolatileVerdict::Ignored;
    }
    let lane = h.kind.lane();
    let need_present = match origin {
        Origin::Local => false,
        Origin::Peer => true,
        Origin::Snapshot { from, .. } => {
            // 적용 경로의 논리 시계 검사를 거치지 않으므로 여기서 같은 상한을 둡니다(머리가 부풀지 않게).
            if lamport_too_far(node, space, h.lamport) {
                return VolatileVerdict::Ignored;
            }
            // 키 묶음은 응답한 참가자가 만든 것만. 메시지는 수명이 '퇴장 시'일 때만 작성자가 지금 참가자여야 합니다.
            if lane == Lane::Key && h.author != from {
                return VolatileVerdict::Ignored;
            }
            lane == Lane::Key || settings.ttl == ChatTtl::OnLeave
        }
    };
    let key = (*space, channel);
    let mut chans = node.volatile.chans.lock();
    let Some(rt) = live_rt(node, &mut chans, state, key, settings, me.device_id(), fx) else {
        return VolatileVerdict::Ignored;
    };
    if need_present
        && !author_present(
            node,
            state,
            key,
            settings,
            rt,
            me.device_id(),
            &h.user,
            &h.author,
            fx,
        )
    {
        // 입장 알림보다 먼저 온 것일 수 있어(알림 유실 뒤 다시 알리는 사이 등) 잠깐 둡니다. 그 기기의 입장을 받아야만
        // 처리하므로 들어오지 않은 기기의 이벤트는 끝내 적용되지 않습니다.
        if origin == Origin::Peer
            && rt.mode == JoinMode::JoinLeave
            && !rt.departed.contains_key(&h.author)
            && can_view(state, &h.user, &channel)
        {
            rt.hold_early(ev, Instant::now());
        }
        return VolatileVerdict::Ignored;
    }
    match lane {
        Lane::Key => key_lane(node, state, key, rt, &me, ev, origin, fx),
        Lane::Message => message(node, state, key, settings, rt, &me, ev, origin, fx),
        _ => VolatileVerdict::Rejected,
    }
}

#[allow(clippy::too_many_arguments)]
fn key_share(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    rt: &mut ChanRt,
    me: &Me,
    ev: SignedEvent,
    origin: Origin,
    fx: &mut Effects,
) -> VolatileVerdict {
    let h = &ev.header;
    if h.key != KeyRef::Sealed || h.kind != EventKind::KeyShare {
        return VolatileVerdict::Rejected;
    }
    let Some(bytes) = ev.body.as_deref() else {
        return VolatileVerdict::Rejected;
    };
    let body = match open_sealed_body(&me.device_id(), &me.device.boxkey, h, bytes) {
        Ok(b) => b,
        Err(CryptoError::NotARecipient) => return VolatileVerdict::Ignored,
        Err(_) => return VolatileVerdict::Rejected,
    };
    if authorize_message(state, h, &body, None).is_err() {
        return VolatileVerdict::Rejected;
    }
    let EventBody::KeyShare(ks) = body else {
        return VolatileVerdict::Rejected;
    };
    // 키 묶음은 스냅숏으로 내가 요청해 나에게 준 것만 받습니다(회전은 `VolatileRotation` 공지와 감싸기). 키 넣기는 적용
    // 잠금 안에서 일어나므로, 아무 묶음으로나 세대 키를 대량으로 심지 못하게 합니다.
    let allowed = match origin {
        Origin::Snapshot { .. } => {
            ks.for_user == Some(me.user()) && ks.keys.len() <= MAX_EVENTS_PER_MESSAGE
        }
        Origin::Peer | Origin::Local => false,
    };
    if !allowed {
        return VolatileVerdict::Ignored;
    }
    let store = &node.volatile.store;
    let now = Instant::now();
    let mut gens = Vec::new();
    for k in &ks.keys {
        if let SharedKey::Volatile {
            channel,
            generation,
            key: secret,
        } = k
            && *channel == key.1
            // 같은 세대에 다른 키가 오면 먼저 받은 키를 둡니다.
            && store.set_key(&key.0, &key.1, *generation, secret, now).is_ok()
        {
            gens.push(*generation);
        }
    }
    if gens.is_empty() {
        return VolatileVerdict::Ignored;
    }
    rt.known_gens.extend(gens.iter().copied());
    rt.bump_head(h.lamport, ev.id());
    drop(ks);
    keys_arrived(node, state, key, rt, me, &gens, fx);
    VolatileVerdict::Accepted
}

/// 세대 키가 생긴 뒤: 보낼 세대를 다시 고르고, 그 키를 기다리던 메시지와 그 세대를 부모로 둔 회전 공지를 처리합니다.
fn keys_arrived(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    rt: &mut ChanRt,
    me: &Me,
    gens: &[u64],
    fx: &mut Effects,
) {
    let store = &node.volatile.store;
    // 새 세대를 부모로 둔 공지는 이제 유도할 수 있습니다(그 공지가 또 다른 공지의 부모일 수 있어 차례로).
    let mut todo: Vec<u64> = gens.to_vec();
    while let Some(g) = todo.pop() {
        let next: Vec<u64> = rt
            .pending_rot
            .values()
            .filter(|p| p.body.parent == Some(g))
            .map(|p| p.body.generation)
            .collect();
        for n in next {
            let Some(p) = rt.pending_rot.remove(&n) else {
                continue;
            };
            match try_rotation(node, key, rt, me, &p) {
                RotStep::Applied => todo.push(n),
                RotStep::Wait => {
                    rt.pending_rot.insert(n, p);
                }
                RotStep::Reject => {}
            }
        }
    }
    reselect_current(store, key, rt, me.device_id());
    fx.keys_changed = true;
    let mut work = Vec::new();
    let have: Vec<u64> = rt.known_gens.iter().copied().collect();
    for g in have {
        if let Some(ids) = rt.pending_key.remove(&g) {
            work.extend(ids);
        }
    }
    drain(store, state, key, rt, me.user(), work, fx);
}

/// 키 레인 이벤트: 회전 공지 또는 스냅숏 키 묶음.
#[allow(clippy::too_many_arguments)]
fn key_lane(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    rt: &mut ChanRt,
    me: &Me,
    ev: SignedEvent,
    origin: Origin,
    fx: &mut Effects,
) -> VolatileVerdict {
    if ev.header.kind == EventKind::VolatileRotation {
        rotation(node, state, key, rt, me, ev, origin, fx)
    } else {
        key_share(node, state, key, rt, me, ev, origin, fx)
    }
}

/// 받은 회전 공지. 참가자가 낸 것만, 직접 받은 것만 받습니다. 키(가입 유도·감싸기)가 아직 없으면 기다립니다.
#[allow(clippy::too_many_arguments)]
fn rotation(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    rt: &mut ChanRt,
    me: &Me,
    ev: SignedEvent,
    origin: Origin,
    fx: &mut Effects,
) -> VolatileVerdict {
    let h = &ev.header;
    if origin != Origin::Peer || h.author == me.device_id() {
        return VolatileVerdict::Ignored;
    }
    let Some(bytes) = ev.body.as_deref() else {
        return VolatileVerdict::Rejected;
    };
    let notice = yangpa_crypto::keysched::vol_notice_key(&key.0, &key.1);
    let Ok(body) = decrypt_body(&notice, h, bytes) else {
        return VolatileVerdict::Rejected;
    };
    if authorize_message(state, h, &body, None).is_err() {
        return VolatileVerdict::Rejected;
    }
    let EventBody::VolatileRotation(b) = body else {
        return VolatileVerdict::Rejected;
    };
    let id = ev.id();
    let p = PendingRot {
        at: Instant::now(),
        author: h.author,
        lamport: h.lamport,
        id,
        order: (h.lamport, h.created_at, id),
        body: b,
    };
    match try_rotation(node, key, rt, me, &p) {
        RotStep::Applied => {
            let g = p.body.generation;
            keys_arrived(node, state, key, rt, me, &[g], fx);
            // 내 키를 받은 기기가 담당이 아닌데 갱신을 냈으면 그 기기는 내 키를 잃은 것입니다(다시 시작해 RAM을 잃었는데
            // 내가 끊김을 못 봄 등). 내 키 받는 기기 목록에 이미 있어 가입 유도로는 줄 수 없으니 새 키를 냅니다.
            let had_mine = rt
                .current
                .as_ref()
                .is_some_and(|c| c.recipients.contains(&p.author));
            if had_mine && rt.participants(me.device_id()).first() == Some(&me.device_id()) {
                rt.need_fresh = true;
                fx.rekey.insert(key);
            }
            VolatileVerdict::Accepted
        }
        RotStep::Wait => {
            debug!("회전 공지: 키를 기다림");
            if rt.pending_rot.len() >= MAX_PENDING_ROTATIONS
                && !rt.pending_rot.contains_key(&p.body.generation)
                && let Some(old) = rt
                    .pending_rot
                    .values()
                    .min_by_key(|x| x.at)
                    .map(|x| x.body.generation)
            {
                rt.pending_rot.remove(&old);
            }
            rt.pending_rot.insert(p.body.generation, p);
            VolatileVerdict::Accepted
        }
        RotStep::Reject => VolatileVerdict::Rejected,
    }
}

/// 회전 공지 하나를 적용해 봅니다: 받는 기기 목록을 부모 세대 목록(또는 감싸기에 실린 목록)에서 세어 요약과 맞추고,
/// 키를 부모 키에서 유도하거나 감싸기에서 열어 약속값과 맞춥니다. 맞지 않는 감싸기는 버립니다(다른 참가자가 심은 것).
fn try_rotation(node: &Node, key: ChanKey, rt: &mut ChanRt, me: &Me, p: &PendingRot) -> RotStep {
    use yangpa_crypto::keysched;
    let (space, channel) = key;
    let store = &node.volatile.store;
    let b = &p.body;
    let g = b.generation;
    let me_d = me.device_id();
    if store.has_key(&space, &channel, g) {
        // 이미 받은 세대(같은 세대의 다른 공지는 먼저 받은 것을 둠).
        return RotStep::Reject;
    }
    let from_parent = b
        .parent
        .and_then(|pg| rt.gen_recipients.get(&pg))
        .map(|ps| {
            let mut s = ps.clone();
            for d in &b.removed {
                s.remove(d);
            }
            s.extend(b.added.iter().copied());
            s
        });
    let from_wrap = || {
        rt.wraps.get(&g).and_then(|ws| {
            ws.iter()
                .filter(|w| w.from == p.author)
                .filter_map(|w| w.recipients.as_ref())
                .map(|l| l.iter().copied().collect::<BTreeSet<DeviceId>>())
                .find(|s| {
                    let v: Vec<DeviceId> = s.iter().copied().collect();
                    keysched::vol_recipients_hash(&v) == b.recipients_hash
                })
        })
    };
    let recipients = match (b.parent, from_parent) {
        (_, Some(s)) => s,
        (None, None) => b.added.iter().copied().collect(),
        // 부모 목록을 모르고(다시 시작해 잃음 등) 감싸기에도 목록이 없으면, 내가 보는 참가자가 요약과 맞을 때 그것을 씁니다.
        (Some(_), None) => match from_wrap().or_else(|| {
            let mine = rt.participants(me_d);
            let v: Vec<DeviceId> = mine.iter().copied().collect();
            (keysched::vol_recipients_hash(&v) == b.recipients_hash).then_some(mine)
        }) {
            Some(s) => s,
            None => return RotStep::Wait,
        },
    };
    let list: Vec<DeviceId> = recipients.iter().copied().collect();
    if keysched::vol_recipients_hash(&list) != b.recipients_hash || !recipients.contains(&p.author)
    {
        debug!("회전 공지 거절: 받는 기기 목록이 요약과 다름");
        return RotStep::Reject;
    }
    if !recipients.contains(&me_d) {
        // 나는 받는 기기가 아님(작성 기기가 내 상자 키를 아직 모름): 다음 회전을 기다립니다.
        debug!("회전 공지: 나는 받는 기기가 아님");
        return RotStep::Reject;
    }
    let derived = match (b.fresh, b.parent) {
        (false, Some(pg)) => store
            .with_key(&space, &channel, pg, |pk| {
                keysched::vol_join(pk, &space, &channel, g, &b.added)
            })
            .map(|k| (k, false)),
        _ => None,
    };
    let got = match derived {
        Some(x) => Some(x),
        None => {
            // 감싸기: 약속값과 맞는 것만. 맞지 않는 것은 버립니다.
            let mut found = None;
            if let Some(ws) = rt.wraps.get_mut(&g) {
                ws.retain(|w| {
                    if found.is_some() {
                        return true;
                    }
                    if w.from != p.author {
                        return false;
                    }
                    let ad = keysched::vol_wrap_ad(&space, &channel, g, &me_d, &b.commit);
                    let Ok(pt) = yangpa_crypto::open_sealed(&me_d, &me.device.boxkey, &ad, &w.wrap)
                    else {
                        return false;
                    };
                    let Ok(arr) = <[u8; 32]>::try_from(pt.as_slice()) else {
                        return false;
                    };
                    let k = Secret32(arr);
                    if keysched::ct_eq(
                        &keysched::vol_key_commit(&space, &channel, g, &k),
                        &b.commit,
                    ) {
                        found = Some((k, true));
                        true
                    } else {
                        false
                    }
                });
            }
            found
        }
    };
    let Some((k, wrapped)) = got else {
        return RotStep::Wait;
    };
    if !wrapped
        && !keysched::ct_eq(
            &keysched::vol_key_commit(&space, &channel, g, &k),
            &b.commit,
        )
    {
        // 부모 키에서 유도한 키가 약속과 다름: 공지가 거짓.
        debug!("회전 공지 거절: 유도한 키가 약속과 다름");
        return RotStep::Reject;
    }
    if store
        .set_key(&space, &channel, g, &k, Instant::now())
        .is_err()
    {
        debug!("회전 공지: 키를 저장하지 못함");
        return RotStep::Reject;
    }
    drop(k);
    rt.wraps.remove(&g);
    rt.known_gens.insert(g);
    note_recipients(rt, g, recipients.clone());
    rt.bump_head(p.lamport, p.id);
    if rt
        .rotations
        .get(&p.author)
        .is_none_or(|r| r.order < p.order)
    {
        rt.rotations.insert(
            p.author,
            Rotation {
                order: p.order,
                generation: g,
                recipients,
            },
        );
    }
    RotStep::Applied
}

#[allow(clippy::too_many_arguments)]
fn message(
    node: &Node,
    state: &SpaceState,
    key: ChanKey,
    settings: &ChannelSettings,
    rt: &mut ChanRt,
    me: &Me,
    ev: SignedEvent,
    origin: Origin,
    fx: &mut Effects,
) -> VolatileVerdict {
    let h = &ev.header;
    let KeyRef::Volatile { generation } = h.key else {
        return VolatileVerdict::Rejected;
    };
    if !matches!(
        h.kind.base(),
        EventKind::Message
            | EventKind::Edit
            | EventKind::Delete
            | EventKind::Reaction
            | EventKind::ReactionRemove
    ) {
        return VolatileVerdict::Rejected;
    }
    let id = ev.id();
    if rt.gone.contains_key(&id) || rt.meta.contains_key(&id) {
        return VolatileVerdict::Ignored;
    }
    let ttl = match origin {
        Origin::Snapshot {
            remaining: Some(rem),
            ..
        } if rem.is_zero() => return VolatileVerdict::Ignored,
        Origin::Snapshot {
            remaining: Some(rem),
            ..
        } => Some(ttl_of(settings).map_or(rem, |t| t.min(rem))),
        Origin::Snapshot {
            remaining: None, ..
        }
        | Origin::Local
        | Origin::Peer => ttl_of(settings),
    };
    let now = Instant::now();
    let meta = Meta {
        kind: h.kind.base(),
        anonymous: h.kind.is_anonymous(),
        lamport: h.lamport,
        user: h.user,
        device: h.author,
        created_at: h.created_at,
        generation,
        target: None,
        ready: false,
        quiet: matches!(origin, Origin::Snapshot { .. }),
        mentions_me: false,
        since: now,
    };
    let store = &node.volatile.store;
    match store.insert(&key.0, &key.1, ev, ttl, now) {
        Ok(true) => {}
        Ok(false) => return VolatileVerdict::Ignored,
        Err(StoreError::InvalidInput(_)) => return VolatileVerdict::Rejected,
        Err(e) => {
            debug!(error = %e, "휘발성 이벤트를 RAM에 넣지 못함");
            return VolatileVerdict::Ignored;
        }
    }
    rt.meta.insert(id, meta);
    let mut woken = Vec::new();
    let outcome = try_ready(store, state, key, rt, me.user(), id, &mut woken, fx);
    drain(store, state, key, rt, me.user(), woken, fx);
    match outcome {
        Outcome::Ready | Outcome::Pending => VolatileVerdict::Accepted,
        Outcome::Invalid => VolatileVerdict::Rejected,
        Outcome::Moot | Outcome::Gone => VolatileVerdict::Ignored,
    }
}

/// 기다리던 이벤트들을 차례로 다시 봅니다.
fn drain(
    store: &VolatileStore,
    state: &SpaceState,
    key: ChanKey,
    rt: &mut ChanRt,
    me_user: UserId,
    mut work: Vec<EventId>,
    fx: &mut Effects,
) {
    work.sort_by_key(|id| std::cmp::Reverse(rt.meta.get(id).map(|m| m.pos(*id))));
    while let Some(id) = work.pop() {
        let mut woken = Vec::new();
        try_ready(store, state, key, rt, me_user, id, &mut woken, fx);
        work.extend(woken);
    }
}

/// 풀고 권한을 확인해 보기에 넣습니다. 키·대상이 없으면 기다리고, 틀리면 지웁니다.
#[allow(clippy::too_many_arguments)]
fn try_ready(
    store: &VolatileStore,
    state: &SpaceState,
    key: ChanKey,
    rt: &mut ChanRt,
    me_user: UserId,
    id: EventId,
    woken: &mut Vec<EventId>,
    fx: &mut Effects,
) -> Outcome {
    let Some(m) = rt.meta.get(&id) else {
        return Outcome::Gone;
    };
    if m.ready {
        return Outcome::Ready;
    }
    let generation = m.generation;
    let Some(ve) = store.get(&key.0, &key.1, &id) else {
        return Outcome::Gone;
    };
    let header = &ve.event.header;
    let ciphertext = ve.event.body.as_deref().unwrap_or_default();
    let Some(opened) = store.with_key(&key.0, &key.1, generation, |k| {
        decrypt_body(k, header, ciphertext)
    }) else {
        rt.pending_key.entry(generation).or_default().push(id);
        return Outcome::Pending;
    };
    let Ok(mut body) = opened else {
        return discard(store, key, rt, id, Outcome::Invalid);
    };
    let target = body_target(&body);
    let mut target_author = None;
    if let Some(t) = target {
        if rt.gone.contains_key(&t) {
            scrub(&mut body);
            return discard(store, key, rt, id, Outcome::Moot);
        }
        match rt.meta.get(&t) {
            Some(tm) if tm.kind != EventKind::Message => {
                scrub(&mut body);
                return discard(store, key, rt, id, Outcome::Invalid);
            }
            Some(tm) if tm.ready => target_author = Some(tm.user),
            _ => {
                if let Some(mm) = rt.meta.get_mut(&id) {
                    mm.target = Some(t);
                }
                rt.pending_target.entry(t).or_default().push(id);
                scrub(&mut body);
                return Outcome::Pending;
            }
        }
    }
    let allowed = authorize_message(state, header, &body, target_author).is_ok();
    let mentions_me = match &body {
        EventBody::Message(mb) => {
            crate::messages::mentions_me(state, header, mb, &me_user, Timestamp::now().as_millis())
        }
        _ => false,
    };
    let attachments = attach::listed(&body);
    scrub(&mut body);
    drop(body);
    drop(ve);
    if !allowed {
        return discard(store, key, rt, id, Outcome::Invalid);
    }
    let Some(m) = rt.meta.get_mut(&id) else {
        return Outcome::Gone;
    };
    m.ready = true;
    m.target = target;
    m.mentions_me = mentions_me;
    let (kind, pos, quiet) = (m.kind, m.pos(id), m.quiet);
    if kind == EventKind::Message && !attachments.is_empty() {
        let (user, device) = (m.user, m.device);
        for (blob, size, t) in &attachments {
            rt.attach.add(id, *blob, *size, *t);
        }
        fx.attach.push(attach::Ready {
            key,
            owner: id,
            user,
            device,
            blobs: attachments,
        });
    }
    // 확인을 마친 이벤트만 머리에 반영합니다(풀리지 않는 위조 이벤트가 다음 논리 시계를 끌어올리지 않게).
    rt.bump_head(pos.0, id);
    if let Some(t) = target {
        rt.related.entry(t).or_default().insert(pos);
    }
    let (space, channel) = key;
    match (kind, target) {
        (EventKind::Message, _) => {
            if let Some(ids) = rt.pending_target.remove(&id) {
                woken.extend(ids);
            }
            if !quiet && let Some(message) = build_view(store, key, rt, me_user, &id) {
                fx.notes.push(Notification::MessageNew {
                    space,
                    channel,
                    message,
                    notify: NotifyView {
                        title: NOTIFY_TITLE.into(),
                        body: None,
                        sender: None,
                    },
                });
            }
        }
        (EventKind::Delete, Some(t)) => {
            let view = deleted_view(rt, &t);
            remove_events(store, key, rt, &[t, id], me_user, false, fx);
            if let Some(message) = view {
                fx.notes.push(Notification::MessageUpdated {
                    space,
                    channel,
                    message,
                });
            }
        }
        (_, Some(t)) => {
            if !quiet && let Some(message) = build_view(store, key, rt, me_user, &t) {
                fx.notes.push(Notification::MessageUpdated {
                    space,
                    channel,
                    message,
                });
            }
        }
        _ => {}
    }
    Outcome::Ready
}

fn discard(
    store: &VolatileStore,
    key: ChanKey,
    rt: &mut ChanRt,
    id: EventId,
    outcome: Outcome,
) -> Outcome {
    store.remove(&key.0, &key.1, &id);
    rt.meta.remove(&id);
    rt.gone.insert(id, Instant::now());
    outcome
}

/// 이벤트들과 그 메시지에 딸린 이벤트를 지웁니다. 지운 메시지는 `announce`면 `message.expired`로, 반응·수정이
/// 빠진 남은 메시지는 `message.updated`로 알립니다.
fn remove_events(
    store: &VolatileStore,
    key: ChanKey,
    rt: &mut ChanRt,
    ids: &[EventId],
    me_user: UserId,
    announce: bool,
    fx: &mut Effects,
) {
    let now = Instant::now();
    let mut expired = Vec::new();
    let mut touched = BTreeSet::new();
    let mut work: Vec<EventId> = ids.to_vec();
    while let Some(id) = work.pop() {
        store.remove(&key.0, &key.1, &id);
        let Some(m) = rt.meta.remove(&id) else {
            continue;
        };
        rt.gone.insert(id, now);
        if m.kind == EventKind::Message {
            // RAM의 첨부는 저장소가 메시지와 함께 지웠습니다.
            rt.attach.remove_owner(&id);
            if m.ready {
                expired.push(id);
            }
            if let Some(rel) = rt.related.remove(&id) {
                work.extend(rel.into_iter().map(|(_, _, r)| r));
            }
        } else if let Some(t) = m.target {
            if let Some(set) = rt.related.get_mut(&t) {
                set.remove(&m.pos(id));
            }
            if m.ready {
                touched.insert(t);
            }
        }
    }
    let (space, channel) = key;
    if announce && !expired.is_empty() {
        fx.notes.push(Notification::MessageExpired {
            space,
            channel,
            ids: expired,
        });
    }
    for t in touched {
        if let Some(message) = build_view(store, key, rt, me_user, &t) {
            fx.notes.push(Notification::MessageUpdated {
                space,
                channel,
                message,
            });
        }
    }
}

/// 채널 상태를 통째로 지웁니다(퇴장·채널 삭제·권한 잃음·종료).
fn wipe(node: &Node, key: ChanKey, rt: ChanRt, fx: &mut Effects) {
    let ids: Vec<EventId> = rt
        .meta
        .iter()
        .filter(|(_, m)| m.ready && m.kind == EventKind::Message)
        .map(|(id, _)| *id)
        .collect();
    node.volatile.store.drop_channel(&key.0, &key.1);
    attach::forget(node, Some(key));
    drop(rt);
    if !ids.is_empty() {
        fx.notes.push(Notification::MessageExpired {
            space: key.0,
            channel: key.1,
            ids,
        });
    }
}

/// 담당이 아닌데 담당의 키 없이 [`STALE_AFTER`]가 지나면 담당에게 새 키를 청합니다(내가 새 무작위 키로 갱신하면
/// 담당이 그것을 보고 새 키를 냄). 가입 회전이 오가는 짧은 동안은 기다립니다.
fn check_stale(key: ChanKey, rt: &mut ChanRt, me: DeviceId, now: Instant, fx: &mut Effects) {
    if !rt.participating() || rt.keyed_by_rotator(me) {
        rt.stale_since = None;
        return;
    }
    match rt.stale_since {
        None => rt.stale_since = Some(now),
        Some(t) if now.duration_since(t) >= STALE_AFTER => {
            rt.stale_since = Some(now);
            fx.rekey_force.insert(key);
        }
        Some(_) => {}
    }
}

/// 상시 채널 상한에 밀려났으면 받은 이벤트·세션 키를 지우고 참가자 셈만 남깁니다(나간 것과 같음).
fn shed_if_full(node: &Node, key: ChanKey, rt: &mut ChanRt, fx: &mut Effects) {
    if rt.full.is_none() || (rt.meta.is_empty() && rt.known_gens.is_empty()) {
        return;
    }
    let mut kept = ChanRt::new(rt.mode);
    kept.full = rt.full;
    kept.present = std::mem::take(&mut rt.present);
    let old = std::mem::replace(rt, kept);
    wipe(node, key, old, fx);
}

/// 이 기기가 참가 중인 휘발성 채널의 다른 참가자 피어(글 쓰는 중 알림용). 참가 중이 아니면 빈 목록.
pub(crate) fn participant_peers(node: &Node, space: &SpaceId, channel: &ChannelId) -> Vec<PeerId> {
    node.volatile
        .chans
        .lock()
        .get(&(*space, *channel))
        .filter(|rt| rt.participating())
        .map(ChanRt::peers)
        .unwrap_or_default()
}

/// 이 기기가 참가 중인 휘발성 채널에 그 기기도 함께 참가 중인지.
pub(crate) fn is_participant(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    device: &DeviceId,
) -> bool {
    node.volatile
        .chans
        .lock()
        .get(&(*space, *channel))
        .is_some_and(|rt| rt.participating() && rt.present.contains_key(device))
}

/// 모든 휘발성 상태(암호문·세션 키·읽음 표시)를 지웁니다. 종료할 때 부릅니다.
pub(crate) fn wipe_all(node: &Node) {
    let all: Vec<ChanKey> = {
        let mut chans = node.volatile.chans.lock();
        let keys = chans.keys().copied().collect();
        chans.clear();
        keys
    };
    for (s, c) in all.into_iter().chain(node.volatile.store.channels()) {
        node.volatile.store.drop_channel(&s, &c);
    }
    attach::forget(node, None);
}

// ───────────── 보기 ─────────────

fn open_body(
    store: &VolatileStore,
    key: ChanKey,
    generation: u64,
    id: &EventId,
) -> Option<(EventBody, Option<Instant>)> {
    let ve = store.get(&key.0, &key.1, id)?;
    let body = store
        .with_key(&key.0, &key.1, generation, |k| {
            decrypt_body(
                k,
                &ve.event.header,
                ve.event.body.as_deref().unwrap_or_default(),
            )
        })?
        .ok()?;
    Some((body, ve.expires_at))
}

/// 메시지 보기(필요할 때만 풀고, 수정·반응을 접어 넣음).
fn build_view(
    store: &VolatileStore,
    key: ChanKey,
    rt: &ChanRt,
    me_user: UserId,
    id: &EventId,
) -> Option<MessageView> {
    let m = rt.ready_message(id)?;
    let (body, expires) = open_body(store, key, m.generation, id)?;
    let EventBody::Message(mut mb) = body else {
        return None;
    };
    let mut text = std::mem::take(&mut mb.text);
    let mut edited_at = None;
    let mut reacts: BTreeMap<String, BTreeMap<UserId, bool>> = BTreeMap::new();
    if let Some(rel) = rt.related.get(id) {
        for (_, _, rid) in rel {
            let Some(rm) = rt.meta.get(rid).filter(|r| r.ready) else {
                continue;
            };
            let Some((body, _)) = open_body(store, key, rm.generation, rid) else {
                continue;
            };
            match body {
                EventBody::Edit { text: t, .. } if rm.user == m.user => {
                    text.zeroize();
                    text = t;
                    edited_at = Some(rm.created_at);
                }
                EventBody::Reaction { emoji, .. } => {
                    reacts
                        .entry(messages::emoji_string(&emoji))
                        .or_default()
                        .insert(rm.user, true);
                }
                EventBody::ReactionRemove { emoji, .. } => {
                    reacts
                        .entry(messages::emoji_string(&emoji))
                        .or_default()
                        .insert(rm.user, false);
                }
                mut other => scrub(&mut other),
            }
        }
    }
    let attachments = attach::views(store, key, id, &mb.attachments);
    for a in &mut mb.attachments {
        a.name.zeroize();
        a.thumbnail.zeroize();
    }
    let reactions = reacts
        .into_iter()
        .filter_map(|(emoji, users)| {
            let count = users.values().filter(|on| **on).count();
            (count > 0).then(|| ReactionView {
                emoji,
                count: u32::try_from(count).unwrap_or(u32::MAX),
                me: users.get(&me_user).copied().unwrap_or(false),
            })
        })
        .collect();
    let now = Instant::now();
    let expires_at = expires.map(|e| {
        Timestamp(
            Timestamp::now()
                .as_millis()
                .saturating_add(millis(e.saturating_duration_since(now))),
        )
    });
    Some(MessageView {
        id: *id,
        author: m.user,
        author_device: m.device,
        created_at: m.created_at,
        lamport: m.lamport,
        text,
        attachments,
        embeds: Vec::new(),
        reply_to: mb.reply_to,
        reactions,
        edited_at,
        deleted: false,
        volatile: true,
        expires_at,
        thread: None,
        space: None,
        channel: None,
        author_blocked: false,
        anonymous: m.anonymous,
        anon_name: None,
        mine: m.user == me_user,
        mentions_me: m.mentions_me,
    })
}

/// 지운 메시지의 알림용 보기(내용 없음).
fn deleted_view(rt: &ChanRt, id: &EventId) -> Option<MessageView> {
    let m = rt.ready_message(id)?;
    Some(MessageView {
        id: *id,
        author: m.user,
        author_device: m.device,
        created_at: m.created_at,
        lamport: m.lamport,
        text: String::new(),
        attachments: Vec::new(),
        embeds: Vec::new(),
        reply_to: None,
        reactions: Vec::new(),
        edited_at: None,
        deleted: true,
        volatile: true,
        expires_at: None,
        thread: None,
        space: None,
        channel: None,
        author_blocked: false,
        anonymous: m.anonymous,
        anon_name: None,
        mine: false,
        mentions_me: false,
    })
}

/// 채널 보기 채우기(휘발성 채널만). 입장 여부(입장·퇴장 방식), 참가자, 읽지 않은 수, API 읽기 허용.
pub(crate) fn fill_channel_view(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    view: &mut ChannelView,
) {
    if !view.settings.is_volatile() {
        return;
    }
    let key = (*space, view.channel);
    view.api_read_allowed = node.volatile.api_read.lock().contains(&key);
    let Ok(me) = node.me() else {
        return;
    };
    let chans = node.volatile.chans.lock();
    if view.settings.join_mode == JoinMode::Always {
        view.volatile_cap = Some(state.volatile_cap(&view.channel));
        view.volatile_full = chans.get(&key).is_some_and(|rt| rt.full.is_some());
    }
    let rt = chans
        .get(&key)
        .filter(|rt| rt.mode == view.settings.join_mode && rt.participating());
    if view.settings.join_mode == JoinMode::JoinLeave {
        view.joined = Some(rt.is_some());
    }
    view.unread = 0;
    if let Some(rt) = rt {
        let mut users: BTreeSet<UserId> = rt.present.values().map(|(u, _)| *u).collect();
        users.insert(me.user());
        let mut users: Vec<UserId> = users.into_iter().collect();
        // 익명 채널은 몇 명인지만 보여 줍니다.
        if view.anonymous {
            users.fill(ANONYMOUS_USER);
        }
        view.participants = Some(users);
        let unread = rt
            .meta
            .iter()
            .filter(|(id, m)| {
                m.ready
                    && m.kind == EventKind::Message
                    && m.user != me.user()
                    && rt.read_marker.is_none_or(|r| m.pos(**id) > r)
            })
            .count();
        view.unread = u32::try_from(unread).unwrap_or(u32::MAX);
    }
}

/// `channel.mark_read`: 휘발성 채널의 읽음 위치는 RAM에만 둡니다. `event`가 없으면 가장 최근 메시지까지.
pub(crate) fn mark_read(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    event: Option<&EventId>,
) -> std::result::Result<(), RpcError> {
    let mut chans = node.volatile.chans.lock();
    let Some(rt) = chans.get_mut(&(*space, *channel)) else {
        return match event {
            Some(_) => Err(RpcError::not_found("이 채널에 없는 메시지")),
            None => Ok(()),
        };
    };
    let pos = match event {
        Some(event) => rt
            .ready_message(event)
            .map(|m| m.pos(*event))
            .ok_or_else(|| RpcError::not_found("이 채널에 없는 메시지"))?,
        None => {
            let latest = rt
                .meta
                .iter()
                .filter(|(_, m)| m.ready && m.kind == EventKind::Message)
                .map(|(id, m)| m.pos(*id))
                .max();
            let Some(pos) = latest else {
                return Ok(());
            };
            pos
        }
    };
    if rt.read_marker.is_none_or(|r| r < pos) {
        rt.read_marker = Some(pos);
    }
    Ok(())
}

// ───────────── 메시지 API ─────────────

/// 공간 멤버이고 채널을 볼 수 있는지 확인합니다.
fn api_channel(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
) -> std::result::Result<(Arc<SpaceState>, ChannelSettings, Arc<Me>), RpcError> {
    let me = api::me(node)?;
    let state = node
        .space_state(space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    let ch = state
        .channels
        .get(channel)
        .cloned()
        .ok_or_else(|| RpcError::not_found("없는 채널"))?;
    if !can_view(&state, &me.user(), channel) {
        return Err(RpcError::forbidden("이 채널을 볼 권한이 없습니다"));
    }
    Ok((state, ch, me))
}

/// 휘발성 채널 확인 + 상시 채널이면 참가 상태를 바로 세웁니다(1초 주기를 기다리지 않게).
fn prepare(
    node: &Arc<Node>,
    key: ChanKey,
) -> std::result::Result<(Arc<SpaceState>, ChannelSettings, Arc<Me>), RpcError> {
    let (state, ch, me) = api_channel(node, &key.0, &key.1)?;
    if !ch.is_volatile() {
        return Err(RpcError::invalid_input("휘발성 채널이 아닙니다"));
    }
    if ch.join_mode == JoinMode::Always {
        let mut fx = Effects::default();
        {
            let mut chans = node.volatile.chans.lock();
            let _ = live_rt(node, &mut chans, &state, key, &ch, me.device_id(), &mut fx);
        }
        fx.run(node);
    }
    Ok((state, ch, me))
}

/// 모더레이션(신고·처리)의 작성자 조회: 참가 중인 채널에서 지금 보기에 있는 메시지 하나(목록 페이지와 상관없이).
/// 자동화 토큰은 `message.list`처럼 휘발성 읽기를 허락받은 채널만.
pub(crate) fn author_for_api(
    node: &Arc<Node>,
    ctx: &CallCtx,
    space: &SpaceId,
    channel: &ChannelId,
    id: &EventId,
) -> std::result::Result<Option<UserId>, RpcError> {
    let key = (*space, *channel);
    if ctx.is_automation() && !node.volatile.api_read.lock().contains(&key) {
        return Err(RpcError::volatile_read_denied());
    }
    prepare(node, key)?;
    let chans = node.volatile.chans.lock();
    Ok(chans
        .get(&key)
        .filter(|rt| rt.participating())
        .and_then(|rt| rt.ready_message(id))
        .map(|m| m.user))
}

/// 메시지 상태: 보기에 있으면 작성자, 지웠거나 만료됐으면 `Err(true)`, 모르면 `Err(false)`.
fn message_author(node: &Node, key: ChanKey, id: &EventId) -> std::result::Result<UserId, bool> {
    let chans = node.volatile.chans.lock();
    let Some(rt) = chans.get(&key) else {
        return Err(false);
    };
    match rt.ready_message(id) {
        Some(m) => Ok(m.user),
        None => Err(rt.gone.contains_key(id)),
    }
}

fn full_error(cap: u32) -> RpcError {
    RpcError::conflict(format!(
        "상시 참여 인원이 가득 찼습니다(최대 {cap}명). 자리가 나면 자동으로 참여합니다"
    ))
}

/// 보낼 세션 키. 참가자 모두가 가진 키가 생길 때까지 잠깐 기다리고, 오래 없으면 직접 갱신합니다.
async fn current_key(
    node: &Arc<Node>,
    key: ChanKey,
    me: DeviceId,
) -> std::result::Result<(u64, Secret32), RpcError> {
    let start = Instant::now();
    let mut forced = false;
    loop {
        let notified = node.volatile.key_arrived.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let pick = {
            let chans = node.volatile.chans.lock();
            if let Some(cap) = chans.get(&key).and_then(|rt| rt.full) {
                return Err(full_error(cap));
            }
            let Some(rt) = chans.get(&key).filter(|rt| rt.participating()) else {
                return Err(RpcError::conflict(
                    "이 채널에 들어가 있지 않습니다(channel.join)",
                ));
            };
            let parts = rt.participants(me);
            rt.current.as_ref().and_then(|c| {
                let k = node
                    .volatile
                    .store
                    .with_key(&key.0, &key.1, c.generation, |k| k.clone())?;
                Some((c.generation, k, c.recipients == parts))
            })
        };
        let waited = start.elapsed();
        let has_key = pick.is_some();
        if let Some((generation, k, exact)) = pick
            && (exact || waited >= KEY_SETTLE)
        {
            return Ok((generation, k));
        }
        if waited >= KEY_WAIT {
            return Err(RpcError::conflict(
                "아직 이 채널의 세션 키가 없습니다(참가자 응답을 기다리는 중)",
            ));
        }
        if !has_key && !forced && waited >= KEY_WAIT / 2 {
            forced = true;
            request_rekey(node, key, true);
            continue;
        }
        let next = if has_key {
            KEY_SETTLE
        } else if !forced {
            KEY_WAIT / 2
        } else {
            KEY_WAIT
        };
        let _ = tokio::time::timeout(next.saturating_sub(waited), notified).await;
    }
}

/// 세션 키로 이벤트를 만들어 이 기기에 적용하고 참가자에게 직접 보냅니다. `claim`은 이 메시지에 묶을, 미리 RAM에
/// 올린 첨부입니다(참가자가 요청하기 전에 묶음).
async fn publish(
    node: &Arc<Node>,
    key: ChanKey,
    me: DeviceId,
    mut body: EventBody,
    claim: &[BlobId],
) -> std::result::Result<EventId, RpcError> {
    let (generation, secret) = current_key(node, key, me).await?;
    let (head, peers) = {
        let chans = node.volatile.chans.lock();
        let rt = chans
            .get(&key)
            .ok_or_else(|| RpcError::conflict("이 채널에 들어가 있지 않습니다"))?;
        (rt.head, rt.peers())
    };
    let known = node
        .store
        .max_lamport(&key.0)
        .map_err(|e| RpcError::from(Error::from(e)))?;
    let extra = volatile_parent(head, known);
    let authored = author::author_event(
        node,
        &key.0,
        Some(key.1),
        &body,
        &KeyChoice::Volatile(generation, secret),
        &extra,
    );
    scrub(&mut body);
    let ev = api::rpc(authored)?;
    let state = node
        .space_state(&key.0)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    let id = ev.id();
    let mut fx = Effects::default();
    let verdict = accept(node, &key.0, &state, ev.clone(), Origin::Local, &mut fx);
    // 첨부는 참가자가 요청하기 전에 묶습니다. 하나라도 묶지 못하면(메시지가 그새 밀려났거나 채널을 나감) 아무도
    // 받을 수 없는 첨부가 실린 메시지를 보내지 않고 이 기기에서도 거둡니다(묶이지 않은 것은 `attach::Staged`가 지움).
    let claimed = !matches!(verdict, VolatileVerdict::Accepted)
        || claim.iter().all(|b| attach::claim(node, key, b, &id));
    if !claimed {
        retract(node, key, &id, &mut fx);
    }
    fx.run(node);
    match verdict {
        VolatileVerdict::Accepted => {}
        VolatileVerdict::Rejected => {
            return Err(RpcError::forbidden("이 채널에서 그 작업을 할 수 없습니다"));
        }
        VolatileVerdict::Ignored => {
            return Err(RpcError::conflict("휘발성 이벤트를 적용하지 못했습니다"));
        }
    }
    if !claimed {
        debug!("휘발성 첨부를 메시지에 묶지 못해 보내지 않음");
        return Err(RpcError::conflict(
            "첨부를 메시지에 묶지 못해 보내지 않았습니다. 다시 시도하세요",
        ));
    }
    push_events(node, key.0, peers, vec![ev]);
    Ok(id)
}

/// 이 기기에 막 적용한 메시지를 거둡니다(보내기 전에 실패했을 때). 보던 화면에는 사라진 메시지로 알립니다.
fn retract(node: &Node, key: ChanKey, id: &EventId, fx: &mut Effects) {
    let Ok(me) = node.me() else { return };
    let mut chans = node.volatile.chans.lock();
    if let Some(rt) = chans.get_mut(&key) {
        remove_events(&node.volatile.store, key, rt, &[*id], me.user(), true, fx);
    } else {
        node.volatile.store.remove(&key.0, &key.1, id);
    }
}

pub(crate) async fn send_message(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: MessageSendParams,
) -> R<m::MessageSend> {
    if p.thread.is_some() {
        return Err(RpcError::invalid_input(
            "휘발성 채널에는 게시글 댓글이 없습니다",
        ));
    }
    if p.text.trim().is_empty() && p.attachments.is_empty() {
        return Err(RpcError::invalid_input("빈 메시지입니다"));
    }
    let key = (p.space, p.channel);
    let (state, _, me) = prepare(node, key)?;
    // 받는 쪽도 같은 권한을 검사하므로, 파일을 읽기 전에 거절합니다.
    if !p.attachments.is_empty()
        && !state
            .channel_permissions(&me.user(), &p.channel)
            .contains(Permissions::ATTACH_FILES)
    {
        return Err(RpcError::forbidden("이 채널에 파일을 올릴 권한이 없습니다"));
    }
    drop(state);
    if let Some(r) = &p.reply_to
        && message_author(node, key, r).is_err()
    {
        return Err(RpcError::not_found("답장할 메시지가 없습니다"));
    }
    // 첨부는 RAM에만 올리고(디스크·임시 파일 없음), 키는 세션 키로 암호화한 본문에만 실립니다. `staged`는 이
    // 함수가 어떻게 끝나든(취소 포함) 메시지에 묶이지 않은 첨부를 지웁니다.
    let (attachments, staged) = attach::prepare_files(node, key, &p.attachments).await?;
    // 링크 미리보기는 만들지 않습니다(채널 설정과 무관: 외부 요청이 내용을 드러내고, 미리보기 이미지가 디스크
    // 블롭으로 남음). decisions.md.
    let body = EventBody::Message(MessageBody {
        text: p.text,
        attachments,
        embeds: Vec::new(),
        reply_to: p.reply_to,
        thread: None,
        mentions: Vec::new(),
    });
    let id = publish(node, key, me.device_id(), body, &staged.blobs).await?;
    drop(staged);
    Ok(EventRef { id })
}

pub(crate) async fn list_messages(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: MessageListParams,
) -> R<m::MessageList> {
    let key = (p.space, p.channel);
    if ctx.is_automation() && !node.volatile.api_read.lock().contains(&key) {
        return Err(RpcError::volatile_read_denied());
    }
    let (state, _, me) = prepare(node, key)?;
    let anonymous = state.is_anonymous(&p.channel);
    let empty = MessageListResult {
        messages: Vec::new(),
        gaps: Vec::new(),
    };
    if p.thread.is_some() {
        return Ok(empty);
    }
    let chans = node.volatile.chans.lock();
    let Some(rt) = chans.get(&key).filter(|rt| rt.participating()) else {
        return Ok(empty);
    };
    let before = match &p.before {
        Some(b) => Some(
            rt.ready_message(b)
                .map(|m| m.pos(*b))
                .ok_or_else(|| RpcError::not_found("before 메시지가 이 채널에 없습니다"))?,
        ),
        None => None,
    };
    let mut ids: Vec<Pos> = rt
        .meta
        .iter()
        .filter(|(_, m)| m.ready && m.kind == EventKind::Message)
        .map(|(id, m)| m.pos(*id))
        .filter(|pos| before.is_none_or(|b| *pos < b))
        .collect();
    ids.sort();
    let limit = p.effective_limit() as usize;
    let skip = ids.len().saturating_sub(limit);
    let messages = ids[skip..]
        .iter()
        .filter_map(|(_, _, id)| build_view(&node.volatile.store, key, rt, me.user(), id))
        .map(|mut v| {
            messages::mask(&mut v, &key.0, &key.1, anonymous);
            v
        })
        .collect();
    Ok(MessageListResult {
        messages,
        gaps: Vec::new(),
    })
}

pub(crate) async fn edit_message(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: MessageEditParams,
) -> R<m::MessageEdit> {
    let key = (p.space, p.channel);
    let (_, _, me) = prepare(node, key)?;
    let author = message_author(node, key, &p.id).map_err(|gone| {
        if gone {
            RpcError::conflict("삭제됐거나 만료된 메시지입니다")
        } else {
            RpcError::not_found("없는 메시지")
        }
    })?;
    if author != me.user() {
        return Err(RpcError::forbidden("자기 메시지만 고칠 수 있습니다"));
    }
    let body = EventBody::Edit {
        target: p.id,
        text: p.text,
        embeds: Vec::new(),
    };
    publish(node, key, me.device_id(), body, &[]).await?;
    Ok(Empty)
}

pub(crate) async fn delete_message(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: MessageRefParams,
) -> R<m::MessageDelete> {
    let key = (p.space, p.channel);
    let (state, _, me) = prepare(node, key)?;
    crate::api::message::check_deletable(&state, &me.user(), &p.channel)?;
    let author = match message_author(node, key, &p.id) {
        Ok(a) => a,
        // 이미 지웠거나 만료된 메시지는 할 일이 없습니다.
        Err(true) => return Ok(Empty),
        Err(false) => return Err(RpcError::not_found("없는 메시지")),
    };
    if author != me.user() {
        let perms = state.channel_permissions(&me.user(), &p.channel);
        if state.kind == SpaceKind::Space || !perms.contains(Permissions::MANAGE_MESSAGES) {
            return Err(RpcError::forbidden("남의 메시지를 지울 권한이 없습니다"));
        }
        let params = serde_json::to_value(&p).map_err(|e| RpcError::internal(e.to_string()))?;
        admin::confirm_when_others(node, ctx, "message.delete", &params).await?;
    }
    publish(
        node,
        key,
        me.device_id(),
        EventBody::Delete { target: p.id },
        &[],
    )
    .await?;
    Ok(Empty)
}

pub(crate) async fn react(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: ReactParams,
    present: bool,
) -> R<m::MessageReact> {
    let key = (p.space, p.channel);
    let (_, _, me) = prepare(node, key)?;
    let emoji = api::rpc(messages::parse_reaction(&p.emoji))?;
    if message_author(node, key, &p.id).is_err() {
        return Err(RpcError::not_found("없는 메시지"));
    }
    let body = if present {
        EventBody::Reaction {
            target: p.id,
            emoji,
        }
    } else {
        EventBody::ReactionRemove {
            target: p.id,
            emoji,
        }
    };
    publish(node, key, me.device_id(), body, &[]).await?;
    Ok(Empty)
}

// ───────────── 입장·퇴장·API 읽기 허용 ─────────────

/// `channel.join`: 들어가서 온라인 참가자와 입장을 주고받고, 과거 기록 공개면 스냅숏을 받습니다.
pub(crate) async fn join(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
) -> std::result::Result<Empty, RpcError> {
    let (state, ch, me) = api_channel(node, &space, &channel)?;
    if !ch.is_volatile() || ch.join_mode != JoinMode::JoinLeave {
        return Err(RpcError::invalid_input(
            "입장·퇴장 방식의 휘발성 채널이 아닙니다",
        ));
    }
    let key = (space, channel);
    if !p2p_open(node, key, &state) {
        return Err(RpcError::forbidden(
            "P2P 전용 채널에 먼저 참가해야 들어갈 수 있습니다(참가하면 참여자에게 IP가 보입니다)",
        ));
    }
    let targets = online_viewers(node, &state, key, me.device_id());
    {
        let mut fx = Effects::default();
        let mut chans = node.volatile.chans.lock();
        if chans
            .get(&key)
            .is_some_and(|rt| rt.mode != JoinMode::JoinLeave)
            && let Some(old) = chans.remove(&key)
        {
            wipe(node, key, old, &mut fx);
        }
        let rt = chans
            .entry(key)
            .or_insert_with(|| ChanRt::new(JoinMode::JoinLeave));
        if rt.joined {
            return Ok(Empty);
        }
        rt.joined = true;
        for (d, (_, p)) in &targets {
            rt.announced.insert(*d, *p);
        }
        drop(chans);
        fx.run(node);
    }
    // 들어가 있는 기기는 자기 입장으로 답하므로, 다 끝나면 지금 참가자를 압니다. 알리지 못한 기기는
    // `announced`에서 빠져 1초 주기 작업이 다시 알립니다.
    futures::future::join_all(
        targets
            .iter()
            .map(|(d, (_, p))| announce(node, key, *d, *p)),
    )
    .await;
    if ch.history == HistoryVisibility::Public {
        let from = node
            .volatile
            .chans
            .lock()
            .get(&key)
            .map(ChanRt::peers)
            .unwrap_or_default();
        fetch_snapshot(node, space, channel, from).await;
    }
    let mut fx = Effects::default();
    {
        let mut chans = node.volatile.chans.lock();
        if let Some(rt) = chans.get_mut(&key).filter(|rt| rt.joined) {
            after_view_change(node, key, &ch, rt, me.device_id(), &[], &mut fx);
        }
    }
    fx.run(node);
    Ok(Empty)
}

/// `channel.leave`: 이 기기의 채널 내용·키를 바로 지우고 참가자에게 퇴장을 알립니다.
pub(crate) async fn leave(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
) -> std::result::Result<Empty, RpcError> {
    let me = api::me(node)?;
    let state = node
        .space_state(&space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    // 채널이 이미 없어졌으면 남은 것만 지웁니다.
    if let Some(ch) = state.channels.get(&channel)
        && (!ch.is_volatile() || ch.join_mode != JoinMode::JoinLeave)
    {
        return Err(RpcError::invalid_input(
            "입장·퇴장 방식의 휘발성 채널이 아닙니다",
        ));
    }
    let key = (space, channel);
    let Some(rt) = node.volatile.chans.lock().remove(&key) else {
        return Ok(Empty);
    };
    let peers = rt.peers();
    let mut fx = Effects::default();
    wipe(node, key, rt, &mut fx);
    fx.run(node);
    if !peers.is_empty() {
        node.spawn_detached(move |node| async move {
            let n = &node;
            futures::future::join_all(
                peers
                    .iter()
                    .map(|p| send_presence(n, *p, space, channel, false)),
            )
            .await;
        });
    }
    Ok(Empty)
}

fn api_read_setting(key: &ChanKey) -> String {
    format!("{API_READ_PREFIX}{}.{}", key.0, key.1)
}

/// `channel.allow_api_read`: 자동화 토큰의 읽기 허용(설정에 채널 ID만 저장).
pub(crate) fn set_api_read(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
    allowed: bool,
) -> std::result::Result<Empty, RpcError> {
    let (_, ch, _) = api_channel(node, &space, &channel)?;
    if !ch.is_volatile() {
        return Err(RpcError::invalid_input("휘발성 채널이 아닙니다"));
    }
    let key = (space, channel);
    let name = api_read_setting(&key);
    if allowed {
        node.store
            .put_setting(&name, &true)
            .map_err(|e| RpcError::from(Error::from(e)))?;
        node.volatile.api_read.lock().insert(key);
    } else {
        node.store
            .delete_setting(&name)
            .map_err(|e| RpcError::from(Error::from(e)))?;
        node.volatile.api_read.lock().remove(&key);
    }
    Ok(Empty)
}

fn load_api_read(node: &Node) {
    let Ok(rows) = node.store.settings_with_prefix(API_READ_PREFIX) else {
        return;
    };
    let mut set = node.volatile.api_read.lock();
    for (name, value) in rows {
        let parsed = name
            .strip_prefix(API_READ_PREFIX)
            .and_then(|rest| rest.split_once('.'))
            .and_then(|(s, c)| Some((s.parse().ok()?, c.parse().ok()?)));
        if let Some(key) = parsed
            && value == serde_json::Value::Bool(true)
        {
            set.insert(key);
        }
    }
}

/// 연결별 알림 거르기. 기본 규칙(공간 제한, 자동화 토큰이면 휘발성 내용 지움)에 채널별 API 읽기 허용을 반영합니다.
pub(crate) fn filter_notification(
    node: &Node,
    auth: &AuthInfo,
    n: &Notification,
) -> Option<Notification> {
    if auth.automation {
        let channel = match n {
            Notification::MessageNew {
                space,
                channel,
                message,
                ..
            }
            | Notification::MessageUpdated {
                space,
                channel,
                message,
            } if message.volatile => Some((*space, *channel)),
            _ => None,
        };
        if let Some(key) = channel
            && node.volatile.api_read.lock().contains(&key)
        {
            let human = AuthInfo {
                automation: false,
                ..auth.clone()
            };
            return default_notification_filter(&human, n);
        }
    }
    default_notification_filter(auth, n)
}

// ───────────── 노드 간 요청 ─────────────

async fn send_presence(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
    present: bool,
) -> bool {
    if take_fault(node.volatile.faults.lock().presence_to.get_mut(&peer)) {
        return false;
    }
    if peers::ensure_auth(node, peer).await.is_err() {
        return false;
    }
    let req = YpRequest::VolatilePresence {
        space,
        channel,
        present,
    };
    match crate::p2p::request(node, peer, req).await {
        Ok(YpResponse::Ok) => true,
        Ok(YpResponse::Error { code, .. }) => {
            debug!(peer = %peer, code = ?code, "입장 알림 거절");
            false
        }
        Ok(_) => false,
        Err(e) => {
            debug!(peer = %peer, error = %e, "입장 알림 실패");
            false
        }
    }
}

/// 입장을 알립니다. 실패하면 그 기기를 `announced`에서 빼(그새 다른 피어로 바뀌지 않았으면) 1초 주기 작업이
/// 다시 알리게 합니다. 연결이 그대로인 채 알림 하나가 사라져 서로를 영영 모르는 일이 없게.
async fn announce(node: &Arc<Node>, key: ChanKey, device: DeviceId, peer: PeerId) -> bool {
    let ok = send_presence(node, peer, key.0, key.1, true).await;
    if !ok
        && let Some(rt) = node.volatile.chans.lock().get_mut(&key)
        && rt.announced.get(&device) == Some(&peer)
    {
        rt.announced.remove(&device);
    }
    ok
}

/// 참가자에게 직접 보냅니다(gossip·전체 멤버 전파 없음).
fn push_events(node: &Arc<Node>, space: SpaceId, to: Vec<PeerId>, events: Vec<SignedEvent>) {
    let events = Arc::new(events);
    for peer in to {
        let events = events.clone();
        node.spawn_detached(move |node| async move {
            if let Err(e) = push_to(&node, space, peer, &events).await {
                debug!(peer = %peer, error = %e, "휘발성 이벤트 직접 전송 실패");
            }
        });
    }
}

async fn push_to(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
    events: &[SignedEvent],
) -> Result<()> {
    peers::ensure_auth(node, peer).await?;
    for chunk in events.chunks(MAX_EVENTS_PER_MESSAGE) {
        let req = YpRequest::PushEvents {
            space,
            events: chunk.to_vec(),
        };
        if let YpResponse::Error { code, .. } = crate::p2p::request(node, peer, req).await? {
            return Err(Error::Network(format!("상대가 거절함: {code:?}")));
        }
    }
    Ok(())
}

/// 받은 `VolatilePresence`. 이 기기가 들어가 있을 때만 기억하고, 새 참가자에게는 내 입장으로 답합니다.
pub(crate) async fn handle_presence(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
    present: bool,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !member_device(&state, &user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    // P2P 전용 채널은 P2P 스웜으로, 그 밖의 채널은 Tor로 온 알림만.
    if !crate::p2p::route_ok(node, &peer, &space, &state, &StreamId::Channel(channel)) {
        return YpResponse::error(WireErrorCode::NotFound, "없는 채널");
    }
    let Some(ch) = state.channels.get(&channel).cloned() else {
        return YpResponse::error(WireErrorCode::NotFound, "없는 채널");
    };
    if !ch.is_volatile() || ch.join_mode != JoinMode::JoinLeave {
        return YpResponse::error(WireErrorCode::BadRequest, "입장·퇴장 방식 채널이 아님");
    }
    if present && !can_view(&state, &user, &channel) {
        return YpResponse::error(WireErrorCode::Forbidden, "채널을 볼 수 없음");
    }
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let key = (space, channel);
    let mut fx = Effects::default();
    let reply = {
        let mut chans = node.volatile.chans.lock();
        let Some(rt) = chans.get_mut(&key).filter(|rt| rt.joined) else {
            return YpResponse::Ok;
        };
        if present {
            rt.departed.remove(&device);
            let changed = rt.present.insert(device, (user, peer)) != Some((user, peer));
            if changed {
                after_view_change(node, key, &ch, rt, me.device_id(), &[], &mut fx);
                // 입장 알림보다 먼저 와서 잠깐 둔 이 기기의 이벤트(키 묶음 먼저)를 이제 처리합니다.
                let mut early = rt.take_early(|e| e.author() == Some(device));
                early.sort_by_cached_key(|e| {
                    (e.header.kind.lane() != Lane::Key, e.header.lamport, e.id())
                });
                for ev in early {
                    match ev.header.kind.lane() {
                        Lane::Key => {
                            key_lane(node, &state, key, rt, &me, ev, Origin::Peer, &mut fx);
                        }
                        Lane::Message => {
                            message(node, &state, key, &ch, rt, &me, ev, Origin::Peer, &mut fx);
                        }
                        _ => {}
                    }
                }
            }
            let first = rt.announced.insert(device, peer) != Some(peer);
            changed || first
        } else {
            rt.announced.remove(&device);
            rt.mark_departed(&[device], Instant::now());
            if rt.present.remove(&device).is_some() {
                after_view_change(node, key, &ch, rt, me.device_id(), &[device], &mut fx);
            }
            false
        }
    };
    // 새 참가자가 갱신 키보다 내 입장을 먼저 알도록, 답을 보낸 뒤에 갱신합니다.
    if reply {
        announce(node, key, device, peer).await;
    }
    fx.run(node);
    YpResponse::Ok
}

/// 받은 `VolatileKeyWrap`. 보낸 기기가 지금 참가자일 때만 RAM에 잠깐 두고, 그 세대 공지가 와 있으면 바로 적용합니다.
/// 감싸기는 서명이 없지만 공지의 약속값과 맞아야 쓰므로, 다른 참가자가 다른 키를 심지 못합니다.
pub(crate) fn handle_key_wrap(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
    generation: u64,
    wrap: Vec<u8>,
    recipients: Option<Vec<DeviceId>>,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !member_device(&state, &user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if !crate::p2p::route_ok(node, &peer, &space, &state, &StreamId::Channel(channel)) {
        return YpResponse::error(WireErrorCode::NotFound, "없는 채널");
    }
    if wrap.len() > MAX_WRAP_BYTES
        || generation == 0
        || recipients
            .as_ref()
            .is_some_and(|r| r.len() > yangpa_core::body::MAX_VOLATILE_ROTATION_DELTA * 4)
    {
        return YpResponse::error(WireErrorCode::BadRequest, "감싸기 형식");
    }
    let Some(settings) = state
        .channels
        .get(&channel)
        .filter(|c| c.is_volatile())
        .cloned()
    else {
        return YpResponse::error(WireErrorCode::NotFound, "없는 휘발성 채널");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let key = (space, channel);
    let mut fx = Effects::default();
    {
        let mut chans = node.volatile.chans.lock();
        let Some(rt) = live_rt(
            node,
            &mut chans,
            &state,
            key,
            &settings,
            me.device_id(),
            &mut fx,
        ) else {
            return YpResponse::Ok;
        };
        if !author_present(
            node,
            &state,
            key,
            &settings,
            rt,
            me.device_id(),
            &user,
            &device,
            &mut fx,
        ) {
            return YpResponse::error(WireErrorCode::Forbidden, "참가자가 아님");
        }
        if node.volatile.store.has_key(&space, &channel, generation) {
            return YpResponse::Ok;
        }
        if rt
            .pending_rot
            .get(&generation)
            .is_some_and(|p| p.author != device)
        {
            return YpResponse::error(WireErrorCode::Forbidden, "공지를 쓴 기기가 아님");
        }
        if rt.wraps.len() >= MAX_PENDING_ROTATIONS
            && !rt.wraps.contains_key(&generation)
            && let Some(old) = rt
                .wraps
                .iter()
                .min_by_key(|(_, ws)| ws.iter().map(|w| w.at).min())
                .map(|(g, _)| *g)
        {
            rt.wraps.remove(&old);
        }
        // 보낸 기기마다 한 자리. 자리가 다 차면 새 기기의 것은 받지 않습니다(먼저 온 공지 작성 기기 것을 지킴).
        let ws = rt.wraps.entry(generation).or_default();
        ws.retain(|w| w.from != device);
        if ws.len() >= MAX_WRAPS_PER_GEN {
            return YpResponse::error(WireErrorCode::Forbidden, "감싸기가 너무 많음");
        }
        ws.push(WrapIn {
            at: Instant::now(),
            from: device,
            wrap,
            recipients,
        });
        node.volatile.faults.lock().wraps_in += 1;
        if let Some(p) = rt.pending_rot.remove(&generation) {
            match try_rotation(node, key, rt, &me, &p) {
                RotStep::Applied => {
                    keys_arrived(node, &state, key, rt, &me, &[generation], &mut fx)
                }
                RotStep::Wait => {
                    rt.pending_rot.insert(generation, p);
                }
                RotStep::Reject => {}
            }
        }
    }
    fx.run(node);
    YpResponse::Ok
}

/// 받은 `VolatileSnapshot`(과거 기록 공개 채널의 늦은 입장자). 수명이 남은 메시지와, 그 세대 키를 요청한 기기에
/// 봉인한 키 묶음을 줍니다.
pub(crate) async fn handle_snapshot(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    channel: ChannelId,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !member_device(&state, &user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if !crate::p2p::route_ok(node, &peer, &space, &state, &StreamId::Channel(channel)) {
        return YpResponse::error(WireErrorCode::NotFound, "없는 휘발성 채널");
    }
    let Some(ch) = state.channels.get(&channel).filter(|c| c.is_volatile()) else {
        return YpResponse::error(WireErrorCode::NotFound, "없는 휘발성 채널");
    };
    if !can_view(&state, &user, &channel) {
        return YpResponse::error(WireErrorCode::Forbidden, "채널을 볼 수 없음");
    }
    if ch.history != HistoryVisibility::Public {
        return YpResponse::error(WireErrorCode::HistoryPrivate, "과거 기록 비공개 채널");
    }
    let Some(box_key) = crate::devkeys::device_box_key(node, &state, &user, &device) else {
        return YpResponse::error(WireErrorCode::NotMember, "활성 기기가 아님");
    };
    let key = (space, channel);
    let store = &node.volatile.store;
    let (events, gens) = {
        let chans = node.volatile.chans.lock();
        let Some(rt) = chans
            .get(&key)
            .filter(|rt| rt.participating() && rt.mode == ch.join_mode)
        else {
            return YpResponse::error(WireErrorCode::NotFound, "이 채널에 참가하고 있지 않음");
        };
        if rt.mode == JoinMode::JoinLeave && !rt.present.contains_key(&device) {
            return YpResponse::error(WireErrorCode::Forbidden, "참가자가 아님");
        }
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut gens = BTreeSet::new();
        // 최근 것부터 상한까지(키 묶음 자리 하나를 남김).
        for item in store
            .snapshot(&space, &channel, Instant::now())
            .iter()
            .rev()
        {
            let Some(m) = rt.meta.get(&item.event.id()).filter(|m| m.ready) else {
                continue;
            };
            let size =
                item.event.body.as_ref().map_or(0, Vec::len) + 512 + yangpa_core::sign::Sig::LEN;
            if out.len() + 1 >= MAX_EVENTS_PER_MESSAGE || bytes + size > SNAPSHOT_MAX_BYTES {
                break;
            }
            bytes += size;
            gens.insert(m.generation);
            out.push(VolatileItem {
                event: item.event.clone(),
                remaining_ms: item.remaining.map(millis),
            });
        }
        out.reverse();
        (out, gens)
    };
    let keys: Vec<SharedKey> = gens
        .iter()
        .filter_map(|g| {
            store.with_key(&space, &channel, *g, |k| SharedKey::Volatile {
                channel,
                generation: *g,
                key: k.clone(),
            })
        })
        .collect();
    if events.is_empty() || keys.is_empty() {
        return YpResponse::VolatileEvents(Vec::new());
    }
    let body = EventBody::KeyShare(KeyShareBody {
        keys,
        for_user: Some(user),
    });
    let share = match author::author_event(
        node,
        &space,
        Some(channel),
        &body,
        &KeyChoice::Sealed(vec![(device, box_key)]),
        &[],
    ) {
        Ok(e) => e,
        Err(e) => {
            debug!(error = %e, "스냅숏 키 묶음을 만들지 못함");
            return YpResponse::error(WireErrorCode::Internal, "키 묶음을 만들 수 없음");
        }
    };
    drop(body);
    let mut items = Vec::with_capacity(events.len() + 1);
    items.push(VolatileItem {
        event: share,
        remaining_ms: None,
    });
    items.extend(events);
    YpResponse::VolatileEvents(items)
}

/// 참가자 모두에게 스냅숏을 요청해 받은 것을 모두 적용합니다(같은 이벤트는 한 번만 들어감). 먼저 답한 참가자가
/// 막 들어와 가진 것이 없거나 일부만 가졌어도 다른 참가자가 가진 남은 기록을 놓치지 않게.
async fn fetch_snapshot(node: &Arc<Node>, space: SpaceId, channel: ChannelId, from: Vec<PeerId>) {
    use futures::StreamExt;
    if take_fault(Some(&mut node.volatile.faults.lock().snapshot)) {
        return;
    }
    let mut replies = futures::stream::iter(from)
        .map(|peer| request_snapshot(node, space, channel, peer))
        .buffer_unordered(SNAPSHOT_PARALLEL);
    while let Some(reply) = replies.next().await {
        if let Some((device, items)) = reply {
            apply_snapshot(node, space, channel, device, items);
        }
    }
}

/// 참가자 하나에게 스냅숏을 요청합니다. 응답한 기기와 항목, 거절·실패면 `None`.
async fn request_snapshot(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    peer: PeerId,
) -> Option<(DeviceId, Vec<VolatileItem>)> {
    peers::ensure_auth(node, peer).await.ok()?;
    let (_, device) = node.peers.identity_of(&peer)?;
    match crate::p2p::request(node, peer, YpRequest::VolatileSnapshot { space, channel }).await {
        Ok(YpResponse::VolatileEvents(items)) => Some((device, items)),
        Ok(_) => None,
        Err(e) => {
            debug!(peer = %peer, error = %e, "휘발성 스냅숏 요청 실패");
            None
        }
    }
}

/// 스냅숏 적용. `from`은 응답한 기기(키 묶음은 그 기기가 만든 것만 받음).
fn apply_snapshot(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    from: DeviceId,
    items: Vec<VolatileItem>,
) {
    if items.len() > MAX_EVENTS_PER_MESSAGE {
        return;
    }
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let fits = |ev: &SignedEvent| {
        ev.header.space == space
            && ev.header.stream() == StreamId::Channel(channel)
            && crate::devkeys::verify_event(node, &state, ev)
    };
    let (keys, mut msgs): (Vec<VolatileItem>, Vec<VolatileItem>) = items
        .into_iter()
        .filter(|i| fits(&i.event))
        .partition(|i| i.event.header.kind.lane() == Lane::Key);
    msgs.sort_by_cached_key(|i| {
        (
            i.event.header.lamport,
            i.event.header.created_at,
            i.event.id(),
        )
    });
    let mut fx = Effects::default();
    for i in keys {
        let origin = Origin::Snapshot {
            remaining: None,
            from,
        };
        accept(node, &space, &state, i.event, origin, &mut fx);
    }
    for i in msgs {
        let origin = Origin::Snapshot {
            remaining: i.remaining_ms.map(Duration::from_millis),
            from,
        };
        accept(node, &space, &state, i.event, origin, &mut fx);
    }
    fx.run(node);
}

// ───────────── 연결·주기 작업 ─────────────

pub(crate) fn on_peer_disconnected(node: &Arc<Node>, peer: PeerId) {
    let Ok(me) = node.me() else {
        return;
    };
    let mut fx = Effects::default();
    {
        let mut chans = node.volatile.chans.lock();
        for (key, rt) in chans.iter_mut() {
            rt.announced.retain(|_, p| *p != peer);
            let removed: Vec<DeviceId> = rt
                .present
                .iter()
                .filter(|(_, (_, p))| *p == peer)
                .map(|(d, _)| *d)
                .collect();
            if removed.is_empty() {
                continue;
            }
            for d in &removed {
                rt.present.remove(d);
            }
            rt.mark_departed(&removed, Instant::now());
            let Some(settings) = node
                .space_state(&key.0)
                .and_then(|s| s.channels.get(&key.1).cloned())
            else {
                continue;
            };
            after_view_change(node, *key, &settings, rt, me.device_id(), &removed, &mut fx);
        }
    }
    fx.run(node);
}

pub(crate) fn start(node: &Arc<Node>) {
    load_api_read(node);
    // 첨부 한도는 설정값으로(암호문 크기 = 평문 + 조각마다 태그, 넉넉히 1/512). 채널 한도는 보내기·받기가 따로 지킵니다.
    let v = &node.cfg.volatile;
    let per = v.max_attachment_bytes;
    node.volatile.store.set_limits(VolatileLimits {
        max_blob_size: per.saturating_add(per / 512).saturating_add(4096),
        max_blob_bytes: usize::try_from(v.max_attachment_ram_bytes).unwrap_or(usize::MAX),
        ..node.volatile.store.limits()
    });
    attach::start(node);
    let weak = Arc::downgrade(node);
    let mut stop = node.shutdown_signal();
    node.tasks.spawn(async move {
        let mut iv = tokio::time::interval(TICK);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = iv.tick() => {}
                _ = stop.wait_for(|v| *v) => {
                    if let Some(node) = weak.upgrade() {
                        wipe_all(&node);
                    }
                    break;
                }
            }
            let Some(node) = weak.upgrade() else {
                break;
            };
            if node.is_shutting_down() {
                wipe_all(&node);
                break;
            }
            tick(&node);
        }
    });
}

/// 1초마다: 만료, 참가자 다시 세기, 없어진 채널 지우기, 묘비·대기 정리.
fn tick(node: &Arc<Node>) {
    let Ok(me) = node.me() else {
        return;
    };
    let now = Instant::now();
    let store = &node.volatile.store;
    let mut fx = Effects::default();
    let expired = store.expire(now);
    let mut chans = node.volatile.chans.lock();

    let mut by_chan: HashMap<ChanKey, Vec<EventId>> = HashMap::new();
    for e in expired {
        by_chan.entry((e.space, e.channel)).or_default().push(e.id);
    }
    for (key, ids) in by_chan {
        if let Some(rt) = chans.get_mut(&key) {
            remove_events(store, key, rt, &ids, me.user(), true, &mut fx);
        }
    }

    let mut live: HashSet<ChanKey> = HashSet::new();
    for space in node.space_ids() {
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        if !state.is_member(&me.user()) {
            continue;
        }
        for (c, s) in &state.channels {
            let key = (space, *c);
            if !s.is_volatile() || !can_view(&state, &me.user(), c) || !p2p_open(node, key, &state)
            {
                continue;
            }
            match s.join_mode {
                JoinMode::Always => {
                    if let Some(rt) = chans.get_mut(&key).filter(|rt| rt.mode == JoinMode::Always) {
                        refresh_always(node, &state, key, s, rt, me.device_id(), false, &mut fx);
                        shed_if_full(node, key, rt, &mut fx);
                        check_stale(key, rt, me.device_id(), now, &mut fx);
                        live.insert(key);
                    } else {
                        // 상한에 밀린 채널도 상태는 남겨 자리가 나는지 계속 셉니다.
                        live_rt(node, &mut chans, &state, key, s, me.device_id(), &mut fx);
                        if chans.contains_key(&key) {
                            live.insert(key);
                        }
                    }
                }
                JoinMode::JoinLeave => {
                    if let Some(rt) = chans
                        .get_mut(&key)
                        .filter(|rt| rt.mode == JoinMode::JoinLeave && rt.joined)
                    {
                        refresh_join_leave(node, &state, key, s, rt, me.device_id(), &mut fx);
                        check_stale(key, rt, me.device_id(), now, &mut fx);
                        live.insert(key);
                    }
                }
            }
        }
    }
    // 채널이 없어졌거나·휘발성이 아니게 됐거나·볼 수 없게 됐거나·방식이 바뀌었으면 지웁니다.
    let dead: Vec<ChanKey> = chans
        .keys()
        .filter(|k| !live.contains(k))
        .copied()
        .collect();
    for key in dead {
        if let Some(rt) = chans.remove(&key) {
            wipe(node, key, rt, &mut fx);
        }
    }
    // 상태 없이 남은 RAM 데이터(퇴장과 엇갈려 늦게 온 키 등).
    for (s, c) in store.channels() {
        if !chans.contains_key(&(s, c)) {
            store.drop_channel(&s, &c);
        }
    }
    for (key, rt) in chans.iter_mut() {
        cleanup(store, *key, rt, now);
    }
    drop(chans);
    fx.run(node);
}

/// 묘비·오래 기다린 이벤트·지워진 세대 기록 정리.
fn cleanup(store: &VolatileStore, key: ChanKey, rt: &mut ChanRt, now: Instant) {
    rt.gone
        .retain(|_, t| now.saturating_duration_since(*t) < TOMBSTONE_TTL);
    if rt.gone.len() > MAX_TOMBSTONES {
        let mut by_age: Vec<(Instant, EventId)> = rt.gone.iter().map(|(id, t)| (*t, *id)).collect();
        by_age.sort();
        for (_, id) in by_age.iter().take(rt.gone.len() - MAX_TOMBSTONES) {
            rt.gone.remove(id);
        }
    }
    let stale: Vec<EventId> = rt
        .meta
        .iter()
        .filter(|(_, m)| !m.ready && now.saturating_duration_since(m.since) >= PENDING_TTL)
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        discard(store, key, rt, id, Outcome::Gone);
    }
    rt.pending_key
        .retain(|_, ids| ids.iter().any(|id| rt.meta.contains_key(id)));
    rt.pending_target
        .retain(|_, ids| ids.iter().any(|id| rt.meta.contains_key(id)));
    rt.known_gens.retain(|g| store.has_key(&key.0, &key.1, *g));
    rt.gen_recipients.retain(|g, _| rt.known_gens.contains(g));
    rt.wraps.retain(|_, ws| {
        ws.retain(|w| now.saturating_duration_since(w.at) < EARLY_TTL);
        !ws.is_empty()
    });
    rt.pending_rot
        .retain(|_, p| now.saturating_duration_since(p.at) < EARLY_TTL);
    rt.drop_early(|e| now.saturating_duration_since(e.at) >= EARLY_TTL);
    rt.departed
        .retain(|_, at| now.saturating_duration_since(*at) < EARLY_TTL);
}

/// 진단용 RAM 저장소 통계(내용 없음).
pub(crate) fn stats(node: &Node) -> VolatileStats {
    node.volatile.store.stats()
}

/// 지금 RAM에 있는 휘발성 첨부 블롭 ID(`diag.leakcheck`가 디스크 블롭 저장소와 대조).
pub(crate) fn ram_blob_ids(node: &Node) -> Vec<BlobId> {
    node.volatile.store.blob_ids()
}

/// 시험 도구(`testkit`)용 들여다보기. 키·평문은 내보내지 않습니다.
pub(crate) mod inspect {
    use super::*;

    /// 이 기기가 키를 가진 세대.
    pub(crate) fn generations(node: &Node, space: &SpaceId, channel: &ChannelId) -> Vec<u64> {
        let chans = node.volatile.chans.lock();
        chans
            .get(&(*space, *channel))
            .map(|rt| {
                rt.known_gens
                    .iter()
                    .copied()
                    .filter(|g| node.volatile.store.has_key(space, channel, *g))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 보낼 때 쓰는 세대.
    pub(crate) fn current_generation(
        node: &Node,
        space: &SpaceId,
        channel: &ChannelId,
    ) -> Option<u64> {
        let chans = node.volatile.chans.lock();
        chans
            .get(&(*space, *channel))
            .and_then(|rt| rt.current.as_ref().map(|c| c.generation))
    }

    /// RAM에 있는 암호문 이벤트(논리 시계 순).
    pub(crate) fn raw_events(
        node: &Node,
        space: &SpaceId,
        channel: &ChannelId,
    ) -> Vec<SignedEvent> {
        node.volatile
            .store
            .list(space, channel)
            .iter()
            .map(|e| e.event.clone())
            .collect()
    }

    /// 이 채널에 세대 키가 하나라도 남았는지.
    pub(crate) fn has_any_key(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
        node.volatile
            .store
            .current_generation(space, channel)
            .is_some()
    }

    /// 다음 `n`번의 `peer`로 보내는 입장 알림을 보내지 않고 실패로 처리합니다.
    pub(crate) fn fail_presence_to(node: &Node, peer: PeerId, n: u32) {
        *node
            .volatile
            .faults
            .lock()
            .presence_to
            .entry(peer)
            .or_default() += n;
    }

    /// 다음 `n`번의 `peer`로 보내는 세션 키 갱신을 보내지 않고 실패로 처리합니다.
    pub(crate) fn fail_key_to(node: &Node, peer: PeerId, n: u32) {
        *node.volatile.faults.lock().key_to.entry(peer).or_default() += n;
    }

    /// `peer`로 보내는 세션 키 갱신 중 아직 실패로 처리할 남은 횟수.
    pub(crate) fn key_faults_left(node: &Node, peer: PeerId) -> u32 {
        node.volatile
            .faults
            .lock()
            .key_to
            .get(&peer)
            .copied()
            .unwrap_or(0)
    }

    /// 지금까지 받아 둔 감싸기 수.
    pub(crate) fn wraps_received(node: &Node) -> u64 {
        node.volatile.faults.lock().wraps_in
    }

    /// 그 세대 키의 약속값(키는 내보내지 않음). 키가 없으면 `None`.
    pub(crate) fn key_commit(
        node: &Node,
        space: &SpaceId,
        channel: &ChannelId,
        generation: u64,
    ) -> Option<[u8; 32]> {
        node.volatile
            .store
            .with_key(space, channel, generation, |k| {
                yangpa_crypto::keysched::vol_key_commit(space, channel, generation, k)
            })
    }

    /// 이 기기와 `to` 기기 둘만 받는 새 회전 공지(약속값은 `commit_key`)를 만듭니다(적용·전송하지 않음).
    pub(crate) fn forge_rotation(
        node: &Node,
        space: SpaceId,
        channel: ChannelId,
        to: DeviceId,
        commit_key: &Secret32,
    ) -> Result<(SignedEvent, u64, Vec<DeviceId>)> {
        use yangpa_crypto::keysched;
        let me = node.me()?;
        let generation = rand::random::<u64>() | 1;
        let mut recipients = vec![me.device_id(), to];
        recipients.sort();
        let body = EventBody::VolatileRotation(VolatileRotationBody {
            generation,
            parent: None,
            fresh: true,
            added: recipients.clone(),
            removed: Vec::new(),
            recipients_hash: keysched::vol_recipients_hash(&recipients),
            commit: keysched::vol_key_commit(&space, &channel, generation, commit_key),
        });
        let extra = volatile_parent(None, node.store.max_lamport(&space)?);
        let ev = author::author_event(
            node,
            &space,
            Some(channel),
            &body,
            &KeyChoice::Volatile(0, keysched::vol_notice_key(&space, &channel)),
            &extra,
        )?;
        Ok((ev, generation, recipients))
    }

    /// `to`에게 `wrap_key`를 봉인한 그 세대 감싸기(AD는 `commit_key`의 약속값). 둘이 다르면 거짓 감싸기입니다.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn forge_wrap(
        node: &Node,
        space: SpaceId,
        channel: ChannelId,
        generation: u64,
        to: DeviceId,
        commit_key: &Secret32,
        wrap_key: &Secret32,
    ) -> Result<Vec<u8>> {
        use yangpa_crypto::keysched;
        let state = node
            .space_state(&space)
            .ok_or_else(|| Error::not_found("모르는 공간"))?;
        let user = node
            .sync
            .index
            .get(&space, &state)
            .device_user(&to)
            .ok_or_else(|| Error::not_found("모르는 기기"))?;
        let pk = crate::devkeys::device_box_key(node, &state, &user, &to)
            .ok_or_else(|| Error::not_found("상자 키 없음"))?;
        let commit = keysched::vol_key_commit(&space, &channel, generation, commit_key);
        let ad = keysched::vol_wrap_ad(&space, &channel, generation, &to, &commit);
        Ok(yangpa_crypto::seal_to(&[(to, pk)], &ad, &wrap_key.0))
    }

    /// `from` 피어가 감싸기를 보낸 것처럼 처리합니다.
    pub(crate) fn deliver_wrap(
        node: &Arc<Node>,
        from: PeerId,
        space: SpaceId,
        channel: ChannelId,
        generation: u64,
        wrap: Vec<u8>,
        recipients: Option<Vec<DeviceId>>,
    ) -> YpResponse {
        handle_key_wrap(node, from, space, channel, generation, wrap, recipients)
    }

    /// 다음 `n`번의 스냅숏 받아 오기를 건너뜁니다.
    pub(crate) fn fail_snapshot(node: &Node, n: u32) {
        node.volatile.faults.lock().snapshot += n;
    }

    /// RAM 첨부 블롭 하나의 상태(내용 없음).
    pub(crate) fn blob_info(
        node: &Node,
        space: &SpaceId,
        blob: &BlobId,
    ) -> Option<yangpa_store::RamBlobInfo> {
        let channel = node.volatile.store.find_blob(space, blob)?;
        node.volatile.store.blob_info(space, &channel, blob)
    }

    /// 대기열에 있거나 받는 중인 첨부 수.
    pub(crate) fn fetching(node: &Node) -> usize {
        node.volatile.attach.fetching()
    }

    /// 첨부 받기 시험용 조절(멈춤 판정·포기 시간, 받다 멈추기).
    pub(crate) fn tune_fetch(node: &Node, tuning: attach::Tuning) {
        attach::set_tuning(node, tuning);
    }

    /// 다음 `n`번의 첨부 묶기를 실패로 처리합니다.
    pub(crate) fn fail_claims(node: &Node, n: u32) {
        attach::fail_claims(node, n);
    }

    /// 지금 보내는 세대 키로 메시지를 만들어 서명만 합니다(적용·전송하지 않음). `lamport`를 주면 그 값을 씁니다.
    pub(crate) fn author_unsent(
        node: &Node,
        space: SpaceId,
        channel: ChannelId,
        text: &str,
        lamport: Option<u64>,
    ) -> Result<SignedEvent> {
        use yangpa_core::event::{EVENT_VERSION, EventHeader};
        let me = node.me()?;
        let generation = current_generation(node, &space, &channel)
            .ok_or_else(|| Error::conflict("보낼 세션 키가 없음"))?;
        let secret = node
            .volatile
            .store
            .with_key(&space, &channel, generation, Secret32::clone)
            .ok_or_else(|| Error::conflict("세션 키가 없음"))?;
        let lamport = match lamport {
            Some(l) => l,
            None => node.store.max_lamport(&space)?.saturating_add(1),
        };
        let body = EventBody::Message(MessageBody {
            text: text.to_owned(),
            attachments: Vec::new(),
            embeds: Vec::new(),
            reply_to: None,
            thread: None,
            mentions: Vec::new(),
        });
        let header = EventHeader {
            v: EVENT_VERSION,
            space,
            channel: Some(channel),
            kind: body.kind(),
            author: me.device_id(),
            user: me.user(),
            parents: Vec::new(),
            lamport,
            created_at: Timestamp::now(),
            key: KeyRef::Volatile { generation },
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        };
        let bytes = yangpa_crypto::encrypt_body(&secret, &header, &body)?;
        SignedEvent::sign(header, bytes, &me.device.signing).map_err(Error::from)
    }

    /// `from` 피어가 스냅숏으로 답한 것처럼 항목을 적용합니다(응답자 검사·논리 시계 상한 포함).
    pub(crate) fn deliver_snapshot(
        node: &Arc<Node>,
        space: SpaceId,
        channel: ChannelId,
        from: PeerId,
        items: Vec<(SignedEvent, Option<u64>)>,
    ) -> Result<()> {
        let (_, device) = node
            .peers
            .identity_of(&from)
            .ok_or_else(|| Error::not_found("인증되지 않은 피어"))?;
        let items = items
            .into_iter()
            .map(|(event, remaining_ms)| VolatileItem {
                event,
                remaining_ms,
            })
            .collect();
        apply_snapshot(node, space, channel, device, items);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 회전 비용(performance.md "휘발성 세션 키"): 감싸기 하나와 공지 본문 크기의 상한.
    #[test]
    fn rotation_wire_sizes_stay_small() {
        let kp = yangpa_crypto::BoxKeypair::generate();
        let d = DeviceId::from_bytes([3; 32]);
        let wrap = yangpa_crypto::seal_to(&[(d, kp.public())], b"ad", &[9u8; 32]);
        let body = |added: usize, removed: usize, fresh: bool| {
            EventBody::VolatileRotation(VolatileRotationBody {
                generation: u64::MAX,
                parent: Some(1),
                fresh,
                added: (0..added)
                    .map(|i| DeviceId::from_bytes([i as u8; 32]))
                    .collect(),
                removed: (0..removed)
                    .map(|i| DeviceId::from_bytes([200 + i as u8; 32]))
                    .collect(),
                recipients_hash: [0; 32],
                commit: [0; 32],
            })
            .encode()
            .expect("인코딩")
            .len()
        };
        let (join, leave) = (body(1, 0, false), body(0, 1, true));
        eprintln!(
            "감싸기 {}B, 가입 공지 본문 {join}B, 나감 공지 본문 {leave}B",
            wrap.len()
        );
        assert!(wrap.len() <= 1400, "{}", wrap.len());
        assert!(join <= 160 && leave <= 160, "{join} {leave}");
    }

    #[test]
    fn volatile_parent_lamport_stays_near_stored_max() {
        let id = EventId::from_bytes([7; 32]);
        assert!(volatile_parent(None, 10).is_empty());
        assert_eq!(volatile_parent(Some((15, id)), 10), vec![(id, 15)]);
        // 휘발성 이벤트만 오가 머리가 저장된 최댓값보다 많이 앞서도 부모 논리 시계는 상한에서 자름.
        let far = 10 + VOLATILE_LAMPORT_LEAD + 5_000;
        assert_eq!(
            volatile_parent(Some((far, id)), 10),
            vec![(id, 10 + VOLATILE_LAMPORT_LEAD)]
        );
        assert_eq!(
            volatile_parent(Some((u64::MAX, id)), u64::MAX - 1),
            vec![(id, u64::MAX)]
        );
        // 받는 쪽 부풀리기 방어 안: 다음 논리 시계(+1)도 known + LAMPORT_SLACK을 넘지 않음.
        const { assert!(VOLATILE_LAMPORT_LEAD < LAMPORT_SLACK) };
    }
}
