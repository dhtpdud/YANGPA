//! Node: 저장소·네트워크·키 그룹·공간 상태를 묶는 중심. 테스트는 한 프로세스에 여러 개를 띄웁니다.
//!
//! 다른 작업 영역의 노드 수준 상태는 `crews`, `volatile`, `blobs` … 필드에 둡니다(각 모듈의 `Default` 구조체).
//! 새 필드가 필요하면 그 모듈의 상태 구조체에 넣으면 되고, 이 파일은 고치지 않아도 됩니다.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info};
use yangpa_core::ids::{SpaceId, UserId};
use yangpa_core::model::SpaceState;
use yangpa_net::{Multiaddr, NetEvent, NetHandle, PeerId};
use yangpa_rpc::api::Notification;
use yangpa_rpc::{EventHub, RpcError};
use yangpa_store::{Store, StoreOptions};

use crate::api::{self, CallCtx};
use crate::config::NodeConfig;
use crate::error::{Error, Result};
use crate::identity::{self, Me};
use crate::notify::Notifier;
use crate::peers::PeerDirectory;
use crate::spaces::SpaceRuntime;
use crate::{
    admin, archive, blobs, crews, diag, friends, inbound, keygroup, locator, moderation, p2p,
    peers, policy, spaces, sync_service, tor, trust, voice, volatile,
};

/// 노드가 띄운 작업 묶음(오래 도는 작업, 짧은 백그라운드 작업, 들어온 요청 처리). 종료 때 한꺼번에 멈추고,
/// 그 뒤로는 새 작업을 띄우지 않습니다(종료한 노드가 같은 DB에 늦게 쓰지 않게).
#[derive(Default)]
pub struct TaskSet {
    handles: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl TaskSet {
    pub fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        // 닫기와 엇갈리지 않게 목록 잠금 안에서 확인하고 띄웁니다.
        let mut v = self.handles.lock();
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        v.retain(|h| !h.is_finished());
        v.push(tokio::spawn(fut));
    }

    fn abort_all(&self) {
        let mut v = self.handles.lock();
        self.closed.store(true, Ordering::Release);
        for h in v.drain(..) {
            h.abort();
        }
    }
}

pub struct Node {
    pub(crate) cfg: NodeConfig,
    pub(crate) store: Arc<Store>,
    pub(crate) net: NetHandle,
    pub(crate) me: RwLock<Option<Arc<Me>>>,
    pub(crate) spaces: RwLock<HashMap<SpaceId, SpaceRuntime>>,
    pub(crate) peers: PeerDirectory,
    pub(crate) notify: Notifier,
    pub(crate) tasks: TaskSet,
    /// 이벤트 작성·적용의 관문. 공간 하나만 다루는 쪽은 읽기로, 여러 공간을 다루는 쪽은 쓰기로 쥡니다
    /// (`ingest::lock_space`·`ingest::lock`, 잠금 순서는 `ingest::Locked`). 종료하면 쓰기로 쥔 채로 두어 그 뒤의
    /// 작성·적용을 막습니다(`shutdown_guard`).
    pub(crate) ingest_gate: Arc<tokio::sync::RwLock<()>>,
    /// 공간마다의 적용 잠금: 한 공간 안의 작성·적용을 한 줄로 세웁니다(부모·논리 시계·해소기·키 그룹 순서가 엇갈리지
    /// 않게). 다른 공간은 서로 기다리지 않습니다. 쥐거나 기다리는 쪽이 없는 잠금은 남기지 않습니다(약한 참조: 지운
    /// 공간·받기만 해 본 공간 ID가 쌓이지 않게).
    pub(crate) ingest_spaces: Mutex<HashMap<SpaceId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    shutdown_guard: Mutex<Option<tokio::sync::OwnedRwLockWriteGuard<()>>>,
    /// 기기 키 표(키 약속으로 찾는 상자 키·서명 키, DB 앞의 LRU).
    pub(crate) devkeys: crate::devkeys::DeviceKeyTable,
    /// 신원 설치(만들기·기기 연결)를 한 줄로. 두 호출의 비밀값이 섞여 저장되지 않게.
    pub(crate) identity_lock: Mutex<()>,
    pub(crate) identity: identity::IdentityState,
    pub(crate) devlink: crate::devlink::DeviceLinkState,
    pub(crate) owner_token_hash: Mutex<Option<[u8; 32]>>,
    /// 지금 보내는 중인 친구 요청(`friends`)과, 그동안 다른 쪽이 다시 보내려 했는지. 같은 것을 겹쳐 보내지 않게.
    pub(crate) friend_sending: Mutex<HashMap<[u8; 16], bool>>,
    /// 지금 보내는 중인 친구 수락(1:1 공간별)과, 그동안 다른 쪽이 다시 보내려 했는지. 같은 수락을 겹쳐 보내지 않게.
    pub(crate) accept_sending: Mutex<HashMap<SpaceId, bool>>,
    /// 시험용: 친구 수락을 보내지 않고 붙잡아 둠(`testkit::hold_friend_accepts`).
    pub(crate) accept_hold: std::sync::atomic::AtomicBool,
    /// 받아들이는 중인 연락처 링크 요청(사용자, 링크 해시). 요청을 가져간 뒤 공간을 만드는 동안 같은 사람이 같은
    /// 링크로 다시 보내면 1회용 링크를 다시 쓰지 않고 기다리게 합니다.
    pub(crate) contact_accepting: Mutex<HashSet<(UserId, [u8; 32])>>,
    pub(crate) sync: sync_service::SyncState,
    /// 상태 체크포인트(내기·받기·가입).
    pub(crate) checkpoints: crate::checkpoint::CheckpointState,
    /// 키 그룹(확정 기기 표·묶음 창·지금 크루 상태 키, `keygroup`).
    pub(crate) keygroup: keygroup::KeyGroupState,
    shutdown_tx: watch::Sender<bool>,
    /// 종료가 끝났는지(동시에 부른 쪽들이 함께 기다림).
    shutdown_done: watch::Sender<bool>,
    pub(crate) crews: crews::CrewState,
    pub(crate) volatile: volatile::VolatileState,
    pub(crate) blobs: blobs::BlobState,
    pub(crate) policy: policy::PolicyState,
    pub(crate) voice: voice::VoiceState,
    pub(crate) admin: admin::AdminState,
    pub(crate) moderation: moderation::ModerationState,
    pub(crate) tor: tor::TorState,
    pub(crate) locator: locator::LocatorState,
    pub(crate) diag: diag::DiagState,
    pub(crate) archive: archive::ArchiveState,
    pub(crate) p2p: p2p::P2pState,
    pub(crate) trust: trust::TrustState,
    pub(crate) filelink: crate::filelink::FileLinkState,
    pub(crate) transfer: crate::transfer::TransferState,
    pub(crate) ratelimit: crate::ratelimit::RateLimitState,
    pub(crate) typing: crate::typing::TypingState,
    /// 붙여넣은 첨부(RAM, `blob.paste`).
    pub(crate) pastes: crate::paste::Pastes,
    /// 후원자 확인(진행 중인 로그인, `supporter`).
    pub(crate) supporter: crate::supporter::SupporterState,
    /// 표시 상태와 상대 기기의 상태(`presence`).
    pub(crate) presence: crate::presence::PresenceState,
    /// 도전과제 뱃지(모으는 중인 시딩 바이트, `badges`).
    pub(crate) badges: crate::badges::BadgeState,
    /// 프로필·뱃지 카드 싣기와 받아오기(`cards`).
    pub(crate) cards: crate::cards::CardState,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("peer_id", &self.net.local_peer_id())
            .finish_non_exhaustive()
    }
}

impl Node {
    /// 노드를 띄웁니다. 저장소를 열고(없으면 만듦), 네트워크를 시작하고, 저장된 공간 상태를 다시 세웁니다.
    pub async fn start(cfg: NodeConfig) -> Result<Arc<Node>> {
        Self::start_with_hub(cfg, EventHub::new()).await
    }

    /// RPC 서버와 같은 [`EventHub`]를 쓰도록 지정해 띄웁니다.
    pub async fn start_with_hub(cfg: NodeConfig, hub: EventHub) -> Result<Arc<Node>> {
        yangpa_rpc::endpoint::create_private_dir(&cfg.data_dir)?;
        let db_key = identity::load_or_create_db_key(&cfg)?;
        let opts = StoreOptions {
            memory_security: cfg.store_memory_security,
            audit_cap: cfg.audit_caps.0,
            audit_important_cap: cfg.audit_caps.1,
            ..StoreOptions::default()
        };
        let store = Arc::new(Store::open_with(&cfg.data_dir, &db_key, opts)?);
        drop(db_key);

        // Tor 설정(B4). Tor를 지원하지 않으면 여기서 시작을 멈춥니다.
        let ncfg = tor::net_config(&cfg, &store)?;
        // 오프라인 모드면 스웜·Arti 없이 핸들만 둡니다(`presence`).
        let offline = crate::presence::offline_at_start(&store);
        let (net, events) = if offline {
            info!("오프라인 모드로 시작(망을 띄우지 않음)");
            yangpa_net::spawn_offline(&ncfg)
        } else {
            yangpa_net::spawn(ncfg)?
        };

        identity::forget_old_otp(&store);
        let me = identity::load_me(&store)?.map(Arc::new);
        let (shutdown_tx, _) = watch::channel(false);
        let (shutdown_done, _) = watch::channel(false);
        let node = Arc::new(Node {
            cfg,
            store,
            net,
            me: RwLock::new(me),
            spaces: RwLock::new(HashMap::new()),
            peers: PeerDirectory::default(),
            notify: Notifier::new(hub),
            tasks: TaskSet::default(),
            ingest_gate: Arc::new(tokio::sync::RwLock::new(())),
            ingest_spaces: Mutex::new(HashMap::new()),
            shutdown_guard: Mutex::new(None),
            devkeys: Default::default(),
            identity_lock: Mutex::new(()),
            identity: identity::IdentityState::default(),
            devlink: Default::default(),
            owner_token_hash: Mutex::new(None),
            friend_sending: Mutex::new(HashMap::new()),
            accept_sending: Mutex::new(HashMap::new()),
            accept_hold: std::sync::atomic::AtomicBool::new(false),
            contact_accepting: Mutex::new(HashSet::new()),
            sync: sync_service::SyncState::default(),
            checkpoints: Default::default(),
            keygroup: keygroup::KeyGroupState::default(),
            shutdown_tx,
            shutdown_done,
            crews: Default::default(),
            volatile: Default::default(),
            blobs: Default::default(),
            policy: Default::default(),
            voice: Default::default(),
            admin: Default::default(),
            moderation: Default::default(),
            tor: Default::default(),
            locator: Default::default(),
            diag: Default::default(),
            archive: Default::default(),
            p2p: Default::default(),
            trust: Default::default(),
            filelink: Default::default(),
            transfer: Default::default(),
            ratelimit: Default::default(),
            typing: Default::default(),
            pastes: Default::default(),
            supporter: Default::default(),
            presence: Default::default(),
            badges: Default::default(),
            cards: Default::default(),
        });

        if node.cfg.owner_token_file {
            api::init_owner_token(&node)?;
        }
        // 다른 시작 고리(P2P 스웜·후원 확인)가 오프라인 모드를 보도록 먼저.
        crate::presence::start(&node, offline);
        spaces::load_all(&node).await?;
        // 다른 작업 영역의 시작 고리(저장된 공간을 다시 세운 뒤, 네트워크 이벤트를 받기 전).
        crews::start(&node);
        volatile::start(&node);
        blobs::start(&node);
        policy::start(&node);
        voice::start(&node);
        admin::start(&node);
        moderation::start(&node);
        locator::start(&node);
        diag::start(&node);
        archive::start(&node);
        p2p::start(&node);
        crate::bridges::start(&node);
        crate::transfer::start(&node);
        crate::checkpoint::start(&node);
        crate::supporter::start(&node);
        crate::badges::start(&node);
        crate::cards::start(&node);
        node.spawn_net_loop(events);
        sync_service::spawn_periodic(&node);
        peers::spawn_redial(&node);
        peers::reconnect_known(&node);
        friends::spawn_contact_expiry(&node);
        friends::spawn_cut_sweep(&node);
        crate::identity::spawn_own_share(&node);
        crate::identity::spawn_tidy_own_list(&node);
        info!(peer = %node.net.local_peer_id(), "노드 시작");
        Ok(node)
    }

    /// 노드를 멈춥니다. 작업을 멈추고 네트워크를 닫은 뒤, 진행 중인 적용이 끝나면 적용 잠금을 쥔 채 WAL을 정리합니다.
    ///
    /// 여러 곳에서 동시에 불러도 종료는 한 번만 하고, 모두 종료가 끝날 때까지 기다립니다. 종료 작업은 따로 띄우므로
    /// 부른 작업이 취소돼도 중간에 끊기지 않습니다.
    pub async fn shutdown(self: &Arc<Self>) {
        let mut done = self.shutdown_done.subscribe();
        if !self.shutdown_tx.send_replace(true) {
            let node = self.clone();
            tokio::spawn(async move {
                node.shutdown_inner().await;
                node.shutdown_done.send_replace(true);
            });
        }
        let _ = done.wait_for(|d| *d).await;
    }

    async fn shutdown_inner(&self) {
        info!("노드 종료");
        self.tasks.abort_all();
        // 작업을 먼저 멈춥니다(시각을 쓰는 동안 끊을 요청이 더 나가지 않게).
        crate::presence::on_shutdown(self);
        crate::badges::flush(self);
        p2p::shutdown(self);
        crate::filelink::shutdown(self);
        self.net.shutdown();
        // 스웜 작업이 끝나 수신 소켓이 닫힐 때까지 잠깐 기다립니다(같은 프로세스·같은 포트로 다시 띄울 수 있게).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !self.net.is_closed() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // 진행 중인 작성·적용이 끝나기를 기다립니다(중간에 끊으면 키 그룹 상태가 이벤트와 어긋날 수 있음).
        // 잠금은 돌려주지 않습니다: 종료한 노드가 그 뒤에 이벤트·키 상태를 쓰지 않게(같은 DB로 다시 띄울 때).
        let g = self.ingest_gate.clone().write_owned().await;
        // 적용 잠금을 쥔 뒤라 더 들어올 휘발성 이벤트가 없습니다. 작업 중단과 경쟁하지 않게 여기서 지웁니다.
        crate::volatile::wipe_all(self);
        if let Err(e) = self.store.checkpoint() {
            debug!(error = %e, "종료 체크포인트 실패");
        }
        *self.shutdown_guard.lock() = Some(g);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown_tx.borrow()
    }

    /// 오프라인 모드로 들어가거나 나오느라 멈췄음: 데몬을 같은 인자로 다시 띄워야 합니다(main.rs).
    pub fn restart_requested(&self) -> bool {
        crate::presence::restart_requested(self)
    }

    /// 종료 신호(셋이 되면 끝).
    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// 로컬 API 호출(프로세스 안). 서버가 하는 범위·사람 전용·공간 제한 검사도 똑같이 합니다.
    pub async fn call(
        self: &Arc<Self>,
        ctx: &CallCtx,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcError> {
        api::call_in_process(self, ctx, method, params).await
    }

    /// 프로세스 안 알림 구독(테스트용). 로컬 API 구독자와 같은 알림을 받습니다.
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notify.subscribe()
    }

    pub fn event_hub(&self) -> EventHub {
        self.notify.hub().clone()
    }

    pub fn peer_id(&self) -> PeerId {
        self.net.local_peer_id()
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.cfg.data_dir
    }

    /// 실제 수신 주소(`/p2p/<id>` 없이). 이 기기 진단·테스트용이며 링크에 넣지 않습니다.
    pub async fn listen_addrs(&self) -> Vec<Multiaddr> {
        self.net.status().await.listen_addrs
    }

    /// 알려진 주소로 피어에 연결하고, 성공하면 주소를 재시작 때 다시 쓰도록 기록합니다.
    pub async fn connect(&self, peer: PeerId, addrs: Vec<Multiaddr>) -> Result<()> {
        peers::connect(self, peer, addrs).await
    }

    pub(crate) fn me(&self) -> Result<Arc<Me>> {
        self.me.read().clone().ok_or(Error::NoIdentity)
    }

    /// 공간의 해소된 상태(사본). 모르는 공간이거나 아직 해소 전이면 `None`.
    pub(crate) fn space_state(&self, space: &SpaceId) -> Option<Arc<SpaceState>> {
        self.spaces.read().get(space).and_then(|r| r.state.clone())
    }

    pub(crate) fn space_ids(&self) -> Vec<SpaceId> {
        self.spaces.read().keys().copied().collect()
    }

    fn spawn_net_loop(self: &Arc<Self>, mut events: mpsc::Receiver<NetEvent>) {
        let weak = Arc::downgrade(self);
        self.tasks.spawn(async move {
            while let Some(ev) = events.recv().await {
                let Some(node) = weak.upgrade() else { break };
                if node.is_shutting_down() {
                    break;
                }
                match ev {
                    NetEvent::InboundRequest { peer, req, reply } => {
                        let n = node.clone();
                        node.tasks.spawn(async move {
                            let resp = inbound::handle(&n, peer, req).await;
                            let _ = reply.send(resp);
                        });
                    }
                    NetEvent::Gossip {
                        from,
                        msg_id,
                        topic,
                        msg,
                    } => {
                        let n = node.clone();
                        node.tasks.spawn(async move {
                            sync_service::on_gossip(&n, from, msg_id, topic, msg).await;
                        });
                    }
                    NetEvent::Media { peer, frames } => {
                        crate::voice::media::on_frames(&node, peer, frames);
                    }
                    NetEvent::PeerConnected(p) => peers::on_connected(&node, p),
                    NetEvent::PeerDisconnected(p) => {
                        crate::filelink::on_disconnected(&node, &p);
                        peers::on_disconnected(&node, p);
                    }
                    NetEvent::Status(s) => {
                        peers::on_status(&node, &s);
                        node.notify
                            .publish(Notification::NetStatus(api::system::net_status_for(
                                &node, &s,
                            )));
                    }
                }
            }
        });
    }

    /// 짧은 백그라운드 작업. 노드가 종료 중이면 띄우지 않고, 종료할 때 함께 멈춥니다.
    pub(crate) fn spawn_detached<F>(self: &Arc<Self>, f: impl FnOnce(Arc<Node>) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if self.is_shutting_down() {
            return;
        }
        self.tasks.spawn(f(self.clone()));
    }
}
