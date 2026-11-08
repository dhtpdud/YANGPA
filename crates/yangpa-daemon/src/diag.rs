//! 진단: 전달 지연·처리량·자원 지표(`diag.metrics`), 휘발성 누출 검사(`diag.leakcheck`).
//!
//! 적용 경로가 묶음마다 [`on_ingest`]를 부르므로 여기서는 숫자만 짧게 더합니다(잠금 하나, 내용·키 없음).
//! 메모리·CPU는 `diag.metrics`를 부를 때만 운영체제에서 읽습니다.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use yangpa_core::body::Secret32;
use yangpa_core::event::{Lane, SignedEvent};
use yangpa_core::ids::{BlobId, EventId};
use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{DiagMetrics, LeakcheckResult};

use crate::node::Node;
use crate::{blobs, volatile};

/// 전달 지연 표본 수(최근 것만).
const LATENCY_SAMPLES: usize = 1024;
/// 시계가 크게 어긋난 기기의 표본이 분위수를 망치지 않게 자르는 값.
const MAX_LATENCY_MS: u64 = 5 * 60 * 1000;
/// 초당 이벤트를 셀 구간(지난 몇 초의 평균).
const RATE_WINDOW_SECS: u64 = 10;
/// 누출 검사 표식 길이(바이트, 저장소 검사와 같음).
const MARKER_LEN: std::ops::RangeInclusive<usize> = 6..=1024;

/// 진단 영역의 노드 수준 상태(`Node::diag`).
#[derive(Default)]
pub struct DiagState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// 받은 메시지 레인 이벤트의 전달 지연(ms).
    latency: VecDeque<u64>,
    /// (유닉스 초, 그 초에 받아들인 이벤트 수).
    per_sec: VecDeque<(u64, u64)>,
    /// 지난 `diag.metrics` 때의 (시각, 누적 CPU ms).
    cpu: Option<(Instant, u64)>,
}

pub(crate) fn start(_node: &Arc<Node>) {}

/// 피어가 보낸 묶음에서 메시지 레인 이벤트의 작성 시각(전달 지연 계산용). 다른 출처면 부르지 않습니다.
pub(crate) fn sent_times(events: &[SignedEvent]) -> Vec<(EventId, Timestamp)> {
    events
        .iter()
        .filter(|e| e.header.kind.lane() == Lane::Message)
        .map(|e| (e.id(), e.header.created_at))
        .collect()
}

/// 적용 한 묶음이 끝날 때. `accepted`는 새로 받아들인 이벤트, `sent`는 [`sent_times`].
pub(crate) fn on_ingest(node: &Node, accepted: &[EventId], sent: &[(EventId, Timestamp)]) {
    if accepted.is_empty() {
        return;
    }
    let now = Timestamp::now().as_millis();
    let sec = now / 1000;
    let delays: Vec<u64> = if sent.is_empty() {
        Vec::new()
    } else {
        let ok: HashSet<&EventId> = accepted.iter().collect();
        sent.iter()
            .filter(|(id, _)| ok.contains(id))
            .map(|(_, at)| now.saturating_sub(at.as_millis()).min(MAX_LATENCY_MS))
            .collect()
    };
    let mut g = node.diag.inner.lock();
    let n = accepted.len() as u64;
    match g.per_sec.back_mut() {
        Some((s, c)) if *s == sec => *c += n,
        _ => g.per_sec.push_back((sec, n)),
    }
    while g
        .per_sec
        .front()
        .is_some_and(|(s, _)| s.saturating_add(RATE_WINDOW_SECS) < sec)
    {
        g.per_sec.pop_front();
    }
    for d in delays {
        if g.latency.len() >= LATENCY_SAMPLES {
            g.latency.pop_front();
        }
        g.latency.push_back(d);
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * p).div_ceil(100).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// 지난 호출 이후 CPU 사용률(%, 코어 여러 개면 100을 넘을 수 있음). 지난 표본이 없으면(첫 호출) 0(docs/api.md).
fn cpu_percent(prev: Option<(Instant, u64)>, now: Instant, cpu_ms: u64) -> f64 {
    let Some((t, before)) = prev else {
        return 0.0;
    };
    let wall_ms = now.saturating_duration_since(t).as_millis() as f64;
    if wall_ms > 0.0 {
        cpu_ms.saturating_sub(before) as f64 / wall_ms * 100.0
    } else {
        0.0
    }
}

/// 이 프로세스의 메모리(RSS 바이트)와 지난 호출 이후 CPU 사용률([`cpu_percent`]).
fn process_usage(node: &Node) -> (u64, f64) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(pid) = sysinfo::get_current_pid() else {
        return (0, 0.0);
    };
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let Some(p) = sys.process(pid) else {
        return (0, 0.0);
    };
    let memory = p.memory();
    let cpu_ms = p.accumulated_cpu_time();
    let now = Instant::now();
    let prev = node.diag.inner.lock().cpu.replace((now, cpu_ms));
    (memory, cpu_percent(prev, now, cpu_ms))
}

/// `diag.metrics`.
pub(crate) fn metrics(node: &Node) -> DiagMetrics {
    let (p50, p95, eps) = {
        let g = node.diag.inner.lock();
        let mut sorted: Vec<u64> = g.latency.iter().copied().collect();
        sorted.sort_unstable();
        let now_sec = Timestamp::now().as_millis() / 1000;
        // 지금 진행 중인 초는 빼고, 지난 RATE_WINDOW_SECS초 평균.
        let total: u64 = g
            .per_sec
            .iter()
            .filter(|(s, _)| *s < now_sec && s.saturating_add(RATE_WINDOW_SECS) >= now_sec)
            .map(|(_, c)| *c)
            .sum();
        (
            percentile(&sorted, 50),
            percentile(&sorted, 95),
            total as f64 / RATE_WINDOW_SECS as f64,
        )
    };
    let (memory_bytes, cpu_percent) = process_usage(node);
    let mut queues = BTreeMap::new();
    let (mut held, mut waiting) = (0u64, 0u64);
    for rt in node.spaces.read().values() {
        // 적용 중인 공간은 건너뜁니다(진단이 적용을 기다리게 하지 않음).
        if let Some(core) = rt.core.try_lock() {
            held += core.held.len() as u64;
            waiting += core.waiting.values().map(|s| s.len() as u64).sum::<u64>();
        }
    }
    queues.insert("ingest_held".to_owned(), held);
    queues.insert("ingest_waiting".to_owned(), waiting);
    let v = volatile::stats(node);
    queues.insert("volatile_channels".to_owned(), v.channels as u64);
    queues.insert("volatile_events".to_owned(), v.events as u64);
    queues.insert("volatile_event_bytes".to_owned(), v.event_bytes as u64);
    queues.insert("volatile_keys".to_owned(), v.keys as u64);
    queues.insert("volatile_blobs".to_owned(), v.blobs as u64);
    queues.insert("volatile_blob_bytes".to_owned(), v.blob_bytes as u64);
    queues.insert("volatile_blob_reserved".to_owned(), v.blob_reserved as u64);
    DiagMetrics {
        delivery_ms_p50: p50,
        delivery_ms_p95: p95,
        memory_bytes,
        cpu_percent,
        queues,
        events_per_sec: eps,
    }
}

/// `diag.leakcheck`: 데이터 디렉터리(복호화한 DB, 봉인 열, 모든 파일)와 설정된 로그 파일에서 표식을 찾습니다.
/// 첨부도 봅니다: 키를 아는 디스크 블롭은 풀어서 평문을 훑고(`blob:`), 지금 RAM에 있는 휘발성 첨부가 디스크 블롭
/// 저장소에도 있으면 알립니다(`blob-store:`). 표식은 메모리에서만 쓰고 어디에도 기록하지 않습니다. 결과에는
/// 위치만 담습니다.
pub(crate) async fn leakcheck(
    node: &Arc<Node>,
    marker: String,
) -> std::result::Result<LeakcheckResult, RpcError> {
    if !MARKER_LEN.contains(&marker.len()) {
        return Err(RpcError::invalid_input("표식은 6~1024바이트여야 합니다"));
    }
    let store = node.store.clone();
    let data_dir = node.cfg.data_dir.clone();
    let log_file = node.cfg.log_file.clone();
    let blob_keys = disk_blob_keys(node);
    let ram_blobs = volatile::ram_blob_ids(node);
    let scan = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
        let bytes = zeroize::Zeroizing::new(marker.into_bytes());
        let mut findings = store
            .scan_for_marker(&bytes)
            .map_err(|e| format!("저장소 검사 실패: {e}"))?;
        findings.extend(
            store
                .scan_blobs_for_marker(&bytes, &blob_keys)
                .map_err(|e| format!("블롭 검사 실패: {e}"))?,
        );
        for id in ram_blobs {
            if store
                .blob_manifest(&id)
                .map_err(|e| format!("블롭 검사 실패: {e}"))?
                .is_some()
            {
                findings.push(format!("blob-store:{}(휘발성 첨부)", id.short()));
            }
        }
        if let Some(log) = log_file.filter(|p| !inside(p, &data_dir)) {
            match yangpa_store::file_contains_marker(&log, &bytes) {
                Ok(true) => findings.push(format!("log:{}", log.display())),
                Ok(false) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("로그 파일 검사 실패: {e}")),
            }
        }
        Ok(findings)
    });
    let findings = scan
        .await
        .map_err(|_| RpcError::internal("누출 검사 작업이 끝나지 않음"))?
        .map_err(RpcError::internal)?;
    Ok(LeakcheckResult {
        clean: findings.is_empty(),
        findings,
    })
}

/// 디스크 블롭 중 키를 아는 것(첨부를 실은 메시지·이모지·아이콘·프로필 이미지). 키를 모르는 블롭(남의 것을
/// 시더로 들고 있는 것)은 풀 수 없어 훑지 못합니다.
fn disk_blob_keys(node: &Node) -> Vec<(BlobId, Secret32)> {
    let mut out = Vec::new();
    for space in node.space_ids() {
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        for blob in node.store.blobs_in_space(&space).unwrap_or_default() {
            let key_from = blobs::refs_of(node, &blob)
                .into_iter()
                .find(|r| r.space == space)
                .map_or(EventId::from_bytes([0; 32]), |r| r.event);
            if let Some(k) = blobs::key_for(node, &space, &state, &blob, &key_from) {
                out.push((blob, k));
            }
        }
    }
    for blob in node
        .store
        .blobs_in_space(&blobs::PROFILE_SPACE)
        .unwrap_or_default()
    {
        if let Some(k) = blobs::profile_image_key(node, &blob) {
            out.push((blob, k));
        }
    }
    out
}

/// 로그 파일이 데이터 디렉터리 안이면 저장소 검사가 이미 훑습니다.
fn inside(path: &Path, dir: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    canon(path).starts_with(canon(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 95), 7);
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50), 50);
        assert_eq!(percentile(&v, 95), 95);
        assert_eq!(percentile(&v[..10], 95), 10);
    }

    #[test]
    fn cpu_percent_is_zero_on_first_sample() {
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_millis(2000);
        assert_eq!(cpu_percent(None, t1, 123_456), 0.0, "첫 호출은 0");
        assert_eq!(cpu_percent(Some((t0, 1000)), t1, 2000), 50.0);
        assert_eq!(cpu_percent(Some((t0, 1000)), t1, 5000), 200.0, "여러 코어");
        assert_eq!(cpu_percent(Some((t1, 1000)), t1, 2000), 0.0, "같은 시각");
        assert_eq!(
            cpu_percent(Some((t0, 9000)), t1, 2000),
            0.0,
            "줄어든 누적값"
        );
    }
}
