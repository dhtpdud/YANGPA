//! 성능 기준 작업 묶음.
//!
//! 작업마다 준비(`setup`)와 반복(`run(n)`)을 나눕니다. 준비는 `n`과 무관하게 늘 같은 일을 하므로, 같은 작업을
//! 반복 수만 달리해 두 번 재고 그 차이를 반복 수 차이로 나누면 준비 비용이 빠진 한 번당 값이 나옵니다
//! (`yangpa-bench measure`가 valgrind cachegrind의 명령 수로 이렇게 잽니다, docs/performance.md).
//!
//! 무작위 키·nonce를 쓰는 곳이 있어 명령 수가 완전히 같지는 않지만, 흔들림은 1% 안쪽입니다(측정값 참고).

pub mod scale;

use std::hint::black_box;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use yangpa_core::body::{EventBody, MessageBody, Secret32};
use yangpa_core::device::DeviceSignKeys;
use yangpa_core::event::{EVENT_VERSION, EventHeader, EventKind, KeyRef, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::SecurityLevel;
use yangpa_core::sign::SigningKeypair;
use yangpa_core::time::Timestamp;
use yangpa_core::wire::LamportRange;
use yangpa_crypto::BoxKeypair;
use yangpa_crypto::keytree::{CommitCtx, GroupKind, KeyTree, MemberCtx, MemberKeys};
use yangpa_store::{PersistentEvent, Store};
use yangpa_sync::reconcile::{
    MemorySet, ReconcileRequest, ReconcileResponse, Reconciler, answer_fingerprints, answer_ids,
    full_range, split_range,
};

/// 준비한 자료로 버틸 수 있는 최대 반복 수(미리 만들어 두는 작업).
pub const MAX_ITERS: u64 = 64;

/// 한 작업의 반복.
pub trait Workload {
    fn run(&mut self, n: u64) -> Result<()>;
}

/// 작업 설명.
pub struct Spec {
    pub name: &'static str,
    pub about: &'static str,
    /// 기준선 대비 허용 증가율(0.10 = 10%). `gate --threshold`를 주면 모든 작업에 그 값을 씁니다.
    pub tolerance: f64,
    /// 시간을 잴 때 기본 반복 수.
    pub time_iters: u64,
    pub setup: fn() -> Result<Box<dyn Workload>>,
}

/// 모든 작업.
pub fn specs() -> Vec<Spec> {
    vec![
        Spec {
            name: "aead_seal_open_1k",
            about: "XChaCha20-Poly1305 봉인·열기(1KiB, 추가 인증 자료 포함)",
            tolerance: 0.05,
            time_iters: 20_000,
            setup: aead_setup,
        },
        Spec {
            name: "sealed_box_8",
            about: "기기 8대 앞 봉인 상자 만들기 + 한 대가 열기(1KiB)",
            tolerance: 0.05,
            time_iters: 2_000,
            setup: sealed_setup,
        },
        Spec {
            name: "event_sign_verify",
            about: "이벤트 서명 + 검증(본문 1KiB)",
            tolerance: 0.05,
            time_iters: 5_000,
            setup: sign_setup,
        },
        Spec {
            name: "body_encrypt_decrypt",
            about: "메시지 본문 암호화 + 복호화(패딩·직렬화 포함)",
            tolerance: 0.05,
            time_iters: 20_000,
            setup: body_setup,
        },
        Spec {
            name: "keygroup_refresh_8",
            about: "기기 8대 키 그룹(직접 인코딩)의 시간 회전 만들기 + 다른 기기가 적용",
            tolerance: 0.10,
            time_iters: 200,
            setup: keygroup_setup,
        },
        Spec {
            name: "store_insert_100",
            about: "이벤트 100개 묶음 저장(SQLCipher, 트랜잭션 하나)",
            tolerance: 0.10,
            time_iters: 40,
            setup: insert_setup,
        },
        Spec {
            name: "store_fingerprints_5k",
            about: "이벤트 5천 개 스트림의 구간 지문 16개(전 구간)",
            tolerance: 0.10,
            time_iters: 200,
            setup: fingerprint_setup,
        },
        Spec {
            name: "reconcile_10k_diff_50",
            about: "1만 개 중 50개가 다른 두 집합의 지문 비교 끝까지",
            tolerance: 0.05,
            time_iters: 200,
            setup: reconcile_setup,
        },
        Spec {
            name: "ingest_batch_20",
            about: "데몬 적용 경로: 다른 멤버가 쓴 메시지 20개 묶음 받기(검증·복호화·색인·저장)",
            tolerance: 0.10,
            time_iters: INGEST_BATCHES,
            setup: ingest_setup,
        },
    ]
}

pub fn find(name: &str) -> Option<Spec> {
    specs().into_iter().find(|s| s.name == name)
}

/// 미리 만든 자료를 `used`개 쓴 뒤 `n`번 더 할 수 있는지.
fn check_iters(used: usize, n: u64) -> Result<()> {
    if used as u64 + n > MAX_ITERS {
        return Err(anyhow!("이 작업은 모두 {MAX_ITERS}번까지 반복합니다"));
    }
    Ok(())
}

fn fixed_key(b: u8) -> Secret32 {
    Secret32([b; 32])
}

fn header(
    author: &SigningKeypair,
    space: SpaceId,
    channel: ChannelId,
    parents: Vec<EventId>,
    lamport: u64,
) -> EventHeader {
    EventHeader {
        v: EVENT_VERSION,
        space,
        channel: Some(channel),
        kind: EventKind::Message,
        author: DeviceId(author.public()),
        user: UserId([9; 32]),
        parents,
        lamport,
        created_at: Timestamp(1_700_000_000_000 + lamport),
        key: KeyRef::Record {
            root: ChannelRootId([3; 16]),
        },
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    }
}

// ───────────── 암호 ─────────────

struct Aead {
    key: Secret32,
    pt: Vec<u8>,
}

impl Workload for Aead {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            let ct = yangpa_crypto::seal(&self.key, b"yangpa/bench", &self.pt);
            let pt = yangpa_crypto::open(&self.key, b"yangpa/bench", &ct)?;
            black_box(pt);
        }
        Ok(())
    }
}

fn aead_setup() -> Result<Box<dyn Workload>> {
    Ok(Box::new(Aead {
        key: fixed_key(1),
        pt: vec![0x5a; 1024],
    }))
}

struct Sealed {
    recipients: Vec<(DeviceId, yangpa_core::device::BoxPublicKey)>,
    me: DeviceId,
    kp: BoxKeypair,
    pt: Vec<u8>,
}

impl Workload for Sealed {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            let env = yangpa_crypto::seal_to(&self.recipients, b"ad", &self.pt);
            let pt = yangpa_crypto::open_sealed(&self.me, &self.kp, b"ad", &env)?;
            black_box(pt);
        }
        Ok(())
    }
}

fn sealed_setup() -> Result<Box<dyn Workload>> {
    let kps: Vec<BoxKeypair> = (0..8u8)
        .map(|i| BoxKeypair::from_bytes([i + 1; 32]))
        .collect();
    let recipients: Vec<(DeviceId, yangpa_core::device::BoxPublicKey)> = kps
        .iter()
        .enumerate()
        .map(|(i, k)| (DeviceId([i as u8; 32]), k.public()))
        .collect();
    Ok(Box::new(Sealed {
        me: recipients[3].0,
        kp: kps[3].clone(),
        recipients,
        pt: vec![0x33; 1024],
    }))
}

struct Sign {
    kp: SigningKeypair,
    keys: DeviceSignKeys,
    header: EventHeader,
    body: Vec<u8>,
}

impl Workload for Sign {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            let ev = SignedEvent::sign(self.header.clone(), self.body.clone(), &self.kp)?;
            // 채널 이벤트는 짧은 서명이라 받는 쪽처럼 상태에서 찾은 작성 기기 키로 확인합니다.
            ev.verify_with(&self.keys)?;
            black_box(ev);
        }
        Ok(())
    }
}

fn sign_setup() -> Result<Box<dyn Workload>> {
    let kp = SigningKeypair::from_seed(&[7; 32]);
    let header = header(&kp, SpaceId([1; 16]), ChannelId([2; 16]), Vec::new(), 1);
    Ok(Box::new(Sign {
        keys: DeviceSignKeys::of(&kp),
        kp,
        header,
        body: vec![0x11; 1024],
    }))
}

struct Body {
    key: Secret32,
    header: EventHeader,
    body: EventBody,
}

impl Workload for Body {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            let ct = yangpa_crypto::encrypt_body(&self.key, &self.header, &self.body)?;
            let pt = yangpa_crypto::decrypt_body(&self.key, &self.header, &ct)?;
            black_box(pt);
        }
        Ok(())
    }
}

fn body_setup() -> Result<Box<dyn Workload>> {
    let kp = SigningKeypair::from_seed(&[8; 32]);
    Ok(Box::new(Body {
        key: fixed_key(2),
        header: header(&kp, SpaceId([1; 16]), ChannelId([2; 16]), Vec::new(), 1),
        body: EventBody::Message(MessageBody {
            text: "양파 메신저 성능 기준 메시지입니다. ".repeat(8),
            ..Default::default()
        }),
    }))
}

struct KeyGroup {
    tree: KeyTree,
    boxes: std::collections::BTreeMap<DeviceId, yangpa_core::device::BoxPublicKey>,
    member: MemberKeys,
    kp: BoxKeypair,
}

impl Workload for KeyGroup {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            let cx = CommitCtx {
                boxes: &self.boxes,
                crew: None,
            };
            let out = self.tree.refresh_root(&cx)?;
            let mcx = MemberCtx {
                box_keys: Some(&self.kp),
                ..Default::default()
            };
            self.member = self.member.apply(&out.payload, &mcx)?;
            if self.member.root_commit() != self.tree.root_commit() {
                return Err(anyhow!("키 그룹 뿌리 약속이 어긋남"));
            }
        }
        Ok(())
    }
}

/// 확정 기기와 멤버 7대의 크루 그룹(직접 인코딩). 회전 하나는 새 T를 8대에게 봉인(X-Wing 8번)하고 멤버가 엽니다.
fn keygroup_setup() -> Result<Box<dyn Workload>> {
    let space = SpaceId([3; 16]);
    let kps: Vec<BoxKeypair> = (0..8u8)
        .map(|i| BoxKeypair::from_bytes([i + 10; 32]))
        .collect();
    let devices: Vec<DeviceId> = (0..8u8).map(|i| DeviceId([i + 1; 32])).collect();
    let boxes = devices
        .iter()
        .zip(&kps)
        .map(|(d, k)| (*d, k.public()))
        .collect();
    let mut tree = KeyTree::crew(space);
    let cx = CommitCtx {
        boxes: &boxes,
        crew: None,
    };
    let first = tree.update(&[], &devices, &cx)?;
    let mcx = MemberCtx {
        box_keys: Some(&kps[3]),
        ..Default::default()
    };
    let member = MemberKeys::join(
        space,
        *tree.group(),
        GroupKind::Crew,
        devices[3],
        &first.payload,
        &mcx,
    )?;
    let kp = kps[3].clone();
    Ok(Box::new(KeyGroup {
        tree,
        boxes,
        member,
        kp,
    }))
}

// ───────────── 저장소 ─────────────

/// 한 저자가 한 채널에 이어 쓴 메시지 `count`개(부모 사슬).
fn chain(
    kp: &SigningKeypair,
    space: SpaceId,
    channel: ChannelId,
    count: usize,
) -> Result<Vec<SignedEvent>> {
    let mut out: Vec<SignedEvent> = Vec::with_capacity(count);
    for i in 0..count {
        let parents = out.last().map(|e| vec![e.id()]).unwrap_or_default();
        let body = (i as u32).to_le_bytes().repeat(64);
        out.push(SignedEvent::sign(
            header(kp, space, channel, parents, i as u64 + 1),
            body,
            kp,
        )?);
    }
    Ok(out)
}

struct Insert {
    store: Store,
    _dir: tempfile::TempDir,
    batches: Vec<Vec<PersistentEvent>>,
    next: usize,
}

impl Workload for Insert {
    fn run(&mut self, n: u64) -> Result<()> {
        check_iters(self.next, n)?;
        for _ in 0..n {
            let batch = &self.batches[self.next];
            self.next += 1;
            black_box(self.store.insert_events(batch)?);
        }
        Ok(())
    }
}

const INSERT_BATCH: usize = 100;

fn insert_setup() -> Result<Box<dyn Workload>> {
    let (store, dir) = Store::open_temp()?;
    let kp = SigningKeypair::from_seed(&[5; 32]);
    let events = chain(
        &kp,
        SpaceId([4; 16]),
        ChannelId([5; 16]),
        INSERT_BATCH * MAX_ITERS as usize,
    )?;
    let batches = events
        .chunks(INSERT_BATCH)
        .map(|c| {
            c.iter()
                .map(|e| PersistentEvent::new(e.clone(), SecurityLevel::Normal))
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Box::new(Insert {
        store,
        _dir: dir,
        batches,
        next: 0,
    }))
}

struct Fingerprints {
    store: Store,
    _dir: tempfile::TempDir,
    space: SpaceId,
    stream: StreamId,
    ranges: Vec<LamportRange>,
}

impl Workload for Fingerprints {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            black_box(
                self.store
                    .fingerprints(&self.space, self.stream, &self.ranges)?,
            );
        }
        Ok(())
    }
}

fn fingerprint_setup() -> Result<Box<dyn Workload>> {
    const COUNT: usize = 5_000;
    let (store, dir) = Store::open_temp()?;
    let kp = SigningKeypair::from_seed(&[6; 32]);
    let (space, channel) = (SpaceId([6; 16]), ChannelId([6; 16]));
    let events = chain(&kp, space, channel, COUNT)?;
    for c in events.chunks(500) {
        let batch = c
            .iter()
            .map(|e| PersistentEvent::new(e.clone(), SecurityLevel::Normal))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        store.insert_events(&batch)?;
    }
    let mut w = Fingerprints {
        store,
        _dir: dir,
        space,
        stream: StreamId::Channel(channel),
        ranges: split_range(full_range(COUNT as u64), 16),
    };
    // 저장소는 읽기 연결(기본 3개)을 돌아가며 쓰고 연결마다 페이지·문장 캐시가 따로라 연결마다 첫 조회가 더 듭니다.
    // 데워 두어 늘 하는 조회의 값을 잽니다(재는 구간에 찬 연결이 끼면 한 번당 값이 높게 나옴).
    w.run(8)?;
    Ok(Box::new(w))
}

// ───────────── 동기화 ─────────────

struct Reconcile {
    local: MemorySet,
    remote: MemorySet,
}

impl Workload for Reconcile {
    fn run(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            let max = self.local.max_lamport().max(self.remote.max_lamport());
            let mut r = Reconciler::new(full_range(max));
            let mut found = 0usize;
            while let Some((token, req)) = r.next_request() {
                match req {
                    ReconcileRequest::Fingerprints(ranges) => {
                        let fps = answer_fingerprints(&self.remote, &ranges)
                            .map_err(|e| anyhow!("{e:?}"))?;
                        r.on_response(&self.local, token, ReconcileResponse::Fingerprints(&fps))
                            .map_err(|e| anyhow!("{e:?}"))?;
                    }
                    ReconcileRequest::Ids { range, limit } => {
                        let ids =
                            answer_ids(&self.remote, range, limit).map_err(|e| anyhow!("{e:?}"))?;
                        r.on_response(&self.local, token, ReconcileResponse::Ids(&ids))
                            .map_err(|e| anyhow!("{e:?}"))?;
                    }
                }
                found += r.take_fetch().len();
            }
            if found != 50 {
                return Err(anyhow!("찾은 차이 {found}개(50개여야 함)"));
            }
        }
        Ok(())
    }
}

fn reconcile_setup() -> Result<Box<dyn Workload>> {
    let id = |n: u64| EventId::hash_of(&n.to_le_bytes());
    let mut local = MemorySet::new();
    let mut remote = MemorySet::new();
    for n in 0..10_000u64 {
        // 50개 중 하나꼴로 상대에게만 있음(흩어진 차이).
        if n % 200 != 7 {
            local.insert(n / 3 + 1, id(n));
        }
        remote.insert(n / 3 + 1, id(n));
    }
    Ok(Box::new(Reconcile { local, remote }))
}

// ───────────── 데몬 적용 경로 ─────────────

struct Ingest {
    rt: tokio::runtime::Runtime,
    a: Arc<yangpa_daemon::Node>,
    from: yangpa_net::PeerId,
    space: SpaceId,
    batches: Vec<Vec<SignedEvent>>,
    next: usize,
    _b: Arc<yangpa_daemon::Node>,
    _dir: tempfile::TempDir,
}

/// 묶음 하나의 메시지 수.
pub const INGEST_BATCH: usize = 20;
/// 미리 만들어 두는 묶음 수(`run`의 최대 반복 수).
pub const INGEST_BATCHES: u64 = 16;

impl Workload for Ingest {
    fn run(&mut self, n: u64) -> Result<()> {
        // 준비한 묶음을 다 쓰면 더 할 수 없습니다(같은 이벤트를 다시 넣으면 중복이라 재는 일이 달라짐).
        if self.next as u64 + n > INGEST_BATCHES {
            return Err(anyhow!("이 작업은 모두 {INGEST_BATCHES}번까지 반복합니다"));
        }
        for _ in 0..n {
            let batch = self.batches[self.next].clone();
            self.next += 1;
            let want = batch.len();
            let report = self.rt.block_on(yangpa_daemon::testkit::ingest_from(
                &self.a, self.space, batch, self.from,
            ))?;
            if report.waiting != 0 || report.accepted.len() != want {
                return Err(anyhow!("적용되지 않은 이벤트가 있음: {report:?}"));
            }
        }
        Ok(())
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        let (a, b) = (self.a.clone(), self._b.clone());
        self.rt.block_on(async move {
            a.shutdown().await;
            b.shutdown().await;
        });
    }
}

/// 망을 쓰지 않는 시험 노드 설정(서로 연결하지 않음).
pub fn quiet_node_config(dir: &std::path::Path) -> yangpa_daemon::NodeConfig {
    let mut cfg = yangpa_daemon::NodeConfig::for_test(dir);
    cfg.net.mdns = false;
    cfg
}

/// 신원을 만든 노드.
pub async fn start_node(dir: &std::path::Path, name: &str) -> Result<Arc<yangpa_daemon::Node>> {
    let node = yangpa_daemon::Node::start(quiet_node_config(dir)).await?;
    node.call(
        &yangpa_daemon::CallCtx::owner(),
        "identity.create",
        serde_json::json!({ "name": name }),
    )
    .await
    .map_err(|e| anyhow!("신원 만들기: {e:?}"))?;
    Ok(node)
}

fn ingest_setup() -> Result<Box<dyn Workload>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let dir = tempfile::tempdir()?;
    let (a, b, space, batches) = rt.block_on(async {
        let a = start_node(&dir.path().join("a"), "a").await?;
        let b = start_node(&dir.path().join("b"), "b").await?;
        let space = yangpa_daemon::testkit::seed_crew(&a, std::slice::from_ref(&b), "기준").await?;
        let chans = a
            .call(
                &yangpa_daemon::CallCtx::owner(),
                "channel.list",
                serde_json::json!({ "space": space }),
            )
            .await
            .map_err(|e| anyhow!("채널 목록: {e:?}"))?;
        let channel: ChannelId = serde_json::from_value(
            chans
                .get(0)
                .and_then(|c| c.get("channel"))
                .cloned()
                .context("채널 없음")?,
        )?;
        let mut batches = Vec::new();
        for i in 0..INGEST_BATCHES {
            let mut evs = Vec::new();
            for j in 0..INGEST_BATCH {
                let text = format!("성능 기준 메시지 {i}-{j}: 양파 메신저는 서버 없이 동작합니다.");
                evs.extend(
                    yangpa_daemon::testkit::send_unpropagated(&b, space, channel, &text).await?,
                );
            }
            batches.push(evs);
        }
        anyhow::Ok((a, b, space, batches))
    })?;
    let from = b.peer_id();
    Ok(Box::new(Ingest {
        rt,
        a,
        from,
        space,
        batches,
        next: 0,
        _b: b,
        _dir: dir,
    }))
}
