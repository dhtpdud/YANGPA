//! 공간 런타임: 해소기·상태·키 대기 목록, 시작 때 다시 세우기, 새 공간 만들기.
//!
//! `Node::spaces`의 [`SpaceRuntime`]은 읽기용 상태 사본(`state`)과 적용 경로만 쓰는 [`SpaceCore`]로 나뉩니다.
//! `SpaceCore`는 그 공간의 적용 잠금(`ingest::lock_space`, 또는 모든 공간의 `ingest::lock`)을 쥔 쪽만 만집니다.
//! 적용 잠금 없이 읽어야 하는 것(따라잡기가 보는 부분 전달 기억)은 잎 잠금인 `SpaceRuntime::scoped_seen`에 둡니다.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use parking_lot::Mutex;
use yangpa_core::body::{EventBody, KeyShareBody, Secret32, SharedKey, state_key_id};
use yangpa_core::device::{BoxPublicKey, SignedDeviceList};
use yangpa_core::event::{EventHeader, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, StateKeyId};
use yangpa_core::model::{
    ChannelSettings, Member, Permissions, SpaceKind, SpaceSettings, SpaceState,
};
use yangpa_core::time::Timestamp;
use yangpa_sync::Resolver;

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::{self, IngestReport, Locked, Need, Source};
use crate::keys;
use crate::node::Node;
use crate::sync_service;

/// `Node::spaces`의 한 칸.
pub(crate) struct SpaceRuntime {
    /// 해소된 상태 사본(API·동기화가 읽음). 해소 전이면 `None`.
    pub(crate) state: Option<Arc<SpaceState>>,
    pub(crate) genesis: Option<EventId>,
    /// 시작 이벤트의 논리 시계(처음 멤버 판단용).
    pub(crate) genesis_lamport: Option<u64>,
    /// 멤버가 (마지막으로) 들어온 논리 시계. 시더의 과거 기록 공개 검사에 씁니다(해소기의 지속 맵 그대로).
    pub(crate) member_since: yangpa_core::model::SharedMap<yangpa_core::ids::UserId, u64>,
    pub(crate) dormant: bool,
    pub(crate) core: Arc<Mutex<SpaceCore>>,
    /// `core`의 부분 전달 대기열이 따라잡기에 보여 주는 기억(잎 잠금). 따라잡기는 적용 잠금 없이 이것만 읽습니다.
    pub(crate) scoped_seen: Arc<Mutex<crate::scoped::ScopedSeen>>,
    /// 적용이 끝날 때마다 새로 쓰는 키 그룹 요약(잎 잠금, 첫 적용 전에는 `None`). 보내는 쪽 규칙·`space.get`은 적용
    /// 잠금 없이 이것만 읽습니다.
    pub(crate) key_summary: Arc<Mutex<Option<Arc<crate::keygroup::KeySummary>>>>,
}

/// 상태가 생기기 전에 받은 키 묶음(공간을 만든 기기가 봉인한 첫 상태 키). 상태가 생기면 권한을 확인해 저장. 내 앞으로
/// 봉인했는데 지금 상태로는 작성자를 모르는 묶음(상태를 아직 다 받지 못함: 확정 기기 인계 등)도 상태가 바뀔 때까지 둡니다.
pub(crate) struct Provisional {
    pub(crate) id: EventId,
    pub(crate) header: EventHeader,
    pub(crate) body: KeyShareBody,
}

/// RAM에 두는 임시 키 묶음 상한(넘으면 오래된 것부터 버림).
pub(crate) const MAX_PROVISIONAL: usize = 256;

/// 적용 경로 전용 상태.
#[derive(Default)]
pub(crate) struct SpaceCore {
    pub(crate) resolver: Resolver,
    /// 키 그룹 멤버 상태(크루·비공개·덮개 그룹의 내 키와 그룹 사슬).
    pub(crate) keys: crate::keygroup::MemberState,
    pub(crate) provisional: Vec<Provisional>,
    /// 키·대상·상태를 기다리는 이벤트(디스크에 저장됨). 목록은 설정 `space.wait.<공간>`에도 둡니다.
    pub(crate) waiting: HashMap<Need, BTreeSet<EventId>>,
    /// 저장할 수 없어 RAM에 보류한 이벤트(채널·작성자를 아직 모름, 논리 시계 부풀림).
    pub(crate) held: Vec<SignedEvent>,
    /// `held`의 대략적인 바이트 수(상한 검사용).
    pub(crate) held_bytes: usize,
    /// 저장소에 마지막으로 쓴 상태(바뀐 행만 쓰는 기준, `ingest::apply_state`). 시작할 때 저장소에서 읽은 판이고,
    /// 없거나 읽지 못했으면 다음 적용이 상태 행을 통째로 다시 씁니다.
    pub(crate) persisted: Option<Arc<SpaceState>>,
    /// 이 런타임이 상태를 저장한 횟수(통째로·바뀐 행만)와 쓴 행 수. 바뀐 행만 쓰는 길을 타는지 시험이 봅니다.
    pub(crate) state_writes: StateWrites,
    /// 기준점 상태로 권한을 확인하기 전의 부분 전달 이벤트(받은편지함·키 그룹)와 그 기억(RAM 전용).
    pub(crate) scoped: crate::scoped::ScopedQueue,
}

/// [`SpaceCore::state_writes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateWrites {
    /// 상태 행을 통째로 다시 쓴 횟수(기준이 없을 때).
    pub full: u32,
    /// 바뀐 행만 쓴 횟수(쓸 행이 없던 적용은 세지 않음).
    pub deltas: u32,
    /// 바뀐 행만 쓸 때 쓰거나 지운 행 수의 합.
    pub rows: usize,
}

impl SpaceCore {
    /// 아직 저장하지 않은 임시 상태 키(ID가 키와 맞는 것만: 남이 진짜 ID 아래 엉뚱한 키를 끼워 넣지 못하게).
    pub(crate) fn provisional_state_key(&self, id: &StateKeyId) -> Option<Secret32> {
        self.provisional.iter().find_map(|p| {
            p.body.keys.iter().find_map(|k| match k {
                SharedKey::State { id: kid, key } if kid == id && k.bound_to(&p.header.space) => {
                    Some(key.clone())
                }
                _ => None,
            })
        })
    }
}

fn genesis_setting(space: &SpaceId) -> String {
    format!("space.genesis.{space}")
}

pub(crate) fn wait_setting(space: &SpaceId) -> String {
    format!("space.wait.{space}")
}

/// 고정한 시작 이벤트(공간 생성). 재시작 뒤에도 같은 소유자를 믿기 위해 저장합니다.
pub(crate) fn load_genesis(node: &Node, space: &SpaceId) -> Result<Option<EventId>> {
    Ok(node.store.get_setting(&genesis_setting(space))?)
}

pub(crate) fn save_genesis(node: &Node, space: &SpaceId, genesis: &EventId) -> Result<()> {
    if load_genesis(node, space)?.is_none() {
        node.store.put_setting(&genesis_setting(space), genesis)?;
    }
    Ok(())
}

/// 공간 런타임이 없으면 만듭니다. `genesis`를 주면(친구 수락·초대로 받은 공간) 그 시작 이벤트로 고정합니다.
pub(crate) fn ensure_runtime(
    node: &Node,
    space: &SpaceId,
    genesis: Option<EventId>,
) -> Result<Arc<Mutex<SpaceCore>>> {
    if let Some(rt) = node.spaces.read().get(space) {
        return Ok(rt.core.clone());
    }
    node.store.ensure_space(space, Timestamp::now())?;
    let genesis = match load_genesis(node, space)? {
        Some(g) => Some(g),
        None => {
            if let Some(g) = &genesis {
                save_genesis(node, space, g)?;
            }
            genesis
        }
    };
    let dormant = node
        .store
        .get_space(space)?
        .map(|r| r.dormant)
        .unwrap_or(false);
    // 저장된 상태 행을 기준으로 읽어 둡니다: 다시 해소한 상태와 같으면 시작 때 행을 다시 쓰지 않습니다.
    let persisted = match node.store.load_space_state(space) {
        Ok(s) => s.map(Arc::new),
        Err(e) => {
            tracing::warn!(error = %e, space = %space.short(), "저장된 공간 상태를 읽지 못해 다시 씀");
            None
        }
    };
    let core = SpaceCore {
        resolver: genesis.map(Resolver::with_genesis).unwrap_or_default(),
        persisted,
        ..Default::default()
    };
    let scoped_seen = core.scoped.seen();
    let core = Arc::new(Mutex::new(core));
    let mut spaces = node.spaces.write();
    let rt = spaces.entry(*space).or_insert_with(|| SpaceRuntime {
        state: None,
        genesis,
        genesis_lamport: None,
        member_since: Default::default(),
        dormant,
        core: core.clone(),
        scoped_seen,
        key_summary: Default::default(),
    });
    Ok(rt.core.clone())
}

/// 저장된 공간을 모두 다시 세웁니다(시작 때). 상태·키 갱신 레인과 공간 스트림 키 묶음을 다시 적용하고, 키 그룹
/// 스트림의 갱신으로 그룹 사슬(RAM 전용)을 다시 세운 뒤 키를 기다리던 이벤트를 다시 봅니다. 알림은 내지 않습니다.
pub(crate) async fn load_all(node: &Arc<Node>) -> Result<()> {
    if node.me.read().is_none() {
        return Ok(());
    }
    for space in node.store.space_ids()? {
        let core = ensure_runtime(node, &space, None)?;
        // 저장된 체크포인트가 있으면 스냅숏으로 해소기를 세우고 경계 위 이벤트만 다시 넣습니다(차가운 시작).
        let floor = match load_genesis(node, &space)? {
            Some(g) => match crate::checkpoint::restore(node, &space, g) {
                Some((r, f)) => {
                    core.lock().resolver = r;
                    f
                }
                None => 0,
            },
            None => 0,
        };
        let evs: Vec<SignedEvent> = node
            .store
            .events_by_lane_from(&space, &[Lane::State, Lane::KeyAgreement, Lane::Key], floor)?
            .into_iter()
            .filter(|e| e.header.stream() == StreamId::Space)
            .collect();
        let groups: Vec<SignedEvent> = node
            .store
            .events_by_lane(&space, &[Lane::KeyAgreement])?
            .into_iter()
            .filter(|e| matches!(e.header.stream(), StreamId::KeyGroup(_)))
            .collect();
        let waiting: Vec<EventId> = node
            .store
            .get_setting(&wait_setting(&space))?
            .unwrap_or_default();
        let waiting = node.store.get_events(&waiting)?;
        let l = ingest::lock_space(node, space).await;
        let r1 = ingest::ingest_stored_locked(node, &l, space, evs, Source::Replay)?;
        ingest::replay_key_groups_locked(node, &l, space, groups)?;
        let r2 = ingest::ingest_stored_locked(node, &l, space, waiting, Source::Replay)?;
        drop(l);
        tracing::debug!(
            space = %space.short(),
            state = r1.accepted.len(),
            waiting = r2.waiting,
            "공간 다시 세움"
        );
        sync_service::refresh_space_peers(node, &space);
        ingest::after_unlock(node, space, &r1);
    }
    Ok(())
}

/// 공간을 이 기기에서 모두 지웁니다(검사에 실패했거나 상대가 받지 않은 친구 공간 정리). 런타임, 이벤트·키·메시지
/// (`Store::purge_space`), 시작 고정·대기 목록·지금 상태 키 설정, 이 공간의 키 그룹 상태·표, gossip 구독을 지웁니다.
/// 적용 잠금 안에서 부릅니다(적용 중인 공간을 지우지 않게).
pub(crate) fn discard_space_locked(node: &Node, l: &Locked<'_>, space: &SpaceId) -> Result<()> {
    l.check(space)?;
    node.spaces.write().remove(space);
    crate::checkpoint::forget_space(node, space);
    node.store.purge_space(space)?;
    node.store.delete_setting(&genesis_setting(space))?;
    node.store.delete_setting(&wait_setting(space))?;
    keys::forget_space(node, space)?;
    crate::moderation::forget_space(node, space);
    crate::ratelimit::forget_space(node, space);
    node.sync.index.invalidate(space);
    crate::keygroup::forget_space(node, space);
    node.net
        .unsubscribe(yangpa_core::wire::GossipTopic::Crew(*space));
    crate::topics::forget(node, space);
    crate::chantopics::forget(node, space);
    crate::chantopics::forget_follows(node, space);
    tracing::debug!(space = %space.short(), "공간 지움");
    Ok(())
}

/// 새 공간의 처음 구성.
pub(crate) struct NewSpace {
    pub kind: SpaceKind,
    pub settings: SpaceSettings,
    pub everyone: Permissions,
    /// 처음 멤버(나 포함).
    pub members: Vec<Member>,
    /// 처음 멤버들의 서명된 기기 목록(나 포함).
    pub devices: Vec<SignedDeviceList>,
    /// 처음 채널 이름(없으면 채널 없이).
    pub first_channel: Option<String>,
}

/// 새 공간을 만듭니다: 첫 상태 키 봉인 배포 → `SpaceCreate` → 첫 채널 → 첫 크루 키 갱신(처음 기기 모두). 만든 이벤트를
/// 순서대로 돌려줍니다(전파와 [`ingest::after_unlock`]은 부르는 쪽, 잠금은 돌아올 때 풀림). 새 ID라 그 공간 잠금만
/// 쥡니다: 모든 공간 잠금(관문 쓰기)을 기다리면 큰 크루의 긴 적용 뒤에 줄을 서고, 그동안 다른 공간도 모두 막힙니다.
pub(crate) async fn create_space(
    node: &Arc<Node>,
    spec: NewSpace,
) -> Result<(SpaceId, Vec<SignedEvent>, IngestReport)> {
    let space = SpaceId::random();
    let l = ingest::lock_space(node, space).await;
    create_space_with(node, &l, space, spec)
}

/// [`create_space`]를 이미 쥔 잠금 안에서.
pub(crate) fn create_space_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    spec: NewSpace,
) -> Result<(SpaceId, Vec<SignedEvent>, IngestReport)> {
    create_space_with(node, l, SpaceId::random(), spec)
}

/// 중간에 실패하면 만들던 공간을 지웁니다.
fn create_space_with(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    spec: NewSpace,
) -> Result<(SpaceId, Vec<SignedEvent>, IngestReport)> {
    l.check(&space)?;
    ensure_runtime(node, &space, None)?;
    match build_space_locked(node, l, space, spec) {
        Ok((out, report)) => Ok((space, out, report)),
        Err(e) => {
            if let Err(e2) = discard_space_locked(node, l, &space) {
                tracing::warn!(error = %e2, "만들다 실패한 공간 정리 실패");
            }
            Err(e)
        }
    }
}

fn build_space_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    spec: NewSpace,
) -> Result<(Vec<SignedEvent>, IngestReport)> {
    let me = node.me()?;
    let mut out = Vec::new();
    let mut report = IngestReport::default();

    // 1. 첫 상태 키를 처음 멤버들의 모든 기기에게 봉인해 먼저 발행(protocol.md "이벤트 본문 암호화").
    let k0 = Secret32::random();
    let k0_id = state_key_id(&space, &k0);
    let rcpts: Vec<(DeviceId, BoxPublicKey)> = spec
        .devices
        .iter()
        .flat_map(|l| l.list.active().map(|d| (d.device, d.box_key.clone())))
        .collect();
    if !rcpts.iter().any(|(d, _)| *d == me.device_id()) {
        return Err(Error::invalid("내 기기가 처음 기기 목록에 없습니다"));
    }
    let share = EventBody::KeyShare(KeyShareBody {
        keys: vec![SharedKey::State {
            id: k0_id,
            key: k0.clone(),
        }],
        for_user: None,
    });
    let (ev, r) =
        author::publish_locked(node, l, space, None, &share, KeyChoice::Sealed(rcpts), &[])?;
    out.push(ev);
    report.merge(r);

    // 2. 공간 생성.
    let create = EventBody::SpaceCreate {
        kind: spec.kind,
        settings: spec.settings,
        owner: me.user(),
        committer: me.device_id(),
        everyone: spec.everyone,
        members: spec.members,
        devices: spec.devices,
    };
    let (ev, r) = author::publish_locked(
        node,
        l,
        space,
        None,
        &create,
        KeyChoice::StateWith(k0_id, k0),
        &[],
    )?;
    out.push(ev);
    report.merge(r);

    // 3. 첫 채널.
    if let Some(name) = spec.first_channel {
        let body = EventBody::ChannelCreate {
            channel: ChannelId::random(),
            settings: ChannelSettings::text(name),
        };
        let (ev, r) = author::publish_locked(node, l, space, None, &body, KeyChoice::State, &[])?;
        out.push(ev);
        report.merge(r);
    }

    // 4. 첫 크루 키 갱신: 확정 기기(나) 바퀴를 바로 한 번 돌려 처음 기기 모두에게 크루 키를 줍니다(묶음 창 없이).
    let r = crate::keygroup::committer::round_locked(node, l, space)?;
    out.extend(r.events);
    Ok((out, report))
}

/// 공간 이름(1:1 공간이면 상대 이름).
pub(crate) fn display_name(node: &Node, space: &SpaceId, state: &SpaceState) -> String {
    if state.kind == SpaceKind::Space
        && let Ok(me) = node.me()
        && let Some(other) = state.members.keys().find(|u| **u != me.user())
    {
        let _ = space;
        return crate::identity::display_name(node, other);
    }
    state.settings.name.clone()
}
