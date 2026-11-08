//! 이벤트 적용. 이 기기가 만든 이벤트와 받은 이벤트가 모두 이 경로를 탑니다(docs/daemon.md "받은 이벤트 처리").
//!
//! 1. 서명·공간 확인, 중복 제거, 논리 시계 부풀림(2^20) 보류.
//! 2. 채널 스트림: 휘발성이면 `volatile::on_inbound_event`로 넘기고 끝(디스크로 가지 않음). 채널을 아직 모르면 RAM 보류.
//!    작성자가 아직 모르는 멤버·기기이면(가입·기기 목록 갱신이 늦게 옴) 저장하지 않고 RAM 보류(공간 스트림 키 묶음도).
//! 3. 레인별: 상태(상태 키로 열어 해소기에), 키 합의(크루 `KeyUpdate`는 상태 이벤트처럼 열어 해소기에, 받아들인 갱신은
//!    `keygroup::member`가 해소 순서대로 적용), 키(봉인·상태 키로 열고 권한을 확인한 뒤에 저장), 메시지(채널 뿌리의
//!    기록 키로 열어 권한 확인 → 보기 행·알림. 모르는 뿌리는 보관한 그룹 키에서 파생해 보고, 없으면 멤버에게 묻습니다).
//! 4. 키·대상이 없으면 저장만 하고 대기 목록에 올렸다가 키가 오면 다시 처리합니다. 수정·삭제·반응의 대상은 같은
//!    공간·같은 채널의 메시지(`Message`·`ForumPost`)여야 하고, 대상을 모르면 권한이 있어도 대상이 올 때까지 기다립니다.
//!
//! 5. 부분 전달 이벤트(받은편지함·키 그룹)는 해소기에 넣지 않습니다. 기준점(`state_anchor`)의 조상이 모두 오면 그
//!    지점의 상태로(가입·기기 연결 중 믿는 피어에게서 받은 것이 아니면 지금 상태로도) 권한을 확인한 뒤에만 저장하고, 그
//!    전에는 RAM에 둡니다(`scoped::ScopedQueue`).
//!
//! 모든 함수는 적용 잠금([`lock`])을 쥔 채 동기로 돕니다(부모·논리 시계·해소기·키 그룹 순서가 엇갈리지 않게).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, warn};
use yangpa_core::body::{EventBody, KeyShareBody, Secret32, SharedKey};
use yangpa_core::event::{EventHeader, EventKind, KeyRef, Lane, Scope, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, EventId, SpaceId, StateKeyId};
use yangpa_core::model::{Permissions, SecurityLevel, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_crypto::{CryptoError, DeviceBundle, decrypt_body, open_sealed_body};
use yangpa_net::PeerId;
use yangpa_rpc::api::{CheckpointAlertKind, Notification};
use yangpa_store::{
    EditUpdate, ForumMeta, MessageContent, MessageKind, MessageRecord, PersistentEvent,
    ReactionUpdate,
};
use yangpa_sync::state::state_key_handover_only;
use yangpa_sync::{
    AnchorState, AuthError, DecodedEvent, LinkEvent, authorize_message,
    authorize_revoked_device_message, authorize_scoped,
};

use crate::error::{Error, Result};
use crate::keygroup::{self, MemberState};
use crate::node::Node;
use crate::ratelimit::{self, EventClass};
use crate::scoped::{self, PendingScoped, ScopedQueue};
use crate::spaces::{self, Provisional, SpaceCore};
use crate::volatile::{self, VolatileVerdict};
use crate::{blobs, crews, diag, keys, messages, sync_service};

/// `old`에서 `new`로 가며 새로 공간의 활성 기기가 된 기기. 바뀐 멤버만 봅니다(4.5만 대 집합을 매번 만들지 않음).
fn new_active_devices(old: Option<&SpaceState>, new: &SpaceState) -> Vec<DeviceId> {
    let Some(old) = old else {
        return new.active_devices().map(|(_, d)| d.device).collect();
    };
    let mut out = Vec::new();
    for u in old.changed_members(new) {
        let before: HashSet<DeviceId> = old.member_active_devices(&u).map(|d| d.device).collect();
        out.extend(
            new.member_active_devices(&u)
                .map(|d| d.device)
                .filter(|d| !before.contains(d)),
        );
    }
    out
}

/// 내가 아는 공간 최댓값보다 이만큼 넘게 큰 논리 시계는 바로 적용하지 않습니다(protocol.md).
pub(crate) const LAMPORT_SLACK: u64 = 1 << 20;
/// RAM 보류 상한(공간마다): 개수와 대략의 바이트.
const MAX_HELD: usize = 4096;
const MAX_HELD_BYTES: usize = 64 * 1024 * 1024;
/// 정리 한 번에 도는 바퀴 상한.
const MAX_SETTLE_ROUNDS: usize = 16;
/// 이벤트가 어디서 왔는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// 이 기기가 만든 것.
    Local,
    /// 인증된 피어가 보낸 것(push·gossip·따라잡기).
    Peer(PeerId),
    /// 친구 수락·초대 직후 받은 처음 이벤트(인증 전).
    Bootstrap(PeerId),
    /// 시작 때 저장소에서 다시 적용(알림 없음).
    Replay,
}

impl Source {
    fn notifies(&self) -> bool {
        !matches!(self, Source::Replay)
    }

    fn peer(&self) -> Option<PeerId> {
        match self {
            Source::Peer(p) | Source::Bootstrap(p) => Some(*p),
            _ => None,
        }
    }
}

/// 이벤트가 기다리는 것.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Need {
    /// 상태 키(크루 S든 그룹 S_g든).
    StateKey(StateKeyId),
    /// 채널 뿌리(메시지의 기록 키). 채널은 그룹 키에서 뿌리를 다시 파생해 볼 때 씁니다.
    Record(ChannelId, ChannelRootId),
    /// 수정·삭제 대상 메시지(작성자 확인용).
    Target(EventId),
    /// 공간 상태(상태 전에 온 키 묶음).
    SpaceState,
}

/// 적용 결과.
#[derive(Clone, Debug, Default)]
pub struct IngestReport {
    /// 새로 저장·적용했거나 키를 기다리며 저장한 이벤트.
    pub accepted: Vec<EventId>,
    pub duplicate: usize,
    /// 거부(저장하지 않음, 또는 상태 규칙 위반으로 적용하지 않음)와 이유.
    pub rejected: Vec<(EventId, String)>,
    /// `rejected` 가운데 지금 상태에 대한 검사(권한·멤버십·상태 규칙)로 갈린 것. 보낸 피어는 다른 시점의 상태로
    /// 검사했을 수 있어(권한 회수를 아직 모름 등) gossip에서 전달한 피어를 감점하지 않습니다.
    pub rejected_by_state: Vec<EventId>,
    /// RAM에 보류(채널을 아직 모름·논리 시계 부풀림).
    pub held: usize,
    /// 키·대상을 기다리며 저장만 한 수.
    pub waiting: usize,
    /// 휘발성 영역으로 넘긴 수.
    pub volatile: usize,
    /// 부모 중 저장소에 없는 것이 있음(따라잡기 필요).
    pub missing_parents: bool,
    /// 해소된 공간 상태가 바뀜.
    pub state_changed: bool,
    /// 이번에 새로 공간의 활성 기기가 된 기기(그 기기와 따라잡기).
    pub new_devices: Vec<DeviceId>,
    /// 작성 기기별 속도 제한으로 버린 상태·키 합의 이벤트(저장하지 않음, 따라잡기로 다시 받음, [`ingest_live`]).
    pub throttled: Vec<EventId>,
    /// 남의 받은편지함 이벤트를 전달자로서 저장한 것(받아 준 가입자에게 넘김, [`after_unlock`]).
    pub relay: Vec<SignedEvent>,
    /// 해소기가 새 체크포인트를 바탕으로 삼음(스냅숏 블롭·바탕 기록, [`after_unlock`]).
    pub checkpoint: bool,
    /// 늦은 이벤트(경계 아래 부모)·은퇴한 상태 키로 거부된 이벤트. 내 것이면 다시 냅니다([`after_unlock`]).
    pub late: Vec<EventId>,
    /// 크루 키 갱신을 새로 적용함(해소된 상태는 그대로일 수 있음): 들고 있던 표가 낡았는지 봅니다.
    pub key_chain_moved: bool,
    /// 짧은 서명을 확인할 서명 키가 없어 보류한 이벤트의 작성 기기(피어에게 키를 물음, [`after_unlock`]).
    pub missing_keys: Vec<DeviceId>,
}

impl IngestReport {
    pub(crate) fn merge(&mut self, o: IngestReport) {
        self.accepted.extend(o.accepted);
        self.duplicate += o.duplicate;
        self.rejected.extend(o.rejected);
        self.rejected_by_state.extend(o.rejected_by_state);
        self.held += o.held;
        self.waiting += o.waiting;
        self.volatile += o.volatile;
        self.missing_parents |= o.missing_parents;
        self.state_changed |= o.state_changed;
        self.new_devices.extend(o.new_devices);
        self.throttled.extend(o.throttled);
        self.relay.extend(o.relay);
        self.checkpoint |= o.checkpoint;
        self.late.extend(o.late);
        self.key_chain_moved |= o.key_chain_moved;
        self.missing_keys.extend(o.missing_keys);
    }

    pub fn is_rejected(&self, id: &EventId) -> bool {
        self.rejected.iter().any(|(x, _)| x == id)
    }

    pub fn is_accepted(&self, id: &EventId) -> bool {
        self.accepted.contains(id) && !self.is_rejected(id)
    }

    /// `ids` 가운데 상태와 상관없는 이유(서명·형식·암호·레인 등)로 거부된 것이 있는지. 전달한 피어도 같은 검사를
    /// 했어야 하므로 이런 이벤트를 넘긴 피어는 잘못 전달한 것입니다.
    pub fn has_invalid(&self, ids: &HashSet<EventId>) -> bool {
        self.rejected
            .iter()
            .any(|(id, _)| ids.contains(id) && !self.rejected_by_state.contains(id))
    }

    /// `ids` 가운데 지금 상태로 갈려 거부된 것이 있는지.
    pub fn has_rejected_by_state(&self, ids: &HashSet<EventId>) -> bool {
        self.rejected_by_state.iter().any(|id| ids.contains(id))
    }
}

/// 적용 잠금을 쥐고 있다는 표시.
///
/// 잠금 순서(교착 방지): ① 적용 관문(`Node::ingest_gate`) → ② 공간 잠금(`Node::ingest_spaces`의 그 공간 것) →
/// ③ `SpaceCore`(parking_lot) → ④ `Node::spaces` 표(잠깐만). 공간 하나만 다루는 쪽([`lock_space`])은 관문을 읽기로
/// 쥐고 그 공간 잠금 하나만 쥡니다(공간 잠금을 둘 쥐지 않음). 공간 하나를 만들거나 지우는 것도 여기에 듭니다. 여러
/// 공간을 다루는 쪽([`lock`]: 신원 지우기)만 관문을 쓰기로 쥐어 모든 공간을 막고, 공간 잠금은 쥐지 않습니다.
/// 관문은 쓰기 대기가 있으면 새 읽기를 세우므로, 큰 크루의 긴 적용 중에 쓰기를 기다리면 그동안 모든 공간이
/// 멈춥니다. 어느 쪽도 잠금을 쥔 채 다른 적용 잠금을 기다리지 않습니다: tokio 잠금은 다시 들어갈 수 없고, 관문은
/// 쓰기 대기가 있으면 새 읽기를 세우므로 읽기를 쥔 채 또 읽기를 기다리면 막힙니다. 적용 함수(`ingest_locked` 등)는
/// 잠금이 그 공간을 덮는지 확인합니다([`Locked::covers`]). 부분 전달 이벤트(받은편지함·키 그룹)의 받기·다시 셈도
/// 같은 공간 잠금([`lock_space`]) 아래에서 합니다. 노드 수준의 짧은 잠금(받은편지함 전달자 표 `crews`의
/// `inbox_relays`, 피어 표, 공간마다 따라잡기가 읽는 부분 전달 기억 `SpaceRuntime::scoped_seen` 등)은 잎이라 어느
/// 단계에서나 잠깐 쥘 수 있고, 쥔 채로 다른 잠금을 잡지 않습니다. 비동기 작업은 공간 잠금 없이 `SpaceCore`를 잡지
/// 않습니다: 적용은 `SpaceCore`를 적용 내내 쥐므로, 그 사이 잡으려는 작업은 기다리는 동안 작업 스레드를 붙잡습니다.
pub(crate) struct Locked<'a> {
    _gate: Gate<'a>,
    _space: Option<tokio::sync::OwnedMutexGuard<()>>,
    /// 덮는 공간(`None`이면 모두).
    scope: Option<SpaceId>,
}

/// 쥔 관문(놓을 때까지 들고만 있음).
#[allow(dead_code)]
enum Gate<'a> {
    Read(tokio::sync::RwLockReadGuard<'a, ()>),
    Write(tokio::sync::RwLockWriteGuard<'a, ()>),
}

impl Locked<'_> {
    /// 이 잠금으로 `space`에 쓸 수 있는지.
    pub(crate) fn covers(&self, space: &SpaceId) -> bool {
        self.scope.is_none_or(|s| s == *space)
    }

    pub(crate) fn check(&self, space: &SpaceId) -> Result<()> {
        if self.covers(space) {
            Ok(())
        } else {
            Err(Error::internal("다른 공간의 적용 잠금으로 적용하려 함"))
        }
    }
}

/// 모든 공간의 적용 잠금(여러 공간을 다루거나 공간을 만들고 지울 때).
pub(crate) async fn lock(node: &Node) -> Locked<'_> {
    Locked {
        _gate: Gate::Write(node.ingest_gate.write().await),
        _space: None,
        scope: None,
    }
}

/// 공간 하나의 적용 잠금. 다른 공간의 적용과는 동시에 돕니다.
pub(crate) async fn lock_space(node: &Node, space: SpaceId) -> Locked<'_> {
    let gate = node.ingest_gate.read().await;
    let m = space_mutex(node, space);
    Locked {
        _gate: Gate::Read(gate),
        _space: Some(m.lock_owned().await),
        scope: Some(space),
    }
}

/// 그 공간의 잠금. 쥐거나 기다리는 쪽이 들고 있는 동안만 살아 있으므로, 없으면 새로 만들고 그때 죽은 것을 치웁니다
/// (표가 지금 쓰는 공간 수만큼만 남음).
fn space_mutex(node: &Node, space: SpaceId) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = node.ingest_spaces.lock();
    if let Some(m) = map.get(&space).and_then(std::sync::Weak::upgrade) {
        return m;
    }
    map.retain(|_, w| w.strong_count() > 0);
    let m = Arc::new(tokio::sync::Mutex::new(()));
    map.insert(space, Arc::downgrade(&m));
    m
}

/// 잠그고 적용한 뒤, 잠금 밖에서 할 일(커밋 확정)을 띄웁니다.
pub(crate) async fn ingest(
    node: &Arc<Node>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    source: Source,
) -> Result<IngestReport> {
    let l = lock_space(node, space).await;
    let r = ingest_locked(node, &l, space, events, source);
    drop(l);
    if let Ok(r) = &r {
        after_unlock(node, space, r);
    }
    r
}

/// 곧바로 받는 길(gossip·`PushEvents`, 그렇게 받은 이벤트의 빈 부모를 채우는 따라잡기)로 온 묶음. 큰 크루에서는 이미
/// 가진 것을 뺀 상태·키 합의 레인 이벤트에 작성 기기별 속도 제한을 겁니다(`ratelimit::EventLimiter`, 넘친 것은
/// [`IngestReport::throttled`]). 제한 없는 따라잡기로 받은 것은 [`ingest`]로 받으므로, 버린 이벤트도 나중에 들어옵니다.
pub(crate) async fn ingest_live(
    node: &Arc<Node>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    peer: PeerId,
) -> Result<IngestReport> {
    let l = lock_space(node, space).await;
    let r = run(node, space, events, Source::Peer(peer), false, true);
    drop(l);
    if let Ok(r) = &r {
        after_unlock(node, space, r);
    }
    r
}

/// 잠금을 푼 뒤: 상태가 바뀌었고 내가 확정 기기면 키 그룹을 맞춥니다(아니면 들고 있던 표를 넘김). 새로 멤버 기기가
/// 된 피어와는 따라잡기를 돌려, 그 기기의 이벤트가 상태보다 먼저 와서 보류됐거나 거절됐던 것을 받아 옵니다.
pub(crate) fn after_unlock(node: &Arc<Node>, space: SpaceId, r: &IngestReport) {
    // 추방·밴·기기 폐기도 묶음 창을 엽니다(늘어나는 빚이 창을 2초까지 늘림). 한 건마다 바로 돌면 대량 추방이 추방
    // 수만큼의 크루 Rotate가 됩니다. 창을 건너뛰는 것은 명시적 요청과 기다리는 보내는 쪽뿐입니다(`keygroup::urgent`).
    if r.state_changed {
        keygroup::on_state_changed(node, space);
    } else if r.key_chain_moved {
        keygroup::on_key_chain_moved(node, space);
    }
    if !r.relay.is_empty() {
        sync_service::deliver_scoped(node, space, r.relay.clone());
    }
    if r.checkpoint {
        crate::checkpoint::on_accepted(node, space);
    }
    if !r.late.is_empty() {
        crate::checkpoint::resign(node, space, r.late.clone());
    }
    if !r.new_devices.is_empty() {
        for (peer, _, device) in node.peers.authed_peers() {
            if r.new_devices.contains(&device) {
                sync_service::spawn_catch_up(node, space, peer);
            }
        }
    }
    if !r.missing_keys.is_empty() {
        fetch_sign_keys(node, space, &r.missing_keys);
    }
}

/// 짧은 서명을 확인할 기기 키를 피어에게 묻고(체크포인트로 들어와 가입 이벤트의 키 목록을 못 본 기기), 얻었으면
/// 보류한 이벤트를 다시 봅니다. 같은 기기는 잠시 다시 묻지 않습니다(가짜 작성자가 물음을 부풀리지 못하게).
fn fetch_sign_keys(node: &Arc<Node>, space: SpaceId, devices: &[DeviceId]) {
    let want = node.devkeys.start_asking(space, devices);
    if want.is_empty() {
        return;
    }
    let node = node.clone();
    tokio::spawn(async move {
        let n = want.len();
        let got = crate::checkpoint::prefetch_device_keys(&node, space, want).await;
        debug!(asked = n, got, "짧은 서명 키 물음");
        if got > 0 {
            retry_held(&node, space).await;
        }
    });
}

/// RAM에 보류한 이벤트를 다시 봅니다(밖에서 기기 키를 얻은 뒤).
async fn retry_held(node: &Arc<Node>, space: SpaceId) {
    let l = lock_space(node, space).await;
    let r = run_with(node, space, Vec::new(), Source::Local, false, false, |cx| {
        cx.retry_held = true;
    });
    drop(l);
    match &r {
        Ok(r) => after_unlock(node, space, r),
        Err(e) => debug!(error = %e, "보류한 이벤트 다시 보기 실패"),
    }
}

/// 기다리는 부분 전달 이벤트를 다시 봅니다(유예 작업·따라잡기 뒤). 아직 기다리는 것이 있으면 true(없으면 유예 작업을
/// 내려놓음).
async fn recheck_scoped(node: &Arc<Node>, space: SpaceId) -> bool {
    let l = lock_space(node, space).await;
    let core = node.spaces.read().get(&space).map(|rt| rt.core.clone());
    let Some(core) = core else {
        return false;
    };
    // `SpaceCore`는 공간 잠금을 쥔 동안에만 만집니다(잠금 밖에서 잡으면 다음 긴 적용 내내 작업 스레드가 멈춤).
    let still_waiting = |core: &parking_lot::Mutex<SpaceCore>| {
        let mut c = core.lock();
        if c.scoped.pending.is_empty() {
            c.scoped.timer = false;
            return false;
        }
        true
    };
    if !still_waiting(&core) {
        return false;
    }
    core.lock().scoped.checked_at = u64::MAX;
    let r = run(node, space, Vec::new(), Source::Local, false, false);
    let more = still_waiting(&core);
    drop(l);
    match &r {
        Ok(r) => after_unlock(node, space, r),
        Err(e) => debug!(error = %e, "부분 전달 이벤트 다시 보기 실패"),
    }
    more
}

/// 따라잡기 한 바퀴(공간 스트림과 받은편지함)가 끝났습니다(`started`에 시작). 그 전에 온 부분 전달 이벤트는 받을 수
/// 있는 것을 다 받아 본 것이므로 남은 빈 구간을 빈 구간으로 보고 다시 셉니다. 따라잡기 작업에서 부르므로 적용 잠금
/// 없이 잎 잠금([`scoped::ScopedSeen`])만 잠깐 쥐고, 다시 세는 일은 공간 잠금을 기다리는 작업에 넘깁니다.
pub(crate) fn note_caught_up(node: &Arc<Node>, space: SpaceId, started: Instant) {
    let seen = node
        .spaces
        .read()
        .get(&space)
        .map(|rt| rt.scoped_seen.clone());
    let Some(seen) = seen else { return };
    let due = seen.lock().note_caught_up(started);
    if due {
        node.spawn_detached(move |node| async move {
            recheck_scoped(&node, space).await;
        });
    }
}

/// 새로 온 이벤트 묶음(중복은 건너뜀).
pub(crate) fn ingest_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    source: Source,
) -> Result<IngestReport> {
    l.check(&space)?;
    run(node, space, events, source, false, false)
}

/// 이미 저장된 이벤트를 다시 처리합니다(시작 때 다시 세우기, 내 커밋 저장 직후).
pub(crate) fn ingest_stored_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    source: Source,
) -> Result<IngestReport> {
    l.check(&space)?;
    run(node, space, events, source, true, false)
}

/// 밖(멤버 중계·조각 블롭)에서 얻은 키로 기다리던 이벤트를 다시 봅니다.
pub(crate) async fn wake_needs(node: &Arc<Node>, space: SpaceId, needs: Vec<Need>) {
    let l = lock_space(node, space).await;
    let r = run_with(node, space, Vec::new(), Source::Local, false, false, |cx| {
        for n in needs {
            cx.wake(n);
        }
    });
    drop(l);
    match &r {
        Ok(r) => after_unlock(node, space, r),
        Err(e) => debug!(error = %e, "키를 기다리던 이벤트 다시 보기 실패"),
    }
}

/// 밖에서 얻은 키 그룹 재료(기기 몫 묶음, 중계받은 T·R)를 멤버 상태에 넣고 정리를 한 번 돌립니다.
/// `f`의 결과를 돌려줍니다(적용하지 못했으면 기본값).
pub(crate) async fn offer_keys<T: Default + Send>(
    node: &Arc<Node>,
    space: SpaceId,
    f: impl FnOnce(&mut MemberState) -> T + Send,
) -> T {
    let l = lock_space(node, space).await;
    let mut out = T::default();
    let r = run_with(node, space, Vec::new(), Source::Local, false, false, |cx| {
        cx.core.keys.ensure_loaded(cx.node, &cx.space);
        out = f(&mut cx.core.keys);
        // 정리가 키 그룹 상태를 다시 보게 합니다(아니면 받은 재료가 다음 갱신이 올 때까지 쓰이지 않음).
        cx.keys_dirty = true;
    });
    drop(l);
    match &r {
        Ok(r) => after_unlock(node, space, r),
        Err(e) => debug!(error = %e, "받은 키 그룹 재료 적용 실패"),
    }
    out
}

/// 시작 때: 저장된 키 그룹 스트림 갱신으로 그룹 사슬(RAM 전용)을 다시 세우고 정리를 한 번 돌립니다. 저장할 때 기준점
/// 상태로 권한을 확인한 것들이고, 사슬은 기준점의 확정 기기·순번 규칙을 다시 봅니다.
pub(crate) fn replay_key_groups_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    mut events: Vec<SignedEvent>,
) -> Result<()> {
    l.check(&space)?;
    if events.is_empty() {
        return Ok(());
    }
    events.sort_by_cached_key(|e| (e.header.lamport, e.id()));
    run_with(
        node,
        space,
        Vec::new(),
        Source::Replay,
        false,
        false,
        |cx| {
            let core = &mut *cx.core;
            core.keys.ensure_loaded(cx.node, &cx.space);
            for ev in &events {
                if let StreamId::KeyGroup(g) = ev.header.stream() {
                    core.keys
                        .on_group_event(cx.node, &cx.space, &core.resolver, ev, g);
                }
            }
            cx.keys_dirty = true;
        },
    )?;
    Ok(())
}

fn run(
    node: &Arc<Node>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    source: Source,
    force: bool,
    live: bool,
) -> Result<IngestReport> {
    run_with(node, space, events, source, force, live, |_| {})
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    node: &Arc<Node>,
    space: SpaceId,
    mut events: Vec<SignedEvent>,
    source: Source,
    force: bool,
    live: bool,
    pre: impl FnOnce(&mut Cx<'_>),
) -> Result<IngestReport> {
    let (core, state, summary) = {
        let spaces = node.spaces.read();
        let rt = spaces
            .get(&space)
            .ok_or_else(|| Error::not_found("모르는 공간"))?;
        (rt.core.clone(), rt.state.clone(), rt.key_summary.clone())
    };
    // P2P 전용 채널의 채널 스트림은 P2P 스웜에서, 나머지는 Tor에서 온 것만 받습니다(다른 길로 온 것은 조용히 버림).
    if let Some(peer) = source.peer() {
        let st = state.as_deref();
        events.retain(|e| crate::p2p::accepts(node, &peer, &space, st, e));
    }
    let mut core = core.lock();
    events.sort_by_cached_key(|e| (e.header.lamport, e.id()));
    // 전달 지연(진단)은 피어가 보낸 메시지만 셉니다.
    let sent = match source {
        Source::Peer(_) => diag::sent_times(&events),
        _ => Vec::new(),
    };
    let mut cx = Cx {
        node,
        space,
        source,
        core: &mut core,
        state,
        report: IngestReport::default(),
        decoded: Vec::new(),
        links: Vec::new(),
        woken: BTreeSet::new(),
        retry_held: false,
        retrying_held: false,
        waiting_dirty: false,
        live,
        known_keys: None,
        keys_dirty: false,
        rejudge: false,
        ask_roots: Vec::new(),
        ask_state: Vec::new(),
        new_state_keys: Vec::new(),
        space_updated: false,
    };
    pre(&mut cx);
    let settled = (|| {
        for ev in events {
            cx.process(ev, force)?;
        }
        // 제한은 이 묶음의 이벤트에만: 다시 꺼내 보는 보류·대기 이벤트는 이미 받은 것입니다.
        cx.live = false;
        cx.settle()
    })();
    // 적용 잠금 없이 읽는 쪽(보내는 쪽 규칙·`space.get`)에 끝난 상태를 보입니다. 실패해도 바뀐 만큼은 보여야 합니다.
    *summary.lock() = Some(Arc::new(crate::keygroup::KeySummary::of(
        &space,
        &cx.core.keys,
        &cx.core.resolver,
    )));
    // `space.updated`는 요약을 쓴 뒤에 냅니다. 적용 중에 내면 앱이 곧바로 읽은 `space.get`이 옛 요약으로 커밋 대기를
    // 그대로 보여 주고, 다음 알림이 올 때까지 남습니다.
    if cx.space_updated {
        node.notify.publish(Notification::SpaceUpdated { space });
    }
    settled?;
    diag::on_ingest(node, &cx.report.accepted, &sent);
    Ok(cx.report)
}

/// 이 이벤트를 쓴 기기가 그 사용자의 폐기된 기기인지.
fn is_revoked(ev: &SignedEvent, state: &SpaceState) -> bool {
    let h = &ev.header;
    state
        .devices
        .get(&h.user)
        .is_some_and(|l| l.is_revoked(&h.author))
}

/// 기준점 상태를 센 결과(같은 묶음의 다른 이벤트에 다시 씀).
#[derive(Clone)]
enum At {
    /// 그 상태(공간 생성이 없으면 `None`)와, 빈 구간 없이 셌는지.
    Ready(Option<Arc<SpaceState>>, bool),
    Incomplete {
        unknown: Vec<EventId>,
        undecoded: Vec<EventId>,
    },
}

/// 키 묶음의 상태 키를 지금 상태 키 후보로 삼을지(`keys::put_state_key`).
#[derive(Clone, Copy)]
enum Promote {
    /// 공간 스트림 묶음: 확정 기기 인계로만 통과한 것이 아니면 후보.
    ByShare,
    /// 확정 기기가 받은편지함으로 보낸 묶음: 마지막 상태 키만, 이 순위(묶음의 논리 시계)로 후보.
    Ranked(u64),
    /// 저장만.
    Never,
}

/// 처리 중 문맥.
struct Cx<'a> {
    node: &'a Arc<Node>,
    space: SpaceId,
    source: Source,
    core: &'a mut SpaceCore,
    state: Option<Arc<SpaceState>>,
    report: IngestReport,
    decoded: Vec<DecodedEvent>,
    links: Vec<LinkEvent>,
    woken: BTreeSet<EventId>,
    retry_held: bool,
    /// 보류를 다시 보는 중(그 이벤트를 내준 피어는 이 묶음의 피어가 아닐 수 있음).
    retrying_held: bool,
    waiting_dirty: bool,
    /// 실시간 전파로 받은 묶음을 처리하는 중(상태·키 합의 이벤트에 작성 기기별 제한, [`ingest_live`]).
    live: bool,
    /// 시작 때 다시 적용하는 동안 이 공간에 이미 있는 기기 키 행의 약속(처음 필요할 때 한 번 읽음). 4.5만 대 크루를
    /// 다시 세울 때 목록마다 행을 다시 넣어 보지 않게.
    known_keys: Option<HashSet<[u8; 32]>>,
    /// 키 그룹 멤버 상태를 다시 정리할 일이 생김(갱신 조각·묶음·R·상태 키가 옴).
    keys_dirty: bool,
    /// 해소기가 바뀌어 그룹 사슬을 다시 판정해야 함.
    rejudge: bool,
    /// 모르는 채널 뿌리(정리 끝에 멤버에게 물음).
    ask_roots: Vec<(ChannelId, ChannelRootId)>,
    /// 모르는 크루 상태 키(정리 끝에 멤버에게 물음: 가입 묶음보다 오래된 상태 이벤트).
    ask_state: Vec<StateKeyId>,
    /// 키 묶음으로 새로 얻은 상태 키(그 키를 기다리던 키 그룹 갱신을 다시 엶).
    new_state_keys: Vec<StateKeyId>,
    /// 끝나면 `space.updated`를 냄(키 요약을 쓴 뒤에).
    space_updated: bool,
}

impl Cx<'_> {
    fn reject(&mut self, id: EventId, why: impl Into<String>) {
        let why = why.into();
        debug!(event = %id.short(), reason = %why, "이벤트 거부");
        self.report.rejected.push((id, why));
    }

    /// 권한·상태 규칙 검사([`AuthError`])로 거부. 지금 상태에 따라 갈리는 오류이면 따로 표시합니다.
    fn reject_auth(&mut self, id: EventId, what: &str, e: &AuthError) {
        if depends_on_state(e) {
            self.report.rejected_by_state.push(id);
        }
        self.reject(id, format!("{what}: {e}"));
    }

    fn accept(&mut self, id: EventId) {
        if !self.report.accepted.contains(&id) {
            self.report.accepted.push(id);
        }
    }

    fn hold(&mut self, ev: SignedEvent) {
        let size = held_size(&ev);
        while !self.core.held.is_empty()
            && (self.core.held.len() >= MAX_HELD
                || self.core.held_bytes.saturating_add(size) > MAX_HELD_BYTES)
        {
            let old = self.core.held.remove(0);
            self.core.held_bytes = self.core.held_bytes.saturating_sub(held_size(&old));
        }
        self.core.held_bytes = self.core.held_bytes.saturating_add(size);
        self.core.held.push(ev);
        self.report.held += 1;
    }

    /// 지금 상태에서 작성자가 멤버의 활성 기기인지. 아니면 새로 받은 것은 RAM에 보류하고(가입·기기 목록 갱신이
    /// 늦게 왔을 수 있음, 상태가 바뀌면 다시 봄), 저장소에서 다시 꺼낸 것은 거부합니다. 처리를 계속하면 true.
    fn author_is_member(&mut self, ev: &SignedEvent, state: &SpaceState, force: bool) -> bool {
        let h = &ev.header;
        if state.is_member(&h.user) && state.device_belongs(&h.user, &h.author) {
            return true;
        }
        if force {
            self.report.rejected_by_state.push(ev.id());
            self.reject(ev.id(), "멤버의 활성 기기가 아닌 작성자");
        } else if !is_revoked(ev, state) {
            // 폐기는 되돌릴 수 없어 보류해 봐야 소용없습니다(글쓴이의 기기가 내주면 그때 다시 받음).
            self.hold(ev.clone());
        }
        false
    }

    /// 폐기된 기기가 쓴 메시지를 받을지. 메시지는 지난 기록이라 나중에 연결한 기기·멤버도 받아야 하는데, 지금 상태로만 보면
    /// 아무도 못 받고 빈 구간으로 남습니다. 폐기 뒤에 꾸민 글이 끼지 않게 그 사람의 살아 있는 기기가 내줄 때만 받습니다
    /// (시각·논리 시계는 쓴 기기가 적으므로 믿지 않음). 저장소에서 다시 꺼낸 것은 저장할 때 본 것입니다. 보류를 다시 볼 때는
    /// 내준 피어를 모르므로 받지 않고, 따라잡기가 다시 가져옵니다.
    fn vouched_revoked(&self, ev: &SignedEvent, state: &SpaceState, force: bool) -> bool {
        let h = &ev.header;
        h.kind.lane() == Lane::Message
            && state.is_member(&h.user)
            && is_revoked(ev, state)
            && (force
                || !self.retrying_held
                    && self
                        .source
                        .peer()
                        .and_then(|p| self.node.peers.identity_of(&p))
                        .is_some_and(|(u, d)| u == h.user && state.device_belongs(&u, &d)))
    }

    /// 짧은 서명(채널 스트림, [`yangpa_core::sign::Sig::is_slim`])을 상태 기록이 가리키는 작성 기기 서명 키로
    /// 확인합니다. 작성자가 멤버의 활성 기기인지(`revoked`면 받기로 한 폐기된 기기인지)는 호출자가 먼저 봅니다. 키를 아직
    /// 모르면 새로 받은 것은 RAM에 보류하고 피어에게 키를 묻습니다. 저장소에서 다시 꺼낸 것은 저장할 때 확인했으므로
    /// 통과시킵니다. 처리를 계속하면 true.
    fn slim_ok(
        &mut self,
        ev: &SignedEvent,
        state: &SpaceState,
        force: bool,
        revoked: bool,
    ) -> bool {
        if !ev.sig.is_slim() {
            return true;
        }
        let h = &ev.header;
        match crate::devkeys::device_sign_keys(self.node, state, &h.user, &h.author, revoked) {
            Some(keys) => {
                if ev.verify_with(&keys).is_ok() {
                    return true;
                }
                self.reject(ev.id(), "서명이 맞지 않음");
                false
            }
            None if force => true,
            None => {
                if !self.report.missing_keys.contains(&h.author) {
                    self.report.missing_keys.push(h.author);
                }
                self.hold(ev.clone());
                false
            }
        }
    }

    /// 로컬 사용자가 이 채널을 볼 수 있는지(알림을 낼지).
    fn local_can_view(&self, channel: &ChannelId) -> bool {
        let Ok(me) = self.node.me() else { return false };
        self.state.as_ref().is_some_and(|s| {
            s.channel_permissions(&me.user(), channel)
                .contains(Permissions::VIEW_CHANNEL)
        })
    }

    fn wait(&mut self, need: Need, id: EventId) {
        self.core.waiting.entry(need).or_default().insert(id);
        self.waiting_dirty = true;
        self.report.waiting += 1;
        self.accept(id);
    }

    fn wake(&mut self, need: Need) {
        if let Some(ids) = self.core.waiting.remove(&need) {
            self.waiting_dirty = true;
            self.woken.extend(ids);
        }
    }

    /// 저장(같은 ID면 아무 일 없음). 부모가 저장소에 없으면 따라잡기 표시.
    fn store(&mut self, ev: &SignedEvent, level: SecurityLevel) -> Result<()> {
        let pe = PersistentEvent::new(ev.clone(), level)?;
        self.node.store.insert_events(&[pe])?;
        self.note_parents(ev)?;
        Ok(())
    }

    /// 상태 이벤트와 본문에 실린 기기 키를 한 트랜잭션으로 저장합니다(키 행은 이 공간 몫, [`crate::devkeys`]).
    fn store_state(&mut self, ev: &SignedEvent, body: &EventBody) -> Result<()> {
        let mut keys = crate::devkeys::body_keys(body);
        if matches!(self.source, Source::Replay) && !keys.is_empty() {
            // 처음 받을 때 이벤트와 함께 넣었으므로 보통 다 있습니다. 키를 기다리던 이벤트를 처음 푸는 경우만 남습니다.
            if self.known_keys.is_none() {
                self.known_keys = Some(self.node.store.device_key_commits(&self.space)?);
            }
            let known = self.known_keys.as_mut().expect("방금 채움");
            keys.retain(|k| known.insert(k.key_commit()));
        }
        let pe = PersistentEvent::new(ev.clone(), SecurityLevel::Normal)?;
        self.node
            .store
            .insert_events_with_device_keys(&[pe], &self.space, &keys)?;
        self.note_parents(ev)
    }

    fn note_parents(&mut self, ev: &SignedEvent) -> Result<()> {
        if !ev.header.parents.is_empty()
            && self
                .node
                .store
                .has_events(&ev.header.parents)?
                .iter()
                .any(|h| !h)
        {
            self.report.missing_parents = true;
        }
        Ok(())
    }

    fn process(&mut self, ev: SignedEvent, force: bool) -> Result<()> {
        let id = ev.id();
        // 짧은 서명은 키를 상태에서 찾아야 해서 여기서는 모양·본문 해시만 보고, 상태가 있을 때 확인합니다([`Self::slim_ok`]).
        let checked = if ev.sig.is_slim() {
            ev.check_integrity()
        } else {
            ev.verify()
        };
        if checked.is_err() {
            self.reject(id, "서명·본문 해시가 맞지 않음");
            return Ok(());
        }
        if ev.header.space != self.space {
            self.reject(id, "다른 공간의 이벤트");
            return Ok(());
        }
        if !force {
            if let Some(meta) = self.node.store.event_meta(&id)?
                && (meta.has_body || meta.erased || ev.body.is_none())
            {
                self.report.duplicate += 1;
                return Ok(());
            }
            // 논리 시계 보류보다 먼저 셉니다: 보류했다 다시 꺼내 볼 때는 묶지 않으므로 그 길로 비켜 가지 못하게.
            if self.live && !self.admit_live(&ev) {
                self.report.throttled.push(id);
                return Ok(());
            }
            let known = self.node.store.max_lamport(&self.space)?;
            if known > 0 && ev.header.lamport > known.saturating_add(LAMPORT_SLACK) {
                self.hold(ev);
                return Ok(());
            }
        }
        let lane = ev.header.kind.lane();
        match ev.header.stream() {
            StreamId::Channel(c) => {
                if matches!(ev.header.key, KeyRef::Volatile { .. }) {
                    return self.volatile(ev);
                }
                let Some(state) = self.state.clone() else {
                    self.hold(ev);
                    return Ok(());
                };
                match state.channels.get(&c) {
                    None => {
                        self.hold(ev);
                        return Ok(());
                    }
                    Some(ch) if ch.is_volatile() => return self.volatile(ev),
                    Some(_) => {}
                }
                let revoked = self.vouched_revoked(&ev, &state, force);
                if !(revoked || self.author_is_member(&ev, &state, force))
                    || !self.slim_ok(&ev, &state, force, revoked)
                {
                    return Ok(());
                }
                match lane {
                    Lane::Message => self.message(ev, revoked),
                    Lane::Key => self.key_event(ev),
                    _ => {
                        self.reject(id, "채널 스트림에 올 수 없는 레인");
                        Ok(())
                    }
                }
            }
            StreamId::KeyGroup(_) | StreamId::Inbox(_) => self.scoped_event(ev),
            StreamId::Space => match lane {
                Lane::State => self.state_event(ev, force),
                Lane::KeyAgreement => self.key_update_event(ev, force),
                Lane::Key => {
                    // 상태가 있으면 키 묶음도 멤버 기기의 것만 저장합니다(가짜 묶음이 머리·시더 응답에 남지 않게).
                    if let Some(state) = self.state.clone()
                        && !self.author_is_member(&ev, &state, force)
                    {
                        return Ok(());
                    }
                    self.key_event(ev)
                }
                Lane::Message => {
                    self.reject(id, "채널 없는 메시지");
                    Ok(())
                }
            },
        }
    }

    fn volatile(&mut self, ev: SignedEvent) -> Result<()> {
        let id = ev.id();
        let Some(state) = self.state.clone() else {
            // RAM 보류는 디스크로 가지 않으므로 휘발성 이벤트도 괜찮습니다.
            self.hold(ev);
            return Ok(());
        };
        // 휘발성 이벤트는 저장하지 않으므로 다시 꺼내 보는 일(force)이 없습니다.
        if ev.sig.is_slim()
            && !(self.author_is_member(&ev, &state, false)
                && self.slim_ok(&ev, &state, false, false))
        {
            return Ok(());
        }
        let from = self.source.peer();
        match volatile::on_inbound_event(self.node, &self.space, &state, ev, from) {
            VolatileVerdict::Accepted => {
                self.report.volatile += 1;
                self.accept(id);
            }
            VolatileVerdict::Rejected => self.reject(id, "휘발성 이벤트 거부"),
            VolatileVerdict::Ignored => self.report.volatile += 1,
        }
        Ok(())
    }

    // ───────────── 상태·키 합의 레인 ─────────────

    /// 곧바로 받은 공간 스트림의 상태·키 합의 이벤트를 작성 기기별 버킷에 셉니다(다른 이벤트는 그대로 통과). 이미 가진
    /// 이벤트는 앞의 중복 검사에서 빠져 세지 않고, 서명을 확인한 뒤라 남의 기기 ID를 사칭해 그 기기 몫을 써 버릴 수
    /// 없습니다.
    fn admit_live(&self, ev: &SignedEvent) -> bool {
        let h = &ev.header;
        if h.stream() != StreamId::Space
            || !matches!(h.kind.lane(), Lane::State | Lane::KeyAgreement)
        {
            return true;
        }
        // 상태를 아직 못 풀었으면(가입 중) 묶지 않습니다. 처음 받는 상태 이벤트들입니다.
        let Some(state) = self.state.clone() else {
            return true;
        };
        // 작은 크루는 예전 그대로 묶지 않습니다(활성 기기 수는 상태 사본마다 한 번 셈).
        if self.node.sync.index.get(&self.space, &state).active_count()
            <= self.node.cfg.event_limit_devices
        {
            return true;
        }
        if ratelimit::event_exempt(&state, &h.user, &h.author) {
            return true;
        }
        let author = ratelimit::event_author(&state, &h.user, &h.author, self.source.peer());
        if self.take_event(author, EventClass::Base) {
            return true;
        }
        // 보통 몫을 다 쓴 뒤에만 초대 증명을 봅니다(복호화와 서명 확인이 들어 평소에는 하지 않음).
        h.kind == EventKind::MemberJoin
            && self.invite_join_ok(ev, &state)
            && self.take_event(author, EventClass::Join)
    }

    fn take_event(&self, author: ratelimit::EventAuthor, class: EventClass) -> bool {
        let mut g = self.node.ratelimit.events.lock();
        let now = g.now_ms();
        g.try_take(self.space, author, class, now)
    }

    /// 지금 상태의 쓸 수 있는 초대로 증명이 확인되는 `MemberJoin`인지(받아 주는 멤버가 낸 가입).
    fn invite_join_ok(&self, ev: &SignedEvent, state: &SpaceState) -> bool {
        let (Some(bytes), KeyRef::State(kid)) = (ev.body.as_deref(), ev.header.key) else {
            return false;
        };
        let key = match keys::crew_state_key(self.node, &self.space, &kid) {
            Ok(Some(k)) => k,
            _ => match self.core.provisional_state_key(&kid) {
                Some(k) => k,
                None => return false,
            },
        };
        let Ok(EventBody::MemberJoin {
            devices,
            invite: Some(proof),
            ..
        }) = decrypt_body(&key, &ev.header, bytes)
        else {
            return false;
        };
        state
            .invites
            .iter()
            .find(|i| i.info.token_hash == proof.token_hash)
            .is_some_and(|i| {
                i.usable_at(ev.header.created_at)
                    && proof
                        .verify(&i.info.verify_key, &self.space, &devices)
                        .is_ok()
            })
    }

    fn state_event(&mut self, ev: SignedEvent, force: bool) -> Result<()> {
        let id = ev.id();
        let Some(bytes) = ev.body.as_deref() else {
            // 헤더만 받은 것(본문을 지운 이벤트 등): 순서에만 씁니다. 풀릴 일이 없으므로 기준점 상태도 기다리지 않습니다.
            self.store(&ev, SecurityLevel::Normal)?;
            self.links.push(LinkEvent::without_body(id, &ev.header));
            self.accept(id);
            return Ok(());
        };
        let KeyRef::State(kid) = ev.header.key else {
            self.reject(id, "상태 이벤트는 상태 키로 암호화해야 함");
            return Ok(());
        };
        let key = match keys::crew_state_key(self.node, &self.space, &kid)? {
            Some(k) => Some(k),
            None => self.core.provisional_state_key(&kid),
        };
        let Some(key) = key else {
            // 경계 아래 이벤트는 본문이 효과가 없으므로(늦은 이벤트) 키를 기다리지 않고 순서에만 씁니다. 은퇴한 키는 새
            // 멤버에게 영영 오지 않습니다.
            let floor = self.core.resolver.floor();
            if floor > 0 && ev.header.lamport < floor {
                self.store(&ev, SecurityLevel::Normal)?;
                self.links.push(LinkEvent::without_body(id, &ev.header));
                self.accept(id);
                return Ok(());
            }
            // 풀지 못한 상태 이벤트는 체크포인트를 막습니다. 멤버가 아닌 사람(쫓겨난 사람, 새로 만든 신원)이 아무 키
            // ID로 봉인한 이벤트 하나로 크루 전체의 체크포인트를 멈추지 못하게, 키를 기다리는 것은 지금 멤버가 쓴 것과
            // 가입(작성자가 아직 멤버가 아님)뿐입니다.
            let stranger = ev.header.kind != EventKind::MemberJoin
                && self
                    .state
                    .as_ref()
                    .is_some_and(|s| !s.is_member(&ev.header.user));
            if stranger {
                if force {
                    self.report.rejected_by_state.push(id);
                    self.reject(id, "멤버가 아닌 작성자의 풀 수 없는 상태 이벤트");
                } else {
                    self.hold(ev);
                }
                return Ok(());
            }
            self.store(&ev, SecurityLevel::Normal)?;
            self.links.push(LinkEvent::from_header(id, &ev.header));
            self.wait(Need::StateKey(kid), id);
            self.ask_state.push(kid);
            return Ok(());
        };
        match decrypt_body(&key, &ev.header, bytes) {
            Ok(body) => {
                // 기기 키 표는 해소기보다 먼저 채웁니다(적용된 기록이 가리키는 약속의 키가 늘 표에 있게).
                self.store_state(&ev, &body)?;
                if let EventBody::KeyUpdate(p) = &body {
                    // 해소기가 받아들이면 멤버 쪽이 조각을 합쳐 적용합니다(`keygroup::member`).
                    self.core.keys.note_crew_part(id, &ev.header, p.clone());
                    self.keys_dirty = true;
                }
                crate::identity::observe_state_body(self.node, &body);
                self.decoded.push(DecodedEvent {
                    id,
                    header: ev.header.clone(),
                    body,
                });
                self.accept(id);
            }
            Err(_) => self.reject(id, "상태 이벤트 복호화 실패"),
        }
        Ok(())
    }

    /// 키 합의 레인. 크루 `KeyUpdate`는 (갱신 전) 크루 상태 키로 암호화하므로 상태 이벤트와 같은 길로 열어 해소기에
    /// 넣습니다(해소기가 사슬 규칙을 판정). 이 레인의 다른 코드(2판까지 쓰던 80·81 등)는 이 판이 모르는 종류라
    /// 받지 않습니다.
    fn key_update_event(&mut self, ev: SignedEvent, force: bool) -> Result<()> {
        if ev.header.kind == EventKind::KeyUpdate {
            return self.state_event(ev, force);
        }
        self.reject(ev.id(), "모르는 키 합의 이벤트");
        Ok(())
    }

    // ───────────── 키 레인 ─────────────

    /// 키 묶음 이벤트를 저장합니다(공간 스트림이면 순서 연결에도 넣음).
    fn store_key_event(&mut self, ev: &SignedEvent) -> Result<()> {
        self.store(ev, SecurityLevel::Normal)?;
        if ev.header.stream() == StreamId::Space {
            // 키 레인은 상태에 영향이 없지만 공간 스트림의 순서 연결에 씁니다.
            self.links.push(LinkEvent::from_header(ev.id(), &ev.header));
        }
        Ok(())
    }

    /// 본문을 열 수 없는(내 상자가 망가졌거나 복호화되지 않거나 묶음이 아닌) 공간 스트림 키 레인 이벤트: 서명·헤더는
    /// 맞으므로 저장하고 순서 연결에 넣습니다. 다른 노드는 '내 앞이 아님'으로 저장·연결하므로, 이 노드만 빼면 공간
    /// 스트림의 조상 관계가 노드마다 달라져 크루 갱신 판정이 갈립니다. 다른 스트림은 거부합니다.
    fn keep_undecodable(&mut self, ev: &SignedEvent, why: &'static str) -> Result<()> {
        if ev.header.stream() != StreamId::Space {
            self.reject(ev.id(), why);
            return Ok(());
        }
        warn!(space = %self.space.short(), event = %ev.id().short(), reason = why, "공간 스트림 키 묶음 본문을 열 수 없어 연결만 함");
        self.store_key_event(ev)?;
        self.accept(ev.id());
        Ok(())
    }

    /// 키 레인. 열고 권한을 확인한 뒤에만 저장합니다. 키를 기다리거나(저장하고 대기), 다른 기기 앞으로 봉인돼
    /// 열 수 없는 것은 그대로 저장합니다. 공간 스트림에서 본문을 열 수 없는 것은 연결만 하고
    /// ([`Self::keep_undecodable`]), 권한이 없는 묶음은 저장하지 않습니다.
    fn key_event(&mut self, ev: SignedEvent) -> Result<()> {
        let id = ev.id();
        let Some(bytes) = ev.body.as_deref() else {
            self.store_key_event(&ev)?;
            self.accept(id);
            return Ok(());
        };
        let me = self.node.me()?;
        let h = &ev.header;
        let body = match h.key {
            KeyRef::Sealed => {
                match open_sealed_body(&me.device_id(), &me.device.boxkey, h, bytes) {
                    Ok(b) => b,
                    // 다른 기기 앞으로 봉인한 키 묶음.
                    Err(CryptoError::NotARecipient) => {
                        self.store_key_event(&ev)?;
                        self.accept(id);
                        return Ok(());
                    }
                    Err(_) => return self.keep_undecodable(&ev, "봉인 키 묶음을 열 수 없음"),
                }
            }
            KeyRef::State(kid) => {
                // 공간 스트림 묶음은 크루 상태 키로만, 채널 스트림 묶음(다시 봉인한 뿌리)은 그 채널 그룹의 상태 키로도.
                let stored = if h.stream() == StreamId::Space {
                    keys::crew_state_key(self.node, &self.space, &kid)?
                } else {
                    keys::state_key(self.node, &self.space, &kid)?
                };
                let key = match stored {
                    Some(k) => Some(k),
                    None => self.core.provisional_state_key(&kid),
                };
                let Some(key) = key else {
                    self.store_key_event(&ev)?;
                    self.wait(Need::StateKey(kid), id);
                    if h.stream() == StreamId::Space {
                        self.ask_state.push(kid);
                    }
                    return Ok(());
                };
                match decrypt_body(&key, h, bytes) {
                    Ok(b) => b,
                    Err(_) => return self.keep_undecodable(&ev, "키 묶음 복호화 실패"),
                }
            }
            _ => {
                self.reject(id, "키 묶음에 쓸 수 없는 키");
                return Ok(());
            }
        };
        let EventBody::KeyShare(ks) = body else {
            return self.keep_undecodable(&ev, "키 레인에 키 묶음이 아닌 본문");
        };
        match self.state.clone() {
            None => {
                self.store_key_event(&ev)?;
                // 상태 키 묶음이 그 키로 연 상태 이벤트(시작 이벤트, 초대 가입 묶음 등)보다 늦게 오면, 키를 기다리던
                // 상태 이벤트를 이 임시 키로 다시 엽니다. 깨우지 않으면 새 기기·가입자는 상태를 영영 세우지 못합니다
                // (권한 확인은 상태가 생긴 뒤 `apply_state`에서).
                let state_keys: Vec<StateKeyId> = ks
                    .keys
                    .iter()
                    .filter_map(|k| match k {
                        SharedKey::State { id, .. } => Some(*id),
                        _ => None,
                    })
                    .collect();
                self.keep_provisional(id, h.clone(), ks);
                for kid in state_keys {
                    self.wake(Need::StateKey(kid));
                }
                self.wait(Need::SpaceState, id);
            }
            Some(state) => {
                let body = EventBody::KeyShare(ks);
                match authorize_message(&state, h, &body, None) {
                    Ok(()) => {
                        let EventBody::KeyShare(ks) = body else {
                            unreachable!()
                        };
                        self.store_key_event(&ev)?;
                        self.persist_keys(h, &ks)?;
                        self.core.provisional.retain(|p| p.id != id);
                        self.accept(id);
                    }
                    Err(e)
                        if h.key == KeyRef::Sealed
                            && matches!(&body, EventBody::KeyShare(ks)
                                if may_settle_later(self.node, &e, ks, &state)) =>
                    {
                        // 내 앞으로 봉인한 묶음인데 지금 상태로는 작성자를 모름: 상태를 아직 다 받지 못했을 수
                        // 있으므로(멤버가 확정 기기에게 넘긴 키로 상태를 세우는 중 등) 임시 키로 두고 상태가 바뀌면
                        // 다시 봅니다.
                        let EventBody::KeyShare(ks) = body else {
                            unreachable!()
                        };
                        debug!(event = %id.short(), reason = %e, "키 묶음 권한을 상태가 바뀐 뒤 다시 봄");
                        self.store_key_event(&ev)?;
                        self.keep_provisional(id, h.clone(), ks);
                        self.wait(Need::SpaceState, id);
                    }
                    Err(e) => self.reject_auth(id, "키 묶음 권한", &e),
                }
            }
        }
        Ok(())
    }

    /// 임시 키 묶음으로 둡니다(같은 이벤트는 한 번, 상한을 넘으면 오래된 것부터 버림).
    fn keep_provisional(&mut self, id: EventId, header: EventHeader, body: KeyShareBody) {
        if self.core.provisional.iter().any(|p| p.id == id) {
            return;
        }
        if self.core.provisional.len() >= spaces::MAX_PROVISIONAL {
            self.core.provisional.remove(0);
        }
        self.core.provisional.push(Provisional { id, header, body });
    }

    fn persist_keys(&mut self, header: &EventHeader, ks: &KeyShareBody) -> Result<()> {
        let state = self.state.clone();
        self.persist_keys_at(header, ks, state.as_deref(), Promote::ByShare)
    }

    /// 키 묶음의 키를 저장합니다. `basis`는 권한을 확인한 상태(지금 상태, 부분 전달 이벤트면 기준점 상태)입니다.
    fn persist_keys_at(
        &mut self,
        header: &EventHeader,
        ks: &KeyShareBody,
        basis: Option<&SpaceState>,
        promote: Promote,
    ) -> Result<()> {
        // 확정 기기 인계로만 통과한 묶음(나눌 권한 없는 멤버가 봉인)의 상태 키는 저장만 하고 지금 키로 쓰지 않습니다.
        let handover_only = basis.is_some_and(|st| state_key_handover_only(st, header));
        let last_state = ks
            .keys
            .iter()
            .rposition(|k| matches!(k, SharedKey::State { .. }));
        for (i, k) in ks.keys.iter().enumerate() {
            match k {
                SharedKey::State { id, key } => {
                    // 권한 확인이 이미 막지만, ID가 키에서 나오지 않은 상태 키는 저장하지 않습니다(먼저 쓴 쪽이 이기므로
                    // 진짜 ID 아래 엉뚱한 키가 남으면 그 키로 쓴 상태 이벤트를 영영 못 읽음).
                    if !k.bound_to(&self.space) {
                        continue;
                    }
                    let (promote, rank) = match promote {
                        Promote::ByShare => (!handover_only, None),
                        Promote::Ranked(r) => (Some(i) == last_state, Some(r)),
                        Promote::Never => (false, None),
                    };
                    keys::put_state_key(self.node, &self.space, id, key, promote, rank)?;
                    self.wake(Need::StateKey(*id));
                    self.new_state_keys.push(*id);
                    self.keys_dirty = true;
                }
                SharedKey::ChannelRoot { channel, id, key } => {
                    let volatile = basis
                        .and_then(|s| s.channels.get(channel))
                        .is_none_or(|c| c.is_volatile());
                    // ID가 뿌리에서 나오지 않은 것은 저장하지 않습니다(먼저 쓴 쪽이 이김, 권한 확인도 막음). 받은 뿌리는
                    // 읽기에만 씁니다(보낼 때는 지금 그룹 키에서 파생).
                    if volatile || !k.bound_to(&self.space) {
                        continue;
                    }
                    keys::put_root(self.node, &self.space, channel, key)?;
                    self.wake(Need::Record(*channel, *id));
                }
                // 그룹 R(가입자 몫): 그 R의 공개 채널 뿌리를 파생할 수 있게 둡니다.
                SharedKey::Root { group, key, .. } => {
                    if !k.bound_to(&self.space) {
                        continue;
                    }
                    self.core.keys.ensure_loaded(self.node, &self.space);
                    self.core.keys.offer_r(*group, key.clone());
                    self.keys_dirty = true;
                    self.retry_roots();
                }
                // 기기 몫 봉인 묶음: 내 것만 두고, 증명은 갱신의 `bundle_root`로 적용할 때 봅니다.
                SharedKey::Bundle { group, seq, bundle } => {
                    let Ok(me) = self.node.me() else { continue };
                    if let Ok(b) = DeviceBundle::decode(bundle)
                        && b.device == me.device_id()
                    {
                        self.core.keys.offer_bundle(*group, *seq, b);
                        self.keys_dirty = true;
                    }
                }
                // 휘발성 세션 키는 디스크에 두지 않습니다(휘발성 영역이 RAM에서 처리).
                SharedKey::Volatile { .. } => {}
            }
        }
        Ok(())
    }

    // ───────────── 부분 전달 스트림(받은편지함·키 그룹) ─────────────

    /// 이 묶음을 가입·기기 연결 중에 믿는 피어(받아 준 기기·공간을 만든 사용자의 기기·알려 준 멤버·내 기기)에게서 받았는지.
    /// 그 피어는 받은편지함 이벤트를 자기 지금 상태로도 확인한 뒤에만 저장·전달하므로, 상태가 없거나 아직 덜 받은 이
    /// 기기는 기준점 상태만으로 확인하고 키를 미리 엽니다. 그 밖에서는 지금 상태로도 확인합니다([`Self::finish_scoped`]).
    fn trusted_bootstrap(&self) -> bool {
        // 상태가 없으면 믿는 피어에게서만 받습니다(`sync_service::handle_push`·따라잡기 상대, gossip은 받지 않음).
        if self.state.is_none() {
            return true;
        }
        let peer = match self.source {
            Source::Bootstrap(_) => return true,
            Source::Peer(p) => p,
            Source::Local | Source::Replay => return false,
        };
        let Some((user, _)) = self.node.peers.identity_of(&peer) else {
            return false;
        };
        (crews::is_pending_join(self.node, &self.space)
            && crews::join_peer_ok(self.node, &self.space, &peer, &user))
            || crate::identity::adopting_from(self.node, &self.space, &user)
    }

    /// 지금 상태가 이 기준점을 모두 담고 빈 구간(빠진 부모·키를 기다리는 상태 이벤트)도 없는지. 그러면 지금 상태로 갈린
    /// 거부를 기억해도 됩니다(아직 받지 못한 확정 기기 변경 때문에 맞는 묶음을 영영 거부하지 않게).
    fn current_settled(&self, anchor: &[EventId]) -> bool {
        anchor.iter().all(|a| self.core.resolver.contains(a))
            && self.core.resolver.missing().is_empty()
            && !self
                .core
                .waiting
                .keys()
                .any(|n| matches!(n, Need::StateKey(_)))
    }

    /// 부분 전달 이벤트를 거부합니다. `remember`면(어느 노드에서나, 언제 다시 봐도 같은 답) 따라잡기가 다시 내려받지
    /// 않게 기억합니다.
    fn refuse_scoped(&mut self, id: EventId, remember: bool, what: &str, e: &AuthError) {
        self.drop_provisional(id);
        if remember {
            self.core.scoped.remember_rejected(id);
        }
        self.reject_auth(id, what, e);
    }

    /// 부분 전달 이벤트. 이 기기가 받을 것(내 받은편지함, 지금 볼 수 있는 키 그룹의 지금 확정 기기 것), 내가 쓴 것, 내가
    /// 전달자인 사용자의 받은편지함(`crews::InboxRelay`)만 다룹니다. 권한은 기준점 상태로 확인한 뒤에만 저장합니다
    /// ([`Self::settle_scoped`]). 가입·기기 연결 중 믿는 피어에게서 받은 내 받은편지함의 키 묶음은 그 전에 열어 임시 키로
    /// 둡니다: 가입자는 이 키가 있어야 기준점까지의 상태 이벤트를 열 수 있습니다.
    fn scoped_event(&mut self, ev: SignedEvent) -> Result<()> {
        let id = ev.id();
        if self.core.scoped.is_known(&id) {
            self.report.duplicate += 1;
            return Ok(());
        }
        let Some(scope) = ev.header.scope else {
            self.reject(id, "범위 없는 부분 전달 이벤트");
            return Ok(());
        };
        if ev.body.is_none() {
            self.core.scoped.remember_rejected(id);
            self.reject(id, "본문 없는 부분 전달 이벤트");
            return Ok(());
        }
        let me = self.node.me()?;
        let mine = ev.header.author == me.device_id();
        let wanted = mine
            || match scope {
                Scope::Inbox(u) => {
                    u == me.user()
                        || crews::is_inbox_relay(self.node, &self.space, &u, &me.device_id())
                }
                // 지금 볼 수 있는 그룹의 것만, 실시간 전파로는 지금 확정 기기가 쓴 것만(아무 멤버나 키 그룹 이벤트로
                // 대기열·기준점 계산을 쓰지 못하게). 따라잡기로 받은 옛 확정 기기의 갱신은 받습니다: 새로 자격을 얻은
                // 기기는 그룹 사슬을 처음부터 이어야 하고, 쓸 수 있는지는 사슬이 지금 상태로 정합니다. 키 그룹은 상태가
                // 생긴 뒤에만 따라잡습니다.
                Scope::KeyGroup(g) => self.state.as_ref().is_some_and(|s| {
                    s.can_view_key_group(&self.space, &g, &me.user())
                        && (s.committer == ev.header.author
                            || (!self.live && ev.header.kind == EventKind::KeyUpdate))
                }),
            };
        if !wanted {
            // 잘못 온 부분 전달 이벤트(남의 받은편지함·볼 수 없는 키 그룹): 저장하지도 넘기지도 않습니다.
            sync_service::note_foreign_scoped(self.node, &ev);
            if matches!(scope, Scope::Inbox(_)) {
                self.core.scoped.remember_rejected(id);
            }
            self.report.rejected_by_state.push(id);
            self.reject(id, "받을 자격이 없는 부분 전달 이벤트");
            return Ok(());
        }
        let trusted = self.trusted_bootstrap();
        // 믿는 피어에게서 받은 것이 아니면 기준점 상태를 세우기 전에 지금 상태로 먼저 거릅니다(싸게): 지금 자격이 없는
        // 작성자(강등된 관리자·바뀐 확정 기기·폐기된 기기)는 옛 기준점을 적어도 대기열과 기준점 계산을 쓰지 못합니다.
        // 그룹 사슬 갱신은 빼고(위), 사슬이 지금 상태로 쓸 수 있는지를 정합니다.
        if !trusted
            && !mine
            && !chain_update(&ev.header)
            && let Some(cur) = self.state.clone()
            && let Err(e) = authorize_scoped(&cur, &self.space, &ev.header)
        {
            let remember = self.current_settled(&ev.header.state_anchor);
            self.refuse_scoped(id, remember, "부분 전달 이벤트 권한(지금 상태)", &e);
            return Ok(());
        }
        if trusted && scope == Scope::Inbox(me.user()) {
            self.open_inbox_early(&ev);
        }
        let p = PendingScoped {
            ev,
            id,
            since: Instant::now(),
            trusted,
            blockers: Vec::new(),
            gaps_tried: false,
        };
        // 내가 쓴 것은 지금 바로 확인합니다(내 해소기가 아는 이벤트로 기준점을 골랐음).
        if self.source == Source::Local {
            match self.core.resolver.state_at(&p.ev.header.state_anchor, true) {
                AnchorState::Ready { state, gaps } => {
                    self.decide_scoped(&p, state.map(|s| Arc::new(*s)), !gaps)?
                }
                AnchorState::Incomplete { .. } => {
                    self.drop_provisional(id);
                    self.reject(id, "기준점 상태를 세울 수 없음");
                }
            }
            return Ok(());
        }
        for old in self.core.scoped.push(p) {
            self.drop_provisional(old);
        }
        // 새로 온 것은 해소기가 그대로여도 한 번은 봅니다.
        self.core.scoped.checked_at = u64::MAX;
        self.arm_scoped_timer();
        Ok(())
    }

    /// 내 받은편지함의 키 묶음을 권한 확인 전에 열어 임시 키로 둡니다(그 상태 키를 기다리던 상태 이벤트를 깨움). 믿는
    /// 피어에게서 받은 것만 그렇게 합니다([`Self::trusted_bootstrap`]): 아무 멤버나 지어낸 상태 키로 이 기기만 읽는 상태
    /// 이벤트를 열게 하면 이 기기의 상태가 다른 멤버와 갈립니다. 임시 키는 상태 이벤트를 여는 데만 쓰고(상태 키 ID가
    /// 키에서 나와 엉뚱한 키를 끼울 수 없음), 저장은 확인한 뒤에만 합니다.
    fn open_inbox_early(&mut self, ev: &SignedEvent) {
        let Ok(me) = self.node.me() else { return };
        let Some(bytes) = ev.body.as_deref() else {
            return;
        };
        let Ok(EventBody::KeyShare(ks)) =
            open_sealed_body(&me.device_id(), &me.device.boxkey, &ev.header, bytes)
        else {
            return;
        };
        let state_keys: Vec<StateKeyId> = ks
            .keys
            .iter()
            .filter_map(|k| match k {
                SharedKey::State { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        self.keep_provisional(ev.id(), ev.header.clone(), ks);
        for kid in state_keys {
            self.wake(Need::StateKey(kid));
        }
    }

    fn drop_provisional(&mut self, id: EventId) {
        self.core.provisional.retain(|p| p.id != id);
    }

    /// 기다리는 것이 있으면 유예마다 다시 보는 작업을 하나 띄웁니다(따라잡을 피어가 없어도 빈 구간 허용·버리기가 돌게).
    fn arm_scoped_timer(&mut self) {
        if self.core.scoped.timer {
            return;
        }
        self.core.scoped.timer = true;
        let space = self.space;
        self.node.spawn_detached(move |node| async move {
            loop {
                tokio::time::sleep(scoped::GAP_GRACE / 4).await;
                if !recheck_scoped(&node, space).await {
                    break;
                }
            }
        });
    }

    /// 기다리던 부분 전달 이벤트를 다시 봅니다. 기준점 조상을 따라가는 비용이 공간 스트림 크기에 비례하므로, 이벤트마다
    /// 지난번에 막던 것(모르는 이벤트·키를 기다리는 상태 이벤트)이 그대로이고 빈 구간 허용도 그대로이면 다시 세지
    /// 않습니다. 하나라도 정해지면 true.
    fn settle_scoped(&mut self) -> Result<bool> {
        if self.core.scoped.pending.is_empty() {
            return Ok(false);
        }
        let stats = self.core.resolver.stats();
        let version = stats.incremental + stats.full + stats.replay;
        // 빈 구간 없이 센 기준점 상태는 조상 집합이 해시로 고정돼 있어, 이미 있던 항목이 바뀌는 재해소(본문을 받은 순서
        // 연결 등) 전까지 그대로입니다.
        let generation = stats.full + stats.replay;
        let now = Instant::now();
        let caught_up = self.core.scoped.caught_up();
        let due = self.core.scoped.pending.iter().any(|p| {
            (!p.gaps_tried && ScopedQueue::gaps_allowed(caught_up, p, now))
                || now.duration_since(p.since) >= scoped::PENDING_TTL
        });
        if version == self.core.scoped.checked_at && !due {
            return Ok(false);
        }
        self.core.scoped.checked_at = version;
        let pending = self.core.scoped.take();
        let mut decided = false;
        // 한 묶음(가입 키 묶음 수십 개)은 기준점이 같으므로 기준점 상태를 한 번만 셉니다.
        let mut memo: HashMap<(Vec<EventId>, bool), At> = HashMap::new();
        for mut p in pending {
            let gaps = ScopedQueue::gaps_allowed(caught_up, &p, now);
            let blocked = !p.blockers.is_empty()
                && p.gaps_tried == gaps
                && p.blockers.iter().all(|(id, unknown)| {
                    if *unknown {
                        !self.core.resolver.contains(id)
                    } else {
                        self.core.resolver.awaiting_body(id)
                    }
                });
            if !blocked {
                let key = (p.ev.header.state_anchor.clone(), gaps);
                let at = match memo.get(&key) {
                    Some(at) => at.clone(),
                    None => {
                        let at = self.anchor_state(&key.0, gaps, generation);
                        memo.insert(key, at.clone());
                        at
                    }
                };
                match at {
                    At::Ready(state, complete) => {
                        self.decide_scoped(&p, state, complete)?;
                        decided = true;
                        continue;
                    }
                    At::Incomplete { unknown, undecoded } => {
                        if !unknown.is_empty() {
                            self.report.missing_parents = true;
                        }
                        p.blockers = unknown
                            .into_iter()
                            .map(|id| (id, true))
                            .chain(undecoded.into_iter().map(|id| (id, false)))
                            .take(scoped::MAX_BLOCKERS)
                            .collect();
                        p.gaps_tried = gaps;
                    }
                }
            }
            if now.duration_since(p.since) >= scoped::PENDING_TTL {
                debug!(event = %p.id.short(), "기준점을 끝내 받지 못한 부분 전달 이벤트를 버림");
                self.drop_provisional(p.id);
                continue;
            }
            for old in self.core.scoped.push(p) {
                self.drop_provisional(old);
            }
        }
        Ok(decided)
    }

    /// 기준점 상태. 빈 구간 없이 센 것은 공간마다 몇 개 기억해 둡니다(묶음이 여러 전송으로 나뉘어 와도 한 번만 셈).
    fn anchor_state(&mut self, anchor: &[EventId], gaps: bool, generation: u64) -> At {
        if let Some(st) = self.core.scoped.cached_anchor(anchor, generation) {
            return At::Ready(st, true);
        }
        match self.core.resolver.state_at(anchor, gaps) {
            AnchorState::Ready { state, gaps } => {
                let state = state.map(|s| Arc::new(*s));
                if !gaps {
                    self.core
                        .scoped
                        .cache_anchor(anchor.to_vec(), generation, state.clone());
                }
                At::Ready(state, !gaps)
            }
            AnchorState::Incomplete { unknown, undecoded } => At::Incomplete { unknown, undecoded },
        }
    }

    /// 센 기준점 상태로 정합니다. `complete`면 빈 구간 없이 센 상태라 거부를 기억해도 됩니다.
    fn decide_scoped(
        &mut self,
        p: &PendingScoped,
        st: Option<Arc<SpaceState>>,
        complete: bool,
    ) -> Result<()> {
        let Some(st) = st else {
            // 기준점 조상에 공간 생성이 없음: 어디서도 맞을 수 없는 기준점.
            self.refuse_scoped(
                p.id,
                complete,
                "부분 전달 이벤트 기준점",
                &AuthError::Invalid("기준점에 공간 생성이 없음"),
            );
            return Ok(());
        };
        self.finish_scoped(&p.ev, p.id, &st, complete, p.trusted)
    }

    /// 기준점 상태 `st`로 권한을 확인하고 저장합니다. 이 스트림들은 해소기에 들어가지 않아 노드마다 상태가 갈릴 일은
    /// 없지만, 키를 나눌 자격이 없는 기기가 보낸 키를 받아들이면 안 됩니다. 기준점 상태는 작성자가 고르므로 옛 기준점을
    /// 적으면 그때의 권한이 되살아납니다. 그래서 믿는 피어에게서 받은 가입·기기 연결 묶음(`trusted`, 그 피어가 지금 상태로
    /// 확인함)이 아니면 지금 상태로도 확인합니다(scoped_event에서 먼저 걸렀지만 그새 상태가 바뀌었을 수 있음).
    ///
    /// 받은 상태 키는 작성자가 기준점 상태(와 지금 상태)의 확정 기기일 때만 지금 키 후보가 되고, 그 가운데 논리 시계가 큰
    /// 묶음의 마지막 상태 키가 지금 키가 됩니다(확정 기기는 지난 키를 오래된 것부터 담고 지금 키를 마지막에 둠).
    fn finish_scoped(
        &mut self,
        ev: &SignedEvent,
        id: EventId,
        st: &SpaceState,
        complete: bool,
        trusted: bool,
    ) -> Result<()> {
        let h = &ev.header;
        if let Err(e) = authorize_scoped(st, &self.space, h) {
            self.refuse_scoped(id, complete, "부분 전달 이벤트 권한", &e);
            return Ok(());
        }
        let me = self.node.me()?;
        let cur = if trusted || h.author == me.device_id() || chain_update(h) {
            None
        } else {
            self.state.clone()
        };
        if let Some(cur) = &cur
            && let Err(e) = authorize_scoped(cur, &self.space, h)
        {
            let remember = self.current_settled(&h.state_anchor);
            self.refuse_scoped(id, remember, "부분 전달 이벤트 권한(지금 상태)", &e);
            return Ok(());
        }
        match h.scope {
            Some(Scope::Inbox(u)) if u == me.user() => {
                let bytes = ev.body.as_deref().unwrap_or_default();
                match open_sealed_body(&me.device_id(), &me.device.boxkey, h, bytes) {
                    Ok(EventBody::KeyShare(ks)) => {
                        if ks.for_user != Some(u) {
                            self.refuse_scoped(
                                id,
                                true,
                                "키 묶음",
                                &AuthError::Invalid("받는 사람이 다른 키 묶음"),
                            );
                            return Ok(());
                        }
                        let body = EventBody::KeyShare(ks);
                        if let Err(e) = authorize_message(st, h, &body, None) {
                            self.refuse_scoped(id, complete, "키 묶음 권한", &e);
                            return Ok(());
                        }
                        let EventBody::KeyShare(ks) = body else {
                            unreachable!()
                        };
                        let committer = st.committer == h.author
                            && cur.as_ref().is_none_or(|c| c.committer == h.author);
                        let promote = if committer {
                            Promote::Ranked(h.lamport)
                        } else {
                            Promote::Never
                        };
                        self.store(ev, SecurityLevel::Normal)?;
                        self.persist_keys_at(h, &ks, Some(st), promote)?;
                    }
                    Ok(_) => {
                        self.refuse_scoped(id, true, "키 묶음", &AuthError::WrongLane);
                        return Ok(());
                    }
                    // 내 다른 기기 앞으로 봉인한 묶음: 내 받은편지함이므로 저장만 합니다(그 기기가 나에게서도 받아 감).
                    Err(CryptoError::NotARecipient) => self.store(ev, SecurityLevel::Normal)?,
                    Err(_) => {
                        self.drop_provisional(id);
                        self.core.scoped.remember_rejected(id);
                        self.reject(id, "봉인 키 묶음을 열 수 없음");
                        return Ok(());
                    }
                }
            }
            Some(Scope::Inbox(_)) => {
                // 내가 쓴 것이거나, 전달자로서 받은 것: 저장해 그 사용자의 기기에게만 내줍니다.
                self.store(ev, SecurityLevel::Normal)?;
                if h.author != me.device_id() {
                    self.report.relay.push(ev.clone());
                }
            }
            Some(Scope::KeyGroup(g)) => {
                // 받을지는 지금 상태로 봅니다(기준점 상태는 작성자 권한에만): 갱신 때는 자격이 없었어도 지금 볼 수 있으면
                // 사슬을 이으려고 그 갱신이 필요합니다. 자격은 상태에 따라 바뀌므로 거부를 기억하지 않습니다.
                let viewer = self
                    .state
                    .as_ref()
                    .is_some_and(|s| s.can_view_key_group(&self.space, &g, &me.user()));
                if h.author != me.device_id() && !viewer {
                    self.report.rejected_by_state.push(id);
                    self.reject(id, "볼 수 없는 키 그룹");
                    return Ok(());
                }
                self.store(ev, SecurityLevel::Normal)?;
                // 그룹 키 갱신이면 그룹 사슬에 넣습니다(키를 아직 모르면 기다림).
                if h.kind == EventKind::KeyUpdate {
                    let core = &mut *self.core;
                    core.keys.ensure_loaded(self.node, &self.space);
                    core.keys
                        .on_group_event(self.node, &self.space, &core.resolver, ev, g);
                    self.keys_dirty = true;
                }
            }
            None => {
                self.reject(id, "범위 없는 부분 전달 이벤트");
                return Ok(());
            }
        }
        self.drop_provisional(id);
        self.accept(id);
        Ok(())
    }

    // ───────────── 메시지 레인 ─────────────

    /// `revoked`: 받기로 한 폐기된 기기의 글([`Self::vouched_revoked`]).
    fn message(&mut self, ev: SignedEvent, revoked: bool) -> Result<()> {
        let Some(state) = self.state.clone() else {
            self.hold(ev);
            return Ok(());
        };
        let state = &*state;
        let id = ev.id();
        let Some(bytes) = ev.body.as_deref() else {
            // 본문 없는 머리(삭제·폐기된 메시지): DAG 연결만. 이 메시지를 대상으로 기다리던 수정·삭제·반응도 깨웁니다.
            self.store(&ev, SecurityLevel::Normal)?;
            if matches!(
                ev.header.kind.base(),
                EventKind::Message | EventKind::ForumPost
            ) {
                self.wake(Need::Target(id));
            }
            self.accept(id);
            return Ok(());
        };
        let KeyRef::Record { root } = ev.header.key else {
            self.reject(id, "일반 채널 메시지는 기록 키로 암호화해야 함");
            return Ok(());
        };
        let Some(channel) = ev.header.channel else {
            self.reject(id, "채널 없는 메시지");
            return Ok(());
        };
        let root_key = match keys::root(self.node, &root)? {
            Some(k) => k,
            None => {
                // 풀지 못해도 볼 수 없는 채널에 쓴 것은 받지 않습니다(풀린 메시지는 authorize_message가 같은 것을 봄).
                // 안 그러면 못 보는 멤버가 아무 뿌리 ID로 모두의 디스크와 대기 목록을 채웁니다.
                if !state
                    .channel_permissions(&ev.header.user, &channel)
                    .contains(Permissions::VIEW_CHANNEL)
                {
                    let e = AuthError::MissingPermission(Permissions::VIEW_CHANNEL);
                    self.reject_auth(id, "메시지 권한", &e);
                    return Ok(());
                }
                // 모르는 뿌리는 (작성 기기, 채널)마다 예산 안에서만 파생해 보고 멤버에게 묻습니다. 아무 뿌리 ID나 붙인
                // 메시지로 받는 기기마다 일을 시키지 못하게(넘으면 저장해 두고 새 그룹 상태를 얻을 때 다시 봄).
                self.core.keys.ensure_loaded(self.node, &self.space);
                let look = self.core.keys.may_look_up_root(
                    ev.header.author,
                    channel,
                    &root,
                    Timestamp::now().as_millis(),
                );
                match look.then(|| self.find_root(&channel, &root)).flatten() {
                    Some(k) => {
                        keys::put_root(self.node, &self.space, &channel, &k)?;
                        k
                    }
                    None => {
                        self.store(&ev, SecurityLevel::Normal)?;
                        self.wait(Need::Record(channel, root), id);
                        if look {
                            self.core.keys.missed_root(root);
                            self.ask_roots.push((channel, root));
                        }
                        return Ok(());
                    }
                }
            }
        };
        let key = yangpa_crypto::keysched::record_key(&root_key);
        let body = match decrypt_body(&key, &ev.header, bytes) {
            Ok(b) => b,
            Err(_) => {
                self.reject(id, "메시지 복호화 실패");
                return Ok(());
            }
        };
        let target = match &body {
            EventBody::Edit { target, .. }
            | EventBody::Delete { target }
            | EventBody::Reaction { target, .. }
            | EventBody::ReactionRemove { target, .. } => Some(*target),
            _ => None,
        };
        let authorize = if revoked {
            authorize_revoked_device_message
        } else {
            authorize_message
        };
        let target_author = match target {
            Some(t) => match self.node.store.get_event(&t)? {
                Some(te) => {
                    if let Err(why) = check_target(&te.header, &ev.header) {
                        self.reject(id, why);
                        return Ok(());
                    }
                    Some(te.header.user)
                }
                None => {
                    // 대상을 아직 모름: 대상과 상관없는 권한만 먼저 보고, 대상이 오면 종류·채널·작성자를 다시 검사합니다
                    // (MANAGE_MESSAGES가 있어도 대상이 메시지인지 확인하기 전에는 적용하지 않음).
                    match authorize(state, &ev.header, &body, None) {
                        Ok(()) | Err(AuthError::UnknownTarget) => {
                            self.store(&ev, SecurityLevel::Normal)?;
                            self.wait(Need::Target(t), id);
                        }
                        Err(e) => self.reject_auth(id, "메시지 권한", &e),
                    }
                    return Ok(());
                }
            },
            None => None,
        };
        match authorize(state, &ev.header, &body, target_author) {
            Ok(()) => self.apply_message(ev, body),
            Err(e) => {
                self.reject_auth(id, "메시지 권한", &e);
                Ok(())
            }
        }
    }

    fn apply_message(&mut self, ev: SignedEvent, body: EventBody) -> Result<()> {
        let id = ev.id();
        let h = ev.header.clone();
        let me = self.node.me()?;
        let pe = PersistentEvent::new(ev.clone(), SecurityLevel::Normal)?;
        let mut updated = None;
        match body {
            EventBody::Message(m) => {
                let now = Timestamp::now().as_millis();
                let mentions_me = match &self.state {
                    Some(state) => crate::messages::mentions_me(state, &h, &m, &me.user(), now),
                    None => m.mentions.contains(&me.user()),
                };
                let mut rec = MessageRecord::from_header(
                    id,
                    &h,
                    MessageKind::Message,
                    MessageContent {
                        body: m.clone(),
                        forum: None,
                    },
                )?;
                rec.mentions_me = mentions_me;
                self.node.store.insert_events_with_views(&[pe], &[rec])?;
                self.note_parents(&ev)?;
                self.wake(Need::Target(id));
                if let Some(channel) = h.channel {
                    blobs::on_message_applied(self.node, &self.space, &channel, &id, &h, &m);
                }
                self.notify_new(&id, &h)?;
            }
            EventBody::ForumPost { title, tags, first } => {
                let attachments = (!first.attachments.is_empty()).then(|| first.clone());
                let rec = MessageRecord::from_header(
                    id,
                    &h,
                    MessageKind::ForumPost,
                    MessageContent {
                        body: first,
                        forum: Some(ForumMeta { title, tags }),
                    },
                )?;
                self.node.store.insert_events_with_views(&[pe], &[rec])?;
                self.note_parents(&ev)?;
                self.wake(Need::Target(id));
                if let (Some(channel), Some(first)) = (h.channel, &attachments) {
                    blobs::on_message_applied(self.node, &self.space, &channel, &id, &h, first);
                }
                self.notify_new(&id, &h)?;
                // 게시글보다 먼저 온 게시글 변경(상태 레인)을 이제 반영합니다.
                crews::on_forum_post_applied(
                    self.node,
                    &self.space,
                    self.state.as_deref(),
                    &id,
                    self.source.notifies(),
                )?;
            }
            EventBody::Edit {
                target,
                text,
                embeds,
            } => {
                self.node.store.insert_events(&[pe])?;
                self.note_parents(&ev)?;
                self.node.store.apply_edit(
                    &target,
                    &EditUpdate {
                        event: id,
                        lamport: h.lamport,
                        at: h.created_at,
                        text,
                        embeds,
                    },
                )?;
                updated = Some(target);
            }
            EventBody::Delete { target } => {
                self.node.store.insert_events(&[pe])?;
                self.note_parents(&ev)?;
                self.node
                    .store
                    .mark_message_deleted(&self.space, &target, h.created_at)?;
                updated = Some(target);
            }
            EventBody::Reaction { target, emoji } | EventBody::ReactionRemove { target, emoji } => {
                let present = h.kind == yangpa_core::event::EventKind::Reaction;
                self.node.store.insert_events(&[pe])?;
                self.note_parents(&ev)?;
                self.node.store.apply_reaction(&ReactionUpdate {
                    space: self.space,
                    target,
                    user: h.user,
                    emoji,
                    present,
                    event: id,
                    lamport: h.lamport,
                })?;
                updated = Some(target);
            }
            _ => {
                self.reject(id, "메시지 레인에 올 수 없는 본문");
                return Ok(());
            }
        }
        self.accept(id);
        if let (Some(target), Some(channel)) = (updated, h.channel)
            && self.source.notifies()
            && self.local_can_view(&channel)
            && let Some(rec) = self.node.store.get_message(&target, Some(&me.user()))?
        {
            self.node.notify.publish(Notification::MessageUpdated {
                space: self.space,
                channel,
                message: messages::view_of(self.node, &rec),
            });
        }
        Ok(())
    }

    fn notify_new(&self, id: &EventId, h: &yangpa_core::event::EventHeader) -> Result<()> {
        if !self.source.notifies() {
            return Ok(());
        }
        let Some(channel) = h.channel else {
            return Ok(());
        };
        // 볼 수 없는 채널의 내용을 알림으로 내보내지 않습니다.
        if !self.local_can_view(&channel) {
            return Ok(());
        }
        let me = self.node.me()?;
        if let Some(rec) = self.node.store.get_message(id, Some(&me.user()))? {
            let view = messages::view_of(self.node, &rec);
            let notify = messages::notify_view(self.node, &self.space, &rec);
            self.node.notify.publish(Notification::MessageNew {
                space: self.space,
                channel,
                message: view,
                notify,
            });
        }
        Ok(())
    }

    // ───────────── 정리: 해소기, 키 그룹, 대기 이벤트 ─────────────

    fn settle(&mut self) -> Result<()> {
        let mut keys_changed = false;
        for _ in 0..MAX_SETTLE_ROUNDS {
            let mut progress = false;
            if !self.decoded.is_empty() || !self.links.is_empty() {
                let decoded = std::mem::take(&mut self.decoded);
                let mut links = std::mem::take(&mut self.links);
                links.extend(self.below_floor_parents(&decoded, &links)?);
                crews::on_decoded(self.node, &decoded);
                let upd = self.core.resolver.insert(decoded, links);
                // 해소기가 바뀌면 크루 갱신 목록과 그룹 사슬의 기준점 판정이 달라질 수 있습니다.
                self.rejudge = true;
                for (id, e) in &upd.rejected {
                    self.report.accepted.retain(|x| x != id);
                    self.reject_auth(*id, "상태 규칙", e);
                    // 저장소에서 다시 적용한 판정은 이미 처리한 것입니다(재시작마다 다시 내지 않게).
                    if matches!(e, AuthError::LateEvent | AuthError::RetiredStateKey)
                        && !matches!(self.source, Source::Replay)
                    {
                        self.report.late.push(*id);
                    }
                }
                if !upd.checkpoints.is_empty() {
                    self.report.checkpoint = true;
                    crate::checkpoint::set_floor(
                        self.node,
                        &self.space,
                        self.core.resolver.floor(),
                    );
                }
                for c in &upd.conflicts {
                    warn!(
                        space = %self.space.short(),
                        base = %c.base.short(),
                        other = %c.other.short(),
                        same_author = c.same_author,
                        "겨룬 체크포인트를 봄"
                    );
                    let kind = if c.same_author {
                        CheckpointAlertKind::Equivocation
                    } else {
                        CheckpointAlertKind::Conflict
                    };
                    crate::checkpoint::alert(self.node, self.space, kind);
                }
                if upd.needs_update {
                    warn!(space = %self.space.short(), "더 새 해소기 판의 체크포인트: 업데이트가 필요합니다");
                }
                if !upd.key_events.is_empty() {
                    self.report.key_chain_moved = true;
                }
                let first = self.state.is_none() && self.core.resolver.state().is_some();
                if upd.state_changed || first {
                    self.apply_state()?;
                }
                // 해소기가 적용한 포럼 게시글 변경을 보기 행에 반영합니다(크루 영역).
                crews::on_resolved(
                    self.node,
                    &self.space,
                    self.state.as_deref(),
                    &upd,
                    self.source.notifies(),
                )?;
                progress = true;
            }
            if self.fill_below_floor() {
                progress = true;
            }
            if self.drain_keys()? {
                progress = true;
                keys_changed = true;
            }
            let woken = std::mem::take(&mut self.woken);
            if !woken.is_empty() {
                let ids: Vec<EventId> = woken.into_iter().collect();
                for ev in self.node.store.get_events(&ids)? {
                    self.process(ev, true)?;
                }
                progress = true;
            }
            if self.retry_held {
                self.retry_held = false;
                let held = std::mem::take(&mut self.core.held);
                self.core.held_bytes = 0;
                if !held.is_empty() {
                    self.retrying_held = true;
                    for ev in held {
                        self.process(ev, false)?;
                    }
                    self.retrying_held = false;
                    progress = true;
                }
            }
            // 부분 전달 이벤트는 다른 일이 다 끝난 뒤에 봅니다(기준점 상태를 세는 비용이 커서, 같은 묶음의 상태 이벤트를
            // 다 적용한 뒤 한 번만 세게).
            if !progress && self.settle_scoped()? {
                progress = true;
            }
            if !progress {
                break;
            }
        }
        if self.waiting_dirty {
            let all: BTreeSet<EventId> = self.core.waiting.values().flatten().copied().collect();
            let all: Vec<EventId> = all.into_iter().collect();
            self.node
                .store
                .put_setting(&spaces::wait_setting(&self.space), &all)?;
        }
        // 키 갱신 적용은 해소된 상태를 바꾸지 않아 apply_state가 알리지 않고, 그 알림이 키를 얻기 전에 나갔을 수도
        // 있습니다. 앱이 커밋 대기(pending_commit)를 다시 읽게 따로 알립니다.
        if keys_changed && self.source.notifies() {
            self.space_updated = true;
        }
        // 열지 못한 상태 이벤트의 크루 상태 키는 멤버(가입 중이면 받아 준 노드)에게 묻습니다(백그라운드, 잠깐 기다렸다가
        // 그래도 기다리는 것만).
        let asks: BTreeSet<StateKeyId> = std::mem::take(&mut self.ask_state)
            .into_iter()
            .filter(|k| self.core.waiting.contains_key(&Need::StateKey(*k)))
            .collect();
        if !asks.is_empty() {
            keygroup::relay::ask_state_keys(self.node, self.space, asks.into_iter().collect());
        }
        // 그룹 키로도 얻지 못한 뿌리는 같은 채널을 볼 수 있는 멤버에게 묻습니다(백그라운드, 같은 것은 잠시 다시 묻지 않음).
        let asks = std::mem::take(&mut self.ask_roots);
        if !asks.is_empty() {
            let mut by_channel: BTreeMap<ChannelId, Vec<ChannelRootId>> = BTreeMap::new();
            for (c, r) in asks {
                if self.core.waiting.contains_key(&Need::Record(c, r)) {
                    by_channel.entry(c).or_default().push(r);
                }
            }
            for (c, ids) in by_channel {
                keygroup::relay::ask_roots(self.node, self.space, c, ids);
            }
        }
        Ok(())
    }

    /// 이 묶음 항목의 부모 가운데 해소기가 모르고 저장소(또는 따라잡다 버린 기록)에 있는 경계 아래 이벤트를 순서
    /// 연결로 알려 줍니다. 경계 아래 부모는 판정에 쓰이지 않지만(효과가 없음), 모르면 해소기가 빠진 부모로 기다려
    /// 체크포인트를 낼 수 없습니다(재시작·새 멤버·압축한 노드는 경계 아래를 해소기에 다시 넣지 않음).
    fn below_floor_parents(
        &self,
        decoded: &[DecodedEvent],
        links: &[LinkEvent],
    ) -> Result<Vec<LinkEvent>> {
        let floor = self.core.resolver.floor();
        if floor == 0 {
            return Ok(Vec::new());
        }
        let batch: HashSet<EventId> = decoded
            .iter()
            .map(|e| e.id)
            .chain(links.iter().map(|l| l.id))
            .collect();
        let parents: BTreeSet<EventId> = decoded
            .iter()
            .filter(|e| e.header.lamport >= floor)
            .flat_map(|e| e.header.parents.iter())
            .chain(
                links
                    .iter()
                    .filter(|l| l.lamport >= floor)
                    .flat_map(|l| l.parents.iter()),
            )
            .filter(|p| !batch.contains(p) && !self.core.resolver.contains(p))
            .copied()
            .collect();
        let mut out = Vec::new();
        for p in parents {
            let lamport = match crate::checkpoint::below_lamport(self.node, &self.space, &p) {
                Some(l) => Some(l),
                None => self.node.store.event_meta(&p)?.map(|m| m.lamport),
            };
            if let Some(l) = lamport.filter(|l| *l < floor) {
                out.push(floor_link(p, l));
            }
        }
        Ok(out)
    }

    /// 해소기가 기다리는 부모 가운데 따라잡다 버린 경계 아래 이벤트를 순서 연결로 알려 줍니다(다음 정리 차례에 넣음).
    /// 넣을 것이 있으면 true.
    fn fill_below_floor(&mut self) -> bool {
        let floor = self.core.resolver.floor();
        if floor == 0 || !self.core.resolver.has_missing() {
            return false;
        }
        let resolver = &self.core.resolver;
        let fills: Vec<LinkEvent> =
            crate::checkpoint::below_waited(self.node, &self.space, |id| resolver.waits_for(id))
                .into_iter()
                .filter(|(_, l)| *l < floor)
                .map(|(id, l)| floor_link(id, l))
                .collect();
        let any = !fills.is_empty();
        self.links.extend(fills);
        any
    }

    fn apply_state(&mut self) -> Result<()> {
        let Some(new) = self.core.resolver.state().cloned() else {
            return Ok(());
        };
        let old = self.state.clone();
        if old.as_deref() == Some(&new) {
            return Ok(());
        }
        let new = Arc::new(new);
        // 저장은 마지막으로 쓴 판에 비춰 바뀐 행만(3만 명 상태를 통째로 다시 쓰지 않음). 기준이 없으면 통째로 다시 씀.
        match self.core.persisted.as_deref() {
            Some(base) => {
                let delta = yangpa_store::StateDelta::between(Some(base), &new)?;
                if !delta.is_empty() {
                    self.node.store.apply_state_delta(&self.space, &delta)?;
                    self.core.state_writes.deltas += 1;
                    self.core.state_writes.rows += delta.rows();
                }
            }
            None => {
                self.node.store.put_space_state(&self.space, &new)?;
                self.core.state_writes.full += 1;
            }
        }
        self.core.persisted = Some(new.clone());
        self.report
            .new_devices
            .extend(new_active_devices(old.as_deref(), &new));
        let genesis = self.core.resolver.genesis();
        if let Some(g) = &genesis {
            spaces::save_genesis(self.node, &self.space, g)?;
        }
        let genesis_lamport = match &genesis {
            Some(g) => self.node.store.event_meta(g)?.map(|m| m.lamport),
            None => None,
        };
        let member_since = self.core.resolver.member_since_map();
        if let Some(rt) = self.node.spaces.write().get_mut(&self.space) {
            rt.state = Some(new.clone());
            rt.genesis = genesis;
            rt.genesis_lamport = genesis_lamport;
            rt.member_since = member_since;
        }
        // 상태 색인은 사본마다라 옛 값이 새 사본에 쓰일 수 없습니다(`state_index`). 새 색인은 처음 물을 때 만들며, 기기 →
        // 사용자 표는 앞 색인에서 바뀐 멤버만 고칩니다(그래서 여기서 비우지 않음).
        self.state = Some(new.clone());
        self.report.state_changed = true;
        if self.source.notifies() {
            self.space_updated = true;
        }
        crews::on_state_applied(self.node, &self.space, old.as_deref(), &new);
        crate::identity::on_state_applied(self.node, &self.space, old.as_deref(), &new);
        // 상태 전에 받은 키 묶음을 이제 권한 확인해 저장합니다. 내 앞으로 봉인한 묶음이 아직 작성자를 몰라 통과하지
        // 못하면(상태를 다 받지 못함) 두었다가 상태가 바뀔 때 다시 봅니다.
        for p in std::mem::take(&mut self.core.provisional) {
            // 부분 전달 이벤트의 임시 키는 기준점 상태로만 확인합니다(`settle_scoped`).
            if p.header.scope.is_some() {
                self.core.provisional.push(p);
                continue;
            }
            let body = EventBody::KeyShare(p.body);
            match authorize_message(&new, &p.header, &body, None) {
                Ok(()) => {
                    let EventBody::KeyShare(ks) = body else {
                        unreachable!()
                    };
                    self.persist_keys(&p.header, &ks)?;
                }
                Err(e)
                    if p.header.key == KeyRef::Sealed
                        && matches!(&body, EventBody::KeyShare(ks)
                            if may_settle_later(self.node, &e, ks, &new)) =>
                {
                    let EventBody::KeyShare(ks) = body else {
                        unreachable!()
                    };
                    self.core.provisional.push(Provisional {
                        id: p.id,
                        header: p.header,
                        body: ks,
                    });
                }
                Err(e) => warn!(reason = %e, "상태 전에 받은 키 묶음 거부"),
            }
        }
        self.wake(Need::SpaceState);
        self.retry_held = true;
        crate::sync_service::refresh_space_peers(self.node, &self.space);
        Ok(())
    }

    /// 키 그룹 멤버 상태를 정리합니다(받아들인 크루 갱신·이긴 그룹 사슬 적용). 새 상태 키를 기다리던 이벤트를 깨우고,
    /// 막힌 그룹·조각 블롭은 밖에서 받아 오게 합니다(백그라운드).
    fn drain_keys(&mut self) -> Result<bool> {
        if !self.keys_dirty && !self.rejudge {
            return Ok(false);
        }
        self.keys_dirty = false;
        let rejudge = std::mem::take(&mut self.rejudge);
        let core = &mut *self.core;
        let d = core
            .keys
            .drain(self.node, &self.space, &core.resolver, rejudge)?;
        // 사슬 끝 상태가 아직 없으면(묶음을 기다림) 앞 키를 그대로 씁니다. 그 사이 쓴 상태 이벤트는 남은 멤버가 다 읽습니다.
        if let Some(k) = core.keys.crew_current(&self.space, &core.resolver) {
            keygroup::set_current_crew_key(self.node, &self.space, Some(k.state_key()));
        }
        let mut kids = std::mem::take(&mut self.new_state_keys);
        kids.extend(d.state_keys.iter().copied());
        for kid in &d.state_keys {
            self.wake(Need::StateKey(*kid));
        }
        // 그 상태 키를 기다리던 키 그룹 갱신을 다시 엽니다(다음 정리에서 적용).
        let again = self.core.keys.take_waiting(&kids);
        if !again.is_empty() {
            for ev in self.node.store.get_events(&again)? {
                if let StreamId::KeyGroup(g) = ev.header.stream() {
                    let core = &mut *self.core;
                    core.keys
                        .on_group_event(self.node, &self.space, &core.resolver, &ev, g);
                }
            }
            self.keys_dirty = true;
        }
        if d.progressed {
            self.retry_roots();
        }
        for g in d.stuck {
            keygroup::relay::ask_group(
                self.node,
                self.space,
                g,
                &self.core.keys,
                &self.core.resolver,
            );
        }
        for g in d.learn {
            keygroup::relay::ask_members(
                self.node,
                self.space,
                g,
                &self.core.keys,
                &self.core.resolver,
            );
        }
        for (seq, blob) in d.fetch {
            keygroup::shards::fetch(self.node, self.space, seq, blob);
        }
        // 그룹 사슬이 나아갔을 수 있습니다: 제거 반영을 기다리는 보내는 쪽을 깨웁니다.
        keygroup::notify_moved(self.node);
        Ok(d.progressed || self.keys_dirty)
    }

    /// 모르는 채널 뿌리를 보관한 그룹 상태에서 파생해 찾습니다.
    fn find_root(&mut self, channel: &ChannelId, root: &ChannelRootId) -> Option<Secret32> {
        self.core.keys.ensure_loaded(self.node, &self.space);
        let group = self
            .state
            .as_ref()
            .and_then(|st| keygroup::channel_group(self.node, &self.space, st, channel));
        self.core
            .keys
            .derive_root(&self.space, channel, group.as_ref(), root)
    }

    /// 뿌리를 기다리는 메시지를 새로 얻은 그룹 상태로 다시 봅니다(채널마다 후보를 한 번만 파생).
    fn retry_roots(&mut self) {
        let mut wanted: BTreeMap<ChannelId, Vec<ChannelRootId>> = BTreeMap::new();
        for n in self.core.waiting.keys() {
            if let Need::Record(c, r) = n {
                wanted.entry(*c).or_default().push(*r);
            }
        }
        if wanted.is_empty() {
            return;
        }
        self.core.keys.ensure_loaded(self.node, &self.space);
        for (c, ids) in wanted {
            let found = self.core.keys.candidate_roots(&self.space, &c);
            for id in ids {
                let Some((_, k)) = found.iter().find(|(x, _)| *x == id) else {
                    continue;
                };
                match keys::put_root(self.node, &self.space, &c, k) {
                    Ok(_) => self.wake(Need::Record(c, id)),
                    Err(e) => debug!(error = %e, "파생한 채널 뿌리 저장 실패"),
                }
            }
        }
    }
}

/// 경계 아래 이벤트의 순서 연결(부모 없이 ID·논리 시계만: 해소기가 경계 아래 기록으로 둠).
fn floor_link(id: EventId, lamport: u64) -> LinkEvent {
    LinkEvent {
        id,
        parents: Vec::new(),
        lamport,
        undecoded: false,
    }
}

/// 키 그룹 사슬의 갱신(`KeyGroup` 범위의 `KeyUpdate`). 작성 기기의 권한은 기준점 상태로만 보고, 지금 상태로 쓸 수
/// 있는지는 그룹 사슬(`GroupChain`)이 정합니다(옛 확정 기기의 정직한 갱신도 사슬을 잇는 데 필요).
fn chain_update(h: &EventHeader) -> bool {
    matches!(h.scope, Some(Scope::KeyGroup(_))) && h.kind == EventKind::KeyUpdate
}

/// 권한·상태 규칙 오류가 지금 상태에 따라 갈리는지. 상태와 상관없이 늘 틀린 것(종류·레인·헤더 구조·공간·논리 시계·
/// 순환·키 종류)만 아니라고 봅니다. 나머지는 멤버·권한·역할·채널·초대·태그·개수 한도처럼 상태가 바뀌면 결과가
/// 달라질 수 있습니다(애매하면 상태 쪽으로: 정직한 중계자를 감점하는 것보다 감점을 놓치는 편이 낫습니다).
fn depends_on_state(e: &AuthError) -> bool {
    !matches!(
        e,
        AuthError::KindMismatch
            | AuthError::WrongLane
            | AuthError::Malformed
            | AuthError::WrongSpace
            | AuthError::BadLamport
            | AuthError::Cycle
            | AuthError::WrongKey
    )
}

/// 상태를 더 받으면 통과할 수 있는 권한 오류인지(작성자·채널을 아직 모르거나 확정 기기 인계를 아직 모름). 내 사용자
/// 앞으로 봉인한 묶음만이며, 권한 부족은 상태 키만 담은 묶음을 내가 소유자·관리자일 때(확정 기기가 될 수 있어 인계를 받을
/// 수 있음)만 봅니다. 그 밖의 묶음은 거부해 남이 끝내 통과하지 못할 묶음으로 저장소·임시 목록을 채우지 못하게 합니다.
fn may_settle_later(node: &Node, e: &AuthError, ks: &KeyShareBody, state: &SpaceState) -> bool {
    let Ok(me) = node.me() else { return false };
    let me = me.user();
    if ks.for_user != Some(me) {
        return false;
    }
    match e {
        AuthError::NotMember | AuthError::UnknownDevice | AuthError::UnknownChannel => true,
        AuthError::MissingPermission(_) => {
            ks.keys.iter().all(|k| matches!(k, SharedKey::State { .. }))
                && (state.owner == me
                    || state
                        .base_permissions(&me)
                        .contains(Permissions::ADMINISTRATOR))
        }
        _ => false,
    }
}

/// RAM 보류 상한 계산용 크기(머리 몫을 넉넉히 더함).
fn held_size(ev: &SignedEvent) -> usize {
    ev.body.as_ref().map_or(0, |b| b.len()) + 512
}

/// 수정·삭제·반응의 대상 검사: 같은 공간·같은 채널의 메시지·게시글이어야 합니다(상태·키 갱신·키 묶음 이벤트나 다른 채널의
/// 메시지를 지우면 공간을 다시 세울 수 없거나 그 채널 권한을 건너뛰게 됨).
fn check_target(target: &EventHeader, ev: &EventHeader) -> std::result::Result<(), &'static str> {
    if target.space != ev.space {
        return Err("다른 공간의 메시지를 대상으로 함");
    }
    if !matches!(
        target.kind.base(),
        EventKind::Message | EventKind::ForumPost
    ) {
        return Err("메시지가 아닌 이벤트를 대상으로 함");
    }
    if target.channel != ev.channel {
        return Err("다른 채널의 메시지를 대상으로 함");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors_split_by_state() {
        // 상태가 바뀌면 결과가 달라질 수 있는 오류: 전달한 피어는 다른 시점의 상태로 통과시켰을 수 있음.
        for e in [
            AuthError::NotMember,
            AuthError::UnknownDevice,
            AuthError::MissingPermission(Permissions::SEND_MESSAGES),
            AuthError::UnknownChannel,
            AuthError::RankTooLow,
            AuthError::Invalid("게시글 태그"),
            AuthError::Limit("채널 수"),
        ] {
            assert!(depends_on_state(&e), "{e}");
        }
        // 어느 상태에서도 틀린 것: 전달한 피어도 걸렀어야 함.
        for e in [
            AuthError::KindMismatch,
            AuthError::WrongLane,
            AuthError::Malformed,
            AuthError::WrongSpace,
            AuthError::BadLamport,
            AuthError::Cycle,
            AuthError::WrongKey,
        ] {
            assert!(!depends_on_state(&e), "{e}");
        }
    }

    #[test]
    fn report_separates_invalid_from_state_rejects() {
        let (a, b, c) = (
            EventId::from_bytes([1; 32]),
            EventId::from_bytes([2; 32]),
            EventId::from_bytes([3; 32]),
        );
        let mut r = IngestReport::default();
        r.rejected.push((a, "메시지 권한: 권한 없음".into()));
        r.rejected_by_state.push(a);
        // 이번 묶음 밖의 이벤트(다시 해소하며 거부로 바뀐 옛 이벤트 등)는 묶음 판정에 넣지 않음.
        r.rejected.push((c, "메시지 복호화 실패".into()));
        let batch: HashSet<EventId> = [a, b].into_iter().collect();
        assert!(!r.has_invalid(&batch));
        assert!(r.has_rejected_by_state(&batch));
        r.rejected.push((b, "서명·본문 해시가 맞지 않음".into()));
        assert!(r.has_invalid(&batch));
    }
}
