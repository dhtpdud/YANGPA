//! Tor 브리지(검열 우회, 설정 `net.bridges`, 기본 꺼짐, protocol.md "Tor 브리지").
//!
//! - 브리지 전송(obfs4·snowflake·webtunnel·meek_lite)은 Tor 프로젝트의 lyrebird가 맡습니다. 설치본·포터블은 실행
//!   파일 옆 `pt` 폴더에 두고, 없으면 실행 파일 옆과 PATH(리눅스 패키지, Windows는 빼고)에서 찾습니다.
//! - 자동 Snowflake: 브리지 설정을 한 번도 정하지 않은 기기가 Tor에 들어가지 못하고 부트스트랩 진행률이 [`STALL`] 동안
//!   그대로면, 내장 Snowflake를 켜 보고 [`TRIAL`] 안에 들어가면 켠 채로 저장하고 앱에 알립니다(`net.bridges`, `auto`).
//!   못 들어가면(인터넷이 아예 없는 경우 등) 되돌리고 [`RETRY`] 뒤에 다시 해 봅니다. 사용자가 정한 설정(끔 포함)은
//!   덮어쓰지 않습니다.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use yangpa_net::TorBridges;
use yangpa_net::config::{BUILTIN_OBFS4, BUILTIN_SNOWFLAKE};
use yangpa_rpc::api::{BridgeKind, BridgesView, Notification};
use yangpa_store::Store;

use crate::error::Result;
use crate::node::Node;

/// 설정 키. 없으면 한 번도 정하지 않은 것(자동 Snowflake를 할 수 있음).
pub(crate) const SETTING: &str = "net.bridges";
/// 부트스트랩을 살피는 간격.
const CHECK: Duration = Duration::from_secs(5);
/// 진행률이 이만큼 그대로면 막힌 것으로 봅니다(느려도 나아가는 부트스트랩은 건드리지 않게).
const STALL: Duration = Duration::from_secs(90);
/// Snowflake로 들어가기를 기다리는 시간(실제로 40초~2분).
const TRIAL: Duration = Duration::from_secs(180);
/// Snowflake로도 못 들어갔을 때 다시 해 보기까지.
const RETRY: Duration = Duration::from_secs(30 * 60);

/// 저장하는 설정.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stored {
    pub enabled: bool,
    #[serde(default)]
    pub kind: BridgeKind,
    #[serde(default)]
    pub custom: Vec<String>,
    /// 데몬이 스스로 켰음(자동 Snowflake).
    #[serde(default)]
    pub auto: bool,
}

fn load(store: &Store) -> Option<Stored> {
    store.get_setting::<Stored>(SETTING).unwrap_or_else(|e| {
        warn!(error = %e, "브리지 설정을 읽지 못함");
        None
    })
}

/// lyrebird 실행 파일(실행 파일 옆 `pt`, 실행 파일 옆, 그다음 Windows가 아니면 PATH 순).
pub(crate) fn lyrebird() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(crate::config::ENV_LYREBIRD).map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let name = if cfg!(windows) {
        "lyrebird.exe"
    } else {
        "lyrebird"
    };
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));
    let near = exe_dir.into_iter().flat_map(|d| [d.join("pt"), d]);
    // Windows는 배포본에 함께 넣으므로 사용자가 쓸 수 있는 PATH 폴더의 아무 lyrebird.exe나 띄우지 않습니다.
    let path = if cfg!(windows) {
        Vec::new()
    } else {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default()
    };
    near.chain(path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// 저장한 설정으로 Arti에 줄 브리지(꺼져 있거나 lyrebird가 없으면 `None`).
fn tor_bridges(s: &Stored) -> Option<TorBridges> {
    if !s.enabled {
        return None;
    }
    let lines: Vec<String> = match s.kind {
        BridgeKind::Snowflake => BUILTIN_SNOWFLAKE.iter().map(|l| l.to_string()).collect(),
        BridgeKind::Obfs4 => BUILTIN_OBFS4.iter().map(|l| l.to_string()).collect(),
        BridgeKind::Custom => s.custom.clone(),
    };
    if lines.is_empty() {
        return None;
    }
    let Some(lyrebird) = lyrebird() else {
        warn!("브리지가 켜져 있지만 lyrebird가 없어 브리지 없이 Tor를 씀");
        return None;
    };
    Some(TorBridges { lines, lyrebird })
}

/// 네트워크를 띄울 때 쓸 브리지([`crate::tor`]).
pub(crate) fn configured(store: &Store) -> Option<TorBridges> {
    load(store).as_ref().and_then(tor_bridges)
}

fn view_of(s: &Stored) -> BridgesView {
    BridgesView {
        enabled: s.enabled,
        kind: s.kind,
        custom: s.custom.clone(),
        qr: (!s.custom.is_empty())
            .then(|| crate::qr::encode(&s.custom.join("\n")).ok())
            .flatten(),
        auto: s.auto,
        available: lyrebird().is_some(),
    }
}

pub(crate) fn view(node: &Node) -> BridgesView {
    view_of(&load(&node.store).unwrap_or_default())
}

/// 사용자가 정한 설정을 바로 쓰고 저장합니다. 입력 검사는 API가 합니다.
pub(crate) fn set(node: &Node, s: Stored) -> Result<()> {
    node.net.set_tor_bridges(tor_bridges(&s))?;
    node.store.put_setting(SETTING, &s)?;
    node.notify.publish(Notification::Bridges(view_of(&s)));
    Ok(())
}

/// 부트스트랩 진행률의 최고값이 오르지 않은 시간을 잽니다. 연결했다 끊기는 차단(DPI)은 진행률을 오르내리게 하므로
/// 바뀐 것이 아니라 최고값이 오른 것만 나아간 것으로 봅니다.
#[derive(Default)]
struct Stall {
    best: Option<(u8, Instant)>,
}

impl Stall {
    /// 최고 진행률이 [`STALL`] 동안 오르지 않았는지.
    fn stuck(&mut self, pct: u8, now: Instant) -> bool {
        match self.best {
            Some((best, t)) if pct <= best => now.duration_since(t) >= STALL,
            _ => {
                self.best = Some((pct, now));
                false
            }
        }
    }
}

/// 자동 Snowflake 작업을 띄웁니다(직접 연결 노드·lyrebird가 없는 빌드는 하지 않음).
pub(crate) fn start(node: &Arc<Node>) {
    if node.cfg.net.direct || lyrebird().is_none() {
        return;
    }
    node.spawn_detached(|node| async move {
        let mut stall = Stall::default();
        loop {
            tokio::time::sleep(CHECK).await;
            if load(&node.store).is_some() {
                return;
            }
            let Some((pct, ready)) = node.net.tor_bootstrap() else {
                stall = Stall::default();
                continue;
            };
            if ready {
                return;
            }
            if !stall.stuck(pct, Instant::now()) {
                continue;
            }
            if try_snowflake(&node).await {
                return;
            }
            tokio::time::sleep(RETRY).await;
            stall = Stall::default();
        }
    });
}

/// 내장 Snowflake를 켜 보고, 들어가면 저장하고 알립니다. 못 들어가면 되돌립니다.
async fn try_snowflake(node: &Arc<Node>) -> bool {
    info!("Tor에 들어가지 못해 Snowflake 브리지를 시험함");
    let trial = Stored {
        enabled: true,
        kind: BridgeKind::Snowflake,
        custom: Vec::new(),
        auto: true,
    };
    if let Err(e) = node.net.set_tor_bridges(tor_bridges(&trial)) {
        warn!(error = %e, "Snowflake 브리지를 켜지 못함");
        return false;
    }
    let deadline = Instant::now() + TRIAL;
    while Instant::now() < deadline {
        tokio::time::sleep(CHECK).await;
        // 그 사이 사용자가 정했으면 그 설정이 이미 쓰이고 있습니다.
        if load(&node.store).is_some() {
            return true;
        }
        if node.net.tor_bootstrap().is_some_and(|(_, ready)| ready) {
            info!("Snowflake 브리지로 Tor에 들어감, 켠 채로 둠");
            if let Err(e) = node.store.put_setting(SETTING, &trial) {
                warn!(error = %e, "브리지 설정을 저장하지 못함");
            }
            node.notify.publish(Notification::Bridges(view_of(&trial)));
            return true;
        }
    }
    info!("Snowflake 브리지로도 Tor에 들어가지 못해 되돌림");
    if let Err(e) = node.net.set_tor_bridges(configured(&node.store)) {
        warn!(error = %e, "브리지를 되돌리지 못함");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_needs_unchanged_progress_for_the_whole_window() {
        let t0 = Instant::now();
        let mut s = Stall::default();
        assert!(!s.stuck(10, t0));
        assert!(!s.stuck(10, t0 + STALL / 2));
        // 나아가면 처음부터 다시 잽니다.
        assert!(!s.stuck(20, t0 + STALL));
        assert!(!s.stuck(20, t0 + STALL + STALL / 2));
        assert!(s.stuck(20, t0 + STALL * 2));
        // 연결했다 끊기기를 되풀이해 오르내려도 최고값이 그대로면 막힌 것.
        let mut s = Stall::default();
        assert!(!s.stuck(20, t0));
        assert!(!s.stuck(10, t0 + STALL / 2));
        assert!(!s.stuck(20, t0 + STALL / 2 + STALL / 4));
        assert!(s.stuck(15, t0 + STALL));
    }

    #[test]
    fn bridges_follow_setting() {
        assert_eq!(tor_bridges(&Stored::default()), None, "기본은 꺼짐");
        let custom_empty = Stored {
            enabled: true,
            kind: BridgeKind::Custom,
            ..Stored::default()
        };
        assert_eq!(tor_bridges(&custom_empty), None, "줄이 없으면 브리지 없이");
        // 예전 판이 남긴 값이 없어도 읽힘(빠진 필드는 기본값).
        let old: Stored = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert_eq!(old.kind, BridgeKind::Snowflake);
    }
}
