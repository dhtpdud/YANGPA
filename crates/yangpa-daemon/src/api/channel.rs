//! `channel.create`/`update`/`delete`(비공개 채널 포함). 채널 목록·읽음 표시는 코어(`api/space.rs`).
//!
//! 비공개 채널(`private_roles`)은 @everyone의 VIEW_CHANNEL 거부와 그 역할들의 VIEW_CHANNEL 허용 덮어쓰기입니다.
//! 비공개 채널의 기록 키는 그 채널 열람 규칙의 비공개 키 그룹(볼 수 있는 멤버의 기기만)에서 나옵니다(`keygroup`·`messages`).
//! 볼 수 있는 사람을 바꾸는 `channel.update`는 자동화 토큰이면 사람 확인을 거칩니다(`admin::confirm_when_others`).
//! P2P 전용 채널(`p2p: true`)과 `channel.p2p_join`·`p2p_leave`의 실제 일은 `crate::p2p`가 합니다.

use std::sync::Arc;

use yangpa_core::body::EventBody;
use yangpa_core::ids::{ChannelId, RoleId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, ChatTtl, EditRules, ForumTag, HistoryVisibility, JoinMode,
    MAX_VOLATILE_CAP, MIN_ATTACH_LIMIT, MIN_VOLATILE_CAP, OverwriteTarget, PermissionOverwrite,
    Permissions, SecurityLevel, SpaceKind, SpaceState, valid_attach_limit,
};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{ChannelRef, m};
use yangpa_sync::state::limits::{MAX_FORUM_TAGS, MAX_NAME_CHARS, MAX_OVERWRITES, MAX_TOPIC_CHARS};

use super::{CallCtx, P, R, empty, rpc};
use crate::author::{self, KeyChoice};
use crate::node::Node;
use crate::{admin, crews, ingest, p2p, sync_service};

/// 채팅 수명 인자(`ttl_secs`·`ttl_on_leave`)를 지금 값에 반영합니다. `ttl_secs: 0`은 수명 없음.
fn ttl_of(cur: ChatTtl, secs: Option<u32>, on_leave: Option<bool>) -> Result<ChatTtl, RpcError> {
    match (secs, on_leave) {
        (Some(s), Some(true)) if s > 0 => Err(RpcError::invalid_input(
            "ttl_secs와 ttl_on_leave를 함께 쓸 수 없습니다",
        )),
        (_, Some(true)) => Ok(ChatTtl::OnLeave),
        (Some(0), _) => Ok(ChatTtl::None),
        (Some(s), _) => Ok(ChatTtl::Seconds(s)),
        (None, Some(false)) if cur == ChatTtl::OnLeave => Ok(ChatTtl::None),
        (None, _) => Ok(cur),
    }
}

fn overwrite_entry(
    v: &mut Vec<PermissionOverwrite>,
    t: OverwriteTarget,
) -> &mut PermissionOverwrite {
    let i = match v.iter().position(|o| o.target == t) {
        Some(i) => i,
        None => {
            v.push(PermissionOverwrite {
                target: t,
                allow: Permissions::NONE,
                deny: Permissions::NONE,
            });
            v.len() - 1
        }
    };
    &mut v[i]
}

/// 역할 덮어쓰기의 VIEW_CHANNEL 비트만 다시 정합니다. 빈 목록이면 공개 채널. 다른 권한 덮어쓰기는 그대로 둡니다.
fn set_private_roles(
    state: &SpaceState,
    overwrites: &mut Vec<PermissionOverwrite>,
    roles: &[RoleId],
) -> Result<(), RpcError> {
    if roles
        .iter()
        .any(|r| *r == RoleId::EVERYONE || !state.roles.contains_key(r))
    {
        return Err(RpcError::invalid_input("없는 역할입니다"));
    }
    for o in overwrites.iter_mut() {
        if matches!(o.target, OverwriteTarget::Role(_)) {
            o.allow = o.allow.remove(Permissions::VIEW_CHANNEL);
            o.deny = o.deny.remove(Permissions::VIEW_CHANNEL);
        }
    }
    if !roles.is_empty() {
        let e = overwrite_entry(overwrites, OverwriteTarget::Role(RoleId::EVERYONE));
        e.deny = e.deny.union(Permissions::VIEW_CHANNEL);
        for r in roles {
            let e = overwrite_entry(overwrites, OverwriteTarget::Role(*r));
            e.allow = e.allow.union(Permissions::VIEW_CHANNEL);
        }
    }
    overwrites.retain(|o| o.allow != Permissions::NONE || o.deny != Permissions::NONE);
    Ok(())
}

/// `overwrites` 인자 확인(해소기 규칙과 같음, 알아보기 쉬운 오류를 먼저 냅니다). 빈 덮어쓰기는 뺍니다.
fn check_overwrites(
    state: &SpaceState,
    me: &UserId,
    old: &[PermissionOverwrite],
    new: &[PermissionOverwrite],
) -> Result<Vec<PermissionOverwrite>, RpcError> {
    let known = Permissions::NAMES
        .iter()
        .fold(Permissions::NONE, |a, (_, b)| a.union(*b));
    let mut out: Vec<PermissionOverwrite> = Vec::new();
    for o in new {
        if out.iter().any(|x| x.target == o.target) {
            return Err(RpcError::invalid_input("권한 덮어쓰기 대상이 겹칩니다"));
        }
        if o.allow.union(o.deny).remove(known) != Permissions::NONE {
            return Err(RpcError::invalid_input("알 수 없는 권한이 있습니다"));
        }
        if o.allow.0 & o.deny.0 != 0 {
            return Err(RpcError::invalid_input(
                "같은 권한을 허용과 거부에 함께 둘 수 없습니다",
            ));
        }
        let target_ok = match &o.target {
            OverwriteTarget::Role(r) => state.roles.contains_key(r),
            OverwriteTarget::Member(u) => state.is_member(u),
        };
        if !target_ok {
            return Err(RpcError::invalid_input("없는 역할이나 멤버입니다"));
        }
        if o.allow != Permissions::NONE || o.deny != Permissions::NONE {
            out.push(*o);
        }
    }
    if out.len() > MAX_OVERWRITES {
        return Err(RpcError::invalid_input(format!(
            "권한 덮어쓰기는 {MAX_OVERWRITES}개까지입니다"
        )));
    }
    let base = state.base_permissions(me);
    if base != Permissions::ALL && !base.contains(overwrite_delta(old, &out)) {
        return Err(RpcError::forbidden(
            "자기에게 없는 권한은 덮어쓰기로 주거나 뺄 수 없습니다",
        ));
    }
    Ok(out)
}

/// 덮어쓰기가 바뀐 권한 비트(해소기의 `overwrite_delta`와 같음).
fn overwrite_delta(old: &[PermissionOverwrite], new: &[PermissionOverwrite]) -> Permissions {
    let find = |v: &[PermissionOverwrite], t| {
        v.iter()
            .find(|x| x.target == t)
            .map_or((0, 0), |x| (x.allow.0, x.deny.0))
    };
    let mut bits = 0u64;
    for o in new.iter().chain(old) {
        let (a0, d0) = find(old, o.target);
        let (a1, d1) = find(new, o.target);
        bits |= (a0 ^ a1) | (d0 ^ d1);
    }
    Permissions(bits)
}

/// `private_roles`와 `overwrites`는 같은 덮어쓰기를 다른 방식으로 정하므로 하나만 받습니다.
fn one_overwrite_arg(
    private_roles: &Option<Vec<RoleId>>,
    overwrites: &Option<Vec<PermissionOverwrite>>,
) -> Result<(), RpcError> {
    if private_roles.is_some() && overwrites.is_some() {
        return Err(RpcError::invalid_input(
            "private_roles와 overwrites를 함께 쓸 수 없습니다",
        ));
    }
    Ok(())
}

fn check_forum_tags(kind: ChannelKind, tags: &[ForumTag]) -> Result<(), RpcError> {
    if kind != ChannelKind::Forum && !tags.is_empty() {
        return Err(RpcError::invalid_input(
            "포럼 채널에만 태그를 둘 수 있습니다",
        ));
    }
    if tags.len() > MAX_FORUM_TAGS {
        return Err(RpcError::invalid_input(format!(
            "포럼 태그는 {MAX_FORUM_TAGS}개까지입니다"
        )));
    }
    for (i, t) in tags.iter().enumerate() {
        if tags[..i].iter().any(|x| x.id == t.id) {
            return Err(RpcError::invalid_input("포럼 태그 ID가 겹칩니다"));
        }
        if crews::check_name(&t.name, "포럼 태그 이름")? != t.name {
            return Err(RpcError::invalid_input(
                "포럼 태그 이름 앞뒤에 공백을 둘 수 없습니다",
            ));
        }
        if let Some(e) = &t.emoji {
            rpc(crews::check_text(e, MAX_NAME_CHARS, "포럼 태그 이모지"))?;
        }
    }
    Ok(())
}

/// 상위 채널은 있는 카테고리여야 하고, 카테고리는 상위를 가질 수 없습니다.
fn check_parent(
    state: &SpaceState,
    id: Option<&ChannelId>,
    kind: ChannelKind,
    parent: &Option<ChannelId>,
) -> Result<(), RpcError> {
    let Some(p) = parent else { return Ok(()) };
    let ok = Some(p) != id
        && kind != ChannelKind::Category
        && state
            .channels
            .get(p)
            .is_some_and(|c| c.kind == ChannelKind::Category);
    if ok {
        Ok(())
    } else {
        Err(RpcError::invalid_input(
            "상위 채널은 있는 카테고리여야 합니다(카테고리는 상위를 가질 수 없음)",
        ))
    }
}

/// 이 채널을 관리할 수 있는지(채널 덮어쓰기까지 적용).
fn need_manage(state: &SpaceState, me: &UserId, c: &ChannelId) -> Result<(), RpcError> {
    if state
        .channel_permissions(me, c)
        .contains(Permissions::MANAGE_CHANNELS)
    {
        Ok(())
    } else {
        Err(RpcError::forbidden("이 채널을 관리할 권한이 없습니다"))
    }
}

async fn publish(node: &Arc<Node>, space: SpaceId, body: EventBody) -> Result<(), RpcError> {
    rpc(author::publish(node, space, None, &body, KeyChoice::State).await)?;
    Ok(())
}

/// 채널을 만들고, P2P 전용·익명이면 같은 적용 잠금 안에서 바로 `ChannelTransportSet`·`ChannelAnonymousSet`을
/// 잇습니다(그 사이에 Tor로 내용이 오가거나 이름이 붙은 글이 쓰이는 틈이 없게, 전파도 함께).
async fn publish_create(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    settings: ChannelSettings,
    p2p: bool,
    anonymous: bool,
    extra: Vec<EventBody>,
) -> Result<(), RpcError> {
    let create = EventBody::ChannelCreate { channel, settings };
    let l = ingest::lock_space(node, space).await;
    let (ev, mut report) = rpc(author::publish_locked(
        node,
        &l,
        space,
        None,
        &create,
        KeyChoice::State,
        &[],
    ))?;
    let mut out = vec![ev];
    let mut failed = None;
    let mut follow = Vec::new();
    if p2p {
        follow.push(p2p::transport_body(channel, true));
    }
    if anonymous {
        follow.push(EventBody::ChannelAnonymousSet {
            channel,
            anonymous: true,
        });
    }
    follow.extend(extra);
    for set in &follow {
        match author::publish_locked(node, &l, space, None, set, KeyChoice::State, &[]) {
            Ok((ev, r)) => {
                out.push(ev);
                report.merge(r);
            }
            Err(e) => {
                // 미리 검사했으므로 드뭅니다. 바란 것과 다른 채널(Tor 채널·이름이 보이는 채널)로 남기지 않게 지웁니다
                // (지우기도 실패하면 오류만 알림).
                let delete = EventBody::ChannelDelete { channel };
                if let Ok((ev, r)) =
                    author::publish_locked(node, &l, space, None, &delete, KeyChoice::State, &[])
                {
                    out.push(ev);
                    report.merge(r);
                }
                failed = Some(e);
                break;
            }
        }
    }
    drop(l);
    ingest::after_unlock(node, space, &report);
    sync_service::propagate(node, space, out);
    failed.map_or(Ok(()), |e| Err(RpcError::from(e)))
}

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelCreate>,
) -> R<m::ChannelCreate> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_CHANNELS))?;
    let name = rpc(crews::check_name(&p.name, "채널 이름"))?;
    let topic = p.topic.unwrap_or_default();
    rpc(crews::check_text(&topic, MAX_TOPIC_CHARS, "채널 주제"))?;
    if matches!(p.kind, ChannelKind::Other(_)) {
        return Err(RpcError::invalid_input("만들 수 없는 채널 종류입니다"));
    }
    check_parent(&state, None, p.kind, &p.parent)?;
    let security = p.security.unwrap_or(SecurityLevel::Normal);
    if security == SecurityLevel::Volatile && p.kind == ChannelKind::Forum {
        // 게시글 제목·고정·잠금 변경은 상태 레인(디스크)으로 가므로 휘발성으로 둘 수 없습니다.
        return Err(RpcError::invalid_input(
            "포럼 채널은 휘발성으로 만들 수 없습니다",
        ));
    }
    one_overwrite_arg(&p.private_roles, &p.overwrites)?;
    let mut overwrites = Vec::new();
    if let Some(roles) = &p.private_roles {
        set_private_roles(&state, &mut overwrites, roles)?;
    }
    if let Some(o) = &p.overwrites {
        overwrites = check_overwrites(&state, &me, &[], o)?;
    }
    let forum_tags = p.forum_tags.unwrap_or_default();
    check_forum_tags(p.kind, &forum_tags)?;
    let settings = ChannelSettings {
        name,
        kind: p.kind,
        topic,
        parent: p.parent,
        position: p
            .position
            .unwrap_or(i32::try_from(state.channels.len()).unwrap_or(i32::MAX)),
        security,
        join_mode: p.join_mode.unwrap_or(JoinMode::Always),
        ttl: ttl_of(ChatTtl::None, p.ttl_secs, p.ttl_on_leave)?,
        history: p.history.unwrap_or(HistoryVisibility::Private),
        // 휘발성 채널은 미리보기 요청이 링크를 밖으로 흘리지 않게 기본으로 끕니다.
        link_previews: p
            .link_previews
            .unwrap_or(security != SecurityLevel::Volatile),
        overwrites,
        forum_tags,
    };
    let channel = ChannelId::random();
    let p2p = p.p2p.unwrap_or(false);
    if p2p {
        rpc(p2p::check_create(&state, &me, &channel, &settings))?;
    }
    let anonymous = p.anonymous.unwrap_or(false);
    if anonymous {
        check_anonymous(&state, settings.kind)?;
    }
    // 첨부 한도: 크루의 휘발성 채널은 따로 정하지 않으면 10MiB(모든 참가자 RAM에 올라가므로).
    let attach_limit = p.attach_limit.unwrap_or_else(|| {
        (state.kind == SpaceKind::Crew && settings.is_volatile())
            .then_some(VOLATILE_DEFAULT_ATTACH_LIMIT)
    });
    let mut extra = Vec::new();
    if let Some(l) = attach_limit {
        check_attach_limit(&state, settings.kind, Some(l))?;
        extra.push(attach_limit_body(channel, Some(l)));
    }
    let rules = EditRules {
        no_edit: p.no_edit.unwrap_or(false),
        no_delete: p.no_delete.unwrap_or(false),
    };
    if !rules.is_empty() {
        check_edit_rules(&state, settings.kind)?;
        extra.push(edit_rules_body(channel, rules));
    }
    publish_create(node, p.space, channel, settings, p2p, anonymous, extra).await?;
    Ok(ChannelRef { channel })
}

/// 익명으로 둘 수 있는 채널인지(해소기 규칙과 같음: 크루의, 카테고리가 아닌 채널).
fn check_anonymous(state: &SpaceState, kind: ChannelKind) -> Result<(), RpcError> {
    if state.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input(
            "익명 채널은 크루에서만 만들 수 있습니다",
        ));
    }
    if kind == ChannelKind::Category {
        return Err(RpcError::invalid_input(
            "카테고리는 익명으로 만들 수 없습니다",
        ));
    }
    Ok(())
}

pub(crate) async fn update(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::ChannelUpdate>,
) -> R<m::ChannelUpdate> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    let old = state
        .channels
        .get(&p.channel)
        .ok_or_else(|| RpcError::not_found("없는 채널"))?;
    need_manage(&state, &me, &p.channel)?;
    let mut s = old.clone();
    if let Some(n) = &p.name {
        s.name = rpc(crews::check_name(n, "채널 이름"))?;
    }
    if let Some(parent) = p.parent {
        check_parent(&state, Some(&p.channel), s.kind, &parent)?;
        s.parent = parent;
    }
    if let Some(t) = &p.topic {
        rpc(crews::check_text(t, MAX_TOPIC_CHARS, "채널 주제"))?;
        s.topic.clone_from(t);
    }
    if p.security.is_some_and(|x| x != old.security) {
        // 이미 저장된 데이터의 보관 약속이 깨지므로 보안 등급은 바꿀 수 없습니다(해소기 규칙과 같음).
        return Err(RpcError::invalid_input(
            "보안 등급은 바꿀 수 없습니다(새 채널을 만드세요)",
        ));
    }
    if let Some(j) = p.join_mode {
        s.join_mode = j;
    }
    s.ttl = ttl_of(s.ttl, p.ttl_secs, p.ttl_on_leave)?;
    if let Some(h) = p.history {
        s.history = h;
    }
    if let Some(l) = p.link_previews {
        s.link_previews = l;
    }
    if let Some(pos) = p.position {
        s.position = pos;
    }
    if let Some(tags) = &p.forum_tags {
        check_forum_tags(s.kind, tags)?;
        s.forum_tags.clone_from(tags);
    }
    one_overwrite_arg(&p.private_roles, &p.overwrites)?;
    if let Some(roles) = &p.private_roles {
        set_private_roles(&state, &mut s.overwrites, roles)?;
    }
    if let Some(o) = &p.overwrites {
        s.overwrites = check_overwrites(&state, &me, &old.overwrites, o)?;
    }
    let anonymous = p.anonymous.filter(|a| *a != state.is_anonymous(&p.channel));
    if anonymous == Some(true) {
        check_anonymous(&state, s.kind)?;
    }
    let attach_limit = p
        .attach_limit
        .filter(|l| *l != state.attach_limits.get(&p.channel).copied());
    if let Some(l) = attach_limit {
        check_attach_limit(&state, s.kind, l)?;
    }
    let old_rules = state.edit_rules(&p.channel);
    let rules = EditRules {
        no_edit: p.no_edit.unwrap_or(old_rules.no_edit),
        no_delete: p.no_delete.unwrap_or(old_rules.no_delete),
    };
    let rules = (rules != old_rules).then_some(rules);
    if rules.is_some() {
        check_edit_rules(&state, s.kind)?;
    }
    let volatile_cap = p
        .volatile_cap
        .filter(|c| *c != state.volatile_caps.get(&p.channel).copied());
    if let Some(c) = volatile_cap {
        check_volatile_cap(&state, &s, c)?;
    }
    if s == *old {
        if let Some(a) = anonymous {
            publish(node, p.space, anonymous_body(p.channel, a)).await?;
        }
        if let Some(l) = attach_limit {
            publish(node, p.space, attach_limit_body(p.channel, l)).await?;
        }
        if let Some(r) = rules {
            publish(node, p.space, edit_rules_body(p.channel, r)).await?;
        }
        if let Some(c) = volatile_cap {
            publish(node, p.space, volatile_cap_body(p.channel, c)).await?;
        }
        return Ok(empty());
    }
    let opens_history =
        s.history == HistoryVisibility::Public && old.history != HistoryVisibility::Public;
    if s.overwrites != old.overwrites || opens_history {
        // 볼 수 있는 사람이 바뀌거나(기록 키를 받을 사람이 바뀜) 지난 기록을 새 멤버에게 열면 자동화 토큰은 사람
        // 확인을 받습니다. 1:1에서 전환된 크루의 전환 전 채널은 원래 두 사람이 모두 공개해야 열립니다(해소기).
        let params = serde_json::to_value(&p).map_err(|e| RpcError::internal(e.to_string()))?;
        admin::confirm_when_others(node, ctx, "channel.update", &params).await?;
    }
    publish(
        node,
        p.space,
        EventBody::ChannelUpdate {
            channel: p.channel,
            settings: s,
        },
    )
    .await?;
    if let Some(a) = anonymous {
        publish(node, p.space, anonymous_body(p.channel, a)).await?;
    }
    if let Some(l) = attach_limit {
        publish(node, p.space, attach_limit_body(p.channel, l)).await?;
    }
    if let Some(r) = rules {
        publish(node, p.space, edit_rules_body(p.channel, r)).await?;
    }
    if let Some(c) = volatile_cap {
        publish(node, p.space, volatile_cap_body(p.channel, c)).await?;
    }
    Ok(empty())
}

fn volatile_cap_body(channel: ChannelId, cap: Option<u32>) -> EventBody {
    EventBody::ChannelVolatileCapSet { channel, cap }
}

/// 상시 참가 상한은 크루의 상시 휘발성 채널에만, 해소기와 같은 범위로.
fn check_volatile_cap(
    state: &SpaceState,
    s: &ChannelSettings,
    cap: Option<u32>,
) -> Result<(), RpcError> {
    if state.kind != SpaceKind::Crew || !s.is_volatile() || s.join_mode != JoinMode::Always {
        return Err(RpcError::invalid_input(
            "참가 상한은 크루의 상시 휘발성 채널에서만 정할 수 있습니다",
        ));
    }
    if let Some(c) = cap
        && !(MIN_VOLATILE_CAP..=MAX_VOLATILE_CAP).contains(&c)
    {
        return Err(RpcError::invalid_input(format!(
            "참가 상한은 {MIN_VOLATILE_CAP}..={MAX_VOLATILE_CAP}명이어야 합니다"
        )));
    }
    Ok(())
}

fn anonymous_body(channel: ChannelId, anonymous: bool) -> EventBody {
    EventBody::ChannelAnonymousSet { channel, anonymous }
}

fn attach_limit_body(channel: ChannelId, max_bytes: Option<u64>) -> EventBody {
    EventBody::ChannelAttachLimitSet { channel, max_bytes }
}

fn edit_rules_body(channel: ChannelId, rules: EditRules) -> EventBody {
    EventBody::ChannelEditRulesSet {
        channel,
        no_edit: rules.no_edit,
        no_delete: rules.no_delete,
    }
}

/// 크루의 휘발성 채널을 만들 때 따로 정하지 않으면 쓰는 첨부 한도(10MiB).
pub(crate) const VOLATILE_DEFAULT_ATTACH_LIMIT: u64 = 10 * 1024 * 1024;

/// 수정·삭제 금지는 크루의 카테고리가 아닌 채널에만(해소기 규칙과 같음).
fn check_edit_rules(state: &SpaceState, kind: ChannelKind) -> Result<(), RpcError> {
    if state.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input(
            "수정·삭제 금지는 크루 채널에서만 정할 수 있습니다",
        ));
    }
    if kind == ChannelKind::Category {
        return Err(RpcError::invalid_input(
            "카테고리에는 수정·삭제 금지가 없습니다",
        ));
    }
    Ok(())
}

/// 첨부 한도는 크루의 카테고리가 아닌 채널에만, 0(첨부 금지) 또는 1MiB 이상으로(해소기 규칙과 같음).
fn check_attach_limit(
    state: &SpaceState,
    kind: ChannelKind,
    max_bytes: Option<u64>,
) -> Result<(), RpcError> {
    if state.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input(
            "첨부 한도는 크루 채널에서만 정할 수 있습니다",
        ));
    }
    if kind == ChannelKind::Category {
        return Err(RpcError::invalid_input("카테고리에는 첨부 한도가 없습니다"));
    }
    if max_bytes.is_some_and(|n| !valid_attach_limit(n)) {
        return Err(RpcError::invalid_input(format!(
            "첨부 한도는 0(첨부 금지)이거나 {}MiB 이상이어야 합니다",
            MIN_ATTACH_LIMIT / (1024 * 1024)
        )));
    }
    Ok(())
}

pub(crate) async fn delete(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelDelete>,
) -> R<m::ChannelDelete> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    if !state.channels.contains_key(&p.channel) {
        return Err(RpcError::not_found("없는 채널"));
    }
    need_manage(&state, &me, &p.channel)?;
    publish(
        node,
        p.space,
        EventBody::ChannelDelete { channel: p.channel },
    )
    .await?;
    Ok(empty())
}

/// 있는 채널을 P2P 전용으로 바꾸거나 Tor로 되돌립니다(어느 쪽이든 언제든). 지금 값과 같으면 아무것도 쓰지 않습니다.
/// Tor로 되돌리면 그때부터 그 채널 기록이 Tor로 모두에게 동기화되고, 이미 서로 보인 IP는 다시 숨겨지지 않습니다.
pub(crate) async fn set_p2p(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelSetP2p>,
) -> R<m::ChannelSetP2p> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    rpc(p2p::check_set(&state, &me, &p.channel, p.enabled))?;
    if state.is_p2p(&p.channel) == p.enabled {
        return Ok(empty());
    }
    publish(node, p.space, p2p::transport_body(p.channel, p.enabled)).await?;
    Ok(empty())
}

/// P2P 전용 채널 참여(IP 노출 경고를 받아들임). 사람 토큰만(메서드 표가 검사).
pub(crate) async fn p2p_join(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelP2pJoin>,
) -> R<m::ChannelP2pJoin> {
    rpc(p2p::join(node, p.space, p.channel))?;
    Ok(empty())
}

/// P2P 전용 채널 참여 그만두기(이 기기에서 다시 잠김).
pub(crate) async fn p2p_leave(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelP2pLeave>,
) -> R<m::ChannelP2pLeave> {
    rpc(p2p::leave(node, p.space, p.channel))?;
    Ok(empty())
}

/// `channel.typing`: 글 쓰는 중 알림(`typing.rs`).
pub(crate) async fn typing(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelTyping>,
) -> R<m::ChannelTyping> {
    rpc(crate::typing::send(node, p.space, p.channel))?;
    Ok(empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_arguments_map_to_chat_ttl() {
        assert_eq!(
            ttl_of(ChatTtl::None, Some(30), None).unwrap(),
            ChatTtl::Seconds(30)
        );
        assert_eq!(
            ttl_of(ChatTtl::Seconds(30), Some(0), None).unwrap(),
            ChatTtl::None
        );
        assert_eq!(
            ttl_of(ChatTtl::None, None, Some(true)).unwrap(),
            ChatTtl::OnLeave
        );
        assert_eq!(
            ttl_of(ChatTtl::OnLeave, None, Some(false)).unwrap(),
            ChatTtl::None
        );
        assert_eq!(
            ttl_of(ChatTtl::Seconds(5), None, Some(false)).unwrap(),
            ChatTtl::Seconds(5)
        );
        assert!(ttl_of(ChatTtl::None, Some(5), Some(true)).is_err());
    }

    #[test]
    fn private_roles_only_touch_view_bits() {
        let role = RoleId::random();
        let mut st = crate::crews::tests_support::empty_state();
        st.roles.insert(
            role,
            yangpa_core::model::Role {
                id: role,
                name: "비밀".into(),
                color: 0,
                rank: 1,
                permissions: Permissions::NONE,
                automation: false,
            },
        );
        let mut ow = vec![PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::NONE,
            deny: Permissions::SEND_MESSAGES,
        }];
        set_private_roles(&st, &mut ow, &[role]).unwrap();
        let everyone = ow
            .iter()
            .find(|o| o.target == OverwriteTarget::Role(RoleId::EVERYONE))
            .unwrap();
        assert!(everyone.deny.contains(Permissions::VIEW_CHANNEL));
        assert!(everyone.deny.contains(Permissions::SEND_MESSAGES));
        assert!(ow.iter().any(|o| o.target == OverwriteTarget::Role(role)
            && o.allow.contains(Permissions::VIEW_CHANNEL)));
        // 빈 목록이면 공개로 돌아가고, 다른 덮어쓰기는 남습니다.
        set_private_roles(&st, &mut ow, &[]).unwrap();
        assert_eq!(ow.len(), 1);
        assert_eq!(ow[0].deny, Permissions::SEND_MESSAGES);
        assert!(set_private_roles(&st, &mut ow, &[RoleId::random()]).is_err());
    }
}
