//! `message.*`(코어). 휘발성 채널이면 `volatile`의 함수로 넘기고, 첨부는 `blobs`가 준비합니다.
//! `forum.*`(B1): 게시글은 메시지 레인(기록 키), 제목·태그·고정·잠금 변경은 상태 레인의 `ForumPostUpdate`
//! (게시글 단위 마지막 쓰기 우선, 보기 행 반영은 `crews`). 잠긴 게시글에 댓글을 막는 것은 API에서만 합니다
//! (해소기는 메시지 레인에서 게시글 상태를 모름). 대문 게시글은 상태 레인의 `ForumFeaturedSet`(채널마다 하나)이고,
//! 해소기는 게시글이 있는지 모르므로 보기가 이 기기에 없는 글·지운 글을 거릅니다.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use yangpa_core::body::{EventBody, MessageBody};
use yangpa_core::event::StreamId;
use yangpa_core::ids::{ChannelId, EventId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, ChannelSettings, Permissions, SpaceKind, SpaceState};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    ANONYMOUS_USER, EventRef, FORUM_PREVIEW_CHARS, ForumPostView, ForumSort, GapView,
    MessageListResult, MessageSearchPage, MessageSearchParams, SortOrder, TagMatch, m,
};
use yangpa_store::{MessageKind, MessageQuery, MessageRecord, SearchFilter};
use yangpa_sync::state::limits::MAX_POST_TAGS;

use super::{CallCtx, P, R, me, rpc};
use crate::author::{self, KeyChoice};
use crate::error::Error;
use crate::node::Node;
use crate::{admin, blobs, crews, ingest, keygroup, messages, policy, sync_service, volatile};

/// 공간 상태·채널과 내 권한을 확인합니다.
fn channel_ctx(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    need: Permissions,
) -> Result<(Arc<SpaceState>, ChannelSettings), RpcError> {
    let me = me(node)?;
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
    let perms = state.channel_permissions(&me.user(), channel);
    if !perms.contains(Permissions::VIEW_CHANNEL) || !perms.contains(need) {
        return Err(RpcError::forbidden(
            "이 채널에서 그 작업을 할 권한이 없습니다",
        ));
    }
    Ok((state, ch))
}

/// 메시지 레인 본문을 채널 뿌리의 기록 키로 발행합니다. 보내기 전에 나간·내보낸 기기를 뺀 키 갱신을 기다립니다
/// (보내는 쪽 규칙, `keygroup::wait_rotation`).
async fn publish_message(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    body: EventBody,
) -> Result<EventRef, RpcError> {
    // 기다린 뒤 적용 잠금을 잡기 전에 제거가 들어왔을 수 있으므로 잠금 안에서 다시 봅니다(그 사이 들어온 제거를 옛 키로
    // 보내지 않게). 기다림을 포기하고 옛 키로 보내기로 했을 때만 그대로 보냅니다.
    let l = loop {
        let gave_up = keygroup::wait_rotation(node, space, channel).await;
        let l = ingest::lock_space(node, space).await;
        if gave_up || !keygroup::removal_pending(node, &space, &channel) {
            break l;
        }
        drop(l);
    };
    let state = node
        .space_state(&space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    let (hid, key) = rpc(messages::send_key_locked(
        node, &l, &space, &channel, &state,
    ))?;
    let (ev, report) = rpc(author::publish_locked(
        node,
        &l,
        space,
        Some(channel),
        &body,
        KeyChoice::HistoryWith(hid, key),
        &[],
    ))?;
    drop(l);
    ingest::after_unlock(node, space, &report);
    let id = ev.id();
    sync_service::propagate(node, space, vec![ev]);
    policy::mark_channel_opened(node, &space, &channel);
    Ok(EventRef { id })
}

pub(crate) async fn send(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageSend>,
) -> R<m::MessageSend> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::SEND_MESSAGES)?;
    if ch.is_volatile() {
        return volatile::send_message(node, ctx, p).await;
    }
    if p.text.trim().is_empty() && p.attachments.is_empty() {
        return Err(RpcError::invalid_input("빈 메시지입니다"));
    }
    if ch.kind == ChannelKind::Forum && p.thread.is_none() {
        return Err(RpcError::invalid_input(
            "포럼 채널에는 게시글(forum.create_post) 안에만 쓸 수 있습니다",
        ));
    }
    if let Some(post) = &p.thread {
        check_thread_open(node, &state, &p.space, &p.channel, post)?;
    }
    let perms = state.channel_permissions(&me(node)?.user(), &p.channel);
    // 받는 쪽도 같은 권한을 검사하므로, 첨부를 만들기 전에 거절합니다(남는 고정 블롭이 생기지 않게).
    if !p.attachments.is_empty() && !perms.contains(Permissions::ATTACH_FILES) {
        return Err(RpcError::forbidden("이 채널에 파일을 올릴 권한이 없습니다"));
    }
    // 멘션 목록은 받는 쪽의 멘션 알림·안 읽은 멘션 수와 알림 토픽에 씁니다(멤버만).
    let mut mentions = crate::messages::mentioned_users(&p.text, |u| state.is_member(u));
    // 받는 쪽은 상한을 넘는 메시지를 거부하므로 앞쪽만 싣습니다(글에는 그대로 남음).
    mentions.truncate(yangpa_sync::state::limits::MAX_MENTIONS);
    drop(state);
    let attachments =
        blobs::prepare_attachments(node, &p.space, &p.channel, &p.attachments).await?;
    // EMBED_LINKS가 없으면 외부 사이트에 접속하지도 않습니다.
    let embeds = if ch.link_previews && perms.contains(Permissions::EMBED_LINKS) {
        blobs::link_previews(node, &p.text).await
    } else {
        Vec::new()
    };
    let prepared: Vec<_> = attachments.iter().map(|a| a.blob).collect();
    let body = EventBody::Message(MessageBody {
        text: p.text,
        attachments,
        embeds,
        reply_to: p.reply_to,
        thread: p.thread,
        mentions,
    });
    let r = publish_message(node, p.space, p.channel, body).await;
    if r.is_err() {
        // 보내지 못한 메시지의 첨부는 아무도 가리키지 않으니 지웁니다.
        blobs::discard_prepared(node, &prepared);
    }
    r
}

pub(crate) async fn edit(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageEdit>,
) -> R<m::MessageEdit> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::NONE)?;
    check_editable(&state, &p.channel)?;
    if ch.is_volatile() {
        return volatile::edit_message(node, ctx, p).await;
    }
    let me = me(node)?;
    let rec = node
        .store
        .get_message(&p.id, None)
        .map_err(Error::from)?
        .filter(|r| r.space == p.space && r.channel == p.channel)
        .ok_or_else(|| RpcError::not_found("없는 메시지"))?;
    if rec.author != me.user() {
        return Err(RpcError::forbidden("자기 메시지만 고칠 수 있습니다"));
    }
    if rec.deleted {
        return Err(RpcError::conflict("삭제된 메시지입니다"));
    }
    let body = EventBody::Edit {
        target: p.id,
        text: p.text,
        embeds: Vec::new(),
    };
    publish_message(node, p.space, p.channel, body).await?;
    Ok(super::empty())
}

pub(crate) async fn delete(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageDelete>,
) -> R<m::MessageDelete> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::NONE)?;
    if ch.is_volatile() {
        return volatile::delete_message(node, ctx, p).await;
    }
    let me = me(node)?;
    let rec = node
        .store
        .get_message(&p.id, None)
        .map_err(Error::from)?
        .filter(|r| r.space == p.space && r.channel == p.channel)
        .ok_or_else(|| RpcError::not_found("없는 메시지"))?;
    if rec.deleted {
        return Ok(super::empty());
    }
    check_deletable(&state, &me.user(), &p.channel)?;
    if rec.author != me.user() {
        let perms = state.channel_permissions(&me.user(), &p.channel);
        if state.kind == SpaceKind::Space || !perms.contains(Permissions::MANAGE_MESSAGES) {
            return Err(RpcError::forbidden("남의 메시지를 지울 권한이 없습니다"));
        }
        let params = serde_json::to_value(&p).map_err(|e| RpcError::internal(e.to_string()))?;
        admin::confirm_when_others(node, ctx, "message.delete", &params).await?;
    }
    publish_message(node, p.space, p.channel, EventBody::Delete { target: p.id }).await?;
    Ok(super::empty())
}

/// 수정 금지 채널이면 거절합니다(받는 쪽 권한 검사와 같은 규칙).
pub(crate) fn check_editable(state: &SpaceState, channel: &ChannelId) -> Result<(), RpcError> {
    if state.edit_rules(channel).no_edit {
        return Err(RpcError::forbidden(
            "이 채널은 메시지 수정을 막아 두었습니다",
        ));
    }
    Ok(())
}

/// 삭제 금지 채널이면 운영 삭제(크루의 MANAGE_MESSAGES)만 됩니다(받는 쪽 권한 검사와 같은 규칙).
pub(crate) fn check_deletable(
    state: &SpaceState,
    me: &UserId,
    channel: &ChannelId,
) -> Result<(), RpcError> {
    let moderator = state.kind != SpaceKind::Space
        && state
            .channel_permissions(me, channel)
            .contains(Permissions::MANAGE_MESSAGES);
    if state.edit_rules(channel).no_delete && !moderator {
        return Err(RpcError::forbidden(
            "이 채널은 메시지 삭제를 막아 두었습니다",
        ));
    }
    Ok(())
}

async fn react_inner(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageReact>,
    present: bool,
) -> R<m::MessageReact> {
    let need = if present {
        Permissions::ADD_REACTIONS
    } else {
        Permissions::NONE
    };
    let (_, ch) = channel_ctx(node, &p.space, &p.channel, need)?;
    if ch.is_volatile() {
        return volatile::react(node, ctx, p, present).await;
    }
    let emoji = rpc(messages::parse_reaction(&p.emoji))?;
    node.store
        .get_message(&p.id, None)
        .map_err(Error::from)?
        .filter(|r| r.space == p.space && r.channel == p.channel && !r.deleted)
        .ok_or_else(|| RpcError::not_found("없는 메시지"))?;
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
    publish_message(node, p.space, p.channel, body).await?;
    Ok(super::empty())
}

pub(crate) async fn react(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageReact>,
) -> R<m::MessageReact> {
    react_inner(node, ctx, p, true).await
}

pub(crate) async fn unreact(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageUnreact>,
) -> R<m::MessageUnreact> {
    react_inner(node, ctx, p, false).await
}

pub(crate) async fn list(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageList>,
) -> R<m::MessageList> {
    let (_, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::NONE)?;
    // 신고하며 숨긴 메시지는 빼고(모더레이션), 뺀 만큼 더 오래된 쪽에서 채웁니다. 화면은 요청한 수보다 짧은
    // 페이지를 기록의 시작으로 보므로, 짧은 페이지는 정말 더 없을 때만 돌려줍니다.
    let limit = p.effective_limit();
    let any_hidden = crate::moderation::has_hidden(node, &p.space);
    if ch.is_volatile() {
        let space = p.space;
        let mut page = p.clone();
        let mut r = volatile::list_messages(node, ctx, page.clone()).await?;
        if any_hidden {
            let mut acc = Vec::new();
            loop {
                let full = r.messages.len() >= limit as usize;
                let oldest = r.messages.first().map(|m| m.id);
                let mut visible: Vec<_> = std::mem::take(&mut r.messages)
                    .into_iter()
                    .filter(|m| !crate::moderation::is_hidden(node, &space, &m.id))
                    .collect();
                visible.append(&mut acc);
                acc = visible;
                let Some(oldest) = oldest.filter(|_| full && acc.len() < limit as usize) else {
                    break;
                };
                page.before = Some(oldest);
                // 그사이 만료돼 위치를 잃으면 모은 것까지만 돌려줍니다.
                match volatile::list_messages(node, ctx, page.clone()).await {
                    Ok(next) => r = next,
                    Err(_) => break,
                }
            }
            let skip = acc.len().saturating_sub(limit as usize);
            acc.drain(..skip);
            r.messages = acc;
        }
        // 차단한 사용자·기기의 메시지는 표시만 합니다.
        for m in &mut r.messages {
            m.author_blocked =
                crate::moderation::is_author_blocked(node, &m.author, &m.author_device);
        }
        return Ok(r);
    }
    let me = me(node)?;
    let mut before = match &p.before {
        Some(b) => Some(
            messages::position_in_channel(node, b, &p.space, &p.channel)?
                .ok_or_else(|| RpcError::not_found("before 메시지가 이 채널에 없습니다"))?,
        ),
        None => None,
    };
    let start = before;
    // 새것부터 모읍니다.
    let mut recs = Vec::new();
    loop {
        let page = node
            .store
            .list_messages_for(&MessageQuery {
                space: p.space,
                channel: p.channel,
                thread: p.thread,
                before,
                limit,
                viewer: Some(me.user()),
            })
            .map_err(Error::from)?;
        let full = page.len() >= limit as usize;
        before = page.last().map(|r| (r.lamport, r.id));
        recs.extend(
            page.into_iter()
                .filter(|r| !any_hidden || !crate::moderation::is_hidden(node, &p.space, &r.id)),
        );
        if !any_hidden || !full || recs.len() >= limit as usize || before.is_none() {
            break;
        }
    }
    recs.truncate(limit as usize);
    // 포럼 글타래의 맨 앞(더 오래된 댓글이 없음)이면 게시글 자체를 첫 메시지로 넣습니다(글 본문·첨부가 보이게).
    if let Some(post) = p.thread
        && recs.len() < limit as usize
        && let Some(rec) = node
            .store
            .get_message(&post, Some(&me.user()))
            .map_err(Error::from)?
        && rec.kind == MessageKind::ForumPost
        && rec.space == p.space
        && rec.channel == p.channel
        && start.is_none_or(|b| (rec.lamport, rec.id) < b)
        && !(any_hidden && crate::moderation::is_hidden(node, &p.space, &rec.id))
    {
        recs.push(rec);
    }
    recs.reverse();
    let messages = recs.iter().map(|r| messages::view_of(node, r)).collect();
    let gaps = node
        .store
        .missing_details(&p.space, StreamId::Channel(p.channel))
        .map_err(Error::from)?
        .into_iter()
        .map(|m| GapView {
            after: None,
            before: Some(m.child),
        })
        .collect();
    policy::mark_channel_opened(node, &p.space, &p.channel);
    Ok(MessageListResult { messages, gaps })
}

/// 검색 쪽 크기 기본값·상한.
const SEARCH_PAGE: u32 = 50;
const SEARCH_PAGE_MAX: u32 = yangpa_store::MAX_PAGE;

/// 첫 검색은 일반 등급 기록 전체를 한 번 복호화해 색인을 만들고(기록 크기만큼 걸림), 그 뒤로도 맞은 글 수만큼 권한을
/// 확인하므로 블로킹 스레드에서 돕니다(비동기 작업자가 묶이지 않게).
pub(crate) async fn search(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageSearch>,
) -> R<m::MessageSearch> {
    let me = me(node)?.user();
    let (node, ctx) = (node.clone(), ctx.clone());
    tokio::task::spawn_blocking(move || search_blocking(&node, &ctx, &me, &p))
        .await
        .map_err(|e| RpcError::internal(format!("메시지 검색 작업 실패: {e}")))?
}

fn search_blocking(
    node: &Node,
    ctx: &CallCtx,
    me: &UserId,
    p: &MessageSearchParams,
) -> Result<MessageSearchPage, RpcError> {
    let started = Instant::now();
    let limit = p.limit.unwrap_or(SEARCH_PAGE).clamp(1, SEARCH_PAGE_MAX) as usize;
    let by_author = p.author.is_some();
    let filter = SearchFilter {
        space: p.space.as_ref(),
        channel: p.channel.as_ref(),
        author: p.author.as_ref(),
    };
    // (공간, 채널)마다 한 번만 판단합니다: 토큰 범위, 채널 보기 권한, 작성자로 찾을 때는 지금 익명인 채널도 뺌.
    let mut chans: Vec<Option<bool>> = Vec::new();
    let mut hidden: HashMap<SpaceId, bool> = HashMap::new();
    let page = node
        .store
        .search_messages(
            &p.query,
            filter,
            |h| {
                let c = h.chan as usize;
                if chans.len() <= c {
                    chans.resize(c + 1, None);
                }
                let visible = *chans[c].get_or_insert_with(|| {
                    ctx.allows_space(h.space)
                        && node.space_state(h.space).is_some_and(|st| {
                            st.channel_permissions(me, h.channel)
                                .contains(Permissions::VIEW_CHANNEL)
                                && !(by_author && st.is_anonymous(h.channel))
                        })
                });
                // 작성자로 찾을 때 익명 글을 넣으면 누가 썼는지 드러납니다.
                visible
                    && !(by_author && h.anonymous)
                    && !(*hidden
                        .entry(*h.space)
                        .or_insert_with(|| crate::moderation::has_hidden(node, h.space))
                        && crate::moderation::is_hidden(node, h.space, &h.id))
            },
            p.offset.unwrap_or(0) as usize,
            limit,
        )
        .map_err(Error::from)?;
    let messages = node
        .store
        .get_messages(&page.ids, None)
        .map_err(Error::from)?
        .iter()
        .map(|rec| {
            let mut v = messages::view_of(node, rec);
            v.space = Some(rec.space);
            v.channel = Some(rec.channel);
            v
        })
        .collect();
    Ok(MessageSearchPage {
        messages,
        total: u32::try_from(page.total).unwrap_or(u32::MAX),
        offset: u32::try_from(page.offset).unwrap_or(u32::MAX),
        took_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// 게시글 안 댓글: 같은 채널의 게시글이어야 하고, 잠긴 게시글이면 MANAGE_MESSAGES가 있어야 씁니다.
fn check_thread_open(
    node: &Node,
    state: &SpaceState,
    space: &SpaceId,
    channel: &ChannelId,
    post: &yangpa_core::ids::EventId,
) -> Result<(), RpcError> {
    let me = me(node)?;
    let Some(rec) = node
        .store
        .get_message(post, None)
        .map_err(Error::from)?
        .filter(|r| r.space == *space && r.channel == *channel)
    else {
        // 일반 채널의 답글 묶음은 대상이 아직 없을 수 있으므로 그대로 둡니다.
        return Ok(());
    };
    if rec.kind == MessageKind::ForumPost
        && rec.locked
        && !state
            .channel_permissions(&me.user(), channel)
            .contains(Permissions::MANAGE_MESSAGES)
    {
        return Err(RpcError::forbidden("잠긴 게시글에는 댓글을 쓸 수 없습니다"));
    }
    Ok(())
}

/// 포럼 채널이어야 합니다.
fn forum_channel(ch: &ChannelSettings) -> Result<(), RpcError> {
    if ch.kind == ChannelKind::Forum {
        Ok(())
    } else {
        Err(RpcError::invalid_input("포럼 채널이 아닙니다"))
    }
}

/// 게시글 태그: 채널에 있는 태그, 중복 없이 최대 5개.
fn check_post_tags(ch: &ChannelSettings, tags: &[u32]) -> Result<(), RpcError> {
    if tags.len() > MAX_POST_TAGS {
        return Err(RpcError::invalid_input(format!(
            "게시글 태그는 {MAX_POST_TAGS}개까지입니다"
        )));
    }
    for (i, t) in tags.iter().enumerate() {
        if tags[..i].contains(t) || !ch.forum_tags.iter().any(|ft| ft.id == *t) {
            return Err(RpcError::invalid_input("채널에 없는 태그이거나 중복입니다"));
        }
    }
    Ok(())
}

pub(crate) async fn forum_create_post(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ForumCreatePost>,
) -> R<m::ForumCreatePost> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::CREATE_POSTS)?;
    forum_channel(&ch)?;
    let title = rpc(crews::check_name(&p.title, "게시글 제목"))?;
    if p.text.trim().is_empty() && p.attachments.is_empty() {
        return Err(RpcError::invalid_input("빈 게시글입니다"));
    }
    check_post_tags(&ch, &p.tags)?;
    let perms = state.channel_permissions(&me(node)?.user(), &p.channel);
    // 받는 쪽도 같은 권한을 검사하므로, 첨부를 만들기 전에 거절합니다(남는 고정 블롭이 생기지 않게).
    if !p.attachments.is_empty() && !perms.contains(Permissions::ATTACH_FILES) {
        return Err(RpcError::forbidden("이 채널에 파일을 올릴 권한이 없습니다"));
    }
    if p.featured && !perms.contains(Permissions::MANAGE_CHANNELS) {
        return Err(RpcError::forbidden(
            "대문 게시글은 채널 관리 권한이 있어야 정할 수 있습니다",
        ));
    }
    drop(state);
    let attachments =
        blobs::prepare_attachments(node, &p.space, &p.channel, &p.attachments).await?;
    let embeds = if ch.link_previews && perms.contains(Permissions::EMBED_LINKS) {
        blobs::link_previews(node, &p.text).await
    } else {
        Vec::new()
    };
    let prepared: Vec<_> = attachments.iter().map(|a| a.blob).collect();
    let body = EventBody::ForumPost {
        title,
        tags: p.tags,
        first: MessageBody {
            text: p.text,
            attachments,
            embeds,
            reply_to: None,
            thread: None,
            mentions: Vec::new(),
        },
    };
    let r = publish_message(node, p.space, p.channel, body).await;
    let post = match r {
        Ok(r) => r,
        Err(e) => {
            // 올리지 못한 게시글의 첨부는 아무도 가리키지 않으니 지웁니다.
            blobs::discard_prepared(node, &prepared);
            return Err(e);
        }
    };
    if p.featured {
        // 게시글은 이미 올라갔으므로, 대문 지정이 실패해도 글 ID는 돌려주지 않을 수 없어 오류로 알립니다.
        publish_featured(node, p.space, p.channel, Some(post.id)).await?;
    }
    Ok(post)
}

/// 대문 상태 이벤트를 냅니다.
async fn publish_featured(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    post: Option<EventId>,
) -> Result<(), RpcError> {
    // 다른 상태 이벤트처럼 헤더에는 채널을 두지 않습니다(채널은 본문에 있음, 익명 표시도 붙지 않게).
    let body = EventBody::ForumFeaturedSet { channel, post };
    rpc(author::publish(node, space, None, &body, KeyChoice::State).await)?;
    Ok(())
}

/// 이 기기에 있는, 지우지 않은 그 채널의 게시글 행.
fn forum_post_record(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    post: &EventId,
    viewer: Option<&UserId>,
) -> Result<Option<MessageRecord>, RpcError> {
    Ok(node
        .store
        .get_message(post, viewer)
        .map_err(Error::from)?
        .filter(|r| {
            r.space == *space
                && r.channel == *channel
                && r.kind == MessageKind::ForumPost
                && !r.deleted
        }))
}

/// 보기에 실을 대문 게시글(상태에 있고 이 기기에 그 글이 있을 때만).
pub(crate) fn featured_post(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    state: &SpaceState,
) -> Option<EventId> {
    let post = state.forum_featured.get(channel)?;
    forum_post_record(node, space, channel, post, None)
        .ok()
        .flatten()
        .map(|r| r.id)
}

/// 저장된 게시글 행 → 목록 항목.
fn forum_post_view(
    node: &Node,
    rec: MessageRecord,
    me: &UserId,
    anonymous_now: bool,
    featured: Option<&EventId>,
) -> Option<ForumPostView> {
    let forum = rec.content.as_ref()?.forum.clone()?;
    let msg = messages::view_of(node, &rec);
    let anonymous = rec.anonymous || anonymous_now;
    let cover = msg
        .attachments
        .iter()
        .find(|a| a.mime.starts_with("image/") || a.mime.starts_with("video/"))
        .cloned();
    Some(ForumPostView {
        id: rec.id,
        title: forum.title,
        tags: forum.tags,
        author: if anonymous {
            ANONYMOUS_USER
        } else {
            rec.author
        },
        created_at: rec.created_at,
        last_activity: rec.last_activity,
        reply_count: rec.reply_count,
        pinned: rec.pinned,
        locked: rec.locked,
        anonymous,
        anon_name: anonymous.then(|| crate::anon::name(&rec.space, &rec.channel, &rec.author)),
        mine: rec.author == *me,
        featured: featured == Some(&rec.id),
        edited_at: rec.edited_at,
        preview: msg.text.chars().take(FORUM_PREVIEW_CHARS).collect(),
        attachment_count: msg.attachments.len() as u32,
        cover,
    })
}

pub(crate) async fn forum_list_posts(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ForumListPosts>,
) -> R<m::ForumListPosts> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::NONE)?;
    forum_channel(&ch)?;
    let me = me(node)?;
    let anonymous_now = state.is_anonymous(&p.channel);
    let sort = match p.sort {
        ForumSort::Activity => yangpa_store::ForumSort::Activity,
        ForumSort::Created => yangpa_store::ForumSort::Created,
    };
    let mut tags = p.tags.clone();
    if let Some(t) = p.tag
        && !tags.contains(&t)
    {
        tags.push(t);
    }
    // 태그로 거르면 걸러질 글까지 더 읽어 둡니다(돌려주는 것은 200개까지).
    let fetch = if tags.is_empty() {
        FORUM_LIST_LIMIT
    } else {
        yangpa_store::MAX_PAGE
    };
    let recs = node
        .store
        .list_forum_posts(
            &p.space,
            &p.channel,
            sort,
            p.order == SortOrder::Asc,
            fetch,
            Some(&me.user()),
        )
        .map_err(Error::from)?;
    policy::mark_channel_opened(node, &p.space, &p.channel);
    let featured = state.forum_featured.get(&p.channel);
    Ok(recs
        .into_iter()
        .filter(|r| !r.deleted)
        .filter(|r| {
            let Some(forum) = r.content.as_ref().and_then(|c| c.forum.as_ref()) else {
                return false;
            };
            match p.tag_match {
                _ if tags.is_empty() => true,
                TagMatch::Any => tags.iter().any(|t| forum.tags.contains(t)),
                // 글의 태그가 고른 태그와 꼭 같아야 합니다(태그 하나만 골랐을 때도 일부 일치와 달라지게).
                TagMatch::All => {
                    tags.iter().all(|t| forum.tags.contains(t))
                        && forum.tags.iter().all(|t| tags.contains(t))
                }
            }
        })
        .take(FORUM_LIST_LIMIT as usize)
        .filter_map(|r| forum_post_view(node, r, &me.user(), anonymous_now, featured))
        .collect())
}

/// `forum.list_posts`가 돌려주는 게시글 수.
const FORUM_LIST_LIMIT: u32 = 200;

pub(crate) async fn forum_get_post(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ForumGetPost>,
) -> R<m::ForumGetPost> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::NONE)?;
    forum_channel(&ch)?;
    let me = me(node)?;
    let rec = forum_post_record(node, &p.space, &p.channel, &p.post, Some(&me.user()))?
        .ok_or_else(|| RpcError::not_found("없는 게시글"))?;
    forum_post_view(
        node,
        rec,
        &me.user(),
        state.is_anonymous(&p.channel),
        state.forum_featured.get(&p.channel),
    )
    .ok_or_else(|| RpcError::not_found("없는 게시글"))
}

pub(crate) async fn forum_set_featured(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ForumSetFeatured>,
) -> R<m::ForumSetFeatured> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::MANAGE_CHANNELS)?;
    forum_channel(&ch)?;
    if let Some(post) = &p.post
        && forum_post_record(node, &p.space, &p.channel, post, None)?.is_none()
    {
        return Err(RpcError::not_found("없는 게시글"));
    }
    if state.forum_featured.get(&p.channel) == p.post.as_ref() {
        return Ok(super::empty());
    }
    drop(state);
    publish_featured(node, p.space, p.channel, p.post).await?;
    Ok(super::empty())
}

pub(crate) async fn forum_update_post(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ForumUpdatePost>,
) -> R<m::ForumUpdatePost> {
    let (state, ch) = channel_ctx(node, &p.space, &p.channel, Permissions::NONE)?;
    forum_channel(&ch)?;
    if ch.is_volatile() {
        // 변경은 상태 레인으로 디스크에 남습니다.
        return Err(RpcError::invalid_input(
            "휘발성 채널의 게시글은 바꿀 수 없습니다",
        ));
    }
    let me = me(node)?;
    let rec = node
        .store
        .get_message(&p.post, None)
        .map_err(Error::from)?
        .filter(|r| {
            r.space == p.space && r.channel == p.channel && r.kind == MessageKind::ForumPost
        })
        .ok_or_else(|| RpcError::not_found("없는 게시글"))?;
    if rec.deleted {
        return Err(RpcError::conflict("삭제된 게시글입니다"));
    }
    if p.title.is_none() && p.tags.is_none() && p.pinned.is_none() && p.locked.is_none() {
        return Err(RpcError::invalid_input("바꿀 항목이 없습니다"));
    }
    let perms = state.channel_permissions(&me.user(), &p.channel);
    let manage = perms.contains(Permissions::MANAGE_MESSAGES);
    if (p.pinned.is_some() || p.locked.is_some()) && !manage {
        return Err(RpcError::forbidden(
            "게시글 고정·잠금에는 메시지 관리 권한이 필요합니다",
        ));
    }
    let title = match &p.title {
        Some(t) => Some(rpc(crews::check_name(t, "게시글 제목"))?),
        None => None,
    };
    if let Some(tags) = &p.tags {
        check_post_tags(&ch, tags)?;
    }
    if title.is_some() || p.tags.is_some() {
        if rec.author != me.user() && !manage {
            return Err(RpcError::forbidden(
                "남의 게시글 제목·태그는 메시지 관리 권한이 있어야 바꿀 수 있습니다",
            ));
        }
        if !manage {
            check_editable(&state, &p.channel)?;
        }
        // 변경은 상태 레인(멤버 전원이 받는 상태 키)으로 가므로, 볼 수 없는 멤버가 있는 채널의 제목·태그를 실으면
        // 그 멤버에게 드러납니다.
        if crews::needs_channel_group(&state, &p.channel, &ch) {
            return Err(RpcError::invalid_input(
                "볼 수 없는 멤버가 있는 포럼 채널의 제목·태그는 게시 뒤 바꿀 수 없습니다",
            ));
        }
    }
    let body = EventBody::ForumPostUpdate {
        post: p.post,
        title,
        tags: p.tags,
        pinned: p.pinned,
        locked: p.locked,
    };
    rpc(author::publish(node, p.space, Some(p.channel), &body, KeyChoice::State).await)?;
    Ok(super::empty())
}
