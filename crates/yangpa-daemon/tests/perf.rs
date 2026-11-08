//! 성능 목표 확인(docs/plan.md "성능 목표", docs/performance.md).
//!
//! 측정은 기본으로 건너뜁니다(`#[ignore]`). 최적화 빌드에서 하나씩 돌려야 의미가 있어 `bench.yml`이 이렇게 부릅니다.
//! 시뮬레이션이 찾은 문제의 회귀 시험(`gossip_relays_messages_from_unmet_members`)만 늘 돕니다.
//!
//! ```text
//! cargo test --profile perf -p yangpa-daemon --test perf -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! - 직접 연결 전달 지연 p95 ≤ 300ms, 실행부터 대화 목록까지 ≤ 1초(채운 DB), 유휴 데몬 메모리 ≤ 150MB·CPU < 1%,
//!   크루 가입부터 상태·최근 메시지 표시까지 ≤ 10초.
//! - 여러 노드 시뮬레이션(`scaled_crew_simulation`)은 수치를 기록만 합니다(목표는 P6에서 정함). 지연·손실은
//!   `bench.yml`이 루프백에 `tc netem`을 걸어 흉내 냅니다.
//! - 디버그 빌드처럼 목표를 볼 수 없는 환경에서는 `YANGPA_PERF_REPORT_ONLY=1`로 수치만 찍습니다.
//! - 기다릴 때는 조건을 다시 보는 도우미만 씁니다. 고정 시간을 쓰는 곳은 유휴 CPU를 재는 구간 하나뿐입니다.

mod common;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{
    Cluster, TIMEOUT, TestNode, befriend, call, connect_both, eventually, next_notification,
};
use yangpa_core::ids::{ChannelId, EventId, SpaceId};
use yangpa_core::model::{ChannelKind, HistoryVisibility};
use yangpa_core::wire::YpRequest;
use yangpa_daemon::{Node, NodeConfig, testkit};
use yangpa_rpc::api::{
    ChannelCreateParams, ContactKind, CrewCreateParams, Empty, FriendLinkCreateParams,
    IdentityCreateParams, InviteCreateParams, InviteJoinStatus, MessageListParams,
    MessageSendParams, Notification, SpaceParams, UrlParams, m,
};
use yangpa_rpc::endpoint::{OWNER_TOKEN_FILE, read_token_file};
use yangpa_rpc::{Endpoint, RpcClient};

// ───────────── 목표(docs/plan.md) ─────────────

const DELIVERY_P95: Duration = Duration::from_millis(300);
const START_TO_LIST: Duration = Duration::from_secs(1);
const IDLE_MEMORY: u64 = 150 * 1024 * 1024;
const IDLE_CPU_PERCENT: f64 = 1.0;
const JOIN_TO_USABLE: Duration = Duration::from_secs(10);

// ───────────── 도우미 ─────────────

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn report_only() -> bool {
    std::env::var_os("YANGPA_PERF_REPORT_ONLY").is_some_and(|v| !v.is_empty() && v != "0")
}

/// 결과 한 줄을 찍고 GitHub Actions 요약에도 남깁니다.
fn report(line: &str) {
    println!("성능: {line}");
    if let Some(p) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(p)
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("- {line}\n").as_bytes()));
    }
}

/// 목표를 넘으면 실패(수치만 보는 모드면 경고만).
fn check(ok: bool, what: &str) {
    if ok {
        return;
    }
    if report_only() {
        println!("성능 경고(수치만 보는 모드): {what}");
    } else {
        panic!("성능 목표 미달: {what}");
    }
}

/// 정렬한 값의 백분위(가장 가까운 순위).
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    assert!(!sorted.is_empty());
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn summary(v: &mut [Duration]) -> (Duration, Duration, Duration) {
    v.sort();
    (
        percentile(v, 50.0),
        percentile(v, 95.0),
        *v.last().expect("값 있음"),
    )
}

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

async fn invite(a: &TestNode, space: SpaceId) -> String {
    a.ok::<m::InviteCreate>(InviteCreateParams {
        space,
        expires_in_secs: None,
        max_uses: None,
        roles: Vec::new(),
    })
    .await
    .url
}

fn public_channel(space: SpaceId, name: &str) -> ChannelCreateParams {
    ChannelCreateParams {
        space,
        name: name.into(),
        kind: ChannelKind::Text,
        parent: None,
        topic: None,
        security: None,
        join_mode: None,
        ttl_secs: None,
        ttl_on_leave: None,
        history: Some(HistoryVisibility::Public),
        link_previews: None,
        private_roles: None,
        position: None,
        forum_tags: None,
        p2p: None,
        overwrites: None,
        anonymous: None,
        attach_limit: None,
        no_edit: None,
        no_delete: None,
    }
}

/// 멤버이고 크루 키 그룹에 들어갈 때까지.
async fn wait_joined(n: &TestNode, space: SpaceId) {
    let me = n.user();
    eventually(&format!("{} 가입 완료", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        (st.is_member(&me) && testkit::crew_key_seq(&n.node, &space).is_some()).then_some(())
    })
    .await;
}

/// 서로 멤버로 본 뒤 직접 인증합니다(시험에는 기기 위치 찾기가 없어 가입 전 연결은 인증 없이 남음).
async fn meet(x: &TestNode, y: &TestNode, space: SpaceId) {
    for (p, q) in [(x, y), (y, x)] {
        let user = q.user();
        eventually(
            &format!("{}가 {}를 멤버로 봄", p.name, q.name),
            || async {
                testkit::space_state(&p.node, &space)?
                    .is_member(&user)
                    .then_some(())
            },
        )
        .await;
    }
    testkit::authenticate(&x.node, y.peer())
        .await
        .unwrap_or_else(|e| panic!("{} ↔ {} 인증 실패: {e}", x.name, y.name));
}

/// 따로 띄운 `yangpad` 프로세스(테스트가 끝나면 죽입니다).
struct Daemon {
    child: tokio::process::Child,
    client: RpcClient,
    /// 실행부터 소유자 토큰으로 API에 붙을 때까지.
    ready: Duration,
}

impl Daemon {
    async fn spawn(dir: &Path) -> Daemon {
        Self::spawn_with(dir, &[]).await
    }

    /// 시작 옵션을 더해 띄웁니다(예: `--bootstrap`).
    async fn spawn_with(dir: &Path, extra: &[&str]) -> Daemon {
        let ep = Endpoint::in_dir(dir);
        let token_path = dir.join(OWNER_TOKEN_FILE);
        let t0 = Instant::now();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_yangpad"))
            .arg("--data-dir")
            .arg(dir)
            .arg("--endpoint")
            .arg(ep.path())
            .args([
                "--key-storage",
                "file",
                "--no-mdns",
                "--direct",
                "--listen",
                "/ip4/127.0.0.1/tcp/0",
                "--log",
                "warn",
            ])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("yangpad 실행");
        let client = loop {
            if let Ok(token) = read_token_file(&token_path)
                && let Ok((c, _)) = RpcClient::connect_with_token(&ep, token.expose()).await
            {
                break c;
            }
            if let Ok(Some(st)) = child.try_wait() {
                panic!("yangpad가 일찍 끝남: {st}");
            }
            assert!(t0.elapsed() < TIMEOUT, "yangpad API 대기 시간 초과");
            // 붙을 수 있을 때까지 짧게 다시 봅니다(재는 값의 오차는 이 간격 이하).
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        Daemon {
            child,
            client,
            ready: t0.elapsed(),
        }
    }

    async fn ok<M: yangpa_rpc::api::Method>(&self, p: M::Params) -> M::Result {
        self.client
            .request::<M>(p)
            .await
            .unwrap_or_else(|e| panic!("yangpad {} 실패: {e:?}", M::NAME))
    }

    async fn stop(mut self) {
        let _ = self.client.request::<m::SystemShutdown>(Empty).await;
        drop(self.client);
        match tokio::time::timeout(TIMEOUT, self.child.wait()).await {
            Ok(Ok(st)) => assert!(st.success(), "yangpad 종료 상태: {st}"),
            _ => {
                let _ = self.child.kill().await;
            }
        }
    }
}

// ───────────── 1. 직접 연결 전달 지연 ─────────────

/// 보내기 호출 직전부터 받는 쪽 `message.new`까지(서명·암호화·저장·전파·검증·적용 전부 포함).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "성능 측정: bench.yml이 --profile perf로 돌립니다"]
async fn direct_delivery_p95() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let mut rx = b.subscribe();
    let n = env_usize("YANGPA_PERF_MESSAGES", 200);
    let warm = 10;
    let mut lat = Vec::with_capacity(n);
    for i in 0..warm + n {
        let t0 = Instant::now();
        let id = a.send(space, channel, &format!("지연 측정 {i}")).await;
        next_notification(&mut rx, "B가 메시지를 받음", |ev| match ev {
            Notification::MessageNew { message, .. } if message.id == id => Some(()),
            _ => None,
        })
        .await;
        if i >= warm {
            lat.push(t0.elapsed());
        }
    }
    let (p50, p95, max) = summary(&mut lat);
    let metrics = b.ok::<m::DiagMetricsGet>(Empty).await;
    report(&format!(
        "직접 연결 전달 지연 {n}개: p50 {:.1}ms, p95 {:.1}ms, 최대 {:.1}ms (목표 p95 ≤ {}ms; B의 diag.metrics p95 {}ms)",
        ms(p50),
        ms(p95),
        ms(max),
        DELIVERY_P95.as_millis(),
        metrics.delivery_ms_p95
    ));
    check(p95 <= DELIVERY_P95, "직접 연결 전달 지연 p95");
}

// ───────────── 2. 실행부터 대화 목록까지 ─────────────

/// 친구 1:1과 크루 여럿, 메시지 수천 개가 든 DB로 다시 시작해 `space.list`가 돌아올 때까지(네트워크 연결 전).
/// 프로세스 안(`Node::start`)과 `yangpad` 프로세스(실행 → API 인증 → `space.list`) 둘 다 잽니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "성능 측정: bench.yml이 --profile perf로 돌립니다"]
async fn start_to_space_list() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let friend = befriend(&a, &b).await;
    let fch = a.first_channel(friend).await;
    let crews = env_usize("YANGPA_PERF_CREWS", 10);
    let per_crew = env_usize("YANGPA_PERF_CREW_MESSAGES", 200);
    let per_friend = env_usize("YANGPA_PERF_FRIEND_MESSAGES", 500);
    for c in 0..crews {
        let space = create_crew(&a, &format!("크루 {c}")).await;
        let ch = a.first_channel(space).await;
        for i in 0..per_crew {
            a.send(space, ch, &format!("크루 {c} 메시지 {i}")).await;
        }
    }
    let mut last = None;
    for i in 0..per_friend {
        a.send(friend, fch, &format!("A {i}")).await;
        last = Some(b.send(friend, fch, &format!("B {i}")).await);
    }
    let last = last.expect("메시지 하나 이상");
    eventually("A가 B의 마지막 메시지를 받음", || async {
        testkit::message_text(&a.node, &last).map(|_| ())
    })
    .await;
    let spaces = crews + 1;
    let total = crews * per_crew + per_friend * 2;
    a.shutdown().await;
    b.shutdown().await;

    // 프로세스 안: 노드 시작 → space.list.
    let t0 = Instant::now();
    let node = Node::start(NodeConfig::for_test(&a.dir))
        .await
        .expect("노드 다시 시작");
    let list = call::<m::SpaceList>(&node, Empty)
        .await
        .expect("space.list");
    let in_process = t0.elapsed();
    assert_eq!(list.len(), spaces, "공간 수");
    node.shutdown().await;
    drop(node);

    // yangpad 프로세스: 실행 → 소유자 토큰으로 인증 → space.list(SQLCipher 메모리 보호 등 실제 설정).
    let t0 = Instant::now();
    let d = Daemon::spawn(&a.dir).await;
    let list = d.ok::<m::SpaceList>(Empty).await;
    let process = t0.elapsed();
    assert_eq!(list.len(), spaces, "공간 수");
    let ready = d.ready;
    d.stop().await;

    report(&format!(
        "실행부터 대화 목록까지(공간 {spaces}개, 메시지 {total}개): 프로세스 안 {:.0}ms, yangpad {:.0}ms(API 준비 {:.0}ms) (목표 ≤ {}ms)",
        ms(in_process),
        ms(process),
        ms(ready),
        START_TO_LIST.as_millis()
    ));
    check(in_process <= START_TO_LIST, "프로세스 안 시작 → space.list");
    check(process <= START_TO_LIST, "yangpad 실행 → space.list");
}

// ───────────── 3. 유휴 데몬 자원 ─────────────

/// 친구 하나와 연결된 `yangpad`가 할 일 없이 있을 때의 RSS와 CPU(한 코어 기준 %, `diag.metrics`).
/// 재는 구간 길이는 `YANGPA_PERF_IDLE_SECS`(기본 30초).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "성능 측정: bench.yml이 --profile perf로 돌립니다"]
async fn idle_daemon_resources() {
    let cl = Cluster::new();
    let b = cl.node("b").await;
    let dir = cl.dir("a");
    std::fs::create_dir_all(&dir).expect("데이터 경로");
    // B의 주소를 부트스트랩(시작 옵션)으로 넣어 연결하고 친구가 됩니다.
    let b_addr = format!(
        "{}/p2p/{}",
        common::listen_addrs(&b.node).await[0],
        b.peer()
    );
    let d = Daemon::spawn_with(&dir, &["--bootstrap", &b_addr]).await;
    let me = d
        .ok::<m::IdentityCreate>(IdentityCreateParams { name: "a".into() })
        .await;
    let link = b
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    let added = eventually("yangpad가 링크를 씀", || async {
        d.client
            .request::<m::FriendAdd>(UrlParams {
                url: link.url.clone(),
            })
            .await
            .ok()
    })
    .await;
    assert_eq!(added.user, b.user());
    // 링크 주인은 수락 없이 바로 임시 연락처로 기록하고 1:1 공간을 만듭니다.
    let space = eventually("B가 임시 연락처로 기록", || async {
        b.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .find(|f| f.user == me.user && f.kind == ContactKind::Temporary)
            .map(|f| f.space)
    })
    .await;
    eventually("yangpad가 1:1 공간을 받음", || async {
        d.client
            .request::<m::FriendList>(Empty)
            .await
            .ok()?
            .into_iter()
            .any(|f| f.space == space)
            .then_some(())
    })
    .await;
    let channel = eventually("yangpad 1:1 채널", || async {
        d.client
            .request::<m::ChannelList>(SpaceParams { space })
            .await
            .ok()?
            .first()
            .map(|c| c.channel)
    })
    .await;
    // 메시지를 주고받아 두 쪽 모두 키·연결이 자리 잡은 상태로.
    let sent = eventually("yangpad가 보낼 수 있음", || async {
        d.client
            .request::<m::MessageSend>(MessageSendParams {
                space,
                channel,
                text: "유휴 측정 전".into(),
                attachments: Vec::new(),
                reply_to: None,
                thread: None,
            })
            .await
            .ok()
    })
    .await;
    eventually("B가 받음", || async {
        testkit::message_text(&b.node, &sent.id).map(|_| ())
    })
    .await;
    let back = b.send(space, channel, "유휴 측정 전 답").await;
    eventually("yangpad가 받음", || async {
        d.client
            .request::<m::MessageList>(MessageListParams {
                space,
                channel,
                before: None,
                limit: Some(10),
                thread: None,
            })
            .await
            .ok()?
            .messages
            .iter()
            .any(|v| v.id == back)
            .then_some(())
    })
    .await;

    // 첫 호출은 CPU 기준점(0)이고, 재는 구간이 끝난 뒤 두 번째 호출이 그 사이 평균을 줍니다.
    let window = Duration::from_secs(env_usize("YANGPA_PERF_IDLE_SECS", 30) as u64);
    let _ = d.ok::<m::DiagMetricsGet>(Empty).await;
    tokio::time::sleep(window).await;
    let metrics = d.ok::<m::DiagMetricsGet>(Empty).await;
    d.stop().await;

    let mb = metrics.memory_bytes as f64 / (1024.0 * 1024.0);
    report(&format!(
        "유휴 yangpad(친구 1명 연결, {}초): 메모리 {mb:.1}MB, CPU {:.2}% (목표 ≤ {}MB, < {IDLE_CPU_PERCENT}%)",
        window.as_secs(),
        metrics.cpu_percent,
        IDLE_MEMORY / (1024 * 1024)
    ));
    check(metrics.memory_bytes <= IDLE_MEMORY, "유휴 메모리");
    check(metrics.cpu_percent < IDLE_CPU_PERCENT, "유휴 CPU");
}

// ───────────── 4. 크루 가입부터 사용 가능까지 ─────────────

/// 기록 공개 채널에 메시지가 쌓인 크루에 초대 링크로 들어와, 채널 목록과 그 채널의 최근 메시지를 볼 수 있을 때까지.
/// 연결(주소 찾기)은 미리 맺어 둡니다(시험에는 DHT가 없음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "성능 측정: bench.yml이 --profile perf로 돌립니다"]
async fn crew_join_to_usable() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let c = cl.node("c").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "가입 측정").await;
    let general = a.first_channel(space).await;
    let notice = a
        .ok::<m::ChannelCreate>(public_channel(space, "공지"))
        .await
        .channel;
    connect_both(&a, &c).await;
    let url = invite(&a, space).await;
    let r = c.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.status, InviteJoinStatus::Joined);
    wait_joined(&c, space).await;
    let history = env_usize("YANGPA_PERF_JOIN_HISTORY", 200);
    let mut last = None;
    for i in 0..history {
        let who = if i % 2 == 0 { &a } else { &c };
        a.send(space, general, &format!("일반 {i}")).await;
        last = Some(who.send(space, notice, &format!("공지 {i}")).await);
    }
    let last = last.expect("메시지 하나 이상");
    eventually("A가 마지막 공지를 봄", || async {
        testkit::message_text(&a.node, &last).map(|_| ())
    })
    .await;

    connect_both(&a, &b).await;
    connect_both(&c, &b).await;
    let url = invite(&a, space).await;
    let t0 = Instant::now();
    let r = b.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.space, space);
    let usable = usable_after_join(&b, space, notice, last).await;
    let elapsed = t0.elapsed();

    // 쓸 수 있는지: B가 보낸 메시지를 A가 받습니다(목표 밖, 참고 수치).
    let t1 = Instant::now();
    let mine = b.send(space, general, "들어왔어요").await;
    eventually("A가 B의 첫 메시지를 받음", || async {
        testkit::message_text(&a.node, &mine).map(|_| ())
    })
    .await;
    let first_send = t1.elapsed();

    report(&format!(
        "크루 가입 → 상태·최근 메시지(멤버 2명, 공개 기록 {history}개): {:.0}ms(채널 {usable}개), 첫 메시지 전달 {:.0}ms (목표 ≤ {}ms)",
        ms(elapsed),
        ms(first_send),
        JOIN_TO_USABLE.as_millis()
    ));
    check(elapsed <= JOIN_TO_USABLE, "크루 가입 → 사용 가능");
}

/// 가입 상태가 되고 채널 목록과 `channel`의 최근 메시지(`last` 포함)가 보일 때까지. 보이는 채널 수를 돌려줍니다.
async fn usable_after_join(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    last: EventId,
) -> usize {
    wait_joined(n, space).await;
    eventually(
        &format!("{}가 채널과 최근 메시지를 봄", n.name),
        || async {
            let chans = n.call::<m::ChannelList>(SpaceParams { space }).await.ok()?;
            if !chans.iter().any(|c| c.channel == channel) {
                return None;
            }
            let msgs = n
                .call::<m::MessageList>(MessageListParams {
                    space,
                    channel,
                    before: None,
                    limit: Some(50),
                    thread: None,
                })
                .await
                .ok()?;
            msgs.messages
                .iter()
                .any(|v| v.id == last)
                .then_some(chans.len())
        },
    )
    .await
}

// ───────────── 5. 여러 노드 시뮬레이션 ─────────────

/// 기기가 많아 gossip으로 퍼지는 크루에서, 서로 만난 적 없는 멤버의 메시지도 중간 멤버를 거쳐 실시간으로 닿습니다.
/// (시뮬레이션이 찾은 문제: 허용 목록은 이 노드가 인증해 본 기기의 노드뿐인데 처음 올린 피어가 목록에 없으면 버리고
/// 전달도 하지 않아, 큰 크루에서는 대부분의 메시지가 주기 따라잡기 전까지 닿지 않았습니다.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_relays_messages_from_unmet_members() {
    let cl = Cluster::new();
    // 직접 전송 대신 늘 gossip(기기가 8개를 넘는 크루와 같은 경로).
    let gossip_only = |c: &mut NodeConfig| c.push_max_devices = 0;
    let hub = cl.node_with("hub", gossip_only).await;
    let b = cl.node_with("b", gossip_only).await;
    let c = cl.node_with("c", gossip_only).await;
    let space = create_crew(&hub, "중계").await;
    let channel = hub.first_channel(space).await;
    for n in [&b, &c] {
        connect_both(&hub, n).await;
        let url = invite(&hub, space).await;
        n.ok::<m::InviteJoin>(UrlParams { url }).await;
        wait_joined(n, space).await;
    }
    for n in [&b, &c] {
        let user = n.user();
        eventually("허브가 둘 다 멤버로 봄", || async {
            testkit::space_state(&hub.node, &space)?
                .is_member(&user)
                .then_some(())
        })
        .await;
    }
    // B와 C는 연결·인증한 적이 없어 서로의 노드를 모릅니다(허브만 거침).
    assert!(!testkit::net::is_authed(&b.node, &c.peer()));
    assert!(!testkit::net::is_authed(&c.node, &b.peer()));

    let from_b = b.send(space, channel, "B가 보냄").await;
    let from_c = c.send(space, channel, "C가 보냄").await;
    eventually("C가 만나지 않은 B의 메시지를 받음", || async {
        testkit::message_text(&c.node, &from_b).map(|_| ())
    })
    .await;
    eventually("B가 만나지 않은 C의 메시지를 받음", || async {
        testkit::message_text(&b.node, &from_c).map(|_| ())
    })
    .await;
    assert!(
        !testkit::net::is_authed(&b.node, &c.peer()),
        "직접 만나지 않음"
    );
}

/// 노드 `YANGPA_SIM_NODES`개(기본 24)가 한 크루에 들어가(별 + 이웃 둘씩의 고리 연결, 활성 기기가 8개를 넘어 gossip
/// 경로) 저마다 메시지 하나를 보내고, 모든 노드가 모두 받을 때까지의 지연 분포를 기록합니다. 실패 조건은 시간 안에
/// 모두 받는지뿐입니다. gossip이 놓친 것은 실제처럼 주기 따라잡기가 채우되, 간격을 `YANGPA_SIM_SYNC_SECS`(기본 10초,
/// 실제 기본은 5분)로 줄이고 5초 넘게 걸린 전달 수를 따로 찍습니다. 지연·손실은 이 시험 밖에서(`tc netem`) 걸고
/// `YANGPA_SIM_NETEM`에 설명을 넣습니다.
///
/// `YANGPA_SIM_TOR=1`이면 노드가 모두 실제 Tor 망(Arti, onion 주소)으로 돕니다(12단계 측정, 노드 30~60개). 노드 시작·
/// 연결은 동시에 하고(Tor 연결 하나에 수십 초), 이어진 쌍마다 `Ping` 왕복으로 홉 지연을 재서 하네스 gossip 모형에 넣을
/// `yangpa-bench scale` 명령을 찍습니다. 이때만 크루 토픽 전달 p95 ≤ 3초를 봅니다. onion 서비스가 서기까지 몇 분 걸리므로
/// `YANGPA_TEST_TIMEOUT_SECS`(예: 1800)로 기다림을 늘립니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "규모 시뮬레이션: bench.yml이 --profile perf로 돌립니다"]
async fn scaled_crew_simulation() {
    let count = env_usize("YANGPA_SIM_NODES", 24).clamp(3, 64);
    let tor = env_flag("YANGPA_SIM_TOR");
    let netem = if tor {
        "실제 Tor 망".to_string()
    } else {
        std::env::var("YANGPA_SIM_NETEM").unwrap_or_else(|_| "없음".into())
    };
    let sync_secs = env_usize("YANGPA_SIM_SYNC_SECS", 10);
    let cl = Cluster::new();
    let names: Vec<String> = (0..count).map(|i| format!("n{i:02}")).collect();
    let t_start = Instant::now();
    let nodes: Vec<TestNode> = if tor {
        let nodes =
            futures::future::join_all(names.iter().map(|n| cl.node_with(n, sim_config_tor))).await;
        futures::future::join_all(nodes.iter().map(wait_onion)).await;
        nodes
    } else {
        let mut v = Vec::with_capacity(count);
        for n in &names {
            v.push(cl.node_with(n, sim_config).await);
        }
        v
    };
    let started = t_start.elapsed();
    let hub = &nodes[0];
    let space = create_crew(hub, "시뮬레이션").await;
    let channel = hub.first_channel(space).await;

    // 가입: 모두 허브와 연결하고 차례로 초대 링크로 들어옵니다(가입마다 걸린 시간도 기록).
    if tor {
        futures::future::join_all(nodes[1..].iter().map(|n| connect_retry(hub, n))).await;
    }
    let mut joins = Vec::with_capacity(count - 1);
    for n in &nodes[1..] {
        if !tor {
            connect_both(hub, n).await;
        }
        let url = invite(hub, space).await;
        let t0 = Instant::now();
        n.ok::<m::InviteJoin>(UrlParams { url }).await;
        wait_joined(n, space).await;
        joins.push(t0.elapsed());
    }
    // 허브 말고도 길이 있게, 나머지 노드를 고리로 놓고 양쪽 이웃 둘씩과 연결·인증합니다(노드마다 피어 5개 안팎,
    // gossip 메시 크기 6에 가깝게).
    let ring = count - 1;
    let mut pairs = HashSet::new();
    for i in 0..ring {
        for step in [1, 2] {
            let j = (i + step) % ring;
            if j != i {
                pairs.insert((1 + i.min(j), 1 + i.max(j)));
            }
        }
    }
    let mut pairs: Vec<_> = pairs.into_iter().collect();
    pairs.sort();
    if tor {
        futures::future::join_all(pairs.iter().map(|&(x, y)| {
            let (a, b) = (&nodes[x], &nodes[y]);
            async move {
                connect_retry(a, b).await;
                meet(a, b, space).await;
            }
        }))
        .await;
    } else {
        for &(x, y) in &pairs {
            connect_both(&nodes[x], &nodes[y]).await;
            meet(&nodes[x], &nodes[y], space).await;
        }
    }
    // 홉 지연: 이어진 쌍마다 `Ping` 세 번의 왕복 절반.
    let mut hops: Vec<Duration> = futures::future::join_all(pairs.iter().map(|&(x, y)| {
        let (a, b) = (&nodes[x], &nodes[y]);
        async move {
            let mut out = Vec::new();
            for _ in 0..3 {
                let t0 = Instant::now();
                if testkit::net_request(&a.node, b.peer(), YpRequest::Ping)
                    .await
                    .is_ok()
                {
                    out.push(t0.elapsed() / 2);
                }
            }
            out
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();

    // 받기 기록: 노드마다 알림을 받아 (받은 노드, 메시지, 시각)을 모읍니다.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, EventId, Instant)>();
    let mut tasks = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let mut sub = n.subscribe();
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(Notification::MessageNew { message, .. }) => {
                        if tx.send((i, message.id, Instant::now())).is_err() {
                            return;
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }));
    }
    drop(tx);

    // 데우기: 허브 메시지 하나가 모두에게 닿을 때까지(gossip 메시 형성).
    let warm = hub.send(space, channel, "데우기").await;
    let mut seen: HashMap<EventId, HashSet<usize>> = HashMap::new();
    let mut arrivals: Vec<(usize, EventId, Instant)> = Vec::new();
    collect_until(
        &mut rx,
        &mut seen,
        &mut arrivals,
        &[(warm, 0)],
        count,
        "데우기 메시지",
    )
    .await;

    // 측정: 모든 노드가 하나씩 보냅니다.
    let mut sent: HashMap<EventId, (usize, Instant)> = HashMap::new();
    let t_all = Instant::now();
    for (i, n) in nodes.iter().enumerate() {
        let t0 = Instant::now();
        let id = n.send(space, channel, &format!("시뮬레이션 {i}")).await;
        sent.insert(id, (i, t0));
    }
    let ids: Vec<(EventId, usize)> = sent.iter().map(|(id, (from, _))| (*id, *from)).collect();
    collect_until(
        &mut rx,
        &mut seen,
        &mut arrivals,
        &ids,
        count,
        "모든 메시지",
    )
    .await;
    let converge = t_all.elapsed();
    for t in tasks {
        t.abort();
    }

    let mut lat: Vec<Duration> = Vec::new();
    let mut first: HashMap<(usize, EventId), Instant> = HashMap::new();
    for (node, id, at) in arrivals {
        first.entry((node, id)).or_insert(at);
    }
    // 고리 이웃 사이 전달(대부분 메시 한 홉, 검증·저장 포함): 하네스 모형의 홉 지연으로 씁니다. 허브는 이웃이 많아
    // 메시 밖 이웃이 있으므로 뺍니다.
    let ring_pairs: HashSet<(usize, usize)> = pairs.iter().copied().collect();
    let mut one_hop: Vec<Duration> = Vec::new();
    for ((node, id), at) in first {
        if let Some(&(from, t0)) = sent.get(&id)
            && from != node
        {
            let d = at.saturating_duration_since(t0);
            lat.push(d);
            if ring_pairs.contains(&(from.min(node), from.max(node))) {
                one_hop.push(d);
            }
        }
    }
    let late = lat.iter().filter(|d| **d > Duration::from_secs(5)).count();
    let (p50, p95, max) = summary(&mut lat);
    let (j50, j95, jmax) = summary(&mut joins);
    if !hops.is_empty() && !one_hop.is_empty() {
        let (h50, h95, hmax) = summary(&mut hops);
        let (o50, o95, _) = summary(&mut one_hop);
        // 고리 연결은 노드마다 이웃 넷과 허브 하나.
        report(&format!(
            "시뮬레이션 홉(netem: {netem}): Ping 왕복 절반 {}건 p50 {:.0}ms·p95 {:.0}ms·최대 {:.0}ms, 이웃 전달 {}건 p50 {:.0}ms·p95 {:.0}ms; 노드 시작 {:.1}초. 모형: yangpa-bench scale --gossip {count}:5 --density 1 --hop-ms {:.0},{:.0}",
            hops.len(),
            ms(h50),
            ms(h95),
            ms(hmax),
            one_hop.len(),
            ms(o50),
            ms(o95),
            started.as_secs_f64(),
            ms(o50).max(1.0),
            ms(o95).max(ms(o50)).max(1.0),
        ));
    }
    report(&format!(
        "시뮬레이션 노드 {count}개(netem: {netem}, 따라잡기 {sync_secs}초): 전달 {}건 p50 {:.0}ms, p95 {:.0}ms, 최대 {:.0}ms, 5초 넘음 {late}건, 전체 수렴 {:.0}ms; 가입 p50 {:.0}ms, p95 {:.0}ms, 최대 {:.0}ms",
        lat.len(),
        ms(p50),
        ms(p95),
        ms(max),
        ms(converge),
        ms(j50),
        ms(j95),
        ms(jmax)
    ));
    if tor && !report_only() {
        assert!(
            p95 <= TOR_CREW_P95,
            "실제 Tor 크루 토픽 전달 p95 {:.0}ms > 목표 {:.0}ms",
            ms(p95),
            ms(TOR_CREW_P95)
        );
    }
}

/// 실제 Tor 망에서 크루 토픽 전달 p95 목표(대형 크루 12단계).
const TOR_CREW_P95: Duration = Duration::from_secs(3);

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// 시뮬레이션 노드 설정: gossip이 놓친 것을 채우는 주기 따라잡기 간격만 줄입니다.
fn sim_config(c: &mut NodeConfig) {
    c.sync_interval = Duration::from_secs(env_usize("YANGPA_SIM_SYNC_SECS", 10).max(1) as u64);
}

/// [`sim_config`]에 더해 실제 Tor 망(Arti)으로 돕니다.
fn sim_config_tor(c: &mut NodeConfig) {
    sim_config(c);
    c.net.direct = false;
}

/// onion 서비스가 설 때까지.
async fn wait_onion(n: &TestNode) {
    eventually(&format!("{} onion 주소", n.name), || async {
        n.ok::<m::NetStatusGet>(Empty).await.onion_addr.map(|_| ())
    })
    .await;
}

/// 서로 연결합니다. 막 선 onion 서비스는 서술자가 퍼질 때까지 닿지 않을 수 있어 될 때까지 다시 겁니다.
async fn connect_retry(a: &TestNode, b: &TestNode) {
    for (x, y) in [(a, b), (b, a)] {
        eventually(&format!("{} → {} 연결", x.name, y.name), || async {
            let addrs = y.node.listen_addrs().await;
            x.node.connect(y.peer(), addrs).await.ok()
        })
        .await;
    }
}

/// `ids`(메시지, 보낸 노드)가 보낸 노드 말고 나머지 `count - 1`개 노드에 모두 닿을 때까지 알림을 모읍니다.
async fn collect_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(usize, EventId, Instant)>,
    seen: &mut HashMap<EventId, HashSet<usize>>,
    arrivals: &mut Vec<(usize, EventId, Instant)>,
    ids: &[(EventId, usize)],
    count: usize,
    what: &str,
) {
    let others = |seen: &HashMap<EventId, HashSet<usize>>, id: &EventId, from: usize| {
        seen.get(id)
            .map_or(0, |s| s.iter().filter(|&&n| n != from).count())
    };
    let done = |seen: &HashMap<EventId, HashSet<usize>>| {
        ids.iter()
            .all(|(id, from)| others(seen, id, *from) >= count - 1)
    };
    let fut = async {
        while !done(seen) {
            let Some((node, id, at)) = rx.recv().await else {
                panic!("알림 수집이 끝남: {what}");
            };
            seen.entry(id).or_default().insert(node);
            arrivals.push((node, id, at));
        }
    };
    if tokio::time::timeout(common::timeout(), fut).await.is_err() {
        // 보낸 노드 → 못 받은 노드들.
        let missing: Vec<String> = ids
            .iter()
            .filter_map(|(id, from)| {
                let got = seen.get(id);
                let lost: Vec<usize> = (0..count)
                    .filter(|n| n != from && !got.is_some_and(|s| s.contains(n)))
                    .collect();
                (!lost.is_empty()).then(|| format!("{from}→{lost:?}"))
            })
            .collect();
        panic!("시간 초과: {what}(못 받은 전달: {})", missing.join(", "));
    }
}
