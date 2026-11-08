//! 보내는 쪽 링크 미리보기: 본문의 http(s) 링크마다 OpenGraph 제목·설명·사이트 이름·이미지를 조회합니다.
//! 받는 쪽은 외부 사이트에 접속하지 않습니다(미리보기는 메시지와 함께 암호화돼 갑니다).
//!
//! 한도(`LinkPreviewOptions`, decisions.md): 링크 3개, 링크마다 전체 5초, HTML은 512KiB까지만 읽음, 이미지는
//! png·jpeg·gif·webp 2MiB까지 받아 64KiB를 넘으면 긴 변 320px 이하 JPEG로 줄여 실음(풀 때 크기·메모리 한도),
//! 넘겨주기 3번, 쿠키·인증 정보 없음, `Accept-Encoding: identity`.
//! 포트는 80·443·8080·8443만. `<https://…>`처럼 꺾쇠로 감싼 링크는 미리보기를 만들지 않습니다.
//!
//! 직접 조회(직접 연결 노드)의 SSRF 방어: 이름을 직접 풀어 공인 주소가 아니면(사설·루프백·링크 로컬·CGNAT·문서용·
//! 멀티캐스트·예약 등) 거절하고, 검사한 그 주소로 연결합니다(이름을 다시 풀게 해 우회하는 것을 막음). 넘겨주기마다
//! 다시 검사합니다.
//! 시험용 설정 `allow_private_addrs`는 주소 검사만, `allow_any_port`는 포트 제한만 끕니다.
//!
//! Tor로 도는 기기(`tor::tor_active`, 앱 사용자 기기 모두)는 모든 조회를 Arti로만 합니다(HTTP/1.1을 Arti 스트림
//! 위에, https는 그 위에 rustls). 이름은 출구 노드가 풀어 로컬 DNS 조회가 없고, 링크마다 회로를 따로 써(넘겨주기·
//! 이미지는 같은 회로) 출구가 링크끼리, 또 노드 연결과 묶어 보지 못하게 합니다. 한도·포트·인증 정보 규칙은 같고,
//! SSRF 검사는 이름을 풀 수 없으므로 IP 리터럴은 공인 주소만, 이름은 특수 용도 이름(`localhost`·`.local`·한 단어
//! 이름 등)을 거절합니다. `.onion`은 Tor로 돌 때만 조회하고, 직접 조회하는 직접 연결 노드(`--direct`)는 이름이
//! DNS로 새지 않게 거절합니다.
//! Arti가 없으면(빌드·설정) 직접 조회로 물러나지 않고 미리보기를 만들지 않습니다.
//! 로그에는 주소를 남기지 않습니다(메시지 내용).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;
use url::{Host, Position, Url};
use yangpa_core::body::LinkEmbed;
use yangpa_net::{NetHandle, TorIsolation};
use yangpa_sync::state::limits::{
    MAX_EMBED_TEXT_CHARS, MAX_NAME_CHARS, MAX_THUMBNAIL_BYTES, MAX_URL_CHARS,
};

use super::media;
use crate::config::LinkPreviewOptions;
use crate::node::Node;

/// 응답 머리 최대 크기.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// 미리보기 이미지를 받는 최대 크기. 썸네일 한도(64KiB)를 넘으면 풀어서 줄입니다.
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// 조회할 수 있는 포트(직접 조회·Arti 조회 같음).
const ALLOWED_PORTS: [u16; 4] = [80, 443, 8080, 8443];
/// 미리보기 설명을 이 글자 수로 줄입니다(검증 한도보다 짧게).
const DESCRIPTION_CHARS: usize = 1000;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; yangpa-link-preview/1)";

/// 메시지 본문에서 미리보기를 만듭니다. 실패한 링크는 조용히 건너뜁니다.
pub(crate) async fn link_previews(node: &Arc<Node>, text: &str) -> Vec<LinkEmbed> {
    let opts = node.cfg.sharing.link_preview.clone();
    if !opts.enabled {
        return Vec::new();
    }
    // Tor로 도는 노드는 Arti로만 조회합니다(직접 연결 스웜인 시더·시험만 직접 조회).
    let tor = crate::tor::tor_active(node);
    let urls = extract_urls(text, opts.max_links);
    let jobs = urls.into_iter().map(|u| {
        let opts = opts.clone();
        // 링크마다 회로를 따로 씁니다.
        let route = if tor {
            Route::Tor {
                net: node.net.clone(),
                isolation: TorIsolation::new(),
            }
        } else {
            Route::Direct
        };
        async move {
            match tokio::time::timeout(opts.timeout, preview(&opts, &route, u)).await {
                Ok(e) => e,
                Err(_) => {
                    debug!("링크 미리보기 시간 초과");
                    None
                }
            }
        }
    });
    futures::future::join_all(jobs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// 본문의 http(s) 링크(앞에서부터 `max`개, 중복 없이). 꺾쇠로 감싼 링크는 뺍니다.
pub(crate) fn extract_urls(text: &str, max: usize) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    for raw in text.split_whitespace() {
        if out.len() >= max {
            break;
        }
        let Some(start) = raw.find("http://").or_else(|| raw.find("https://")) else {
            continue;
        };
        if raw[..start].ends_with('<') {
            continue;
        }
        let s = raw[start..]
            .trim_end_matches(|c: char| ".,;:!?\"')]}>".contains(c))
            .trim_end_matches('*');
        if s.chars().count() > MAX_URL_CHARS {
            continue;
        }
        let Ok(u) = Url::parse(s) else { continue };
        if !matches!(u.scheme(), "http" | "https") || u.host().is_none() {
            continue;
        }
        // 친구·초대 링크(웹 초대 페이지)는 미리보기가 늘 같은 페이지라 받으러 가지 않습니다.
        if u.fragment()
            .is_some_and(yangpa_core::links::is_link_fragment)
        {
            continue;
        }
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// 조회 경로.
pub(crate) enum Route {
    /// 이름을 직접 풀고 검사한 주소로 연결(직접 연결 노드).
    Direct,
    /// Arti로만 연결(Tor로 도는 기기). 같은 링크의 넘겨주기·이미지는 같은 격리 단위를 씁니다.
    Tor {
        net: NetHandle,
        isolation: TorIsolation,
    },
}

async fn preview(opts: &LinkPreviewOptions, route: &Route, url: Url) -> Option<LinkEmbed> {
    let page = http_get(opts, route, url.clone(), Accept::Html, opts.max_page_bytes).await?;
    let html = String::from_utf8_lossy(&page.body);
    let meta = parse_meta(&html);
    let title = clean_text(
        meta.og_title
            .as_deref()
            .or(meta.title.as_deref())
            .unwrap_or(""),
        MAX_NAME_CHARS * 3,
    );
    let description = clean_text(
        meta.og_description
            .as_deref()
            .or(meta.description.as_deref())
            .unwrap_or(""),
        DESCRIPTION_CHARS.min(MAX_EMBED_TEXT_CHARS),
    );
    if title.is_empty() && description.is_empty() {
        return None;
    }
    let site_name = clean_text(
        meta.site_name
            .as_deref()
            .or(page.url.host_str())
            .unwrap_or(""),
        MAX_NAME_CHARS,
    );
    let mut thumbnail = Vec::new();
    if let Some(img) = meta.image.as_deref().and_then(|i| page.url.join(i).ok())
        && let Some(f) = http_get(opts, route, img, Accept::Image, MAX_IMAGE_BYTES).await
        && let Some(mime) = media::sniff_magic(&f.body)
        && media::THUMBNAIL_MIMES.contains(&mime)
    {
        thumbnail = if f.body.len() <= MAX_THUMBNAIL_BYTES {
            f.body
        } else {
            // 풀기는 CPU 일이라 비동기 일꾼을 막지 않게 따로 돌립니다(한도로 메모리·크기를 묶음).
            tokio::task::spawn_blocking(move || {
                media::make_thumbnail(
                    std::io::Cursor::new(f.body),
                    mime,
                    MAX_THUMBNAIL_BYTES,
                    media::REMOTE_DECODE,
                )
            })
            .await
            .ok()
            .flatten()
            .map(|t| t.jpeg)
            .unwrap_or_default()
        };
    }
    Some(LinkEmbed {
        url: url.to_string(),
        title,
        description,
        site_name,
        thumbnail,
        player_url: youtube_player(&url),
    })
}

/// 유튜브 링크면 쿠키 없는 임베드 주소('눌러서 불러오기'용).
pub(crate) fn youtube_player(url: &Url) -> Option<String> {
    let host = url
        .host_str()?
        .trim_start_matches("www.")
        .trim_start_matches("m.");
    let id = match host {
        "youtu.be" => url.path_segments()?.next()?.to_owned(),
        "youtube.com" | "music.youtube.com" => {
            let mut seg = url.path_segments()?;
            match seg.next()? {
                "watch" => url
                    .query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.into_owned())?,
                "shorts" | "embed" | "live" => seg.next()?.to_owned(),
                _ => return None,
            }
        }
        _ => return None,
    };
    let valid = id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| format!("https://www.youtube-nocookie.com/embed/{id}"))
}

// ───────────── HTML ─────────────

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) og_title: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) og_description: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) site_name: Option<String>,
    pub(crate) image: Option<String>,
}

/// `<meta property|name=… content=…>`와 `<title>`을 읽습니다(본문 시작 전까지만 보면 충분).
pub(crate) fn parse_meta(html: &str) -> Meta {
    let lower = html.to_ascii_lowercase();
    let end = lower.find("<body").unwrap_or(html.len());
    let mut m = Meta::default();
    let mut pos = 0;
    while let Some(i) = lower[pos..end].find("<meta") {
        let start = pos + i + 5;
        let Some(close) = lower[start..end].find('>') else {
            break;
        };
        let attrs = parse_attrs(&html[start..start + close]);
        pos = start + close;
        let key = attrs
            .iter()
            .find(|(k, _)| k == "property" || k == "name")
            .map(|(_, v)| v.to_ascii_lowercase());
        let Some(content) = attrs
            .iter()
            .find(|(k, _)| k == "content")
            .map(|(_, v)| decode_entities(v))
        else {
            continue;
        };
        let slot = match key.as_deref() {
            Some("og:title") => &mut m.og_title,
            Some("og:description") => &mut m.og_description,
            Some("description") => &mut m.description,
            Some("og:site_name") => &mut m.site_name,
            Some("og:image" | "og:image:url" | "og:image:secure_url") => &mut m.image,
            _ => continue,
        };
        if slot.is_none() && !content.trim().is_empty() {
            *slot = Some(content);
        }
    }
    if let Some(i) = lower[..end].find("<title")
        && let Some(gt) = lower[i..end].find('>')
    {
        let s = i + gt + 1;
        if let Some(e) = lower[s..end].find("</title") {
            m.title = Some(decode_entities(&html[s..s + e]));
        }
    }
    m
}

/// 태그 속성 `이름=값`(값은 따옴표 둘 다 또는 없이). 이름은 소문자로.
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if ns == i {
            i += 1;
            continue;
        }
        let name = s[ns..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            out.push((name, String::new()));
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
            let q = b[i];
            let vs = i + 1;
            let ve = b[vs..]
                .iter()
                .position(|c| *c == q)
                .map_or(b.len(), |p| vs + p);
            i = (ve + 1).min(b.len());
            &s[vs..ve]
        } else {
            let vs = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            &s[vs..i]
        };
        out.push((name, value.to_owned()));
    }
    out
}

/// 자주 쓰는 HTML 개체만 풉니다.
pub(crate) fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // 바이트로 찾습니다(12바이트 자리가 여러 바이트 글자 한가운데일 수 있음). ';'는 ASCII라 그 자리는 글자 경계입니다.
        let Some(semi) = rest.as_bytes().iter().take(12).position(|b| *b == b';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => {
                let num = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X"))
                {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse().ok()
                } else {
                    None
                };
                num.and_then(char::from_u32)
            }
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 제어 문자를 빼고 공백을 하나로 모은 뒤 `max`자로 자릅니다.
fn clean_text(s: &str, max: usize) -> String {
    let mut out = String::new();
    for word in s.split_whitespace() {
        let word: String = word.chars().filter(|c| !c.is_control()).collect();
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    if out.chars().count() > max {
        out = out.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
    }
    out
}

// ───────────── SSRF 방어 ─────────────

/// 공인 유니캐스트 주소인지(미리보기로 조회해도 되는지).
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_public_v4(a),
        IpAddr::V6(a) => is_public_v6(a),
    }
}

fn is_public_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_documentation()
        || a.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xFE) == 18)
        || o[0] >= 240)
}

fn is_public_v6(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = a.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    // NAT64(64:ff9b::/96)·6to4(2002::/16)는 안에 든 IPv4로 판단합니다.
    if s[0] == 0x64 && s[1] == 0xFF9B && s[2..6] == [0, 0, 0, 0] {
        return is_public_v4(embedded(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return is_public_v4(embedded(s[1], s[2]));
    }
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        || s[0..6] == [0, 0, 0, 0, 0, 0]
        || (s[0] & 0xFE00) == 0xFC00
        || (s[0] & 0xFFC0) == 0xFE80
        || (s[0] & 0xFFC0) == 0xFEC0
        || (s[0] == 0x2001 && s[1] < 0x0200)
        || (s[0] == 0x2001 && s[1] == 0x0DB8)
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0]))
}

// ───────────── HTTP/1.1 GET ─────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Accept {
    Html,
    Image,
}

impl Accept {
    fn header(self) -> &'static str {
        match self {
            Accept::Html => "text/html,application/xhtml+xml;q=0.9",
            Accept::Image => "image/png,image/jpeg,image/gif,image/webp",
        }
    }

    fn allows(self, content_type: &str) -> bool {
        let ct = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match self {
            Accept::Html => ct == "text/html" || ct == "application/xhtml+xml",
            Accept::Image => media::THUMBNAIL_MIMES.contains(&ct.as_str()),
        }
    }
}

struct Fetched {
    /// 넘겨주기 뒤 최종 주소(상대 이미지 주소를 풀 때).
    url: Url,
    body: Vec<u8>,
}

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

fn tls_config() -> Option<Arc<ClientConfig>> {
    static CFG: OnceLock<Option<Arc<ClientConfig>>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots: rustls::RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()
            .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
    })
    .clone()
}

/// 두 경로 공통 규칙(scheme·인증 정보·포트). 통과하면 포트.
fn checked_port(opts: &LinkPreviewOptions, url: &Url) -> Option<u16> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let port = url.port_or_known_default()?;
    if !opts.allow_any_port && !ALLOWED_PORTS.contains(&port) {
        return None;
    }
    Some(port)
}

fn is_onion_name(d: &str) -> bool {
    d.trim_end_matches('.')
        .to_ascii_lowercase()
        .ends_with(".onion")
}

/// 조회해도 되는 주소들(이름을 직접 풀고 SSRF 검사).
async fn resolve(opts: &LinkPreviewOptions, url: &Url) -> Option<Vec<SocketAddr>> {
    let port = checked_port(opts, url)?;
    let addrs: Vec<SocketAddr> = match url.host()? {
        Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        // onion 이름은 시스템 DNS로 새면 안 됩니다(풀리지도 않음).
        Host::Domain(d) if is_onion_name(d) => return None,
        Host::Domain(d) => tokio::net::lookup_host((d, port)).await.ok()?.collect(),
    };
    let ok: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| opts.allow_private_addrs || is_public(a.ip()))
        .take(2)
        .collect();
    (!ok.is_empty()).then_some(ok)
}

/// 이름을 풀 수 없는(출구가 푸는) Tor 경로의 SSRF 검사. 통과하면 Arti에 넘길 (이름, 포트).
///
/// IP 리터럴은 공인 주소만, 이름은 특수 용도·내부망 이름을 거절합니다(출구 노드 쪽 내부망을 건드리지 않게).
/// `.onion`은 허용합니다. 루프백·사설 주소·`localhost`는 Arti도 한 번 더 거절합니다.
pub(crate) fn tor_target(opts: &LinkPreviewOptions, url: &Url) -> Option<(String, u16)> {
    let port = checked_port(opts, url)?;
    let host = match url.host()? {
        Host::Ipv4(ip) => {
            (opts.allow_private_addrs || is_public(ip.into())).then(|| ip.to_string())?
        }
        Host::Ipv6(ip) => {
            (opts.allow_private_addrs || is_public(ip.into())).then(|| ip.to_string())?
        }
        Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            (opts.allow_private_addrs || !is_special_name(&d)).then_some(d)?
        }
    };
    Some((host, port))
}

/// 공인 DNS에서 풀리지 않거나 내부망을 가리키는 이름(RFC 6761·6762·8375 등과 흔한 내부 접미사).
pub(crate) fn is_special_name(d: &str) -> bool {
    const SUFFIXES: [&str; 10] = [
        ".localhost",
        ".local",
        ".home.arpa",
        ".arpa",
        ".internal",
        ".intranet",
        ".lan",
        ".invalid",
        ".test",
        ".example",
    ];
    d.is_empty() || !d.contains('.') || d == "localhost" || SUFFIXES.iter().any(|s| d.ends_with(s))
}

/// 경로에 따라 연결하고 https면 TLS를 씌웁니다.
async fn open(opts: &LinkPreviewOptions, route: &Route, url: &Url) -> Option<Box<dyn Io>> {
    match route {
        Route::Direct => {
            let addrs = resolve(opts, url).await?;
            let mut tcp = None;
            for a in &addrs {
                if let Ok(s) = TcpStream::connect(a).await {
                    tcp = Some(s);
                    break;
                }
            }
            wrap_tls(url, tcp?).await
        }
        Route::Tor { net, isolation } => {
            let (host, port) = tor_target(opts, url)?;
            match net.tor_connect(&host, port, *isolation).await {
                Ok(s) => wrap_tls(url, s).await,
                Err(e) => {
                    // 오류 문구에는 대상이 들어 있지 않습니다(`NetHandle::tor_connect`).
                    debug!(error = %e, "링크 미리보기: Tor 연결 실패");
                    None
                }
            }
        }
    }
}

async fn wrap_tls<S>(url: &Url, io: S) -> Option<Box<dyn Io>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if url.scheme() == "http" {
        return Some(Box::new(io));
    }
    let name = match url.host()? {
        Host::Domain(d) => ServerName::try_from(d.to_owned()).ok()?,
        Host::Ipv4(ip) => ServerName::IpAddress(IpAddr::V4(ip).into()),
        Host::Ipv6(ip) => ServerName::IpAddress(IpAddr::V6(ip).into()),
    };
    let tls = tokio_rustls::TlsConnector::from(tls_config()?)
        .connect(name, io)
        .await
        .ok()?;
    Some(Box::new(tls))
}

/// GET 한 번(넘겨주기를 따라감). 상태 200, 허용한 형식일 때만 본문(최대 `max`바이트).
/// HTML은 넘치면 앞부분만, 이미지는 넘치면 실패.
async fn http_get(
    opts: &LinkPreviewOptions,
    route: &Route,
    mut url: Url,
    accept: Accept,
    max: usize,
) -> Option<Fetched> {
    for _ in 0..=opts.max_redirects {
        // 넘겨주기마다 같은 검사를 다시 거칩니다(경로별 `open`).
        let mut io = open(opts, route, &url).await?;
        let host = match (url.host()?, url.port()) {
            (Host::Ipv6(ip), Some(p)) => format!("[{ip}]:{p}"),
            (Host::Ipv6(ip), None) => format!("[{ip}]"),
            (h, Some(p)) => format!("{h}:{p}"),
            (h, None) => h.to_string(),
        };
        let target = &url[Position::BeforePath..Position::AfterQuery];
        let req = format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {USER_AGENT}\r\nAccept: {}\r\n\
             Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
            accept.header()
        );
        io.write_all(req.as_bytes()).await.ok()?;
        io.flush().await.ok()?;
        let head = read_head(&mut io).await?;
        if (300..400).contains(&head.status) {
            let loc = head.header("location")?;
            url = url.join(loc).ok()?;
            continue;
        }
        if head.status != 200 || !accept.allows(head.header("content-type").unwrap_or("")) {
            return None;
        }
        let body = read_body(&mut io, head, max, accept == Accept::Html).await?;
        return Some(Fetched { url, body });
    }
    debug!("링크 미리보기: 넘겨주기가 너무 많음");
    None
}

/// 요청 한 번(넘겨주기를 따르지 않음): 상태와 본문(최대 `max`바이트, 넘치면 실패). 후원 확인(`supporter`)이
/// GitHub·Patreon API에 씁니다. 주소 검사와 연결 경로는 미리보기와 같습니다.
pub(crate) async fn http_request(
    route: &Route,
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    body: &[u8],
    max: usize,
) -> Option<(u16, Vec<u8>)> {
    // 받은 토큰을 머리에 실으므로 줄바꿈으로 요청을 쪼개지 못하게 막습니다.
    if headers
        .iter()
        .any(|(k, v)| k.contains(['\r', '\n']) || v.contains(['\r', '\n']))
    {
        return None;
    }
    let host = url.host_str()?;
    // 실패한 단계만 남깁니다(주소·본문은 남기지 않음).
    let Some(mut io) = open(&LinkPreviewOptions::default(), route, url).await else {
        debug!("HTTP 요청: 연결·TLS 실패");
        return None;
    };
    let target = &url[Position::BeforePath..Position::AfterQuery];
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host}\r\nAccept-Encoding: identity\r\n\
         Connection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let sent = async {
        io.write_all(req.as_bytes()).await?;
        io.write_all(body).await?;
        io.flush().await
    };
    if let Err(e) = sent.await {
        debug!(error = %e, "HTTP 요청: 보내기 실패");
        return None;
    }
    let Some(head) = read_head(&mut io).await else {
        debug!("HTTP 요청: 응답 머리를 읽지 못함");
        return None;
    };
    let status = head.status;
    let Some(body) = read_body(&mut io, head, max, false).await else {
        debug!(status, "HTTP 요청: 응답 본문을 읽지 못함");
        return None;
    };
    Some((status, body))
}

/// GET 한 번의 상태와 `Location` 머리(본문은 읽지 않음). 업데이트 확인이 `releases/latest`의 넘겨주기를 읽습니다.
pub(crate) async fn http_location(route: &Route, url: &Url) -> Option<(u16, Option<String>)> {
    let mut io = open(&LinkPreviewOptions::default(), route, url).await?;
    send_get(&mut io, url).await?;
    let head = read_head(&mut io).await?;
    Some((head.status, head.header("location").map(str::to_owned)))
}

/// 받을 본문: 머리 뒤에 이미 읽은 앞부분(`rest`)과 나머지를 읽을 연결, 전체 길이(`Content-Length`).
pub(crate) struct Body {
    pub(crate) io: Box<dyn Io>,
    pub(crate) rest: Vec<u8>,
    pub(crate) len: u64,
}

/// https GET을 넘겨주기를 따라(최대 5번, 주소마다 `allow` 검사) 열고 상태 200의 본문을 돌려줍니다.
/// `Content-Length`가 있고 `max` 이하여야 합니다(청크 인코딩은 받지 않음). 업데이트 파일 받기(`update`)가 씁니다.
pub(crate) async fn http_open(
    route: &Route,
    mut url: Url,
    allow: impl Fn(&Url) -> bool,
    max: u64,
) -> Option<Body> {
    for _ in 0..=5 {
        if url.scheme() != "https" || !allow(&url) {
            debug!("HTTP 받기: 허용하지 않은 주소");
            return None;
        }
        let mut io = open(&LinkPreviewOptions::default(), route, &url).await?;
        send_get(&mut io, &url).await?;
        let head = read_head(&mut io).await?;
        if (300..400).contains(&head.status) {
            url = url.join(head.header("location")?).ok()?;
            continue;
        }
        let len: u64 = head.header("content-length")?.trim().parse().ok()?;
        if head.status != 200 || len > max || head.header("transfer-encoding").is_some() {
            debug!(status = head.status, len, "HTTP 받기: 받을 수 없는 응답");
            return None;
        }
        return Some(Body {
            io,
            rest: head.rest,
            len,
        });
    }
    None
}

async fn send_get(io: &mut Box<dyn Io>, url: &Url) -> Option<()> {
    let host = url.host_str()?;
    let target = &url[Position::BeforePath..Position::AfterQuery];
    let req = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    io.write_all(req.as_bytes()).await.ok()?;
    io.flush().await.ok()
}

struct Head {
    status: u16,
    headers: Vec<(String, String)>,
    /// 머리 뒤에 이미 읽은 본문 앞부분.
    rest: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn read_head(io: &mut Box<dyn Io>) -> Option<Head> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = io.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut hs = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut hs);
        match resp.parse(&buf).ok()? {
            httparse::Status::Complete(len) => {
                let status = resp.code?;
                let headers = resp
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_owned(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect();
                return Some(Head {
                    status,
                    headers,
                    rest: buf[len..].to_vec(),
                });
            }
            httparse::Status::Partial if buf.len() < MAX_HEAD_BYTES => {}
            httparse::Status::Partial => return None,
        }
    }
}

/// 본문(최대 `max`바이트). `truncate`면 넘치는 부분을 버리고, 아니면 실패.
async fn read_body(
    io: &mut Box<dyn Io>,
    head: Head,
    max: usize,
    truncate: bool,
) -> Option<Vec<u8>> {
    let chunked = head
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length: Option<usize> = head
        .header("content-length")
        .and_then(|v| v.trim().parse().ok());
    if !chunked && !truncate && length.is_some_and(|l| l > max) {
        return None;
    }
    let mut raw = head.rest;
    let mut chunk = [0u8; 8192];
    // 원문을 읽을 상한: 청크 표지 여유를 조금 둡니다.
    let raw_cap = if chunked {
        max + max / 8 + 1024
    } else {
        length.unwrap_or(usize::MAX).min(max + 1)
    };
    loop {
        if !chunked && length.is_some_and(|l| raw.len() >= l) {
            break;
        }
        if chunked && chunked_done(&raw) {
            break;
        }
        if raw.len() >= raw_cap {
            break;
        }
        let n = io.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let mut body = if chunked { decode_chunked(&raw)? } else { raw };
    if let Some(l) = length
        && !chunked
    {
        body.truncate(l);
    }
    if body.len() > max {
        if !truncate {
            return None;
        }
        body.truncate(max);
    }
    Some(body)
}

/// 청크 본문이 끝났는지(마지막 0 크기 청크까지 받았는지).
fn chunked_done(raw: &[u8]) -> bool {
    let mut pos = 0;
    loop {
        match httparse::parse_chunk_size(&raw[pos..]) {
            Ok(httparse::Status::Complete((n, 0))) => return raw.len() >= pos + n,
            Ok(httparse::Status::Complete((n, size))) => {
                // 크기는 원격이 정하므로(최대 16자리 hex) 넘침 없이 셉니다. 넘치면 아직 다 받지 못한 것으로 봅니다.
                let Some(next) = usize::try_from(size)
                    .ok()
                    .and_then(|size| pos.checked_add(n)?.checked_add(size)?.checked_add(2))
                else {
                    return false;
                };
                if next > raw.len() {
                    return false;
                }
                pos = next;
            }
            _ => return false,
        }
    }
}

/// 청크 인코딩을 풉니다(앞부분만 있으면 받은 데까지).
fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        match httparse::parse_chunk_size(&raw[pos..]) {
            Ok(httparse::Status::Complete((_, 0))) => return Some(out),
            Ok(httparse::Status::Complete((n, size))) => {
                let start = pos.saturating_add(n);
                let end = start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
                if end > raw.len() {
                    out.extend_from_slice(&raw[start.min(raw.len())..]);
                    return Some(out);
                }
                out.extend_from_slice(&raw[start..end]);
                pos = end.saturating_add(2);
                if pos > raw.len() {
                    return Some(out);
                }
            }
            Ok(httparse::Status::Partial) => return Some(out),
            Err(_) => return (!out.is_empty()).then_some(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links() {
        let t = "봐 https://example.com/a?b=1, 그리고 (http://x.org/p). <https://hidden.example/> \
                 https://example.com/a?b=1 ftp://no";
        let v: Vec<String> = extract_urls(t, 5).iter().map(|u| u.to_string()).collect();
        assert_eq!(v, ["https://example.com/a?b=1", "http://x.org/p"]);
        assert_eq!(extract_urls(t, 1).len(), 1);
        let invite = format!(
            "들어와 {}#invite/3yZe7d https://x.org/#top",
            yangpa_core::links::WEB_BASE
        );
        let v: Vec<String> = extract_urls(&invite, 5)
            .iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(v, ["https://x.org/#top"]);
    }

    #[test]
    fn ssrf_guard() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "192.0.0.8",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2002:7f00:1::",
            "2001:db8::1",
            "ff02::1",
        ] {
            assert!(!is_public(bad.parse().unwrap()), "{bad}");
        }
        for good in [
            "93.184.216.34",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(good.parse().unwrap()), "{good}");
        }
    }

    fn opts() -> LinkPreviewOptions {
        LinkPreviewOptions::default()
    }

    #[test]
    fn tor_target_rules() {
        let t = |s: &str| tor_target(&opts(), &Url::parse(s).unwrap());
        assert_eq!(
            t("https://Example.COM./a"),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            t("http://93.184.216.34/"),
            Some(("93.184.216.34".into(), 80))
        );
        assert_eq!(
            t("http://[2606:4700:4700::1111]:8080/"),
            Some(("2606:4700:4700::1111".into(), 8080))
        );
        let onion = format!("http://{}.onion/", "a".repeat(56));
        assert_eq!(t(&onion), Some((format!("{}.onion", "a".repeat(56)), 80)));
        for bad in [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://169.254.169.254/latest",
            "http://localhost/",
            "http://LOCALHOST./",
            "http://foo.localhost/",
            "http://printer.local/",
            "http://router.home.arpa/",
            "http://1.0.0.127.in-addr.arpa/",
            "http://db.internal/",
            "http://nas.lan/",
            "http://intranet/",
            "http://x.test/",
            "http://example.com:22/",
            "https://user:pw@example.com/",
            "ftp://example.com/",
        ] {
            assert_eq!(t(bad), None, "{bad}");
        }
        let mut loose = opts();
        loose.allow_private_addrs = true;
        loose.allow_any_port = true;
        assert_eq!(
            tor_target(&loose, &Url::parse("http://127.0.0.1:9/").unwrap()),
            Some(("127.0.0.1".into(), 9))
        );
    }

    #[tokio::test]
    async fn direct_route_never_resolves_onion_names() {
        let onion = format!("http://{}.onion/", "a".repeat(56));
        assert!(
            resolve(&opts(), &Url::parse(&onion).unwrap())
                .await
                .is_none()
        );
        assert!(
            resolve(&opts(), &Url::parse("http://127.0.0.1/").unwrap())
                .await
                .is_none()
        );
    }

    #[test]
    fn parses_open_graph() {
        let html = r#"<html><head><title>예비 &amp; 제목</title>
            <meta property="og:title" content="진짜 &quot;제목&quot;">
            <META name='description' content='설명&#33;'>
            <meta property=og:image content=/img.png>
            <meta property="og:site_name" content="사이트"></head><body><meta property="og:title" content="본문">"#;
        let m = parse_meta(html);
        assert_eq!(m.og_title.as_deref(), Some("진짜 \"제목\""));
        assert_eq!(m.title.as_deref(), Some("예비 & 제목"));
        assert_eq!(m.description.as_deref(), Some("설명!"));
        assert_eq!(m.image.as_deref(), Some("/img.png"));
        assert_eq!(m.site_name.as_deref(), Some("사이트"));
    }

    #[test]
    fn youtube_links() {
        let p = |s: &str| youtube_player(&Url::parse(s).unwrap());
        let want = Some("https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ".to_owned());
        assert_eq!(p("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"), want);
        assert_eq!(p("https://youtu.be/dQw4w9WgXcQ"), want);
        assert_eq!(p("https://youtube.com/shorts/dQw4w9WgXcQ"), want);
        assert_eq!(p("https://example.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(p("https://youtu.be/bad"), None);
    }

    #[test]
    fn chunked_bodies() {
        let raw = b"4\r\nRust\r\n7\r\n yangpa\r\n0\r\n\r\n";
        assert!(chunked_done(raw));
        assert_eq!(decode_chunked(raw).unwrap(), b"Rust yangpa");
        assert!(!chunked_done(&raw[..10]));
        assert_eq!(decode_chunked(&raw[..10]).unwrap(), b"Rust");
        // 원격이 정한 거대한 청크 크기: 넘침으로 멈추거나(디버그) 끝없이 돌지(릴리스) 않아야 합니다.
        for huge in [
            &b"FFFFFFFFFFFFFFEC\r\nabc"[..],
            b"FFFFFFFFFFFFFFFF\r\n",
            b"4\r\nRust\r\nFFFFFFFFFFFFFFFE\r\nx",
        ] {
            assert!(!chunked_done(huge));
            assert!(decode_chunked(huge).is_some());
        }
    }

    #[test]
    fn entities_near_multibyte_text() {
        // '&' 뒤 12바이트 자리가 한글 글자 한가운데여도 멈추지 않아야 합니다.
        assert_eq!(decode_entities("R&D연구소장"), "R&D연구소장");
        assert_eq!(decode_entities("&aaaaaaaaa한"), "&aaaaaaaaa한");
        assert_eq!(decode_entities("가&amp;나&#xAC00;&"), "가&나가&");
        assert_eq!(decode_entities("&한글한글한글;"), "&한글한글한글;");
    }

    #[test]
    fn text_is_cleaned() {
        assert_eq!(clean_text("  a\n\tb\u{7}c  ", 10), "a bc");
        assert_eq!(clean_text("abcdef", 4), "abc…");
    }
}
