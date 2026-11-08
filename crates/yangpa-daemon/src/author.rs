//! 이벤트 만들기: 부모·논리 시계·키 고르기·암호화·서명, 그리고 받는 것과 같은 적용 경로로 넣기.
//!
//! 다른 영역이 상태 이벤트(크루·채널·역할 …)를 낼 때도 이 모듈을 씁니다:
//!
//! ```ignore
//! let l = ingest::lock_space(node, space).await;
//! let (ev, _) = author::publish_locked(node, &l, space, None, &body, KeyChoice::State, &[])?;
//! drop(l);
//! sync_service::propagate(node, space, vec![ev]);
//! ```
//!
//! 잠금 없이 하나만 낼 때는 [`publish`]가 위 과정을 한 번에 합니다.

use std::sync::Arc;

use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::device::BoxPublicKey;
use yangpa_core::event::{
    EVENT_VERSION, EventHeader, KeyRef, Lane, MAX_STATE_ANCHOR, Scope, SignedEvent, StreamId,
};
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, EventId, SpaceId, StateKeyId};
use yangpa_core::time::Timestamp;
use yangpa_crypto::{encrypt_body, seal_body};
use yangpa_sync::AnchorPlace;
use yangpa_sync::dag::{choose_parents, next_lamport};

use crate::error::{Error, Result};
use crate::ingest::{self, IngestReport, Locked, Source};
use crate::keys;
use crate::node::Node;
use crate::sync_service;

/// 본문을 어떤 키로 암호화할지.
#[derive(Clone)]
pub(crate) enum KeyChoice {
    /// 공간의 지금 상태 키(상태 레인).
    State,
    /// 지정한 상태 키(공간을 만들 때 첫 키, 키 그룹 갱신, 그룹 상태 키로 봉인하는 채널 키 묶음).
    StateWith(StateKeyId, Secret32),
    /// 지정한 채널 뿌리(메시지). 본문은 그 뿌리의 기록 키로 암호화합니다.
    HistoryWith(ChannelRootId, Secret32),
    /// 휘발성 채널의 세션 키(세대, 키). 이 키로 쓴 이벤트는 적용 경로가 저장하지 않고 `volatile`로 넘깁니다(B2).
    Volatile(u64, Secret32),
    /// 기기들에게 봉인(키 묶음).
    Sealed(Vec<(DeviceId, BoxPublicKey)>),
}

/// 이벤트 하나를 만들어 서명합니다(적용하지 않음). `extra_parents`는 같은 잠금 안에서 방금 만든,
/// 아직 머리에 없는 이벤트(예: 같은 변동의 키 갱신 조각)를 부모로 더할 때 씁니다.
pub(crate) fn author_event(
    node: &Node,
    space: &SpaceId,
    channel: Option<ChannelId>,
    body: &EventBody,
    key: &KeyChoice,
    extra_parents: &[(EventId, u64)],
) -> Result<SignedEvent> {
    author_in(
        node,
        space,
        channel,
        None,
        body,
        key,
        extra_parents,
        &[],
        None,
    )
}

/// 부분 전달 스트림(받은편지함·키 그룹)의 이벤트를 만들어 서명합니다(적용하지 않음). 부모는 그 스트림의 머리에서만
/// 고르고, 권한을 검사할 기준점으로 해소기가 마지막으로 적용한 상태 이벤트를 적습니다([`state_anchor`]).
/// `anchor_hint`는 기준점에 꼭 넣을 공간 스트림 이벤트(예: 받는 사람의 가입 이벤트)로, 최근 이벤트만으로는 그것을
/// 덮지 못할 때 씁니다.
pub(crate) fn author_scoped(
    node: &Node,
    space: &SpaceId,
    scope: Scope,
    body: &EventBody,
    key: &KeyChoice,
    anchor_hint: &[EventId],
    extra_parents: &[(EventId, u64)],
) -> Result<SignedEvent> {
    author_in(
        node,
        space,
        None,
        Some(scope),
        body,
        key,
        extra_parents,
        anchor_hint,
        None,
    )
}

/// 기준점: 꼭 넣을 이벤트를 먼저, 나머지는 해소기가 마지막으로 적용한 상태 이벤트부터 [`MAX_STATE_ANCHOR`]개까지
/// (정렬). 저장소 머리를 그대로 쓰지 않는 것은, 머리에 다른 노드가 거부해 갖지 않을 키 레인 이벤트가 끼면 받는 쪽이
/// 그 기준점을 영영 모르기 때문입니다. 아직 적용한 것이 없을 때만 공간 스트림 머리를 씁니다.
///
/// 바탕 체크포인트가 있으면 그 조상(경계 위 조상·경계 아래)은 넣지 않습니다(꼭 넣을 이벤트여도): 그 상태는 바탕 상태에
/// 이미 들어 있고, 체크포인트로 들어온 멤버는 그 조상의 상태를 셀 수 없으며, 키 그룹 갱신은 기준점에 바탕 뒤 이벤트가
/// 없으면 늦은 이벤트입니다. 남는 것이 없으면 바탕 이벤트를 넣습니다.
pub(crate) fn state_anchor(node: &Node, space: &SpaceId, hint: &[EventId]) -> Result<Vec<EventId>> {
    let core = node.spaces.read().get(space).map(|rt| rt.core.clone());
    let (hint, recent, base) = match core {
        Some(c) => {
            let c = c.lock();
            let r = &c.resolver;
            let base = r.base().map(|b| b.event);
            let keep = |id: &EventId| {
                base.is_none()
                    || !matches!(
                        r.anchor_place(id),
                        AnchorPlace::Covered | AnchorPlace::Below
                    )
            };
            let hint: Vec<EventId> = hint.iter().filter(|h| keep(h)).copied().collect();
            let recent: Vec<EventId> = r
                .recent_applied(MAX_STATE_ANCHOR)
                .into_iter()
                .filter(|id| keep(id))
                .collect();
            (hint, recent, base)
        }
        None => (hint.to_vec(), Vec::new(), None),
    };
    let mut out: Vec<EventId> = hint.into_iter().take(MAX_STATE_ANCHOR).collect();
    let fallback = if recent.is_empty() && base.is_none() {
        node.store
            .heads_with_lamport(space, StreamId::Space)?
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    } else {
        Vec::new()
    };
    for id in recent.into_iter().chain(fallback) {
        if out.len() >= MAX_STATE_ANCHOR {
            break;
        }
        if !out.contains(&id) {
            out.push(id);
        }
    }
    if out.is_empty()
        && let Some(b) = base
    {
        out.push(b);
    }
    if out.is_empty() {
        return Err(Error::conflict("아직 이 공간의 상태가 없습니다"));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// 부모 후보로 넘긴 이벤트 가운데 이미 저장된 다른 스트림의 것을 뺍니다. 특히 공간·채널 스트림 이벤트가 부분 전달
/// 이벤트를 부모로 가리키면, 그것을 받지 못한 노드는 그 부모를 빈 구간으로 보고 다른 순서로 해소할 수 있습니다.
/// 아직 저장하지 않은 것(같은 잠금 안에서 방금 만든 키 갱신 조각 등)은 부르는 쪽이 같은 스트림으로 만든 것입니다.
fn same_stream_parents(
    node: &Node,
    stream: StreamId,
    extra: &[(EventId, u64)],
) -> Result<Vec<(EventId, u64)>> {
    let mut out = Vec::with_capacity(extra.len());
    for (id, l) in extra {
        match node.store.event_meta(id)? {
            Some(m) if m.stream != stream => {}
            _ => out.push((*id, *l)),
        }
    }
    Ok(out)
}

/// `fixed`를 주면 부모를 고르지 않고 그대로 씁니다(체크포인트: 본문의 머리와 같아야 함).
#[allow(clippy::too_many_arguments)]
fn author_in(
    node: &Node,
    space: &SpaceId,
    channel: Option<ChannelId>,
    scope: Option<Scope>,
    body: &EventBody,
    key: &KeyChoice,
    extra_parents: &[(EventId, u64)],
    anchor_hint: &[EventId],
    fixed: Option<&[(EventId, u64)]>,
) -> Result<SignedEvent> {
    let me = node.me()?;
    if crate::friends::space_deactivated(node, space) {
        return Err(Error::forbidden(
            "만료된 연락처와의 대화라 보낼 수 없습니다(새 연락처 링크로 다시 만나면 이어집니다)",
        ));
    }
    if crate::checkpoint::is_unconfirmed(node, space) {
        return Err(Error::conflict(
            "체크포인트로 들어온 상태를 다른 멤버와 확인하는 중입니다(잠시 뒤 다시 시도)",
        ));
    }
    let mut kind = body.kind();
    // 익명 채널의 글은 헤더 종류로 익명 표시를 남깁니다(서명돼 있어 나중에 익명을 꺼도 익명으로 남음).
    if let Some(c) = channel
        && node
            .space_state(space)
            .is_some_and(|st| st.is_anonymous(&c))
    {
        kind = kind.anonymized();
    }
    let stream = match (scope, kind.lane(), channel) {
        (Some(s), _, None) => s.stream(),
        (Some(_), _, Some(_)) => {
            return Err(Error::invalid("부분 전달 이벤트에는 채널이 없습니다"));
        }
        (None, Lane::Message | Lane::Key, Some(c)) => StreamId::Channel(c),
        _ => StreamId::Space,
    };
    if kind.requires_channel() && channel.is_none() {
        return Err(Error::invalid("채널이 필요한 이벤트입니다"));
    }
    // P2P 전용 채널은 참가한 기기만 씁니다(참가하지 않은 기기의 글은 P2P로 나갈 길도 없음).
    if let StreamId::Channel(c) = stream
        && !crate::p2p::may_author(node, space, &c)
    {
        return Err(Error::forbidden(
            "P2P 전용 채널에 참가하지 않아 보낼 수 없습니다(참가하면 참여자에게 IP가 보입니다)",
        ));
    }
    let parents = match fixed {
        Some(f) => {
            let mut v: Vec<EventId> = f.iter().map(|(id, _)| *id).collect();
            v.sort_unstable();
            v.dedup();
            v
        }
        None => {
            // 공간 스트림은 체크포인트 경계 아래 머리(늦게 와 거부된 이벤트)를 부모로 두지 않습니다: 그러면 새 이벤트도
            // 늦은 이벤트가 되어 모든 노드가 거부합니다.
            let floor = match stream {
                StreamId::Space => crate::checkpoint::floor(node, space),
                _ => 0,
            };
            let mut heads: Vec<(EventId, u64)> = node
                .store
                .heads_with_lamport(space, stream)?
                .into_iter()
                .filter(|(l, _)| *l >= floor)
                .map(|(l, id)| (id, l))
                .collect();
            // 해소기 머리도 더합니다: 저장소에서는 경계 아래 자식(낮은 논리 시계로 지어낸 것 포함)이 경계 위 머리를 가릴
            // 수 있고, 그 머리를 덮지 않으면 다음 체크포인트를 낼 수 없습니다.
            if floor > 0 {
                for h in crate::checkpoint::resolver_heads(node, space) {
                    if !heads.iter().any(|(id, _)| *id == h.0) {
                        heads.push(h);
                    }
                }
            }
            let extra = same_stream_parents(node, stream, extra_parents)?;
            heads.extend(extra.iter().copied());
            let mut parents = choose_parents(&heads);
            // 넘긴 부모(나눈 갱신의 머리 조각)는 꼭 넣습니다: 이어지는 조각은 머리를 부모로 둬야 받아들여집니다. 머리 수
            // 상한을 넘으면 논리 시계가 가장 작은 머리를 뺍니다.
            for (id, _) in &extra {
                if parents.contains(id) {
                    continue;
                }
                if parents.len() >= yangpa_core::event::MAX_PARENTS {
                    let lamport_of =
                        |p: &EventId| heads.iter().find(|(h, _)| h == p).map_or(0, |(_, l)| *l);
                    if let Some(pos) = parents
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| !extra.iter().any(|(e, _)| e == *p))
                        .min_by_key(|(_, p)| (lamport_of(p), **p))
                        .map(|(i, _)| i)
                    {
                        parents.remove(pos);
                    }
                }
                parents.push(*id);
            }
            parents.sort_unstable();
            parents.dedup();
            parents
        }
    };
    let state_anchor = match scope {
        Some(_) => state_anchor(node, space, anchor_hint)?,
        None => Vec::new(),
    };
    let max = extra_parents
        .iter()
        .chain(fixed.unwrap_or_default())
        .map(|(_, l)| *l)
        .fold(node.store.max_lamport(space)?, u64::max);
    let lamport = next_lamport(max);

    let (key_ref, sym): (KeyRef, Option<Secret32>) = match key {
        KeyChoice::State => {
            let (id, k) = keys::current_state_key(node, space)?
                .ok_or_else(|| Error::conflict("아직 이 공간의 상태 키가 없습니다"))?;
            (KeyRef::State(id), Some(k))
        }
        KeyChoice::StateWith(id, k) => (KeyRef::State(*id), Some(k.clone())),
        KeyChoice::HistoryWith(id, k) => (
            KeyRef::Record { root: *id },
            Some(yangpa_crypto::keysched::record_key(k)),
        ),
        KeyChoice::Volatile(generation, k) => (
            KeyRef::Volatile {
                generation: *generation,
            },
            Some(k.clone()),
        ),
        KeyChoice::Sealed(_) => (KeyRef::Sealed, None),
    };
    let header = EventHeader {
        v: EVENT_VERSION,
        space: *space,
        channel,
        kind,
        author: me.device_id(),
        user: me.user(),
        parents,
        lamport,
        created_at: Timestamp::now(),
        key: key_ref,
        body_hash: [0; 32],
        body_len: 0,
        scope,
        state_anchor,
    };
    let bytes = match key {
        KeyChoice::Sealed(rcpts) => seal_body(rcpts, &header, body)?,
        _ => {
            let k = sym.ok_or_else(|| Error::internal("대칭 키 없음"))?;
            encrypt_body(&k, &header, body)?
        }
    };
    Ok(SignedEvent::sign(header, bytes, &me.device.signing)?)
}

/// 이벤트를 만들어 이 노드에 적용합니다(잠금 안). 적용 경로가 거부하면 오류입니다. 전파는 부르는 쪽이
/// 잠금을 푼 뒤 [`sync_service::propagate`]로 합니다.
pub(crate) fn publish_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    channel: Option<ChannelId>,
    body: &EventBody,
    key: KeyChoice,
    extra_parents: &[(EventId, u64)],
) -> Result<(SignedEvent, IngestReport)> {
    // 부모·논리 시계는 그 공간의 적용 잠금 안에서 골라야 합니다.
    l.check(&space)?;
    // 해산한 크루에는 무엇을 내도 거부되므로(프로필·기기 목록처럼 모든 공간에 내는 것 포함) 만들지 않습니다.
    if node.space_state(&space).is_some_and(|s| s.disbanded) {
        return Err(Error::forbidden("해산한 크루입니다"));
    }
    let ev = author_event(node, &space, channel, body, &key, extra_parents)?;
    let id = ev.id();
    let report = ingest::ingest_locked(node, l, space, vec![ev.clone()], Source::Local)?;
    if let Some((_, why)) = report.rejected.iter().find(|(x, _)| *x == id) {
        return Err(Error::forbidden(why.clone()));
    }
    Ok((ev, report))
}

/// 부모를 정해 이벤트를 만들고 이 노드에 적용합니다(잠금 안). 체크포인트처럼 부모가 본문과 맞아야 할 때 씁니다.
pub(crate) fn publish_with_parents_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    body: &EventBody,
    key: KeyChoice,
    parents: &[(EventId, u64)],
) -> Result<(SignedEvent, IngestReport)> {
    l.check(&space)?;
    let ev = author_in(
        node,
        &space,
        None,
        None,
        body,
        &key,
        &[],
        &[],
        Some(parents),
    )?;
    let id = ev.id();
    let report = ingest::ingest_locked(node, l, space, vec![ev.clone()], Source::Local)?;
    if let Some((_, why)) = report.rejected.iter().find(|(x, _)| *x == id) {
        return Err(Error::forbidden(why.clone()));
    }
    Ok((ev, report))
}

/// 부분 전달 이벤트를 만들어 이 노드에 적용합니다(잠금 안, [`author_scoped`]). 적용 경로가 기준점 상태로 권한을
/// 확인해 거부하면 오류입니다. 전파는 부르는 쪽이 잠금을 푼 뒤 [`sync_service::propagate`]로 합니다(gossip에 싣지
/// 않고 받을 자격이 있는 기기에게만 직접 보냄).
pub(crate) fn publish_scoped_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    scope: Scope,
    body: &EventBody,
    key: KeyChoice,
    anchor_hint: &[EventId],
) -> Result<SignedEvent> {
    publish_scoped_parts_locked(node, l, space, scope, body, key, anchor_hint, &[])
}

/// [`publish_scoped_locked`]에 같은 스트림의 부모를 더해(나눈 키 갱신의 이어지는 조각이 머리 조각을 부모로 둠).
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_scoped_parts_locked(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: SpaceId,
    scope: Scope,
    body: &EventBody,
    key: KeyChoice,
    anchor_hint: &[EventId],
    extra_parents: &[(EventId, u64)],
) -> Result<SignedEvent> {
    l.check(&space)?;
    let ev = author_scoped(node, &space, scope, body, &key, anchor_hint, extra_parents)?;
    let id = ev.id();
    let report = ingest::ingest_locked(node, l, space, vec![ev.clone()], Source::Local)?;
    if let Some((_, why)) = report.rejected.iter().find(|(x, _)| *x == id) {
        return Err(Error::forbidden(why.clone()));
    }
    Ok(ev)
}

/// 잠그고, 만들고, 적용하고, 전파합니다(다른 작업 영역의 상태 이벤트용).
pub(crate) async fn publish(
    node: &Arc<Node>,
    space: SpaceId,
    channel: Option<ChannelId>,
    body: &EventBody,
    key: KeyChoice,
) -> Result<SignedEvent> {
    let l = ingest::lock_space(node, space).await;
    let (ev, report) = publish_locked(node, &l, space, channel, body, key, &[])?;
    drop(l);
    ingest::after_unlock(node, space, &report);
    sync_service::propagate(node, space, vec![ev.clone()]);
    Ok(ev)
}
