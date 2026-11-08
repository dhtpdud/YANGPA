//! 여러 노드를 한 프로세스에 띄우는 시험 도구.
//!
//! - 노드는 127.0.0.1에서만 듣고 mDNS·Tor·릴레이·DHT 탐색 없이 서로 직접 연결합니다([`connect`]).
//! - 기다릴 때는 고정 sleep 대신 [`eventually`]·[`next_notification`]처럼 조건이 될 때까지 짧게 다시 보는
//!   도우미를 씁니다(넉넉한 시간 제한 [`TIMEOUT`]).
//! - 다른 영역 시험도 `mod common;`으로 가져다 씁니다.

#![allow(dead_code)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::sync::broadcast;
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_daemon::{CallCtx, Node, NodeConfig};
use yangpa_net::Multiaddr;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    ContactKind, DeviceLinkDecideParams, DeviceLinkJoinParams, Empty, FriendLinkCreateParams,
    IdentityCreateParams, MessageListParams, MessageSendParams, MessageView, Method, Notification,
    SpaceParams, UrlParams, UserParams, m,
};

/// 줄지 않는(압축되지 않는) 결정적 바이트(xorshift). 블롭은 압축한 뒤 조각으로 나누므로, 조각 수·크기를 보는
/// 시험은 반복 무늬 대신 이것을 씁니다.
pub fn noise(len: usize, seed: u8) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ (u64::from(seed) << 32 | 1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// 조건 하나를 기다리는 최대 시간(느린 CI·동시에 도는 시험을 감안해 넉넉히).
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// [`TIMEOUT`]을 `YANGPA_TEST_TIMEOUT_SECS`로 늘린 값(실제 Tor 망 측정처럼 onion 서비스가 서기까지 몇 분 걸릴 때).
pub fn timeout() -> Duration {
    std::env::var("YANGPA_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(TIMEOUT, Duration::from_secs)
}
/// 조건을 다시 볼 간격.
const POLL: Duration = Duration::from_millis(50);

/// `YANGPA_TEST_LOG=yangpa_daemon=debug`처럼 주면 로그를 봅니다.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("YANGPA_TEST_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

/// 시험 노드 하나. 데이터 경로를 들고 있어 같은 경로로 다시 띄울 수 있습니다.
pub struct TestNode {
    pub node: Arc<Node>,
    pub dir: PathBuf,
    pub name: String,
    /// 처음 받은 수신 주소. 다시 띄울 때 같은 포트를 써서 다른 노드의 주소 기록이 그대로 맞게 합니다
    /// (실제 데몬도 포트를 고정하고, 바뀐 주소는 위치 기록이 찾습니다).
    listen: Vec<Multiaddr>,
    /// 시험별 설정 바꾸기(다시 띄울 때도 적용).
    tweak: fn(&mut NodeConfig),
    /// [`TestNode::add_bootstrap`]으로 더한 상시 노드. 다시 띄울 때 시작 옵션(`--bootstrap`)처럼 넣습니다.
    bootstrap: Mutex<Vec<Multiaddr>>,
    /// 클러스터가 끝날 때 멈출 노드 목록(다시 띄운 노드도 넣음).
    started: Started,
}

impl TestNode {
    pub fn peer(&self) -> yangpa_net::PeerId {
        self.node.peer_id()
    }

    pub fn user(&self) -> UserId {
        yangpa_daemon::testkit::user_id(&self.node).expect("신원 있음")
    }

    /// 소유자 권한으로 API를 부릅니다.
    pub async fn call<M: Method>(&self, p: M::Params) -> Result<M::Result, RpcError> {
        call::<M>(&self.node, p).await
    }

    /// 실패하면 메서드 이름과 함께 멈춥니다.
    pub async fn ok<M: Method>(&self, p: M::Params) -> M::Result {
        match self.call::<M>(p).await {
            Ok(r) => r,
            Err(e) => panic!("{} {} 실패: {e:?}", self.name, M::NAME),
        }
    }

    /// 노드를 멈추고 같은 데이터 경로로 다시 띄웁니다(키·키 그룹 상태·이벤트가 디스크에서 돌아와야 함).
    pub async fn restart(&mut self) {
        self.node.shutdown().await;
        self.start_again().await;
    }

    /// 설정 바꾸기를 갈아 끼우고 같은 데이터 경로로 다시 띄웁니다(저장된 설정을 먼저 바꿔 두고 띄울 때).
    pub async fn restart_with(&mut self, tweak: fn(&mut NodeConfig)) {
        self.tweak = tweak;
        self.restart().await;
    }

    pub async fn shutdown(&self) {
        self.node.shutdown().await;
    }

    /// 상시 노드(DHT·릴레이)를 실행 중인 노드에 더하고, 다시 띄울 때도 시작 옵션으로 넣습니다(사용자 설정이던
    /// `net.add_bootstrap`은 없어짐).
    pub fn add_bootstrap(&self, addr: &str) {
        let a: Multiaddr = addr.parse().expect("상시 노드 주소");
        {
            let mut list = self.bootstrap.lock().expect("잠금");
            if !list.contains(&a) {
                list.push(a.clone());
            }
        }
        yangpa_daemon::testkit::net::add_bootstrap(&self.node, vec![a]);
    }

    fn config(&self) -> NodeConfig {
        let mut cfg = NodeConfig::for_test(&self.dir);
        (self.tweak)(&mut cfg);
        for a in self.bootstrap.lock().expect("잠금").iter() {
            if !cfg.net.bootstrap.contains(a) {
                cfg.net.bootstrap.push(a.clone());
            }
        }
        cfg
    }

    pub async fn start_again(&mut self) {
        let mut cfg = self.config();
        cfg.net.listen = self.listen.clone();
        self.node = Node::start(cfg).await.expect("노드 다시 시작");
        track(&self.started, &self.node);
    }

    /// 멈춘 노드를 다른 포트로 다시 띄웁니다(다른 노드가 기록해 둔 주소가 낡은 상황).
    pub async fn start_on_new_port(&mut self) {
        let mut cfg = self.config();
        cfg.net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().expect("주소")];
        self.node = Node::start(cfg).await.expect("노드 다시 시작");
        track(&self.started, &self.node);
        self.listen = listen_addrs(&self.node).await;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.node.subscribe()
    }

    /// 1:1 공간의 첫 채널.
    pub async fn first_channel(&self, space: SpaceId) -> ChannelId {
        let chans = self.ok::<m::ChannelList>(SpaceParams { space }).await;
        chans.first().expect("채널 하나 이상").channel
    }

    pub async fn send(&self, space: SpaceId, channel: ChannelId, text: &str) -> EventId {
        self.ok::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: text.into(),
            attachments: Vec::new(),
            reply_to: None,
            thread: None,
        })
        .await
        .id
    }

    pub async fn messages(&self, space: SpaceId, channel: ChannelId) -> Vec<MessageView> {
        self.ok::<m::MessageList>(MessageListParams {
            space,
            channel,
            before: None,
            limit: Some(200),
            thread: None,
        })
        .await
        .messages
    }
}

type Started = Arc<Mutex<Vec<Weak<Node>>>>;

fn track(started: &Started, node: &Arc<Node>) {
    started.lock().expect("잠금").push(Arc::downgrade(node));
}

/// 시험 하나의 노드 묶음. 끝나면 노드를 멈추고 데이터 경로를 지웁니다.
pub struct Cluster {
    root: TempDir,
    started: Started,
}

impl Cluster {
    pub fn new() -> Self {
        init_tracing();
        Cluster {
            root: tempfile::tempdir().expect("임시 경로"),
            started: Started::default(),
        }
    }

    /// 신원 없는 노드.
    pub async fn bare(&self, name: &str) -> TestNode {
        self.bare_with(name, |_| {}).await
    }

    /// 설정을 바꾼 신원 없는 노드.
    pub async fn bare_with(&self, name: &str, tweak: fn(&mut NodeConfig)) -> TestNode {
        let dir = self.root.path().join(name);
        let mut cfg = NodeConfig::for_test(&dir);
        tweak(&mut cfg);
        let node = Node::start(cfg).await.expect("노드 시작");
        track(&self.started, &node);
        let listen = listen_addrs(&node).await;
        TestNode {
            node,
            dir,
            name: name.into(),
            listen,
            tweak,
            bootstrap: Mutex::new(Vec::new()),
            started: self.started.clone(),
        }
    }

    /// 신원을 만든 노드.
    pub async fn node(&self, name: &str) -> TestNode {
        self.node_with(name, |_| {}).await
    }

    /// 설정을 바꾼, 신원을 만든 노드(예: `push_max_devices = 0`으로 gossip 경로).
    pub async fn node_with(&self, name: &str, tweak: fn(&mut NodeConfig)) -> TestNode {
        let n = self.bare_with(name, tweak).await;
        n.ok::<m::IdentityCreate>(IdentityCreateParams { name: name.into() })
            .await;
        n
    }

    /// 데이터 경로(노드 없이 시작 실패를 볼 때).
    pub fn dir(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
}

impl Default for Cluster {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Cluster {
    /// 윈도에서는 SQLite가 연 DB 파일을 지울 수 없어, 노드 작업이 도는 채로 `TempDir`이 지우면 폴더가 남습니다(PC 러너의
    /// 임시 폴더에 수만 개). 노드를 멈추고 작업이 DB를 놓아 지워질 때까지 다시 지워 봅니다. 실패한 시험은 그대로 둡니다
    /// (남은 DB로 살펴봄, 되감는 중에 막히지 않게).
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // 단일 스레드 런타임에서는 여기서 기다릴 수 없어 폴더가 남습니다(presence 시험 셋과 뱃지 시험).
        if rt.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return;
        }
        let nodes: Vec<Arc<Node>> = std::mem::take(&mut *self.started.lock().expect("잠금"))
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let root = self.root.path();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let all = futures::future::join_all(nodes.iter().map(|n| n.shutdown()));
                let _ = tokio::time::timeout(Duration::from_secs(30), all).await;
            });
            drop(nodes);
            let deadline = Instant::now() + Duration::from_secs(10);
            while root.exists()
                && std::fs::remove_dir_all(root).is_err()
                && Instant::now() < deadline
            {
                std::thread::sleep(POLL);
            }
        });
    }
}

pub async fn start_node(dir: &Path) -> Arc<Node> {
    Node::start(NodeConfig::for_test(dir))
        .await
        .expect("노드 시작")
}

pub async fn call<M: Method>(node: &Arc<Node>, p: M::Params) -> Result<M::Result, RpcError> {
    let v = serde_json::to_value(&p).expect("매개변수 직렬화");
    let out = node.call(&CallCtx::owner(), M::NAME, v).await?;
    Ok(serde_json::from_value(out).expect("결과 형식"))
}

/// 조건이 `Some`을 낼 때까지 기다립니다.
pub async fn eventually<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout();
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "시간 초과: {what}");
        tokio::time::sleep(POLL).await;
    }
}

/// 조건에 맞는 알림이 올 때까지 기다립니다(다른 알림은 건너뜀).
pub async fn next_notification<T>(
    rx: &mut broadcast::Receiver<Notification>,
    what: &str,
    mut pick: impl FnMut(&Notification) -> Option<T>,
) -> T {
    let fut = async {
        loop {
            match rx.recv().await {
                Ok(n) => {
                    if let Some(v) = pick(&n) {
                        return v;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => panic!("알림 채널 닫힘: {what}"),
            }
        }
    };
    tokio::time::timeout(timeout(), fut)
        .await
        .unwrap_or_else(|_| panic!("알림 시간 초과: {what}"))
}

/// 수신 주소가 잡힐 때까지 기다립니다.
pub async fn listen_addrs(node: &Arc<Node>) -> Vec<Multiaddr> {
    eventually("수신 주소", || async {
        let v = node.listen_addrs().await;
        (!v.is_empty()).then_some(v)
    })
    .await
}

/// `a`가 `b`의 수신 주소로 직접 연결합니다(성공한 주소는 `a`의 노드 기록에 남아 재시작 때 다시 씀).
pub async fn connect(a: &TestNode, b: &TestNode) {
    let addrs = listen_addrs(&b.node).await;
    a.node
        .connect(b.peer(), addrs)
        .await
        .unwrap_or_else(|e| panic!("{} → {} 연결 실패: {e}", a.name, b.name));
}

/// 서로 연결합니다(양쪽 다 주소를 기록).
pub async fn connect_both(a: &TestNode, b: &TestNode) {
    connect(a, b).await;
    connect(b, a).await;
}

/// `a`가 링크를 만들고 `b`가 요청, `a`가 수락합니다. 양쪽이 같은 1:1 공간을 가질 때까지 기다려 돌려줍니다.
pub async fn befriend(a: &TestNode, b: &TestNode) -> SpaceId {
    let space = meet_by_link(a, b, Some(3600)).await;
    make_friends(a, b).await;
    space
}

/// `b`가 `a`의 연락처 링크를 써서 둘이 임시 연락처가 되고 1:1 공간에서 메시지를 보낼 수 있을 때까지 기다립니다.
pub async fn meet_by_link(a: &TestNode, b: &TestNode, expires_in_secs: Option<u64>) -> SpaceId {
    connect_both(a, b).await;
    let link = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs,
            single_use: Some(true),
        })
        .await;
    let added = b.ok::<m::FriendAdd>(UrlParams { url: link.url }).await;
    assert_eq!(added.user, a.user());
    let (a_user, b_user) = (a.user(), b.user());
    // 링크 주인은 수락 없이 바로 1:1 공간을 만듭니다.
    let space = eventually("링크 주인이 임시 연락처로 기록", || async {
        a.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .find(|f| f.user == b_user && f.kind == ContactKind::Temporary)
            .map(|f| f.space)
    })
    .await;
    eventually("링크를 쓴 쪽이 1:1 공간을 받음", || async {
        b.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .any(|f| f.user == a_user && f.space == space && f.kind == ContactKind::Temporary)
            .then_some(())
    })
    .await;
    // 링크를 쓴 쪽이 키 그룹에 들어가 메시지를 보낼 수 있을 때까지.
    eventually("링크를 쓴 쪽 크루 키 순번", || async {
        let ea = yangpa_daemon::testkit::crew_key_seq(&a.node, &space)?;
        let eb = yangpa_daemon::testkit::crew_key_seq(&b.node, &space)?;
        (ea == eb).then_some(())
    })
    .await;
    space
}

/// 임시 연락처인 두 사람이 서로 친구 추가를 눌러 친구가 될 때까지 기다립니다.
pub async fn make_friends(a: &TestNode, b: &TestNode) {
    let (a_user, b_user) = (a.user(), b.user());
    a.ok::<m::FriendPropose>(UserParams { user: b_user }).await;
    b.ok::<m::FriendPropose>(UserParams { user: a_user }).await;
    for (x, other) in [(a, b_user), (b, a_user)] {
        eventually("둘 다 친구", || async {
            x.ok::<m::FriendList>(Empty)
                .await
                .into_iter()
                .any(|f| f.user == other && f.kind == ContactKind::Friend)
                .then_some(())
        })
        .await;
    }
}

/// 노드 `n`의 채널 메시지 중 `id`가 조건을 만족할 때까지 기다립니다.
pub async fn wait_message(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    id: EventId,
    what: &str,
    pred: impl Fn(&MessageView) -> bool,
) -> MessageView {
    eventually(what, || async {
        n.messages(space, channel)
            .await
            .into_iter()
            .find(|v| v.id == id && pred(v))
    })
    .await
}

/// 내 새 기기가 `owner`(내 다른 기기)의 목록에 활성으로 오를 때까지 기다립니다.
pub async fn wait_own_device(owner: &TestNode, device: DeviceId) {
    eventually("내 다른 기기가 새 기기를 받아들임", || async {
        owner
            .ok::<m::DeviceList>(Empty)
            .await
            .iter()
            .any(|d| d.entry.device == device && d.entry.revoked_at.is_none())
            .then_some(())
    })
    .await;
}

/// 살아 있는 `old`가 띄운 링크로 신원이 없는 `new`를 들입니다(두 화면의 번호를 보고 승인). 시험망의 링크에는 주소가 없어
/// 둘을 손으로 잇습니다.
pub async fn link_device_with(old: &TestNode, new: &TestNode) {
    let offer = old.ok::<m::DeviceLinkCreate>(Empty).await;
    connect(new, old).await;
    let joined = new
        .ok::<m::DeviceLinkJoin>(DeviceLinkJoinParams { url: offer.url })
        .await;
    let request = eventually("승인을 기다리는 요청", || async {
        old.ok::<m::DeviceLinkPending>(Empty).await
    })
    .await;
    assert_eq!(request.code, joined.code, "두 화면의 번호가 같음");
    old.ok::<m::DeviceLinkDecide>(DeviceLinkDecideParams { approve: true })
        .await;
    eventually("새 기기가 신원을 받음", || async {
        new.call::<m::IdentityGet>(Empty).await.ok()
    })
    .await;
}

/// `old`의 승인으로 같은 신원의 새 기기를 띄웁니다.
pub async fn linked(
    cl: &Cluster,
    old: &TestNode,
    name: &str,
    tweak: fn(&mut NodeConfig),
) -> TestNode {
    let n = cl.bare_with(name, tweak).await;
    link_device_with(old, &n).await;
    n
}
