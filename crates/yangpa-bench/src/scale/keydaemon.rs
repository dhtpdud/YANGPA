//! 실제 데몬으로 키 그룹 재기(`yangpa-bench scale --keytree-daemon 3000`).
//!
//! [`super::keyreal`]은 확정 기기 키 트리 코드만 떼어 돌리고, 이것은 데몬 노드를 띄워 데몬 쪽 경로를 그대로 탑니다: 확정
//! 기기 바퀴(`keygroup::committer`: 묶음 창, 크루 갱신 조각·조각 블롭·받은편지함 묶음 발행), 멤버 적용(`keygroup::member`),
//! 보내는 쪽 규칙(메시지 키 묶음 없음), 가입(초대 링크·가입 묶음), 옛 상태 키 묻기(`GetStateKeys`).
//!
//! - 노드: 확정 기기 A(소유자), 실제 멤버 B, 재는 때마다 새로 드는 실제 가입자. 셋 다 로컬 TCP로 잇습니다(Tor 없음).
//! - 크루 크기는 지어낸 멤버(`testkit::synthetic_members`, 상자 키는 아무도 열 수 없는 값)로 채웁니다. 사용자 기기는 2대씩,
//!   밴 물결 사용자는 50대씩, 탈퇴·기록 나이용 사용자는 1대씩입니다.
//! - 바이트는 A가 저장한 이벤트의 전송 인코딩(postcard `SignedEvent`) 크기입니다. "기기당 받기"는 크루 기기 하나가 그
//!   작업으로 받는 공간 스트림 키 갱신 전부입니다(트리 전체 재키는 머리 이벤트 + 조각 블롭의 자기 사용자 조각).
//! - 불변식(작업마다): 빚 없음·전체 재키 요구 없음, 크루 그룹 멤버 = 활성 기기, 뺀 기기는 멤버가 아님, 실제 멤버 B·가입자의
//!   크루 키 = 확정 기기 표의 크루 키(순번·키 지문). 어긋나면 `violations`에 적습니다.
//! - 기록 나이: 하루에 가입 1명·추방 1명(각 기기 1대), 7일마다 전체 재키로 `days`일을 돌리고, 처음·30·90·180일·끝에 새 실제
//!   가입자의 가입 묶음 크기를 잽니다(묶음은 최근 상태 키 64개까지라 나이와 상관없어야 함).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use yangpa_core::event::{EventKind, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::links::InviteLink;
use yangpa_daemon::{CallCtx, Node, NodeConfig, testkit};
use yangpa_rpc::api::{
    CrewCreateParams, InviteCreateParams, InviteJoinStatus, MemberBanParams, MemberKickParams,
    MessageSendParams, Method, SpaceParams, UrlParams, m,
};

/// 밴 물결 사용자 하나의 기기 수.
const BAN_DEVICES_EACH: usize = 50;
/// 한 단계를 기다리는 상한(초, `DaemonOpts::step_secs`로 바꿈).
static STEP_SECS: AtomicU64 = AtomicU64::new(600);
const POLL: Duration = Duration::from_millis(20);
/// 한 단계가 이보다 오래 걸리면 무엇을 기다리는지 한 번 알립니다.
const SLOW: Duration = Duration::from_secs(15);

/// 단계 진행을 표준 오류로 알립니다(오래 도는 재기에서 어디서 멈췄는지 보이게, 키·내용은 없음).
fn progress(what: &str) {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let t = T0.get_or_init(Instant::now).elapsed().as_secs_f64();
    eprintln!("[keytree-daemon {t:7.1}초] {what}");
}

/// 옵션.
#[derive(Clone, Debug)]
pub struct DaemonOpts {
    /// 크루 기기 수(실제 A·B 포함).
    pub devices: usize,
    /// 밴 물결 기기 수(50대씩 사용자 단위, 0이면 건너뜀).
    pub ban: usize,
    /// 직접 인코딩 크루에서 탈퇴를 잴 기기 수(0이면 건너뜀).
    pub direct: usize,
    /// 기록 나이(일).
    pub days: usize,
    /// 평상시 메시지 수(A·B가 반씩).
    pub messages: usize,
    /// 한 단계를 기다리는 상한(초).
    pub step_secs: u64,
}

/// 한 줄.
#[derive(Clone, Debug, Serialize)]
pub struct DaemonRow {
    pub op: String,
    pub bytes: f64,
    pub target: Option<f64>,
    /// 확정 기기 벽시계(요청부터 사슬 끝이 나아갈 때까지, 묶음 창 포함).
    pub secs: Option<f64>,
    pub detail: String,
}

impl DaemonRow {
    pub fn meets(&self) -> Option<bool> {
        self.target.map(|t| self.bytes <= t)
    }
}

/// 결과.
#[derive(Clone, Debug, Serialize)]
pub struct DaemonRun {
    pub devices: usize,
    pub users: usize,
    pub depth: u8,
    pub setup_secs: f64,
    pub rows: Vec<DaemonRow>,
    /// 평상시 채널 스트림·공간 스트림의 키 묶음(`KeyShare`) 수.
    pub steady_key_shares: usize,
    /// 기록 나이별 새 가입자 묶음 바이트(일, 바이트).
    pub joiner_by_age: Vec<(usize, f64)>,
    /// 마지막 가입자가 옛 상태 이벤트를 다 연 뒤 가진 크루 상태 키 수(가입 묶음 64개까지 + 멤버에게 물어 받은 것).
    pub fetched_state_keys: usize,
    pub checks: usize,
    pub violations: Vec<String>,
}

fn wire_len(ev: &SignedEvent) -> usize {
    postcard::to_stdvec(ev).map_or(0, |v| v.len())
}

async fn call<M: Method>(node: &Arc<Node>, p: M::Params) -> Result<M::Result> {
    let v = serde_json::to_value(&p)?;
    let out = node
        .call(&CallCtx::owner(), M::NAME, v)
        .await
        .map_err(|e| anyhow!("{}: {e:?}", M::NAME))?;
    Ok(serde_json::from_value(out)?)
}

async fn until<T, F, Fut>(what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let started = Instant::now();
    let deadline = started + Duration::from_secs(STEP_SECS.load(Ordering::Relaxed));
    let mut told = false;
    loop {
        if let Some(v) = f().await {
            return Ok(v);
        }
        if Instant::now() > deadline {
            bail!("시간 초과: {what}");
        }
        if !told && started.elapsed() > SLOW {
            told = true;
            progress(&format!("오래 기다림: {what}"));
        }
        tokio::time::sleep(POLL).await;
    }
}

struct Net {
    nodes: Vec<Arc<Node>>,
}

impl Net {
    async fn start(&mut self, dir: &Path, name: &str) -> Result<Arc<Node>> {
        let cfg = NodeConfig::for_test(dir.join(name));
        let node = Node::start(cfg).await?;
        node.call(
            &CallCtx::owner(),
            "identity.create",
            serde_json::json!({ "name": name }),
        )
        .await
        .map_err(|e| anyhow!("신원 만들기: {e:?}"))?;
        self.nodes.push(node.clone());
        Ok(node)
    }

    async fn shutdown(self) {
        for n in self.nodes {
            n.shutdown().await;
        }
    }
}

async fn connect_both(a: &Arc<Node>, b: &Arc<Node>) -> Result<()> {
    for (x, y) in [(a, b), (b, a)] {
        let addrs = until("수신 주소", || async {
            let v = y.listen_addrs().await;
            (!v.is_empty()).then_some(v)
        })
        .await?;
        x.connect(y.peer_id(), addrs).await?;
    }
    Ok(())
}

/// 사슬 끝이 다 맞춰질 때까지(빚 없음, 멤버 = 활성 기기). 맞춘 사슬 끝 순번.
async fn settled(a: &Arc<Node>, space: &SpaceId, after: u64) -> Result<u64> {
    let last = std::sync::Mutex::new(String::new());
    let r = until("크루 사슬 끝 맞춤", || async {
        let t = testkit::crew_tip(a, space)?;
        let members: BTreeSet<_> = t.members.iter().collect();
        let active: BTreeSet<_> = t.active.iter().collect();
        *last.lock().unwrap_or_else(|e| e.into_inner()) = format!(
            "순번 {} (기다리는 순번 > {after}), 빚 {}, 전체 재키 요구 {}, 멤버 {}대·활성 {}대",
            t.seq,
            t.debt,
            t.rekey_required,
            members.len(),
            active.len()
        );
        (t.seq > after && t.debt == 0 && !t.rekey_required && members == active).then_some(t.seq)
    })
    .await;
    r.with_context(|| last.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// `node`가 크루 사슬 끝 `seq`의 키를 얻을 때까지.
async fn caught(node: &Arc<Node>, space: &SpaceId, seq: u64) -> Result<()> {
    until("멤버가 크루 키를 얻음", || async {
        (testkit::crew_key_seq(node, space)? >= seq).then_some(())
    })
    .await
}

/// 실제 가입 하나의 결과.
struct Joined {
    node: Arc<Node>,
    /// 가입자 받은편지함 묶음 바이트.
    inbox: usize,
    /// 가입 전진 갱신 바이트(크루 기기 하나가 받음).
    advance: usize,
    /// 확정 기기 벽시계(초대 응답부터 사슬 끝이 나아갈 때까지).
    secs: f64,
    note: String,
}

struct Bench {
    a: Arc<Node>,
    b: Arc<Node>,
    space: SpaceId,
    general: ChannelId,
    token: yangpa_core::ids::LinkToken,
    /// 공간 스트림에서 이미 센 논리 시계.
    mark: u64,
    /// 지어낸 가입을 실제 멤버 B에도 넣는지(B가 든 크루일 때만).
    mirror_b: bool,
    checks: usize,
    violations: Vec<String>,
}

impl Bench {
    /// 지금까지 센 뒤로 공간 스트림에 새로 생긴 이벤트(이 종류만).
    fn new_space_events(&mut self, kind: EventKind) -> Result<Vec<SignedEvent>> {
        let evs = testkit::stream_events_since(
            &self.a,
            &self.space,
            StreamId::Space,
            self.mark,
            1 << 20,
        )?;
        if let Some(m) = evs.iter().map(|e| e.header.lamport).max() {
            self.mark = m + 1;
        }
        Ok(evs.into_iter().filter(|e| e.header.kind == kind).collect())
    }

    /// 이번 단계에 확정 기기가 가진 공간 이벤트(종류·짧은 ID)와 `n`에 있는지(+/-). 멈춘 곳을 찾을 때만 씁니다.
    fn presence(&self, n: &Node) -> String {
        let evs =
            testkit::stream_events_since(&self.a, &self.space, StreamId::Space, self.mark, 64)
                .unwrap_or_default();
        evs.iter()
            .map(|e| {
                let id = e.id();
                let have = testkit::get_event(n, &id).ok().flatten().is_some();
                format!(
                    "{:?}:{}{}",
                    e.header.kind,
                    id.short(),
                    if have { "+" } else { "-" }
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn skip_to_now(&mut self) -> Result<()> {
        self.new_space_events(EventKind::KeyUpdate)?;
        Ok(())
    }

    fn check(&mut self, what: &str, ok: bool) {
        self.checks += 1;
        if !ok {
            self.violations.push(what.to_string());
        }
    }

    /// 작업 뒤 불변식. `gone`은 이번에 뺀 기기, `members`는 크루 키가 확정 기기 표와 같아야 할 실제 노드.
    async fn invariants(
        &mut self,
        op: &str,
        gone: &[DeviceId],
        members: &[Arc<Node>],
    ) -> Result<()> {
        let Some(t) = testkit::crew_tip(&self.a, &self.space) else {
            self.check(&format!("{op}: 크루 사슬 없음"), false);
            return Ok(());
        };
        self.check(
            &format!("{op}: 빚·전체 재키 요구가 남음"),
            t.debt == 0 && !t.rekey_required,
        );
        let mem: BTreeSet<_> = t.members.iter().copied().collect();
        let act: BTreeSet<_> = t.active.iter().copied().collect();
        self.check(&format!("{op}: 크루 그룹 멤버 ≠ 활성 기기"), mem == act);
        self.check(
            &format!("{op}: 뺀 기기가 크루 그룹에 남음"),
            gone.iter().all(|d| !mem.contains(d)),
        );
        let crew = yangpa_core::ids::KeyGroupId::crew(&self.space);
        for (i, n) in members.iter().enumerate() {
            // 확정 기기가 곧바로 한 바퀴 더 돌 수 있어(그룹 다시 넣기 등) 멤버와 표가 같은 순번일 때 키를 견줍니다.
            let started = Instant::now();
            let deadline = started + Duration::from_secs(STEP_SECS.load(Ordering::Relaxed));
            let mut told = false;
            loop {
                let snap =
                    testkit::key_table_snapshot(&self.a, &self.space).context("확정 기기 표")?;
                let table =
                    testkit::key_table_digests(&self.a, &self.space, &snap).context("표 지문")?;
                let want = table.get(&crew).copied();
                let got = testkit::member_key_digest(n, &self.space, None).map(|(_, s, d)| (s, d));
                let same_seq = got.is_some() && got.map(|g| g.0) == want.map(|w| w.0);
                if !same_seq && !told && started.elapsed() > SLOW {
                    told = true;
                    progress(&format!(
                        "{op}: 멤버 {i}를 오래 기다림(멤버 키 순번 {:?}, 멤버가 본 사슬 끝 {:?}, 키를 기다리는 이벤트 {}개, 표 순번 {:?}, 이번 단계 공간 이벤트 {})",
                        got.map(|g| g.0),
                        testkit::crew_tip(n, &self.space).map(|t| t.seq),
                        testkit::waiting_count(n, &self.space),
                        want.map(|w| w.0),
                        self.presence(n),
                    ));
                }
                if same_seq || Instant::now() > deadline {
                    self.check(
                        &format!(
                            "{op}: 멤버 {i}의 크루 키 ≠ 확정 기기 표(멤버 순번 {:?}, 표 순번 {:?}, 사슬 끝 {})",
                            got.map(|g| g.0),
                            want.map(|w| w.0),
                            t.seq
                        ),
                        got.is_some() && got == want,
                    );
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        }
        Ok(())
    }

    /// 크루 키 갱신 하나(여러 조각일 수 있음)를 기다려 그 이벤트들의 바이트·조각 수·벽시계를 돌려줍니다.
    async fn crew_update(&mut self, before: u64, started: Instant) -> Result<(usize, usize, f64)> {
        let seq = settled(&self.a, &self.space, before).await?;
        let secs = started.elapsed().as_secs_f64();
        let evs = self.new_space_events(EventKind::KeyUpdate)?;
        let bytes: usize = evs.iter().map(wire_len).sum();
        let _ = seq;
        Ok((bytes, evs.len(), secs))
    }

    async fn synthetic(&mut self, users: usize, each: usize) -> Result<Vec<UserId>> {
        let before: BTreeSet<UserId> = testkit::space_state(&self.a, &self.space)
            .context("상태")?
            .members
            .keys()
            .copied()
            .collect();
        let mut added = Vec::new();
        // 한 번에 너무 많이 서명·적용하지 않게 나눕니다(가입 기록 하나에 혼합 서명 셋).
        let mut left = users;
        while left > 0 {
            let n = left.min(200);
            let evs = testkit::synthetic_members(&self.a, self.space, &self.token, n, each).await?;
            if self.mirror_b {
                testkit::ingest_from(&self.b, self.space, evs, self.a.peer_id()).await?;
            }
            left -= n;
        }
        let after = testkit::space_state(&self.a, &self.space).context("상태")?;
        for u in after.members.keys() {
            if !before.contains(u) {
                added.push(*u);
            }
        }
        Ok(added)
    }

    fn devices_of(&self, user: &UserId) -> Vec<DeviceId> {
        testkit::space_state(&self.a, &self.space)
            .and_then(|s| {
                s.devices
                    .get(user)
                    .map(|l| l.active().map(|d| d.device).collect())
            })
            .unwrap_or_default()
    }

    /// 새 실제 가입자: 가입 묶음(받은편지함) 바이트와 가입 전진 갱신 바이트.
    async fn real_join(&mut self, net: &mut Net, dir: &Path, name: &str) -> Result<Joined> {
        let c = net.start(dir, name).await?;
        connect_both(&self.a, &c).await?;
        let before = testkit::crew_tip(&self.a, &self.space)
            .map(|t| t.seq)
            .unwrap_or(0);
        self.skip_to_now()?;
        let url = call::<m::InviteCreate>(
            &self.a,
            InviteCreateParams {
                space: self.space,
                expires_in_secs: None,
                max_uses: Some(1),
                roles: Vec::new(),
            },
        )
        .await?
        .url;
        let started = Instant::now();
        let r = call::<m::InviteJoin>(&c, UrlParams { url }).await?;
        // 큰 크루는 가입 응답에 공간 스트림이 다 실리지 않아(전송 한도) `pending`으로 답하고 따라잡기로 마저 받습니다.
        let answered = started.elapsed().as_secs_f64();
        let (advance, _, secs) = self.crew_update(before, started).await?;
        let me = testkit::user_id(&c).context("가입자 사용자")?;
        caught(&c, &self.space, before + 1).await?;
        let ready = started.elapsed().as_secs_f64();
        let inbox: usize =
            testkit::stream_events_since(&self.a, &self.space, StreamId::Inbox(me), 0, 1 << 16)?
                .iter()
                .map(wire_len)
                .sum();
        Ok(Joined {
            node: c,
            inbox,
            advance,
            secs,
            note: format!(
                "응답 {:?} {answered:.1}초, 가입자가 크루 키를 얻기까지 {ready:.1}초",
                r.status
            ),
        })
    }
}

/// 크루 하나를 돕니다.
pub fn run(opts: &DaemonOpts) -> Result<DaemonRun> {
    STEP_SECS.store(opts.step_secs.max(1), Ordering::Relaxed);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    rt.block_on(run_async(opts))
}

async fn run_async(opts: &DaemonOpts) -> Result<DaemonRun> {
    let dir = tempfile::tempdir()?;
    let mut net = Net { nodes: Vec::new() };
    let r = run_inner(opts, dir.path(), &mut net).await;
    net.shutdown().await;
    r
}

async fn run_inner(opts: &DaemonOpts, dir: &Path, net: &mut Net) -> Result<DaemonRun> {
    let ban_users = opts.ban / BAN_DEVICES_EACH;
    let ban_devices = ban_users * BAN_DEVICES_EACH;
    // 탈퇴 1명 + 기록 나이 날마다 1명.
    let singles = 1 + opts.days;
    let pairs = opts
        .devices
        .saturating_sub(2 + ban_devices + singles)
        .div_ceil(2);
    let setup = Instant::now();
    progress("노드 A·B 시작");
    let a = net.start(dir, "a").await?;
    let b = net.start(dir, "b").await?;
    connect_both(&a, &b).await?;
    let space = call::<m::CrewCreate>(
        &a,
        CrewCreateParams {
            name: "키 그룹 재기".into(),
            description: None,
        },
    )
    .await?
    .space;
    let general = call::<m::ChannelList>(&a, SpaceParams { space })
        .await?
        .first()
        .context("채널 없음")?
        .channel;
    let url = call::<m::InviteCreate>(
        &a,
        InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: None,
            roles: Vec::new(),
        },
    )
    .await?
    .url;
    let token = InviteLink::parse(&url).context("초대 링크")?.token;
    let r = call::<m::InviteJoin>(&b, UrlParams { url }).await?;
    if r.status != InviteJoinStatus::Joined {
        bail!("B 가입: {:?}", r.status);
    }
    let seq = settled(&a, &space, 0).await?;
    caught(&b, &space, seq).await?;
    let mut x = Bench {
        a: a.clone(),
        b: b.clone(),
        space,
        general,
        token,
        mark: 0,
        mirror_b: true,
        checks: 0,
        violations: Vec::new(),
    };

    let mut rows = Vec::new();
    // 1. 크루 채우기.
    progress(&format!(
        "크루 채우기: 2대 사용자 {pairs}명, 밴 물결 {ban_users}명, 1대 사용자 {singles}명"
    ));
    let _ = x.synthetic(pairs, 2).await?;
    let ban_list = x.synthetic(ban_users, BAN_DEVICES_EACH).await?;
    let single_list = x.synthetic(singles, 1).await?;
    let seq = settled(&a, &space, seq).await?;
    x.invariants("채우기", &[], std::slice::from_ref(&b))
        .await?;
    x.skip_to_now()?;
    let tip = testkit::crew_tip(&a, &space).context("크루 사슬")?;
    let devices = tip.members.len();
    let users = testkit::space_state(&a, &space)
        .context("상태")?
        .members
        .len();
    let depth = tip.depth;
    let setup_secs = setup.elapsed().as_secs_f64();
    let mode = if depth > 0 {
        format!("트리 깊이 {depth}")
    } else {
        "직접".into()
    };

    // 2. 평상시 메시지: 키 묶음이 하나도 나오지 않아야 함.
    progress(&format!("평상시 메시지 {}개", opts.messages));
    let shares_before = count_key_shares(&a, &space, general)?;
    for i in 0..opts.messages {
        let from = if i % 2 == 0 { &a } else { &b };
        call::<m::MessageSend>(
            from,
            MessageSendParams {
                space,
                channel: general,
                text: format!("평상시 메시지 {i}"),
                attachments: Vec::new(),
                reply_to: None,
                thread: None,
            },
        )
        .await?;
    }
    let steady_key_shares = count_key_shares(&a, &space, general)? - shares_before;
    x.check("평상시 키 묶음이 나옴", steady_key_shares == 0);
    x.skip_to_now()?;

    // 3. 탈퇴(기기 1대).
    progress("탈퇴");
    let mut singles = single_list.into_iter();
    let gone_user = singles.next().context("탈퇴할 사용자")?;
    let gone = x.devices_of(&gone_user);
    let started = Instant::now();
    call::<m::MemberKick>(
        &a,
        MemberKickParams {
            space,
            user: gone_user,
            reason: None,
        },
    )
    .await?;
    let (bytes, parts, secs) = x.crew_update(seq, started).await?;
    x.invariants("탈퇴", &gone, std::slice::from_ref(&b))
        .await?;
    rows.push(DaemonRow {
        op: format!("탈퇴(기기 1대, {mode}): 크루 기기당 받기"),
        bytes: bytes as f64,
        target: (depth > 0).then_some(4_500.0),
        secs: Some(secs),
        detail: format!("크루 갱신 {parts}조각"),
    });

    // 4. 실제 가입자.
    progress("실제 가입");
    let Joined {
        node: c,
        inbox,
        advance,
        secs,
        note,
    } = x.real_join(net, dir, "c").await?;
    x.invariants("가입", &[], &[b.clone(), c.clone()]).await?;
    rows.push(DaemonRow {
        op: format!("가입(기기 1대, {mode}): 크루 기기당 받기"),
        bytes: advance as f64,
        target: (depth > 0).then_some(2_000.0),
        secs: Some(secs),
        detail: format!("가입 전진 갱신, {note}"),
    });
    rows.push(DaemonRow {
        op: "가입: 가입자 받기(전진 갱신 + 받은편지함 묶음)".into(),
        bytes: (advance + inbox) as f64,
        target: Some(16_000.0),
        secs: None,
        detail: format!("받은편지함 {inbox}B"),
    });
    let mut joiner_by_age = vec![(0, inbox as f64)];

    // 5. 밴 물결: API로 잇달아 밴합니다. 바퀴를 미루지 않으므로 확정 기기의 묶음 창(빚이 늘면 늘어남)이 실제로 몇 개의
    //    갱신으로 묶는지를 잽니다.
    if !ban_list.is_empty() {
        progress("밴 물결");
        let seq = testkit::crew_tip(&a, &space).context("사슬")?.seq;
        x.skip_to_now()?;
        let mut gone = Vec::new();
        let started = Instant::now();
        for u in &ban_list {
            gone.extend(x.devices_of(u));
            call::<m::MemberBan>(
                &a,
                MemberBanParams {
                    space,
                    user: *u,
                    reason: None,
                    delete_recent: false,
                },
            )
            .await?;
        }
        let (bytes, parts, secs) = x.crew_update(seq, started).await?;
        let updates = testkit::crew_tip(&a, &space).context("사슬")?.seq - seq;
        x.invariants("밴 물결", &gone, &[b.clone(), c.clone()])
            .await?;
        rows.push(DaemonRow {
            op: format!(
                "밴 물결({}명·{}대, {mode}): 크루 기기당 받기",
                ban_list.len(),
                gone.len()
            ),
            bytes: bytes as f64,
            target: (depth > 0).then_some(1_600_000.0),
            secs: Some(secs),
            detail: format!("크루 갱신 {updates}개·{parts}조각"),
        });
    }

    // 6. 전체 재키.
    {
        progress("전체 재키");
        let seq = testkit::crew_tip(&a, &space).context("사슬")?.seq;
        x.skip_to_now()?;
        let started = Instant::now();
        testkit::force_full_rekey(&a, space);
        let (head, parts, secs) = x.crew_update(seq, started).await?;
        x.invariants("전체 재키", &[], &[b.clone(), c.clone()])
            .await?;
        // 실제 멤버 B가 확정 기기에게 자기 사용자 몫만 묻는 답(`GetKeyShard`)의 전송 크기.
        let shard_answer = match testkit::last_shard_blob(&a, &space) {
            Some(blob) if depth > 0 => {
                let r = testkit::net_request(
                    &b,
                    a.peer_id(),
                    yangpa_core::wire::YpRequest::GetKeyShard { space, blob },
                )
                .await
                .context("GetKeyShard")?;
                match &r {
                    yangpa_core::wire::YpResponse::Chunk { .. } => {
                        Some(postcard::to_stdvec(&r).map_or(0, |v| v.len()))
                    }
                    other => bail!("GetKeyShard 답이 조각이 아님: {other:?}"),
                }
            }
            _ => None,
        };
        match testkit::shard_stats(&a, &space) {
            Some(s) if depth > 0 => {
                rows.push(DaemonRow {
                    op: "전체 재키: 사용자 하나의 조각(평균)".into(),
                    bytes: s.user_bytes_avg,
                    target: Some(5_000.0),
                    secs: None,
                    detail: format!("{}명, 최대 {}B", s.users, s.user_bytes_max),
                });
                rows.push(DaemonRow {
                    op: "전체 재키: 기기당 받기(머리 이벤트 + 자기 사용자 조각)".into(),
                    bytes: head as f64 + s.user_bytes_avg,
                    target: Some(5_000.0),
                    secs: None,
                    detail: format!(
                        "모형과 같은 사용자 몫 셈(GetKeyShard 답은 기기 몫만). 묻지 못하면 블롭 조각 단위: 머리 {head}B + 머리 조각 {} + 자료 조각 1개 = {}B",
                        s.header_pages,
                        head as u64 + u64::from(s.header_pages + 1) * u64::from(s.page_bytes)
                    ),
                });
                if let Some(n) = shard_answer {
                    rows.push(DaemonRow {
                        op: "전체 재키: 실제 멤버 B의 받기(머리 이벤트 + GetKeyShard 답)".into(),
                        bytes: (head + n) as f64,
                        target: Some(5_000.0),
                        secs: None,
                        detail: format!("답 {n}B(기기 1대 사용자의 묶음, 전송 인코딩)"),
                    });
                }
                rows.push(DaemonRow {
                    op: format!("전체 재키({mode}): 발행 합(머리 + 조각 블롭)"),
                    bytes: head as f64 + s.blob_bytes as f64,
                    target: None,
                    secs: Some(secs),
                    detail: format!("머리 {parts}조각, 블롭 자료 조각 {}개", s.data_pages),
                });
            }
            _ => rows.push(DaemonRow {
                op: format!("전체 재키({mode}): 발행 합"),
                bytes: head as f64,
                target: None,
                secs: Some(secs),
                detail: format!("{parts}조각(조각 블롭 없음: 받은편지함 묶음)"),
            }),
        }
    }

    // 7. 기록 나이: 하루에 가입 1명·추방 1명, 7일마다 전체 재키.
    let mut fetched_state_keys = 0;
    if opts.days > 0 {
        // 묶음이 상태 키 64개에서 멈추는지 보이게 여러 나이에서 잽니다.
        let probe_at: BTreeSet<usize> = [30usize, 90, 180, opts.days]
            .into_iter()
            .map(|d| d.min(opts.days))
            .collect();
        let mut seq = testkit::crew_tip(&a, &space).context("사슬")?.seq;
        for day in 1..=opts.days {
            progress(&format!("기록 나이 {day}일"));
            let joined = x.synthetic(1, 1).await?;
            let _ = joined;
            let user = singles.next().context("나이용 사용자")?;
            let gone = x.devices_of(&user);
            call::<m::MemberKick>(
                &a,
                MemberKickParams {
                    space,
                    user,
                    reason: None,
                },
            )
            .await?;
            seq = settled(&a, &space, seq).await?;
            if day % 7 == 0 {
                testkit::force_full_rekey(&a, space);
                seq = settled(&a, &space, seq).await?;
            }
            if day % 30 == 0 || day == opts.days {
                x.invariants(&format!("{day}일"), &gone, &[b.clone(), c.clone()])
                    .await?;
            }
            if probe_at.contains(&day) {
                let joined = x.real_join(net, dir, &format!("j{day}")).await?;
                let (j, inbox) = (joined.node, joined.inbox);
                joiner_by_age.push((day, inbox as f64));
                // 옛 상태 이벤트를 다 열 때까지(가입 묶음보다 오래된 키는 물어 받음).
                until("가입자가 기다리는 이벤트 없음", || async {
                    (testkit::waiting_count(&j, &space) == 0).then_some(())
                })
                .await?;
                fetched_state_keys = testkit::state_key_count(&j, &space);
                x.invariants(&format!("{day}일 가입"), &[], std::slice::from_ref(&j))
                    .await?;
                seq = testkit::crew_tip(&a, &space).context("사슬")?.seq;
            }
            x.skip_to_now()?;
        }
        let first = joiner_by_age.first().map_or(0.0, |r| r.1);
        let last = joiner_by_age.last().map_or(0.0, |r| r.1);
        rows.push(DaemonRow {
            op: format!("가입 묶음: {}일 기록 뒤 새 가입자", opts.days),
            bytes: last,
            target: Some(16_000.0),
            secs: None,
            detail: format!(
                "처음 {first:.0}B, 날별 {:?}, 마지막 가입자가 가진 상태 키 {fetched_state_keys}개(묶음 64개까지 + 물어 받음)",
                joiner_by_age
            ),
        });
    }

    // 8. 직접 인코딩 크루의 탈퇴.
    if opts.direct > 2 {
        progress(&format!("직접 인코딩 크루 {}대 탈퇴", opts.direct));
        rows.push(direct_removal(&a, &b, opts.direct).await?);
    }

    let _ = x.general;
    Ok(DaemonRun {
        devices,
        users,
        depth,
        setup_secs,
        rows,
        steady_key_shares,
        joiner_by_age,
        fetched_state_keys,
        checks: x.checks,
        violations: x.violations,
    })
}

/// 채널 스트림의 키 묶음 수 + 공간 스트림의 범위 없는 키 묶음 수.
fn count_key_shares(a: &Arc<Node>, space: &SpaceId, channel: ChannelId) -> Result<usize> {
    let ch = testkit::stream_events_since(a, space, StreamId::Channel(channel), 0, 1 << 20)?
        .iter()
        .filter(|e| e.header.kind == EventKind::KeyShare)
        .count();
    let sp = testkit::stream_events_since(a, space, StreamId::Space, 0, 1 << 20)?
        .iter()
        .filter(|e| e.header.kind == EventKind::KeyShare && e.header.scope.is_none())
        .count();
    Ok(ch + sp)
}

/// 직접 인코딩(256대 이하) 크루에서 기기 1대 사용자의 탈퇴: 남은 기기 모두에게 새 키를 하나씩 봉인합니다.
async fn direct_removal(a: &Arc<Node>, b: &Arc<Node>, devices: usize) -> Result<DaemonRow> {
    let space = call::<m::CrewCreate>(
        a,
        CrewCreateParams {
            name: "직접 인코딩".into(),
            description: None,
        },
    )
    .await?
    .space;
    let url = call::<m::InviteCreate>(
        a,
        InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: None,
            roles: Vec::new(),
        },
    )
    .await?
    .url;
    let token = InviteLink::parse(&url).context("초대 링크")?.token;
    let seq = settled(a, &space, 0).await?;
    let mut x = Bench {
        a: a.clone(),
        b: b.clone(),
        space,
        general: ChannelId([0; 16]),
        token,
        mark: 0,
        // B는 이 크루에 들지 않습니다(확정 기기 쪽 바이트만 잼).
        mirror_b: false,
        checks: 0,
        violations: Vec::new(),
    };
    let users = x.synthetic(devices.saturating_sub(1), 1).await?;
    let seq = settled(a, &space, seq).await?;
    let depth = testkit::crew_tip(a, &space).context("사슬")?.depth;
    x.skip_to_now()?;
    let gone = users.first().copied().context("뺄 사용자")?;
    let started = Instant::now();
    call::<m::MemberKick>(
        a,
        MemberKickParams {
            space,
            user: gone,
            reason: None,
        },
    )
    .await?;
    let (bytes, parts, secs) = x.crew_update(seq, started).await?;
    let n = testkit::crew_tip(a, &space).map_or(0, |t| t.members.len());
    Ok(DaemonRow {
        op: format!("탈퇴({devices}대 크루, 직접 인코딩 깊이 {depth}): 크루 기기당 받기"),
        bytes: bytes as f64,
        target: None,
        secs: Some(secs),
        detail: format!(
            "크루 갱신 {parts}조각, 남은 기기 {n}대(기기당 {:.0}B)",
            bytes as f64 / n.max(1) as f64
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 작은 크루(지어낸 멤버로 256대를 넘겨 트리)로 끝까지 돌고, 불변식이 지켜지며 평상시 키 묶음이 없는지.
    #[test]
    fn small_daemon_run_keeps_invariants() {
        // `YANGPA_TEST_LOG=yangpa_daemon=debug`로 데몬 기록을 봅니다(기본은 끔).
        let filter = tracing_subscriber::EnvFilter::try_from_env("YANGPA_TEST_LOG")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
        let r = run(&DaemonOpts {
            devices: 300,
            ban: 100,
            direct: 0,
            days: 2,
            messages: 4,
            step_secs: 120,
        })
        .unwrap();
        assert!(r.depth > 0, "{r:?}");
        assert!(r.violations.is_empty(), "{:?}", r.violations);
        assert_eq!(r.steady_key_shares, 0);
        assert!(r.checks > 10);
    }
}
