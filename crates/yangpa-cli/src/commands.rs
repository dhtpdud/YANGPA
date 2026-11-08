//! 명령 실행. 데몬에 연결해 인증하고, 이름을 ID로 바꾼 뒤 로컬 API를 부르고, 결과를 출력합니다.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, EmojiId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelKind, HistoryVisibility, JoinMode, Role, SecurityLevel, SpaceKind,
};
use yangpa_rpc::api::serde_util::{SecretString, TokenHash};
use yangpa_rpc::api::*;
use yangpa_rpc::endpoint::{Endpoint, resolve_token};
use yangpa_rpc::{ClientError, RpcClient};
use zeroize::Zeroize;

use crate::args::*;
use crate::output;
use crate::parse::{TtlArg, format_size};
use crate::resolve::{Candidate, resolve, resolve_all, strip_sigil};

/// 표준 입력으로 받는 본문의 상한(메시지·JSON 인자). 데몬도 검사하지만 CLI가 먼저 끊어 메모리를 아낍니다.
const MAX_STDIN: u64 = 4 * 1024 * 1024;

/// 연결된 실행 문맥.
pub struct Ctx {
    client: RpcClient,
    json: bool,
    spaces: Option<Vec<SpaceSummary>>,
}

/// 엔드포인트에 연결하고 인증합니다. 연결 오류는 [`ClientError::Connect`] 그대로 올려 종료 코드를 정하게 합니다.
pub async fn connect(
    endpoint: &Endpoint,
    token: Option<&str>,
    timeout: Duration,
) -> Result<RpcClient> {
    let client = tokio::time::timeout(timeout, RpcClient::connect(endpoint))
        .await
        .map_err(|_| ClientError::Timeout)??
        .with_timeout(timeout);
    let token = resolve_token(token).context("API 토큰을 찾을 수 없습니다")?;
    client.hello(token.expose()).await?;
    Ok(client)
}

/// `yangpa version`: 데몬이 없어도 CLI 버전은 출력합니다.
pub async fn version(endpoint: &Endpoint, json: bool, timeout: Duration) -> Result<()> {
    let cli = env!("CARGO_PKG_VERSION");
    let daemon = match tokio::time::timeout(timeout, RpcClient::connect(endpoint)).await {
        Ok(Ok(c)) => c
            .with_timeout(timeout)
            .request::<m::SystemVersion>(Empty)
            .await
            .ok(),
        _ => None,
    };
    if json {
        return print_json(&json!({ "cli": cli, "daemon": daemon }));
    }
    let mut s = format!("yangpa {cli}\n");
    match daemon {
        Some(v) => s.push_str(&format!(
            "yangpad {} (프로토콜 {})\n",
            v.version, v.protocol
        )),
        None => s.push_str("yangpad: 연결 안 됨\n"),
    }
    out(&s)
}

impl Ctx {
    pub fn new(client: RpcClient, json: bool) -> Self {
        Ctx {
            client,
            json,
            spaces: None,
        }
    }

    pub async fn run(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Status => self.status().await,
            Command::Version | Command::Daemon(_) => bail!("이 명령은 연결 전에 처리됩니다"),
            Command::Shutdown => {
                self.client.request::<m::SystemShutdown>(Empty).await?;
                self.done("데몬을 종료합니다.")
            }
            Command::Watch(a) => self.watch(a).await,
            Command::Identity(c) => self.identity(c).await,
            Command::Device(c) => self.device(c).await,
            Command::Profile(c) => self.profile(c).await,
            Command::Note(c) => self.note(c).await,
            Command::Friend(c) => self.friend(c).await,
            Command::Trust(c) => self.trust(c).await,
            Command::Space(c) => self.space_cmd(c).await,
            Command::Crew(c) => self.crew(c).await,
            Command::Member(c) => self.member(c).await,
            Command::User(c) => self.user_cmd(c).await,
            Command::Report(c) => self.report_cmd(c).await,
            Command::Channel(c) => self.channel_cmd(c).await,
            Command::Message(c) => self.message(c).await,
            Command::Forum(c) => self.forum(c).await,
            Command::Blob(c) => self.blob(c).await,
            Command::Emoji(c) => self.emoji(c).await,
            Command::Role(c) => self.role(c).await,
            Command::Invite(c) => self.invite(c).await,
            Command::Seed(c) => self.seed(c).await,
            Command::Archive(c) => self.archive(c).await,
            Command::Peer(c) => self.peer(c).await,
            Command::Net(c) => self.net(c).await,
            Command::Token(c) => self.token(c).await,
            Command::Audit(c) => self.audit(c).await,
            Command::Confirm(c) => self.confirm(c).await,
            Command::Diag(c) => self.diag(c).await,
            Command::Call(a) => self.call(a).await,
        }
    }

    // ───────────── 출력 ─────────────

    fn emit<T: Serialize>(&self, v: &T, human: impl FnOnce(&T) -> String) -> Result<()> {
        if self.json {
            print_json(v)
        } else {
            out(&human(v))
        }
    }

    fn done(&self, msg: &str) -> Result<()> {
        if self.json {
            print_json(&json!({}))
        } else {
            out(&format!("{msg}\n"))
        }
    }

    // ───────────── 이름 → ID ─────────────

    async fn space_list(&mut self) -> Result<&[SpaceSummary]> {
        if self.spaces.is_none() {
            self.spaces = Some(self.client.request::<m::SpaceList>(Empty).await?);
        }
        Ok(self.spaces.as_deref().unwrap_or_default())
    }

    async fn space(&mut self, input: &str) -> Result<SpaceId> {
        let list = self.space_list().await?;
        let items: Vec<Candidate<SpaceId>> = list
            .iter()
            .map(|s| Candidate::new(s.space, s.name.clone()))
            .collect();
        resolve(input, "크루", &items).map_err(|e| anyhow!(e))
    }

    async fn opt_space(&mut self, input: Option<&str>) -> Result<Option<SpaceId>> {
        match input {
            Some(s) => Ok(Some(self.space(s).await?)),
            None => Ok(None),
        }
    }

    /// 공간 ID → 이름(표시용, 실패하면 빈 표).
    async fn space_names(&mut self) -> HashMap<SpaceId, String> {
        match self.space_list().await {
            Ok(l) => l.iter().map(|s| (s.space, s.name.clone())).collect(),
            Err(_) => HashMap::new(),
        }
    }

    async fn channel_list(&self, space: SpaceId) -> Result<Vec<ChannelView>> {
        Ok(self
            .client
            .request::<m::ChannelList>(SpaceParams { space })
            .await?)
    }

    fn channel_candidates(list: &[ChannelView]) -> Vec<Candidate<ChannelId>> {
        list.iter()
            .map(|c| Candidate::new(c.channel, c.settings.name.clone()))
            .collect()
    }

    async fn channel(&self, space: SpaceId, input: &str) -> Result<ChannelId> {
        let input = strip_sigil(input, '#');
        // ID면 목록을 받지 않습니다.
        if let Ok(id) = ChannelId::from_str(input) {
            return Ok(id);
        }
        let list = self.channel_list(space).await?;
        resolve(input, "채널", &Self::channel_candidates(&list)).map_err(|e| anyhow!(e))
    }

    async fn channels(&self, space: SpaceId, inputs: &[String]) -> Result<Vec<ChannelId>> {
        let inputs: Vec<String> = inputs
            .iter()
            .map(|s| strip_sigil(s, '#').to_string())
            .collect();
        if inputs
            .iter()
            .all(|s| s.trim().is_empty() || ChannelId::from_str(s.trim()).is_ok())
        {
            return resolve_all(&inputs, "채널", &[]).map_err(|e| anyhow!(e));
        }
        let list = self.channel_list(space).await?;
        resolve_all(&inputs, "채널", &Self::channel_candidates(&list)).map_err(|e| anyhow!(e))
    }

    async fn target(&mut self, t: &ChannelTarget) -> Result<(SpaceId, ChannelId)> {
        let space = self.space(&t.crew.crew).await?;
        let channel = self.channel(space, &t.channel).await?;
        Ok((space, channel))
    }

    async fn role_list(&self, space: SpaceId) -> Result<Vec<Role>> {
        Ok(self
            .client
            .request::<m::RoleList>(SpaceParams { space })
            .await?)
    }

    fn role_candidates(list: &[Role]) -> Vec<Candidate<RoleId>> {
        list.iter()
            .map(|r| {
                let mut names = vec![r.name.clone()];
                if r.id == RoleId::EVERYONE {
                    names.push("everyone".into());
                    names.push("@everyone".into());
                }
                Candidate::with_names(r.id, names)
            })
            .collect()
    }

    async fn role_id(&self, space: SpaceId, input: &str) -> Result<RoleId> {
        let list = self.role_list(space).await?;
        resolve(input, "역할", &Self::role_candidates(&list)).map_err(|e| anyhow!(e))
    }

    async fn role_ids(&self, space: SpaceId, inputs: &[String]) -> Result<Vec<RoleId>> {
        if inputs.iter().all(|s| s.trim().is_empty()) {
            return Ok(Vec::new());
        }
        let list = self.role_list(space).await?;
        resolve_all(inputs, "역할", &Self::role_candidates(&list)).map_err(|e| anyhow!(e))
    }

    async fn member_list(&self, space: SpaceId) -> Result<Vec<MemberView>> {
        Ok(self
            .client
            .request::<m::MemberList>(SpaceParams { space })
            .await?)
    }

    async fn member_id(&self, space: SpaceId, input: &str) -> Result<UserId> {
        let input = strip_sigil(input, '@');
        if let Ok(id) = UserId::from_str(input) {
            return Ok(id);
        }
        let list = self.member_list(space).await?;
        let items: Vec<Candidate<UserId>> = list
            .iter()
            .map(|m| {
                Candidate::with_names(
                    m.user,
                    [
                        m.profile.name.clone(),
                        m.nickname.clone().unwrap_or_default(),
                    ],
                )
            })
            .collect();
        resolve(input, "멤버", &items).map_err(|e| anyhow!(e))
    }

    /// 친구 이름이나 사용자 ID.
    async fn friend_user(&self, input: &str) -> Result<UserId> {
        let input = strip_sigil(input, '@');
        if let Ok(id) = UserId::from_str(input) {
            return Ok(id);
        }
        let list = self.client.request::<m::FriendList>(Empty).await?;
        let items: Vec<Candidate<UserId>> = list
            .iter()
            .map(|f| Candidate::new(f.user, f.profile.name.clone()))
            .collect();
        resolve(input, "연락처", &items).map_err(|e| anyhow!(e))
    }

    /// 밴 목록의 이름이나 사용자 ID(밴된 사람은 멤버 목록에 없으므로).
    async fn banned_user(&self, space: SpaceId, input: &str) -> Result<UserId> {
        let input = strip_sigil(input, '@');
        if let Ok(id) = UserId::from_str(input) {
            return Ok(id);
        }
        let list = self
            .client
            .request::<m::MemberBans>(SpaceParams { space })
            .await?;
        let items: Vec<Candidate<UserId>> = list
            .iter()
            .map(|b| Candidate::with_names(b.user, b.profile.as_ref().map(|p| p.name.clone())))
            .collect();
        resolve(input, "밴된 사용자", &items).map_err(|e| anyhow!(e))
    }

    /// 차단 목록의 이름이나 사용자 ID.
    async fn blocked_user(&self, input: &str) -> Result<UserId> {
        let input = strip_sigil(input, '@');
        if let Ok(id) = UserId::from_str(input) {
            return Ok(id);
        }
        let list = self.client.request::<m::UserBlocked>(Empty).await?;
        let items: Vec<Candidate<UserId>> = list
            .iter()
            .map(|b| Candidate::with_names(b.user, b.profile.as_ref().map(|p| p.name.clone())))
            .collect();
        resolve(input, "차단한 사용자", &items).map_err(|e| anyhow!(e))
    }

    async fn names_in(&self, space: SpaceId) -> HashMap<UserId, String> {
        // 이름 표시는 부가 기능이라 실패해도 ID로 출력합니다.
        match self.member_list(space).await {
            Ok(list) => list
                .into_iter()
                .map(|m| (m.user, m.nickname.unwrap_or(m.profile.name)))
                .collect(),
            Err(_) => HashMap::new(),
        }
    }

    /// 채널 ID → `#이름`(표시용, 실패하면 빈 표).
    async fn channel_names(&self, space: Option<SpaceId>) -> HashMap<ChannelId, String> {
        match space {
            Some(s) => self
                .channel_list(s)
                .await
                .map(|l| {
                    l.into_iter()
                        .map(|c| (c.channel, format!("#{}", c.settings.name)))
                        .collect()
                })
                .unwrap_or_default(),
            None => HashMap::new(),
        }
    }

    // ───────────── 명령 ─────────────

    async fn status(&mut self) -> Result<()> {
        let s = self.client.request::<m::SystemStatus>(Empty).await?;
        let v = self.client.request::<m::SystemVersion>(Empty).await.ok();
        if self.json {
            return print_json(&json!({ "status": s, "version": v }));
        }
        let ver = v.map(|v| format!("{} (프로토콜 {})", v.version, v.protocol));
        out(&output::status(&s, ver.as_deref()))
    }

    async fn watch(&mut self, a: WatchArgs) -> Result<()> {
        let only = self.opt_space(a.crew.as_deref()).await?;
        let topics: Vec<String> = a
            .topics
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if let Some(bad) = topics.iter().find(|t| !events::valid_topic(t)) {
            bail!(
                "알 수 없는 주제: {bad} (예: message, message.new, space.updated, friend, confirm.pending)"
            );
        }
        let mut sub = self.client.subscribe(topics).await?;
        if std::io::stderr().is_terminal() {
            eprintln!("이벤트를 기다립니다. 끝내려면 Ctrl+C.");
        }
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            tokio::select! {
                _ = &mut ctrl_c => return Ok(()),
                batch = sub.next() => {
                    let Some(batch) = batch else {
                        bail!(ClientError::Closed);
                    };
                    let mut buf = String::new();
                    for n in batch {
                        // 통화 소리 패킷은 초당 수십 개라 화면을 덮고, 말한 때를 그대로 보여 줍니다.
                        if !a.show_volatile && matches!(n, Notification::VoiceMedia { .. }) {
                            continue;
                        }
                        if let (Some(want), Some(got)) = (only, n.space())
                            && want != got
                        {
                            continue;
                        }
                        if self.json {
                            let n = if a.show_volatile { n } else { hide_volatile(n) };
                            buf.push_str(&serde_json::to_string(&n)?);
                        } else {
                            buf.push_str(&output::notification_line(&n, a.show_volatile));
                        }
                        buf.push('\n');
                    }
                    // 읽는 쪽이 닫혔으면(`yangpa watch | head`) 끝냅니다. 계속 돌면 영원히 끝나지 않습니다.
                    if !write_to(&mut std::io::stdout().lock(), &buf)? {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn identity(&mut self, c: IdentityCmd) -> Result<()> {
        match c {
            IdentityCmd::Create { name } => {
                let r = self
                    .client
                    .request::<m::IdentityCreate>(IdentityCreateParams { name })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "신원을 만들었습니다.\n사용자: {}\n기기: {}\n",
                        r.user, r.device
                    )
                })
            }
            IdentityCmd::Show => {
                let r = self.client.request::<m::IdentityGet>(Empty).await?;
                self.emit(&r, output::identity)
            }
        }
    }

    async fn device(&mut self, c: DeviceCmd) -> Result<()> {
        match c {
            DeviceCmd::List => {
                let r = self.client.request::<m::DeviceList>(Empty).await?;
                let list: Vec<_> = r.into_iter().map(|d| d.entry).collect();
                self.emit(&list, |r| output::devices(r, None))
            }
            DeviceCmd::Rename { device, name } => {
                let device: DeviceId = parse_id(&device, "기기 ID")?;
                self.client
                    .request::<m::DeviceRename>(DeviceRenameParams { device, name })
                    .await?;
                self.done("기기 이름을 바꿨습니다.")
            }
            DeviceCmd::Revoke { device } => {
                let device: DeviceId = parse_id(&device, "기기 ID")?;
                self.client
                    .request::<m::DeviceRevoke>(DeviceRevokeParams { device })
                    .await?;
                self.done("기기를 폐기했습니다.")
            }
            DeviceCmd::Link => {
                let r = self.client.request::<m::DeviceLinkCreate>(Empty).await?;
                self.emit(&r, |r| {
                    format!(
                        "새 기기에 이 링크를 넣으세요(10분 안에, 한 기기만):\n{}\n\n그다음 `yangpa device link-pending`으로 번호를 보고 승인하세요.\n",
                        r.url.expose()
                    )
                })
            }
            DeviceCmd::LinkPending => {
                let r = self.client.request::<m::DeviceLinkPending>(Empty).await?;
                self.emit(&r, |r| match r {
                    Some(v) => output::device_link_request(v),
                    None => "승인을 기다리는 기기가 없습니다.\n".to_owned(),
                })
            }
            DeviceCmd::LinkApprove => {
                self.client
                    .request::<m::DeviceLinkDecide>(DeviceLinkDecideParams { approve: true })
                    .await?;
                self.done("새 기기를 승인했습니다.")
            }
            DeviceCmd::LinkReject => {
                self.client
                    .request::<m::DeviceLinkDecide>(DeviceLinkDecideParams { approve: false })
                    .await?;
                self.done("새 기기를 거절했습니다.")
            }
            DeviceCmd::LinkCancel => {
                self.client.request::<m::DeviceLinkCancel>(Empty).await?;
                self.done("기기 연결 링크를 끝냈습니다.")
            }
            DeviceCmd::Join => {
                let url = read_secret_line("기기 연결 링크")?;
                let r = self
                    .client
                    .request::<m::DeviceLinkJoin>(DeviceLinkJoinParams { url })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "내 기기에 뜨는 번호가 이 번호와 같은지 보고 승인하세요: {}\n",
                        r.code
                    )
                })
            }
            DeviceCmd::JoinAbort => {
                self.client.request::<m::DeviceLinkAbort>(Empty).await?;
                self.done("연결 요청을 그만뒀습니다.")
            }
        }
    }

    async fn profile(&mut self, c: ProfileCmd) -> Result<()> {
        match c {
            ProfileCmd::Show { user } => {
                let user = match user {
                    Some(u) => Some(self.friend_user(&u).await?),
                    None => None,
                };
                let r = self
                    .client
                    .request::<m::ProfileGet>(ProfileGetParams { user })
                    .await?;
                self.emit(&r, output::profile)
            }
            ProfileCmd::Set(a) => {
                let p = ProfileSetParams {
                    name: a.name,
                    bio: a.bio,
                    avatar_path: a.avatar.as_deref().map(abs_path).transpose()?,
                    banner_path: a.banner.as_deref().map(abs_path).transpose()?,
                    theme_color: if a.no_color {
                        Some(None)
                    } else {
                        a.color.map(Some)
                    },
                    status: a.status,
                };
                if p == ProfileSetParams::default() {
                    bail!(
                        "바꿀 항목을 하나 이상 지정하세요(--name, --bio, --avatar, --banner, --color, --status)"
                    );
                }
                let r = self.client.request::<m::ProfileSet>(p).await?;
                self.emit(&r, output::profile)
            }
            ProfileCmd::Nickname { crew, nickname } => {
                let space = self.space(&crew.crew).await?;
                self.client
                    .request::<m::ProfileSetCrew>(ProfileSetCrewParams { space, nickname })
                    .await?;
                self.done("크루 닉네임을 바꿨습니다.")
            }
        }
    }

    async fn trust(&mut self, c: TrustCmd) -> Result<()> {
        // 켤 때마다 앱의 경고창과 같은 뜻을 알립니다(JSON 출력에는 섞지 않음).
        const WARN: &str = "주의: 신뢰하면 상대에게 이 기기의 IP 주소가 보일 수 있습니다(대략의 위치·통신사를 알 수 있음). \
                            서로 신뢰하면 P2P로, 한쪽만 신뢰하면 신뢰한 쪽의 IP만 드러나는 단방향으로 잇습니다.";
        match c {
            TrustCmd::Get { user } => {
                let user = self.friend_user(&user).await?;
                let v = self
                    .client
                    .request::<m::TrustGet>(UserParams { user })
                    .await?;
                let crews = if self.json {
                    Vec::new()
                } else {
                    let list = self.space_list().await?;
                    v.via_crews
                        .iter()
                        .map(|s| {
                            list.iter()
                                .find(|x| x.space == *s)
                                .map(|x| x.name.clone())
                                .unwrap_or_else(|| s.to_string())
                        })
                        .collect()
                };
                self.emit(&v, |v| {
                    let mut s = format!(
                        "신뢰: {}\n연결: {}\n",
                        if v.trusted { "켬" } else { "끔" },
                        output::connection_mode(v.connection)
                    );
                    if !crews.is_empty() {
                        s.push_str(&format!("크루 신뢰로 내 IP가 보임: {}\n", crews.join(", ")));
                    }
                    s
                })
            }
            TrustCmd::User { user, off } => {
                let user = self.friend_user(&user).await?;
                self.client
                    .request::<m::TrustSetUser>(TrustSetUserParams {
                        user,
                        enabled: !off,
                    })
                    .await?;
                if !off && !self.json {
                    eprintln!("{WARN}");
                }
                self.done(if off {
                    "신뢰를 껐습니다."
                } else {
                    "신뢰를 켰습니다."
                })
            }
            TrustCmd::Crew { crew, off } => {
                let space = self.space(&crew).await?;
                self.client
                    .request::<m::TrustSetSpace>(TrustSetSpaceParams {
                        space,
                        enabled: !off,
                    })
                    .await?;
                if !off && !self.json {
                    eprintln!("{WARN}");
                }
                self.done(if off {
                    "이 크루의 신뢰를 껐습니다."
                } else {
                    "이 크루를 신뢰합니다."
                })
            }
        }
    }

    async fn note(&mut self, c: NoteCmd) -> Result<()> {
        match c {
            NoteCmd::Show { user } => {
                let user = self.friend_user(&user).await?;
                let r = self
                    .client
                    .request::<m::NoteGet>(UserParams { user })
                    .await?;
                self.emit(&r, |r| {
                    if r.text.is_empty() {
                        "메모가 없습니다.\n".into()
                    } else {
                        format!("{}\n", r.text)
                    }
                })
            }
            NoteCmd::Set { user, text } => {
                let user = self.friend_user(&user).await?;
                self.client
                    .request::<m::NoteSet>(NoteSetParams { user, text })
                    .await?;
                self.done("메모를 저장했습니다.")
            }
        }
    }

    async fn friend(&mut self, c: FriendCmd) -> Result<()> {
        match c {
            FriendCmd::Link(FriendLinkCmd::Create {
                expires,
                single_use,
            }) => {
                let r = self
                    .client
                    .request::<m::FriendLinkCreate>(FriendLinkCreateParams {
                        expires_in_secs: expires.and_then(|e| e.0),
                        single_use: single_use.then_some(true),
                    })
                    .await?;
                self.emit(&r, |r| {
                    let exp = r
                        .expires_at
                        .map(|t| format!("\n만료: {}", output::format_time(t)))
                        .unwrap_or_default();
                    format!("{}{exp}\n이 링크를 친구에게 보내세요.\n", r.url)
                })
            }
            FriendCmd::Link(FriendLinkCmd::List) => {
                let r = self.client.request::<m::FriendLinkList>(Empty).await?;
                self.emit(&r, |r| output::friend_links(r))
            }
            FriendCmd::Link(FriendLinkCmd::Revoke { token_id }) => {
                self.client
                    .request::<m::FriendLinkRevoke>(TokenIdParams { token_id })
                    .await?;
                self.done("연락처 링크를 폐기했습니다(이 링크로 생긴 임시 연락처도 만료됩니다).")
            }
            FriendCmd::Add { url } => {
                let r = self
                    .client
                    .request::<m::FriendAdd>(UrlParams { url })
                    .await?;
                self.emit(&r, |r| match r.status {
                    FriendAddStatus::Pending => format!(
                        "연락처 링크를 보냈습니다({}). 상대 기기가 받으면 임시 연락처로 대화가 생깁니다.\n",
                        r.user
                    ),
                    FriendAddStatus::Connected => format!("이미 연결된 연락처입니다: {}\n", r.user),
                })
            }
            FriendCmd::Propose { user } => {
                let user = self.friend_user(&user).await?;
                let r = self
                    .client
                    .request::<m::FriendPropose>(UserParams { user })
                    .await?;
                self.emit(&r, |r| match r.status {
                    FriendProposeStatus::Requested => {
                        "친구 추가를 보냈습니다. 상대가 수락하면 친구가 됩니다.\n".to_string()
                    }
                    FriendProposeStatus::Friends => "친구가 됐습니다.\n".to_string(),
                })
            }
            FriendCmd::Requests => {
                let r = self.client.request::<m::FriendRequests>(Empty).await?;
                self.emit(&r, |r| output::friend_requests(r))
            }
            FriendCmd::Accept { request_id } => {
                let r = self
                    .client
                    .request::<m::FriendAccept>(RequestIdParams { request_id })
                    .await?;
                self.emit(&r, |r| {
                    format!("친구 요청을 수락했습니다. 1대1 공간: {}\n", r.space)
                })
            }
            FriendCmd::Reject { request_id } => {
                self.client
                    .request::<m::FriendReject>(RequestIdParams { request_id })
                    .await?;
                self.done("친구 요청을 거절했습니다.")
            }
            FriendCmd::List => {
                let r = self.client.request::<m::FriendList>(Empty).await?;
                self.emit(&r, |r| output::friends(r))
            }
            FriendCmd::Remove { user } => {
                let user = self.friend_user(&user).await?;
                self.client
                    .request::<m::FriendRemove>(UserParams { user })
                    .await?;
                self.done("친구를 삭제했습니다.")
            }
            FriendCmd::SafetyNumber { user } => {
                let user = self.friend_user(&user).await?;
                let r = self
                    .client
                    .request::<m::FriendSafetyNumber>(UserParams { user })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "{}\n{}\n",
                        group_digits(&r.digits),
                        if r.verified { "확인함" } else { "아직 확인하지 않음: 직접 만나거나 통화로 비교한 뒤 `yangpa friend verify`" }
                    )
                })
            }
            FriendCmd::Verify { user, unset } => {
                let user = self.friend_user(&user).await?;
                self.client
                    .request::<m::FriendVerify>(FriendVerifyParams {
                        user,
                        verified: !unset,
                    })
                    .await?;
                self.done(if unset {
                    "확인 표시를 지웠습니다."
                } else {
                    "안전 번호를 확인함으로 표시했습니다."
                })
            }
        }
    }

    async fn space_cmd(&mut self, c: SpaceCmd) -> Result<()> {
        match c {
            SpaceCmd::List => {
                let r = self.space_list().await?.to_vec();
                self.emit(&r, |r| output::spaces(r))
            }
            SpaceCmd::Show { space } => {
                let space = self.space(&space).await?;
                let r = self
                    .client
                    .request::<m::SpaceGet>(SpaceParams { space })
                    .await?;
                self.emit(&r, output::space)
            }
        }
    }

    async fn crew(&mut self, c: CrewCmd) -> Result<()> {
        match c {
            CrewCmd::Create { name, description } => {
                let r = self
                    .client
                    .request::<m::CrewCreate>(CrewCreateParams { name, description })
                    .await?;
                self.emit(&r, |r| format!("크루를 만들었습니다: {}\n", r.space))
            }
            CrewCmd::List => {
                let r: Vec<SpaceSummary> = self
                    .space_list()
                    .await?
                    .iter()
                    .filter(|s| s.kind == SpaceKind::Crew)
                    .cloned()
                    .collect();
                self.emit(&r, |r| output::spaces(r))
            }
            CrewCmd::Update {
                crew,
                name,
                description,
                icon,
            } => {
                if name.is_none() && description.is_none() && icon.is_none() {
                    bail!("바꿀 항목을 하나 이상 지정하세요(--name, --description, --icon)");
                }
                let space = self.space(&crew.crew).await?;
                self.client
                    .request::<m::CrewUpdate>(CrewUpdateParams {
                        space,
                        name,
                        description,
                        icon_path: icon.as_deref().map(abs_path).transpose()?,
                        strict_rotation: None,
                        emoji_limits: None,
                    })
                    .await?;
                self.done("크루 설정을 바꿨습니다.")
            }
            CrewCmd::Convert { crew, name } => {
                let space = self.space(&crew.crew).await?;
                self.client
                    .request::<m::CrewConvertRequest>(CrewConvertRequestParams { space, name })
                    .await?;
                self.done("크루 전환을 요청했습니다. 상대가 동의하면 크루가 됩니다.")
            }
            CrewCmd::AcceptConvert { crew } => {
                let space = self.space(&crew.crew).await?;
                self.client
                    .request::<m::CrewConvertAccept>(SpaceParams { space })
                    .await?;
                self.done("크루 전환에 동의했습니다.")
            }
            CrewCmd::Leave { crew } => {
                let space = self.space(&crew.crew).await?;
                self.client
                    .request::<m::CrewLeave>(SpaceParams { space })
                    .await?;
                self.done("크루에서 나왔습니다.")
            }
            CrewCmd::Transfer { crew, to } => {
                let space = self.space(&crew.crew).await?;
                let user = self.member_id(space, &to).await?;
                self.client
                    .request::<m::CrewTransferOwnership>(TransferOwnershipParams { space, user })
                    .await?;
                self.done("소유권을 넘겼습니다.")
            }
            CrewCmd::Committer { crew, set } => {
                let space = self.space(&crew.crew).await?;
                let list = self
                    .client
                    .request::<m::CrewCommitterCandidates>(SpaceParams { space })
                    .await?;
                let Some(input) = set else {
                    let names = self.names_in(space).await;
                    return self.emit(&list, |l| output::committers(l, &names));
                };
                let items: Vec<Candidate<DeviceId>> = list
                    .iter()
                    .map(|c| Candidate::new(c.device, c.name.clone()))
                    .collect();
                let device = resolve(&input, "확정 기기 후보", &items).map_err(|e| anyhow!(e))?;
                self.client
                    .request::<m::CrewSetCommitter>(CommitterSetParams { space, device })
                    .await?;
                self.done("확정 기기를 바꿨습니다. 새 기기가 그룹을 넘겨받는 동안 잠시 가입·기기 추가가 늦어질 수 있습니다.")
            }
        }
    }

    async fn member(&mut self, c: MemberCmd) -> Result<()> {
        match c {
            MemberCmd::List { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self.member_list(space).await?;
                let roles = if self.json {
                    Vec::new()
                } else {
                    self.role_list(space).await.unwrap_or_default()
                };
                self.emit(&r, |r| output::members(r, &roles))
            }
            MemberCmd::Kick { crew, user, reason } => {
                let space = self.space(&crew.crew).await?;
                let user = self.member_id(space, &user).await?;
                self.client
                    .request::<m::MemberKick>(MemberKickParams {
                        space,
                        user,
                        reason,
                    })
                    .await?;
                self.done(
                    "멤버를 추방했습니다. 이미 받은 데이터는 그 멤버 기기에 남을 수 있습니다.",
                )
            }
            MemberCmd::Roles { crew, user, roles } => {
                let space = self.space(&crew.crew).await?;
                let user = self.member_id(space, &user).await?;
                let roles = self.role_ids(space, &roles).await?;
                self.client
                    .request::<m::MemberSetRoles>(MemberSetRolesParams { space, user, roles })
                    .await?;
                self.done("멤버 역할을 바꿨습니다.")
            }
            MemberCmd::Ban {
                crew,
                user,
                reason,
                delete_recent,
            } => {
                let space = self.space(&crew.crew).await?;
                let user = self.member_id(space, &user).await?;
                self.client
                    .request::<m::MemberBan>(MemberBanParams {
                        space,
                        user,
                        reason,
                        delete_recent,
                    })
                    .await?;
                self.done(if delete_recent {
                    "밴했고 최근 24시간 메시지도 지웠습니다. 이미 받은 데이터는 그 사람 기기에 남을 수 있습니다."
                } else {
                    "밴했습니다. 이미 받은 데이터는 그 사람 기기에 남을 수 있습니다."
                })
            }
            MemberCmd::Unban { crew, user } => {
                let space = self.space(&crew.crew).await?;
                let user = self.banned_user(space, &user).await?;
                self.client
                    .request::<m::MemberUnban>(SpaceUserParams { space, user })
                    .await?;
                self.done("밴을 풀었습니다. 다시 들어오려면 초대가 필요합니다.")
            }
            MemberCmd::Bans { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::MemberBans>(SpaceParams { space })
                    .await?;
                let names = if self.json {
                    HashMap::new()
                } else {
                    self.names_in(space).await
                };
                self.emit(&r, |r| output::bans(r, &names))
            }
        }
    }

    async fn user_cmd(&mut self, c: UserCmd) -> Result<()> {
        match c {
            UserCmd::Block { user, crew } => {
                let user = self.user_in(&user, crew).await?;
                self.client
                    .request::<m::UserBlock>(UserParams { user })
                    .await?;
                self.done("차단했습니다. 상대에게는 알리지 않습니다.")
            }
            UserCmd::Unblock { user } => {
                let user = self.blocked_user(&user).await?;
                self.client
                    .request::<m::UserUnblock>(UserParams { user })
                    .await?;
                self.done("차단을 풀었습니다.")
            }
            UserCmd::Blocked => {
                let r = self.client.request::<m::UserBlocked>(Empty).await?;
                self.emit(&r, |r| output::blocked(r))
            }
            UserCmd::Devices { user, crew } => {
                let user = self.user_in(&user, crew).await?;
                let r = self
                    .client
                    .request::<m::UserDevices>(UserParams { user })
                    .await?;
                self.emit(&r, |r| output::user_devices(r))
            }
            UserCmd::BlockDevice { user, device, crew } => {
                let user = self.user_in(&user, crew).await?;
                let device: DeviceId = parse_id(&device, "기기 ID")?;
                self.client
                    .request::<m::UserBlockDevice>(UserDeviceParams { user, device })
                    .await?;
                self.done("그 기기를 차단했습니다. 상대에게는 알리지 않습니다.")
            }
            UserCmd::UnblockDevice { device } => {
                let device: DeviceId = parse_id(&device, "기기 ID")?;
                let list = self.client.request::<m::UserBlockedDevices>(Empty).await?;
                let user = list
                    .iter()
                    .find(|b| b.device == device)
                    .map(|b| b.user)
                    .ok_or_else(|| anyhow!("차단한 기기가 아닙니다"))?;
                self.client
                    .request::<m::UserUnblockDevice>(UserDeviceParams { user, device })
                    .await?;
                self.done("기기 차단을 풀었습니다.")
            }
            UserCmd::BlockedDevices => {
                let r = self.client.request::<m::UserBlockedDevices>(Empty).await?;
                self.emit(&r, |r| output::blocked_devices(r))
            }
        }
    }

    /// 친구 이름·사용자 ID, `crew`를 주면 그 크루 멤버 이름으로 사용자를 찾습니다.
    async fn user_in(&mut self, user: &str, crew: Option<String>) -> Result<UserId> {
        match crew {
            Some(c) => {
                let space = self.space(&c).await?;
                self.member_id(space, user).await
            }
            None => self.friend_user(user).await,
        }
    }

    async fn report_cmd(&mut self, c: ReportCmd) -> Result<()> {
        match c {
            ReportCmd::List { crew, status } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::ReportList>(ReportListParams {
                        space,
                        status: status.map(report_status),
                    })
                    .await?;
                let (names, channels) = if self.json {
                    (HashMap::new(), HashMap::new())
                } else {
                    (
                        self.names_in(space).await,
                        self.channel_names(Some(space)).await,
                    )
                };
                self.emit(&r, |r| output::reports(r, &names, &channels))
            }
            ReportCmd::Resolve {
                crew,
                report_id,
                action,
                note,
            } => {
                let space = self.space(&crew.crew).await?;
                let action = report_action(action);
                self.client
                    .request::<m::ReportResolve>(ReportResolveParams {
                        space,
                        report_id: report_id.trim().to_string(),
                        action,
                        note,
                    })
                    .await?;
                self.done(match action {
                    ReportAction::Dismiss => "신고를 닫았습니다.",
                    ReportAction::DeleteMessage => "메시지를 지우고 신고를 닫았습니다.",
                    ReportAction::Kick => "작성자를 추방하고 신고를 닫았습니다.",
                    ReportAction::Ban => "작성자를 밴하고 신고를 닫았습니다.",
                })
            }
        }
    }

    async fn channel_cmd(&mut self, c: ChannelCmd) -> Result<()> {
        match c {
            ChannelCmd::List { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self.channel_list(space).await?;
                self.emit(&r, |r| output::channels(r))
            }
            ChannelCmd::Create(a) => {
                let space = self.space(&a.crew.crew).await?;
                let parent = match &a.parent {
                    Some(p) => Some(self.channel(space, p).await?),
                    None => None,
                };
                let private_roles = match &a.private_roles {
                    Some(r) => Some(self.role_ids(space, r).await?),
                    None => None,
                };
                let (ttl_secs, ttl_on_leave) = match a.ttl {
                    Some(TtlArg::Seconds(s)) => (Some(s), None),
                    Some(TtlArg::OnLeave) => (None, Some(true)),
                    Some(TtlArg::None) | None => (None, None),
                };
                let p = ChannelCreateParams {
                    space,
                    name: a.name,
                    kind: channel_kind(a.kind),
                    parent,
                    topic: a.topic,
                    security: a.volatile.then_some(SecurityLevel::Volatile),
                    join_mode: a.join_leave.then_some(JoinMode::JoinLeave),
                    ttl_secs,
                    ttl_on_leave,
                    history: a.history.map(history),
                    link_previews: a.link_previews,
                    private_roles,
                    position: a.position,
                    forum_tags: None,
                    p2p: None,
                    overwrites: None,
                    anonymous: None,
                    attach_limit: None,
                    no_edit: None,
                    no_delete: None,
                };
                let r = self.client.request::<m::ChannelCreate>(p).await?;
                self.emit(&r, |r| format!("채널을 만들었습니다: {}\n", r.channel))
            }
            ChannelCmd::Update(a) => {
                let space = self.space(&a.crew.crew).await?;
                let channel = self.channel(space, &a.channel).await?;
                let mut p = ChannelUpdateParams::new(space, channel);
                p.name = a.name;
                p.topic = a.topic;
                if a.no_parent {
                    p.parent = Some(None);
                } else if let Some(parent) = &a.parent {
                    p.parent = Some(Some(self.channel(space, parent).await?));
                }
                if a.volatile {
                    p.security = Some(SecurityLevel::Volatile);
                } else if a.normal {
                    p.security = Some(SecurityLevel::Normal);
                }
                if a.join_leave {
                    p.join_mode = Some(JoinMode::JoinLeave);
                } else if a.always {
                    p.join_mode = Some(JoinMode::Always);
                }
                match a.ttl {
                    Some(TtlArg::Seconds(s)) => p.ttl_secs = Some(s),
                    Some(TtlArg::OnLeave) => p.ttl_on_leave = Some(true),
                    Some(TtlArg::None) => {
                        p.ttl_secs = Some(0);
                        p.ttl_on_leave = Some(false);
                    }
                    None => {}
                }
                p.history = a.history.map(history);
                p.link_previews = a.link_previews;
                if a.public {
                    p.private_roles = Some(Vec::new());
                } else if let Some(r) = &a.private_roles {
                    p.private_roles = Some(self.role_ids(space, r).await?);
                }
                p.position = a.position;
                if p == ChannelUpdateParams::new(space, channel) {
                    bail!("바꿀 항목을 하나 이상 지정하세요");
                }
                self.client.request::<m::ChannelUpdate>(p).await?;
                self.done("채널 설정을 바꿨습니다.")
            }
            ChannelCmd::Delete { crew, channel } => {
                let (space, channel) = self.space_channel(&crew, &channel).await?;
                self.client
                    .request::<m::ChannelDelete>(SpaceChannelParams { space, channel })
                    .await?;
                self.done("채널을 지웠습니다.")
            }
            ChannelCmd::Join { crew, channel } => {
                let (space, channel) = self.space_channel(&crew, &channel).await?;
                self.client
                    .request::<m::ChannelJoin>(SpaceChannelParams { space, channel })
                    .await?;
                self.done("채널에 들어갔습니다.")
            }
            ChannelCmd::Leave { crew, channel } => {
                let (space, channel) = self.space_channel(&crew, &channel).await?;
                self.client
                    .request::<m::ChannelLeave>(SpaceChannelParams { space, channel })
                    .await?;
                self.done("채널에서 나왔습니다.")
            }
            ChannelCmd::AllowApiRead {
                crew,
                channel,
                allowed,
            } => {
                let (space, channel) = self.space_channel(&crew, &channel).await?;
                self.client
                    .request::<m::ChannelAllowApiRead>(AllowApiReadParams {
                        space,
                        channel,
                        allowed,
                    })
                    .await?;
                self.done(if allowed {
                    "이 휘발성 채널을 API(AI)가 읽을 수 있게 했습니다. 읽힌 내용은 사실상 기록이 될 수 있습니다."
                } else {
                    "이 휘발성 채널의 API 읽기를 막았습니다."
                })
            }
            ChannelCmd::MarkRead {
                crew,
                channel,
                event,
            } => {
                let (space, channel) = self.space_channel(&crew, &channel).await?;
                let event: EventId = parse_id(&event, "메시지 ID")?;
                self.client
                    .request::<m::ChannelMarkRead>(MarkReadParams {
                        space,
                        channel,
                        event: Some(event),
                    })
                    .await?;
                self.done("읽음으로 표시했습니다.")
            }
        }
    }

    async fn space_channel(
        &mut self,
        crew: &CrewArg,
        channel: &str,
    ) -> Result<(SpaceId, ChannelId)> {
        let space = self.space(&crew.crew).await?;
        let channel = self.channel(space, channel).await?;
        Ok((space, channel))
    }

    async fn message(&mut self, c: MessageCmd) -> Result<()> {
        match c {
            MessageCmd::List {
                target,
                before,
                limit,
                thread,
            } => {
                let (space, channel) = self.target(&target).await?;
                let p = MessageListParams {
                    space,
                    channel,
                    before: before
                        .as_deref()
                        .map(|s| parse_id(s, "메시지 ID"))
                        .transpose()?,
                    limit,
                    thread: thread
                        .as_deref()
                        .map(|s| parse_id(s, "스레드 ID"))
                        .transpose()?,
                };
                let r = self.client.request::<m::MessageList>(p).await?;
                let names = if self.json {
                    HashMap::new()
                } else {
                    self.names_in(space).await
                };
                self.emit(&r, |r| output::messages(r, &names))
            }
            MessageCmd::Send {
                target,
                text,
                attachments,
                reply_to,
                thread,
            } => {
                let (space, channel) = self.target(&target).await?;
                let text = text_or_stdin(text)?;
                if text.trim().is_empty() && attachments.is_empty() {
                    bail!("보낼 내용이 없습니다");
                }
                let attachments = attachments
                    .iter()
                    .map(|p| {
                        Ok(AttachmentInput {
                            path: abs_path(p)?,
                            ..Default::default()
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let p = MessageSendParams {
                    space,
                    channel,
                    text,
                    attachments,
                    reply_to: reply_to
                        .as_deref()
                        .map(|s| parse_id(s, "메시지 ID"))
                        .transpose()?,
                    thread: thread
                        .as_deref()
                        .map(|s| parse_id(s, "스레드 ID"))
                        .transpose()?,
                };
                let r = self.client.request::<m::MessageSend>(p).await?;
                self.emit(&r, |r| format!("보냈습니다: {}\n", r.id))
            }
            MessageCmd::Edit { target, id, text } => {
                let (space, channel) = self.target(&target).await?;
                let id = parse_id(&id, "메시지 ID")?;
                let text = text_or_stdin(text)?;
                self.client
                    .request::<m::MessageEdit>(MessageEditParams {
                        space,
                        channel,
                        id,
                        text,
                    })
                    .await?;
                self.done("메시지를 고쳤습니다.")
            }
            MessageCmd::Delete { target, id } => {
                let (space, channel) = self.target(&target).await?;
                let id = parse_id(&id, "메시지 ID")?;
                self.client
                    .request::<m::MessageDelete>(MessageRefParams { space, channel, id })
                    .await?;
                self.done("메시지를 지웠습니다.")
            }
            MessageCmd::React { target, id, emoji } => {
                let (space, channel) = self.target(&target).await?;
                let id = parse_id(&id, "메시지 ID")?;
                self.client
                    .request::<m::MessageReact>(ReactParams {
                        space,
                        channel,
                        id,
                        emoji,
                    })
                    .await?;
                self.done("반응을 달았습니다.")
            }
            MessageCmd::Unreact { target, id, emoji } => {
                let (space, channel) = self.target(&target).await?;
                let id = parse_id(&id, "메시지 ID")?;
                self.client
                    .request::<m::MessageUnreact>(ReactParams {
                        space,
                        channel,
                        id,
                        emoji,
                    })
                    .await?;
                self.done("반응을 뺐습니다.")
            }
            MessageCmd::Report {
                target,
                id,
                reason,
                hide,
            } => {
                let (space, channel) = self.target(&target).await?;
                let message = parse_id(&id, "메시지 ID")?;
                if reason.trim().is_empty() {
                    bail!("신고 이유를 적어 주세요(--reason)");
                }
                let r = self
                    .client
                    .request::<m::MessageReport>(MessageReportParams {
                        space,
                        channel,
                        message,
                        reason,
                        hide,
                    })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "운영진에게 신고했습니다({}).{}\n",
                        r.report_id,
                        if hide {
                            " 이 기기에서 그 메시지를 숨겼습니다."
                        } else {
                            ""
                        }
                    )
                })
            }
            MessageCmd::Search {
                query,
                crew,
                channel,
                limit,
                page,
            } => {
                let space = self.opt_space(crew.as_deref()).await?;
                let channel = match (space, channel) {
                    (Some(s), Some(c)) => Some(self.channel(s, &c).await?),
                    _ => None,
                };
                let r = self
                    .client
                    .request::<m::MessageSearch>(MessageSearchParams {
                        space,
                        channel,
                        author: None,
                        query,
                        offset: Some((page - 1).saturating_mul(limit.unwrap_or(50))),
                        limit,
                    })
                    .await?;
                self.emit(&r, output::search_results)
            }
        }
    }

    async fn forum(&mut self, c: ForumCmd) -> Result<()> {
        match c {
            ForumCmd::Post {
                target,
                title,
                text,
                tags,
            } => {
                let (space, channel) = self.target(&target).await?;
                let text = text_or_stdin(text)?;
                let r = self
                    .client
                    .request::<m::ForumCreatePost>(ForumCreatePostParams {
                        space,
                        channel,
                        title,
                        text,
                        tags,
                        attachments: Vec::new(),
                        featured: false,
                    })
                    .await?;
                self.emit(&r, |r| format!("글을 올렸습니다: {}\n", r.id))
            }
            ForumCmd::List { target, sort, tag } => {
                let (space, channel) = self.target(&target).await?;
                let sort = match sort {
                    SortArg::Activity => ForumSort::Activity,
                    SortArg::Created => ForumSort::Created,
                };
                let r = self
                    .client
                    .request::<m::ForumListPosts>(ForumListPostsParams {
                        space,
                        channel,
                        sort,
                        order: Default::default(),
                        tag,
                        tags: Vec::new(),
                        tag_match: Default::default(),
                    })
                    .await?;
                self.emit(&r, |r| output::forum_posts(r))
            }
            ForumCmd::Update {
                target,
                post,
                title,
                tags,
                pin,
                lock,
            } => {
                if title.is_none() && tags.is_none() && pin.is_none() && lock.is_none() {
                    bail!("바꿀 항목을 하나 이상 지정하세요(--title, --tags, --pin, --lock)");
                }
                let (space, channel) = self.target(&target).await?;
                let post = parse_id(&post, "글 ID")?;
                self.client
                    .request::<m::ForumUpdatePost>(ForumUpdatePostParams {
                        space,
                        channel,
                        post,
                        title,
                        tags,
                        pinned: pin,
                        locked: lock,
                    })
                    .await?;
                self.done("글 설정을 바꿨습니다.")
            }
        }
    }

    async fn blob(&mut self, c: BlobCmd) -> Result<()> {
        match c {
            BlobCmd::Status { crew, blob } => {
                let space = self.space(&crew.crew).await?;
                let blob: BlobId = parse_id(&blob, "블롭 ID")?;
                let r = self
                    .client
                    .request::<m::BlobStatus>(BlobParams { space, blob })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "조각 {}/{}, 온라인 시더 {}개\n",
                        r.have_chunks, r.total_chunks, r.seeders_online
                    )
                })
            }
            BlobCmd::Fetch {
                crew,
                blob,
                priority,
            } => {
                let space = self.space(&crew.crew).await?;
                let blob: BlobId = parse_id(&blob, "블롭 ID")?;
                self.client
                    .request::<m::BlobFetch>(BlobFetchParams {
                        space,
                        blob,
                        priority,
                    })
                    .await?;
                self.done("다운로드를 시작했습니다. 진행은 `yangpa watch --topics blob`으로 볼 수 있습니다.")
            }
            BlobCmd::Save {
                crew,
                blob,
                key_from,
                path,
                allow_volatile,
            } => {
                let space = self.space(&crew.crew).await?;
                let blob: BlobId = parse_id(&blob, "블롭 ID")?;
                let key_from = parse_id(&key_from, "메시지 ID")?;
                let path_s = abs_path(&path)?;
                self.client
                    .request::<m::BlobSave>(BlobSaveParams {
                        space,
                        blob,
                        key_from,
                        path: path_s,
                        allow_volatile,
                    })
                    .await?;
                self.done(&format!("저장했습니다: {}", path.display()))
            }
            BlobCmd::Read {
                crew,
                blob,
                key_from,
                index,
                offset,
                len,
                out: path,
                allow_volatile,
            } => {
                if len.is_some_and(|l| l > BLOB_READ_MAX_LEN) {
                    bail!(
                        "한 번에 읽을 수 있는 크기는 {} 이하입니다",
                        format_size(BLOB_READ_MAX_LEN)
                    );
                }
                let space = self.space(&crew.crew).await?;
                let blob: BlobId = parse_id(&blob, "블롭 ID")?;
                let key_from = parse_id(&key_from, "메시지 ID")?;
                let read = self.client.blob_read(BlobReadParams {
                    space,
                    blob,
                    key_from,
                    index,
                    offset,
                    len,
                    binary: Some(true),
                });
                let data = if path.as_os_str() == "-" || allow_volatile {
                    read.await?
                } else {
                    // 상태는 데몬이 시더에게 물어 늦을 수 있어 읽기와 같이 보냅니다.
                    let status = self
                        .client
                        .request::<m::BlobStatus>(BlobParams { space, blob });
                    let (st, data) = tokio::try_join!(status, read)?;
                    if st.volatile {
                        bail!(
                            "휘발성 채널 첨부입니다. 저장한 파일은 메시지가 사라져도 남습니다. 확인했으면 --allow-volatile을 붙이세요"
                        );
                    }
                    data
                };
                if path.as_os_str() == "-" {
                    let mut so = std::io::stdout().lock();
                    return ignore_broken_pipe(so.write_all(&data).and_then(|_| so.flush()));
                }
                std::fs::write(&path, &data)
                    .with_context(|| format!("파일을 쓸 수 없습니다: {}", path.display()))?;
                if self.json {
                    return print_json(&json!({ "path": path, "bytes": data.len() }));
                }
                out(&format!(
                    "{}를 저장했습니다: {}\n",
                    format_size(data.len() as u64),
                    path.display()
                ))
            }
            BlobCmd::Pin { crew, blob } => {
                let space = self.space(&crew.crew).await?;
                let blob: BlobId = parse_id(&blob, "블롭 ID")?;
                self.client
                    .request::<m::BlobPin>(BlobParams { space, blob })
                    .await?;
                self.done("고정했습니다. 저장 공간을 정리해도 지우지 않습니다.")
            }
            BlobCmd::Unpin { crew, blob } => {
                let space = self.space(&crew.crew).await?;
                let blob: BlobId = parse_id(&blob, "블롭 ID")?;
                self.client
                    .request::<m::BlobUnpin>(BlobParams { space, blob })
                    .await?;
                self.done("고정을 풀었습니다.")
            }
            BlobCmd::Pins { crew } => {
                let space = self.opt_space(crew.crew.as_deref()).await?;
                let r = self
                    .client
                    .request::<m::BlobPins>(OptSpaceParams { space })
                    .await?;
                let names = self.space_names().await;
                self.emit(&r, |r| output::blob_pins(r, &|id| name_or_id(&names, id)))
            }
            BlobCmd::Cache(c) => self.blob_cache(c).await,
        }
    }

    async fn blob_cache(&mut self, c: CacheCmd) -> Result<()> {
        match c {
            CacheCmd::Status => {
                let r = self.client.request::<m::BlobCacheStatus>(Empty).await?;
                self.emit(&r, output::blob_cache)
            }
            CacheCmd::Limit {
                limit,
                min_free,
                reset,
            } => {
                if limit.is_none() && min_free.is_none() && !reset {
                    bail!("한도(예: 10GB), --min-free, --reset 중 하나를 지정하세요");
                }
                let r = self
                    .client
                    .request::<m::BlobCacheSetLimit>(BlobCacheLimitParams {
                        limit_bytes: limit,
                        min_free_bytes: min_free,
                        reset,
                    })
                    .await?;
                self.emit(&r, output::blob_cache)
            }
            CacheCmd::Clear => {
                let r = self.client.request::<m::BlobCacheClear>(Empty).await?;
                self.emit(&r, |r| {
                    format!(
                        "받은 첨부 {}개({})를 비웠습니다. 필요하면 다시 받습니다.\n",
                        r.evicted_count,
                        format_size(r.freed_bytes)
                    )
                })
            }
        }
    }

    async fn emoji(&mut self, c: EmojiCmd) -> Result<()> {
        match c {
            EmojiCmd::List { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::EmojiList>(SpaceParams { space })
                    .await?;
                self.emit(&r, |r| output::emojis(r))
            }
            EmojiCmd::Create { crew, name, path } => {
                let space = self.space(&crew.crew).await?;
                let path = abs_path(&path)?;
                let r = self
                    .client
                    .request::<m::EmojiCreate>(EmojiCreateParams {
                        space,
                        name: name.clone(),
                        path,
                    })
                    .await?;
                self.emit(&r, |r| {
                    format!("이모지를 추가했습니다: <:{name}:{}>\n", r.id)
                })
            }
            EmojiCmd::Delete { crew, emoji } => {
                let space = self.space(&crew.crew).await?;
                let id = match EmojiId::from_str(emoji.trim()) {
                    Ok(id) => id,
                    Err(_) => {
                        let list = self
                            .client
                            .request::<m::EmojiList>(SpaceParams { space })
                            .await?;
                        let items: Vec<Candidate<EmojiId>> = list
                            .iter()
                            .map(|e| Candidate::new(e.id, e.name.clone()))
                            .collect();
                        let name = emoji.trim().trim_matches(':');
                        resolve(name, "이모지", &items).map_err(|e| anyhow!(e))?
                    }
                };
                self.client
                    .request::<m::EmojiDelete>(EmojiDeleteParams { space, id })
                    .await?;
                self.done("이모지를 지웠습니다.")
            }
        }
    }

    async fn role(&mut self, c: RoleCmd) -> Result<()> {
        match c {
            RoleCmd::List { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self.role_list(space).await?;
                self.emit(&r, |r| output::roles(r))
            }
            RoleCmd::Create {
                crew,
                name,
                perms,
                color,
                rank,
            } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::RoleCreate>(RoleCreateParams {
                        space,
                        name,
                        color,
                        permissions: perms,
                        rank,
                    })
                    .await?;
                self.emit(&r, |r| format!("역할을 만들었습니다: {}\n", r.role))
            }
            RoleCmd::Update {
                crew,
                role,
                name,
                perms,
                color,
                rank,
            } => {
                if name.is_none() && perms.is_none() && color.is_none() && rank.is_none() {
                    bail!("바꿀 항목을 하나 이상 지정하세요(--name, --perms, --color, --rank)");
                }
                let space = self.space(&crew.crew).await?;
                let role = self.role_id(space, &role).await?;
                self.client
                    .request::<m::RoleUpdate>(RoleUpdateParams {
                        space,
                        role,
                        name,
                        color,
                        permissions: perms,
                        rank,
                        visible_to: None,
                        see_admins: None,
                    })
                    .await?;
                self.done("역할을 바꿨습니다.")
            }
            RoleCmd::Delete { crew, role } => {
                let space = self.space(&crew.crew).await?;
                let role = self.role_id(space, &role).await?;
                if role == RoleId::EVERYONE {
                    bail!("@everyone 역할은 지울 수 없습니다");
                }
                self.client
                    .request::<m::RoleDelete>(RoleParams { space, role })
                    .await?;
                self.done("역할을 지웠습니다.")
            }
        }
    }

    async fn invite(&mut self, c: InviteCmd) -> Result<()> {
        match c {
            InviteCmd::Create {
                crew,
                expires,
                uses,
                roles,
            } => {
                let space = self.space(&crew.crew).await?;
                let roles = self.role_ids(space, &roles).await?;
                let r = self
                    .client
                    .request::<m::InviteCreate>(InviteCreateParams {
                        space,
                        expires_in_secs: expires.and_then(|e| e.0),
                        max_uses: uses,
                        roles,
                    })
                    .await?;
                self.emit(&r, |r| format!("{}\n", r.url))
            }
            InviteCmd::List { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::InviteList>(SpaceParams { space })
                    .await?;
                let roles = if self.json {
                    Vec::new()
                } else {
                    self.role_list(space).await.unwrap_or_default()
                };
                self.emit(&r, |r| output::invites(r, &roles))
            }
            InviteCmd::Revoke { crew, token_hash } => {
                let space = self.space(&crew.crew).await?;
                let token_hash = match TokenHash::from_str(&token_hash) {
                    Ok(h) => h,
                    Err(_) => {
                        // 목록에 보이는 해시 앞부분만 적어도 되게 합니다(8자 이상, 하나로 정해질 때).
                        let prefix = token_hash.trim().to_ascii_lowercase();
                        if prefix.len() < 8 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
                            bail!("토큰 해시는 64자리 hex(또는 앞 8자 이상)여야 합니다");
                        }
                        let list = self
                            .client
                            .request::<m::InviteList>(SpaceParams { space })
                            .await?;
                        let found: Vec<TokenHash> = list
                            .iter()
                            .map(|i| i.token_hash)
                            .filter(|h| h.to_string().starts_with(&prefix))
                            .collect();
                        match found.as_slice() {
                            [one] => *one,
                            [] => bail!("'{prefix}'로 시작하는 초대가 없습니다"),
                            _ => bail!("'{prefix}'로 시작하는 초대가 여럿입니다. 더 길게 적으세요"),
                        }
                    }
                };
                self.client
                    .request::<m::InviteRevoke>(InviteRevokeParams { space, token_hash })
                    .await?;
                self.done("초대를 폐기했습니다.")
            }
            InviteCmd::Join { url } => {
                let r = self
                    .client
                    .request::<m::InviteJoin>(UrlParams { url })
                    .await?;
                self.emit(&r, |r| match r.status {
                    InviteJoinStatus::Joined => format!("가입했습니다: {}\n", r.space),
                    InviteJoinStatus::Pending => format!(
                        "가입 요청을 보냈습니다({}). 크루 기기가 온라인이 되면 처리됩니다.\n",
                        r.space
                    ),
                })
            }
        }
    }

    async fn seed(&mut self, c: SeedCmd) -> Result<()> {
        match c {
            SeedCmd::Show { crew } => {
                let space = self.opt_space(crew.crew.as_deref()).await?;
                let r = self
                    .client
                    .request::<m::SeedGet>(OptSpaceParams { space })
                    .await?;
                let names = self.channel_names(space).await;
                self.emit(&r, |r| output::seed_policy(r, &|id| name_or_id(&names, id)))
            }
            SeedCmd::Set(a) => {
                let space = self.opt_space(a.crew.crew.as_deref()).await?;
                let mut p = SeedSetParams {
                    space,
                    ..Default::default()
                };
                if a.enable {
                    p.enabled = Some(true);
                } else if a.disable {
                    p.enabled = Some(false);
                }
                p.types = a.types;
                if let Some(list) = &a.channels {
                    p.channels = Some(Some(self.channels_for(space, list).await?));
                    p.all_channels = Some(false);
                } else if a.all_channels {
                    p.channels = Some(None);
                    p.all_channels = Some(true);
                } else if a.opened_channels {
                    p.channels = Some(None);
                    p.all_channels = Some(false);
                }
                if let Some(list) = &a.exclude_channels {
                    p.exclude_channels = Some(self.channels_for(space, list).await?);
                }
                p.upload_limit_bps = a.upload_limit.map(|r| r.0);
                p.max_concurrent = a.max_concurrent;
                p.prefetch = a.prefetch;
                if p == (SeedSetParams {
                    space,
                    ..Default::default()
                }) {
                    bail!("바꿀 항목을 하나 이상 지정하세요");
                }
                self.client.request::<m::SeedSet>(p).await?;
                self.done("시더 설정을 바꿨습니다.")
            }
            SeedCmd::Exclude { target, id, undo } => {
                let (space, channel) = self.target(&target).await?;
                let id = parse_id(&id, "메시지 ID")?;
                self.client
                    .request::<m::SeedExcludeMessage>(SeedExcludeParams {
                        space,
                        channel,
                        id,
                        excluded: !undo,
                    })
                    .await?;
                self.done(if undo {
                    "시딩 제외를 취소했습니다."
                } else {
                    "이 메시지를 시딩에서 뺐습니다."
                })
            }
            SeedCmd::Coverage { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::SeedCoverage>(SpaceParams { space })
                    .await?;
                let names = self.channel_names(Some(space)).await;
                self.emit(&r, |r| output::coverage(r, &|id| name_or_id(&names, id)))
            }
        }
    }

    async fn archive(&mut self, c: ArchiveCmd) -> Result<()> {
        match c {
            ArchiveCmd::Offer { expires, name } => {
                let r = self
                    .client
                    .request::<m::ArchiveOffer>(ArchiveOfferParams {
                        expires_in_secs: expires.map(|d| d.as_secs()),
                        name,
                    })
                    .await?;
                self.emit(&r, |r| {
                    let until = r
                        .expires_at
                        .map(|t| format!("(만료 {})", output::format_time(t)))
                        .unwrap_or_default();
                    format!(
                        "{}\n이 링크를 크루 관리자에게만 따로 전하세요. 한 번 쓰면 끝납니다{until}.\n",
                        r.url
                    )
                })
            }
            ArchiveCmd::Hosted => {
                let r = self.client.request::<m::ArchiveHosted>(Empty).await?;
                self.emit(&r, |r| output::archive_hosted(r))
            }
            ArchiveCmd::Drop { space } => {
                let space = parse_id(&space, "공간 ID")?;
                self.client
                    .request::<m::ArchiveDrop>(SpaceParams { space })
                    .await?;
                self.done("받아 둔 크루 데이터를 지웠습니다.")
            }
            ArchiveCmd::Designate {
                crew,
                url,
                private_channels,
                name,
            } => {
                let space = self.space(&crew.crew).await?;
                let private_channels = self.channels(space, &private_channels).await?;
                let r = self
                    .client
                    .request::<m::ArchiveDesignate>(ArchiveDesignateParams {
                        space,
                        url,
                        private_channels,
                        name,
                    })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "보관 노드 '{}'({})를 지정했습니다.\n",
                        output::one_line(&r.name),
                        r.device
                    )
                })
            }
            ArchiveCmd::Revoke { crew, archive } => {
                let space = self.space(&crew.crew).await?;
                let list = self
                    .client
                    .request::<m::ArchiveList>(SpaceParams { space })
                    .await?;
                let items: Vec<Candidate<DeviceId>> = list
                    .iter()
                    .map(|a| Candidate::new(a.device, a.name.clone()))
                    .collect();
                let device = resolve(&archive, "보관 노드", &items).map_err(|e| anyhow!(e))?;
                self.client
                    .request::<m::ArchiveRevoke>(ArchiveRevokeParams { space, device })
                    .await?;
                self.done("보관 노드 지정을 해제했습니다.")
            }
            ArchiveCmd::List { crew } => {
                let space = self.space(&crew.crew).await?;
                let r = self
                    .client
                    .request::<m::ArchiveList>(SpaceParams { space })
                    .await?;
                let names = self.channel_names(Some(space)).await;
                self.emit(&r, |r| output::archives(r, &|id| name_or_id(&names, id)))
            }
        }
    }

    async fn channels_for(
        &self,
        space: Option<SpaceId>,
        list: &[String],
    ) -> Result<Vec<ChannelId>> {
        match space {
            Some(s) => self.channels(s, list).await,
            None => resolve_all(list, "채널", &[] as &[Candidate<ChannelId>]).map_err(|_| {
                anyhow!(
                    "채널 이름을 쓰려면 --crew를 지정하세요(전역 설정에는 채널 ID만 쓸 수 있습니다)"
                )
            }),
        }
    }

    async fn peer(&mut self, c: PeerCmd) -> Result<()> {
        match c {
            PeerCmd::Show { crew } => {
                let space = self.opt_space(crew.crew.as_deref()).await?;
                let r = self
                    .client
                    .request::<m::PeerGet>(OptSpaceParams { space })
                    .await?;
                self.emit(&r, output::peer_policy)
            }
            PeerCmd::Set {
                crew,
                types,
                priorities,
                download_limit,
                auto_download_max,
            } => {
                let space = self.opt_space(crew.crew.as_deref()).await?;
                let p = PeerSetParams {
                    space,
                    types,
                    priorities,
                    download_limit_bps: download_limit.map(|r| r.0),
                    auto_download_max_bytes: auto_download_max,
                };
                if p == (PeerSetParams {
                    space,
                    ..Default::default()
                }) {
                    bail!("바꿀 항목을 하나 이상 지정하세요");
                }
                self.client.request::<m::PeerSet>(p).await?;
                self.done("피어 설정을 바꿨습니다.")
            }
        }
    }

    async fn net(&mut self, c: NetCmd) -> Result<()> {
        match c {
            NetCmd::Status => {
                let r = self.client.request::<m::NetStatusGet>(Empty).await?;
                self.emit(&r, output::net_status)
            }
            NetCmd::Stun { action: None } => {
                let r = self.client.request::<m::NetStunServers>(Empty).await?;
                self.emit(&r, output::stun_servers)
            }
            NetCmd::Stun {
                action: Some(action),
            } => {
                let (servers, done) = match action {
                    StunCmd::Set { servers } => (Some(servers), "STUN 서버 목록을 바꿨습니다."),
                    StunCmd::Reset => (None, "STUN 서버 목록을 기본값으로 되돌렸습니다."),
                    StunCmd::Off => (Some(Vec::new()), "STUN을 쓰지 않습니다."),
                };
                self.client
                    .request::<m::NetSetStunServers>(StunServersParams { servers })
                    .await?;
                self.done(done)
            }
            NetCmd::Peers => {
                let r = self.client.request::<m::NetPeers>(Empty).await?;
                self.emit(&r, |r| output::peers(r))
            }
        }
    }

    async fn token(&mut self, c: TokenCmd) -> Result<()> {
        match c {
            TokenCmd::Create {
                name,
                scopes,
                crews,
                automation,
            } => {
                let mut spaces = Vec::new();
                for c in crews.iter().filter(|c| !c.trim().is_empty()) {
                    let s = self.space(c).await?;
                    if !spaces.contains(&s) {
                        spaces.push(s);
                    }
                }
                let r = self
                    .client
                    .request::<m::TokenCreate>(TokenCreateParams {
                        name,
                        scopes,
                        spaces,
                        automation: automation.then_some(true),
                    })
                    .await?;
                self.emit(&r, |r| {
                    format!(
                        "토큰을 발급했습니다(ID {}). 다시 보여 주지 않으니 안전하게 보관하세요.\n{}\n",
                        r.id,
                        r.token.expose()
                    )
                })
            }
            TokenCmd::List => {
                let r = self.client.request::<m::TokenList>(Empty).await?;
                let names = self.space_names().await;
                self.emit(&r, |r| output::tokens(r, &|id| name_or_id(&names, id)))
            }
            TokenCmd::Revoke { id } => {
                self.client
                    .request::<m::TokenRevoke>(IdParams { id })
                    .await?;
                self.done("토큰을 폐기했습니다.")
            }
        }
    }

    async fn audit(&mut self, c: AuditCmd) -> Result<()> {
        match c {
            AuditCmd::List { limit, before } => {
                let r = self
                    .client
                    .request::<m::AuditList>(AuditListParams { limit, before })
                    .await?;
                self.emit(&r, |r| output::audit(r))
            }
        }
    }

    async fn confirm(&mut self, c: ConfirmCmd) -> Result<()> {
        match c {
            ConfirmCmd::List => {
                let r = self.client.request::<m::ConfirmList>(Empty).await?;
                self.emit(&r, |r| output::confirmations(r))
            }
            ConfirmCmd::Approve { id, yes } => {
                let id = id.trim().to_string();
                // 무엇을 승인하는지 먼저 보여 줍니다. AI가 알려 준 ID만 믿고 승인하면, 프롬프트 인젝션으로
                // 다른 작업(추방 등)을 승인하게 될 수 있습니다.
                match self.client.request::<m::ConfirmList>(Empty).await {
                    Ok(list) => {
                        let Some(item) = list.iter().find(|c| c.id == id) else {
                            bail!(
                                "확인 대기 목록에 '{id}'가 없습니다(이미 처리됐거나 만료됐을 수 있음). `yangpa confirm list`로 확인하세요"
                            );
                        };
                        eprint!("승인할 작업:\n{}", output::confirmation(item));
                    }
                    // 목록을 볼 수 없는 토큰이면 --yes일 때만 그대로 진행합니다.
                    Err(e) if !yes => {
                        return Err(anyhow::Error::new(e).context(
                            "승인할 작업 내용을 확인할 수 없습니다(--yes로 건너뛸 수 있음)",
                        ));
                    }
                    Err(_) => {}
                }
                match approval_gate(yes, std::io::stdin().is_terminal()) {
                    ApprovalGate::Proceed => {}
                    ApprovalGate::Ask => {
                        if !ask_yes_no("이 작업을 승인하고 실행할까요? [y/N] ")? {
                            bail!("승인하지 않았습니다");
                        }
                    }
                    ApprovalGate::NeedYes => bail!(
                        "표준 입력이 터미널이 아니라 물어볼 수 없습니다. 위 내용을 확인했다면 --yes를 붙이세요"
                    ),
                }
                let r = self
                    .client
                    .request::<m::ConfirmApprove>(IdParams { id })
                    .await?;
                self.emit(&r, |r| {
                    let detail = match &r.result {
                        Value::Object(o) if o.is_empty() => String::new(),
                        Value::Null => String::new(),
                        v => format!(
                            "결과: {}\n",
                            serde_json::to_string_pretty(v).unwrap_or_default()
                        ),
                    };
                    format!("승인했고 실행했습니다.\n{detail}")
                })
            }
            ConfirmCmd::Deny { id } => {
                self.client
                    .request::<m::ConfirmDeny>(IdParams { id })
                    .await?;
                self.done("거절했습니다.")
            }
        }
    }

    async fn diag(&mut self, c: DiagCmd) -> Result<()> {
        match c {
            DiagCmd::Metrics => {
                let r = self.client.request::<m::DiagMetricsGet>(Empty).await?;
                self.emit(&r, output::metrics)
            }
            DiagCmd::Leakcheck { marker } => {
                let r = self
                    .client
                    .request::<m::DiagLeakcheck>(LeakcheckParams { marker })
                    .await?;
                self.emit(&r, |r| {
                    if r.clean {
                        "깨끗합니다: 표식이 디스크·로그에서 발견되지 않았습니다.\n".into()
                    } else {
                        format!(
                            "발견됨({}곳):\n{}\n",
                            r.findings.len(),
                            r.findings.join("\n")
                        )
                    }
                })?;
                if !r.clean {
                    bail!("휘발성 표식이 디스크·로그에 남아 있습니다");
                }
                Ok(())
            }
        }
    }

    async fn call(&mut self, a: CallArgs) -> Result<()> {
        if method_spec(&a.method).is_none() {
            bail!("알 수 없는 메서드: {} (docs/api.md 참고)", a.method);
        }
        let params: Value = match a.params.as_deref() {
            None => json!({}),
            Some("-") => serde_json::from_str(&read_stdin_text()?)
                .context("표준 입력이 올바른 JSON이 아닙니다")?,
            Some(s) => serde_json::from_str(s).context("인자가 올바른 JSON이 아닙니다")?,
        };
        let r = self.client.call_value(&a.method, params).await?;
        print_json(&r)
    }
}

// ───────────── 도우미 ─────────────

/// `confirm approve`를 묻지 않고 진행할지.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalGate {
    Proceed,
    /// 터미널에서 y/N을 묻습니다.
    Ask,
    /// 물어볼 수 없으니 `--yes`가 필요합니다.
    NeedYes,
}

pub fn approval_gate(yes: bool, stdin_is_terminal: bool) -> ApprovalGate {
    match (yes, stdin_is_terminal) {
        (true, _) => ApprovalGate::Proceed,
        (false, true) => ApprovalGate::Ask,
        (false, false) => ApprovalGate::NeedYes,
    }
}

/// 표준 오류에 질문을 쓰고 표준 입력 한 줄로 답을 받습니다. 기본은 '아니오'.
fn ask_yes_no(question: &str) -> Result<bool> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .take(1024)
        .read_line(&mut line)
        .context("표준 입력을 읽을 수 없습니다")?;
    Ok(is_yes(&line))
}

fn is_yes(answer: &str) -> bool {
    matches!(
        answer.trim().to_lowercase().as_str(),
        "y" | "yes" | "예" | "네" | "응"
    )
}

/// 표준 출력에 씁니다. 파이프가 닫혔으면(`| head`) 조용히 끝냅니다.
pub fn out(s: &str) -> Result<()> {
    write_to(&mut std::io::stdout().lock(), s).map(|_| ())
}

/// 쓰고 나서 읽는 쪽이 아직 있는지 돌려줍니다. 파이프가 닫혔으면(`| head`) `false`.
fn write_to<W: Write>(w: &mut W, s: &str) -> Result<bool> {
    match w.write_all(s.as_bytes()).and_then(|_| w.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn ignore_broken_pipe(r: std::io::Result<()>) -> Result<()> {
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

pub fn print_json<T: Serialize + ?Sized>(v: &T) -> Result<()> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    out(&s)
}

fn name_or_id<K: std::hash::Hash + Eq + std::fmt::Display>(
    names: &HashMap<K, String>,
    id: K,
) -> String {
    names.get(&id).cloned().unwrap_or_else(|| id.to_string())
}

fn report_status(s: ReportStatusArg) -> ReportStatus {
    match s {
        ReportStatusArg::Open => ReportStatus::Open,
        ReportStatusArg::Resolved => ReportStatus::Resolved,
    }
}

fn report_action(a: ReportActionArg) -> ReportAction {
    match a {
        ReportActionArg::Dismiss => ReportAction::Dismiss,
        ReportActionArg::DeleteMessage => ReportAction::DeleteMessage,
        ReportActionArg::Kick => ReportAction::Kick,
        ReportActionArg::Ban => ReportAction::Ban,
    }
}

fn parse_id<T: FromStr>(s: &str, what: &str) -> Result<T> {
    T::from_str(s.trim()).map_err(|_| anyhow!("{what} 형식이 잘못됐습니다: {s}"))
}

fn channel_kind(t: ChannelType) -> ChannelKind {
    match t {
        ChannelType::Text => ChannelKind::Text,
        ChannelType::Voice => ChannelKind::Voice,
        ChannelType::Forum => ChannelKind::Forum,
        ChannelType::Category => ChannelKind::Category,
    }
}

fn history(h: HistoryArg) -> HistoryVisibility {
    match h {
        HistoryArg::Public => HistoryVisibility::Public,
        HistoryArg::Private => HistoryVisibility::Private,
    }
}

/// 데몬은 다른 작업 디렉터리에서 돌므로 파일 경로는 절대 경로로 바꿔 보냅니다.
fn abs_path(p: &Path) -> Result<String> {
    let abs: PathBuf = std::path::absolute(p)
        .with_context(|| format!("경로를 해석할 수 없습니다: {}", p.display()))?;
    abs.into_os_string()
        .into_string()
        .map_err(|_| anyhow!("경로가 UTF-8이 아닙니다: {}", p.display()))
}

/// `-`이면 표준 입력 전체를 읽습니다.
fn text_or_stdin(text: String) -> Result<String> {
    if text == "-" {
        read_stdin_text()
    } else {
        Ok(text)
    }
}

fn read_stdin_text() -> Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .lock()
        .take(MAX_STDIN + 1)
        .read_to_string(&mut s)
        .context("표준 입력을 읽을 수 없습니다")?;
    if s.len() as u64 > MAX_STDIN {
        bail!("표준 입력이 너무 깁니다(최대 {})", format_size(MAX_STDIN));
    }
    Ok(s.trim_end_matches(['\n', '\r']).to_string())
}

/// 비밀값 한 줄(기기 연결 링크)을 표준 입력에서 읽습니다. 셸 기록·프로세스 목록에 남지 않게 인자로 받지 않습니다.
fn read_secret_line(what: &str) -> Result<SecretString> {
    let mut echo = None;
    if std::io::stdin().is_terminal() {
        let off = EchoOff::new();
        if off.active {
            eprint!("{what}을(를) 입력하세요(입력 내용은 화면에 보이지 않습니다): ");
        } else {
            eprint!("{what}을(를) 입력하세요(화면에 보이니 주변을 확인하세요): ");
        }
        let _ = std::io::stderr().flush();
        echo = Some(off);
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().take(64 * 1024).read_line(&mut line);
    drop(echo);
    read.context("표준 입력을 읽을 수 없습니다")?;
    let secret = SecretString::new(line.trim());
    line.zeroize();
    if secret.is_empty() {
        bail!("{what}이(가) 비었습니다");
    }
    Ok(secret)
}

/// 터미널 입력 표시를 잠시 끕니다(Unix의 `stty -echo`). 비밀값이 화면·녹화·어깨 너머로 보이지 않게 합니다.
/// 새 의존성 없이 하려고 `stty`를 쓰며, 실패하면(`active == false`) 보이는 채로 받습니다. 버릴 때 되돌립니다.
struct EchoOff {
    active: bool,
}

impl EchoOff {
    fn new() -> Self {
        #[cfg(unix)]
        if std::io::stdin().is_terminal() {
            return EchoOff {
                active: stty("-echo"),
            };
        }
        EchoOff { active: false }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if self.active {
            #[cfg(unix)]
            stty("echo");
            // 사용자가 친 Enter가 화면에 나오지 않았으므로 줄을 바꿔 줍니다.
            eprintln!();
        }
    }
}

#[cfg(unix)]
fn stty(arg: &str) -> bool {
    use std::process::{Command, Stdio};
    Command::new("stty")
        .arg(arg)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 안전 번호를 5자리씩 끊어 읽기 쉽게.
fn group_digits(d: &str) -> String {
    let chars: Vec<char> = d.chars().filter(|c| !c.is_whitespace()).collect();
    chars
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `watch --json`에서 휘발성 메시지 내용을 지웁니다.
fn hide_volatile(n: Notification) -> Notification {
    match n {
        Notification::MessageNew {
            space,
            channel,
            message,
            notify,
        } if message.volatile => Notification::MessageNew {
            space,
            channel,
            message: message.redacted(),
            notify: NotifyView {
                title: notify.title,
                body: None,
                sender: None,
            },
        },
        Notification::MessageUpdated {
            space,
            channel,
            message,
        } if message.volatile => Notification::MessageUpdated {
            space,
            channel,
            message: message.redacted(),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn closed_pipe_is_reported_not_swallowed() {
        // watch는 이 결과로 끝낼지 정합니다(예전에는 닫힌 파이프에 영원히 썼음).
        assert!(!write_to(&mut ClosedPipe, "x").unwrap());
        let mut v = Vec::new();
        assert!(write_to(&mut v, "x").unwrap());
        assert_eq!(v, b"x");
    }

    #[test]
    fn approval_needs_a_human_decision() {
        assert_eq!(approval_gate(true, false), ApprovalGate::Proceed);
        assert_eq!(approval_gate(true, true), ApprovalGate::Proceed);
        assert_eq!(approval_gate(false, true), ApprovalGate::Ask);
        assert_eq!(approval_gate(false, false), ApprovalGate::NeedYes);
        assert!(is_yes("y\n") && is_yes(" YES ") && is_yes("예"));
        assert!(!is_yes("") && !is_yes("\n") && !is_yes("n") && !is_yes("yolo"));
    }

    #[test]
    fn digits_grouping() {
        assert_eq!(group_digits("123451234512"), "12345 12345 12");
        assert_eq!(group_digits("12345 67890"), "12345 67890");
    }

    #[test]
    fn ids_and_paths() {
        let id = EventId([7; 32]);
        assert_eq!(
            parse_id::<EventId>(&format!(" {id} "), "메시지 ID").unwrap(),
            id
        );
        assert!(parse_id::<EventId>("not-an-id", "메시지 ID").is_err());
        let p = abs_path(Path::new("a/b.png")).unwrap();
        assert!(Path::new(&p).is_absolute() && p.ends_with("b.png"));
    }

    #[test]
    fn volatile_hidden_in_json_watch() {
        use yangpa_core::time::Timestamp;
        let m = MessageView {
            id: EventId([1; 32]),
            author: UserId([2; 32]),
            author_device: DeviceId([3; 32]),
            created_at: Timestamp(1),
            lamport: 1,
            text: "비밀".into(),
            attachments: vec![],
            embeds: vec![],
            reply_to: None,
            reactions: vec![],
            edited_at: None,
            deleted: false,
            volatile: true,
            expires_at: None,
            thread: None,
            space: None,
            channel: None,
            author_blocked: false,
            anonymous: false,
            anon_name: None,
            mine: false,
            mentions_me: false,
        };
        let n = Notification::MessageNew {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            message: m,
            notify: NotifyView {
                title: "t".into(),
                body: Some("비밀".into()),
                sender: None,
            },
        };
        let s = serde_json::to_string(&hide_volatile(n)).unwrap();
        assert!(!s.contains("비밀"), "{s}");
    }
}
