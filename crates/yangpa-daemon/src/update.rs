//! 앱 업데이트(`update.*`): 공개 저장소(`yangpa_core::release::REPO`)의 최신 릴리스를 확인하고 릴리스 파일을 받습니다.
//!
//! Tor로 도는 기기는 Arti로만(받기 한 번마다 새 회로) 보내 GitHub가 이 기기의 IP를 보지 못하게 합니다. 오프라인 모드에서는
//! 막습니다(`presence::OFFLINE_BLOCKED`). 최신 판은 API(출구 IP마다 시간당 60번이라 Tor에서 자주 막힘) 대신
//! `releases/latest`가 넘겨주는 `…/releases/tag/v<판>` 주소에서 읽습니다(정식 판만, 시험판·초안은 빠짐).
//!
//! 받기: 서명한 `SHA256SUMS.txt`와 `.sig`를 먼저 받아 확인하고, 그 목록에 있는 그 판의 파일만 데이터 폴더 `updates/`에
//! `.part`로 받아 SHA-256이 맞을 때만 제 이름으로 바꿉니다. 목록·서명도 같은 폴더에 두어 앱이 설치 직전에 다시
//! 확인합니다(앱 `update.rs`). 진행은 `update.progress` 알림으로 보냅니다. 데몬이 뜰 때 지난 파일을 지웁니다.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};
use url::Url;
use yangpa_core::release::{self, REPO, SUMS, SUMS_SIG};
use yangpa_net::TorIsolation;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{Notification, UpdateDownloadParams, UpdateInfo};

use crate::blobs::preview::{Route, http_location, http_open};
use crate::node::Node;

/// 받는 파일 크기 상한(포터블 zip이 400MB 남짓).
const MAX_ASSET: u64 = 1 << 30;
/// 서명 목록·서명 크기 상한.
const MAX_SUMS: u64 = 64 * 1024;
/// 읽기마다 기다리는 시간. Tor 회로가 멈추면 끝냅니다(전체 시간은 제한하지 않음).
const IDLE: Duration = Duration::from_secs(60);
/// 확인 한 번의 시간 상한.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// 진행 알림 간격.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// 받는 주소로 허용하는 호스트(GitHub가 릴리스 파일을 저장소 호스트로 넘겨줌).
const HOSTS: [&str; 3] = [
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];
const DIR: &str = "updates";

#[derive(Default)]
pub(crate) struct UpdateState {
    /// 받는 중(한 번에 하나).
    busy: AtomicBool,
}

fn dir(node: &Node) -> PathBuf {
    node.cfg.data_dir.join(DIR)
}

/// 지난 업데이트 파일을 지웁니다(설치를 마쳤거나 그만둔 것). 설치 파일이 아직 돌아 못 지우면 다음에 지웁니다.
pub(crate) fn start(node: &Node) {
    if let Err(e) = std::fs::remove_dir_all(dir(node))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        debug!(error = %e, "지난 업데이트 파일을 지우지 못했습니다");
    }
}

fn route(node: &Node) -> Route {
    if crate::tor::tor_active(node) {
        Route::Tor {
            net: node.net.clone(),
            isolation: TorIsolation::new(),
        }
    } else {
        Route::Direct
    }
}

fn latest_url() -> Url {
    Url::parse(&format!("https://github.com/{REPO}/releases/latest")).expect("고정 주소")
}

pub(crate) async fn check(node: &Arc<Node>) -> Result<UpdateInfo, RpcError> {
    let found = tokio::time::timeout(CHECK_TIMEOUT, http_location(&route(node), &latest_url()))
        .await
        .ok()
        .flatten();
    let latest = found
        .filter(|(status, _)| (300..400).contains(status))
        .and_then(|(_, loc)| tag_version(&loc?))
        .ok_or_else(|| RpcError::network("업데이트를 확인하지 못했습니다"))?;
    let current = env!("CARGO_PKG_VERSION").to_owned();
    Ok(UpdateInfo {
        newer: release::is_newer(&latest, &current),
        current,
        latest,
    })
}

/// `releases/latest`가 넘겨준 주소(`https://github.com/<REPO>/releases/tag/v<판>`)의 판(`x.y.z`).
fn tag_version(location: &str) -> Option<String> {
    let u = latest_url().join(location).ok()?;
    if u.host_str() != Some("github.com") {
        return None;
    }
    let v = u.path().strip_prefix(&format!("/{REPO}/releases/tag/v"))?;
    release::parse_version(v)?;
    Some(v.to_owned())
}

/// 받기를 시작합니다(끝은 `update.progress`로). 이미 받는 중이면 그 진행이 이어집니다.
pub(crate) fn download(node: &Arc<Node>, p: UpdateDownloadParams) -> Result<(), RpcError> {
    if !release::valid_asset(&p.asset, &p.version) {
        return Err(RpcError::invalid_params(
            "릴리스 파일 이름이 올바르지 않습니다",
        ));
    }
    if node.update.busy.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let node = node.clone();
    tokio::spawn(async move {
        let r = fetch(&node, &p).await;
        node.update.busy.store(false, Ordering::Release);
        match r {
            Ok(total) => {
                info!("업데이트 파일을 받았습니다");
                publish(&node, &p.version, total, total, true, None);
            }
            Err(reason) => {
                warn!(reason, "업데이트 파일을 받지 못했습니다");
                publish(&node, &p.version, 0, 0, false, Some(reason));
            }
        }
    });
    Ok(())
}

fn publish(node: &Node, version: &str, done: u64, total: u64, ready: bool, error: Option<&str>) {
    node.notify.publish(Notification::UpdateProgress {
        version: version.to_owned(),
        done,
        total,
        ready,
        error: error.map(str::to_owned),
    });
}

fn allowed(u: &Url) -> bool {
    u.host_str().is_some_and(|h| HOSTS.contains(&h))
}

/// 받은 크기. 실패하면 까닭 코드(`unsigned`·`network`·`hash`·`failed`, 앱이 문구로 바꿈).
async fn fetch(node: &Arc<Node>, p: &UpdateDownloadParams) -> Result<u64, &'static str> {
    let route = route(node);
    let base = format!(
        "https://github.com/{REPO}/releases/download/v{}/",
        p.version
    );
    let sums = fetch_small(&route, &format!("{base}{SUMS}"))
        .await
        .ok_or("network")?;
    // 목록은 받았는데 서명이 없으면 서명하지 않은 릴리스입니다(서명 키가 생기기 전 판).
    let sig = fetch_small(&route, &format!("{base}{SUMS_SIG}"))
        .await
        .ok_or("unsigned")?;
    let want = release::signed_sum(&sums, &sig, &p.asset).ok_or("unsigned")?;
    let dir = dir(node);
    // 지난 판 파일을 지우고 새로 받습니다.
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|_| "failed")?;
    let part = dir.join(format!("{}.part", p.asset));
    let url = Url::parse(&format!("{base}{}", p.asset)).map_err(|_| "failed")?;
    let (got, total) = stream(&route, url, &part, |done, total| {
        publish(node, &p.version, done, total, false, None)
    })
    .await?;
    if got != want {
        let _ = tokio::fs::remove_file(&part).await;
        return Err("hash");
    }
    // 앱은 파일이 제 이름일 때만 설치하므로 목록·서명을 먼저 둡니다.
    for (name, bytes) in [(SUMS, &sums), (SUMS_SIG, &sig)] {
        tokio::fs::write(dir.join(name), bytes)
            .await
            .map_err(|_| "failed")?;
    }
    tokio::fs::rename(&part, dir.join(&p.asset))
        .await
        .map_err(|_| "failed")?;
    Ok(total)
}

async fn fetch_small(route: &Route, url: &str) -> Option<Vec<u8>> {
    let body = http_open(route, Url::parse(url).ok()?, allowed, MAX_SUMS).await?;
    let mut out = body.rest;
    let more = body.len.saturating_sub(out.len() as u64);
    tokio::time::timeout(IDLE, body.io.take(more).read_to_end(&mut out))
        .await
        .ok()?
        .ok()?;
    out.truncate(usize::try_from(body.len).ok()?);
    (out.len() as u64 == body.len).then_some(out)
}

/// 본문을 `part`에 쓰며 SHA-256(소문자 hex)과 크기를 돌려줍니다. 진행은 `progress(받은, 전체)`로 0.5초마다.
async fn stream(
    route: &Route,
    url: Url,
    part: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<(String, u64), &'static str> {
    let mut body = http_open(route, url, allowed, MAX_ASSET)
        .await
        .ok_or("network")?;
    let total = body.len;
    let mut file = tokio::fs::File::create(part).await.map_err(|_| "failed")?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let rest = std::mem::take(&mut body.rest);
    let mut chunk: &[u8] = &rest;
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        // 길이보다 더 온 것은 버립니다.
        let take = chunk
            .len()
            .min(usize::try_from(total - done).unwrap_or(usize::MAX));
        hash.update(&chunk[..take]);
        file.write_all(&chunk[..take]).await.map_err(|_| "failed")?;
        done += take as u64;
        if done >= total {
            break;
        }
        if last.elapsed() >= PROGRESS_EVERY {
            progress(done, total);
            last = Instant::now();
        }
        let n = tokio::time::timeout(IDLE, body.io.read(&mut buf))
            .await
            .map_err(|_| "network")?
            .map_err(|_| "network")?;
        if n == 0 {
            return Err("network");
        }
        chunk = &buf[..n];
    }
    file.sync_all().await.map_err(|_| "failed")?;
    let hex = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((hex, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tag_from_redirect() {
        let ok = format!("https://github.com/{REPO}/releases/tag/v1.2.3");
        assert_eq!(tag_version(&ok).as_deref(), Some("1.2.3"));
        assert_eq!(
            tag_version(&format!("/{REPO}/releases/tag/v1.2.3")).as_deref(),
            Some("1.2.3")
        );
        for bad in [
            format!("https://github.com/{REPO}/releases/tag/v1.2.3-rc.1"),
            format!("https://github.com/{REPO}/releases/tag/1.2.3"),
            format!("https://github.com/{REPO}/releases"),
            format!("https://evil.example/{REPO}/releases/tag/v9.9.9"),
            "https://github.com/someone/else/releases/tag/v9.9.9".to_owned(),
        ] {
            assert_eq!(tag_version(&bad), None, "{bad}");
        }
    }

    #[test]
    fn only_github_hosts() {
        assert!(allowed(&Url::parse("https://github.com/x").unwrap()));
        assert!(allowed(
            &Url::parse("https://release-assets.githubusercontent.com/x").unwrap()
        ));
        assert!(!allowed(
            &Url::parse("https://github.com.evil.example/x").unwrap()
        ));
        assert!(!allowed(&Url::parse("https://example.com/x").unwrap()));
    }

    /// 실제 릴리스 파일(CLI zip 24MB)을 직접 받아 목록의 SHA-256과 맞는지. 네트워크가 필요해 평소에는 돌지 않습니다:
    /// `cargo test -p yangpa-daemon --lib update -- --ignored`
    #[tokio::test]
    #[ignore = "GitHub에 직접 접속"]
    async fn streams_real_release_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = format!("https://github.com/{REPO}/releases/download/v1.0.0/");
        let sums = fetch_small(&Route::Direct, &format!("{base}{SUMS}"))
            .await
            .expect("목록");
        let asset = "yangpa-cli-1.0.0-x86_64-pc-windows-msvc.zip";
        let want = std::str::from_utf8(&sums)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_suffix(asset)?.split_whitespace().next())
            .unwrap()
            .to_owned();
        let url = Url::parse(&format!("{base}{asset}")).unwrap();
        let (got, total) = stream(&Route::Direct, url, &dir.path().join("a.part"), |_, _| {})
            .await
            .expect("받기");
        assert_eq!(got, want);
        assert_eq!(
            std::fs::metadata(dir.path().join("a.part")).unwrap().len(),
            total
        );
    }
}
