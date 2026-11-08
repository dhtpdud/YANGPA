//! `blob.cache_*`, `blob.pin`/`unpin`/`pins`: 받은 첨부의 저장 공간(정리는 `blobs::cache`).

use std::sync::Arc;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::{BlobCacheClearResult, BlobPinView, m};

use super::{CallCtx, P, R, empty, me, rpc};
use crate::admin;
use crate::blobs::{self, cache};
use crate::node::Node;

/// 고정하면서 받기 시작할 때의 우선순위(`blob.fetch` 기본값과 같음).
const PIN_FETCH_PRIORITY: u8 = 128;

pub(crate) async fn cache_status(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::BlobCacheStatus>,
) -> R<m::BlobCacheStatus> {
    cache::run_blocking(node, cache::status)
        .await
        .ok_or_else(|| RpcError::internal("저장 공간 정리 작업이 멈췄습니다"))
}

/// 한도를 낮추거나 최소 여유 공간을 올리면 바로 정리하므로 `blob.cache_clear`처럼 받은 첨부를 모두 비울 수
/// 있습니다. 그래서 자동화 토큰이 쓸 자리를 줄이면 사람 확인을 받습니다(늘리거나 그대로면 바로 실행).
pub(crate) async fn cache_set_limit(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::BlobCacheSetLimit>,
) -> R<m::BlobCacheSetLimit> {
    if p.reset && (p.limit_bytes.is_some() || p.min_free_bytes.is_some()) {
        return Err(RpcError::invalid_input("reset과 값은 함께 줄 수 없습니다"));
    }
    if cache::lowers_budget(node, p.limit_bytes, p.min_free_bytes, p.reset) {
        let params = serde_json::to_value(&p).map_err(|e| RpcError::internal(e.to_string()))?;
        admin::confirm_when_others(node, ctx, "blob.cache_set_limit", &params).await?;
    }
    rpc(cache::set_limits(
        node,
        p.limit_bytes,
        p.min_free_bytes,
        p.reset,
    ))?;
    cache::run_blocking(node, |n| {
        cache::enforce(n);
        cache::status(n)
    })
    .await
    .ok_or_else(|| RpcError::internal("저장 공간 정리 작업이 멈췄습니다"))
}

pub(crate) async fn cache_clear(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::BlobCacheClear>,
) -> R<m::BlobCacheClear> {
    let (evicted_count, freed_bytes) = cache::run_blocking(node, cache::clear)
        .await
        .ok_or_else(|| RpcError::internal("저장 공간 정리 작업이 멈췄습니다"))?;
    Ok(BlobCacheClearResult {
        evicted_count,
        freed_bytes,
    })
}

/// 이 공간에서 쓰이는 블롭만 고정할 수 있습니다(휘발성 채널의 첨부는 기록되지 않아 여기서 걸러짐).
pub(crate) async fn pin(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::BlobPin>) -> R<m::BlobPin> {
    let me = me(node)?;
    let state = node
        .space_state(&p.space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    let want = blobs::manual_want(node, &p.space, &state, &p.blob, PIN_FETCH_PRIORITY)
        .ok_or_else(|| RpcError::not_found("이 공간에서 쓰이지 않는 블롭"))?;
    rpc(cache::pin(node, p.space, p.blob))?;
    // 고정은 "이 기기에 남겨 둠"이라 아직 받지 않았으면 받기 시작합니다.
    blobs::add_want(node, p.blob, want);
    Ok(empty())
}

/// 이 공간 일반 채널의 첨부만 지웁니다(휘발성 첨부는 기록되지 않아 여기서 걸러짐). 이모지·아이콘·프로필 이미지는 그
/// 쓰임에 늘 필요해 지우지 않습니다.
pub(crate) async fn evict(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::BlobEvict>) -> R<m::BlobEvict> {
    let me = me(node)?;
    let state = node
        .space_state(&p.space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    if !blobs::uses_in_space(node, &p.space, &state, &p.blob)
        .iter()
        .any(|u| matches!(u, blobs::Use::Attachment(_)))
    {
        return Err(RpcError::not_found("이 공간에서 쓰이지 않는 블롭"));
    }
    if cache::in_fixed_use(node, &p.blob) {
        return Err(RpcError::invalid_input(
            "이모지·아이콘·프로필 이미지로 쓰이는 파일은 지울 수 없습니다",
        ));
    }
    let blob = p.blob;
    let done = cache::run_blocking(node, move |n| blobs::evict(n, &blob))
        .await
        .ok_or_else(|| RpcError::internal("저장 공간 정리 작업이 멈췄습니다"))?;
    if !rpc(done)? {
        return Err(RpcError::conflict(
            "받기를 멈추는 중입니다. 잠시 뒤 다시 지워 주세요",
        ));
    }
    Ok(empty())
}

pub(crate) async fn unpin(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::BlobUnpin>) -> R<m::BlobUnpin> {
    me(node)?;
    let pinned_here = cache::pins(node)
        .iter()
        .any(|(b, pin)| *b == p.blob && pin.space == p.space);
    if !pinned_here {
        return Err(RpcError::not_found("이 공간에서 고정한 블롭이 아닙니다"));
    }
    rpc(cache::unpin(node, &p.blob))?;
    Ok(empty())
}

pub(crate) async fn pins(node: &Arc<Node>, ctx: &CallCtx, p: P<m::BlobPins>) -> R<m::BlobPins> {
    me(node)?;
    Ok(cache::pins(node)
        .into_iter()
        .filter(|(_, pin)| p.space.is_none_or(|s| s == pin.space) && ctx.allows_space(&pin.space))
        .map(|(blob, pin)| BlobPinView {
            space: pin.space,
            blob,
            pinned_at: pin.at,
            size: blobs::plaintext_size(node, &pin.space, &blob),
            downloaded: blobs::is_downloaded(node, &blob),
        })
        .collect())
}
