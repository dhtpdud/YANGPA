//! `seed.*`, `peer.get`/`peer.set`: 시더·피어 정책(저장·판단은 `policy`).
//!
//! 담당: B3(블롭·정책).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use yangpa_core::event::StreamId;
use yangpa_core::ids::{ChannelId, SpaceId};
use yangpa_core::model::ChannelKind;
use yangpa_core::wire::{YpRequest, YpResponse};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{self as rpc_api, ChannelCoverage, m};
use yangpa_rpc::api::{Notification, SeedSetParams};
use yangpa_sync::policy::{DataType, PeerPolicy, SeederPolicy};

use super::{CallCtx, P, R, me, rpc};
use crate::error::Error;
use crate::node::Node;
use crate::policy;

/// 커버리지 확인 요청 하나의 시간 제한(응답이 없으면 그 시더는 없는 것으로 셈).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 커버리지 확인 동시 요청 수.
const PROBE_CONCURRENCY: usize = 16;

pub(crate) fn sync_type(t: rpc_api::DataType) -> DataType {
    match t {
        rpc_api::DataType::Text => DataType::Text,
        rpc_api::DataType::Image => DataType::Image,
        rpc_api::DataType::Video => DataType::Video,
        rpc_api::DataType::File => DataType::File,
        rpc_api::DataType::Emoji => DataType::Emoji,
        rpc_api::DataType::HistoryBundle => DataType::HistoryBundle,
    }
}

pub(crate) fn api_type(t: DataType) -> rpc_api::DataType {
    match t {
        DataType::Text => rpc_api::DataType::Text,
        DataType::Image => rpc_api::DataType::Image,
        DataType::Video => rpc_api::DataType::Video,
        DataType::File => rpc_api::DataType::File,
        DataType::Emoji => rpc_api::DataType::Emoji,
        DataType::HistoryBundle => rpc_api::DataType::HistoryBundle,
    }
}

fn seeder_view(p: &SeederPolicy) -> rpc_api::SeederPolicy {
    rpc_api::SeederPolicy {
        enabled: p.enabled,
        types: p.types.iter().copied().map(api_type).collect(),
        include_channels: p
            .include_channels
            .as_ref()
            .map(|s| s.iter().copied().collect()),
        all_channels: p.all_channels,
        exclude_channels: p.exclude_channels.iter().copied().collect(),
        excluded_events: p.excluded_events.iter().copied().collect(),
        upload_limit_bps: p.upload_limit_bps,
        max_concurrent: p.max_concurrent,
        prefetch: p.prefetch,
    }
}

fn peer_view(p: &PeerPolicy) -> rpc_api::PeerPolicy {
    rpc_api::PeerPolicy {
        types: p.types.iter().copied().map(api_type).collect(),
        priorities: p
            .priorities
            .iter()
            .map(|(t, v)| (api_type(*t), *v))
            .collect(),
        download_limit_bps: p.download_limit_bps,
        auto_download_max_bytes: p.auto_download_max_bytes,
    }
}

/// 공간을 주면 그 공간을 알아야 합니다(모르는 공간의 설정을 쌓아 두지 않게).
fn known_space(node: &Node, space: Option<&SpaceId>) -> Result<(), RpcError> {
    match space {
        Some(s) if node.space_state(s).is_none() => Err(RpcError::not_found("모르는 공간")),
        _ => Ok(()),
    }
}

pub(crate) async fn seed_get(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::SeedGet>) -> R<m::SeedGet> {
    known_space(node, p.space.as_ref())?;
    let mut policy = policy::stored_seeder_policy(node, p.space.as_ref());
    if let Some(s) = &p.space {
        policy.excluded_events = policy::excluded_events(node, s);
    }
    Ok(seeder_view(&policy))
}

fn apply_seed_set(mut policy: SeederPolicy, p: &SeedSetParams) -> Result<SeederPolicy, RpcError> {
    if let Some(e) = p.enabled {
        policy.enabled = e;
    }
    if let Some(t) = &p.types {
        policy.types = t.iter().copied().map(sync_type).collect();
    }
    if let Some(c) = &p.channels {
        policy.include_channels = c.as_ref().map(|v| v.iter().copied().collect());
    }
    if let Some(a) = p.all_channels {
        policy.all_channels = a;
    }
    if let Some(x) = &p.exclude_channels {
        policy.exclude_channels = x.iter().copied().collect();
    }
    if let Some(u) = p.upload_limit_bps {
        policy.upload_limit_bps = u;
    }
    if let Some(n) = p.max_concurrent {
        if n == 0 {
            return Err(RpcError::invalid_input(
                "max_concurrent는 1 이상이어야 합니다",
            ));
        }
        policy.max_concurrent = n;
    }
    if let Some(on) = p.prefetch {
        policy.prefetch = on;
    }
    Ok(policy)
}

pub(crate) async fn seed_set(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::SeedSet>) -> R<m::SeedSet> {
    known_space(node, p.space.as_ref())?;
    let old = policy::stored_seeder_policy(node, p.space.as_ref());
    let policy = apply_seed_set(old.clone(), &p)?;
    let prefetch = policy.enabled && policy.prefetch;
    rpc(policy::set_seeder_policy(
        node,
        p.space.as_ref(),
        policy.clone(),
    ))?;
    if p.space.is_none() {
        rpc(policy::follow_global_rate(node, &old, &policy))?;
    }
    if prefetch {
        // 미리 받기를 켠 공간은 휴면이 아니므로 깨우고, 지난 첨부를 바로 채우기 시작합니다.
        let spaces = match p.space {
            Some(s) => vec![s],
            None => node.space_ids(),
        };
        for s in &spaces {
            policy::is_dormant(node, s);
        }
        crate::blobs::cache::run_blocking(node, crate::blobs::prefetch::backfill).await;
    }
    Ok(super::empty())
}

pub(crate) async fn exclude_message(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SeedExcludeMessage>,
) -> R<m::SeedExcludeMessage> {
    me(node)?;
    let state = node
        .space_state(&p.space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.channels.contains_key(&p.channel) {
        return Err(RpcError::not_found("없는 채널"));
    }
    // 이 공간·채널에 저장된 메시지만(남의 공간 이벤트 ID를 섞어 넣지 않게).
    node.store
        .get_message(&p.id, None)
        .map_err(Error::from)?
        .filter(|r| r.space == p.space && r.channel == p.channel)
        .ok_or_else(|| RpcError::not_found("없는 메시지"))?;
    rpc(policy::set_excluded(node, &p.space, &p.id, p.excluded))?;
    Ok(super::empty())
}

/// 채널마다 그 채널을 지금 제공하는 온라인 시더(기기) 수. 인증된 멤버 기기에 `GetHeads`로 물어 답한(바쁨 포함)
/// 기기를 세고, 내 설정이 그 채널을 제공하면 나도 셉니다. P2P 전용 채널은 함께 참여한 기기에게만 직접 연결로 묻고
/// (Tor로는 답하지 않음), 내가 참여하지 않았으면 셀 수 없어 뺍니다. 기록이 없는 휘발성·카테고리·통화 채널도
/// 뺍니다. 지정된 보관 노드는 `online_archives`로 따로 셉니다. 둘 다 0이 된 채널은 `seed.coverage_warning`도
/// 알립니다(0인 동안 한 번).
pub(crate) async fn coverage(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SeedCoverage>,
) -> R<m::SeedCoverage> {
    let me = me(node)?;
    let space = p.space;
    let state = node
        .space_state(&space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    let channels: Vec<ChannelId> = state
        .channels
        .iter()
        .filter(|(c, ch)| {
            !ch.is_volatile()
                && !matches!(ch.kind, ChannelKind::Category | ChannelKind::Voice)
                && (!state.is_p2p(c) || crate::p2p::p2p_joined(node, &space, c))
        })
        .map(|(c, _)| *c)
        .collect();
    let mine = policy::seeder_policy(node, &space);
    let mut counts: BTreeMap<ChannelId, u32> = channels
        .iter()
        .map(|c| {
            let opened = policy::channel_opened(node, &space, &state, c);
            (
                *c,
                u32::from(mine.seeds(DataType::Text, Some(c), None, opened)),
            )
        })
        .collect();

    let me_peer = node.peer_id();
    let peers: BTreeSet<_> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(p, u, d)| *p != me_peer && state.is_member(u) && state.device_belongs(u, d))
        .map(|(p, _, _)| p)
        .collect();
    let pairs: Vec<_> = channels
        .iter()
        .flat_map(|c| {
            let to: Vec<_> = if state.is_p2p(c) {
                crate::p2p::channel_peers(node, &space, &state, c)
                    .into_iter()
                    .map(|(p, _, _)| p)
                    .collect()
            } else {
                peers.iter().copied().collect()
            };
            to.into_iter().map(|p| (p, *c))
        })
        .collect();
    let probes = pairs.into_iter().map(|(peer, channel)| {
        let node = node.clone();
        async move {
            let req = YpRequest::GetHeads {
                space,
                stream: StreamId::Channel(channel),
            };
            let r =
                tokio::time::timeout(PROBE_TIMEOUT, crate::p2p::request(&node, peer, req)).await;
            let seeds = matches!(
                r,
                Ok(Ok(YpResponse::Heads { .. } | YpResponse::Busy { .. }))
            );
            (channel, seeds)
        }
    });
    let results: Vec<(ChannelId, bool)> = futures::stream::iter(probes)
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;
    for (c, seeds) in results {
        if seeds && let Some(n) = counts.get_mut(&c) {
            *n += 1;
        }
    }
    // 보관 노드는 따로 셉니다(멤버 시더가 없어도 기록은 남아 있음).
    let archives = crate::archive::coverage(node, &space, &state, &channels).await;
    let out: Vec<ChannelCoverage> = channels
        .iter()
        .map(|c| ChannelCoverage {
            channel: *c,
            online_seeders: counts.get(c).copied().unwrap_or(0),
            online_archives: archives.get(c).copied().unwrap_or(0),
        })
        .collect();
    let zero = out
        .iter()
        .filter(|c| c.online_seeders == 0 && c.online_archives == 0)
        .map(|c| c.channel)
        .collect();
    for channel in policy::newly_uncovered(node, &space, zero) {
        node.notify
            .publish(Notification::SeedCoverageWarning { space, channel });
    }
    Ok(out)
}

pub(crate) async fn peer_get(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::PeerGet>) -> R<m::PeerGet> {
    known_space(node, p.space.as_ref())?;
    Ok(peer_view(&policy::peer_policy(node, p.space.as_ref())))
}

pub(crate) async fn peer_set(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::PeerSet>) -> R<m::PeerSet> {
    known_space(node, p.space.as_ref())?;
    let mut policy = policy::peer_policy(node, p.space.as_ref());
    if let Some(t) = &p.types {
        policy.types = t.iter().copied().map(sync_type).collect();
    }
    if let Some(pr) = &p.priorities {
        policy.priorities = pr.iter().map(|(t, v)| (sync_type(*t), *v)).collect();
    }
    if let Some(d) = p.download_limit_bps {
        // 0은 받지 않음(첨부 조각을 요청하지 않음).
        policy.download_limit_bps = d;
    }
    if let Some(a) = p.auto_download_max_bytes {
        policy.auto_download_max_bytes = a;
    }
    rpc(policy::set_peer_policy(node, p.space.as_ref(), policy))?;
    Ok(super::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_type_names_match() {
        for t in DataType::ALL {
            assert_eq!(api_type(t).name(), t.name());
            assert_eq!(sync_type(api_type(t)), t);
        }
    }

    #[test]
    fn seed_set_keeps_missing_fields() {
        let base = SeederPolicy::default();
        let p = SeedSetParams {
            enabled: Some(false),
            ..Default::default()
        };
        let out = apply_seed_set(base.clone(), &p).unwrap();
        assert!(!out.enabled);
        assert_eq!(out.types, base.types);
        assert_eq!(out.upload_limit_bps, base.upload_limit_bps);
        let p = SeedSetParams {
            upload_limit_bps: Some(None),
            channels: Some(Some(vec![ChannelId([1; 16])])),
            ..Default::default()
        };
        let out = apply_seed_set(base, &p).unwrap();
        assert_eq!(out.upload_limit_bps, None);
        assert_eq!(
            out.include_channels,
            Some([ChannelId([1; 16])].into_iter().collect())
        );
        let bad = SeedSetParams {
            max_concurrent: Some(0),
            ..Default::default()
        };
        assert!(apply_seed_set(SeederPolicy::default(), &bad).is_err());
    }
}
