//! 상태 체크포인트 규모 측정(`--checkpoint`).
//!
//! 큰 크루의 가입 기록 위에 소유자 기기가 체크포인트를 내고 그 뒤 꼬리(가입)가 이어질 때를 실제 키·서명·해소기로 만들어
//! 잽니다.
//!
//! - 새 멤버가 받는 양: 체크포인트 이벤트 + 스냅숏 블롭(데몬과 같은 결정론적 암호화) + 경계 F 이상 이벤트. 체크포인트
//!   없이 공간 스트림 전체를 받을 때와 견줍니다.
//! - 차가운 시작: 새 프로세스(`scale-checkpoint-load`)에서 디스크의 이벤트를 읽어 서명 확인·복호화·해소하는 시간과 상주
//!   메모리를, 처음부터(전체 기록)와 스냅숏 + 경계 위 꼬리(데몬 `checkpoint::restore` + `events_by_lane_from`)로 잽니다.
//!   같은 프로세스에서 재면 크루를 만들며 쓴 메모리가 RSS에 남기 때문입니다.
//! - 해소기 메모리: 해소기가 아는 항목 수(전체 기록, 다음 체크포인트를 받아 앞 바탕 앞 기록을 버린 뒤, 새 멤버).
//!   해소기는 겨룬 체크포인트를 판정하려고 마지막 바탕의 앞 바탕까지 남기므로, 첫 체크포인트로는 줄지 않고 두 번째에서
//!   첫 체크포인트 앞이 빠집니다.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use yangpa_core::blob::BlobManifest;
use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::checkpoint::{
    CheckpointBody, MAX_SNAPSHOT_LEN, RESOLVER_VERSION, SignedCheckpoint,
};
use yangpa_core::event::{KeyRef, SignedEvent};
use yangpa_core::ids::{BlobId, EventId, LinkToken, SpaceId, StateKeyId};
use yangpa_crypto::{decrypt_body, encrypt_body};
use yangpa_daemon::testkit::checkpoint::{open_snapshot, seal_snapshot};
use yangpa_sync::state::{CheckpointDraft, DecodedEvent, Limits, Resolver};

use super::state::{
    Person, generate_parallel, genesis_bodies, header, peak_rss_mb, rss_mb, wire_len,
};

const STATE_KEY: Secret32 = Secret32([0x11; 32]);
const KEY_ID: StateKeyId = StateKeyId([0x22; 16]);

/// 체크포인트 측정 하나.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointReport {
    /// 체크포인트 앞 가입 수(소유자 제외).
    pub members: usize,
    /// 체크포인트 뒤 가입 수(꼬리).
    pub tail: usize,
    /// 공간 스트림 전체 전송 크기(체크포인트 없이 새 멤버가 받는 양).
    pub full_wire: u64,
    /// 체크포인트 이벤트 하나의 전송 크기.
    pub checkpoint_wire: u64,
    /// 스냅숏 평문(정규 인코딩)과 블롭(암호문) 크기.
    pub snapshot_plain: u64,
    pub snapshot_blob: u64,
    /// 경계 위 조상 목록 수와 경계 F 이상 이벤트(체크포인트 빼고) 수·전송 크기.
    pub frontier: usize,
    pub above_floor: usize,
    pub above_floor_wire: u64,
    /// 새 멤버가 받는 합(체크포인트 + 스냅숏 블롭 + 경계 위 이벤트).
    pub newcomer_wire: u64,
    /// 사용자·기기 키와 기록 만들기.
    pub generate: Duration,
    /// 체크포인트 초안(정규 인코딩·머클 트리)과 스냅숏 암호화.
    pub draft: Duration,
    pub seal: Duration,
    /// 꼬리 뒤 다음 체크포인트를 같은 해소기에 넣기(첫 체크포인트 앞 기록을 RAM에서 버림).
    pub compact: Duration,
    /// 해소기 항목 수: 체크포인트 앞 전체 기록, 다음 체크포인트까지 넣은 뒤, 새 멤버.
    pub entries_full: usize,
    pub entries_compacted: usize,
    pub entries_newcomer: usize,
    /// 새 프로세스의 차가운 시작: 처음부터와 스냅숏에서.
    pub cold_full: ColdStart,
    pub cold_checkpoint: ColdStart,
    pub peak_rss_mb: Option<f64>,
}

/// 새 프로세스에서 잰 차가운 시작 하나.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdStart {
    /// 파일 읽기와 이벤트 디코딩.
    pub read: Duration,
    /// 서명 확인·복호화(스냅숏 블롭 포함).
    pub open: Duration,
    /// 해소기 세우기(스냅숏 확인 포함).
    pub resolve: Duration,
    /// 해소기를 세운 뒤 상주 메모리 증가와 최대 상주 메모리(MB, 리눅스만).
    pub rss_mb: Option<f64>,
    pub peak_rss_mb: Option<f64>,
    /// 해소기 항목 수, 멤버 수, 지금 상태의 해시(두 방식이 같은 상태인지 볼 때).
    pub entries: usize,
    pub members: usize,
    pub state_digest: Option<[u8; 32]>,
}

/// 차가운 시작 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColdMode {
    /// 공간 스트림 전체를 처음부터.
    Full,
    /// 저장한 스냅숏과 경계 위 이벤트.
    Checkpoint,
}

/// 새 프로세스에 넘기는 값(`case.json`).
#[derive(Serialize, Deserialize)]
struct Case {
    genesis: [u8; 32],
    seq: u64,
    state_hash: [u8; 32],
    manifest: BlobManifest,
    max_members: usize,
}

/// 지금 상태의 해시(프로세스 사이에서 상태가 같은지 볼 때).
fn state_digest(r: &Resolver) -> Option<[u8; 32]> {
    let st = r.state()?;
    Some(*blake3::hash(&postcard::to_stdvec(st).ok()?).as_bytes())
}

/// 멤버 `members`명 크루에 체크포인트를 내고 꼬리 `tail`개를 더한 뒤 잽니다. `exe`는 차가운 시작을 잴 이 실행
/// 파일이고, 없으면 같은 프로세스에서 잽니다(시험용, 상주 메모리는 믿을 수 없음).
pub fn measure(members: usize, tail: usize, exe: Option<&Path>) -> Result<CheckpointReport> {
    let space = SpaceId([0x5d; 16]);
    let token = LinkToken([0x33; 16]);
    let key = KeyRef::State(KEY_ID);
    let limits = Limits {
        max_members: (members + tail + 1).max(Limits::default().max_members),
    };
    let owner = Person::generate();
    let by = (owner.device.device_id(), owner.user());
    let event = |body: EventBody, parents: Vec<EventId>, lamport: u64| -> Result<SignedEvent> {
        let h = header(space, None, &body, by, parents, lamport, key);
        let ct = encrypt_body(&STATE_KEY, &h, &body)?;
        Ok(SignedEvent::sign(h, ct, &owner.device.signing)?)
    };
    let decoded = |ev: &SignedEvent, body: EventBody| DecodedEvent {
        id: ev.id(),
        header: ev.header.clone(),
        body,
    };

    let t = Instant::now();
    let mut bodies = genesis_bodies(&owner, &token)?;
    let owner_list = match &bodies[0] {
        EventBody::SpaceCreate { devices, .. } => devices[0].clone(),
        _ => bail!("시작 이벤트가 아님"),
    };
    bodies.extend(generate_parallel(members + tail, |i| {
        Person::generate().join_body(i, &space, &token)
    })?);
    let tail_bodies = bodies.split_off(bodies.len() - tail);
    // 받아 준 멤버(소유자 기기)가 차례로 낸 선형 기록.
    let mut stream: Vec<SignedEvent> = Vec::with_capacity(bodies.len() + tail + 1);
    let mut history = Vec::with_capacity(bodies.len());
    for (i, body) in bodies.into_iter().enumerate() {
        let parents = stream.last().map(|e| e.id()).into_iter().collect();
        let ev = event(body.clone(), parents, i as u64 + 1)?;
        history.push(decoded(&ev, body));
        stream.push(ev);
    }
    let generate = t.elapsed();
    let genesis = stream[0].id();

    let mut full = Resolver::with_genesis_limits(genesis, limits);
    let up = full.insert_events(history);
    if let Some((_, e)) = up.rejected.first() {
        bail!("가입 기록이 거부됨: {e}");
    }
    let entries_full = full.len();

    // 소유자 기기가 체크포인트를 냅니다(데몬 `checkpoint::issue`와 같은 순서).
    let issue = |r: &Resolver| -> Result<Issued> {
        let t = Instant::now();
        let d = r
            .checkpoint_draft(KEY_ID)
            .map_err(|e| anyhow!("체크포인트 초안: {e}"))?;
        let draft = t.elapsed();
        let t = Instant::now();
        let (ct, manifest) = seal_snapshot(&STATE_KEY, d.seq, &d.state_hash, &d.snapshot)?;
        let seal = t.elapsed();
        if manifest.size > MAX_SNAPSHOT_LEN {
            bail!("스냅숏이 상한보다 큼: {}B", manifest.size);
        }
        let body = CheckpointBody {
            space,
            seq: d.seq,
            prev: d.prev,
            heads: d.heads.clone(),
            frontier_lamport: d.frontier_lamport,
            frontier: d.frontier.clone(),
            state_hash: d.state_hash,
            user_root: d.user_root,
            users: d.users,
            device_root: d.device_root,
            devices: d.devices,
            // 해소기는 블롭 ID를 보지 않습니다(데몬이 받은 블롭을 목록 해시로 확인).
            snapshot: BlobId(*blake3::hash(&ct).as_bytes()),
            snapshot_len: manifest.size,
            key_heads: d.key_heads.clone(),
            valid_state_keys: d.valid_state_keys.clone(),
            owner: d.owner,
            owner_devices: owner_list.clone(),
            owner_proof: d.owner_proof.clone(),
            revoked: d.revoked.clone(),
            revocation_root: d.revocation_root,
            owner_chain: d.owner_chain.clone(),
            resolver_version: RESOLVER_VERSION,
            author: by.0,
            delegation: None,
            delegation_hash: d.delegation_hash,
            signer_proofs: d.signer_proofs_for(&by.0),
        };
        let sc = SignedCheckpoint::sign_owner(body, &owner.identity, &owner.device.signing)?;
        let cp_body = EventBody::StateCheckpoint(Box::new(sc));
        let cp = event(cp_body.clone(), d.heads.clone(), d.lamport)?;
        Ok(Issued {
            decoded: decoded(&cp, cp_body),
            event: cp,
            ct,
            manifest,
            draft,
            seal,
            d,
        })
    };
    let first = issue(&full)?;
    let d = &first.d;
    stream.push(first.event.clone());
    let up = full.insert_events(vec![first.decoded.clone()]);
    if up.applied.len() != 1 || full.base().map(|b| b.seq) != Some(d.seq) {
        bail!("체크포인트가 바탕이 되지 않음: {:?}", up.rejected);
    }
    // 꼬리: 체크포인트 뒤 가입.
    let mut tail_events = Vec::with_capacity(tail);
    for (j, body) in tail_bodies.into_iter().enumerate() {
        let prev = stream.last().map(|e| e.id()).context("빈 기록")?;
        let ev = event(body.clone(), vec![prev], d.lamport + 1 + j as u64)?;
        tail_events.push(decoded(&ev, body));
        stream.push(ev);
    }
    let up = full.insert_events(tail_events);
    if let Some((_, e)) = up.rejected.first() {
        bail!("꼬리가 거부됨: {e}");
    }

    // 새 멤버가 받는 것: 체크포인트, 스냅숏 블롭, 경계 F 이상 이벤트(경계 위 조상 + 꼬리).
    let (cp, ct, manifest) = (&first.event, &first.ct, &first.manifest);
    let cp_id = cp.id();
    let mut full_wire = 0u64;
    let mut above = Vec::new();
    let mut above_wire = 0u64;
    for ev in &stream {
        let len = wire_len(ev)? as u64;
        full_wire += len;
        if ev.header.lamport >= d.frontier_lamport && ev.id() != cp_id {
            above_wire += len;
            above.push(ev.clone());
        }
    }
    let checkpoint_wire = wire_len(cp)? as u64;

    // 새 멤버 해소기: 같은 상태인지와 항목 수.
    let plain = open_snapshot(&STATE_KEY, d.seq, &d.state_hash, manifest, ct)?;
    let mut newcomer = Resolver::from_checkpoint_limits(
        genesis,
        decoded(cp, decrypt_body(&STATE_KEY, &cp.header, body_of(cp)?)?),
        &plain,
        limits,
    )
    .map_err(|e| anyhow!("{e}"))?;
    drop(plain);
    let tail_decoded = above
        .iter()
        .map(|ev| {
            Ok(decoded(
                ev,
                decrypt_body(&STATE_KEY, &ev.header, body_of(ev)?)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let up = newcomer.insert_events(tail_decoded);
    if let Some((_, e)) = up.rejected.first() {
        bail!("새 멤버가 경계 위 이벤트를 거부함: {e}");
    }
    if newcomer.state() != full.state() {
        bail!("새 멤버 상태가 전체 기록 노드와 다름");
    }
    let digest = state_digest(&full);
    let entries_newcomer = newcomer.len();
    drop(newcomer);

    // 다음 체크포인트: 해소기가 첫 체크포인트 앞 기록을 버림.
    let second = issue(&full)?;
    let t = Instant::now();
    let up = full.insert_events(vec![second.decoded]);
    let compact = t.elapsed();
    if up.applied.len() != 1 || full.base().map(|b| b.seq) != Some(second.d.seq) {
        bail!("다음 체크포인트가 바탕이 되지 않음: {:?}", up.rejected);
    }
    let entries_compacted = full.len();
    drop(full);

    // 차가운 시작은 새 프로세스에서.
    let dir = tempfile::Builder::new()
        .prefix("yangpa-bench-checkpoint-")
        .tempdir()?;
    let case = Case {
        genesis: genesis.0,
        seq: d.seq,
        state_hash: d.state_hash,
        manifest: manifest.clone(),
        max_members: limits.max_members,
    };
    std::fs::write(dir.path().join("case.json"), serde_json::to_vec(&case)?)?;
    std::fs::write(dir.path().join("events.bin"), postcard::to_stdvec(&stream)?)?;
    std::fs::write(dir.path().join("checkpoint.bin"), postcard::to_stdvec(cp)?)?;
    std::fs::write(dir.path().join("above.bin"), postcard::to_stdvec(&above)?)?;
    std::fs::write(dir.path().join("snapshot.bin"), ct)?;
    drop(stream);
    let cold = |mode| match exe {
        Some(exe) => cold_in_child(exe, dir.path(), mode),
        None => cold_start(dir.path(), mode),
    };
    let cold_full = cold(ColdMode::Full)?;
    let cold_checkpoint = cold(ColdMode::Checkpoint)?;
    if digest.is_none()
        || cold_full.state_digest != digest
        || cold_checkpoint.state_digest != digest
    {
        bail!("차가운 시작 두 방식의 상태가 다름");
    }

    Ok(CheckpointReport {
        members,
        tail,
        full_wire,
        checkpoint_wire,
        snapshot_plain: d.snapshot.len() as u64,
        snapshot_blob: manifest.size,
        frontier: d.frontier.len(),
        above_floor: above.len(),
        above_floor_wire: above_wire,
        newcomer_wire: checkpoint_wire + manifest.size + above_wire,
        generate,
        draft: first.draft,
        seal: first.seal,
        compact,
        entries_full,
        entries_compacted,
        entries_newcomer,
        cold_full,
        cold_checkpoint,
        peak_rss_mb: peak_rss_mb(),
    })
}

/// 낸 체크포인트 하나.
struct Issued {
    event: SignedEvent,
    decoded: DecodedEvent,
    ct: Vec<u8>,
    manifest: BlobManifest,
    draft: Duration,
    seal: Duration,
    d: CheckpointDraft,
}

fn body_of(ev: &SignedEvent) -> Result<&[u8]> {
    ev.body.as_deref().context("본문 없는 이벤트")
}

fn cold_in_child(exe: &Path, dir: &Path, mode: ColdMode) -> Result<ColdStart> {
    let mode_arg = match mode {
        ColdMode::Full => "full",
        ColdMode::Checkpoint => "checkpoint",
    };
    let out = std::process::Command::new(exe)
        .arg("scale-checkpoint-load")
        .arg(dir)
        .arg(mode_arg)
        .output()
        .context("scale-checkpoint-load 실행")?;
    if !out.status.success() {
        bail!(
            "scale-checkpoint-load 실패: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("scale-checkpoint-load 출력이 JSON이 아님")
}

/// 이벤트를 확인하고 엽니다(데몬이 저장된 이벤트를 다시 넣을 때처럼 서명 확인 + 복호화).
fn open_events(events: &[SignedEvent]) -> Result<Vec<DecodedEvent>> {
    events
        .iter()
        .map(|ev| {
            ev.verify()?;
            let body = decrypt_body(&STATE_KEY, &ev.header, body_of(ev)?)?;
            Ok(DecodedEvent {
                id: ev.id(),
                header: ev.header.clone(),
                body,
            })
        })
        .collect()
}

/// 새 프로세스: [`measure`]가 남긴 파일로 해소기를 세웁니다.
pub fn cold_start(dir: &Path, mode: ColdMode) -> Result<ColdStart> {
    let base_rss = rss_mb();
    let case: Case = serde_json::from_slice(&std::fs::read(dir.join("case.json"))?)?;
    let genesis = EventId(case.genesis);
    let limits = Limits {
        max_members: case.max_members,
    };
    let (read, open, resolve, resolver) = match mode {
        ColdMode::Full => {
            let t = Instant::now();
            let events: Vec<SignedEvent> =
                postcard::from_bytes(&std::fs::read(dir.join("events.bin"))?)?;
            let read = t.elapsed();
            let t = Instant::now();
            let decoded = open_events(&events)?;
            drop(events);
            let open = t.elapsed();
            let t = Instant::now();
            let mut r = Resolver::with_genesis_limits(genesis, limits);
            let up = r.insert_events(decoded);
            if let Some((_, e)) = up.rejected.first() {
                bail!("처음부터 해소에서 거부: {e}");
            }
            (read, open, t.elapsed(), r)
        }
        ColdMode::Checkpoint => {
            let t = Instant::now();
            let ct = std::fs::read(dir.join("snapshot.bin"))?;
            let cp: SignedEvent =
                postcard::from_bytes(&std::fs::read(dir.join("checkpoint.bin"))?)?;
            let above: Vec<SignedEvent> =
                postcard::from_bytes(&std::fs::read(dir.join("above.bin"))?)?;
            let read = t.elapsed();
            let t = Instant::now();
            let plain = open_snapshot(&STATE_KEY, case.seq, &case.state_hash, &case.manifest, &ct)?;
            drop(ct);
            let mut cp = open_events(std::slice::from_ref(&cp))?;
            let tail = open_events(&above)?;
            drop(above);
            let open = t.elapsed();
            let t = Instant::now();
            let cp = cp.pop().context("체크포인트 없음")?;
            let mut r = Resolver::from_checkpoint_limits(genesis, cp, &plain, limits)
                .map_err(|e| anyhow!("{e}"))?;
            drop(plain);
            let up = r.insert_events(tail);
            if let Some((_, e)) = up.rejected.first() {
                bail!("경계 위 이벤트 거부: {e}");
            }
            (read, open, t.elapsed(), r)
        }
    };
    Ok(ColdStart {
        read,
        open,
        resolve,
        rss_mb: rss_mb().zip(base_rss).map(|(a, b)| a - b),
        peak_rss_mb: peak_rss_mb(),
        entries: resolver.len(),
        members: resolver.state().map_or(0, |s| s.members.len()),
        state_digest: state_digest(&resolver),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 작은 크루로 체크포인트를 내고, 새 멤버·두 차가운 시작이 같은 상태에 이르며, 새 멤버가 받는 양과 해소기 항목이
    /// 전체 기록보다 작은지.
    #[test]
    fn small_crew_checkpoint() {
        let r = measure(80, 8, None).unwrap();
        assert!(r.frontier > 0 && r.above_floor >= 8, "{r:?}");
        assert!(r.newcomer_wire < r.full_wire, "{r:?}");
        assert!(r.entries_newcomer < r.entries_full, "{r:?}");
        assert!(r.entries_compacted < r.entries_full, "{r:?}");
        assert_eq!(r.cold_full.members, 81 + 8);
        assert_eq!(r.cold_checkpoint.entries, r.entries_newcomer);
    }
}
