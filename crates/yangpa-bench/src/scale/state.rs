//! 크루 상태(가입 기록) 규모 측정과 해소기 시나리오.
//!
//! 실제 키·서명으로 만든 가입 기록을 해소기에 넣어 이벤트 크기, 해소 시간, 메모리를 잽니다. 키 생성(FN-DSA)이 가장
//! 오래 걸려 사용자 만들기만 여러 스레드로 나눕니다.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use yangpa_core::body::{EventBody, InviteInfo, InviteProof, MessageBody, Secret32};
use yangpa_core::device::{DeviceEntry, DeviceList, DeviceSignKeys, Profile, SignedDeviceList};
use yangpa_core::event::{EVENT_VERSION, EventHeader, KeyRef, SignedEvent};
use yangpa_core::ids::{
    ChannelId, ChannelRootId, DeviceId, EventId, LinkToken, SpaceId, StateKeyId, UserId,
};
use yangpa_core::links::{invite_verify_key, token_hash};
use yangpa_core::model::{Member, Permissions, SpaceKind, SpaceSettings};
use yangpa_core::sign::{Sig, SigningKeypair};
use yangpa_core::time::Timestamp;
use yangpa_crypto::{DeviceKeys, encrypt_body};
use yangpa_sync::state::{DecodedEvent, Limits, Resolver, ResolverStats};

/// 시뮬레이션 시각의 시작(밀리초). 초대는 만료가 없어 값 자체는 상관없습니다.
const T0: u64 = 1_800_000_000_000;

/// 크루 상태(가입 기록) 측정.
#[derive(Debug, Clone, Serialize)]
pub struct StateReport {
    /// 가입시키려 한 멤버 수(소유자 제외).
    pub members: usize,
    /// 해소기가 적용한 가입 수.
    pub applied: usize,
    /// 거부된 가입 수와 첫 거부 사유.
    pub rejected: usize,
    pub first_reject: Option<String>,
    /// 가입 기록 하나의 전송 크기(머리·서명·암호문 포함, 평균).
    pub join_wire: usize,
    /// 공간 스트림 전체 전송 크기(새 멤버가 받는 양).
    pub stream_wire: u64,
    /// 해소기가 메모리에 두는 본문 크기 합(풀린 본문 직렬화 기준).
    pub bodies: u64,
    /// 사용자·기기 키와 가입 기록 만들기.
    pub generate: Duration,
    /// 가입 기록을 하나씩 넣기(증분 해소).
    pub insert: Duration,
    /// 처음부터 다시 해소 한 번(동시 이벤트가 끼어들 때).
    pub rebuild: Duration,
    /// 같은 가입이 모두 동시(부모가 초대 하나)일 때 다시 해소 한 번(서명 검증 제외). 동시 가입마다 준비
    /// 집합 전체를 다시 정렬하면 여기서 O(K²)가 드러납니다.
    pub concurrent: Duration,
    /// 프로세스 최대 상주 메모리(MB, 리눅스만).
    pub peak_rss_mb: Option<f64>,
}

/// 메시지 이벤트 크기.
#[derive(Debug, Clone, Serialize)]
pub struct MessageReport {
    /// 짧은 글 하나의 전송 크기.
    pub short_text: usize,
    /// 그중 서명.
    pub sig: usize,
}

/// `by` = (작성 기기, 그 사용자).
pub(super) fn header(
    space: SpaceId,
    channel: Option<ChannelId>,
    body: &EventBody,
    by: (DeviceId, UserId),
    parents: Vec<EventId>,
    lamport: u64,
    key: KeyRef,
) -> EventHeader {
    EventHeader {
        v: EVENT_VERSION,
        space,
        channel,
        kind: body.kind(),
        author: by.0,
        user: by.1,
        parents,
        lamport,
        created_at: Timestamp(T0 + lamport),
        key,
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    }
}

pub(super) fn wire_len(ev: &SignedEvent) -> Result<usize> {
    Ok(postcard::to_stdvec(ev)?.len())
}

/// 한 기기를 가진 사용자.
pub(super) struct Person {
    pub(super) identity: SigningKeypair,
    pub(super) device: DeviceKeys,
}

impl Person {
    pub(super) fn generate() -> Self {
        Person {
            identity: SigningKeypair::generate(),
            device: DeviceKeys::generate(),
        }
    }

    pub(super) fn user(&self) -> UserId {
        UserId(self.identity.public())
    }

    pub(super) fn list(&self) -> Result<SignedDeviceList> {
        Ok(DeviceList {
            user: self.user(),
            version: 1,
            devices: vec![DeviceEntry {
                device: self.device.device_id(),
                box_key: self.device.box_public(),
                sign_keys: DeviceSignKeys::of(&self.device.signing),
                name: "데스크톱".into(),
                added_at: Timestamp(T0),
                revoked_at: None,
            }],
            otp: None,
        }
        .sign_with(&self.identity, &self.device.signing)?)
    }

    pub(super) fn member(&self) -> Member {
        Member {
            user: self.user(),
            roles: Vec::new(),
            nickname: None,
            automation: false,
        }
    }

    /// 초대 링크로 들어오는 가입 기록 본문(지금 데몬이 싣는 것과 같은 항목).
    pub(super) fn join_body(
        &self,
        i: usize,
        space: &SpaceId,
        token: &LinkToken,
    ) -> Result<EventBody> {
        let devices = self.list()?;
        let profile = Profile {
            version: 1,
            name: format!("멤버 {i}"),
            ..Default::default()
        }
        .sign(&self.identity)?;
        let invite = InviteProof::create(token, space, &devices, Timestamp(T0 + i as u64))?
            .signed_by_joiner(space, &devices, &self.device.signing)?;
        Ok(EventBody::MemberJoin {
            member: self.member(),
            devices,
            profile: Some(profile),
            invite: Some(invite),
        })
    }
}

/// 크루 시작과 만료 없는 초대(`token`). 소유자 기기가 확정 기기입니다.
pub(super) fn genesis_bodies(owner: &Person, token: &LinkToken) -> Result<Vec<EventBody>> {
    Ok(vec![
        EventBody::SpaceCreate {
            kind: SpaceKind::Crew,
            settings: SpaceSettings {
                name: "규모 시뮬레이션".into(),
                description: String::new(),
                icon: None,
                icon_key: None,
                allow_external_emoji: false,
                strict_rotation: false,
            },
            owner: owner.user(),
            committer: owner.device.device_id(),
            everyone: Permissions::DEFAULT_EVERYONE,
            members: vec![owner.member()],
            devices: vec![owner.list()?],
        },
        EventBody::InviteCreate {
            invite: InviteInfo {
                token_hash: token_hash(token),
                verify_key: invite_verify_key(token),
                expires_at: None,
                max_uses: None,
                roles: Vec::new(),
            },
        },
    ])
}

/// `n`개를 스레드 여럿으로 나눠 만듭니다(순서 유지).
pub(super) fn generate_parallel<T: Send>(
    n: usize,
    f: impl Fn(usize) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |p| p.get())
        .min(n.max(1));
    let chunk = n.div_ceil(threads.max(1)).max(1);
    let f = &f;
    let parts: Vec<Result<Vec<T>>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .step_by(chunk)
            .map(|start| s.spawn(move || (start..(start + chunk).min(n)).map(f).collect()))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow!("만들기 스레드가 멈춤")))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(n);
    for p in parts {
        out.extend(p?);
    }
    Ok(out)
}

/// 멤버 `members`명이 초대 링크로 차례로 들어온 크루의 공간 스트림을 만들어 해소합니다.
pub fn state(members: usize) -> Result<StateReport> {
    let space = SpaceId([0x5c; 16]);
    let state_key = Secret32([0x11; 32]);
    let key = KeyRef::State(StateKeyId([0x22; 16]));
    let token = LinkToken([0x33; 16]);
    let owner = Person::generate();
    let owner_dev = owner.device.device_id();

    let t = Instant::now();
    let mut bodies = genesis_bodies(&owner, &token)?;
    bodies.extend(generate_parallel(members, |i| {
        Person::generate().join_body(i, &space, &token)
    })?);

    // 받아 준 멤버(여기서는 소유자 기기)가 차례로 낸 선형 기록. 같은 본문으로 모두 동시인 기록도 만듭니다.
    let mut events = Vec::with_capacity(bodies.len());
    let mut concurrent_events = Vec::with_capacity(bodies.len());
    let mut invite: Option<EventId> = None;
    let mut stream_wire = 0u64;
    let mut join_wire = 0u64;
    let mut body_bytes = 0u64;
    let mut prev: Option<EventId> = None;
    for (i, body) in bodies.into_iter().enumerate() {
        let h = header(
            space,
            None,
            &body,
            (owner_dev, owner.user()),
            prev.into_iter().collect(),
            i as u64 + 1,
            key,
        );
        let ct = encrypt_body(&state_key, &h, &body)?;
        let ev = SignedEvent::sign(h, ct, &owner.device.signing)?;
        let len = wire_len(&ev)? as u64;
        stream_wire += len;
        if matches!(body, EventBody::MemberJoin { .. }) {
            join_wire += len;
        }
        body_bytes += body.encode()?.len() as u64;
        let id = ev.id();
        if i < 2 {
            invite = Some(id);
            concurrent_events.push(DecodedEvent {
                id,
                header: ev.header.clone(),
                body: body.clone(),
            });
        } else {
            let h = header(
                space,
                None,
                &body,
                (owner_dev, owner.user()),
                invite.into_iter().collect(),
                3,
                key,
            );
            let ct = encrypt_body(&state_key, &h, &body)?;
            let cev = SignedEvent::sign(h, ct, &owner.device.signing)?;
            concurrent_events.push(DecodedEvent {
                id: cev.id(),
                header: cev.header,
                body: body.clone(),
            });
        }
        prev = Some(id);
        events.push(DecodedEvent {
            id,
            header: ev.header,
            body,
        });
    }
    let generate = t.elapsed();

    let genesis = events.first().map(|e| e.id).context("시작 이벤트 없음")?;
    let mut resolver = Resolver::with_genesis(genesis);
    let mut applied = 0usize;
    let mut rejected = 0usize;
    let mut first_reject = None;
    let t = Instant::now();
    for e in events {
        let up = resolver.insert_events(vec![e]);
        applied += up.applied.len();
        rejected += up.rejected.len();
        if first_reject.is_none() {
            first_reject = up.rejected.first().map(|(_, err)| err.to_string());
        }
    }
    let insert = t.elapsed();
    let t = Instant::now();
    let full = resolver.rebuild();
    let rebuild = t.elapsed();
    if !full.full {
        return Err(anyhow!("다시 해소가 전체 해소로 돌지 않음"));
    }
    drop(resolver);
    let mut resolver = Resolver::with_genesis(genesis);
    let up = resolver.insert_events(concurrent_events);
    if up.applied.len() != applied {
        return Err(anyhow!(
            "동시 가입 적용 수가 다름: {} != {applied}",
            up.applied.len()
        ));
    }
    // 서명 검증은 넣을 때 끝났으므로 다시 해소는 순서 정하기와 적용만 잽니다.
    let t = Instant::now();
    resolver.rebuild();
    let concurrent = t.elapsed();
    Ok(StateReport {
        members,
        // 시작·초대 이벤트는 빼고 가입만 셉니다.
        applied: applied.saturating_sub(2),
        rejected,
        first_reject,
        join_wire: (join_wire / members.max(1) as u64) as usize,
        stream_wire,
        bodies: body_bytes,
        generate,
        insert,
        rebuild,
        concurrent,
        peak_rss_mb: peak_rss_mb(),
    })
}

/// 짧은 글 하나의 이벤트 크기.
pub fn message() -> Result<MessageReport> {
    let author = DeviceKeys::generate();
    let body = EventBody::Message(MessageBody {
        text: "오늘 모임은 저녁 8시에 시작합니다.".into(),
        ..Default::default()
    });
    let h = header(
        SpaceId([1; 16]),
        Some(ChannelId([2; 16])),
        &body,
        (author.device_id(), UserId([3; 32])),
        vec![EventId([4; 32])],
        10,
        KeyRef::Record {
            root: ChannelRootId([5; 16]),
        },
    );
    let ct = encrypt_body(&Secret32([6; 32]), &h, &body)?;
    let ev = SignedEvent::sign(h, ct, &author.signing)?;
    Ok(MessageReport {
        short_text: wire_len(&ev)?,
        sig: ev.sig.as_bytes().len(),
    })
}

/// 프로세스 최대 상주 메모리(`VmHWM`).
pub(super) fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb / 1024.0)
}

/// 프로세스 현재 상주 메모리(`VmRSS`, MB).
pub(super) fn rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb / 1024.0)
}

/// 해소기 시나리오 하나의 결과.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub scenario: &'static str,
    /// 시나리오 변수(동시 가입 수 K, 늦게 온 갈래 비율 %, 갈라진 동안 한쪽 가입 수).
    pub param: f64,
    /// 끝난 뒤 멤버 수(소유자 포함).
    pub members: usize,
    pub events: usize,
    pub rejected: usize,
    pub first_reject: Option<String>,
    /// 이벤트를 하나씩 넣는 동안 이어서 적용한 횟수, 처음부터 다시 해소한 횟수, 스냅숏에서 다시 해소한 횟수.
    pub incremental: u64,
    pub rebuilds: u64,
    pub replays: u64,
    /// 넣기 전체 시간과 가장 오래 걸린 넣기 한 번.
    pub insert: Duration,
    pub slowest: Duration,
    /// 다 넣은 뒤 처음부터 다시 해소 한 번(본문 서명은 기억해 둔 검증 결과를 씀).
    pub rebuild: Duration,
    /// 한 묶음으로 넣을 때 걸린 시간. 동시 가입은 새 해소기에 K개를 한꺼번에(본문 서명 첫 검증 포함), 갈라짐은
    /// 반대쪽 기록을 따라잡기로.
    pub batch: Option<Duration>,
    /// 받는 순서와 상관없이 같은 상태·같은 적용 순서에 이르는지: 하나씩 넣은 해소기를 새 해소기에 한 묶음으로 넣은
    /// 것, 시드로 섞어 64개씩 넣은 것과 비교합니다(갈라짐은 두 쪽끼리도). 적용 목록은 정렬하지 않고 순서까지 봅니다.
    pub converged: Option<bool>,
    /// 넣는 동안 늘어난 상주 메모리(해소기 안 구조. 본문은 미리 만들어 둬서 빠짐)와 프로세스 최대값.
    pub resolver_rss_mb: Option<f64>,
    pub peak_rss_mb: Option<f64>,
    /// 새 멤버가 받는 공간 스트림 전송 크기(시작부터 끝까지, 본문 암호화·서명 자리 포함).
    pub newcomer_bytes: u64,
    pub generate: Duration,
}

/// 해소기 시나리오용 크루: 소유자 기기, 다른 멤버 기기 하나(동시 기록의 작성자), 시작·초대 이벤트, 미리 만든
/// 가입 본문. 해소기는 이벤트 서명을 보지 않으므로(넣기 전 수집 단계에서 검증) 이벤트에는 서명하지 않고 ID를
/// 머리 해시로 만듭니다. 가입 본문 안의 기기 목록·초대 증명 서명은 실제입니다.
struct Crew {
    space: SpaceId,
    owner: (DeviceId, UserId),
    other: (DeviceId, UserId),
    genesis: Vec<DecodedEvent>,
    /// 첫 가입은 `other`의 가입입니다.
    joins: Vec<EventBody>,
    /// 해소기 멤버 상한(소유자 + 가입자). 시나리오가 `joins`를 꺼내 써도 그대로 둡니다.
    max_members: usize,
    generate: Duration,
}

impl Crew {
    fn new(joiners: usize) -> Result<Self> {
        anyhow::ensure!(joiners >= 1, "가입자가 한 명은 있어야 함");
        let t = Instant::now();
        let space = SpaceId([0x5d; 16]);
        let token = LinkToken([0x34; 16]);
        let owner = Person::generate();
        let other = Person::generate();
        let mut joins = vec![other.join_body(0, &space, &token)?];
        joins.extend(generate_parallel(joiners - 1, |i| {
            Person::generate().join_body(i + 1, &space, &token)
        })?);
        let mut crew = Crew {
            space,
            owner: (owner.device.device_id(), owner.user()),
            other: (other.device.device_id(), other.user()),
            genesis: Vec::new(),
            joins,
            max_members: joiners + 1,
            generate: Duration::ZERO,
        };
        let create = EventBody::SpaceCreate {
            kind: SpaceKind::Crew,
            settings: SpaceSettings {
                name: "해소기 시나리오".into(),
                description: String::new(),
                icon: None,
                icon_key: None,
                allow_external_emoji: false,
                strict_rotation: false,
            },
            owner: owner.user(),
            committer: owner.device.device_id(),
            everyone: Permissions::DEFAULT_EVERYONE,
            members: vec![owner.member()],
            devices: vec![owner.list()?],
        };
        let invite = EventBody::InviteCreate {
            invite: InviteInfo {
                token_hash: token_hash(&token),
                verify_key: invite_verify_key(&token),
                expires_at: None,
                max_uses: None,
                roles: Vec::new(),
            },
        };
        let g = crew.event(create, crew.owner, Vec::new(), 1)?;
        let i = crew.event(invite, crew.owner, vec![g.id], 2)?;
        crew.genesis = vec![g, i];
        crew.generate = t.elapsed();
        Ok(crew)
    }

    fn event(
        &self,
        body: EventBody,
        by: (DeviceId, UserId),
        mut parents: Vec<EventId>,
        lamport: u64,
    ) -> Result<DecodedEvent> {
        // 서명할 때처럼 부모를 정렬합니다(머리 구조 검사가 정렬을 요구).
        parents.sort_unstable();
        parents.dedup();
        let mut h = header(
            self.space,
            None,
            &body,
            by,
            parents,
            lamport,
            KeyRef::State(StateKeyId([0x22; 16])),
        );
        let encoded = body.encode()?;
        h.body_hash = *blake3::hash(&encoded).as_bytes();
        h.body_len = encoded.len() as u32;
        Ok(DecodedEvent {
            id: EventId::hash_of(&h.canonical_bytes()),
            header: h,
            body,
        })
    }

    /// 이벤트의 전송 크기: 실제로 암호화한 본문과 같은 길이의 서명 자리로 직렬화합니다.
    fn wire(&self, e: &DecodedEvent) -> Result<u64> {
        let ct = encrypt_body(&Secret32([0x11; 32]), &e.header, &e.body)?;
        let ev = SignedEvent {
            header: e.header.clone(),
            sig: Sig::from_bytes(&[0; Sig::LEN])?,
            body: Some(ct),
        };
        Ok(wire_len(&ev)? as u64)
    }

    fn wire_all<'a>(&'a self, events: impl IntoIterator<Item = &'a DecodedEvent>) -> Result<u64> {
        let mut sum = 0;
        for e in self.genesis.iter().chain(events) {
            sum += self.wire(e)?;
        }
        Ok(sum)
    }

    fn resolver(&self) -> Resolver {
        // 기본 멤버 상한(65,536명)과 상관없이 시나리오 크기의 상한으로 잽니다(상한에 걸리는 동작도 재려고).
        let mut r = Resolver::with_genesis_limits(
            self.genesis[0].id,
            Limits {
                max_members: self.max_members,
            },
        );
        r.insert_events(self.genesis.clone());
        r
    }
}

/// 이벤트를 넣으며 시간을 잽니다.
#[derive(Default)]
struct Feed {
    insert: Duration,
    slowest: Duration,
    rejected: usize,
    first_reject: Option<String>,
    events: usize,
}

/// `reference`와 같은 이벤트를 새 해소기에 한 묶음으로, 그리고 시드로 섞어 64개씩 넣어도 같은 상태·같은 적용
/// 순서인지.
fn same_in_any_order(
    crew: &Crew,
    reference: &Resolver,
    events: &[DecodedEvent],
    seed: u64,
) -> bool {
    use rand::SeedableRng;
    use rand::seq::SliceRandom;
    let same = |r: &Resolver| r.state() == reference.state() && r.applied() == reference.applied();
    let mut batch = crew.resolver();
    batch.insert_events(events.to_vec());
    if !same(&batch) {
        return false;
    }
    drop(batch);
    let mut shuffled = events.to_vec();
    shuffled.shuffle(&mut rand::rngs::StdRng::seed_from_u64(seed));
    let mut r = crew.resolver();
    for chunk in shuffled.chunks(64) {
        r.insert_events(chunk.to_vec());
    }
    same(&r)
}

fn rebuild_once(r: &mut Resolver) -> Duration {
    let t = Instant::now();
    r.rebuild();
    t.elapsed()
}

impl Feed {
    fn push(&mut self, r: &mut Resolver, events: Vec<DecodedEvent>) {
        self.events += events.len();
        let t = Instant::now();
        let up = r.insert_events(events);
        let d = t.elapsed();
        self.insert += d;
        self.slowest = self.slowest.max(d);
        self.rejected += up.rejected.len();
        if self.first_reject.is_none() {
            self.first_reject = up.rejected.first().map(|(_, e)| e.to_string());
        }
    }
}

/// 동시 가입 K개: 초대 하나를 부모로 둔 가입 기록 K개가 하나씩 도착합니다(여러 멤버가 같은 때 받아 줌). 새 기록이
/// 머리 전부를 덮지 않으므로 하나마다 처음부터 다시 해소합니다.
pub fn fanout_joins(k: usize) -> Result<ScenarioReport> {
    let mut crew = Crew::new(k)?;
    let joins = std::mem::take(&mut crew.joins);
    let invite = crew.genesis[1].id;
    // 이벤트는 메모리 기준점 전에 만들어 해소기 메모리에 섞이지 않게 합니다(해소기는 넣을 때 사본을 가짐).
    // 받아 준 기기는 모두 소유자로 둡니다. 다른 멤버가 받아 줘도 같은 부모·같은 논리 시계라 해소기에는 같고,
    // 동시 가입끼리는 작성 멤버의 가입보다 앞에 정렬될 수 있어(작성자가 아직 멤버가 아님) 섞지 않습니다.
    let events = joins
        .into_iter()
        .map(|b| crew.event(b, crew.owner, vec![invite], 3))
        .collect::<Result<Vec<_>>>()?;
    let newcomer_bytes = crew.wire_all(&events)?;
    let rss0 = rss_mb();
    let mut r = crew.resolver();
    let base = r.stats();
    let mut feed = Feed::default();
    for ev in &events {
        feed.push(&mut r, vec![ev.clone()]);
    }
    let stats = r.stats();
    let resolver_rss_mb = rss_mb().zip(rss0).map(|(a, b)| a - b);
    let rebuild = rebuild_once(&mut r);
    let members = r.state().map_or(0, |s| s.members.len());
    // 따라잡기처럼 K개가 한꺼번에 오면 다시 해소는 한 번입니다.
    let mut fresh = crew.resolver();
    let t = Instant::now();
    fresh.insert_events(events.clone());
    let batch = t.elapsed();
    let converged = fresh.state() == r.state()
        && fresh.applied() == r.applied()
        && same_in_any_order(&crew, &r, &events, k as u64);
    drop(fresh);
    Ok(ScenarioReport {
        scenario: "동시 가입",
        param: k as f64,
        members,
        events: feed.events,
        rejected: feed.rejected,
        first_reject: feed.first_reject,
        incremental: stats.incremental - base.incremental,
        rebuilds: stats.full - base.full,
        replays: stats.replay - base.replay,
        insert: feed.insert,
        slowest: feed.slowest,
        rebuild,
        batch: Some(batch),
        converged: Some(converged),
        resolver_rss_mb,
        peak_rss_mb: peak_rss_mb(),
        newcomer_bytes,
        generate: crew.generate,
    })
}

/// 늦게 온 갈래: 멤버 `members`명이 차례로 들어오는 동안 `pct`%의 가입은 다른 기기가 몇 이벤트 전 상태에서 받아 준
/// 것이라 늦게 도착합니다(부모가 2~8개 앞 이벤트). 다음 기록이 머리를 모두 부모로 삼아 갈래를 합칩니다.
pub fn concurrent_branches(members: usize, pct: f64, seed: u64) -> Result<ScenarioReport> {
    use rand::{Rng, SeedableRng};
    let mut crew = Crew::new(members)?;
    let joins = std::mem::take(&mut crew.joins);
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let invite = &crew.genesis[1];
    let mut heads: Vec<(EventId, u64)> = vec![(invite.id, invite.header.lamport)];
    let mut main: Vec<(EventId, u64)> = heads.clone();
    // 도착 순서대로 미리 만듭니다(메모리 기준점 전).
    let mut events = Vec::with_capacity(joins.len());
    for body in joins {
        let late = main.len() > 8 && rng.gen_bool((pct / 100.0).clamp(0.0, 1.0));
        let ev = if late {
            let (pid, pl) = main[main.len() - rng.gen_range(2..=8)];
            let ev = crew.event(body, crew.other, vec![pid], pl + 1)?;
            heads.push((ev.id, pl + 1));
            ev
        } else {
            let lamport = heads.iter().map(|h| h.1).max().unwrap_or(0) + 1;
            let parents = heads.iter().map(|h| h.0).collect();
            let ev = crew.event(body, crew.owner, parents, lamport)?;
            heads = vec![(ev.id, lamport)];
            main.push((ev.id, lamport));
            ev
        };
        events.push(ev);
    }
    let newcomer_bytes = crew.wire_all(&events)?;
    let rss0 = rss_mb();
    let mut r = crew.resolver();
    let base = r.stats();
    let mut feed = Feed::default();
    for ev in &events {
        feed.push(&mut r, vec![ev.clone()]);
    }
    let stats = r.stats();
    let resolver_rss_mb = rss_mb().zip(rss0).map(|(a, b)| a - b);
    let rebuild = rebuild_once(&mut r);
    let converged = same_in_any_order(&crew, &r, &events, seed);
    Ok(ScenarioReport {
        scenario: "늦게 온 갈래",
        param: pct,
        members: r.state().map_or(0, |s| s.members.len()),
        events: feed.events,
        rejected: feed.rejected,
        first_reject: feed.first_reject,
        incremental: stats.incremental - base.incremental,
        rebuilds: stats.full - base.full,
        replays: stats.replay - base.replay,
        insert: feed.insert,
        slowest: feed.slowest,
        rebuild,
        batch: None,
        converged: Some(converged),
        resolver_rss_mb,
        peak_rss_mb: peak_rss_mb(),
        newcomer_bytes,
        generate: crew.generate,
    })
}

/// `parent` 뒤로 `by`가 `n`개를 차례로 받아 준 선형 기록.
fn chain(
    crew: &Crew,
    by: (DeviceId, UserId),
    bodies: impl IntoIterator<Item = EventBody>,
    mut parent: (EventId, u64),
) -> Result<Vec<DecodedEvent>> {
    let mut out = Vec::new();
    for body in bodies {
        let ev = crew.event(body, by, vec![parent.0], parent.1 + 1)?;
        parent = (ev.id, ev.header.lamport);
        out.push(ev);
    }
    Ok(out)
}

fn tip(events: &[DecodedEvent], or: (EventId, u64)) -> (EventId, u64) {
    events.last().map_or(or, |e| (e.id, e.header.lamport))
}

/// 갈라졌다 합치기: 멤버 `members`명 뒤 두 쪽이 각각 `side`명씩 받아 준 뒤 다시 만납니다. 한쪽 해소기는 반대쪽
/// 기록을 하나씩(실시간으로 흘러들 때), 다른 쪽은 한 묶음으로(따라잡기) 받고, 합친 뒤 두 쪽 상태가 같은지 봅니다.
pub fn partition_merge(members: usize, side: usize) -> Result<ScenarioReport> {
    let mut crew = Crew::new(members + 2 * side + 1)?;
    let mut joins = std::mem::take(&mut crew.joins).into_iter();
    let invite = &crew.genesis[1];
    let start = (invite.id, invite.header.lamport);
    let base = chain(&crew, crew.owner, joins.by_ref().take(members), start)?;
    let fork = tip(&base, start);
    let side_a = chain(&crew, crew.owner, joins.by_ref().take(side), fork)?;
    let side_b = chain(&crew, crew.other, joins.by_ref().take(side), fork)?;
    let (a_tip, b_tip) = (tip(&side_a, fork), tip(&side_b, fork));
    let merge = crew.event(
        joins.next().context("합치기 가입 본문 없음")?,
        crew.owner,
        vec![a_tip.0, b_tip.0],
        a_tip.1.max(b_tip.1) + 1,
    )?;

    let newcomer_bytes = crew.wire_all(
        base.iter()
            .chain(&side_a)
            .chain(&side_b)
            .chain(std::iter::once(&merge)),
    )?;

    // A쪽: 공통 기록과 자기 쪽을 하나씩 넣은 뒤 반대쪽이 실시간으로 하나씩 흘러듭니다.
    let rss0 = rss_mb();
    let mut a = crew.resolver();
    let mut warm = Feed::default();
    for e in base.iter().chain(&side_a) {
        warm.push(&mut a, vec![e.clone()]);
    }
    let before = a.stats();
    let mut feed = Feed::default();
    for e in &side_b {
        feed.push(&mut a, vec![e.clone()]);
    }
    feed.push(&mut a, vec![merge.clone()]);
    let after = a.stats();
    let resolver_rss_mb = rss_mb().zip(rss0).map(|(x, y)| x - y);
    let rebuild = rebuild_once(&mut a);

    // B쪽: 반대쪽을 따라잡기로 한 묶음에.
    let all: Vec<DecodedEvent> = base
        .iter()
        .chain(&side_a)
        .chain(&side_b)
        .chain(std::iter::once(&merge))
        .cloned()
        .collect();
    let mut b = crew.resolver();
    for e in base.into_iter().chain(side_b) {
        b.insert_events(vec![e]);
    }
    let t = Instant::now();
    b.insert_events(side_a);
    let batch = t.elapsed();
    b.insert_events(vec![merge]);
    // 적용 순서까지(정렬하지 않고) 같아야 합니다.
    let converged = a.state() == b.state()
        && a.applied() == b.applied()
        && same_in_any_order(&crew, &a, &all, side as u64);
    Ok(ScenarioReport {
        scenario: "갈라졌다 합치기",
        param: side as f64,
        members: a.state().map_or(0, |s| s.members.len()),
        events: feed.events,
        rejected: feed.rejected + warm.rejected,
        first_reject: warm.first_reject.or(feed.first_reject),
        incremental: after.incremental - before.incremental,
        rebuilds: after.full - before.full,
        replays: after.replay - before.replay,
        insert: feed.insert,
        slowest: feed.slowest,
        rebuild,
        batch: Some(batch),
        converged: Some(converged),
        resolver_rss_mb,
        peak_rss_mb: peak_rss_mb(),
        newcomer_bytes,
        generate: crew.generate,
    })
}

/// 선형 가입 기록 가운데 일부가 옛 부모를 고른(동시) 기록을 하나씩 넣는 측정.
#[derive(Debug, Clone)]
pub struct StaleChainReport {
    pub members: usize,
    /// 옛 부모를 고른 가입 수.
    pub stale: usize,
    /// 모두 하나씩 넣는 데 걸린 시간(서명 검증 포함).
    pub insert: Duration,
    /// 그중 증분으로 이어 붙이지 못한 삽입(동시 기록과 그 뒤 합치는 기록)에 걸린 시간 합과 횟수.
    pub concurrent: Duration,
    pub concurrent_inserts: usize,
    pub stats: ResolverStats,
}

/// 소유자가 `members`명을 차례로 넣는 선형 기록에서 `stale_pct`%는 머리 대신 2~9개 앞의 옛 이벤트를 부모로
/// 고릅니다(동시 이벤트). 다음 기록은 다시 모든 머리를 부모로 둡니다. 하나씩 넣으며 해소기가 처음부터 다시 해소한
/// 횟수와 스냅숏에서 다시 해소한 횟수를 셉니다. 키 생성을 줄이려고 기기 목록은 신원 서명만 하고(공동 서명 없음)
/// 이벤트 ID는 머리의 해시로 만듭니다(해소기는 서명을 보지 않음).
pub fn stale_chain(members: usize, stale_pct: u64) -> Result<StaleChainReport> {
    let space = SpaceId([0x5d; 16]);
    let key = KeyRef::State(StateKeyId([0x22; 16]));
    let owner = Person::generate();
    let owner_dev = owner.device.device_id();
    let box_key = owner.device.box_public();
    let joins = generate_parallel(members, |i| {
        let identity = SigningKeypair::generate();
        let user = UserId(identity.public());
        // 기기 키 쌍은 만들지 않고(서명하지 않음) 번호로 채운 공개키에서 기기 ID를 셉니다(구조 검사는 통과).
        let mut sign_keys = DeviceSignKeys::filled(0x77);
        sign_keys.ed_pk[..8].copy_from_slice(&(i as u64).to_le_bytes());
        let devices = DeviceList {
            user,
            version: 1,
            devices: vec![DeviceEntry {
                device: sign_keys.device_id(),
                box_key: box_key.clone(),
                sign_keys,
                name: format!("기기 {i}"),
                added_at: Timestamp(T0),
                revoked_at: None,
            }],
            otp: None,
        }
        .sign(&identity)?;
        Ok(EventBody::MemberJoin {
            member: Member {
                user,
                roles: Vec::new(),
                nickname: None,
                automation: false,
            },
            devices,
            profile: None,
            invite: None,
        })
    })?;
    let create = EventBody::SpaceCreate {
        kind: SpaceKind::Crew,
        settings: SpaceSettings {
            name: "옛 부모 시뮬레이션".into(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        owner: owner.user(),
        committer: owner_dev,
        everyone: Permissions::DEFAULT_EVERYONE,
        members: vec![owner.member()],
        devices: vec![owner.list()?],
    };
    let mut rng = 0x5eed_u64;
    let mut next = move || {
        // splitmix64: 같은 입력이면 같은 기록.
        rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut events: Vec<DecodedEvent> = Vec::with_capacity(members + 1);
    let mut heads: Vec<EventId> = Vec::new();
    let mut stale = 0;
    for body in std::iter::once(create).chain(joins) {
        let mut parents = if events.len() > 10 && next() % 100 < stale_pct {
            stale += 1;
            let back = 2 + (next() % 8) as usize;
            vec![events[events.len() - back].id]
        } else {
            std::mem::take(&mut heads)
        };
        parents.sort_unstable();
        let lamport = parents
            .iter()
            .filter_map(|p| events.iter().rev().find(|e| e.id == *p))
            .map(|e| e.header.lamport)
            .max()
            .map_or(1, |l| l + 1);
        let mut h = header(
            space,
            None,
            &body,
            (owner_dev, owner.user()),
            parents.clone(),
            lamport,
            key,
        );
        // 부모·논리 시계가 같은 두 기록이 같은 ID가 되지 않게 본문 해시를 머리에 넣습니다.
        h.body_hash = EventId::hash_of(&body.encode()?).0;
        let id = EventId::hash_of(&postcard::to_stdvec(&h)?);
        heads.retain(|x| !parents.contains(x));
        heads.push(id);
        events.push(DecodedEvent {
            id,
            header: h,
            body,
        });
    }
    let genesis = events[0].id;
    // 상한이 기본값(65,536명)과 달라도 걸리지 않게 이 측정의 한도를 멤버 수에 맞춥니다.
    let mut resolver = Resolver::with_genesis_limits(
        genesis,
        Limits {
            max_members: members + 1,
        },
    );
    let mut concurrent = Duration::ZERO;
    let mut concurrent_inserts = 0;
    let t = Instant::now();
    for e in events {
        let before = resolver.stats();
        let s = Instant::now();
        resolver.insert_events(vec![e]);
        if resolver.stats().incremental == before.incremental {
            concurrent += s.elapsed();
            concurrent_inserts += 1;
        }
    }
    let insert = t.elapsed();
    let applied = resolver.applied().len();
    if applied != members + 1 {
        return Err(anyhow!(
            "적용 수가 다름: {applied} != {}, 첫 거부 {:?}",
            members + 1,
            resolver.rejected().first()
        ));
    }
    Ok(StaleChainReport {
        members,
        stale,
        insert,
        concurrent,
        concurrent_inserts,
        stats: resolver.stats(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 동시 가입 200개가 모두 적용되고, 머리를 덮지 못해 가입마다 다시 해소하는지.
    #[test]
    fn fanout_rebuild_runs() {
        let r = fanout_joins(200).unwrap();
        assert_eq!(r.rejected, 0, "{r:?}");
        assert_eq!(r.members, 201, "{r:?}");
        assert_eq!(r.events, 200);
        assert!(r.rebuilds + r.replays >= 199, "{r:?}");
        assert!(r.slowest > Duration::ZERO && r.insert >= r.slowest);
        // 한꺼번에·섞어서 넣어도 같은 상태와 적용 순서.
        assert_eq!(r.converged, Some(true), "{r:?}");
        assert!(r.batch.is_some() && r.rebuild > Duration::ZERO, "{r:?}");
    }

    /// 늦게 온 갈래는 갈래마다 한 번 다시 해소하고, 갈라졌다 합친 두 쪽은 같은 상태에 이릅니다.
    #[test]
    fn branches_and_partition_converge() {
        let r = concurrent_branches(120, 5.0, 3).unwrap();
        assert_eq!(r.rejected, 0, "{r:?}");
        assert_eq!(r.members, 121, "{r:?}");
        assert!(
            r.rebuilds + r.replays >= 1 && r.rebuilds + r.replays < 30,
            "{r:?}"
        );
        assert!(r.incremental > 80, "{r:?}");
        assert_eq!(r.converged, Some(true), "{r:?}");

        let p = partition_merge(40, 10).unwrap();
        assert_eq!(p.converged, Some(true), "{p:?}");
        assert_eq!(p.rejected, 0, "{p:?}");
        assert_eq!(p.members, 1 + 40 + 20 + 1, "{p:?}");
        assert!(p.rebuilds + p.replays >= 1, "{p:?}");
    }

    /// 옛 부모 기록: 모두 적용되고(함수 안에서 확인), 삽입마다 증분·재생·전체 해소 중 하나로 셉니다.
    #[test]
    fn stale_chain_counts_every_insert() {
        let s = stale_chain(40, 30).unwrap();
        assert!(s.stale > 0, "{s:?}");
        let st = s.stats;
        assert_eq!(st.incremental + st.full + st.replay, 41, "{s:?}");
        assert_eq!(s.concurrent_inserts as u64, st.full + st.replay, "{s:?}");
    }
}
