//! 메시지·키 레인 이벤트의 권한 검사(해소된 상태 기준)와 공용 텍스트 검사.

use yangpa_core::body::{
    EventBody, KeyShareBody, LinkEmbed, MessageBody, ReactionEmoji, SharedKey,
};
use yangpa_core::emoji;
use yangpa_core::event::{EventHeader, KeyRef, Lane, Scope};
use yangpa_core::ids::{ChannelId, KeyGroupId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, ChannelSettings, Permissions, SpaceKind, SpaceState};

use super::AuthError;
use super::limits::*;

/// 메시지·키 레인 이벤트(그리고 상태 레인의 `ForumPostUpdate`)를 지금 상태로 검사합니다.
///
/// - 작성자는 멤버이고 작성 기기는 그 사용자의 활성 기기여야 합니다.
/// - 메시지 레인: 채널이 있어야 하고 VIEW_CHANNEL이 필요합니다. 일반 채널은 기록 키(`KeyRef::Record`), 휘발성
///   채널은 세션 키로 암호화해야 합니다. `Message`는 SEND_MESSAGES(첨부가 있으면 ATTACH_FILES), `ForumPost`는
///   CREATE_POSTS, `Reaction`은 ADD_REACTIONS가 필요합니다. 링크 미리보기(`embeds`)를 실은 `Message`·
///   `ForumPost`·`Edit`은 EMBED_LINKS도 필요합니다.
/// - `Edit`은 자기 메시지만, `Delete`는 자기 메시지 또는 MANAGE_MESSAGES. 수정 금지 채널(`edit_rules`)에서는
///   `Edit`을 받지 않고, 삭제 금지 채널에서는 MANAGE_MESSAGES의 `Delete`만 받습니다. `target_author`는 대상
///   메시지의 작성자이며, 모르면 [`AuthError::UnknownTarget`]을 돌려줘 데몬이 대상을 받은 뒤 다시 검사합니다.
///   1대1 스페이스에서는 두 친구가 동등하므로 소유자라도 상대 메시지를 지울 수 없습니다.
/// - 키 레인(`KeyShare`): 멤버여야 하고, 채널 키면 그 채널을 볼 수 있어야 합니다. 상태 키·크루 그룹 R 배포는
///   확정 기기·소유자·ADMINISTRATOR만 할 수 있습니다(가짜 상태 키로 다른 노드의 키를 덮어쓰는 것 방지).
///   비공개 그룹 R은 그 그룹 자격이 있는 사람만 나누고 받습니다.
///   키 묶음은 봉인 상자나 상태 키로만 암호화하고, 휘발성 세션 키는 그 휘발성 채널 스트림(RAM 전용)에서
///   봉인 상자로만 나눠야 합니다(디스크로 가는 공간 스트림에 실리지 않게).
pub fn authorize_message(
    state: &SpaceState,
    header: &EventHeader,
    body: &EventBody,
    target_author: Option<UserId>,
) -> Result<(), AuthError> {
    let active = state.device_belongs(&header.user, &header.author);
    authorize_written(state, header, body, target_author, active)
}

/// 폐기된 기기가 쓴 메시지: 쓴 기기가 지금 활성이어야 한다는 것만 빼고 [`authorize_message`]와 같습니다. 받을지는 데몬이
/// 먼저 정합니다(그 사람의 살아 있는 기기가 내줄 때만).
pub fn authorize_revoked_device_message(
    state: &SpaceState,
    header: &EventHeader,
    body: &EventBody,
    target_author: Option<UserId>,
) -> Result<(), AuthError> {
    authorize_written(state, header, body, target_author, true)
}

fn authorize_written(
    state: &SpaceState,
    header: &EventHeader,
    body: &EventBody,
    target_author: Option<UserId>,
    device_ok: bool,
) -> Result<(), AuthError> {
    if !body.fits(header.kind) {
        return Err(AuthError::KindMismatch);
    }
    let user = header.user;
    if !state.is_member(&user) {
        return Err(AuthError::NotMember);
    }
    if !device_ok {
        return Err(AuthError::UnknownDevice);
    }
    match body {
        EventBody::KeyShare(ks) => return authorize_key_share(state, header, ks),
        EventBody::VolatileRotation(_) => return authorize_volatile_rotation(state, header),
        EventBody::ForumPostUpdate { .. } => {
            return check_forum_update(state, header, body, Some(target_author));
        }
        _ => {}
    }
    if header.kind.lane() != Lane::Message {
        return Err(AuthError::WrongLane);
    }
    let channel_id = header.channel.ok_or(AuthError::Invalid("채널 없음"))?;
    let ch = state
        .channels
        .get(&channel_id)
        .ok_or(AuthError::UnknownChannel)?;
    let perms = state.channel_permissions(&user, &channel_id);
    need(perms, Permissions::VIEW_CHANNEL)?;
    if ch.kind == ChannelKind::Category {
        return Err(AuthError::Invalid("카테고리에는 메시지를 쓸 수 없음"));
    }
    // 휘발성 채널 내용이 기록 키(디스크 보관 키)로 암호화되는 실수를 막습니다.
    let key_ok = match header.key {
        KeyRef::Volatile { .. } => ch.is_volatile(),
        KeyRef::Record { .. } => !ch.is_volatile(),
        _ => false,
    };
    if !key_ok {
        return Err(AuthError::WrongKey);
    }

    match body {
        EventBody::Message(m) => {
            need(perms, Permissions::SEND_MESSAGES)?;
            check_message_body(m)?;
            if !m.attachments.is_empty() {
                need(perms, Permissions::ATTACH_FILES)?;
            }
            if !m.embeds.is_empty() {
                need(perms, Permissions::EMBED_LINKS)?;
            }
            if ch.kind == ChannelKind::Forum && m.thread.is_none() {
                return Err(AuthError::Invalid("포럼 채널의 메시지는 게시글 안에만"));
            }
            Ok(())
        }
        EventBody::ForumPost { title, tags, first } => {
            if ch.kind != ChannelKind::Forum {
                return Err(AuthError::Invalid("포럼 채널이 아님"));
            }
            need(perms, Permissions::CREATE_POSTS)?;
            check_name(title, MAX_NAME_CHARS, "게시글 제목")?;
            check_post_tags(ch, tags)?;
            check_message_body(first)?;
            if first.thread.is_some() {
                return Err(AuthError::Invalid("게시글 첫 메시지에 상위 글"));
            }
            if !first.attachments.is_empty() {
                need(perms, Permissions::ATTACH_FILES)?;
            }
            if !first.embeds.is_empty() {
                need(perms, Permissions::EMBED_LINKS)?;
            }
            Ok(())
        }
        EventBody::Edit { text, embeds, .. } => {
            if state.edit_rules(&channel_id).no_edit {
                return Err(AuthError::Invalid("수정 금지 채널"));
            }
            check_free_text(text, MAX_MESSAGE_CHARS, "메시지")?;
            check_embeds(embeds)?;
            if !embeds.is_empty() {
                need(perms, Permissions::EMBED_LINKS)?;
            }
            match target_author {
                None => Err(AuthError::UnknownTarget),
                Some(a) if a == user => Ok(()),
                Some(_) => Err(AuthError::NotAuthor),
            }
        }
        EventBody::Delete { .. } => {
            if perms.contains(Permissions::MANAGE_MESSAGES) && state.kind != SpaceKind::Space {
                return Ok(());
            }
            // 삭제 금지 채널에서는 운영 삭제(MANAGE_MESSAGES)만 됩니다.
            if state.edit_rules(&channel_id).no_delete {
                return Err(AuthError::Invalid("삭제 금지 채널"));
            }
            match target_author {
                None => Err(AuthError::UnknownTarget),
                Some(a) if a == user => Ok(()),
                Some(_) => Err(AuthError::MissingPermission(Permissions::MANAGE_MESSAGES)),
            }
        }
        EventBody::Reaction { emoji, .. } => {
            need(perms, Permissions::ADD_REACTIONS)?;
            check_reaction_emoji(state, perms, emoji)
        }
        // 반응은 사용자별이라 자기 반응을 지우는 데는 보기 권한이면 충분합니다.
        EventBody::ReactionRemove { emoji, .. } => check_reaction_shape(emoji),
        _ => Err(AuthError::WrongLane),
    }
}

fn need(perms: Permissions, p: Permissions) -> Result<(), AuthError> {
    if perms.contains(p) {
        Ok(())
    } else {
        Err(AuthError::MissingPermission(p))
    }
}

/// 이 작성자가 상태 키를 나눌 권한(확정 기기·소유자·관리자)이 없어, 상태 키 묶음이 확정 기기 인계 예외로만 통과하는지.
/// 그렇게 받은 키는 저장만 하고 지금 상태 키로 쓰지 않습니다(멤버가 지어낸 새 키로 상태를 쓰게 하지 않음).
pub fn state_key_handover_only(state: &SpaceState, header: &EventHeader) -> bool {
    !(header.author == state.committer
        || header.user == state.owner
        || state
            .base_permissions(&header.user)
            .contains(Permissions::ADMINISTRATOR))
}

/// 부분 전달 이벤트(받은편지함·키 그룹)를 주어진 상태로 검사합니다.
///
/// 이 스트림들은 해소기에 들어가지 않아 노드마다 상태가 갈릴 일은 없지만, 보낼 자격이 없는 기기(확정 기기가 아닌
/// 멤버 등)가 보낸 키를 받아들이면 안 됩니다. 받는 데몬은 이것을 기준점 상태([`super::Resolver::state_at`], 어느
/// 노드에서나 같은 답)와 자기 지금 상태 둘 다로 부릅니다. 기준점은 작성자가 고르므로 기준점 상태만 보면 강등된
/// 관리자·바뀐 확정 기기가 옛 기준점으로 그때 권한을 되살립니다. 가입·기기 연결 중 믿는 피어에게서 받은 것만 기준점
/// 상태로만 봅니다(protocol.md "부분 전달 스트림").
///
/// - 작성자는 멤버이고 작성 기기는 그 사용자의 활성 기기.
/// - 받은편지함(u): u가 멤버. 작성 기기가 확정 기기이거나 작성자가 소유자·관리자(키를 나눌 수 있음)이거나, u가 확정
///   기기의 주인(확정 기기 인계: 멤버 누구나 확정 기기의 사용자에게 키를 넘김).
/// - 키 그룹(g): 작성 기기가 확정 기기이고 그 상태에 g를 쓰는 채널이 있음.
///
/// 본문(키 묶음) 검사는 받는 사람이 열어 본 뒤 같은 상태로 [`authorize_message`]를 따로 부릅니다.
pub fn authorize_scoped(
    state: &SpaceState,
    space: &SpaceId,
    header: &EventHeader,
) -> Result<(), AuthError> {
    let Some(scope) = header.scope else {
        return Err(AuthError::WrongLane);
    };
    if header.space != *space {
        return Err(AuthError::WrongSpace);
    }
    let user = header.user;
    if !state.is_member(&user) {
        return Err(AuthError::NotMember);
    }
    if !state.device_belongs(&user, &header.author) {
        return Err(AuthError::UnknownDevice);
    }
    match scope {
        Scope::Inbox(to) => {
            if !state.is_member(&to) {
                return Err(AuthError::UnknownMember);
            }
            let handover = state
                .devices
                .get(&to)
                .is_some_and(|l| l.contains_active(&state.committer));
            if !handover && state_key_handover_only(state, header) {
                return Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR));
            }
            Ok(())
        }
        Scope::KeyGroup(g) => {
            if header.author != state.committer {
                return Err(AuthError::NotCommitter);
            }
            if !state.has_key_group(space, &g) {
                return Err(AuthError::UnknownChannel);
            }
            Ok(())
        }
    }
}

/// 휘발성 세션 키 회전 공지: 그 휘발성 채널을 볼 수 있는 멤버가, 공지 키(세대 0)로만. 지금 참가자인지는 데몬이 봅니다.
fn authorize_volatile_rotation(state: &SpaceState, header: &EventHeader) -> Result<(), AuthError> {
    if header.key != (KeyRef::Volatile { generation: 0 }) {
        return Err(AuthError::WrongKey);
    }
    let c = header.channel.ok_or(AuthError::Invalid("채널 없음"))?;
    let ch = state.channels.get(&c).ok_or(AuthError::UnknownChannel)?;
    if !ch.is_volatile() {
        return Err(AuthError::WrongKey);
    }
    need(
        state.channel_permissions(&header.user, &c),
        Permissions::VIEW_CHANNEL,
    )
}

fn authorize_key_share(
    state: &SpaceState,
    header: &EventHeader,
    ks: &KeyShareBody,
) -> Result<(), AuthError> {
    let user = header.user;
    // protocol.md: 키 묶음은 봉인 상자나 상태 키로만 암호화합니다.
    if !matches!(header.key, KeyRef::Sealed | KeyRef::State(_)) {
        return Err(AuthError::WrongKey);
    }
    if ks.keys.is_empty() {
        return Err(AuthError::Invalid("빈 키 묶음"));
    }
    if ks.keys.len() > MAX_KEYS_PER_SHARE {
        return Err(AuthError::Limit("키 개수"));
    }
    let can_view = |c: &ChannelId| {
        state.channels.contains_key(c)
            && state
                .channel_permissions(&user, c)
                .contains(Permissions::VIEW_CHANNEL)
    };
    if let Some(c) = &header.channel {
        if !state.channels.contains_key(c) {
            return Err(AuthError::UnknownChannel);
        }
        if !can_view(c) {
            return Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL));
        }
    }
    for k in &ks.keys {
        match k {
            SharedKey::State { .. } => {
                // 상태 키는 공간 스트림으로만, 그리고 확정 기기·소유자·관리자만 배포합니다. 예외로 멤버 누구나 확정
                // 기기의 사용자에게 봉인해 넘길 수 있습니다(확정 기기가 바뀌었는데 새 확정 기기가 아직 공간 그룹에 없을
                // 때의 인계, 그 사용자는 소유자·관리자라 이미 모든 키를 받을 자격이 있음). 어느 경우든 ID가 키에서 나온
                // 것이어야 해서([`yangpa_core::body::state_key_id`]) 쓰는 ID에 다른 키를 끼워 넣지 못하고, 인계로만 받은
                // 키는 받는 쪽이 지금 키로 쓰지 않습니다([`state_key_handover_only`]).
                if header.channel.is_some() {
                    return Err(AuthError::Invalid("채널 키 묶음에 상태 키"));
                }
                if !k.bound_to(&header.space) {
                    return Err(AuthError::Invalid("상태 키 ID가 키와 맞지 않음"));
                }
                let handover = header.key == KeyRef::Sealed
                    && ks.for_user.is_some_and(|u| {
                        state
                            .devices
                            .get(&u)
                            .is_some_and(|l| l.contains_active(&state.committer))
                    });
                if !handover && state_key_handover_only(state, header) {
                    return Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR));
                }
            }
            SharedKey::Root { group, .. } => {
                // 그룹 R은 공간 수준 키입니다. ID가 키에서 나와야 해서(`yangpa_core::body::group_root_id`) 쓰는 ID에 다른
                // 키를 끼워 넣지 못합니다.
                if header.channel.is_some() {
                    return Err(AuthError::Invalid("채널 키 묶음에 그룹 키"));
                }
                if !k.bound_to(&header.space) {
                    return Err(AuthError::Invalid("그룹 키 ID가 키와 맞지 않음"));
                }
                if *group == KeyGroupId::crew(&header.space) {
                    // 크루 R에서 상태 키가 나오므로 상태 키와 같은 규칙입니다.
                    let handover = header.key == KeyRef::Sealed
                        && ks.for_user.is_some_and(|u| {
                            state
                                .devices
                                .get(&u)
                                .is_some_and(|l| l.contains_active(&state.committer))
                        });
                    if !handover && state_key_handover_only(state, header) {
                        return Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR));
                    }
                } else {
                    if !state.has_key_group(&header.space, group) {
                        return Err(AuthError::UnknownChannel);
                    }
                    // 비공개 그룹 R은 그 그룹 채널을 볼 수 있는 사람끼리만(받는 사람이 정해져 있으면 그 사람도).
                    let eligible = |u: &UserId| state.can_view_key_group(&header.space, group, u);
                    if !eligible(&user) || ks.for_user.is_some_and(|u| !eligible(&u)) {
                        return Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL));
                    }
                }
            }
            SharedKey::Bundle { group, bundle, .. } => {
                // 기기 몫 묶음은 받는 기기에게만 봉인한 공간 수준 묶음에만 싣습니다. 안의 봉투는 받는 기기만 열고, 받는
                // 쪽이 갱신의 묶음 뿌리 증명으로 확인하므로 작성자 자격은 묻지 않습니다.
                if header.channel.is_some() || header.key != KeyRef::Sealed {
                    return Err(AuthError::WrongKey);
                }
                if bundle.len() > yangpa_core::body::MAX_SHARED_BUNDLE_LEN {
                    return Err(AuthError::Limit("봉인 묶음 크기"));
                }
                if *group != KeyGroupId::crew(&header.space)
                    && !state.has_key_group(&header.space, group)
                {
                    return Err(AuthError::UnknownChannel);
                }
            }
            SharedKey::ChannelRoot { channel, .. } | SharedKey::Volatile { channel, .. } => {
                if header.channel.is_some_and(|c| c != *channel) {
                    return Err(AuthError::Invalid("다른 채널의 키"));
                }
                if matches!(k, SharedKey::ChannelRoot { .. }) && !k.bound_to(&header.space) {
                    return Err(AuthError::Invalid("채널 뿌리 ID가 키와 맞지 않음"));
                }
                // 휘발성 세션 키는 그 채널의 RAM 전용 스트림에서 참가자 기기에게 봉인해서만 나눕니다.
                if matches!(k, SharedKey::Volatile { .. })
                    && (header.channel != Some(*channel) || header.key != KeyRef::Sealed)
                {
                    return Err(AuthError::WrongKey);
                }
                if !state.channels.contains_key(channel) {
                    return Err(AuthError::UnknownChannel);
                }
                if !can_view(channel) {
                    return Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL));
                }
                let volatile_key = matches!(k, SharedKey::Volatile { .. });
                if state
                    .channels
                    .get(channel)
                    .is_some_and(|ch| ch.is_volatile() != volatile_key)
                {
                    return Err(AuthError::WrongKey);
                }
            }
        }
    }
    Ok(())
}

/// 포럼 게시글 갱신 검사. `owner_check`가 `None`이면(상태 해소 중) 게시글 작성자 확인을 건너뛰고,
/// `Some(target_author)`이면 제목·태그 변경에 작성자 본인 또는 MANAGE_MESSAGES를 요구합니다.
pub(super) fn check_forum_update(
    state: &SpaceState,
    header: &EventHeader,
    body: &EventBody,
    owner_check: Option<Option<UserId>>,
) -> Result<(), AuthError> {
    let EventBody::ForumPostUpdate {
        title,
        tags,
        pinned,
        locked,
        ..
    } = body
    else {
        return Err(AuthError::KindMismatch);
    };
    let user = header.user;
    let channel_id = header.channel.ok_or(AuthError::Invalid("채널 없음"))?;
    let ch = state
        .channels
        .get(&channel_id)
        .ok_or(AuthError::UnknownChannel)?;
    if ch.kind != ChannelKind::Forum {
        return Err(AuthError::Invalid("포럼 채널이 아님"));
    }
    let perms = state.channel_permissions(&user, &channel_id);
    need(perms, Permissions::VIEW_CHANNEL)?;
    if pinned.is_some() || locked.is_some() {
        need(perms, Permissions::MANAGE_MESSAGES)?;
    }
    if let Some(t) = title {
        check_name(t, MAX_NAME_CHARS, "게시글 제목")?;
    }
    if let Some(t) = tags {
        check_post_tags(ch, t)?;
    }
    if (title.is_some() || tags.is_some()) && !perms.contains(Permissions::MANAGE_MESSAGES) {
        // 수정 금지 채널에서는 운영자만 게시글 제목·태그를 고칩니다.
        if state.edit_rules(&channel_id).no_edit {
            return Err(AuthError::Invalid("수정 금지 채널"));
        }
        match owner_check {
            None => {}
            Some(None) => return Err(AuthError::UnknownTarget),
            Some(Some(a)) if a == user => {}
            Some(Some(_)) => return Err(AuthError::NotAuthor),
        }
    }
    if title.is_none() && tags.is_none() && pinned.is_none() && locked.is_none() {
        return Err(AuthError::Invalid("바꿀 내용 없음"));
    }
    Ok(())
}

fn check_post_tags(ch: &ChannelSettings, tags: &[u32]) -> Result<(), AuthError> {
    if tags.len() > MAX_POST_TAGS {
        return Err(AuthError::Limit("게시글 태그 수"));
    }
    for (i, t) in tags.iter().enumerate() {
        if tags[..i].contains(t) || !ch.forum_tags.iter().any(|ft| ft.id == *t) {
            return Err(AuthError::Invalid("게시글 태그"));
        }
    }
    Ok(())
}

fn check_message_body(m: &MessageBody) -> Result<(), AuthError> {
    check_free_text(&m.text, MAX_MESSAGE_CHARS, "메시지")?;
    if m.text.trim().is_empty() && m.attachments.is_empty() {
        return Err(AuthError::Invalid("빈 메시지"));
    }
    if m.attachments.len() > MAX_ATTACHMENTS {
        return Err(AuthError::Limit("첨부 수"));
    }
    for a in &m.attachments {
        check_name(&a.name, MAX_FILE_NAME_CHARS, "첨부 이름")?;
        check_name(&a.mime, MAX_MIME_CHARS, "첨부 형식")?;
        if a.thumbnail.len() > MAX_THUMBNAIL_BYTES {
            return Err(AuthError::Limit("썸네일 크기"));
        }
    }
    check_embeds(&m.embeds)?;
    if m.mentions.len() > MAX_MENTIONS {
        return Err(AuthError::Limit("멘션 수"));
    }
    Ok(())
}

fn check_embeds(embeds: &[LinkEmbed]) -> Result<(), AuthError> {
    if embeds.len() > MAX_EMBEDS {
        return Err(AuthError::Limit("링크 미리보기 수"));
    }
    for e in embeds {
        check_name(&e.url, MAX_URL_CHARS, "링크 주소")?;
        check_free_text(&e.title, MAX_NAME_CHARS * 3, "링크 제목")?;
        check_free_text(&e.description, MAX_EMBED_TEXT_CHARS, "링크 설명")?;
        check_free_text(&e.site_name, MAX_NAME_CHARS, "사이트 이름")?;
        if let Some(p) = &e.player_url {
            check_name(p, MAX_URL_CHARS, "플레이어 주소")?;
        }
        if e.thumbnail.len() > MAX_THUMBNAIL_BYTES {
            return Err(AuthError::Limit("썸네일 크기"));
        }
    }
    Ok(())
}

fn check_reaction_shape(e: &ReactionEmoji) -> Result<(), AuthError> {
    match e {
        ReactionEmoji::Unicode { value } => {
            if value.is_empty()
                || value.len() > MAX_UNICODE_EMOJI_BYTES
                || value.chars().any(char::is_control)
            {
                return Err(AuthError::Invalid("이모지"));
            }
        }
        ReactionEmoji::Custom { name, .. } => {
            if !emoji::valid_name(name) {
                return Err(AuthError::Invalid("이모지 이름"));
            }
        }
    }
    Ok(())
}

/// 다른 공간의 커스텀 이모지는 공간 설정이 켜져 있고 USE_EXTERNAL_EMOJI가 있을 때만 씁니다.
fn check_reaction_emoji(
    state: &SpaceState,
    perms: Permissions,
    e: &ReactionEmoji,
) -> Result<(), AuthError> {
    check_reaction_shape(e)?;
    if let ReactionEmoji::Custom { id, .. } = e
        && !state.emojis.contains_key(id)
    {
        if !state.settings.allow_external_emoji {
            return Err(AuthError::ExternalEmojiDisabled);
        }
        need(perms, Permissions::USE_EXTERNAL_EMOJI)?;
    }
    Ok(())
}

/// 이름류: 1~max자, 공백만은 안 되고, 제어 문자 금지.
pub(super) fn check_name(s: &str, max: usize, what: &'static str) -> Result<(), AuthError> {
    let mut n = 0usize;
    for c in s.chars() {
        n += 1;
        if n > max || c.is_control() {
            return Err(AuthError::Invalid(what));
        }
    }
    if n == 0 || s.trim().is_empty() {
        return Err(AuthError::Invalid(what));
    }
    Ok(())
}

/// 자유 텍스트: max자 이하. 줄바꿈·탭 말고 제어 문자 금지.
pub(super) fn check_free_text(s: &str, max: usize, what: &'static str) -> Result<(), AuthError> {
    let mut n = 0usize;
    for c in s.chars() {
        n += 1;
        if n > max || (c.is_control() && c != '\n' && c != '\t') {
            return Err(AuthError::Limit(what));
        }
    }
    Ok(())
}
