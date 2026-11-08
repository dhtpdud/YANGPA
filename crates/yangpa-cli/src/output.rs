//! 사람이 읽는 출력. 함수는 문자열을 돌려주고 출력은 호출한 쪽이 합니다(테스트하기 쉽게).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, UNIX_EPOCH};

use yangpa_core::device::{DeviceEntry, Profile};
use yangpa_core::ids::{ChannelId, RoleId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, ChatTtl, HistoryVisibility, JoinMode, Permissions, Role,
    SecurityLevel, SpaceKind,
};
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::{
    ArchiveHostedView, ArchiveView, AuditEntry, BanView, BlockedDeviceView, BlockedView,
    ChannelCoverage, ChannelView, ContactKind, DataType, DeviceLinkRequestView, DeviceLinkStatus,
    DiagMetrics, EmojiView, ForumPostView, FriendLinkView, FriendRequestView, FriendView,
    IdentityView, InviteView, MemberView, MessageListResult, MessageSearchPage, MessageView,
    NatStatus, NetMode, NetStatus, Notification, PeerPolicy, PeerView, PendingConfirmation,
    ReportStatus, ReportView, SeederPolicy, SpaceSummary, SpaceView, StatusResult, StunServers,
    TokenView, UserDeviceView,
};
use yangpa_rpc::api::{
    BlobCacheView, BlobPinView, CheckpointAlertKind, CommitterCandidate, ConnectionMode,
};

use crate::parse::format_size;

/// 유닉스 밀리초를 UTC RFC 3339(초 단위)로.
pub fn format_time(t: Timestamp) -> String {
    let st = UNIX_EPOCH + Duration::from_millis(t.0);
    humantime::format_rfc3339_seconds(st).to_string()
}

/// 터미널 표시 폭(한글·한자·전각·이모지는 2칸). 표 정렬에만 쓰므로 근사치면 충분합니다.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let u = c as u32;
    if c.is_control() || (0x0300..=0x036F).contains(&u) || (0x200B..=0x200F).contains(&u) {
        return 0;
    }
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// 열 정렬 표.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new<const N: usize>(headers: [&str; N]) -> Self {
        Table {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self) -> String {
        let cols = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| display_width(h)).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(display_width(&one_line(c)));
            }
        }
        let mut out = String::new();
        let mut line = |cells: &[String]| {
            let mut l = String::new();
            for (i, w) in widths.iter().enumerate() {
                let c = cells.get(i).map(|c| one_line(c)).unwrap_or_default();
                l.push_str(&c);
                if i + 1 < cols {
                    let pad = w.saturating_sub(display_width(&c)) + 2;
                    l.extend(std::iter::repeat_n(' ', pad));
                }
            }
            out.push_str(l.trim_end());
            out.push('\n');
        };
        line(&self.headers);
        for r in &self.rows {
            line(r);
        }
        out
    }
}

/// 양방향 텍스트 제어 문자. 화면에 보이는 순서를 뒤집어 이름·ID·명령을 다르게 보이게 할 수 있습니다.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// 표 칸 안의 줄바꿈·제어 문자를 없앱니다(다른 멤버가 쓴 이름이 표를 깨거나 터미널 제어를 못 하게).
pub fn one_line(s: &str) -> String {
    s.chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// 다른 사람이 쓴 여러 줄 텍스트에서 터미널 제어 문자(ESC 등)·양방향 제어 문자를 지웁니다. 줄바꿈·탭은 둡니다.
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .filter(|c| (!c.is_control() || *c == '\n' || *c == '\t') && !is_bidi_control(*c))
        .collect()
}

fn yes(b: bool) -> &'static str {
    if b { "예" } else { "아니오" }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

/// 신뢰 스위치에 따른 연결 방식(`trust.get`). 주소는 보여 주지 않습니다.
pub fn connection_mode(m: ConnectionMode) -> &'static str {
    match m {
        ConnectionMode::P2p => "P2P 직접 연결(서로 IP가 보임)",
        ConnectionMode::OneWayMeExposed => "단방향: 상대에게 내 IP가 보임(상대는 숨음)",
        ConnectionMode::OneWayPeerExposed => "단방향: 내게 상대 IP가 보임(나는 숨음)",
        ConnectionMode::Tor => "Tor(서로 숨음)",
        ConnectionMode::Offline => "오프라인",
    }
}

pub fn space_kind(k: SpaceKind) -> &'static str {
    match k {
        SpaceKind::Space => "1대1",
        SpaceKind::Crew => "크루",
        SpaceKind::SelfSpace => "내 기기",
    }
}

pub fn channel_kind(k: ChannelKind) -> String {
    match k {
        ChannelKind::Text => "텍스트".into(),
        ChannelKind::Voice => "통화".into(),
        ChannelKind::Forum => "포럼".into(),
        ChannelKind::Category => "카테고리".into(),
        ChannelKind::Other(n) => format!("기타({n})"),
    }
}

fn ttl_text(t: ChatTtl) -> String {
    match t {
        ChatTtl::None => String::new(),
        ChatTtl::OnLeave => "퇴장 시 삭제".into(),
        ChatTtl::Seconds(s) => format!(
            "수명 {}",
            humantime::format_duration(Duration::from_secs(s.into()))
        ),
    }
}

fn channel_flags(s: &ChannelSettings) -> String {
    let mut f = Vec::new();
    if s.security == SecurityLevel::Volatile {
        f.push("휘발성".to_string());
    }
    if s.join_mode == JoinMode::JoinLeave {
        f.push("입장·퇴장".into());
    }
    let ttl = ttl_text(s.ttl);
    if !ttl.is_empty() {
        f.push(ttl);
    }
    if s.history == HistoryVisibility::Public {
        f.push("과거 기록 공개".into());
    }
    if s.is_private() {
        f.push("비공개".into());
    }
    f.join(", ")
}

fn sign_ok(ok: bool) -> &'static str {
    if ok { "성공" } else { "실패" }
}

pub fn spaces(list: &[SpaceSummary]) -> String {
    if list.is_empty() {
        return "공간이 없습니다. `yangpa crew create <이름>`으로 만들 수 있습니다.\n".into();
    }
    let mut t = Table::new(["이름", "종류", "멤버", "읽지 않음", "멘션", "상태", "ID"]);
    for s in list {
        t.row(vec![
            s.name.clone(),
            space_kind(s.kind).into(),
            s.member_count.to_string(),
            s.unread.to_string(),
            s.mention.to_string(),
            if s.dormant { "휴면" } else { "" }.into(),
            s.space.to_string(),
        ]);
    }
    t.render()
}

pub fn space(v: &SpaceView) -> String {
    let mut out = String::new();
    let owner = member_name(&v.members, v.owner);
    let _ = writeln!(
        out,
        "{} ({})",
        one_line(&v.settings.name),
        space_kind(v.kind)
    );
    let _ = writeln!(out, "ID: {}", v.space);
    if !v.settings.description.is_empty() {
        let _ = writeln!(out, "설명: {}", one_line(&v.settings.description));
    }
    let _ = writeln!(out, "소유자: {owner}");
    let _ = writeln!(
        out,
        "멤버: {}명, 역할: {}개, 채널: {}개, 이모지: {}개",
        v.members.len(),
        v.roles.len(),
        v.channels.len(),
        v.emojis.len()
    );
    let _ = writeln!(
        out,
        "내 권한: {}",
        if v.my_permissions.is_empty() {
            "-".into()
        } else {
            v.my_permissions.join(", ")
        }
    );
    if v.pending_commit {
        let _ = writeln!(
            out,
            "주의: 커밋 확정 기기가 오프라인이라 멤버·권한 변경이 대기 중입니다."
        );
    }
    if let Some(u) = v.pending_crew_conversion {
        let _ = writeln!(
            out,
            "크루 전환 요청 대기 중(요청자: {})",
            member_name(&v.members, u)
        );
    }
    if v.dormant {
        let _ = writeln!(out, "휴면 공간(주기 동기화 안 함)");
    }
    if !v.channels.is_empty() {
        out.push('\n');
        out.push_str(&channels(&v.channels));
    }
    out
}

fn member_name(members: &[MemberView], user: UserId) -> String {
    members
        .iter()
        .find(|m| m.user == user)
        .map(|m| one_line(m.nickname.as_deref().unwrap_or(&m.profile.name)))
        .unwrap_or_else(|| user.to_string())
}

/// 채널을 카테고리 아래로 묶어 위치 순으로 출력합니다.
pub fn channels(list: &[ChannelView]) -> String {
    let mut sorted: Vec<&ChannelView> = list.iter().collect();
    sorted.sort_by(|a, b| {
        (a.settings.position, &a.settings.name).cmp(&(b.settings.position, &b.settings.name))
    });
    let is_category = |id: ChannelId| {
        list.iter()
            .any(|c| c.channel == id && c.settings.kind == ChannelKind::Category)
    };
    let mut t = Table::new(["채널", "종류", "설정", "읽지 않음", "ID"]);
    let mut push = |c: &ChannelView, indent: bool| {
        let prefix = match (c.settings.kind, indent) {
            (ChannelKind::Category, _) => String::new(),
            (_, true) => "  #".into(),
            (_, false) => "#".into(),
        };
        let mut flags = channel_flags(&c.settings);
        if c.joined == Some(true) {
            flags = join_flag(flags, "입장함");
        }
        if c.api_read_allowed && c.settings.is_volatile() {
            flags = join_flag(flags, "API 읽기 허용");
        }
        t.row(vec![
            format!("{prefix}{}", c.settings.name),
            channel_kind(c.settings.kind),
            flags,
            if c.unread > 0 {
                c.unread.to_string()
            } else {
                String::new()
            },
            c.channel.to_string(),
        ]);
    };
    for c in &sorted {
        let top = c.settings.parent.is_none_or(|p| !is_category(p));
        if !top {
            continue;
        }
        push(c, false);
        if c.settings.kind == ChannelKind::Category {
            for child in sorted
                .iter()
                .filter(|x| x.settings.parent == Some(c.channel))
            {
                push(child, true);
            }
        }
    }
    if t.is_empty() {
        return "채널이 없습니다.\n".into();
    }
    t.render()
}

fn join_flag(mut flags: String, f: &str) -> String {
    if !flags.is_empty() {
        flags.push_str(", ");
    }
    flags.push_str(f);
    flags
}

pub fn roles(list: &[Role]) -> String {
    let mut sorted: Vec<&Role> = list.iter().collect();
    sorted.sort_by(|a, b| b.level().cmp(&a.level()).then_with(|| a.name.cmp(&b.name)));
    let mut t = Table::new(["역할", "순위", "색", "권한", "ID"]);
    for r in sorted {
        let perms = if r.permissions.contains(Permissions::ADMINISTRATOR) {
            "administrator(전체)".to_string()
        } else {
            r.permissions.names().join(",")
        };
        t.row(vec![
            format!("{}{}", r.name, if r.automation { " [자동화]" } else { "" }),
            r.rank.to_string(),
            format!("#{:06x}", r.color),
            perms,
            r.id.to_string(),
        ]);
    }
    t.render()
}

pub fn members(list: &[MemberView], roles: &[Role]) -> String {
    let role_name = |id: &RoleId| {
        roles
            .iter()
            .find(|r| r.id == *id)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let mut t = Table::new(["이름", "닉네임", "역할", "접속", "ID"]);
    for m in list {
        let mut name = m.profile.name.clone();
        if m.automation {
            name.push_str(" [자동화]");
        }
        if m.blocked {
            name.push_str(" [차단]");
        }
        t.row(vec![
            name,
            m.nickname.clone().unwrap_or_default(),
            m.roles.iter().map(role_name).collect::<Vec<_>>().join(","),
            if m.online { "온라인" } else { "" }.into(),
            m.user.to_string(),
        ]);
    }
    t.render()
}

/// 메시지 목록. 오래된 것부터 위에서 아래로.
pub fn messages(r: &MessageListResult, names: &HashMap<UserId, String>) -> String {
    let mut msgs: Vec<&MessageView> = r.messages.iter().collect();
    msgs.sort_by_key(|m| (m.created_at.0, m.lamport));
    let mut out = String::new();
    if !r.gaps.is_empty() {
        let _ = writeln!(out, "(받지 않은 기록 구간 {}개가 있습니다)", r.gaps.len());
    }
    for m in msgs {
        out.push_str(&message_line(m, names));
    }
    if out.is_empty() {
        out.push_str("메시지가 없습니다.\n");
    }
    out
}

pub fn message_line(m: &MessageView, names: &HashMap<UserId, String>) -> String {
    let author = names
        .get(&m.author)
        .map(|n| one_line(n))
        .unwrap_or_else(|| m.author.short());
    let mut out = format!("[{}] {author}", format_time(m.created_at));
    if m.volatile {
        out.push_str(" (휘발성)");
    }
    if m.deleted {
        out.push_str(": (삭제된 메시지)\n");
        let _ = writeln!(out, "    id {}", m.id);
        return out;
    }
    // 차단한 사람의 메시지는 접어 둡니다. 내용이 필요하면 --json으로 봅니다.
    if m.author_blocked {
        out.push_str(": (차단한 사용자의 메시지)\n");
        let _ = writeln!(out, "    id {}", m.id);
        return out;
    }
    if let Some(r) = m.reply_to {
        let _ = write!(out, " (답장 {})", r.short());
    }
    out.push_str(": ");
    let text = sanitize_text(&m.text);
    let mut lines = text.lines();
    out.push_str(lines.next().unwrap_or(""));
    for l in lines {
        out.push_str("\n    ");
        out.push_str(l);
    }
    if m.edited_at.is_some() {
        out.push_str(" (수정됨)");
    }
    out.push('\n');
    for a in &m.attachments {
        let _ = writeln!(
            out,
            "    첨부: {} ({}, {}){} blob {}",
            one_line(&a.name),
            one_line(&a.mime),
            format_size(a.size),
            if a.downloaded { "" } else { " 받지 않음" },
            a.blob
        );
    }
    for e in &m.embeds {
        let _ = writeln!(out, "    링크: {} {}", one_line(&e.title), one_line(&e.url));
    }
    if !m.reactions.is_empty() {
        let r: Vec<String> = m
            .reactions
            .iter()
            .map(|r| {
                format!(
                    "{}×{}{}",
                    one_line(&r.emoji),
                    r.count,
                    if r.me { "(나)" } else { "" }
                )
            })
            .collect();
        let _ = writeln!(out, "    반응: {}", r.join(" "));
    }
    if let Some(exp) = m.expires_at {
        let _ = writeln!(out, "    만료: {}", format_time(exp));
    }
    let _ = writeln!(out, "    id {}", m.id);
    out
}

pub fn search_results(page: &MessageSearchPage) -> String {
    if page.messages.is_empty() {
        return "결과가 없습니다.\n".into();
    }
    let names = HashMap::new();
    let mut out = String::new();
    let per = page.messages.len().max(1) as u32;
    let _ = writeln!(
        out,
        "{}개 중 {}–{}번째 ({}ms)",
        page.total,
        page.offset + 1,
        page.offset + page.messages.len() as u32,
        page.took_ms
    );
    if page.total > page.offset + per {
        let _ = writeln!(out, "다음 쪽: --page {}", page.offset / per + 2);
    }
    for m in &page.messages {
        if let (Some(s), Some(c)) = (m.space, m.channel) {
            let _ = writeln!(out, "공간 {s} / 채널 {c}");
        }
        out.push_str(&message_line(m, &names));
    }
    out
}

pub fn forum_posts(list: &[ForumPostView]) -> String {
    let mut t = Table::new(["제목", "답글", "최근 활동", "표시", "ID"]);
    for p in list {
        let mut flags = Vec::new();
        if p.pinned {
            flags.push("고정");
        }
        if p.locked {
            flags.push("잠김");
        }
        t.row(vec![
            p.title.clone(),
            p.reply_count.to_string(),
            format_time(p.last_activity),
            flags.join(","),
            p.id.to_string(),
        ]);
    }
    t.render()
}

pub fn emojis(list: &[EmojiView]) -> String {
    let mut t = Table::new(["이름", "움직임", "사용", "ID"]);
    for e in list {
        t.row(vec![
            e.name.clone(),
            if e.animated { "예" } else { "" }.into(),
            format!("<:{}:{}>", e.name, e.id),
            e.id.to_string(),
        ]);
    }
    t.render()
}

pub fn invites(list: &[InviteView], roles: &[Role]) -> String {
    if list.is_empty() {
        return "유효한 초대가 없습니다.\n".into();
    }
    let mut t = Table::new(["토큰 해시", "사용", "만료", "역할"]);
    for i in list {
        let uses = match i.max_uses {
            Some(m) => format!("{}/{m}", i.uses),
            None => i.uses.to_string(),
        };
        let r: Vec<String> = i
            .roles
            .iter()
            .map(|id| {
                roles
                    .iter()
                    .find(|r| r.id == *id)
                    .map(|r| r.name.clone())
                    .unwrap_or_else(|| id.to_string())
            })
            .collect();
        t.row(vec![
            i.token_hash.to_string(),
            uses,
            i.expires_at
                .map(format_time)
                .unwrap_or_else(|| "없음".into()),
            r.join(","),
        ]);
    }
    t.render()
}

fn types_text(t: &[DataType]) -> String {
    if t.is_empty() {
        return "없음".into();
    }
    t.iter().map(|t| t.name()).collect::<Vec<_>>().join(",")
}

fn rate_text(r: Option<u64>) -> String {
    match r {
        Some(b) => format!("{}/s", format_size(b)),
        None => "무제한".into(),
    }
}

pub fn seed_policy(p: &SeederPolicy, channel_name: &dyn Fn(ChannelId) -> String) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "시딩: {}", if p.enabled { "켜짐" } else { "꺼짐" });
    let _ = writeln!(out, "종류: {}", types_text(&p.types));
    // 채널 이름은 크루 멤버가 정한 값이라 표 밖에서도 제어 문자를 지웁니다.
    let channel_name = |c: ChannelId| one_line(&channel_name(c));
    let scope = match &p.include_channels {
        Some(list) => list
            .iter()
            .map(|c| channel_name(*c))
            .collect::<Vec<_>>()
            .join(", "),
        None if p.all_channels => "볼 수 있는 모든 채널".into(),
        None => "내가 연 채널".into(),
    };
    let _ = writeln!(out, "채널: {scope}");
    if !p.exclude_channels.is_empty() {
        let ex: Vec<String> = p
            .exclude_channels
            .iter()
            .map(|c| channel_name(*c))
            .collect();
        let _ = writeln!(out, "제외 채널: {}", ex.join(", "));
    }
    if !p.excluded_events.is_empty() {
        let _ = writeln!(out, "제외 메시지: {}개", p.excluded_events.len());
    }
    let _ = writeln!(out, "업로드 상한: {}", rate_text(p.upload_limit_bps));
    let _ = writeln!(out, "동시에 보내 줄 기기: {}대", p.max_concurrent);
    if p.prefetch {
        let _ = writeln!(out, "미리 받기: 켜짐(열지 않은 채널의 첨부도 받아 둠)");
    }
    out
}

/// `blob cache status`.
pub fn blob_cache(v: &BlobCacheView) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "받은 첨부: {} / 한도 {} ({}개)",
        format_size(v.cache_bytes),
        format_size(v.limit_bytes),
        v.cache_count
    );
    let _ = writeln!(
        out,
        "고정한 첨부: {} ({}개)",
        format_size(v.pinned_bytes),
        v.pinned_count
    );
    let _ = writeln!(
        out,
        "늘 남김(내 첨부·이모지·프로필 이미지): {} ({}개)",
        format_size(v.kept_bytes),
        v.kept_count
    );
    let free = v.disk_free_bytes.map_or("알 수 없음".into(), format_size);
    let _ = writeln!(
        out,
        "디스크 여유: {free} (최소 {} 남김)",
        format_size(v.min_free_bytes)
    );
    if v.evicted_count > 0 {
        let _ = writeln!(
            out,
            "켜진 뒤 정리: {}개, {}",
            v.evicted_count,
            format_size(v.evicted_bytes)
        );
    }
    out
}

/// `blob pins`.
pub fn blob_pins(list: &[BlobPinView], space_name: &dyn Fn(SpaceId) -> String) -> String {
    if list.is_empty() {
        return "고정한 블롭이 없습니다.\n".into();
    }
    let mut t = Table::new(["블롭", "공간", "크기", "받음", "고정 시각"]);
    for p in list {
        t.row(vec![
            p.blob.to_string(),
            one_line(&space_name(p.space)),
            format_size(p.size),
            yes(p.downloaded).into(),
            format_time(p.pinned_at),
        ]);
    }
    t.render()
}

/// `crew committer`.
pub fn committers(list: &[CommitterCandidate], names: &HashMap<UserId, String>) -> String {
    let mut t = Table::new(["", "기기", "이름", "사용자", "온라인"]);
    for c in list {
        let mut mark = String::new();
        if c.current {
            mark.push_str("확정");
        }
        if c.mine {
            mark = join_flag(mark, "이 기기");
        }
        t.row(vec![
            mark,
            c.device.to_string(),
            one_line(&c.name),
            names
                .get(&c.user)
                .map_or_else(|| c.user.to_string(), |n| one_line(n)),
            yes(c.online).into(),
        ]);
    }
    t.render()
}

pub fn peer_policy(p: &PeerPolicy) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "받을 종류: {}", types_text(&p.types));
    if !p.priorities.is_empty() {
        let pr: Vec<String> = p
            .priorities
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let _ = writeln!(out, "우선순위: {}", pr.join(","));
    }
    let _ = writeln!(out, "다운로드 상한: {}", rate_text(p.download_limit_bps));
    let _ = writeln!(
        out,
        "자동 다운로드: {} 이하",
        format_size(p.auto_download_max_bytes)
    );
    out
}

pub fn coverage(list: &[ChannelCoverage], channel_name: &dyn Fn(ChannelId) -> String) -> String {
    let mut t = Table::new(["채널", "온라인 시더", "온라인 보관 노드", ""]);
    for c in list {
        t.row(vec![
            channel_name(c.channel),
            c.online_seeders.to_string(),
            c.online_archives.to_string(),
            if c.online_seeders == 0 && c.online_archives == 0 {
                "경고: 지금 이 채널 기록을 줄 수 있는 기기가 없습니다"
            } else {
                ""
            }
            .into(),
        ]);
    }
    t.render()
}

/// 크루에 지정된 보관 노드(`archive.list`).
pub fn archives(list: &[ArchiveView], channel_name: &dyn Fn(ChannelId) -> String) -> String {
    if list.is_empty() {
        return "지정된 보관 노드가 없습니다.\n".into();
    }
    let mut t = Table::new(["이름", "기기", "연결", "함께 보관하는 비공개 채널", "지정"]);
    for a in list {
        let private: Vec<String> = a
            .private_channels
            .iter()
            .map(|c| channel_name(*c))
            .collect();
        t.row(vec![
            one_line(&a.name),
            a.device.to_string(),
            if a.online {
                "온라인"
            } else {
                "오프라인"
            }
            .into(),
            if private.is_empty() {
                "없음".into()
            } else {
                private.join(",")
            },
            format_time(a.granted_at),
        ]);
    }
    t.render()
}

/// 보관 노드가 받아 둔 크루(`archive.hosted`). 내용을 읽을 수 없어 수치만 보입니다.
pub fn archive_hosted(list: &[ArchiveHostedView]) -> String {
    if list.is_empty() {
        return "받아 둔 크루가 없습니다.\n".into();
    }
    let mut t = Table::new([
        "공간",
        "멤버",
        "채널",
        "이벤트",
        "첨부",
        "용량",
        "명단 발행",
    ]);
    for h in list {
        t.row(vec![
            h.space.to_string(),
            h.members.to_string(),
            h.channels.to_string(),
            h.events.to_string(),
            h.blobs.to_string(),
            format_size(h.used_bytes),
            format_time(h.roster_issued_at),
        ]);
    }
    t.render()
}

pub fn net_status(n: &NetStatus) -> String {
    let mut out = String::new();
    let mode = match n.mode {
        NetMode::Normal => "직접 연결",
        NetMode::Tor => "Tor",
    };
    let nat = match n.nat {
        NatStatus::Unknown => "확인 중",
        NatStatus::Public => "공인(직접 연결 가능)",
        NatStatus::Private => "사설(릴레이·홀펀칭)",
        NatStatus::Disabled => "확인 안 함",
    };
    let _ = writeln!(out, "모드: {mode}");
    let _ = writeln!(out, "피어 ID: {}", n.peer_id);
    let _ = writeln!(out, "NAT: {nat}");
    let _ = writeln!(out, "연결: {}개, 인증된 피어: {}개", n.connections, n.peers);
    if let Some(p) = n.bootstrap_progress {
        let _ = writeln!(out, "부트스트랩: {p}%");
    }
    for (label, list) in [
        ("수신 주소", &n.listen_addrs),
        ("외부 주소", &n.external_addrs),
        ("릴레이 주소", &n.relay_addrs),
    ] {
        if !list.is_empty() {
            let _ = writeln!(out, "{label}: {}", list.join(", "));
        }
    }
    if let Some(o) = &n.onion_addr {
        let _ = writeln!(out, "onion 주소: {o}");
    }
    out
}

pub fn stun_servers(s: &StunServers) -> String {
    let mut out = String::new();
    if s.servers.is_empty() {
        out.push_str("STUN 서버: 쓰지 않음\n");
    } else {
        let _ = writeln!(
            out,
            "STUN 서버{}:",
            if s.is_default { "(기본 목록)" } else { "" }
        );
        for x in &s.servers {
            let _ = writeln!(out, "  {x}");
        }
    }
    out.push_str("P2P 연결(신뢰한 상대·P2P 전용 채널)이 공유기 밖 주소를 알아낼 때만 쓰고, Tor 연결에는 쓰지 않습니다.\n");
    out
}

pub fn status(s: &StatusResult, version: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(v) = version {
        let _ = writeln!(out, "데몬 버전: {v}");
    }
    match s.identity {
        Some(u) => {
            let _ = writeln!(out, "신원: {u}");
        }
        None => {
            let _ = writeln!(out, "신원: 없음(`yangpa identity create --name <이름>`)");
        }
    }
    if let Some(d) = s.device {
        let _ = writeln!(out, "기기: {d}");
    }
    let _ = writeln!(out, "공간: {}개", s.spaces);
    let tor = if s.tor.enabled {
        match s.tor.bootstrap_progress {
            Some(p) if p < 100 => format!("켜짐(부트스트랩 {p}%)"),
            _ => "켜짐".into(),
        }
    } else {
        "꺼짐".into()
    };
    let _ = writeln!(out, "Tor: {tor}");
    out.push_str(&net_status(&s.net));
    out
}

/// 승인을 기다리는 기기 한 줄. 같은 링크를 다른 기기도 썼으면 경고를 붙입니다.
pub fn device_link_request(v: &DeviceLinkRequestView) -> String {
    let mut out = format!(
        "{} 기기가 승인을 기다립니다. 번호: {}\n",
        one_line(&v.name),
        v.code
    );
    if v.contested {
        out.push_str("다른 기기도 이 링크를 썼습니다. 내 새 기기가 맞는지 확인하세요.\n");
    }
    out
}

pub fn identity(v: &IdentityView) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "사용자: {}", v.user);
    let _ = writeln!(out, "이 기기: {}", v.device);
    if v.first_syncing {
        out.push_str("첫 동기화 중\n");
    }
    out.push_str(&profile(&v.profile));
    out.push('\n');
    out.push_str(&devices(&v.devices, Some(v.device)));
    out
}

pub fn profile(p: &Profile) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "이름: {}", one_line(&p.name));
    if !p.status.is_empty() {
        let _ = writeln!(out, "상태: {}", one_line(&p.status));
    }
    if !p.bio.is_empty() {
        let _ = writeln!(out, "소개: {}", sanitize_text(&p.bio));
    }
    if let Some(c) = p.theme_color {
        let _ = writeln!(out, "테마 색: #{c:06x}");
    }
    out
}

pub fn devices(list: &[DeviceEntry], me: Option<yangpa_core::ids::DeviceId>) -> String {
    let mut t = Table::new(["기기", "추가", "상태", "ID"]);
    for d in list {
        let state = if d.revoked_at.is_some() {
            "폐기됨"
        } else if Some(d.device) == me {
            "이 기기"
        } else {
            ""
        };
        t.row(vec![
            d.name.clone(),
            format_time(d.added_at),
            state.into(),
            d.device.to_string(),
        ]);
    }
    t.render()
}

pub fn friends(list: &[FriendView]) -> String {
    if list.is_empty() {
        return "연락처가 없습니다. `yangpa friend link create`로 연락처 링크를 만들어 보내세요.\n"
            .into();
    }
    let mut t = Table::new(["이름", "종류", "접속", "안전 번호 확인", "ID"]);
    for f in list {
        let kind = match f.kind {
            ContactKind::Friend => "친구".to_string(),
            ContactKind::Temporary => match f.expires_at {
                Some(t) => format!("임시(~{})", format_time(t)),
                None => "임시".into(),
            },
            ContactKind::Expired => "만료".into(),
        };
        let kind = if f.proposed {
            format!("{kind}, 친구 추가 보냄")
        } else {
            kind
        };
        t.row(vec![
            f.profile.name.clone(),
            kind,
            if f.online { "온라인" } else { "" }.into(),
            if f.safety_number_verified {
                "확인함"
            } else {
                ""
            }
            .into(),
            f.user.to_string(),
        ]);
    }
    t.render()
}

pub fn friend_requests(list: &[FriendRequestView]) -> String {
    if list.is_empty() {
        return "받은 친구 요청이 없습니다.\n".into();
    }
    let mut t = Table::new(["이름", "받은 시각", "요청 ID", "사용자 ID"]);
    for r in list {
        t.row(vec![
            r.profile.name.clone(),
            format_time(r.received_at),
            r.request_id.clone(),
            r.user.to_string(),
        ]);
    }
    t.render()
}

pub fn friend_links(list: &[FriendLinkView]) -> String {
    let mut t = Table::new(["토큰 ID", "만든 시각", "만료", "1회용", "사용"]);
    for l in list {
        t.row(vec![
            l.token_id.clone(),
            format_time(l.created_at),
            l.expires_at
                .map(format_time)
                .unwrap_or_else(|| "없음".into()),
            if l.single_use { "예" } else { "" }.into(),
            l.uses.to_string(),
        ]);
    }
    t.render()
}

pub fn peers(list: &[PeerView]) -> String {
    let mut t = Table::new(["피어 ID", "사용자", "RTT", "경로"]);
    for p in list {
        t.row(vec![
            p.peer_id.clone(),
            opt(p.user),
            p.rtt_ms
                .map(|r| format!("{r}ms"))
                .unwrap_or_else(|| "-".into()),
            if p.relayed { "릴레이" } else { "직접" }.into(),
        ]);
    }
    t.render()
}

pub fn tokens(list: &[TokenView], space_name: &dyn Fn(SpaceId) -> String) -> String {
    let mut t = Table::new(["이름", "범위", "공간", "자동화", "마지막 사용", "ID"]);
    for tk in list {
        let spaces = if tk.spaces.is_empty() {
            "전체".to_string()
        } else {
            tk.spaces
                .iter()
                .map(|s| space_name(*s))
                .collect::<Vec<_>>()
                .join(",")
        };
        t.row(vec![
            tk.name.clone(),
            tk.scopes
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(","),
            spaces,
            if tk.automation { "예" } else { "" }.into(),
            tk.last_used_at
                .map(format_time)
                .unwrap_or_else(|| "-".into()),
            tk.id.clone(),
        ]);
    }
    t.render()
}

pub fn audit(list: &[AuditEntry]) -> String {
    let mut t = Table::new(["ID", "시각", "메서드", "토큰", "결과"]);
    for e in list {
        let result = match e.code {
            Some(c) if !e.ok => format!("{} ({c})", sign_ok(false)),
            _ => sign_ok(e.ok).into(),
        };
        let token = if e.automation {
            format!("{} [자동화]", e.token_name)
        } else {
            e.token_name.clone()
        };
        t.row(vec![
            e.id.to_string(),
            format_time(e.at),
            e.method.clone(),
            token,
            result,
        ]);
    }
    t.render()
}

pub fn confirmations(list: &[PendingConfirmation]) -> String {
    if list.is_empty() {
        return "확인 대기 중인 작업이 없습니다.\n".into();
    }
    let mut out = String::new();
    for c in list {
        out.push_str(&confirmation(c));
        out.push('\n');
    }
    out.push_str("승인: yangpa confirm approve <ID>   거절: yangpa confirm deny <ID>\n");
    out
}

pub fn confirmation(c: &PendingConfirmation) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ID {}: {} (요청 토큰: {})",
        c.id,
        c.method,
        one_line(&c.token_name)
    );
    if !c.summary.is_empty() {
        let _ = writeln!(out, "  내용: {}", one_line(&c.summary));
    }
    let _ = writeln!(out, "  요청 시각: {}", format_time(c.requested_at));
    if let Some(e) = c.expires_at {
        let _ = writeln!(out, "  만료: {}", format_time(e));
    }
    let params = serde_json::to_string(&c.params).unwrap_or_default();
    let _ = writeln!(out, "  인자: {}", one_line(&params));
    out
}

fn profile_name(p: Option<&Profile>, user: UserId, names: &HashMap<UserId, String>) -> String {
    names
        .get(&user)
        .cloned()
        .or_else(|| p.map(|p| p.name.clone()))
        .unwrap_or_else(|| user.short())
}

pub fn bans(list: &[BanView], names: &HashMap<UserId, String>) -> String {
    if list.is_empty() {
        return "밴된 사용자가 없습니다.\n".into();
    }
    let mut t = Table::new(["이름", "사유", "밴한 사람", "시각", "ID"]);
    for b in list {
        t.row(vec![
            profile_name(b.profile.as_ref(), b.user, &HashMap::new()),
            b.reason.clone().unwrap_or_default(),
            profile_name(None, b.banned_by, names),
            format_time(b.banned_at),
            b.user.to_string(),
        ]);
    }
    t.render()
}

pub fn blocked(list: &[BlockedView]) -> String {
    if list.is_empty() {
        return "차단한 사용자가 없습니다.\n".into();
    }
    let mut t = Table::new(["이름", "차단한 시각", "ID"]);
    for b in list {
        t.row(vec![
            profile_name(b.profile.as_ref(), b.user, &HashMap::new()),
            format_time(b.blocked_at),
            b.user.to_string(),
        ]);
    }
    t.render()
}

pub fn user_devices(list: &[UserDeviceView]) -> String {
    if list.is_empty() {
        return "이 기기가 아는 그 사람의 기기가 없습니다.\n".into();
    }
    let mut t = Table::new(["기기", "상태", "추가한 시각", "ID"]);
    for d in list {
        let state = if d.blocked { "차단함" } else { "활성" };
        t.row(vec![
            d.name.clone(),
            state.into(),
            format_time(d.added_at),
            d.device.to_string(),
        ]);
    }
    t.render()
}

pub fn blocked_devices(list: &[BlockedDeviceView]) -> String {
    if list.is_empty() {
        return "차단한 기기가 없습니다.\n".into();
    }
    let mut t = Table::new(["사용자", "기기", "차단한 시각", "기기 ID"]);
    for b in list {
        t.row(vec![
            profile_name(b.profile.as_ref(), b.user, &HashMap::new()),
            b.name.clone(),
            format_time(b.blocked_at),
            b.device.to_string(),
        ]);
    }
    t.render()
}

pub fn reports(
    list: &[ReportView],
    names: &HashMap<UserId, String>,
    channels: &HashMap<ChannelId, String>,
) -> String {
    if list.is_empty() {
        return "신고가 없습니다.\n".into();
    }
    let mut out = String::new();
    for r in list {
        let status = match r.status {
            ReportStatus::Open => "열림",
            ReportStatus::Resolved => "처리됨",
        };
        let _ = writeln!(
            out,
            "[{status}] {} {}",
            format_time(r.reported_at),
            r.report_id
        );
        let _ = writeln!(
            out,
            "  신고자: {}  채널: {}  메시지: {}",
            one_line(&profile_name(None, r.reporter, names)),
            one_line(
                &channels
                    .get(&r.channel)
                    .cloned()
                    .unwrap_or_else(|| r.channel.to_string())
            ),
            r.message
        );
        let _ = writeln!(out, "  이유: {}", one_line(&r.reason));
        if let Some(res) = &r.resolution {
            let _ = writeln!(out, "  처리: {}", one_line(res));
        }
    }
    out.push_str(
        "처리: yangpa report resolve --crew <크루> <신고 ID> --action dismiss|delete-message|kick|ban\n",
    );
    out
}

pub fn metrics(m: &DiagMetrics) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "메시지 전달 지연: p50 {}ms, p95 {}ms",
        m.delivery_ms_p50, m.delivery_ms_p95
    );
    let _ = writeln!(out, "메모리: {}", format_size(m.memory_bytes));
    let _ = writeln!(out, "CPU: {:.1}%", m.cpu_percent);
    let _ = writeln!(out, "초당 이벤트: {:.1}", m.events_per_sec);
    for (k, v) in &m.queues {
        let _ = writeln!(out, "큐 {k}: {v}");
    }
    out
}

/// `watch` 한 줄. 휘발성 메시지 내용은 `show_volatile`일 때만 보입니다.
pub fn notification_line(n: &Notification, show_volatile: bool) -> String {
    match n {
        Notification::MessageNew {
            space,
            channel,
            message,
            notify,
        } => {
            let body = if message.volatile && !show_volatile {
                "(휘발성 메시지, 내용 숨김)".to_string()
            } else if message.text.is_empty() {
                notify
                    .body
                    .clone()
                    .map(|b| one_line(&b))
                    .unwrap_or_default()
            } else {
                one_line(&message.text)
            };
            format!(
                "[새 메시지] {} | {}: {} (공간 {} 채널 {} id {})",
                one_line(&notify.title),
                message.author.short(),
                body,
                space.short(),
                channel.short(),
                message.id
            )
        }
        Notification::MessageUpdated {
            channel, message, ..
        } => {
            let what = if message.deleted { "삭제" } else { "변경" };
            format!("[메시지 {what}] 채널 {} id {}", channel.short(), message.id)
        }
        Notification::MessageExpired { channel, ids, .. } => {
            format!("[메시지 만료] 채널 {} {}개", channel.short(), ids.len())
        }
        Notification::SpaceUpdated { space } => format!("[공간 변경] {space}"),
        Notification::FriendRequest {
            request_id,
            profile,
            ..
        } => format!(
            "[친구 요청] {} (수락: yangpa friend accept {request_id})",
            one_line(&profile.name)
        ),
        Notification::FriendAccepted { user, space } => {
            format!("[연락처] {} 공간 {space}", user.short())
        }
        Notification::FriendUpdated { user } => format!("[연락처] {} 바뀜", user.short()),
        Notification::Presence {
            user, online, away, ..
        } => format!(
            "[접속] {} {}",
            user.short(),
            match (*online, *away) {
                (true, true) => "자리 비움",
                (true, false) => "온라인",
                _ => "오프라인",
            }
        ),
        Notification::PresenceSelf(p) => format!("[접속] 내 표시 상태 {:?}", p.mode),
        Notification::Flair(f) => format!(
            "[후원] 사용자 {}의 후원자 표시가 바뀌었습니다",
            f.user.short()
        ),
        Notification::ProfileUpdated { user } => {
            format!("[프로필] 사용자 {}의 프로필이 바뀌었습니다", user.short())
        }
        Notification::BadgesUpdated { user } => {
            format!("[뱃지] 사용자 {}의 뱃지가 바뀌었습니다", user.short())
        }
        Notification::Typing {
            channel,
            user,
            anon_name,
            ..
        } => {
            let who = anon_name.clone().unwrap_or_else(|| user.short());
            format!("[입력 중] 채널 {} {who}", channel.short())
        }
        Notification::VoiceSignal { channel, from, .. } => {
            format!("[음성 신호] 채널 {} 기기 {}", channel.short(), from.short())
        }
        Notification::VoiceMedia { channel, from, .. } => {
            format!("[통화 소리] 채널 {} 기기 {}", channel.short(), from.short())
        }
        Notification::VoiceParticipants {
            channel,
            participants,
            generation,
            ..
        } => format!(
            "[음성 참가자] 채널 {} {}명(세대 {generation})",
            channel.short(),
            participants.len()
        ),
        Notification::BlobProgress {
            blob, have, total, ..
        } => {
            format!("[다운로드] {} {have}/{total}", blob.short())
        }
        Notification::NetStatus(s) => format!(
            "[네트워크] 연결 {}개, 피어 {}개{}",
            s.connections,
            s.peers,
            s.bootstrap_progress
                .map(|p| format!(", 부트스트랩 {p}%"))
                .unwrap_or_default()
        ),
        // 브리지 줄은 공개되지 않은 주소라 찍지 않습니다.
        Notification::Bridges(b) => format!(
            "[네트워크] 브리지 {}{}",
            if b.enabled { "켬" } else { "끔" },
            if b.auto {
                " (Tor에 들어가지 못해 Snowflake를 자동으로 켬)"
            } else {
                ""
            }
        ),
        Notification::TransferRates(r) => format!(
            "[전송] ↑ {}B/s ↓ {}B/s{}",
            r.up_bps,
            r.down_bps,
            if r.uploads.is_empty() {
                String::new()
            } else {
                format!(", 받는 사람 {}명", r.uploads.len())
            }
        ),
        Notification::ConfirmPending(c) => format!(
            "[사람 확인 필요] {} {} (승인: yangpa confirm approve {})",
            c.method,
            one_line(&c.summary),
            c.id
        ),
        Notification::SeedCoverageWarning { space, channel } => format!(
            "[시더 경고] 공간 {} 채널 {}: 온라인 시더가 없습니다",
            space.short(),
            channel.short()
        ),
        Notification::ReportNew { space, report_id } => format!(
            "[새 신고] 공간 {} 신고 {} (yangpa report list --crew {space})",
            space.short(),
            one_line(report_id)
        ),
        Notification::EventsDropped { count } => {
            format!("[경고] 출력이 밀려 이벤트 {count}개를 놓쳤습니다")
        }
        Notification::DeviceLinkRequest(v) => {
            format!("[기기 연결] {}", device_link_request(v).trim_end())
        }
        Notification::DeviceLinkResult { status } => match status {
            DeviceLinkStatus::Approved => {
                "[기기 연결] 승인됐습니다. 첫 동기화를 시작합니다".to_owned()
            }
            DeviceLinkStatus::Rejected => "[기기 연결] 거절됐습니다".to_owned(),
            DeviceLinkStatus::Expired => "[기기 연결] 링크가 끝났습니다".to_owned(),
            DeviceLinkStatus::Failed => "[기기 연결] 받은 묶음을 쓰지 못했습니다".to_owned(),
        },
        Notification::DeviceSynced => "[기기 연결] 첫 동기화를 마쳤습니다".to_owned(),
        Notification::SupporterUpdated { error } => match error {
            Some(e) => format!("[후원] 후원 확인 로그인 실패({e})"),
            None => "[후원] 후원 확인 상태가 바뀌었습니다".to_owned(),
        },
        Notification::UpdateProgress {
            version,
            done,
            total,
            ready,
            error,
        } => match (ready, error) {
            (_, Some(e)) => format!("[업데이트] {version} 파일 받기 실패({e})"),
            (true, None) => format!("[업데이트] {version} 파일을 받았습니다"),
            _ => format!("[업데이트] {version} 파일 받는 중 {done}/{total}"),
        },
        Notification::DeviceWiped => {
            "[기기 폐기] 다른 기기에서 이 기기를 폐기해 로컬 데이터를 모두 지웠습니다".to_owned()
        }
        Notification::CheckpointAlert { space, kind } => format!(
            "[체크포인트 경보] 공간 {}: {}",
            space.short(),
            match kind {
                CheckpointAlertKind::Conflict =>
                    "같은 순번의 다른 체크포인트를 봄(소유자 기기 확인)",
                CheckpointAlertKind::Equivocation =>
                    "한 기기가 같은 순번에 체크포인트를 둘 씀(탈취 의심)",
                CheckpointAlertKind::Fallback => "체크포인트 상태를 버리고 처음부터 따라잡는 중",
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::ids::{DeviceId, EventId};
    use yangpa_rpc::api::NotifyView;

    #[test]
    fn widths_and_tables() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("게임 모임"), 9);
        let mut t = Table::new(["이름", "ID"]);
        t.row(vec!["게임 모임".into(), "a".into()]);
        t.row(vec!["x".into(), "b".into()]);
        let s = t.render();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        // 두 번째 열이 같은 표시 폭에서 시작합니다.
        let col = |l: &str| display_width(&l[..l.rfind(' ').unwrap() + 1]);
        assert_eq!(col(lines[1]), col(lines[2]));
        assert_eq!(col(lines[0]), col(lines[1]));
    }

    #[test]
    fn control_chars_are_neutralized() {
        let mut t = Table::new(["이름"]);
        t.row(vec!["악성\x1b[31m\n이름".into()]);
        let s = t.render();
        assert!(!s.contains('\x1b'));
        assert_eq!(s.lines().count(), 2);
        assert_eq!(sanitize_text("a\x1b]0;x\x07\nb"), "a]0;x\nb");
        // C1 제어 문자(8비트 CSI)와 양방향 제어 문자도 지웁니다.
        assert_eq!(sanitize_text("a\u{9b}31mb"), "a31mb");
        assert_eq!(one_line("관리자\u{202E}gnp.exe"), "관리자gnp.exe");
        assert_eq!(sanitize_text("x\u{2066}y\u{2069}"), "xy");
    }

    #[test]
    fn seed_policy_channel_names_are_sanitized() {
        let p = SeederPolicy {
            enabled: true,
            types: vec![DataType::Text],
            include_channels: Some(vec![ChannelId([1; 16])]),
            all_channels: false,
            exclude_channels: vec![ChannelId([2; 16])],
            excluded_events: vec![],
            upload_limit_bps: None,
            max_concurrent: 4,
            prefetch: true,
        };
        let s = seed_policy(&p, &|_| "#악성\x1b]52;c;aGk=\x07".to_string());
        assert!(s.contains("미리 받기"));
        assert!(!s.contains('\x1b') && !s.contains('\x07'), "{s:?}");
        assert!(s.contains("#악성"));
    }

    #[test]
    fn coverage_counts_archives_separately() {
        let row = |seeders, archives| ChannelCoverage {
            channel: ChannelId([1; 16]),
            online_seeders: seeders,
            online_archives: archives,
        };
        let name = |_| "#일반".to_string();
        // 보관 노드만 온라인이어도 기록은 받을 수 있으니 경고하지 않습니다.
        assert!(!coverage(&[row(0, 1)], &name).contains("경고"));
        assert!(coverage(&[row(0, 0)], &name).contains("경고"));
        let s = coverage(&[row(2, 1)], &name);
        assert!(s.contains("온라인 보관 노드"), "{s}");
    }

    #[test]
    fn archive_names_are_sanitized() {
        let v = ArchiveView {
            device: DeviceId([3; 32]),
            user: UserId([2; 32]),
            name: "보관\x1b[31m함\n".into(),
            private_channels: vec![ChannelId([1; 16])],
            granted_by: UserId([4; 32]),
            granted_at: Timestamp(0),
            online: true,
        };
        let s = archives(&[v], &|_| "#운영".to_string());
        assert!(!s.contains('\x1b'), "{s:?}");
        assert!(s.contains("#운영") && s.contains("온라인"));
        assert!(archives(&[], &|_| String::new()).contains("없습니다"));
    }

    fn msg(volatile: bool) -> MessageView {
        MessageView {
            id: EventId([1; 32]),
            author: UserId([2; 32]),
            author_device: DeviceId([3; 32]),
            created_at: Timestamp(0),
            lamport: 1,
            text: "비밀 내용".into(),
            attachments: vec![],
            embeds: vec![],
            reply_to: None,
            reactions: vec![],
            edited_at: None,
            deleted: false,
            volatile,
            expires_at: None,
            thread: None,
            space: None,
            channel: None,
            author_blocked: false,
            anonymous: false,
            anon_name: None,
            mine: false,
            mentions_me: false,
        }
    }

    #[test]
    fn watch_hides_volatile_by_default() {
        let n = Notification::MessageNew {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            message: msg(true),
            notify: NotifyView {
                title: "모임".into(),
                body: None,
                sender: None,
            },
        };
        assert!(!notification_line(&n, false).contains("비밀 내용"));
        assert!(notification_line(&n, true).contains("비밀 내용"));
        let n = Notification::MessageNew {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            message: msg(false),
            notify: NotifyView {
                title: "모임".into(),
                body: None,
                sender: None,
            },
        };
        assert!(notification_line(&n, false).contains("비밀 내용"));
    }

    #[test]
    fn message_rendering() {
        let mut names = HashMap::new();
        names.insert(UserId([2; 32]), "철수".to_string());
        let r = MessageListResult {
            messages: vec![msg(false)],
            gaps: vec![],
        };
        let s = messages(&r, &names);
        assert!(
            s.contains("1970-01-01T00:00:00Z") && s.contains("철수: 비밀 내용"),
            "{s}"
        );
        let mut d = msg(false);
        d.deleted = true;
        assert!(message_line(&d, &names).contains("삭제된 메시지"));
        assert!(!message_line(&d, &names).contains("비밀 내용"));
    }

    #[test]
    fn moderation_rendering() {
        let names = HashMap::new();
        let mut m = msg(false);
        m.author_blocked = true;
        let line = message_line(&m, &names);
        assert!(line.contains("차단한 사용자의 메시지"), "{line}");
        assert!(!line.contains("비밀 내용"), "{line}");

        let profile = Profile {
            name: "악성\x1b[31m".into(),
            ..Profile::default()
        };
        let s = bans(
            &[BanView {
                user: UserId([5; 32]),
                profile: Some(profile.clone()),
                reason: Some("도배".into()),
                banned_by: UserId([6; 32]),
                banned_at: Timestamp(0),
            }],
            &names,
        );
        assert!(
            s.contains("도배") && s.contains("악성") && !s.contains('\x1b'),
            "{s}"
        );
        let s = blocked(&[BlockedView {
            user: UserId([5; 32]),
            profile: None,
            blocked_at: Timestamp(0),
        }]);
        assert!(s.contains(&UserId([5; 32]).to_string()), "{s}");
        assert!(blocked(&[]).contains("없습니다"));

        let r = ReportView {
            report_id: "r1".into(),
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            message: EventId([3; 32]),
            reporter: UserId([4; 32]),
            reason: "욕설\n\x1b]52;c;x\x07".into(),
            reported_at: Timestamp(0),
            status: ReportStatus::Resolved,
            resolution: Some("ban: 반복".into()),
        };
        let mut channels = HashMap::new();
        channels.insert(ChannelId([2; 16]), "#일반".to_string());
        let s = reports(&[r], &names, &channels);
        assert!(
            s.contains("[처리됨]") && s.contains("#일반") && s.contains("ban: 반복"),
            "{s}"
        );
        assert!(!s.contains('\x1b') && !s.contains('\x07'), "{s:?}");
        let n = Notification::ReportNew {
            space: SpaceId([1; 16]),
            report_id: "r1".into(),
        };
        assert!(notification_line(&n, false).contains("[새 신고]"));
    }

    #[test]
    fn channel_tree_groups_by_category() {
        let cat = ChannelId([1; 16]);
        let mut c1 = ChannelSettings::text("카테고리");
        c1.kind = ChannelKind::Category;
        let mut c2 = ChannelSettings::text("공지");
        c2.parent = Some(cat);
        let mut c3 = ChannelSettings::text("비밀");
        c3.security = SecurityLevel::Volatile;
        c3.ttl = ChatTtl::Seconds(30);
        let view = |id: u8, s: ChannelSettings| ChannelView {
            channel: ChannelId([id; 16]),
            settings: s,
            unread: 0,
            joined: None,
            participants: None,
            api_read_allowed: false,
            p2p: false,
            p2p_joined: false,
            anonymous: false,
            featured_post: None,
            attach_limit: None,
            no_edit: false,
            no_delete: false,
            volatile_cap: None,
            volatile_full: false,
            has_new: false,
        };
        let s = channels(&[view(3, c3), view(2, c2), view(1, c1)]);
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines[1].starts_with("#비밀") || lines[1].starts_with("카테고리"),
            "{s}"
        );
        let cat_line = lines
            .iter()
            .position(|l| l.starts_with("카테고리"))
            .unwrap();
        assert!(lines[cat_line + 1].starts_with("  #공지"), "{s}");
        assert!(s.contains("휘발성, 수명 30s"), "{s}");
    }
}
