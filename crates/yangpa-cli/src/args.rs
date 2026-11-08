//! 명령줄 구조(clap derive). 기획서의 `yangpa <대상> <동작> --crew …` 형태를 따릅니다.
//!
//! 이름으로 받는 값(`--crew "게임 모임"`, `--channel 공지`, 역할·멤버 이름)은 실행할 때 데몬 목록으로 ID를 찾습니다.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::parse::{
    ExpiryArg, NameList, RateArg, ScopeList, TtlArg, TypeList, parse_color, parse_duration,
    parse_expiry, parse_permissions, parse_priorities, parse_scopes, parse_size, parse_switch,
    parse_ttl, parse_types, rate_arg,
};
use std::collections::BTreeMap;
use yangpa_rpc::api::DataType;

/// YANGPA 명령줄 도구. 실행 중인 데몬(yangpad)의 로컬 API를 씁니다.
#[derive(Debug, Parser)]
#[command(
    name = "yangpa",
    version,
    propagate_version = true,
    max_term_width = 100
)]
pub struct Cli {
    /// 데몬 엔드포인트(Unix 소켓 경로 또는 Windows named pipe).
    #[arg(long, global = true, env = "YANGPA_ENDPOINT", value_name = "경로")]
    pub endpoint: Option<String>,

    /// API 토큰. 없으면 YANGPA_TOKEN, 그다음 소유자 토큰 파일을 씁니다.
    /// 명령줄 인자는 다른 사용자에게 보일 수 있으니 환경 변수를 권합니다.
    #[arg(
        long,
        global = true,
        env = "YANGPA_TOKEN",
        hide_env_values = true,
        value_name = "토큰"
    )]
    pub token: Option<String>,

    /// 결과를 JSON 그대로 출력합니다.
    #[arg(long, global = true)]
    pub json: bool,

    /// 요청마다 기다릴 최대 시간.
    #[arg(long, global = true, value_name = "기간", default_value = "30s", value_parser = parse_duration)]
    pub timeout: Duration,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 데몬 상태(신원, 네트워크, 공간 수, Tor).
    Status,
    /// CLI·데몬 버전.
    Version,
    /// 데몬을 종료합니다(소유자 토큰).
    Shutdown,
    /// 데몬(yangpad)을 GUI 없이 실행합니다. 뒤의 인자는 그대로 yangpad에 넘깁니다.
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Daemon(DaemonArgs),
    /// 이벤트를 실시간으로 출력합니다(Ctrl+C로 끝).
    Watch(WatchArgs),
    /// 신원(계정).
    #[command(subcommand)]
    Identity(IdentityCmd),
    /// 내 기기 목록·폐기.
    #[command(subcommand)]
    Device(DeviceCmd),
    /// 프로필.
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// 친구 메모(내 기기에만 저장).
    #[command(subcommand)]
    Note(NoteCmd),
    /// 친구·친구 링크.
    #[command(subcommand)]
    Friend(FriendCmd),
    /// 공간(1대1 스페이스와 크루) 목록·상세.
    #[command(subcommand)]
    Space(SpaceCmd),
    /// 크루 만들기·설정·나가기.
    #[command(subcommand)]
    Crew(CrewCmd),
    /// 멤버 목록·추방·밴·역할 지정.
    #[command(subcommand)]
    Member(MemberCmd),
    /// 사용자 차단(이 기기에만 저장).
    #[command(subcommand)]
    User(UserCmd),
    /// 신뢰 스위치: 내 IP를 보여도 되는 사람·크루(이 기기에만 저장).
    #[command(subcommand)]
    Trust(TrustCmd),
    /// 운영진이 받은 신고 목록·처리.
    #[command(subcommand, visible_alias = "reports")]
    Report(ReportCmd),
    /// 채널.
    #[command(subcommand)]
    Channel(ChannelCmd),
    /// 메시지.
    #[command(subcommand, alias = "msg")]
    Message(MessageCmd),
    /// 포럼 글.
    #[command(subcommand)]
    Forum(ForumCmd),
    /// 첨부 파일(블롭).
    #[command(subcommand)]
    Blob(BlobCmd),
    /// 커스텀 이모지.
    #[command(subcommand)]
    Emoji(EmojiCmd),
    /// 역할.
    #[command(subcommand)]
    Role(RoleCmd),
    /// 초대 링크.
    #[command(subcommand)]
    Invite(InviteCmd),
    /// 시더(다른 멤버에게 데이터 제공) 설정.
    #[command(subcommand)]
    Seed(SeedCmd),
    /// 보관 전용 노드(복호화 키 없이 암호문만 보관·제공): 크루 쪽 지정·해제, 보관 노드 쪽 관리.
    #[command(subcommand)]
    Archive(ArchiveCmd),
    /// 피어(내가 받아 올 데이터) 설정.
    #[command(subcommand)]
    Peer(PeerCmd),
    /// 네트워크·Tor.
    #[command(subcommand)]
    Net(NetCmd),
    /// API 토큰.
    #[command(subcommand)]
    Token(TokenCmd),
    /// 감사 로그.
    #[command(subcommand)]
    Audit(AuditCmd),
    /// AI(자동화 토큰)가 요청한 파괴적 작업의 사람 확인.
    #[command(subcommand)]
    Confirm(ConfirmCmd),
    /// 진단.
    #[command(subcommand)]
    Diag(DiagCmd),
    /// 로컬 API 메서드를 이름으로 직접 부릅니다(고급).
    Call(CallArgs),
}

// ───────────── 공통 인자 ─────────────

/// 대상 공간. 이름 또는 ID.
#[derive(Debug, Clone, Args)]
pub struct CrewArg {
    /// 크루(또는 1대1 스페이스) 이름이나 ID.
    #[arg(long, visible_alias = "space", value_name = "크루")]
    pub crew: String,
}

/// 선택적 대상 공간(없으면 전역 기본값).
#[derive(Debug, Clone, Args)]
pub struct OptCrewArg {
    /// 크루 이름이나 ID. 없으면 전역 기본값.
    #[arg(long, visible_alias = "space", value_name = "크루")]
    pub crew: Option<String>,
}

/// 공간 + 채널.
#[derive(Debug, Clone, Args)]
pub struct ChannelTarget {
    #[command(flatten)]
    pub crew: CrewArg,
    /// 채널 이름(`#` 생략 가능)이나 ID.
    #[arg(long, short = 'c', value_name = "채널")]
    pub channel: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ChannelType {
    Text,
    Voice,
    Forum,
    Category,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HistoryArg {
    /// 새 입장자가 이전 기록을 봄.
    Public,
    /// 입장 이후 기록만.
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum SortArg {
    /// 최근 활동순.
    #[default]
    Activity,
    /// 작성순.
    Created,
}

// ───────────── 데몬·감시 ─────────────

#[derive(Debug, Clone, Args)]
pub struct DaemonArgs {
    /// yangpad에 그대로 넘길 인자(예: `--data-dir /srv/yangpa --key-storage file`).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "인자"
    )]
    pub args: Vec<OsString>,
}

#[derive(Debug, Clone, Args)]
pub struct WatchArgs {
    /// 받을 주제(`message`, `message.new`, `space.updated` …). 비우면 전부.
    #[arg(long, value_delimiter = ',', value_name = "주제")]
    pub topics: Vec<String>,
    /// 이 크루의 이벤트만 출력합니다.
    #[arg(long, visible_alias = "space", value_name = "크루")]
    pub crew: Option<String>,
    /// 휘발성 메시지 내용도 출력합니다. 기본은 내용을 숨깁니다(터미널 기록·파이프로 남지 않게).
    #[arg(long)]
    pub show_volatile: bool,
}

// ───────────── 신원·기기·프로필 ─────────────

#[derive(Debug, Subcommand)]
pub enum IdentityCmd {
    /// 새 신원을 만듭니다.
    Create {
        /// 표시 이름.
        #[arg(long)]
        name: String,
    },
    /// 내 신원·기기 정보.
    #[command(alias = "get")]
    Show,
}

#[derive(Debug, Subcommand)]
pub enum DeviceCmd {
    /// 기기 목록.
    #[command(alias = "ls")]
    List,
    /// 기기 이름을 바꿉니다.
    Rename {
        /// 기기 ID.
        device: String,
        /// 새 이름.
        name: String,
    },
    /// 기기를 폐기합니다(모든 그룹에서 제거).
    Revoke {
        /// 기기 ID.
        device: String,
    },
    /// 새 기기를 들일 링크를 띄웁니다(10분). 앞서 띄운 링크는 끝납니다.
    Link,
    /// 띄운 링크로 들어와 승인을 기다리는 기기와 그 번호.
    LinkPending,
    /// 기다리는 기기를 승인합니다. 그 기기 화면의 번호가 같은지 먼저 보세요.
    LinkApprove,
    /// 기다리는 기기를 거절합니다.
    LinkReject,
    /// 띄운 링크를 끝냅니다.
    LinkCancel,
    /// 신원이 없는 이 기기로 연결을 청합니다. 링크는 표준 입력으로 받고, 내 기기에서 비교할 번호를 출력합니다.
    Join,
    /// 이 기기의 연결 요청을 그만둡니다.
    JoinAbort,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCmd {
    /// 프로필 보기. 사용자를 빼면 내 프로필.
    #[command(alias = "get")]
    Show {
        /// 친구 이름이나 사용자 ID.
        user: Option<String>,
    },
    /// 내 프로필 바꾸기.
    Set(ProfileSetArgs),
    /// 크루별 닉네임. 닉네임을 빼면 지웁니다.
    Nickname {
        #[command(flatten)]
        crew: CrewArg,
        nickname: Option<String>,
    },
}

#[derive(Debug, Clone, Args)]
pub struct ProfileSetArgs {
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub bio: Option<String>,
    /// 아바타 이미지 파일.
    #[arg(long, value_name = "파일")]
    pub avatar: Option<PathBuf>,
    /// 배너 이미지 파일.
    #[arg(long, value_name = "파일")]
    pub banner: Option<PathBuf>,
    /// 테마 색(#RRGGBB).
    #[arg(long, value_parser = parse_color, conflicts_with = "no_color")]
    pub color: Option<u32>,
    /// 테마 색을 지웁니다.
    #[arg(long)]
    pub no_color: bool,
    /// 상태 메시지.
    #[arg(long)]
    pub status: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum NoteCmd {
    /// 메모 보기.
    #[command(alias = "get")]
    Show {
        /// 친구 이름이나 사용자 ID.
        user: String,
    },
    /// 메모 쓰기(빈 문자열이면 지움).
    Set { user: String, text: String },
}

// ───────────── 신뢰 스위치 ─────────────

#[derive(Debug, Subcommand)]
pub enum TrustCmd {
    /// 이 사람의 신뢰 스위치와 지금 연결 방식(IP는 보여 주지 않음).
    Get { user: String },
    /// 사람을 신뢰합니다: 그 사람에게 이 기기의 IP가 보일 수 있습니다.
    User {
        user: String,
        /// 신뢰를 끕니다.
        #[arg(long)]
        off: bool,
    },
    /// 크루를 신뢰합니다: 그 크루 멤버에게 이 기기의 IP가 보일 수 있습니다.
    Crew {
        crew: String,
        /// 신뢰를 끕니다.
        #[arg(long)]
        off: bool,
    },
}

// ───────────── 친구 ─────────────

#[derive(Debug, Subcommand)]
pub enum FriendCmd {
    /// 연락처 링크.
    #[command(subcommand)]
    Link(FriendLinkCmd),
    /// 연락처 링크를 씁니다(상대 수락 없이 임시 연락처로 1대1 대화가 생김).
    Add { url: String },
    /// 임시 연락처에게 친구 추가를 보냅니다(상대도 누르면 친구).
    Propose { user: String },
    /// 받은 친구 요청.
    Requests,
    /// 친구 요청 수락.
    Accept { request_id: String },
    /// 친구 요청 거절.
    Reject { request_id: String },
    /// 친구·임시 연락처·만료된 연락처 목록.
    #[command(alias = "ls")]
    List,
    /// 친구 삭제.
    Remove { user: String },
    /// 안전 번호(직접 만나 비교).
    SafetyNumber { user: String },
    /// 안전 번호 확인 표시.
    Verify {
        user: String,
        /// 확인 표시를 지웁니다.
        #[arg(long)]
        unset: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum FriendLinkCmd {
    /// 친구 링크 만들기.
    Create {
        /// 만료(예: 24h, never). 기본은 데몬 설정.
        #[arg(long, value_parser = parse_expiry, value_name = "기간")]
        expires: Option<ExpiryArg>,
        /// 한 번만 쓸 수 있는 링크.
        #[arg(long)]
        single_use: bool,
    },
    /// 내 친구 링크 목록.
    #[command(alias = "ls")]
    List,
    /// 친구 링크 폐기.
    Revoke { token_id: String },
}

// ───────────── 공간·크루·멤버 ─────────────

#[derive(Debug, Subcommand)]
pub enum SpaceCmd {
    /// 공간 목록.
    #[command(alias = "ls")]
    List,
    /// 공간 상세(채널·역할·멤버 요약).
    #[command(alias = "get")]
    Show {
        /// 공간 이름이나 ID.
        space: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum CrewCmd {
    /// 크루 만들기.
    Create {
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// 크루 목록.
    #[command(alias = "ls")]
    List,
    /// 크루 설정 바꾸기.
    Update {
        #[command(flatten)]
        crew: CrewArg,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// 아이콘 이미지 파일.
        #[arg(long, value_name = "파일")]
        icon: Option<PathBuf>,
    },
    /// 1대1 스페이스를 크루로 전환하자고 요청합니다(상대 동의 필요).
    Convert {
        #[command(flatten)]
        crew: CrewArg,
        /// 새 크루 이름.
        #[arg(long)]
        name: String,
    },
    /// 상대의 크루 전환 요청에 동의합니다.
    AcceptConvert {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 크루에서 나갑니다.
    Leave {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 소유권 이전.
    Transfer {
        #[command(flatten)]
        crew: CrewArg,
        /// 새 소유자(멤버 이름이나 ID).
        #[arg(long)]
        to: String,
    },
    /// 확정 기기(멤버·기기 변경을 키 그룹 갱신으로 반영하는 기기) 후보 보기, `--set`이면 바꾸기(소유자만).
    Committer {
        #[command(flatten)]
        crew: CrewArg,
        /// 새 확정 기기(기기 ID 또는 기기 이름).
        #[arg(long, value_name = "기기")]
        set: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MemberCmd {
    /// 멤버 목록.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 멤버 추방.
    Kick {
        #[command(flatten)]
        crew: CrewArg,
        /// 멤버 이름이나 ID.
        user: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// 멤버 역할 지정(목록 전체를 바꿉니다).
    Roles {
        #[command(flatten)]
        crew: CrewArg,
        user: String,
        /// 역할 이름이나 ID 목록(쉼표). 빈 문자열(`--roles ""`)이면 역할을 모두 뺍니다.
        /// 필수인 이유: 빼먹었을 때 역할이 모두 지워지면 안 되므로.
        #[arg(long, value_delimiter = ',', required = true, num_args = 1)]
        roles: Vec<String>,
    },
    /// 밴: 내보내고 다시 들어오지 못하게 합니다(옛 초대·새 초대 모두).
    Ban {
        #[command(flatten)]
        crew: CrewArg,
        /// 멤버 이름이나 사용자 ID(멤버가 아니어도 ID로 미리 밴할 수 있음).
        user: String,
        #[arg(long)]
        reason: Option<String>,
        /// 그 사용자가 최근 24시간 동안 쓴 메시지도 지웁니다.
        #[arg(long)]
        delete_recent: bool,
    },
    /// 밴 해제. 다시 들어오려면 새 초대가 필요합니다.
    Unban {
        #[command(flatten)]
        crew: CrewArg,
        /// 밴 목록의 이름이나 사용자 ID.
        user: String,
    },
    /// 밴 목록.
    Bans {
        #[command(flatten)]
        crew: CrewArg,
    },
}

// ───────────── 차단·신고 ─────────────

#[derive(Debug, Subcommand)]
pub enum UserCmd {
    /// 차단: 그 사람 메시지를 접어 보이고, 알림·친구 요청·첨부 자동 받기·시딩을 끊습니다.
    /// 상대에게 알리지 않고 함께 있는 크루에서 나가게 하지도 않습니다.
    Block {
        /// 친구 이름이나 사용자 ID. `--crew`를 주면 그 크루 멤버 이름으로 찾습니다.
        user: String,
        #[arg(long, visible_alias = "space", value_name = "크루")]
        crew: Option<String>,
    },
    /// 차단 해제.
    Unblock {
        /// 차단 목록의 이름이나 사용자 ID.
        user: String,
    },
    /// 차단 목록.
    Blocked,
    /// 그 사람의 기기 목록(이 기기가 아는 것, 폐기한 것 포함)과 기기 차단 여부.
    Devices {
        /// 친구 이름이나 사용자 ID. `--crew`를 주면 그 크루 멤버 이름으로 찾습니다.
        user: String,
        #[arg(long, visible_alias = "space", value_name = "크루")]
        crew: Option<String>,
    },
    /// 그 사람의 계정에서 기기 하나만 차단합니다(인증 거절, 그 기기의 글 접기). 믿지 못할 기기를 끊을 때.
    BlockDevice {
        /// 친구 이름이나 사용자 ID. `--crew`를 주면 그 크루 멤버 이름으로 찾습니다.
        user: String,
        /// 기기 ID(`user devices`로 확인).
        device: String,
        #[arg(long, visible_alias = "space", value_name = "크루")]
        crew: Option<String>,
    },
    /// 기기 차단 해제.
    UnblockDevice {
        /// 기기 ID(`user blocked-devices`로 확인).
        device: String,
    },
    /// 기기 차단 목록.
    BlockedDevices,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReportStatusArg {
    Open,
    Resolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReportActionArg {
    /// 아무것도 하지 않고 닫기.
    Dismiss,
    /// 신고된 메시지 삭제.
    #[value(alias = "delete_message")]
    DeleteMessage,
    /// 작성자 추방.
    Kick,
    /// 작성자 밴.
    Ban,
}

#[derive(Debug, Subcommand)]
pub enum ReportCmd {
    /// 신고 목록(운영진만).
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        crew: CrewArg,
        /// 상태로 거르기.
        #[arg(long, value_enum)]
        status: Option<ReportStatusArg>,
    },
    /// 신고 처리. `dismiss`가 아니면 자동화 토큰은 사람 확인을 기다립니다.
    Resolve {
        #[command(flatten)]
        crew: CrewArg,
        report_id: String,
        #[arg(long, value_enum)]
        action: ReportActionArg,
        /// 운영진끼리만 보이는 처리 메모.
        #[arg(long)]
        note: Option<String>,
    },
}

// ───────────── 채널 ─────────────

#[derive(Debug, Subcommand)]
pub enum ChannelCmd {
    /// 채널 목록.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 채널 만들기.
    Create(ChannelCreateArgs),
    /// 채널 설정 바꾸기.
    Update(ChannelUpdateArgs),
    /// 채널 삭제.
    Delete {
        #[command(flatten)]
        crew: CrewArg,
        channel: String,
    },
    /// 입장·퇴장 방식 채널에 들어갑니다.
    Join {
        #[command(flatten)]
        crew: CrewArg,
        channel: String,
    },
    /// 입장·퇴장 방식 채널에서 나갑니다(휘발성 내용은 즉시 지워짐).
    Leave {
        #[command(flatten)]
        crew: CrewArg,
        channel: String,
    },
    /// 휘발성 채널의 API(AI) 읽기 허용·차단(사람만, 소유자 토큰).
    AllowApiRead {
        #[command(flatten)]
        crew: CrewArg,
        channel: String,
        /// on 또는 off.
        #[arg(value_parser = parse_switch, action = ArgAction::Set)]
        allowed: bool,
    },
    /// 여기까지 읽음으로 표시.
    MarkRead {
        #[command(flatten)]
        crew: CrewArg,
        channel: String,
        /// 마지막으로 읽은 메시지 ID.
        event: String,
    },
}

#[derive(Debug, Clone, Args)]
pub struct ChannelCreateArgs {
    #[command(flatten)]
    pub crew: CrewArg,
    /// 채널 종류.
    #[arg(long = "type", value_enum, default_value = "text")]
    pub kind: ChannelType,
    #[arg(long)]
    pub name: String,
    /// 상위 카테고리(이름이나 ID).
    #[arg(long)]
    pub parent: Option<String>,
    #[arg(long)]
    pub topic: Option<String>,
    /// 휘발성 채널(RAM에만 있고 디스크에 남지 않음).
    #[arg(long)]
    pub volatile: bool,
    /// 입장·퇴장 방식(들어가 있는 동안만 대화 참여).
    #[arg(long)]
    pub join_leave: bool,
    /// 채팅 수명(예: 30s, 5m, on-leave).
    #[arg(long, value_parser = parse_ttl, value_name = "수명")]
    pub ttl: Option<TtlArg>,
    /// 과거 기록 공개.
    #[arg(long, value_enum)]
    pub history: Option<HistoryArg>,
    /// 링크 미리보기(on/off). 휘발성 채널 기본값은 off.
    #[arg(long, value_parser = parse_switch, value_name = "on|off")]
    pub link_previews: Option<bool>,
    /// 이 역할들만 볼 수 있는 비공개 채널(쉼표 목록).
    #[arg(long, value_delimiter = ',', value_name = "역할")]
    pub private_roles: Option<Vec<String>>,
    /// 정렬 위치.
    #[arg(long, allow_negative_numbers = true)]
    pub position: Option<i32>,
}

#[derive(Debug, Clone, Args)]
pub struct ChannelUpdateArgs {
    #[command(flatten)]
    pub crew: CrewArg,
    /// 바꿀 채널(이름이나 ID).
    pub channel: String,
    /// 새 이름.
    #[arg(long)]
    pub name: Option<String>,
    /// 상위 카테고리.
    #[arg(long, conflicts_with = "no_parent")]
    pub parent: Option<String>,
    /// 최상위로 옮깁니다.
    #[arg(long)]
    pub no_parent: bool,
    #[arg(long)]
    pub topic: Option<String>,
    /// 휘발성으로.
    #[arg(long, conflicts_with = "normal")]
    pub volatile: bool,
    /// 일반 등급으로.
    #[arg(long)]
    pub normal: bool,
    /// 입장·퇴장 방식으로.
    #[arg(long, conflicts_with = "always")]
    pub join_leave: bool,
    /// 상시 참여로.
    #[arg(long)]
    pub always: bool,
    /// 채팅 수명(예: 30s, on-leave, none).
    #[arg(long, value_parser = parse_ttl, value_name = "수명")]
    pub ttl: Option<TtlArg>,
    #[arg(long, value_enum)]
    pub history: Option<HistoryArg>,
    #[arg(long, value_parser = parse_switch, value_name = "on|off")]
    pub link_previews: Option<bool>,
    /// 이 역할들만 보게 합니다(쉼표 목록).
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "역할",
        conflicts_with = "public"
    )]
    pub private_roles: Option<Vec<String>>,
    /// 모두에게 공개.
    #[arg(long)]
    pub public: bool,
    #[arg(long, allow_negative_numbers = true)]
    pub position: Option<i32>,
}

// ───────────── 메시지·포럼 ─────────────

#[derive(Debug, Subcommand)]
pub enum MessageCmd {
    /// 최근 메시지(오래된 것부터 출력).
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        target: ChannelTarget,
        /// 이 메시지보다 이전.
        #[arg(long)]
        before: Option<String>,
        /// 개수(최대 200).
        #[arg(long, short = 'n', value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: Option<u32>,
        /// 스레드(시작 메시지 ID).
        #[arg(long)]
        thread: Option<String>,
    },
    /// 메시지 보내기. 내용이 `-`이면 표준 입력에서 읽습니다.
    Send {
        #[command(flatten)]
        target: ChannelTarget,
        text: String,
        /// 첨부 파일(여러 번 지정 가능).
        #[arg(long = "attach", short = 'a', value_name = "파일")]
        attachments: Vec<PathBuf>,
        /// 답장할 메시지 ID.
        #[arg(long)]
        reply_to: Option<String>,
        /// 스레드(시작 메시지 ID).
        #[arg(long)]
        thread: Option<String>,
    },
    /// 내 메시지 수정.
    Edit {
        #[command(flatten)]
        target: ChannelTarget,
        id: String,
        text: String,
    },
    /// 메시지 삭제(남의 메시지는 메시지 관리 권한).
    Delete {
        #[command(flatten)]
        target: ChannelTarget,
        id: String,
    },
    /// 반응 달기(유니코드 이모지 또는 `<:이름:ID>`).
    React {
        #[command(flatten)]
        target: ChannelTarget,
        id: String,
        emoji: String,
    },
    /// 반응 빼기.
    Unreact {
        #[command(flatten)]
        target: ChannelTarget,
        id: String,
        emoji: String,
    },
    /// 신고: 운영진 기기에만 봉인해 보냅니다. `--hide`면 이 기기에서 숨기고 시딩에서도 뺍니다.
    Report {
        #[command(flatten)]
        target: ChannelTarget,
        id: String,
        /// 신고 이유(필수).
        #[arg(long)]
        reason: String,
        #[arg(long)]
        hide: bool,
    },
    /// 검색(일반 등급만, 이 기기의 DB). 낱말은 모두 들어 있어야 하고, 큰따옴표로 묶으면 구절 그대로.
    Search {
        query: String,
        #[arg(long, visible_alias = "space")]
        crew: Option<String>,
        /// 채널(크루와 함께 지정).
        #[arg(long, short = 'c', requires = "crew")]
        channel: Option<String>,
        /// 쪽 크기(기본 50, 최대 500).
        #[arg(long, short = 'n')]
        limit: Option<u32>,
        /// 몇째 쪽(1부터, 최신순).
        #[arg(long, short = 'p', default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        page: u32,
    },
}

#[derive(Debug, Subcommand)]
pub enum ForumCmd {
    /// 글 쓰기.
    Post {
        #[command(flatten)]
        target: ChannelTarget,
        #[arg(long)]
        title: String,
        /// 본문. `-`이면 표준 입력.
        text: String,
        /// 태그 ID 목록.
        #[arg(long, value_delimiter = ',')]
        tags: Vec<u32>,
    },
    /// 글 목록.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        target: ChannelTarget,
        #[arg(long, value_enum, default_value = "activity")]
        sort: SortArg,
        #[arg(long)]
        tag: Option<u32>,
    },
    /// 글 설정 바꾸기.
    Update {
        #[command(flatten)]
        target: ChannelTarget,
        /// 글(시작 메시지) ID.
        post: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<u32>>,
        #[arg(long, value_parser = parse_switch, value_name = "on|off")]
        pin: Option<bool>,
        #[arg(long, value_parser = parse_switch, value_name = "on|off")]
        lock: Option<bool>,
    },
}

// ───────────── 블롭·이모지 ─────────────

#[derive(Debug, Subcommand)]
pub enum BlobCmd {
    /// 받은 조각 수·온라인 시더 수.
    Status {
        #[command(flatten)]
        crew: CrewArg,
        blob: String,
    },
    /// 수동 다운로드 시작.
    Fetch {
        #[command(flatten)]
        crew: CrewArg,
        blob: String,
        #[arg(long)]
        priority: Option<u8>,
    },
    /// 파일로 저장(데몬이 씁니다).
    Save {
        #[command(flatten)]
        crew: CrewArg,
        blob: String,
        /// 첨부가 실린 메시지 ID(복호화 키 출처).
        #[arg(long)]
        key_from: String,
        /// 저장 경로.
        path: PathBuf,
        /// 휘발성 채널 첨부도 저장합니다. 저장한 파일은 메시지가 사라져도 남습니다.
        #[arg(long)]
        allow_volatile: bool,
    },
    /// 내용을 읽어 파일이나 표준 출력(`-`)으로 씁니다.
    Read {
        #[command(flatten)]
        crew: CrewArg,
        blob: String,
        #[arg(long)]
        key_from: String,
        #[arg(long)]
        index: Option<u32>,
        #[arg(long)]
        offset: Option<u64>,
        /// 읽을 크기(예: 1MB, 최대 8MiB).
        #[arg(long, value_parser = parse_size)]
        len: Option<u64>,
        /// 출력 파일. `-`이면 표준 출력.
        #[arg(long, short = 'o', default_value = "-")]
        out: PathBuf,
        /// 휘발성 채널 첨부도 파일로 씁니다. 저장한 파일은 메시지가 사라져도 남습니다.
        #[arg(long)]
        allow_volatile: bool,
    },
    /// 고정: 저장 공간 정리에서 빼고, 아직 받지 않았으면 받기 시작.
    Pin {
        #[command(flatten)]
        crew: CrewArg,
        blob: String,
    },
    /// 고정 풀기.
    Unpin {
        #[command(flatten)]
        crew: CrewArg,
        blob: String,
    },
    /// 고정한 블롭 목록.
    Pins {
        #[command(flatten)]
        crew: OptCrewArg,
    },
    /// 받은 첨부가 쓰는 저장 공간(보기·한도·비우기).
    #[command(subcommand)]
    Cache(CacheCmd),
}

#[derive(Debug, Subcommand)]
pub enum CacheCmd {
    /// 사용량과 한도.
    #[command(alias = "show")]
    Status,
    /// 한도 바꾸기(예: `limit 10GB --min-free 2GB`, 기본값으로는 `--reset`).
    Limit {
        /// 받은 첨부에 쓸 디스크 한도.
        #[arg(value_parser = parse_size, value_name = "크기")]
        limit: Option<u64>,
        /// 디스크에 남겨 둘 최소 여유 공간.
        #[arg(long, value_parser = parse_size, value_name = "크기")]
        min_free: Option<u64>,
        /// 한도와 여유 공간을 기본값으로.
        #[arg(long, conflicts_with_all = ["limit", "min_free"])]
        reset: bool,
    },
    /// 받은 첨부(고정·내 첨부·이모지·프로필 이미지 빼고)의 데이터를 모두 비웁니다. 다시 받을 수 있습니다.
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum EmojiCmd {
    /// 이모지 목록.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 이모지 추가.
    Create {
        #[command(flatten)]
        crew: CrewArg,
        #[arg(long)]
        name: String,
        /// 이미지 파일(png·gif·webp, 크루가 정한 크기 한도가 있으면 그 이하).
        path: PathBuf,
    },
    /// 이모지 삭제(이름이나 ID).
    Delete {
        #[command(flatten)]
        crew: CrewArg,
        emoji: String,
    },
}

// ───────────── 역할·초대 ─────────────

#[derive(Debug, Subcommand)]
pub enum RoleCmd {
    /// 역할 목록.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 역할 만들기.
    Create {
        #[command(flatten)]
        crew: CrewArg,
        #[arg(long)]
        name: String,
        /// 권한 이름 목록(예: manage_channels,kick).
        #[arg(long, visible_alias = "permissions", value_parser = parse_permissions, default_value = "")]
        perms: NameList,
        /// 색(#RRGGBB).
        #[arg(long, value_parser = parse_color)]
        color: Option<u32>,
        /// 순위(클수록 높음).
        #[arg(long)]
        rank: Option<u32>,
    },
    /// 역할 바꾸기.
    Update {
        #[command(flatten)]
        crew: CrewArg,
        /// 역할 이름이나 ID.
        role: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, visible_alias = "permissions", value_parser = parse_permissions)]
        perms: Option<NameList>,
        #[arg(long, value_parser = parse_color)]
        color: Option<u32>,
        #[arg(long)]
        rank: Option<u32>,
    },
    /// 역할 삭제.
    Delete {
        #[command(flatten)]
        crew: CrewArg,
        role: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum InviteCmd {
    /// 초대 링크 만들기.
    Create {
        #[command(flatten)]
        crew: CrewArg,
        /// 만료(예: 24h, never).
        #[arg(long, value_parser = parse_expiry, value_name = "기간")]
        expires: Option<ExpiryArg>,
        /// 최대 사용 횟수.
        #[arg(long, visible_alias = "max-uses")]
        uses: Option<u32>,
        /// 가입 시 줄 역할(쉼표 목록).
        #[arg(long, value_delimiter = ',')]
        roles: Vec<String>,
    },
    /// 초대 목록.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        crew: CrewArg,
    },
    /// 초대 폐기(목록의 토큰 해시).
    Revoke {
        #[command(flatten)]
        crew: CrewArg,
        token_hash: String,
    },
    /// 초대 링크로 가입.
    Join { url: String },
}

// ───────────── 동기화 설정 ─────────────

#[derive(Debug, Subcommand)]
pub enum SeedCmd {
    /// 시더 설정 보기.
    #[command(alias = "get")]
    Show {
        #[command(flatten)]
        crew: OptCrewArg,
    },
    /// 시더 설정 바꾸기.
    Set(SeedSetArgs),
    /// 특정 메시지를 시딩에서 빼거나 되돌립니다.
    Exclude {
        #[command(flatten)]
        target: ChannelTarget,
        /// 메시지 ID.
        id: String,
        /// 제외를 취소합니다.
        #[arg(long)]
        undo: bool,
    },
    /// 채널별 온라인 시더 수(0이면 경고 대상).
    Coverage {
        #[command(flatten)]
        crew: CrewArg,
    },
}

#[derive(Debug, Clone, Args)]
pub struct SeedSetArgs {
    #[command(flatten)]
    pub crew: OptCrewArg,
    /// 시딩 켜기.
    #[arg(long, conflicts_with = "disable")]
    pub enable: bool,
    /// 시딩 끄기.
    #[arg(long)]
    pub disable: bool,
    /// 제공할 데이터 종류(text,image,video,file,emoji,history_bundle).
    #[arg(long, value_parser = parse_types)]
    pub types: Option<TypeList>,
    /// 이 채널들만 제공(쉼표 목록).
    #[arg(long, value_delimiter = ',', conflicts_with_all = ["all_channels", "opened_channels"])]
    pub channels: Option<Vec<String>>,
    /// 볼 수 있는 모든 채널 제공.
    #[arg(long, conflicts_with = "opened_channels")]
    pub all_channels: bool,
    /// 내가 연 채널만 제공(기본).
    #[arg(long)]
    pub opened_channels: bool,
    /// 제외할 채널(쉼표 목록, 빈 문자열이면 제외 없음).
    #[arg(long, value_delimiter = ',')]
    pub exclude_channels: Option<Vec<String>>,
    /// 업로드 상한(예: 2MB/s, unlimited).
    #[arg(long, value_parser = rate_arg, value_name = "속도")]
    pub upload_limit: Option<RateArg>,
    /// 동시에 보내 줄 기기 수(한 기기가 겹쳐 보낸 요청은 하나로 셈).
    #[arg(long)]
    pub max_concurrent: Option<u32>,
    /// 제공 범위 채널의 첨부를 열지 않아도 미리 받아 둠(헤드리스 시더).
    #[arg(long, value_parser = parse_switch, value_name = "on|off")]
    pub prefetch: Option<bool>,
}

// ───────────── 보관 전용 노드 ─────────────

#[derive(Debug, Subcommand)]
pub enum ArchiveCmd {
    /// (보관 노드에서) 크루 관리자에게 따로 전할 1회용 지정 링크를 만듭니다.
    Offer {
        /// 링크 유효 시간(예: 24h). 기본 24시간, 최대 30일.
        #[arg(long, value_parser = parse_duration, value_name = "기간")]
        expires: Option<Duration>,
        /// 크루에 보일 이 보관 노드의 이름.
        #[arg(long)]
        name: Option<String>,
    },
    /// (보관 노드에서) 받아 둔 크루 목록. 내용은 읽을 수 없어 수치만 보입니다.
    Hosted,
    /// (보관 노드에서) 받아 둔 크루의 데이터를 모두 지웁니다.
    Drop {
        /// 받아 둔 크루의 공간 ID(`archive hosted`에 보임).
        space: String,
    },
    /// 보관 노드를 이 크루에 지정합니다(크루 관리 권한).
    Designate {
        #[command(flatten)]
        crew: CrewArg,
        /// 보관 노드가 `archive offer`로 만든 링크.
        url: String,
        /// 함께 보관할 비공개 채널(쉼표 목록). 공개 채널은 늘 보관합니다.
        #[arg(long, value_delimiter = ',', value_name = "채널")]
        private_channels: Vec<String>,
        /// 크루에 보일 이름. 없으면 링크에 적힌 이름.
        #[arg(long)]
        name: Option<String>,
    },
    /// 보관 노드 지정을 해제합니다(크루 관리 권한). 보관 노드는 이 크루 데이터를 지웁니다.
    Revoke {
        #[command(flatten)]
        crew: CrewArg,
        /// 보관 노드 이름이나 기기 ID(`archive list`에 보임).
        archive: String,
    },
    /// 이 크루에 지정된 보관 노드와 연결 상태.
    List {
        #[command(flatten)]
        crew: CrewArg,
    },
}

#[derive(Debug, Subcommand)]
pub enum PeerCmd {
    /// 피어 설정 보기.
    #[command(alias = "get")]
    Show {
        #[command(flatten)]
        crew: OptCrewArg,
    },
    /// 피어 설정 바꾸기.
    Set {
        #[command(flatten)]
        crew: OptCrewArg,
        /// 받아 올 데이터 종류.
        #[arg(long, value_parser = parse_types)]
        types: Option<TypeList>,
        /// 종류별 우선순위(예: text=250,image=100).
        #[arg(long, value_parser = parse_priorities)]
        priorities: Option<BTreeMap<DataType, u8>>,
        /// 다운로드 상한(예: 5MB/s, unlimited).
        #[arg(long, value_parser = rate_arg, value_name = "속도")]
        download_limit: Option<RateArg>,
        /// 이 크기 이하 첨부는 자동으로 받습니다(예: 20MB).
        #[arg(long, value_parser = parse_size, value_name = "크기")]
        auto_download_max: Option<u64>,
    },
}

// ───────────── 네트워크 ─────────────

#[derive(Debug, Subcommand)]
pub enum NetCmd {
    /// 네트워크 상태.
    Status,
    /// STUN 서버 목록. P2P 연결(신뢰한 상대·P2P 전용 채널)이 공유기 밖 주소를 알아낼 때만 쓰고 Tor 연결에는 쓰지
    /// 않습니다. 하위 명령이 없으면 지금 목록을 보여 줍니다.
    Stun {
        #[command(subcommand)]
        action: Option<StunCmd>,
    },
    /// 연결된 피어.
    Peers,
}

#[derive(Debug, Subcommand)]
pub enum StunCmd {
    /// 목록을 바꿉니다(`host:port`, 포트를 빼면 3478, 8개까지). STUN 서버는 이 기기의 공인 IP를 봅니다.
    Set {
        #[arg(required = true, value_name = "HOST:PORT")]
        servers: Vec<String>,
    },
    /// 기본 목록으로 되돌립니다.
    Reset,
    /// STUN을 쓰지 않습니다(공유기 밖 주소를 알아내지 못해 P2P 연결이 덜 될 수 있음).
    Off,
}

// ───────────── 토큰·감사·확인·진단 ─────────────

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// 토큰 발급(소유자). 토큰은 이때 한 번만 출력됩니다.
    Create {
        #[arg(long)]
        name: String,
        /// 범위(read,write,admin,settings,owner).
        #[arg(long, value_parser = parse_scopes)]
        scopes: ScopeList,
        /// 허용 크루(쉼표 목록, 비우면 전체).
        #[arg(long, visible_alias = "spaces", value_delimiter = ',')]
        crews: Vec<String>,
        /// AI(자동화)용 토큰: 파괴적 작업은 사람 확인을 거칩니다.
        #[arg(long)]
        automation: bool,
    },
    /// 토큰 목록.
    #[command(alias = "ls")]
    List,
    /// 토큰 폐기.
    Revoke { id: String },
}

#[derive(Debug, Subcommand)]
pub enum AuditCmd {
    /// 감사 로그(최근 것부터).
    #[command(alias = "ls")]
    List {
        #[arg(long, short = 'n')]
        limit: Option<u32>,
        /// 이 항목 ID보다 이전.
        #[arg(long)]
        before: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfirmCmd {
    /// 확인 대기 중인 작업.
    #[command(alias = "ls")]
    List,
    /// 승인하고 실행합니다. 무엇을 승인하는지 먼저 보여 주고 묻습니다.
    Approve {
        id: String,
        /// 묻지 않고 승인합니다(표준 입력이 터미널이 아니면 필수).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// 거절합니다.
    Deny { id: String },
}

#[derive(Debug, Subcommand)]
pub enum DiagCmd {
    /// 성능 지표.
    Metrics,
    /// 휘발성 표식 문자열이 디스크·로그에 남았는지 검사합니다(소유자).
    Leakcheck { marker: String },
}

#[derive(Debug, Clone, Args)]
pub struct CallArgs {
    /// 메서드 이름(예: space.list).
    pub method: String,
    /// JSON 인자. 빼면 `{}`. `-`이면 표준 입력.
    pub params: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use yangpa_rpc::api::Scope;

    fn parse(args: &[&str]) -> Cli {
        let mut v = vec!["yangpa"];
        v.extend_from_slice(args);
        match Cli::try_parse_from(v) {
            Ok(c) => c,
            Err(e) => panic!("{args:?} 해석 실패: {e}"),
        }
    }

    #[test]
    fn command_tree_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn plan_examples_parse() {
        let c = parse(&["crew", "create", "게임 모임"]);
        assert!(
            matches!(c.command, Command::Crew(CrewCmd::Create { ref name, .. }) if name == "게임 모임")
        );

        let c = parse(&[
            "channel",
            "create",
            "--crew",
            "게임 모임",
            "--type",
            "text",
            "--name",
            "공지",
        ]);
        let Command::Channel(ChannelCmd::Create(a)) = c.command else {
            panic!("채널 생성이 아님")
        };
        assert_eq!(a.crew.crew, "게임 모임");
        assert_eq!(a.kind, ChannelType::Text);
        assert!(!a.volatile && !a.join_leave && a.ttl.is_none());

        let c = parse(&[
            "channel",
            "create",
            "--crew",
            "게임 모임",
            "--type",
            "text",
            "--name",
            "비밀",
            "--volatile",
            "--join-leave",
            "--ttl",
            "30s",
        ]);
        let Command::Channel(ChannelCmd::Create(a)) = c.command else {
            panic!("채널 생성이 아님")
        };
        assert!(a.volatile && a.join_leave);
        assert_eq!(a.ttl, Some(TtlArg::Seconds(30)));
        assert_eq!(a.name, "비밀");

        let c = parse(&[
            "role",
            "create",
            "--crew",
            "게임 모임",
            "--name",
            "운영진",
            "--perms",
            "manage_channels,kick",
        ]);
        let Command::Role(RoleCmd::Create { perms, name, .. }) = c.command else {
            panic!("역할 생성이 아님")
        };
        assert_eq!(name, "운영진");
        assert_eq!(perms, vec!["manage_channels", "kick"]);

        let c = parse(&[
            "invite",
            "create",
            "--crew",
            "게임 모임",
            "--expires",
            "24h",
            "--uses",
            "10",
        ]);
        let Command::Invite(InviteCmd::Create { expires, uses, .. }) = c.command else {
            panic!("초대 생성이 아님")
        };
        assert_eq!(expires, Some(ExpiryArg(Some(86_400))));
        assert_eq!(uses, Some(10));

        let c = parse(&[
            "seed",
            "set",
            "--crew",
            "게임 모임",
            "--types",
            "text,emoji",
            "--upload-limit",
            "2MB/s",
        ]);
        let Command::Seed(SeedCmd::Set(a)) = c.command else {
            panic!("시더 설정이 아님")
        };
        assert_eq!(a.crew.crew.as_deref(), Some("게임 모임"));
        assert_eq!(a.types, Some(vec![DataType::Text, DataType::Emoji]));
        assert_eq!(a.upload_limit, Some(RateArg(Some(2 * 1024 * 1024))));

        let c = parse(&[
            "archive",
            "designate",
            "--crew",
            "게임 모임",
            "yangpa://archive/abc",
            "--private-channels",
            "운영,#기록",
        ]);
        let Command::Archive(ArchiveCmd::Designate {
            crew,
            url,
            private_channels,
            name,
        }) = c.command
        else {
            panic!("보관 노드 지정이 아님")
        };
        assert_eq!(crew.crew, "게임 모임");
        assert_eq!(url, "yangpa://archive/abc");
        assert_eq!(private_channels, vec!["운영", "#기록"]);
        assert_eq!(name, None);

        let c = parse(&["archive", "offer", "--expires", "2h", "--name", "보관함"]);
        let Command::Archive(ArchiveCmd::Offer { expires, name }) = c.command else {
            panic!("보관 링크 만들기가 아님")
        };
        assert_eq!(expires, Some(Duration::from_secs(7200)));
        assert_eq!(name.as_deref(), Some("보관함"));
        assert_eq!(a.prefetch, None);
    }

    #[test]
    fn storage_and_committer_commands() {
        let c = parse(&["seed", "set", "--all-channels", "--prefetch", "on"]);
        let Command::Seed(SeedCmd::Set(a)) = c.command else {
            panic!("시더 설정이 아님")
        };
        assert!(a.all_channels);
        assert_eq!(a.prefetch, Some(true));

        let c = parse(&["blob", "cache", "limit", "10GB", "--min-free", "512MB"]);
        let Command::Blob(BlobCmd::Cache(CacheCmd::Limit {
            limit,
            min_free,
            reset,
        })) = c.command
        else {
            panic!("저장 공간 한도가 아님")
        };
        assert_eq!(limit, Some(10 * 1024 * 1024 * 1024));
        assert_eq!(min_free, Some(512 * 1024 * 1024));
        assert!(!reset);
        assert!(
            Cli::try_parse_from(["yangpa", "blob", "cache", "limit", "1GB", "--reset"]).is_err()
        );

        let c = parse(&["blob", "pin", "--crew", "게임 모임", "abcd"]);
        assert!(
            matches!(c.command, Command::Blob(BlobCmd::Pin { ref blob, .. }) if blob == "abcd")
        );
        let c = parse(&["blob", "cache", "status"]);
        assert!(matches!(
            c.command,
            Command::Blob(BlobCmd::Cache(CacheCmd::Status))
        ));

        let c = parse(&[
            "crew",
            "committer",
            "--crew",
            "게임 모임",
            "--set",
            "노트북",
        ]);
        let Command::Crew(CrewCmd::Committer { set, .. }) = c.command else {
            panic!("확정 기기가 아님")
        };
        assert_eq!(set.as_deref(), Some("노트북"));
    }

    #[test]
    fn invalid_values_are_rejected() {
        let bad: &[&[&str]] = &[
            &[
                "role", "create", "--crew", "x", "--name", "a", "--perms", "fly",
            ],
            &["seed", "set", "--types", "sound"],
            &["seed", "set", "--upload-limit", "fast"],
            &["seed", "set", "--enable", "--disable"],
            &["seed", "set", "--channels", "a", "--all-channels"],
            &["archive", "designate", "--crew", "x"],
            &["archive", "revoke", "--crew", "x"],
            &["archive", "offer", "--expires", "soon"],
            &["invite", "create", "--crew", "x", "--expires", "0s"],
            &[
                "channel", "create", "--crew", "x", "--name", "a", "--type", "radio",
            ],
            &[
                "channel", "create", "--crew", "x", "--name", "a", "--ttl", "soon",
            ],
            &[
                "channel",
                "update",
                "--crew",
                "x",
                "a",
                "--volatile",
                "--normal",
            ],
            &[
                "message", "list", "--crew", "x", "-c", "y", "--limit", "500",
            ],
            &["token", "create", "--name", "ai", "--scopes", "god"],
            // Tor 모드 스위치는 없앴습니다(언제나 Tor).
            &["net", "tor", "on"],
            &["net", "hide-ip", "on"],
            &["net", "low-power", "on"],
            &["net", "bootstrap", "/ip4/1.2.3.4/tcp/1"],
            &["net", "stun", "set"],
            &["message", "search", "q", "--channel", "a"],
            &["channel", "create", "--name", "a"],
            // --roles를 빼먹으면 역할이 모두 지워지므로 필수입니다.
            &["member", "roles", "--crew", "x", "철수"],
        ];
        for args in bad {
            let mut v = vec!["yangpa"];
            v.extend_from_slice(args);
            assert!(Cli::try_parse_from(v).is_err(), "{args:?}는 거부돼야 함");
        }
    }

    #[test]
    fn global_flags_anywhere() {
        let c = parse(&["space", "list", "--json", "--endpoint", "/tmp/x.sock"]);
        assert!(c.json);
        assert_eq!(c.endpoint.as_deref(), Some("/tmp/x.sock"));
        assert_eq!(c.timeout, Duration::from_secs(30));
        let c = parse(&["--timeout", "5s", "status"]);
        assert_eq!(c.timeout, Duration::from_secs(5));
        let c = parse(&["member", "ls", "--space", "모임"]);
        assert!(
            matches!(c.command, Command::Member(MemberCmd::List { ref crew }) if crew.crew == "모임")
        );
    }

    #[test]
    fn daemon_args_pass_through() {
        let c = parse(&[
            "daemon",
            "--headless",
            "--data-dir",
            "/srv/yangpa",
            "--help",
        ]);
        let Command::Daemon(d) = c.command else {
            panic!("daemon이 아님")
        };
        let args: Vec<_> = d
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--headless", "--data-dir", "/srv/yangpa", "--help"]);
        let c = parse(&["daemon"]);
        assert!(matches!(c.command, Command::Daemon(DaemonArgs { ref args }) if args.is_empty()));
    }

    #[test]
    fn misc_commands_parse() {
        let c = parse(&[
            "message", "send", "--crew", "모임", "-c", "#공지", "안녕", "-a", "a.png", "-a",
            "b.txt",
        ]);
        let Command::Message(MessageCmd::Send {
            target,
            text,
            attachments,
            ..
        }) = c.command
        else {
            panic!("메시지 전송이 아님")
        };
        assert_eq!(target.channel, "#공지");
        assert_eq!(text, "안녕");
        assert_eq!(attachments.len(), 2);

        let c = parse(&[
            "peer",
            "set",
            "--priorities",
            "text=250,image=100",
            "--download-limit",
            "unlimited",
        ]);
        let Command::Peer(PeerCmd::Set {
            priorities,
            download_limit,
            ..
        }) = c.command
        else {
            panic!("피어 설정이 아님")
        };
        assert_eq!(priorities.map(|p| p.len()), Some(2));
        assert_eq!(download_limit, Some(RateArg(None)));

        let c = parse(&[
            "token",
            "create",
            "--name",
            "ai",
            "--scopes",
            "read,write",
            "--automation",
            "--crews",
            "모임,다른",
        ]);
        let Command::Token(TokenCmd::Create {
            scopes,
            crews,
            automation,
            ..
        }) = c.command
        else {
            panic!("토큰 발급이 아님")
        };
        assert_eq!(scopes, vec![Scope::Read, Scope::Write]);
        assert_eq!(crews, vec!["모임", "다른"]);
        assert!(automation);

        let c = parse(&[
            "channel",
            "update",
            "--crew",
            "모임",
            "비밀",
            "--ttl",
            "none",
            "--no-parent",
            "--public",
        ]);
        let Command::Channel(ChannelCmd::Update(a)) = c.command else {
            panic!("채널 변경이 아님")
        };
        assert_eq!(a.ttl, Some(TtlArg::None));
        assert!(a.no_parent && a.public);

        let c = parse(&[
            "channel",
            "create",
            "--crew",
            "모임",
            "--name",
            "공지",
            "--position",
            "-1",
        ]);
        let Command::Channel(ChannelCmd::Create(a)) = c.command else {
            panic!("채널 생성이 아님")
        };
        assert_eq!(a.position, Some(-1));

        parse(&[
            "watch",
            "--topics",
            "message,space.updated",
            "--crew",
            "모임",
        ]);
        parse(&["call", "space.list"]);
        parse(&["call", "space.get", "{\"space\":\"abc\"}"]);
        let c = parse(&["confirm", "approve", "c-1", "--yes"]);
        assert!(matches!(
            c.command,
            Command::Confirm(ConfirmCmd::Approve { ref id, yes: true }) if id == "c-1"
        ));
        let c = parse(&["confirm", "approve", "c-1"]);
        assert!(matches!(
            c.command,
            Command::Confirm(ConfirmCmd::Approve { yes: false, .. })
        ));
        // 역할을 모두 빼려면 빈 문자열을 명시합니다.
        let c = parse(&["member", "roles", "--crew", "x", "철수", "--roles", ""]);
        let Command::Member(MemberCmd::Roles { roles, .. }) = c.command else {
            panic!("역할 지정이 아님")
        };
        assert!(roles.iter().all(|r| r.is_empty()), "{roles:?}");
        let c = parse(&[
            "member",
            "roles",
            "--crew",
            "x",
            "철수",
            "--roles",
            "운영진,손님",
        ]);
        let Command::Member(MemberCmd::Roles { roles, .. }) = c.command else {
            panic!("역할 지정이 아님")
        };
        assert_eq!(roles, ["운영진", "손님"]);
        let c = parse(&[
            "net",
            "stun",
            "set",
            "stun.example.org",
            "198.51.100.7:3479",
        ]);
        let Command::Net(NetCmd::Stun {
            action: Some(StunCmd::Set { servers }),
        }) = c.command
        else {
            panic!("STUN 목록 바꾸기가 아님")
        };
        assert_eq!(servers, ["stun.example.org", "198.51.100.7:3479"]);
        let c = parse(&["net", "stun"]);
        assert!(matches!(
            c.command,
            Command::Net(NetCmd::Stun { action: None })
        ));
        parse(&[
            "blob",
            "read",
            "--crew",
            "모임",
            "B",
            "--key-from",
            "E",
            "--len",
            "1MB",
            "-o",
            "out.bin",
        ]);
        parse(&["role", "create", "--crew", "모임", "--name", "손님"]);
    }
}
