//! `library.list`(라이브러리 탭): 이 기기가 보내거나 받은 첨부를 실은 메시지.
//!
//! 기록 전체를 풀지 않고 첨부 쓰임 기록(`blobs::all_refs`: 일반 등급 채널에서 첨부를 실은 메시지)에서 메시지를 모읍니다.
//! 휘발성 채널은 그 기록이 없어 빠집니다(RAM 전용이라 목록에도 넣지 않음). 거르기는 채팅 목록과 같습니다: 토큰 범위,
//! 멤버, 채널 보기 권한, 들어가 있는 동안만 보이는 채널, 이 기기가 참여하지 않은 P2P 전용 채널, 지운 글, 신고하며 숨긴
//! 글, 차단한 사람의 글(목록이 접어 두는 것). 익명 글은 `view_of`가 글쓴이를 가립니다.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use yangpa_core::ids::{ChannelId, EventId, SpaceId, UserId};
use yangpa_core::model::{JoinMode, Permissions};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    LIBRARY_PAGE_MAX, LIBRARY_PICK_MAX, LibraryItem, LibraryListParams, LibraryPage, m,
};
use yangpa_store::MessageRecord;

use super::{CallCtx, P, R, me};
use crate::error::Error;
use crate::node::Node;
use crate::{blobs, messages, moderation, p2p};

pub(crate) async fn list(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::LibraryList>,
) -> R<m::LibraryList> {
    let me = me(node)?.user();
    let (node, ctx) = (node.clone(), ctx.clone());
    // 메시지마다 복호화하므로 블로킹 스레드에서 돕니다(검색과 같음).
    tokio::task::spawn_blocking(move || list_blocking(&node, &ctx, &me, &p))
        .await
        .map_err(|e| RpcError::internal(format!("라이브러리 목록 작업 실패: {e}")))?
}

/// 채팅 목록에 이 채널의 메시지가 보이는지.
fn channel_visible(
    node: &Node,
    ctx: &CallCtx,
    me: &UserId,
    only: Option<&SpaceId>,
    space: &SpaceId,
    channel: &ChannelId,
) -> bool {
    only.is_none_or(|s| s == space)
        && ctx.allows_space(space)
        && node.space_state(space).is_some_and(|st| {
            st.is_member(me)
                && st
                    .channels
                    .get(channel)
                    .is_some_and(|c| !c.is_volatile() && c.join_mode == JoinMode::Always)
                && st
                    .channel_permissions(me, channel)
                    .contains(Permissions::VIEW_CHANNEL)
                && (!st.is_p2p(channel) || p2p::p2p_joined(node, space, channel))
        })
}

fn list_blocking(
    node: &Node,
    ctx: &CallCtx,
    me: &UserId,
    p: &LibraryListParams,
) -> Result<LibraryPage, RpcError> {
    let mut visible: HashMap<(SpaceId, ChannelId), bool> = HashMap::new();
    let mut visible_at = |space: &SpaceId, channel: &ChannelId| {
        *visible
            .entry((*space, *channel))
            .or_insert_with(|| channel_visible(node, ctx, me, p.space.as_ref(), space, channel))
    };
    let pick = p.messages.is_some();
    let (recs, total): (Vec<MessageRecord>, usize) = match &p.messages {
        // 고른 글만(갤러리 썸네일): 쓰임 기록을 훑지 않고 바로 읽습니다. 첨부가 없는 글은 아래에서 빠집니다.
        Some(ids) => {
            let mut ids: Vec<EventId> = ids.iter().take(LIBRARY_PICK_MAX).copied().collect();
            ids.sort_unstable();
            ids.dedup();
            let recs: Vec<MessageRecord> = node
                .store
                .get_messages(&ids, Some(me))
                .map_err(Error::from)?
                .into_iter()
                .filter(|r| visible_at(&r.space, &r.channel))
                .collect();
            let total = recs.len();
            (recs, total)
        }
        None => {
            let mut seen = HashSet::new();
            // (논리 시계, 메시지, 공간, 채널). 쪽이 흔들리지 않게 최신순으로 줄 세웁니다(화면이 다시 정렬함).
            let mut cands: Vec<(u64, EventId, SpaceId, ChannelId)> = Vec::new();
            for r in blobs::all_refs(node) {
                if seen.insert(r.event) && visible_at(&r.space, &r.channel) {
                    cands.push((r.lamport, r.event, r.space, r.channel));
                }
            }
            cands.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            let total = cands.len();
            let limit = p
                .limit
                .unwrap_or(LIBRARY_PAGE_MAX)
                .clamp(1, LIBRARY_PAGE_MAX) as usize;
            let page: Vec<_> = cands
                .into_iter()
                .skip(p.offset.unwrap_or(0) as usize)
                .take(limit)
                .collect();
            let at: HashMap<EventId, (SpaceId, ChannelId)> =
                page.iter().map(|c| (c.1, (c.2, c.3))).collect();
            let ids: Vec<EventId> = page.iter().map(|c| c.1).collect();
            // 지운 글은 `get_messages`가 빼고, 기록에 적힌 공간·채널과 보기 행이 다르면 뺍니다.
            let recs = node
                .store
                .get_messages(&ids, Some(me))
                .map_err(Error::from)?
                .into_iter()
                .filter(|r| at.get(&r.id) == Some(&(r.space, r.channel)))
                .collect();
            (recs, total)
        }
    };
    let mut hidden: HashMap<SpaceId, bool> = HashMap::new();
    let mut items = Vec::with_capacity(recs.len());
    for rec in recs {
        if *hidden
            .entry(rec.space)
            .or_insert_with(|| moderation::has_hidden(node, &rec.space))
            && moderation::is_hidden(node, &rec.space, &rec.id)
        {
            continue;
        }
        let mut v = messages::view_of(node, &rec);
        if v.author_blocked || v.attachments.is_empty() {
            continue;
        }
        v.space = Some(rec.space);
        v.channel = Some(rec.channel);
        if !pick {
            for a in &mut v.attachments {
                a.thumbnail_base64 = None;
            }
        }
        let meta = node.store.event_meta(&rec.id).map_err(Error::from)?;
        items.push(LibraryItem {
            message: v,
            received_at: meta.map_or(rec.created_at, |m| m.received_at),
            excluded: meta.is_some_and(|m| m.seed_excluded),
        });
    }
    Ok(LibraryPage {
        items,
        total: u32::try_from(total).unwrap_or(u32::MAX),
    })
}
