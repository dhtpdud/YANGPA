//! 키 그룹(protocol.md "키 그룹")의 데몬 쪽.
//!
//! - [`committer`]: 확정 기기가 키 그룹 표를 들고 상태에 맞춰 `KeyUpdate`를 냅니다(묶음 창 뒤 한 바퀴).
//! - [`member`]: 모든 기기가 받아들인 갱신을 적용해 그룹 키(T·R)와 상태 키·채널 뿌리를 얻습니다(적용 잠금 안).
//! - [`relay`]: 놓친 채널 뿌리·지금 T·R을 같은 공간 멤버에게 묻고 답합니다.
//! - [`shards`]: 크루 트리 전체 재키의 기기 몫을 사용자별 조각 블롭으로 내고 받습니다.
//! - 여기: 묶음 창(300ms, 제거가 이어지면 2초까지, 소유자·관리자의 즉시 요청은 바로), 확정 기기 표 넘기기, 보내는 쪽 규칙
//!   (제거를 반영한 갱신을 기다림), 지금 크루 상태 키.
//!
//! 잠금: 바퀴는 그 공간의 적용 잠금 안에서 돕니다(`ingest::lock_space`). 여기 상태는 노드 수준 잎 잠금이라 어느
//! 단계에서나 잠깐 쥐고, 쥔 채로 다른 잠금을 잡지 않습니다. 적용 잠금 없이 부르는 보내는 쪽 규칙·`space.get`은
//! `SpaceCore`를 잠그지 않고 적용이 끝날 때마다 새로 쓰는 요약([`KeySummary`], 공간마다 잎 잠금)만 읽습니다(긴 적용
//! 동안 비동기 작업 스레드를 묶지 않게, daemon.md 잠금 규칙).

pub(crate) mod committer;
pub(crate) mod member;
pub(crate) mod relay;
pub(crate) mod shards;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{Notify, watch};
use tracing::{debug, info};
use yangpa_core::body::{EventBody, KeyShareBody, Secret32, SharedKey, state_key_id};
use yangpa_core::event::Scope;
use yangpa_core::ids::{ChannelId, DeviceId, EventId, KeyGroupId, SpaceId, StateKeyId, UserId};
use yangpa_core::model::{HistoryVisibility, Permissions, SpaceState};
use yangpa_core::wire::{KeyTableHandover, WireErrorCode, YpRequest, YpResponse};
use yangpa_crypto::keytree::{SealedHandover, open_handover, seal_handover};
use yangpa_net::PeerId;
use yangpa_rpc::api::Notification;

pub(crate) use member::{MemberState, channel_group};

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{ingest, peers, sync_service};

/// (공간, 받는 확정 기기, 크루 순번).
type HandoverKey = (SpaceId, DeviceId, u64);

/// 적용 잠금 없이 읽는 키 그룹 요약(`SpaceRuntime::key_summary`, 잎 잠금). 적용(`ingest::run_with`)이 끝날 때마다 새로
/// 씁니다. 비밀은 없습니다.
#[derive(Clone, Default)]
pub(crate) struct KeySummary {
    /// 해소기의 크루 키 사슬과 기기 주인(상태 전이면 `None`).
    pub(crate) kv: Option<yangpa_sync::KeyView>,
    /// 크루 사슬 끝 상태(지금 크루 키)를 가졌는지.
    pub(crate) crew_current: bool,
    pub(crate) groups: HashMap<KeyGroupId, GroupSummary>,
}

/// 비공개·덮개 그룹 하나의 요약.
#[derive(Clone)]
pub(crate) struct GroupSummary {
    /// 이긴 사슬 끝 순번.
    pub(crate) tip_seq: u64,
    /// 지금 보내는 데 쓸 수 있는 사슬 끝.
    pub(crate) send_tip: Option<yangpa_sync::GroupTip>,
    /// 그 끝의 내 상태(지금 그룹 키)를 가졌는지.
    pub(crate) current: bool,
}

impl KeySummary {
    /// 적용 잠금 안에서 지금 상태로 만듭니다.
    pub(crate) fn of(
        space: &SpaceId,
        keys: &MemberState,
        resolver: &yangpa_sync::Resolver,
    ) -> Self {
        KeySummary {
            kv: resolver.key_view(),
            crew_current: keys.crew_current(space, resolver).is_some(),
            groups: keys
                .chains
                .iter()
                .map(|(g, c)| {
                    (
                        *g,
                        GroupSummary {
                            tip_seq: c.tip().seq,
                            send_tip: c.send_tip(),
                            current: keys.group_current(g).is_some(),
                        },
                    )
                })
                .collect(),
        }
    }
}

/// 공간의 키 그룹 요약. 공간이 없으면 `None`, 아직 첫 적용이 끝나지 않았는데 지금 적용 중이면 `Some(None)`(부르는 쪽이
/// 조심스러운 답을 냄). 첫 적용 전이고 적용 중이 아니면 지금 상태로 만들어 둡니다(`SpaceCore`는 기다리지 않음).
pub(crate) fn summary(node: &Node, space: &SpaceId) -> Option<Option<Arc<KeySummary>>> {
    let (slot, core) = {
        let spaces = node.spaces.read();
        let rt = spaces.get(space)?;
        (rt.key_summary.clone(), rt.core.clone())
    };
    if let Some(s) = slot.lock().clone() {
        return Some(Some(s));
    }
    let Some(c) = core.try_lock() else {
        return Some(None);
    };
    // `SpaceCore`를 쥔 채 씁니다: 그 사이 끝난 적용이 쓴 더 새 요약을 덮지 않게(적용도 쥔 채 씀).
    let mut g = slot.lock();
    let s = g
        .get_or_insert_with(|| Arc::new(KeySummary::of(space, &c.keys, &c.resolver)))
        .clone();
    Some(Some(s))
}

/// 요약의 크루 키 사슬(요약이 아직 없으면 `None`).
fn summary_kv(node: &Node, space: &SpaceId) -> Option<yangpa_sync::KeyView> {
    summary(node, space).flatten().and_then(|s| s.kv.clone())
}

/// 키 그룹 영역의 노드 수준 상태.
pub(crate) struct KeyGroupState {
    /// 지금 바퀴를 도는 공간과 다시 돌릴지.
    running: Mutex<BTreeMap<SpaceId, bool>>,
    /// 공간마다 (요청 순번, 끝낸 순번).
    rounds: Mutex<BTreeMap<SpaceId, (u64, u64)>>,
    done: Notify,
    /// 시험용: 켜 두면 바퀴를 미룹니다(동시 가입을 갱신 하나로 묶는지 보려고).
    hold: watch::Sender<bool>,
    /// 공간마다 마지막으로 빚(제거할 기기)이 늘어난 때와 그때의 빚 수. 묶음 창을 늘리는 데 씁니다.
    pace: Mutex<HashMap<SpaceId, (Instant, usize)>>,
    /// 소유자·관리자의 즉시 요청(추방·밴·기기 폐기)이 온 공간: 묶음 창을 기다리지 않습니다.
    urgent: Mutex<HashSet<SpaceId>>,
    flush: Notify,
    /// 확정 기기 표 캐시(잠긴 키 저장소의 비밀값을 매 바퀴 풀지 않게).
    pub(crate) tables: Mutex<HashMap<SpaceId, committer::Table>>,
    /// 다음 바퀴에 크루 전체 재키를 할 공간(보유 기기를 그룹에서 빼야 함 등).
    pub(crate) force_full: Mutex<HashSet<SpaceId>>,
    /// 지금 크루 상태 키(크루 사슬 끝을 적용한 상태에서 나온 것). 상태 이벤트를 쓸 때 씁니다.
    current: Mutex<HashMap<SpaceId, (StateKeyId, Secret32)>>,
    /// 내가 확정 기기가 됐는데 표가 없던 때(넘겨받기를 기다림).
    committer_since: Mutex<HashMap<(SpaceId, DeviceId), Instant>>,
    /// 표를 넘긴 (공간, 받는 기기, 크루 순번).
    handed: Mutex<HashSet<(SpaceId, DeviceId, u64)>>,
    /// 멤버가 크루 키 그룹 밖의 확정 기기에게 상태 키를 넘기는 일: (공간, 확정 기기, 크루 순번) → (처음 본 때, 넘겼는지).
    member_handed: Mutex<HashMap<HandoverKey, (Instant, bool)>>,
    /// 넘겨받은 표가 그룹 사슬보다 앞서 그 사슬을 따라잡기를 기다린 처음 때. `key_handover_wait`가 지나면 사슬 끝에서
    /// 다시 시작합니다(끝없이 기다리지 않게).
    pub(crate) lag_since: Mutex<HashMap<(SpaceId, KeyGroupId), Instant>>,
    /// 중계 요청을 마지막으로 보낸 때(같은 것을 자꾸 묻지 않게).
    pub(crate) asked: Mutex<HashMap<(SpaceId, [u8; 32]), Instant>>,
    /// 보내는 쪽이 제거 반영을 기다리다 옛 키로 보낸 공간(`space.get`의 `key_rotation_pending`).
    rotation_pending: Mutex<HashSet<SpaceId>>,
    /// 시험용: 켜 두면 놓친 채널 뿌리를 멤버에게 묻지 않습니다(중계 답을 시험이 직접 넣을 때).
    pub(crate) relay_hold: std::sync::atomic::AtomicBool,
    /// 시험용: 가입자에게 주는 지난 상태 키 수(0이면 `keys::JOINER_STATE_KEYS`). 묻기 경로를 짧은 기록으로 시험합니다.
    pub(crate) joiner_keys: std::sync::atomic::AtomicUsize,
    /// 상태·키 사슬이 바뀔 때 깨웁니다(제거 반영을 기다리는 보내는 쪽이 짧은 간격으로 다시 보지 않게).
    moved: Notify,
    /// 채널마다 마지막 [`removal_pending`] 답과 그때의 상태·사슬 끝. 둘 다 그대로면 그룹 멤버 전체를 다시 훑지 않습니다.
    pending_cache: Mutex<HashMap<(SpaceId, ChannelId), PendingMemo>>,
    /// 확정 기기가 마지막으로 다 끝낸 바퀴의 상태와 지문. 그대로면 바퀴를 쉽니다(표·상자 키·그룹 계산을 매번 하지 않게).
    pub(crate) last_round: Mutex<HashMap<SpaceId, committer::RoundMemo>>,
    /// 그룹 사슬이 받아들이지 않을 갱신을 내지 않고 미룬 그룹과 다음 대기(두 배씩 늘림).
    pub(crate) group_retry: Mutex<HashMap<(SpaceId, KeyGroupId), Duration>>,
    /// 확정 기기가 적용마다 본 기록 공개 채널과 그때 쓰던 그룹(`None`은 크루). 바퀴 사이(묶음 창 안)에 만들고 공개로 쓰다
    /// 끈 채널을 바퀴가 놓치지 않게 합니다(RAM 전용, 비밀 없음).
    pub(crate) public_seen: Mutex<HashMap<(SpaceId, ChannelId), Option<KeyGroupId>>>,
    /// 크루 표를 넘겨받은 때. 그 뒤 `key_handover_wait` 동안은 표에 없는 그룹을 사슬 끝에서 이어받지 않고 그룹 요청을
    /// 기다립니다(그룹마다 따로 넘기므로).
    pub(crate) received: Mutex<HashMap<SpaceId, Instant>>,
    /// 조각 블롭을 내주는 요청 기기마다 바이트 예산(`shards::allow_serve`).
    pub(crate) shard_budget: Mutex<HashMap<DeviceId, yangpa_sync::policy::TokenBucket>>,
    /// 확정 기기가 상자 키를 멤버에게 물은 기기와 때(공간마다, `committer::note_key_fetch`).
    pub(crate) key_fetch: Mutex<HashMap<SpaceId, HashMap<DeviceId, Instant>>>,
}

/// [`removal_pending`]의 답을 다시 쓸 수 있는 조건: 같은 상태(같은 `Arc`, 들고 있어 다른 상태가 같은 주소를 받지
/// 못함)와 같은 쓸 수 있는 사슬 끝.
struct PendingMemo {
    state: Arc<SpaceState>,
    tip: (u64, Option<EventId>),
    pending: bool,
}

impl Default for KeyGroupState {
    fn default() -> Self {
        KeyGroupState {
            running: Mutex::default(),
            rounds: Mutex::default(),
            done: Notify::new(),
            hold: watch::channel(false).0,
            pace: Mutex::default(),
            urgent: Mutex::default(),
            flush: Notify::new(),
            tables: Mutex::default(),
            force_full: Mutex::default(),
            current: Mutex::default(),
            committer_since: Mutex::default(),
            handed: Mutex::default(),
            member_handed: Mutex::default(),
            lag_since: Mutex::default(),
            asked: Mutex::default(),
            rotation_pending: Mutex::default(),
            relay_hold: std::sync::atomic::AtomicBool::new(false),
            joiner_keys: std::sync::atomic::AtomicUsize::new(0),
            moved: Notify::new(),
            pending_cache: Mutex::default(),
            last_round: Mutex::default(),
            group_retry: Mutex::default(),
            public_seen: Mutex::default(),
            received: Mutex::default(),
            shard_budget: Mutex::default(),
            key_fetch: Mutex::default(),
        }
    }
}

impl KeyGroupState {
    /// 신원을 지울 때 RAM의 키를 모두 버립니다.
    pub(crate) fn clear(&self) {
        self.tables.lock().clear();
        self.current.lock().clear();
        self.force_full.lock().clear();
        self.committer_since.lock().clear();
        self.handed.lock().clear();
        self.member_handed.lock().clear();
        self.lag_since.lock().clear();
        self.asked.lock().clear();
        self.rotation_pending.lock().clear();
        self.pending_cache.lock().clear();
        self.last_round.lock().clear();
        self.group_retry.lock().clear();
        self.received.lock().clear();
        self.shard_budget.lock().clear();
    }
}

/// 상태나 키 사슬이 바뀜: 제거 반영을 기다리는 보내는 쪽을 깨웁니다.
pub(crate) fn notify_moved(node: &Node) {
    node.keygroup.moved.notify_waiters();
}

// ───────────── 지금 크루 상태 키 ─────────────

/// 크루 사슬 끝을 적용한 상태의 상태 키(없으면 `None`: 아직 가입 갱신을 적용하지 못함).
pub(crate) fn current_crew_key(node: &Node, space: &SpaceId) -> Option<(StateKeyId, Secret32)> {
    node.keygroup.current.lock().get(space).cloned()
}

/// 멤버 정리 뒤 지금 크루 상태 키를 고칩니다(적용 잠금 안).
pub(crate) fn set_current_crew_key(node: &Node, space: &SpaceId, s: Option<Secret32>) {
    let mut cur = node.keygroup.current.lock();
    match s {
        Some(s) => {
            cur.insert(*space, (state_key_id(space, &s), s));
        }
        None => {
            cur.remove(space);
        }
    }
}

/// 공간을 지울 때.
pub(crate) fn forget_space(node: &Node, space: &SpaceId) {
    node.keygroup.current.lock().remove(space);
    node.keygroup.force_full.lock().remove(space);
    node.keygroup.rotation_pending.lock().remove(space);
    node.keygroup.pace.lock().remove(space);
    node.keygroup
        .committer_since
        .lock()
        .retain(|(s, _), _| s != space);
    node.keygroup.handed.lock().retain(|(s, _, _)| s != space);
    node.keygroup.asked.lock().retain(|(s, _), _| s != space);
    node.keygroup
        .pending_cache
        .lock()
        .retain(|(s, _), _| s != space);
    node.keygroup
        .group_retry
        .lock()
        .retain(|(s, _), _| s != space);
    node.keygroup.received.lock().remove(space);
    node.keygroup.key_fetch.lock().remove(space);
    committer::drop_table(node, space);
    committer::forget_meta(node, space);
    shards::forget_space(node, space);
}

/// 크루 키를 얻어 메시지를 주고받을 수 있는지(가입이 끝났는지).
pub(crate) fn has_crew_key(node: &Node, space: &SpaceId) -> bool {
    current_crew_key(node, space).is_some()
}

// ───────────── 확정 기기 바퀴 ─────────────

/// 상태가 바뀐 뒤(또는 주기적으로) 부릅니다. 내가 확정 기기면 바퀴를 요청하고, 아니면 내가 든 표를 새 확정 기기에게
/// 넘깁니다.
pub(crate) fn on_state_changed(node: &Arc<Node>, space: SpaceId) {
    notify_moved(node);
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    // 적용 잠금 밖(적용 뒤 알림)에서 부르므로 `SpaceCore` 대신 방금 쓴 요약을 읽습니다.
    let kv = summary_kv(node, &space);
    if crate::checkpoint::is_unconfirmed(node, &space) {
        // 한 출처에서만 받은 체크포인트로 세운 상태(위조됐을 수 있음)가 지목한 확정 기기에게 표·키를 넘기지 않고, 바퀴도
        // 돌지 않습니다. 확인되면 다시 부릅니다(`checkpoint::confirm`).
        return;
    }
    if state.committer != me.device_id() {
        maybe_handover(node, space, &state, kv.as_ref());
        maybe_member_handover(node, space, &state, kv.as_ref());
        return;
    }
    {
        let mut seen = node.keygroup.public_seen.lock();
        for (c, ch) in &state.channels {
            if ch.uses_group_keys() && ch.history == HistoryVisibility::Public {
                seen.insert((space, *c), state.channel_key_group(&space, ch));
            }
        }
    }
    // 빚이 늘었으면 묶음 창을 늘릴 근거로 적어 둡니다(대량 추방을 갱신 하나로).
    if let Some(kv) = &kv {
        let n = kv.crew.debt.len();
        let mut pace = node.keygroup.pace.lock();
        let grew = pace.get(&space).is_none_or(|(_, before)| n > *before);
        if n > 0 && grew {
            pace.insert(space, (Instant::now(), n));
        } else if n == 0 {
            pace.remove(&space);
        }
    }
    request_round(node, space);
}

/// 크루 키 사슬만 나아갔을 때(해소된 상태는 그대로): 확정 기기가 아닌 기기가 들고 있던 표가 낡았으면 버리고, 크루 밖
/// 확정 기기에게 상태 키를 넘길지 봅니다. 확정 기기는 자기가 낸 갱신마다 바퀴를 다시 돌 까닭이 없어 건드리지 않습니다.
pub(crate) fn on_key_chain_moved(node: &Arc<Node>, space: SpaceId) {
    notify_moved(node);
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    if state.committer == me.device_id() || crate::checkpoint::is_unconfirmed(node, &space) {
        return;
    }
    let kv = summary_kv(node, &space);
    maybe_handover(node, space, &state, kv.as_ref());
    maybe_member_handover(node, space, &state, kv.as_ref());
}

/// 묶음 창을 기다리지 않고 바로 돕니다: 명시적 요청이나 제거 반영을 기다리는 보내는 쪽만 씁니다. 추방·밴은 창을 열고
/// 늘릴 뿐입니다(대량 추방을 갱신 하나로).
pub(crate) fn urgent(node: &Arc<Node>, space: SpaceId) {
    node.keygroup.urgent.lock().insert(space);
    node.keygroup.flush.notify_waiters();
    on_state_changed(node, space);
}

/// 바퀴를 요청합니다(돌고 있으면 끝난 뒤 한 번 더). 이 요청의 순번을 돌려줍니다.
fn request_round(node: &Arc<Node>, space: SpaceId) -> u64 {
    let target = {
        let mut rounds = node.keygroup.rounds.lock();
        let r = rounds.entry(space).or_insert((0, 0));
        r.0 += 1;
        r.0
    };
    {
        let mut running = node.keygroup.running.lock();
        if let Some(rerun) = running.get_mut(&space) {
            *rerun = true;
            return target;
        }
        running.insert(space, false);
    }
    node.spawn_detached(move |node| async move {
        loop {
            batch_wait(&node, space).await;
            let mut hold = node.keygroup.hold.subscribe();
            let _ = hold.wait_for(|h| !*h).await;
            let upto = node.keygroup.rounds.lock().get(&space).map_or(0, |r| r.0);
            if let Err(e) = round(&node, space).await {
                debug!(space = %space.short(), error = %e, "키 그룹 바퀴 실패");
            }
            if let Some(r) = node.keygroup.rounds.lock().get_mut(&space) {
                r.1 = r.1.max(upto);
            }
            node.keygroup.done.notify_waiters();
            let again = {
                let mut running = node.keygroup.running.lock();
                match running.get_mut(&space) {
                    Some(rerun) if *rerun => {
                        *rerun = false;
                        true
                    }
                    _ => {
                        running.remove(&space);
                        false
                    }
                }
            };
            if !again {
                break;
            }
        }
    });
    target
}

/// 묶음 창: `commit_batch_window`를 기다리고, 그 사이 빚이 또 늘었으면 `commit_batch_max`까지 한 창씩 늘립니다. 즉시
/// 요청이 오면 바로 끝냅니다.
async fn batch_wait(node: &Arc<Node>, space: SpaceId) {
    let window = node.cfg.commit_batch_window;
    let max = node.cfg.commit_batch_max.max(window);
    let start = Instant::now();
    loop {
        let flushed = node.keygroup.flush.notified();
        tokio::pin!(flushed);
        flushed.as_mut().enable();
        if node.keygroup.urgent.lock().remove(&space) {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(window) => {}
            _ = &mut flushed => {
                if node.keygroup.urgent.lock().remove(&space) {
                    return;
                }
                continue;
            }
        }
        let recent = node
            .keygroup
            .pace
            .lock()
            .get(&space)
            .is_some_and(|(t, _)| t.elapsed() < window);
        if !recent || start.elapsed() + window > max {
            return;
        }
    }
}

/// 바퀴 하나: 잠금 안에서 갱신을 만들고, 잠금 밖에서 전파합니다.
async fn round(node: &Arc<Node>, space: SpaceId) -> Result<()> {
    let l = ingest::lock_space(node, space).await;
    let out = committer::round_locked(node, &l, space)?;
    drop(l);
    if !out.fetch_keys.is_empty() {
        let n = out.fetch_keys.len();
        let got = crate::checkpoint::prefetch_device_keys(node, space, out.fetch_keys).await;
        debug!(space = %space.short(), want = n, got, "확정 기기가 봉인할 상자 키를 먼저 물음");
        request_round(node, space);
        return Ok(());
    }
    if !out.events.is_empty() {
        sync_service::propagate(node, space, out.events);
        node.notify.publish(Notification::SpaceUpdated { space });
    }
    if out.again {
        request_round(node, space);
    }
    if let Some(d) = out.retry_after {
        node.spawn_detached(move |node| async move {
            tokio::time::sleep(d).await;
            request_round(&node, space);
        });
    }
    Ok(())
}

/// 바퀴를 요청하고 그 요청이 반영될 때까지 기다립니다(초대를 받아 준 확정 기기가 응답 전에 가입자를 넣을 때). 내가
/// 확정 기기가 아니면 바로 돌아오고, 오래 걸리면 기다리지 않습니다(가입자는 따라잡기로 받음).
pub(crate) async fn reconcile_batched(node: &Arc<Node>, space: SpaceId) {
    let is_committer = node
        .space_state(&space)
        .zip(node.me().ok())
        .is_some_and(|(st, me)| st.committer == me.device_id());
    if !is_committer {
        return;
    }
    let target = request_round(node, space);
    let deadline = tokio::time::Instant::now() + node.cfg.net.request_timeout / 2;
    loop {
        let notified = node.keygroup.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let finished = node
            .keygroup
            .rounds
            .lock()
            .get(&space)
            .is_some_and(|r| r.1 >= target);
        if finished {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            debug!(space = %space.short(), "키 그룹 바퀴를 기다리다 그만둠");
            return;
        }
    }
}

/// 시험용: 바퀴를 미루거나(`true`) 다시 돌립니다.
pub(crate) fn set_hold(node: &Node, hold: bool) {
    node.keygroup.hold.send_replace(hold);
}

/// 표 없이 확정 기기가 됐을 때 넘겨받기를 더 기다릴 시간(`None`이면 기다림이 끝나 공개 끝으로 이어받음).
pub(crate) fn handover_wait(node: &Node, space: &SpaceId) -> Option<Duration> {
    let me = node.me().ok()?.device_id();
    let wait = node.cfg.key_handover_wait;
    let since = *node
        .keygroup
        .committer_since
        .lock()
        .entry((*space, me))
        .or_insert_with(Instant::now);
    let gone = since.elapsed();
    (gone < wait).then(|| wait - gone)
}

/// 확정 기기가 키 그룹을 맞출 일이 남았는지(`space.get`의 `pending_commit`): 크루 키가 아직 없거나, 빚(뺄 기기)이
/// 있거나, 크루에 들지 않은 활성 기기가 있거나, 전체 재키를 요구받았거나, 내가 볼 수 있는 비공개 그룹에 아직 들지
/// 못함(사슬 끝 멤버를 아직 모르면 든 것으로 치지 않음). 적용 잠금 없이 요약만 읽습니다.
pub(crate) fn pending_commit(node: &Node, space: &SpaceId, state: &Arc<SpaceState>) -> bool {
    let Ok(me) = node.me() else { return false };
    let sum = match summary(node, space) {
        None => return false,
        // 시작 직후 첫 적용 중: 맞출 일이 남았다고 봅니다.
        Some(None) => return true,
        Some(Some(s)) => s,
    };
    let Some(kv) = &sum.kv else {
        return true;
    };
    if kv.crew.seq == 0
        || !kv.crew.debt.is_empty()
        || kv.crew.rekey_required
        || kv.owners.keys().any(|d| !kv.crew.members.contains(d))
    {
        return true;
    }
    if !sum.crew_current {
        return true;
    }
    for (ch_id, ch) in &state.channels {
        if !ch.uses_group_keys() {
            continue;
        }
        if !state
            .channel_permissions(&me.user(), ch_id)
            .contains(Permissions::VIEW_CHANNEL)
        {
            continue;
        }
        let Some(g) = channel_group(node, space, state, ch_id) else {
            continue;
        };
        let Some(gs) = sum.groups.get(&g) else {
            return true;
        };
        match &gs.send_tip {
            Some(tip) if tip.members_known && tip.members.contains(&me.device_id()) => {}
            _ => return true,
        }
        if !gs.current {
            return true;
        }
    }
    false
}

// ───────────── 보내는 쪽 규칙 ─────────────

/// 이 채널의 키 그룹에 아직 반영되지 않은 제거가 있는지: 크루 빚·전체 재키 요구, 그룹 사슬에 지금 쓸 수 있는 끝이
/// 없음(작성 기기가 자격을 잃어 새 확정 기기의 갱신을 기다림), 사슬 끝 멤버를 아직 모름(체크포인트 키 머리에서 세운
/// 바닥: 누가 빠져야 하는지 셀 수 없음), 크루 전체 재키 뒤 그룹이 아직 새 T를 내지 않음(넘긴 표에 그 T가 있었음), 또는
/// 사슬 끝 멤버 가운데 이제 채널을 볼 수 없는 사용자의 기기. 마지막 것은 그룹 멤버 수만큼 권한을 계산하므로 상태와
/// 사슬 끝이 그대로면 앞 답을 씁니다(보낼 때마다, 기다리는 동안 다시 부름). 적용 잠금 없이 요약만 읽습니다.
pub(crate) fn removal_pending(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    let Some(state) = node.space_state(space) else {
        return false;
    };
    let sum = match summary(node, space) {
        None => return false,
        // 시작 직후 첫 적용 중: 빠질 기기를 셀 수 없으니 기다립니다(첫 적용이 끝나면 요약이 생김).
        Some(None) => return true,
        Some(Some(s)) => s,
    };
    let group = channel_group(node, space, &state, channel);
    let Some(kv) = &sum.kv else {
        return false;
    };
    if !kv.crew.debt.is_empty() || kv.crew.rekey_required {
        return true;
    }
    let Some(g) = group else {
        return false;
    };
    let Some(gs) = sum.groups.get(&g) else {
        return false;
    };
    if gs.tip_seq == 0 {
        // 아직 키가 없음: 기다릴 제거가 아니라 보내기가 바로 실패합니다.
        return false;
    }
    let Some(tip) = &gs.send_tip else {
        return true;
    };
    if !tip.members_known || kv.crew.last_rekey > tip.fresh_crew {
        return true;
    }
    let at = (tip.seq, tip.head);
    if let Some(m) = node.keygroup.pending_cache.lock().get(&(*space, *channel))
        && Arc::ptr_eq(&m.state, &state)
        && m.tip == at
    {
        return m.pending;
    }
    let mut seen = HashSet::new();
    let mut pending = false;
    for d in tip.members.iter() {
        let Some(u) = kv.owners.get(d) else {
            pending = true;
            break;
        };
        if seen.insert(*u)
            && !state
                .channel_permissions(u, channel)
                .contains(Permissions::VIEW_CHANNEL)
        {
            pending = true;
            break;
        }
    }
    node.keygroup.pending_cache.lock().insert(
        (*space, *channel),
        PendingMemo {
            state,
            tip: at,
            pending,
        },
    );
    pending
}

/// 보내기 전에 제거를 반영한 키 갱신을 기다립니다: `rotation_wait`까지(공간 설정 `strict_rotation`이면 끝없이). 기다림이
/// 끝나도 반영되지 않았으면 `key_rotation_pending`을 켜고 `true`(옛 키로 보냄, 나간 기기가 그 메시지를 읽을 수 있음).
/// 상태·키 사슬이 바뀔 때 깨어 다시 봅니다(놓친 알림에 대비해 1초마다도).
pub(crate) async fn wait_rotation(node: &Arc<Node>, space: SpaceId, channel: ChannelId) -> bool {
    if !removal_pending(node, &space, &channel) {
        node.keygroup.rotation_pending.lock().remove(&space);
        return false;
    }
    // 확정 기기가 나면 바로 돌게 합니다(보내려는 사람이 기다리는 중).
    if node
        .space_state(&space)
        .zip(node.me().ok())
        .is_some_and(|(st, me)| st.committer == me.device_id())
    {
        urgent(node, space);
    }
    let strict = node
        .space_state(&space)
        .is_some_and(|s| s.settings.strict_rotation);
    let deadline = tokio::time::Instant::now() + node.cfg.rotation_wait;
    loop {
        let moved = node.keygroup.moved.notified();
        tokio::pin!(moved);
        moved.as_mut().enable();
        if !removal_pending(node, &space, &channel) {
            node.keygroup.rotation_pending.lock().remove(&space);
            return false;
        }
        if node.is_shutting_down() {
            return false;
        }
        let now = tokio::time::Instant::now();
        if !strict && now >= deadline {
            break;
        }
        let mut wake = now + Duration::from_secs(1);
        if !strict {
            wake = wake.min(deadline);
        }
        tokio::select! {
            _ = &mut moved => {}
            _ = tokio::time::sleep_until(wake) => {}
        }
    }
    info!(space = %space.short(), "제거를 반영한 키 갱신을 기다리다 옛 키로 보냄");
    if node.keygroup.rotation_pending.lock().insert(space) {
        node.notify.publish(Notification::SpaceUpdated { space });
    }
    true
}

/// `space.get`의 `key_rotation_pending`: 옛 키로 보낸 뒤 아직 제거가 반영되지 않음.
pub(crate) fn key_rotation_pending(node: &Node, space: &SpaceId) -> bool {
    if !node.keygroup.rotation_pending.lock().contains(space) {
        return false;
    }
    let still = node
        .space_state(space)
        .is_some_and(|st| st.channels.keys().any(|c| removal_pending(node, space, c)));
    if !still {
        node.keygroup.rotation_pending.lock().remove(space);
    }
    still
}

// ───────────── 확정 기기 지정·표 넘기기 ─────────────

/// `crew.set_committer`: 소유자의 기기가 확정 기기를 정합니다(`CommitterChange`). 대상은 소유자·관리자의 활성 기기.
/// 옛 확정 기기는 상태가 바뀌면 표를 새 확정 기기에게 넘깁니다([`on_state_changed`]).
pub(crate) async fn set_committer(
    node: &Arc<Node>,
    space: SpaceId,
    device: DeviceId,
) -> Result<()> {
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let me = node.me()?;
    if state.owner != me.user() {
        return Err(Error::forbidden("소유자만 확정 기기를 정할 수 있습니다"));
    }
    let holder = node
        .sync
        .index
        .get(&space, &state)
        .device_user(&device)
        .ok_or_else(|| Error::not_found("이 공간의 활성 기기가 아닙니다"))?;
    if holder != state.owner
        && !state
            .base_permissions(&holder)
            .contains(Permissions::ADMINISTRATOR)
    {
        return Err(Error::invalid(
            "소유자·관리자의 기기만 확정 기기가 될 수 있습니다",
        ));
    }
    if state.committer == device {
        return Ok(());
    }
    crate::author::publish(
        node,
        space,
        None,
        &yangpa_core::body::EventBody::CommitterChange { device },
        crate::author::KeyChoice::State,
    )
    .await?;
    Ok(())
}

// ───────────── 확정 기기 표 넘기기 ─────────────

/// 옛 확정 기기: 표를 가졌는데 확정 기기가 바뀌었으면 새 확정 기기(와 예비 기기)에게 봉인해 넘깁니다. 다른 기기가 이미
/// 크루 사슬을 이어 냈으면 표가 낡았으므로 버립니다.
fn maybe_handover(
    node: &Arc<Node>,
    space: SpaceId,
    state: &SpaceState,
    kv: Option<&yangpa_sync::KeyView>,
) {
    let Some(kv) = kv else { return };
    let Some(table) = committer::load_table(node, &space) else {
        return;
    };
    if table.crew.seq() != kv.crew.seq || *table.crew.root_commit() != kv.crew.root_commit {
        info!(space = %space.short(), "다른 확정 기기가 이어 내 낡은 키 그룹 표를 버림");
        committer::drop_table(node, &space);
        return;
    }
    let mut to = vec![state.committer];
    if let Some(b) = kv.crew.backup
        && b != state.committer
    {
        to.push(b);
    }
    let key = (space, state.committer, kv.crew.seq);
    if !node.keygroup.handed.lock().insert(key) {
        return;
    }
    let target = state.committer;
    node.spawn_detached(move |node| async move {
        // 확정 기기 변경을 낸 직후라 새 확정 기기가 아직 그 변경을 받지 못했으면 거절합니다(받는 쪽은 그때 나와 따라잡기를
        // 돌림). 그 기기가 공개 끝으로 이어받기 전(`key_handover_wait`) 안에서 간격을 늘려 가며 다시 보냅니다.
        let deadline = Instant::now() + node.cfg.key_handover_wait;
        let mut delay = Duration::from_millis(250);
        loop {
            match send_handover(&node, space, &table, &to).await {
                Ok(()) => {
                    info!(space = %space.short(), "새 확정 기기에게 키 그룹 표를 넘김");
                    // 넘겼으면 이 기기는 표를 들고 있을 까닭이 없습니다(넘긴 기기는 보유 기기로 남아 빼려면 전체 재키).
                    committer::drop_table(&node, &space);
                    return;
                }
                Err(e) if Instant::now() + delay < deadline && handover_still_due(&node, &space, &table, &target) => {
                    debug!(space = %space.short(), error = %e, "키 그룹 표 넘기기 실패(곧 다시)");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(4));
                }
                Err(e) => {
                    debug!(space = %space.short(), error = %e, "키 그룹 표 넘기기 실패(다음에 다시)");
                    node.keygroup.handed.lock().remove(&key);
                    return;
                }
            }
        }
    });
}

/// 넘기기를 다시 해 볼 만한지: 확정 기기가 그대로이고 표가 아직 크루 사슬 끝과 맞음(다른 기기가 이어 냈으면 다음 상태
/// 변화에서 표를 버림).
fn handover_still_due(
    node: &Node,
    space: &SpaceId,
    table: &committer::Table,
    target: &DeviceId,
) -> bool {
    if node
        .space_state(space)
        .is_none_or(|st| st.committer != *target)
    {
        return false;
    }
    // 적용 잠금 없이 부르는 재시도 고리라 요약만 읽습니다.
    let Some(Some(sum)) = summary(node, space) else {
        return false;
    };
    sum.kv.as_ref().is_some_and(|kv| {
        table.crew.seq() == kv.crew.seq && *table.crew.root_commit() == kv.crew.root_commit
    })
}

/// 확정 기기가 크루 키 그룹 밖이면(표도 크루 키도 없이 확정 기기가 된 기기: 옛 빌드의 복구 기기 등) 그 기기는 상태 이벤트를 열 수 없어 상태를 세우지도, 공개 끝으로 이어받아 전체 재키하지도 못합니다. 크루
/// 멤버 기기가 지난 상태 키와 지금 크루 R을 그 사용자의 받은편지함에 봉인해 넘깁니다(확정 기기 인계: 해소기 검사가
/// 멤버 누구에게나 허용하고, 받는 쪽은 저장만 하고 지금 키로 쓰지 않음). 확정 기기는 소유자·관리자의 기기라 이미 모든
/// 키를 받을 자격이 있습니다. 크루 사슬 끝의 가장 작은 활성 기기가 바로 하고, 다른 기기는 `key_handover_wait`가 지나도
/// 그대로면(주기 점검에서) 합니다. (공간, 확정 기기, 크루 순번)마다 한 번.
fn maybe_member_handover(
    node: &Arc<Node>,
    space: SpaceId,
    state: &Arc<SpaceState>,
    kv: Option<&yangpa_sync::KeyView>,
) {
    let Some(kv) = kv else { return };
    let Ok(me) = node.me() else { return };
    let committer = state.committer;
    if kv.crew.seq == 0
        || committer == me.device_id()
        || kv.crew.members.contains(&committer)
        || !kv.crew.members.contains(&me.device_id())
        || !state.device_belongs(&me.user(), &me.device_id())
    {
        return;
    }
    let index = node.sync.index.get(&space, state);
    let Some(c_user) = index.device_user(&committer) else {
        return;
    };
    let first = kv
        .crew
        .members
        .iter()
        .filter(|d| **d != committer && index.device_user(d).is_some())
        .min()
        .copied();
    let key = (space, committer, kv.crew.seq);
    {
        let mut seen = node.keygroup.member_handed.lock();
        // 이 공간의 다른 확정 기기·옛 순번 기록은 버립니다(상한 없이 쌓이지 않게).
        seen.retain(|k, _| k.0 != space || (k.1 == committer && k.2 == kv.crew.seq));
        let entry = seen.entry(key).or_insert((Instant::now(), false));
        if entry.1 {
            return;
        }
        if first != Some(me.device_id()) && entry.0.elapsed() < node.cfg.key_handover_wait {
            return;
        }
        entry.1 = true;
    }
    node.spawn_detached(move |node| async move {
        match hand_state_keys(&node, space, c_user).await {
            Ok(()) => {}
            Err(e) => {
                debug!(space = %space.short(), error = %e, "확정 기기에게 상태 키 넘기기 실패(다음에 다시)");
                if let Some(entry) = node.keygroup.member_handed.lock().get_mut(&key) {
                    entry.1 = false;
                }
            }
        }
    });
}

/// 상태 키(가입자에게 주는 만큼)와 지금 크루 R을 `user`의 활성 기기 모두에게 봉인해 받은편지함에 냅니다.
async fn hand_state_keys(node: &Arc<Node>, space: SpaceId, user: UserId) -> Result<()> {
    let l = ingest::lock_space(node, space).await;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let rcpts = crate::devkeys::user_sealing_keys(node, &state, &user);
    if rcpts.is_empty() {
        return Err(Error::not_found("받을 기기의 상자 키를 모름"));
    }
    let core = node
        .spaces
        .read()
        .get(&space)
        .map(|rt| rt.core.clone())
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let (r, base) = {
        let mut guard = core.lock();
        let c = &mut *guard;
        c.keys.ensure_loaded(node, &space);
        let r = c
            .keys
            .crew_current(&space, &c.resolver)
            .map(|k| k.r().clone());
        (r, c.resolver.base())
    };
    let crew = KeyGroupId::crew(&space);
    let mut shared = crate::keys::keys_for_joiner(node, &space, base.as_ref())?;
    if let Some(r) = r {
        shared.push(SharedKey::Root {
            group: crew,
            id: yangpa_crypto::keysched::group_root_id(&space, &crew, &r),
            key: r,
        });
    }
    if shared.is_empty() {
        return Ok(());
    }
    let hint: Vec<EventId> = crate::crews::inbox_relays(node, &space, &user)
        .first()
        .map(|r| r.event)
        .into_iter()
        .collect();
    let mut out = Vec::new();
    for chunk in crate::keys::chunk_shares(shared) {
        let body = EventBody::KeyShare(KeyShareBody {
            keys: chunk,
            for_user: Some(user),
        });
        out.push(crate::author::publish_scoped_locked(
            node,
            &l,
            space,
            Scope::Inbox(user),
            &body,
            crate::author::KeyChoice::Sealed(rcpts.clone()),
            &hint,
        )?);
    }
    drop(l);
    info!(space = %space.short(), shares = out.len(), "크루 키 그룹 밖의 확정 기기에게 상태 키를 넘김");
    sync_service::propagate(node, space, out);
    Ok(())
}

/// 봉인한 넘기기 요청 하나(크루 또는 그룹 하나). 메시지 상한을 넘으면 `None`.
fn seal_part(
    space: SpaceId,
    trees: &[&yangpa_crypto::keytree::KeyTree],
    covers: &[&yangpa_crypto::keytree::CoverGroup],
    rcpts: &[(DeviceId, yangpa_core::device::BoxPublicKey)],
    signer: &yangpa_core::sign::SigningKeypair,
    meta: Vec<u8>,
) -> Result<Option<YpRequest>> {
    let sealed = seal_handover(trees, covers, rcpts, signer)
        .map_err(|e| Error::internal(format!("표 봉인: {e}")))?;
    if sealed.ciphertext.len() + sealed.envelope.len() + meta.len() + 64 * 1024
        > yangpa_core::wire::MAX_WIRE_MESSAGE
    {
        return Ok(None);
    }
    Ok(Some(YpRequest::KeyTableHandover(Box::new(
        KeyTableHandover {
            space,
            envelope: sealed.envelope,
            manifest: sealed.manifest,
            ciphertext: sealed.ciphertext,
            sig: sealed.sig,
            meta,
        },
    ))))
}

/// 표를 크루(메타데이터와 함께) 먼저, 그다음 그룹마다 한 요청씩 넘깁니다. 4.5만 대 크루에서 표 전체는 메시지 상한
/// (8MiB)을 넘지만 그룹 하나는 넘지 않습니다. 그래도 넘는 그룹은 보내지 않고, 새 확정 기기가 그 사슬 끝에서 이어받아
/// 새 T를 냅니다.
async fn send_handover(
    node: &Arc<Node>,
    space: SpaceId,
    table: &committer::Table,
    to: &[DeviceId],
) -> Result<()> {
    let me = node.me()?;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let index = node.sync.index.get(&space, &state);
    let mut rcpts = Vec::new();
    for d in to {
        let Some(u) = index.device_user(d) else {
            continue;
        };
        if let Some(k) = crate::devkeys::device_box_key(node, &state, &u, d) {
            rcpts.push((*d, k));
        }
    }
    if rcpts.is_empty() {
        return Err(Error::not_found("받을 기기의 상자 키를 모름"));
    }
    let signer = &me.device.signing;
    let meta = committer::meta_bytes(node, &space);
    let crew = seal_part(space, &[&table.crew], &[], &rcpts, signer, meta)?.ok_or_else(|| {
        Error::invalid("크루 키 표가 한 번에 보내기에 너무 큼(새 확정 기기가 전체 재키)")
    })?;
    let mut groups = Vec::new();
    for t in table.privates.values() {
        match seal_part(space, &[&**t], &[], &rcpts, signer, Vec::new())? {
            Some(r) => groups.push(r),
            None => {
                debug!(space = %space.short(), "그룹 표가 너무 커 넘기지 않음(새 확정 기기가 새 T)")
            }
        }
    }
    for c in table.covers.values() {
        match seal_part(space, &[], &[&**c], &rcpts, signer, Vec::new())? {
            Some(r) => groups.push(r),
            None => {
                debug!(space = %space.short(), "그룹 표가 너무 커 넘기지 않음(새 확정 기기가 새 T)")
            }
        }
    }
    let mut sent = false;
    for (d, _) in &rcpts {
        let Some(peer) = peers::peer_for_device(node, d) else {
            continue;
        };
        if peers::ensure_auth(node, peer).await.is_err() {
            continue;
        }
        match crate::identity::request_after_auth(node, peer, crew.clone()).await {
            Ok(YpResponse::Ok) => sent = true,
            Ok(_) => {
                debug!("표 넘기기를 받지 않음");
                continue;
            }
            Err(e) => {
                debug!(error = %e, "표 넘기기 요청 실패");
                continue;
            }
        }
        for g in &groups {
            match crate::identity::request_after_auth(node, peer, g.clone()).await {
                Ok(YpResponse::Ok) => {}
                Ok(_) => debug!("그룹 표 넘기기를 받지 않음"),
                Err(e) => debug!(error = %e, "그룹 표 넘기기 요청 실패"),
            }
        }
    }
    if sent {
        Ok(())
    } else {
        Err(Error::Network("새 확정 기기에 닿지 못함".into()))
    }
}

/// 들어온 `KeyTableHandover`: 보낸 기기가 지금도 표를 가질 자격이 있고(소유자·관리자의 활성 기기이며 해소기의 표 보유
/// 기기), 크루 전체 재키가 필요하지 않고, 내가 지금 확정 기기나 예비 기기일 때만 엽니다. 자격을 잃은 보유 기기(강등된
/// 관리자)의 표는 받지 않습니다: 그 기기를 빼는 길은 공개 끝에서 이어받는 전체 재키입니다. 서명·표 자체 검사
/// (`open_handover`)를 거친 뒤, 크루 요청이면 크루 사슬 끝이 내가 아는 것과 같아야 쓰고(이미 맞는 표가 있으면 그대로),
/// 그룹 요청이면 지금 표의 크루와 맞춰 본 뒤 합칩니다. 넘겨받은 그룹의 판 정보는 믿지 않고, 크루 전체 재키 뒤에 머문
/// 그룹은 바퀴가 공개 사슬(`fresh_crew`)로 찾아 새 T를 냅니다.
pub(crate) async fn receive_handover(
    node: &Arc<Node>,
    peer: PeerId,
    h: KeyTableHandover,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let space = h.space;
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if crate::checkpoint::is_unconfirmed(node, &space) {
        return YpResponse::error(WireErrorCode::NotFound, "아직 확인하는 중인 상태");
    }
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let Some(core) = node.spaces.read().get(&space).map(|rt| rt.core.clone()) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    // 표를 열기 전 자격 검사는 요약으로(적용 잠금 없이 `SpaceCore`를 잡지 않음). 합칠 때 잠금 안에서 다시 봅니다.
    let kv = match summary(node, &space) {
        Some(Some(s)) => s.kv.clone(),
        _ => {
            return YpResponse::Busy {
                retry_after_ms: 1000,
            };
        }
    };
    let Some(kv) = kv else {
        return YpResponse::error(WireErrorCode::NotFound, "아직 키 사슬이 없음");
    };
    if !holder_ok(&state, &kv, &user, &device) {
        return YpResponse::error(WireErrorCode::Forbidden, "표 보유 기기가 아님");
    }
    if kv.crew.rekey_required {
        return YpResponse::error(WireErrorCode::Forbidden, "크루 전체 재키가 먼저");
    }
    let target = state.committer == me.device_id() || kv.crew.backup == Some(me.device_id());
    if !target {
        // 보유 기기가 넘기려 한다면 확정 기기 변경을 내가 아직 못 받았을 수 있습니다: 그 기기와 따라잡아 두면 보낸
        // 쪽이 곧 다시 보낼 때 받습니다(보낸 쪽은 표 보유 기기라 따라잡기를 남용할 수 있는 상대가 좁음).
        sync_service::spawn_catch_up(node, space, peer);
        return YpResponse::error(WireErrorCode::Forbidden, "확정 기기·예비 기기가 아님");
    }
    let meta = h.meta;
    let sealed = SealedHandover {
        envelope: h.envelope,
        manifest: h.manifest,
        ciphertext: h.ciphertext,
        sig: h.sig,
    };
    let opened = open_handover(&me.device_id(), &me.device.boxkey, &sealed, &device);
    drop(sealed);
    let (trees, covers) = match opened {
        Ok(x) => x,
        Err(e) => {
            debug!(space = %space.short(), reason = %e, "넘겨받은 키 그룹 표를 열지 못함");
            return YpResponse::error(WireErrorCode::BadRequest, "표를 열 수 없음");
        }
    };
    let has_crew = trees
        .iter()
        .any(|t| t.kind() == yangpa_crypto::GroupKind::Crew);
    let l = ingest::lock_space(node, space).await;
    let kv = core.lock().resolver.key_view();
    if !has_crew {
        let merged = committer::merge_groups(node, &space, trees, covers);
        drop(l);
        return match merged {
            Ok(n) => {
                if n > 0 && state.committer == me.device_id() {
                    request_round(node, space);
                }
                YpResponse::Ok
            }
            Err(why) => YpResponse::error(WireErrorCode::BadRequest, why),
        };
    }
    let Some(table) = committer::Table::from_parts(&space, trees, covers) else {
        return YpResponse::error(WireErrorCode::BadRequest, "다른 공간의 표");
    };
    let on_chain = |t: &committer::Table| {
        kv.as_ref().is_some_and(|kv| {
            t.crew.seq() == kv.crew.seq && *t.crew.root_commit() == kv.crew.root_commit
        })
    };
    if !on_chain(&table) {
        return YpResponse::error(WireErrorCode::BadRequest, "내 크루 사슬 끝과 다른 표");
    }
    if committer::load_table(node, &space).is_some_and(|t| on_chain(&t)) {
        // 이미 사슬 끝과 맞는 표가 있으면 덮어쓰지 않습니다(같은 넘기기를 다시 받았거나 내가 이어 낸 표).
        drop(l);
        return YpResponse::Ok;
    }
    committer::save_table(node, &space, &table);
    committer::adopt_meta(node, &space, &meta);
    node.keygroup.received.lock().insert(space, Instant::now());
    drop(l);
    info!(space = %space.short(), "키 그룹 표를 넘겨받음");
    // 표에는 내가 아직 사슬을 받지 못한 그룹(비공개 그룹 밖에서 확정 기기가 된 관리자 등)이 있을 수 있습니다. 넘겨준
    // 기기와 따라잡아 그 그룹 스트림을 받아 둡니다(사슬 끝을 모르고 낸 그룹 갱신은 거부됨).
    sync_service::spawn_catch_up(node, space, peer);
    node.keygroup
        .committer_since
        .lock()
        .retain(|(s, _), _| *s != space);
    if state.committer == me.device_id() {
        request_round(node, space);
    }
    YpResponse::Ok
}

/// 표를 넘길·가질 자격: 해소기의 표 보유 기기이면서 지금 상태에서 소유자·관리자의 활성 기기.
fn holder_ok(
    state: &SpaceState,
    kv: &yangpa_sync::KeyView,
    user: &UserId,
    device: &DeviceId,
) -> bool {
    kv.crew.holders.contains(device)
        && state.device_belongs(user, device)
        && (*user == state.owner
            || state
                .base_permissions(user)
                .contains(Permissions::ADMINISTRATOR))
}
