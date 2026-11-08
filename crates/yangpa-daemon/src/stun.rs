//! STUN 서버 목록(`net.stun_servers` 설정).
//!
//! 직접 연결로 나가는 경로(P2P 스웜, SFU 호스트)가 공유기 밖 주소를 알아낼 때만 씁니다. Tor 연결에는 쓰지 않습니다.
//! STUN 서버는 묻는 기기의 공인 IP를 보므로, 목록은 사람만 바꿉니다(`net.set_stun_servers`는 사람 전용).
//! 저장한 적이 없으면 [`crate::config::NodeConfig::default_stun_servers`]를 씁니다.
//! 크루 STUN 서버(크루 상태 `crew_servers.stun`, `CrewServersSet`)는 역할 권한 `use_crew_stun`이 있고 이 기기가 쓰기로 한
//! 크루의 것만, 내 목록 뒤에 더해 묻고 통화 ICE 서버에도 붙입니다.

use std::collections::BTreeSet;
use std::net::Ipv6Addr;
use std::sync::{Arc, Mutex};

use tracing::warn;
use yangpa_core::ids::{SpaceId, UserId};
use yangpa_core::model::{
    MAX_CREW_SERVER_LEN, MAX_CREW_SERVERS, Permissions, SpaceKind, SpaceState,
};
use yangpa_rpc::api::{CrewServers, IceServer, SpaceView, StunServers};

use crate::error::{Error, Result};
use crate::node::Node;

pub(crate) const SETTING_STUN: &str = "net.stun_servers";
/// 목록 한도.
pub(crate) const MAX_STUN_SERVERS: usize = 8;
/// 호스트 이름 한도(DNS 이름 최대 길이).
const MAX_HOST: usize = 253;
/// 포트를 적지 않았을 때(STUN 표준 포트).
const DEFAULT_PORT: u16 = 3478;

/// 지금 쓸 STUN 서버(`host:port`, 앞쪽부터). 빈 목록이면 STUN을 쓰지 않습니다.
pub(crate) fn stun_servers(node: &Node) -> Vec<String> {
    current(node).0
}

/// `net.stun_servers` 결과.
pub(crate) fn view(node: &Node) -> StunServers {
    let (servers, is_default) = current(node);
    StunServers {
        servers,
        is_default,
    }
}

/// (목록, 기본 목록인지).
fn current(node: &Node) -> (Vec<String>, bool) {
    match node.store.get_setting::<Vec<String>>(SETTING_STUN) {
        // 저장할 때 검사했지만, 손댄 DB나 예전 형식이 남아도 이상한 값으로 나가지 않게 한 번 더 거릅니다.
        Ok(Some(saved)) => (
            saved
                .iter()
                .filter_map(|s| normalize(s).ok())
                .take(MAX_STUN_SERVERS)
                .collect(),
            false,
        ),
        Ok(None) => (node.cfg.default_stun_servers.clone(), true),
        // 설정을 못 읽으면 바깥에 묻지 않는 쪽으로 둡니다.
        Err(e) => {
            warn!(error = %e, "STUN 서버 설정을 읽지 못함");
            (Vec::new(), false)
        }
    }
}

/// 목록을 저장합니다. `None`이면 저장한 목록을 지워 기본 목록으로, 빈 목록이면 STUN을 쓰지 않습니다.
pub(crate) fn set(node: &Node, servers: Option<Vec<String>>) -> Result<()> {
    let Some(list) = servers else {
        node.store.delete_setting(SETTING_STUN)?;
        return Ok(());
    };
    let list = normalize_list(&list)?;
    node.store.put_setting(SETTING_STUN, &list)?;
    Ok(())
}

fn normalize_list(list: &[String]) -> Result<Vec<String>> {
    if list.len() > MAX_STUN_SERVERS {
        return Err(Error::invalid(format!(
            "STUN 서버는 {MAX_STUN_SERVERS}개까지 둘 수 있음"
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for s in list {
        let n = normalize(s)?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
}

/// 항목 하나를 `host:port`로 맞춥니다. 앞의 `stun:`은 떼고, 포트가 없으면 3478. TURN·사용자 이름·비밀번호·경로가
/// 든 주소는 거절합니다(이 목록은 주소 알아내기 전용).
pub(crate) fn normalize(entry: &str) -> Result<String> {
    // 호스트 이름은 대소문자를 가리지 않으므로 소문자로 맞춥니다(같은 서버가 두 번 들어가지 않게).
    let s = entry.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err(Error::invalid("STUN 서버 주소가 비어 있음"));
    }
    if s.starts_with("turn:") || s.starts_with("turns:") {
        return Err(Error::invalid(
            "TURN 서버는 쓸 수 없음(여기는 STUN 서버의 host:port만)",
        ));
    }
    let s = s.strip_prefix("stun:").unwrap_or(&s);
    if s.contains('@') {
        return Err(Error::invalid(
            "사용자 이름·비밀번호가 든 주소는 쓸 수 없음",
        ));
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) || s.contains(['/', '?', '#', '\\']) {
        return Err(Error::invalid("STUN 서버는 host:port 꼴로 적어야 함"));
    }
    let bad = || Error::invalid("STUN 서버는 host:port 꼴로 적어야 함");
    // `[v6]:port`, 괄호 없는 IPv6, `host:port`, `host`.
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest.split_once(']').ok_or_else(bad)?;
        // 괄호는 IPv6 주소에만 씁니다.
        h.parse::<Ipv6Addr>().map_err(|_| bad())?;
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':').ok_or_else(bad)?),
        };
        (h, port)
    } else if s.parse::<Ipv6Addr>().is_ok() {
        (s, None)
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        }
    };
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| Error::invalid("STUN 서버 포트가 잘못됨(1~65535)"))?,
    };
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        return Ok(format!("[{v6}]:{port}"));
    }
    if host.is_empty() || host.len() > MAX_HOST || !valid_hostname(host) {
        return Err(Error::invalid("STUN 서버 이름이 잘못됨"));
    }
    Ok(format!("{host}:{port}"))
}

/// DNS 이름이나 IPv4 주소 꼴인지(점으로 나뉜 영숫자·하이픈 이름표).
fn valid_hostname(host: &str) -> bool {
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

// ───────────── 크루 STUN 서버 ─────────────

/// 크루 서버를 쓰기로 한 크루(`crew.use_servers`). 키 이름은 TURN이 있던 때 그대로입니다(저장된 선택을 그대로 읽게).
const SETTING_CREWS: &str = "turn.crews";
/// 쓰기로 한 크루 한도.
const MAX_CREWS: usize = 4096;

/// 크루 목록 읽고-고쳐-쓰기를 한 번에 하나만.
static CREWS_WRITE: Mutex<()> = Mutex::new(());

fn crews(node: &Node) -> BTreeSet<SpaceId> {
    match node.store.get_setting::<Vec<String>>(SETTING_CREWS) {
        Ok(v) => v
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect(),
        // 못 읽으면 크루 서버를 쓰지 않는 쪽으로 둡니다.
        Err(e) => {
            warn!(error = %e, "크루 서버 설정을 읽지 못함");
            BTreeSet::new()
        }
    }
}

/// `crew.use_servers`: 이 기기가 그 크루의 STUN 서버를 쓸지 저장합니다.
pub(crate) fn set_crew_use(node: &Arc<Node>, space: SpaceId, enabled: bool) -> Result<()> {
    let _w = CREWS_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = crews(node);
    let changed = if enabled {
        list.insert(space)
    } else {
        list.remove(&space)
    };
    if !changed {
        return Ok(());
    }
    if list.len() > MAX_CREWS {
        return Err(Error::invalid("크루 서버를 쓰는 크루가 너무 많음"));
    }
    let v: Vec<String> = list.iter().map(|s| s.to_string()).collect();
    if v.is_empty() {
        node.store.delete_setting(SETTING_CREWS)?;
    } else {
        node.store.put_setting(SETTING_CREWS, &v)?;
    }
    Ok(())
}

/// 크루 STUN 목록을 맞춥니다(`crew.set_servers`).
pub(crate) fn normalize_crew(stun: &[String]) -> Result<Vec<String>> {
    let mut s: Vec<String> = Vec::new();
    for l in stun.iter().filter(|l| !l.trim().is_empty()) {
        let n = normalize(l)?;
        if !s.contains(&n) {
            s.push(n);
        }
    }
    if s.len() > MAX_CREW_SERVERS {
        return Err(Error::invalid(format!(
            "크루 STUN 서버는 {MAX_CREW_SERVERS}개까지 둘 수 있음"
        )));
    }
    if s.iter().any(|l| l.len() > MAX_CREW_SERVER_LEN) {
        return Err(Error::invalid("크루 서버 한 줄이 너무 김"));
    }
    Ok(s)
}

/// 이 크루 상태에서 내가 쓸 수 있는 권한(크루이고 멤버이며 역할 권한이 있음).
fn crew_allows(state: &SpaceState, me: &UserId, perm: Permissions) -> bool {
    state.kind == SpaceKind::Crew
        && state.is_member(me)
        && state.base_permissions(me).contains(perm)
}

/// 이 크루에서 지금 쓸 크루 STUN 서버(쓰기로 했고 권한이 있을 때).
fn crew_stun_of(
    used: &BTreeSet<SpaceId>,
    space: &SpaceId,
    state: &SpaceState,
    me: &UserId,
) -> Vec<String> {
    if !used.contains(space) || !crew_allows(state, me, Permissions::USE_CREW_STUN) {
        return Vec::new();
    }
    state
        .crew_servers
        .stun
        .iter()
        .filter_map(|s| normalize(s).ok())
        .collect()
}

/// 지금 쓸 크루 STUN 서버 전부(쓰기로 한 크루의 것, 앞쪽 크루부터). 직접 스웜의 공인 주소 묻기에 내 목록 뒤로 붙습니다.
pub(crate) fn crew_stun_all(node: &Node) -> Vec<String> {
    let Ok(me) = node.me() else {
        return Vec::new();
    };
    let me = me.user();
    let used = crews(node);
    let mut out: Vec<String> = Vec::new();
    for space in &used {
        if let Some(st) = node.space_state(space) {
            for s in crew_stun_of(&used, space, &st, &me) {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// `space.get`의 크루 서버 칸.
pub(crate) fn fill_space_view(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    me: &UserId,
    view: &mut SpaceView,
) {
    if state.kind != SpaceKind::Crew {
        return;
    }
    if crew_allows(state, me, Permissions::MANAGE_SPACE) {
        view.crew_servers = Some(CrewServers {
            stun: state.crew_servers.stun.clone(),
        });
    }
    view.crew_stun =
        !state.crew_servers.stun.is_empty() && crew_allows(state, me, Permissions::USE_CREW_STUN);
    view.use_crew_servers = crews(node).contains(space);
}

/// 이 공간 통화에 쓸 ICE 서버: 내 STUN 서버 뒤로 쓸 수 있는 그 크루의 STUN 서버를 하나로 묶습니다.
pub(crate) fn call_ice(node: &Node, space: &SpaceId) -> Vec<IceServer> {
    let mut stun = stun_servers(node);
    if let (Ok(me), Some(st)) = (node.me(), node.space_state(space)) {
        for s in crew_stun_of(&crews(node), space, &st, &me.user()) {
            if !stun.contains(&s) {
                stun.push(s);
            }
        }
    }
    if stun.is_empty() {
        return Vec::new();
    }
    vec![IceServer {
        urls: stun.iter().map(|s| format!("stun:{s}")).collect(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_normalized() {
        for (input, want) in [
            ("stun.l.google.com:19302", "stun.l.google.com:19302"),
            ("stun:stun.cloudflare.com:3478", "stun.cloudflare.com:3478"),
            ("STUN:Stun.Example.ORG", "stun.example.org:3478"),
            ("  198.51.100.7:3479 ", "198.51.100.7:3479"),
            ("[2001:db8::1]:3479", "[2001:db8::1]:3479"),
            ("[2001:db8::1]", "[2001:db8::1]:3478"),
            ("2001:db8::1", "[2001:db8::1]:3478"),
            ("stun:[2001:DB8::1]:19302", "[2001:db8::1]:19302"),
        ] {
            assert_eq!(normalize(input).unwrap(), want, "{input}");
        }
    }

    #[test]
    fn turn_credentials_and_junk_are_rejected() {
        let long = format!("{}.example.org", "a".repeat(250));
        for bad in [
            "",
            "turn:turn.example.org:3478",
            "TURNS:turn.example.org",
            "stuns:stun.example.org",
            "user:pass@stun.example.org:3478",
            "stun:user@stun.example.org",
            "https://stun.example.org",
            "stun.example.org:0",
            "stun.example.org:70000",
            "stun.example.org:abc",
            "stun.example.org:3478?transport=udp",
            "stun example.org",
            "stun.\nexample.org",
            "-bad.example.org",
            "a..b",
            "턴.example.org",
            "stun：example.org",
            "[2001:db8::1]x",
            "[nope]:3478",
            long.as_str(),
        ] {
            assert!(normalize(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn list_limit_and_dedupe() {
        let many: Vec<String> = (0..9).map(|i| format!("s{i}.example.org")).collect();
        assert!(normalize_list(&many).is_err());
        let dup = vec![
            "stun:a.example.org".to_string(),
            "A.example.org:3478".to_string(),
            "b.example.org:1".to_string(),
        ];
        assert_eq!(
            normalize_list(&dup).unwrap(),
            vec!["a.example.org:3478", "b.example.org:1"]
        );
        assert!(normalize_list(&[]).unwrap().is_empty());
    }
}
