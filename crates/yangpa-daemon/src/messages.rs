//! 메시지 공용 도우미: 보기 변환, 알림 문구, 보낼 때 쓸 채널 뿌리(키 그룹 키에서 파생).

use base64::Engine;
use yangpa_core::UserId;
use yangpa_core::body::{MessageBody, ReactionEmoji, Secret32};
use yangpa_core::emoji::{format_emoji, parse_emoji_tokens};
use yangpa_core::event::EventHeader;
use yangpa_core::ids::{ChannelId, ChannelRootId, EventId, SpaceId};
use yangpa_core::model::{Permissions, SpaceKind, SpaceState};
use yangpa_rpc::api::{
    ANONYMOUS_DEVICE, ANONYMOUS_USER, AttachmentView, EmbedView, MessageView, NotifySender,
    NotifyView, ReactionView,
};
use yangpa_store::MessageRecord;

use crate::error::{Error, Result};
use crate::ingest::Locked;
use crate::node::Node;
use crate::{blobs, identity, keys};

/// 알림 본문 미리보기 길이(글자).
const PREVIEW_CHARS: usize = 100;

fn b64(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub(crate) fn emoji_string(e: &ReactionEmoji) -> String {
    match e {
        ReactionEmoji::Unicode { value } => value.clone(),
        ReactionEmoji::Custom { id, name, animated } => format_emoji(name, *id, *animated),
    }
}

/// API 문자열 → 반응 이모지. `<:이름:ID>`/`<a:이름:ID>` 하나면 커스텀, 아니면 유니코드.
pub(crate) fn parse_reaction(s: &str) -> Result<ReactionEmoji> {
    let s = s.trim();
    if s.is_empty() || s.len() > 128 {
        return Err(Error::invalid("이모지가 비었거나 너무 깁니다"));
    }
    let tokens = parse_emoji_tokens(s);
    if let [t] = tokens.as_slice()
        && t.range == (0..s.len())
    {
        return Ok(ReactionEmoji::Custom {
            id: t.id,
            name: t.name.to_owned(),
            animated: t.animated,
        });
    }
    Ok(ReactionEmoji::Unicode {
        value: s.to_owned(),
    })
}

/// 지금 익명인 채널인지(공간 상태 기준).
pub(crate) fn channel_anonymous(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    node.space_state(space)
        .is_some_and(|st| st.is_anonymous(channel))
}

/// 익명 글이면(익명으로 썼거나 채널이 지금 익명) 글쓴이를 가리고 그 채널의 가명을 붙입니다(`anon::name`).
/// `anonymous`·`mine`은 미리 채워 둡니다.
pub(crate) fn mask(
    view: &mut MessageView,
    space: &SpaceId,
    channel: &ChannelId,
    channel_anonymous: bool,
) {
    if !(view.anonymous || channel_anonymous) {
        return;
    }
    if view.author != ANONYMOUS_USER {
        view.anon_name = Some(crate::anon::name(space, channel, &view.author));
    }
    view.anonymous = true;
    view.author = ANONYMOUS_USER;
    view.author_device = ANONYMOUS_DEVICE;
    // 차단한 사람의 글인지도 알려 주면 누가 썼는지 좁혀지므로 접지 않습니다.
    view.author_blocked = false;
}

/// 저장된 보기 행 → API 보기.
pub(crate) fn view_of(node: &Node, rec: &MessageRecord) -> MessageView {
    let mut view = raw_view_of(node, rec);
    mask(
        &mut view,
        &rec.space,
        &rec.channel,
        channel_anonymous(node, &rec.space, &rec.channel),
    );
    view
}

fn raw_view_of(node: &Node, rec: &MessageRecord) -> MessageView {
    let body = rec.content.as_ref().map(|c| &c.body);
    MessageView {
        id: rec.id,
        author: rec.author,
        author_device: rec.author_device,
        created_at: rec.created_at,
        lamport: rec.lamport,
        text: body.map(|b| b.text.clone()).unwrap_or_default(),
        attachments: body
            .map(|b| {
                b.attachments
                    .iter()
                    .map(|a| AttachmentView {
                        blob: a.blob,
                        name: a.name.clone(),
                        mime: a.mime.clone(),
                        size: a.size,
                        width: a.width,
                        height: a.height,
                        duration_ms: a.duration_ms,
                        thumbnail_base64: b64(&a.thumbnail),
                        downloaded: blobs::is_downloaded(node, &a.blob),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        embeds: body
            .map(|b| {
                b.embeds
                    .iter()
                    .map(|e| EmbedView {
                        url: e.url.clone(),
                        title: e.title.clone(),
                        description: e.description.clone(),
                        site_name: e.site_name.clone(),
                        thumbnail_base64: b64(&e.thumbnail),
                        player_url: e.player_url.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        reply_to: body.and_then(|b| b.reply_to),
        reactions: rec
            .reactions
            .iter()
            .map(|r| ReactionView {
                emoji: emoji_string(&r.emoji),
                count: r.count,
                me: r.me,
            })
            .collect(),
        edited_at: rec.edited_at,
        deleted: rec.deleted,
        volatile: false,
        expires_at: None,
        thread: rec.thread,
        space: None,
        channel: None,
        author_blocked: crate::moderation::is_author_blocked(node, &rec.author, &rec.author_device),
        anonymous: rec.anonymous,
        anon_name: None,
        mine: node.me().is_ok_and(|me| me.user() == rec.author),
        mentions_me: rec.mentions_me,
    }
}

/// `message.new` 알림 문구. 제목은 작성자 이름(크루면 뒤에 크루·채널 이름), 본문은 앞부분(멘션은 `@이름`).
/// 익명 글이면 이름은 '익명', 프사는 없음(앱이 기본 프사를 그림)입니다.
pub(crate) fn notify_view(node: &Node, space: &SpaceId, rec: &MessageRecord) -> NotifyView {
    let anonymous = rec.anonymous || channel_anonymous(node, space, &rec.channel);
    let (user, avatar, author) = if anonymous {
        let name = crate::anon::name(space, &rec.channel, &rec.author);
        (ANONYMOUS_USER, None, name)
    } else {
        let profile = identity::profile_of(node, &rec.author);
        let name = if profile.name.is_empty() {
            rec.author.short()
        } else {
            profile.name.clone()
        };
        (rec.author, profile.avatar, name)
    };
    let title = match node.space_state(space) {
        Some(st) if st.kind == SpaceKind::Crew => {
            let channel = st.channels.get(&rec.channel).map(|c| c.name.as_str());
            notify_title(&author, &st.settings.name, channel)
        }
        _ => author.clone(),
    };
    let author_name = author;
    let body = rec.content.as_ref().map(|c| {
        let text = strip_markdown(&notify_text(&c.body.text, |u| {
            identity::display_name(node, u)
        }));
        let t: String = text.chars().take(PREVIEW_CHARS).collect();
        if t.is_empty() && !c.body.attachments.is_empty() {
            "첨부 파일".to_owned()
        } else {
            t
        }
    });
    NotifyView {
        sender: Some(NotifySender {
            user,
            name: author_name,
            avatar,
        }),
        title,
        body,
    }
}

/// 크루 메시지 알림 제목: `작성자 · 크루 #채널`(여러 크루의 알림을 구분하게).
fn notify_title(author: &str, crew: &str, channel: Option<&str>) -> String {
    match (
        crew.trim(),
        channel.map(str::trim).filter(|c| !c.is_empty()),
    ) {
        ("", None) => author.to_owned(),
        ("", Some(c)) => format!("{author} · #{c}"),
        (crew, None) => format!("{author} · {crew}"),
        (crew, Some(c)) => format!("{author} · {crew} #{c}"),
    }
}

/// 글에 든 멘션(`<@ID>`)의 사용자, 처음 나온 순서로 한 번씩. `keep`이 거짓인 사용자(멤버가 아님)는 뺍니다.
pub(crate) fn mentioned_users(text: &str, keep: impl Fn(&UserId) -> bool) -> Vec<UserId> {
    let mut out: Vec<UserId> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        let tail = &rest[start + 2..];
        let Some(end) = tail.find('>') else { break };
        if let Ok(u) = tail[..end].parse::<UserId>()
            && keep(&u)
            && !out.contains(&u)
        {
            out.push(u);
        }
        rest = &tail[end..];
    }
    out
}

/// 모두 멘션의 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Broadcast {
    /// `@here`: 그때 접속해 있던 멤버.
    Here,
    /// `@everyone`: 채널을 볼 수 있는 멤버 모두.
    Everyone,
}

/// `@here`를 실시간으로 받은 것으로 보는 시간(작성 시각부터). 나중에 따라잡은 기기는 그때 접속해 있지 않았습니다.
pub(crate) const HERE_WINDOW_MS: u64 = 5 * 60 * 1000;

/// 글에 든 `@everyone`·`@here`(둘 다 있으면 `Everyone`). 앞뒤가 글자·숫자·`_`이거나 앞이 `\`(이스케이프)면 아닙니다.
/// 앱(lib/markdown.ts)과 같은 경계이고, 코드 안인지는 보지 않습니다.
pub(crate) fn broadcast_mention(text: &str) -> Option<Broadcast> {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut found = None;
    for (i, _) in text.match_indices('@') {
        let before = text[..i].chars().next_back();
        if word(before) || before == Some('\\') {
            continue;
        }
        let rest = &text[i + 1..];
        for (name, kind) in [("everyone", Broadcast::Everyone), ("here", Broadcast::Here)] {
            if rest.starts_with(name) && !word(rest[name.len()..].chars().next()) {
                if kind == Broadcast::Everyone {
                    return Some(kind);
                }
                found = Some(kind);
            }
        }
    }
    found
}

/// 이 글이 나를 멘션하는지: `<@나>`, 또는 그 채널에서 `MENTION_MANY`가 있는 작성자의 `@everyone`, 작성 뒤
/// [`HERE_WINDOW_MS`] 안에 받은 `@here`. 받는 쪽이 작성자 권한으로 정합니다(보낸 쪽은 무엇이든 꾸밀 수 있음).
pub(crate) fn mentions_me(
    state: &SpaceState,
    h: &EventHeader,
    m: &MessageBody,
    me: &UserId,
    now_ms: u64,
) -> bool {
    if h.user == *me {
        return false;
    }
    if m.mentions.contains(me) {
        return true;
    }
    let (Some(kind), Some(channel)) = (broadcast_mention(&m.text), h.channel) else {
        return false;
    };
    state
        .channel_permissions(&h.user, &channel)
        .contains(Permissions::MENTION_MANY)
        && (kind == Broadcast::Everyone
            || now_ms.saturating_sub(h.created_at.as_millis()) <= HERE_WINDOW_MS)
}

/// 알림 본문에 쓸 글: 멘션(`<@ID>`)은 `@이름`, 커스텀 이모지(`<:이름:ID>`)는 `:이름:`으로 바꿉니다.
/// 알아볼 수 없는 표기는 그대로 둡니다.
fn notify_text(text: &str, name_of: impl Fn(&UserId) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(end) = tail.find('>') {
            let token = &tail[..=end];
            if let Some(user) = token[1..end]
                .strip_prefix('@')
                .and_then(|id| id.parse::<UserId>().ok())
            {
                out.push('@');
                out.push_str(&name_of(&user));
                rest = &tail[end + 1..];
                continue;
            }
            if let Some(e) = parse_emoji_tokens(token)
                .into_iter()
                .find(|e| e.range == (0..token.len()))
            {
                out.push(':');
                out.push_str(e.name);
                out.push(':');
                rest = &tail[end + 1..];
                continue;
            }
        }
        out.push('<');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// 이스케이프(`\*` 같은)를 잠시 바꿔 두는 사용자 정의 영역 시작(끝에 되돌림).
const ESCAPE_BASE: u32 = 0xE000;

/// 알림 본문에 쓸 글에서 마크다운 표시를 걷어 냅니다(앱의 `stripMarkdown`과 같은 뜻, 대략).
/// 스포일러(`||…||`)는 내용을 `(스포일러)`로 가리고, 가린 링크(`[글](https://…)`)는 글만 남깁니다.
fn strip_markdown(text: &str) -> String {
    // 이스케이프한 문장 부호는 표시로 읽히지 않게 먼저 바꿔 둡니다.
    let mut esc = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\'
            && let Some(&n) = it.peek()
            && n.is_ascii_punctuation()
        {
            it.next();
            esc.push(char::from_u32(ESCAPE_BASE + n as u32).unwrap_or(n));
            continue;
        }
        esc.push(c);
    }
    // 스포일러는 여러 줄에 걸칠 수 있어 줄을 나누기 전에 가립니다.
    let hidden = hide_spoilers(&esc);
    let mut lines = Vec::new();
    let mut in_code = false;
    for line in hidden.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            lines.push(line.to_owned());
            continue;
        }
        // 앱의 정렬 표시(`:::center` … `:::`) 줄은 알림에 보이지 않게 뺍니다.
        if is_align_fence(line) {
            continue;
        }
        let mut l = line;
        for p in [">>> ", "> ", "-# ", "### ", "## ", "# "] {
            if let Some(r) = l.strip_prefix(p) {
                l = r;
                break;
            }
        }
        lines.push(strip_inline(l));
    }
    lines
        .join("\n")
        .chars()
        .map(|c| match c as u32 {
            n @ ESCAPE_BASE..=0xE07F => char::from_u32(n - ESCAPE_BASE).unwrap_or(c),
            _ => c,
        })
        .collect()
}

fn is_align_fence(line: &str) -> bool {
    matches!(
        line.trim_start_matches(' ').trim_end_matches(' '),
        ":::" | ":::left" | ":::center" | ":::right"
    )
}

fn hide_spoilers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("||") {
        let after = &rest[start + 2..];
        match after.find("||") {
            Some(end) if !after[..end].trim().is_empty() => {
                out.push_str(&rest[..start]);
                out.push_str("(스포일러)");
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str(&rest[..start + 2]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 강조 표시(`**`, `__`, `~~`, 짝이 맞는 `*`), 코드 표시(`` ` ``), 가린 링크를 걷어 냅니다.
fn strip_inline(line: &str) -> String {
    let mut s = String::with_capacity(line.len());
    let mut rest = line;
    // 가린 링크: [글](http…) → 글.
    while let Some(open) = rest.find('[') {
        let tail = &rest[open + 1..];
        if let Some(mid) = tail.find("](") {
            let url = &tail[mid + 2..];
            if let Some(close) = url.find(')')
                && (url.starts_with("https://") || url.starts_with("http://"))
                && !url[..close].contains(char::is_whitespace)
            {
                s.push_str(&rest[..open]);
                s.push_str(&tail[..mid]);
                rest = &url[close + 1..];
                continue;
            }
        }
        s.push_str(&rest[..open + 1]);
        rest = tail;
    }
    s.push_str(rest);
    let s = s
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "")
        .replace('`', "");
    // 짝이 맞는 `*기울임*`만 걷어 냅니다(2*3 같은 글은 그대로).
    let stars: Vec<usize> = s.match_indices('*').map(|(i, _)| i).collect();
    if stars.len() >= 2 {
        let mut out = String::with_capacity(s.len());
        let mut skip = std::collections::HashSet::new();
        for pair in stars.chunks(2) {
            if let [a, b] = pair
                && s[a + 1..*b]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace())
                && s[..*b]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace())
            {
                skip.insert(*a);
                skip.insert(*b);
            }
        }
        for (i, c) in s.char_indices() {
            if !skip.contains(&i) {
                out.push(c);
            }
        }
        return out;
    }
    s
}

/// 이 공간·채널 메시지의 위치(논리 시계, ID). 다른 공간·채널의 메시지면 `None`(읽음 위치·페이지 기준이 다른 곳의
/// 메시지로 앞당겨지거나, 볼 수 없는 공간의 이벤트가 있는지 알아내는 데 쓰이지 않게).
pub(crate) fn position_in_channel(
    node: &Node,
    id: &EventId,
    space: &SpaceId,
    channel: &ChannelId,
) -> Result<Option<(u64, EventId)>> {
    Ok(node
        .store
        .get_message(id, None)?
        .filter(|r| r.space == *space && r.channel == *channel)
        .map(|r| (r.lamport, r.id)))
}

/// 이 채널에 보낼 채널 뿌리와 그 ID(본문은 `record_key(뿌리)`로 암호화, 이벤트는 `KeyRef::Record{root: ID}`). 채널이
/// 쓰는 키 그룹(멤버 전원이 보면 크루, 아니면 보기 규칙의 비공개 그룹)의 정규 사슬 끝 상태에서 파생합니다
/// ([`keys::send_root`]): 기록 공개면 R의 공개 뿌리, 아니면 T의 비공개 뿌리. 새 키 묶음을 내지 않습니다. 보내는 쪽
/// 규칙(제거를 반영한 갱신 기다리기)은 잠금 밖에서 먼저 돕니다(`keygroup::wait_rotation`).
pub(crate) fn send_key_locked(
    node: &std::sync::Arc<Node>,
    l: &Locked<'_>,
    space: &SpaceId,
    channel: &ChannelId,
    state: &std::sync::Arc<SpaceState>,
) -> Result<(ChannelRootId, Secret32)> {
    l.check(space)?;
    if !state.channels.contains_key(channel) {
        return Err(Error::not_found("없는 채널"));
    }
    keys::send_root(node, space, channel, state)?.ok_or_else(|| {
        Error::conflict("아직 이 채널의 키 그룹 키를 받지 못했습니다(확정 기기의 키 갱신 대기)")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_parsing() {
        assert_eq!(
            parse_reaction("👍").unwrap(),
            ReactionEmoji::Unicode {
                value: "👍".into()
            }
        );
        match parse_reaction("<:yangpa:123>").unwrap() {
            ReactionEmoji::Custom { id, name, animated } => {
                assert_eq!(id.0, 123);
                assert_eq!(name, "yangpa");
                assert!(!animated);
            }
            other => panic!("커스텀이 아님: {other:?}"),
        }
        // 표기 앞뒤에 다른 글자가 있으면 유니코드 문자열로 봅니다.
        assert!(matches!(
            parse_reaction("x<:yangpa:123>").unwrap(),
            ReactionEmoji::Unicode { .. }
        ));
        assert!(parse_reaction("  ").is_err());
        assert_eq!(
            emoji_string(&parse_reaction("<a:ab:7>").unwrap()),
            "<a:ab:7>"
        );
    }

    #[test]
    fn mentioned_users_are_unique_and_filtered() {
        let a = UserId([1; 32]);
        let b = UserId([2; 32]);
        let text = format!("<@{a}> <@{b}> <@{a}> <@없음> <@{b}");
        assert_eq!(mentioned_users(&text, |_| true), vec![a, b]);
        assert_eq!(mentioned_users(&text, |u| *u == b), vec![b]);
        assert!(mentioned_users("<@", |_| true).is_empty());
    }

    #[test]
    fn broadcast_mentions_need_word_boundaries() {
        use Broadcast::*;
        assert_eq!(broadcast_mention("@everyone 공지"), Some(Everyone));
        assert_eq!(broadcast_mention("다들(@here)!"), Some(Here));
        assert_eq!(broadcast_mention("@here 그리고 @everyone"), Some(Everyone));
        assert_eq!(broadcast_mention("끝에 @here"), Some(Here));
        for no in [
            "a@everyone",
            "@everyones",
            "@here_",
            "\\@everyone",
            "@Everyone",
            "메일 x@here.com",
            "@",
            "",
        ] {
            assert_eq!(broadcast_mention(no), None, "{no}");
        }
    }

    #[test]
    fn notify_text_renders_mentions_and_emoji() {
        let minji = UserId::from_bytes([5; 32]);
        let name = |u: &UserId| {
            if *u == minji {
                "민지".to_owned()
            } else {
                "?".to_owned()
            }
        };
        assert_eq!(
            notify_text(&format!("<@{minji}> 이거 봐 <:yangpa:123><a:ab:7>"), name),
            "@민지 이거 봐 :yangpa::ab:"
        );
        // 알아볼 수 없는 표기는 그대로.
        assert_eq!(
            notify_text("<@없는ID> a<b >c <", name),
            "<@없는ID> a<b >c <"
        );
        assert_eq!(notify_text("<<@x>>", name), "<<@x>>");
        assert_eq!(notify_text("", name), "");
    }

    #[test]
    fn notify_text_strips_markdown_and_hides_spoilers() {
        assert_eq!(
            strip_markdown("**굵게** *기울임* __밑줄__ ~~취소~~ `코드` 2*3"),
            "굵게 기울임 밑줄 취소 코드 2*3"
        );
        assert_eq!(strip_markdown("범인은 ||민지||야"), "범인은 (스포일러)야");
        assert_eq!(strip_markdown("||여러\n줄||").lines().count(), 1);
        assert_eq!(strip_markdown("|| || 만"), "|| || 만");
        assert_eq!(
            strip_markdown("[여기](https://example.com/a) 봐 [x](javascript:1)"),
            "여기 봐 [x](javascript:1)"
        );
        assert_eq!(
            strip_markdown("# 제목\n> 인용\n-# 작은 글씨"),
            "제목\n인용\n작은 글씨"
        );
        assert_eq!(
            strip_markdown("```rust\nlet **a** = 1;\n```"),
            "let **a** = 1;"
        );
        assert_eq!(
            strip_markdown("\\*별\\* \\|\\|안 가림\\|\\|"),
            "*별* ||안 가림||"
        );
        assert_eq!(
            strip_markdown(":::center\n가운데\n:::\n```\n:::\n```"),
            "가운데\n:::"
        );
    }

    #[test]
    fn crew_notify_title() {
        assert_eq!(
            notify_title("민지", "양파", Some("일반")),
            "민지 · 양파 #일반"
        );
        assert_eq!(notify_title("민지", "", Some("일반")), "민지 · #일반");
        assert_eq!(notify_title("민지", "양파", None), "민지 · 양파");
        assert_eq!(notify_title("민지", " ", Some(" ")), "민지");
    }
}
