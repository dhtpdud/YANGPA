//! 블롭 커스텀 프로토콜 `yangpa-blob`.
//!
//! 주소: `yangpa-blob://localhost/blob/<space>/<blob>/<key_from>?mime=<형식>`
//! (윈도: `http://yangpa-blob.localhost/...`). 바이트는 데몬의 `blob.read`로 메모리에서 바로 넘기고,
//! 응답마다 `Cache-Control: no-store`를 붙여 웹뷰 캐시에 남지 않게 합니다(창도 incognito라 디스크 캐시 없음).
//! 크기만 기억해 두고(내용 없음) 동영상의 Range 요청은 필요한 구간만 읽습니다.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tauri::http::{Method, Request, Response, StatusCode, header};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};
use tracing::debug;
use yangpa_core::ids::{BlobId, EventId, SpaceId};
use yangpa_rpc::api::{BLOB_READ_MAX_LEN, BlobParams, BlobReadParams, m};
use yangpa_rpc::{ClientError, RpcClient};

use crate::daemon::{Daemon, MAIN_WINDOW};

/// 프로토콜 이름.
pub const SCHEME: &str = "yangpa-blob";
/// 한 번에 메모리로 올리는 블롭 최대 크기.
pub const MAX_SERVE: u64 = 100 * 1024 * 1024;
/// 크기 기억 항목 수 상한.
const SIZE_CACHE_MAX: usize = 1024;
/// 경로 조각 최대 길이(base58 32바이트는 44자).
const SEGMENT_MAX: usize = 64;
/// 블롭 전체를 메모리로 올리는 읽기를 동시에 몇 개까지 할지.
/// 큰 동영상 여러 개가 한꺼번에 화면에 나와도 메모리가 `MAX_SERVE` × 이 값을 넘지 않게 합니다.
pub const FULL_READ_CONCURRENCY: usize = 4;

/// 바로 렌더링해도 되는 형식만. 나머지(SVG·HTML 포함)는 내려받기용 바이너리로 냅니다.
const RENDERABLE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "video/mp4",
    "video/webm",
    "audio/ogg",
    "audio/mpeg",
    "audio/aac",
    "audio/webm",
    "audio/mp4",
    "audio/wav",
    "audio/opus",
];
const OCTET: &str = "application/octet-stream";
/// 형식을 바이트로 가리라는 요청(`?mime=auto`). 프로필 이미지·크루 아이콘은 API에 형식이 없어서 씁니다.
/// 가릴 수 있는 것은 그림 형식뿐이고, 나머지는 내려받기용 바이너리로 냅니다.
pub const AUTO_IMAGE: &str = "auto";

/// 블롭 요청 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRequest {
    pub space: SpaceId,
    pub blob: BlobId,
    pub key_from: EventId,
    pub mime: &'static str,
}

/// 요청 형식을 화이트리스트로 좁힙니다.
pub fn sanitize_mime(m: &str) -> &'static str {
    let m = m.trim().to_ascii_lowercase();
    if m == AUTO_IMAGE {
        return AUTO_IMAGE;
    }
    RENDERABLE
        .iter()
        .find(|r| **r == m)
        .copied()
        .unwrap_or(OCTET)
}

/// 바이트 앞부분으로 그림 형식을 가립니다(png·jpeg·gif·webp·avif). 모르면 바이너리.
pub fn sniff_image(head: &[u8]) -> &'static str {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if head.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        "image/gif"
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        "image/webp"
    } else if head.len() >= 12
        && &head[4..8] == b"ftyp"
        && matches!(&head[8..12], b"avif" | b"avis")
    {
        "image/avif"
    } else {
        OCTET
    }
}

/// 응답의 Content-Type. `auto`는 처음부터 받은 응답(200)에서만 가리고, 구간 응답은 바이너리로 둡니다.
fn content_type(requested: &'static str, served: &Served) -> &'static str {
    if requested != AUTO_IMAGE {
        return requested;
    }
    if served.status == StatusCode::OK {
        sniff_image(&served.body)
    } else {
        OCTET
    }
}

/// `/blob/<space>/<blob>/<key_from>`와 쿼리를 해석합니다. 실패하면 HTTP 상태.
pub fn parse_request(path: &str, query: Option<&str>) -> Result<BlobRequest, StatusCode> {
    let mut parts = path.trim_start_matches('/').split('/');
    if parts.next() != Some("blob") {
        return Err(StatusCode::NOT_FOUND);
    }
    let (Some(space), Some(blob), Some(key_from), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    if [space, blob, key_from]
        .iter()
        .any(|s| s.is_empty() || s.len() > SEGMENT_MAX)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let space: SpaceId = space.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let blob: BlobId = blob.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let key_from: EventId = key_from.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let mime = query
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "mime")
                .map(|(_, v)| sanitize_mime(&v))
        })
        .unwrap_or(OCTET);
    Ok(BlobRequest {
        space,
        blob,
        key_from,
        mime,
    })
}

/// `Range` 헤더(구간 하나만).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    From(u64),
    FromTo(u64, u64),
    Suffix(u64),
}

/// 이해할 수 없거나 여러 구간이면 None(전체를 보냄, RFC 9110 허용).
pub fn parse_range(h: &str) -> Option<RangeSpec> {
    let spec = h.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    match (a.is_empty(), b.is_empty()) {
        (false, true) => a.parse().ok().map(RangeSpec::From),
        (false, false) => {
            let (s, e) = (a.parse().ok()?, b.parse().ok()?);
            (s <= e).then_some(RangeSpec::FromTo(s, e))
        }
        (true, false) => b.parse().ok().filter(|n| *n > 0).map(RangeSpec::Suffix),
        (true, true) => None,
    }
}

/// 전체 크기에 맞춰 닫힌 구간 `[start, end]`로 풉니다. 만족할 수 없으면 None(416).
pub fn resolve_range(spec: RangeSpec, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let last = total - 1;
    match spec {
        RangeSpec::From(s) => (s <= last).then_some((s, last)),
        RangeSpec::FromTo(s, e) => (s <= last).then_some((s, e.min(last))),
        RangeSpec::Suffix(n) => Some((total.saturating_sub(n), last)),
    }
}

/// 앱 화면 출처만 CORS로 허용합니다(fetch로 읽는 휘발성 미디어용).
pub fn allowed_origin(origin: &str) -> bool {
    matches!(
        origin,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) || (cfg!(debug_assertions) && origin == "http://localhost:1420")
}

/// 데몬 오류를 HTTP 상태로.
fn status_of(e: &ClientError) -> StatusCode {
    match e.code() {
        Some(1004) => StatusCode::NOT_FOUND,
        Some(1001..=1003) | Some(1007) => StatusCode::FORBIDDEN,
        Some(1005) | Some(-32602) => StatusCode::BAD_REQUEST,
        Some(_) => StatusCode::BAD_GATEWAY,
        None => match e {
            ClientError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ClientError::Closed | ClientError::Connect(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_GATEWAY,
        },
    }
}

/// 크기 기억(내용은 담지 않음)과 전체 읽기 동시 수 제한.
pub struct BlobServer {
    sizes: Mutex<HashMap<BlobId, u64>>,
    full_reads: tokio::sync::Semaphore,
}

impl Default for BlobServer {
    fn default() -> Self {
        BlobServer {
            sizes: Mutex::new(HashMap::new()),
            full_reads: tokio::sync::Semaphore::new(FULL_READ_CONCURRENCY),
        }
    }
}

/// 응답 재료.
struct Served {
    status: StatusCode,
    body: Vec<u8>,
    content_range: Option<String>,
}

impl BlobServer {
    fn remember(&self, blob: BlobId, size: u64) {
        let mut m = self.sizes.lock();
        if m.len() >= SIZE_CACHE_MAX {
            m.clear();
        }
        m.insert(blob, size);
    }

    fn read_params(r: &BlobRequest, offset: u64, len: u64) -> BlobReadParams {
        BlobReadParams {
            space: r.space,
            blob: r.blob,
            key_from: r.key_from,
            index: None,
            offset: Some(offset),
            len: Some(len),
            binary: None,
        }
    }

    /// 데몬이 아는 원본 크기(`blob.status`). 모르면 None(전체를 읽어 잼).
    async fn size_of(&self, client: &RpcClient, r: &BlobRequest) -> Option<u64> {
        let params = BlobParams {
            space: r.space,
            blob: r.blob,
        };
        let size = client.request::<m::BlobStatus>(params).await.ok()?.size;
        (size > 0).then(|| {
            self.remember(r.blob, size);
            size
        })
    }

    /// 전체를 8MiB 창 단위로 읽습니다. 짧게 오면 끝.
    async fn read_all(client: &RpcClient, r: &BlobRequest) -> Result<Vec<u8>, StatusCode> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let offset = out.len() as u64;
            let chunk = match client
                .blob_read(Self::read_params(r, offset, BLOB_READ_MAX_LEN))
                .await
            {
                Ok(c) => c,
                // 끝을 넘는 offset은 "잘못된 입력"으로 올 수 있습니다(크기가 창의 배수인 경우).
                Err(e) if offset > 0 && e.code() == Some(1005) => break,
                Err(e) => return Err(status_of(&e)),
            };
            if chunk.len() as u64 > BLOB_READ_MAX_LEN {
                return Err(StatusCode::BAD_GATEWAY);
            }
            if offset + chunk.len() as u64 > MAX_SERVE {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            out.extend_from_slice(&chunk);
            if (chunk.len() as u64) < BLOB_READ_MAX_LEN {
                break;
            }
        }
        Ok(out)
    }

    async fn serve(
        &self,
        client: &RpcClient,
        r: &BlobRequest,
        range: Option<RangeSpec>,
    ) -> Result<Served, (StatusCode, Option<u64>)> {
        let mut known = self.sizes.lock().get(&r.blob).copied();
        // 동영상은 늘 구간으로 읽으므로 크기만 물어 둡니다: 전체를 올리면 `MAX_SERVE`보다 큰 영상은 틀지 못했습니다.
        if known.is_none() && range.is_some() {
            known = self.size_of(client, r).await;
        }
        if let (Some(spec), Some(total)) = (range, known) {
            // 크기를 알면 요청 구간만(최대 한 창) 읽습니다.
            let (start, end) = resolve_range(spec, total)
                .ok_or((StatusCode::RANGE_NOT_SATISFIABLE, Some(total)))?;
            let len = (end - start + 1).min(BLOB_READ_MAX_LEN);
            let chunk = client
                .blob_read(Self::read_params(r, start, len))
                .await
                .map_err(|e| (status_of(&e), None))?;
            if chunk.is_empty() || chunk.len() as u64 > len {
                return Err((StatusCode::BAD_GATEWAY, None));
            }
            let end = start + chunk.len() as u64 - 1;
            return Ok(Served {
                status: StatusCode::PARTIAL_CONTENT,
                body: chunk.to_vec(),
                content_range: Some(format!("bytes {start}-{end}/{total}")),
            });
        }
        let mut all = {
            let _permit = self
                .full_reads
                .acquire()
                .await
                .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, None))?;
            Self::read_all(client, r).await.map_err(|s| (s, None))?
        };
        let total = all.len() as u64;
        self.remember(r.blob, total);
        match range {
            None => Ok(Served {
                status: StatusCode::OK,
                body: all,
                content_range: None,
            }),
            Some(spec) => {
                let (start, end) = resolve_range(spec, total)
                    .ok_or((StatusCode::RANGE_NOT_SATISFIABLE, Some(total)))?;
                // 구간 밖은 잘라 냅니다(복사 없이).
                all.truncate((end + 1) as usize);
                all.drain(..start as usize);
                Ok(Served {
                    status: StatusCode::PARTIAL_CONTENT,
                    body: all,
                    content_range: Some(format!("bytes {start}-{end}/{total}")),
                })
            }
        }
    }
}

/// 모든 응답에 붙이는 헤더(보내기 전 미리보기 [`crate::preview`]도 씀).
pub(crate) fn base(status: StatusCode, origin: Option<&str>) -> tauri::http::response::Builder {
    let mut b = Response::builder()
        .status(status)
        .header(
            header::CACHE_CONTROL,
            "no-store, no-cache, must-revalidate, max-age=0",
        )
        .header(header::PRAGMA, "no-cache")
        .header(header::EXPIRES, "0")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header(header::VARY, "Origin, Range");
    if let Some(o) = origin.filter(|o| allowed_origin(o)) {
        b = b.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, o);
    }
    b
}

pub(crate) fn error_response(
    status: StatusCode,
    origin: Option<&str>,
    total: Option<u64>,
) -> Response<Cow<'static, [u8]>> {
    let mut b = base(status, origin).header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
    if let Some(t) = total {
        b = b.header(header::CONTENT_RANGE, format!("bytes */{t}"));
    }
    b.body(Cow::Borrowed(&[][..]))
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[][..])))
}

/// `register_asynchronous_uri_scheme_protocol` 처리기.
pub fn handle<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    req: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if ctx.webview_label() != MAIN_WINDOW {
        responder.respond(error_response(StatusCode::FORBIDDEN, None, None));
        return;
    }
    let head = req.method() == Method::HEAD;
    if req.method() != Method::GET && !head {
        responder.respond(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            origin.as_deref(),
            None,
        ));
        return;
    }
    // 보내기 전 동영상·음성 미리보기(X4): 데몬을 거치지 않고 등록한 로컬 파일을 내줍니다.
    if let Some(token) = req.uri().path().strip_prefix(crate::preview::PREFIX) {
        let range = req
            .headers()
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok());
        crate::preview::respond(ctx.app_handle(), token, range, origin, head, responder);
        return;
    }
    let parsed = match parse_request(req.uri().path(), req.uri().query()) {
        Ok(p) => p,
        Err(s) => {
            responder.respond(error_response(s, origin.as_deref(), None));
            return;
        }
    };
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);
    let app = ctx.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        let daemon = app.state::<Arc<Daemon>>().inner().clone();
        let Some(client) = daemon.client() else {
            responder.respond(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                origin.as_deref(),
                None,
            ));
            return;
        };
        let server = app.state::<BlobServer>();
        match server.serve(&client, &parsed, range).await {
            Ok(s) => {
                let len = s.body.len();
                let mut b = base(s.status, origin.as_deref())
                    .header(header::CONTENT_TYPE, content_type(parsed.mime, &s))
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::CONTENT_LENGTH, len);
                if let Some(cr) = &s.content_range {
                    b = b.header(header::CONTENT_RANGE, cr);
                }
                let body: Cow<'static, [u8]> = if head {
                    Cow::Borrowed(&[][..])
                } else {
                    Cow::Owned(s.body)
                };
                match b.body(body) {
                    Ok(resp) => responder.respond(resp),
                    Err(_) => responder.respond(error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        origin.as_deref(),
                        None,
                    )),
                }
            }
            Err((status, total)) => {
                debug!("블롭 요청 실패: {status}");
                responder.respond(error_response(status, origin.as_deref(), total));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (String, String, String) {
        (
            SpaceId::from_bytes([1; 16]).to_string(),
            BlobId::from_bytes([2; 32]).to_string(),
            EventId::from_bytes([3; 32]).to_string(),
        )
    }

    #[test]
    fn parses_valid_path() {
        let (s, b, k) = ids();
        let r = parse_request(&format!("/blob/{s}/{b}/{k}"), Some("mime=image%2Fpng")).unwrap();
        assert_eq!(r.space, SpaceId::from_bytes([1; 16]));
        assert_eq!(r.blob, BlobId::from_bytes([2; 32]));
        assert_eq!(r.key_from, EventId::from_bytes([3; 32]));
        assert_eq!(r.mime, "image/png");
        // 쿼리 없으면 바이너리.
        assert_eq!(
            parse_request(&format!("/blob/{s}/{b}/{k}"), None)
                .unwrap()
                .mime,
            OCTET
        );
    }

    #[test]
    fn rejects_bad_paths() {
        let (s, b, k) = ids();
        assert_eq!(parse_request("/other/x", None), Err(StatusCode::NOT_FOUND));
        assert_eq!(
            parse_request(&format!("/blob/{s}/{b}"), None),
            Err(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            parse_request(&format!("/blob/{s}/{b}/{k}/x"), None),
            Err(StatusCode::NOT_FOUND)
        );
        // 길이가 틀린 ID(공간 자리에 블롭 ID).
        assert_eq!(
            parse_request(&format!("/blob/{b}/{b}/{k}"), None),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            parse_request(&format!("/blob/{s}/0OIl/{k}"), None),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            parse_request(&format!("/blob/{s}/{}/{k}", "1".repeat(200)), None),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            parse_request(&format!("/blob/{s}/../{k}"), None),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn auto_image_mime_sniffs_only_images() {
        let (s, b, k) = ids();
        assert_eq!(
            parse_request(&format!("/blob/{s}/{b}/{k}"), Some("mime=auto"))
                .unwrap()
                .mime,
            AUTO_IMAGE
        );
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), "image/jpeg");
        assert_eq!(sniff_image(b"GIF89a.."), "image/gif");
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff_image(b"\0\0\0\x1cftypavif"), "image/avif");
        // SVG·HTML은 그림으로 내지 않습니다(스크립트 실행 위험).
        assert_eq!(sniff_image(b"<svg xmlns="), OCTET);
        assert_eq!(sniff_image(b"<html>"), OCTET);
        assert_eq!(sniff_image(b""), OCTET);
        let full = Served {
            status: StatusCode::OK,
            body: b"GIF89a".to_vec(),
            content_range: None,
        };
        assert_eq!(content_type(AUTO_IMAGE, &full), "image/gif");
        assert_eq!(content_type("video/mp4", &full), "video/mp4");
        let part = Served {
            status: StatusCode::PARTIAL_CONTENT,
            ..full
        };
        assert_eq!(content_type(AUTO_IMAGE, &part), OCTET);
    }

    #[test]
    fn mime_whitelist() {
        assert_eq!(sanitize_mime("IMAGE/PNG"), "image/png");
        assert_eq!(sanitize_mime("video/webm"), "video/webm");
        assert_eq!(sanitize_mime("image/svg+xml"), OCTET);
        assert_eq!(sanitize_mime("text/html"), OCTET);
        assert_eq!(sanitize_mime("image/png; charset=x"), OCTET);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-"), Some(RangeSpec::From(0)));
        assert_eq!(parse_range("bytes=10-19"), Some(RangeSpec::FromTo(10, 19)));
        assert_eq!(parse_range("bytes=-500"), Some(RangeSpec::Suffix(500)));
        assert_eq!(parse_range("bytes=5-1"), None);
        assert_eq!(parse_range("bytes=0-1,5-6"), None);
        assert_eq!(parse_range("items=0-1"), None);
        assert_eq!(parse_range("bytes=-0"), None);
        assert_eq!(parse_range("bytes=x-"), None);

        assert_eq!(resolve_range(RangeSpec::From(0), 100), Some((0, 99)));
        assert_eq!(
            resolve_range(RangeSpec::FromTo(90, 200), 100),
            Some((90, 99))
        );
        assert_eq!(resolve_range(RangeSpec::Suffix(10), 100), Some((90, 99)));
        assert_eq!(resolve_range(RangeSpec::Suffix(1000), 100), Some((0, 99)));
        assert_eq!(resolve_range(RangeSpec::From(100), 100), None);
        assert_eq!(resolve_range(RangeSpec::From(0), 0), None);
    }

    #[test]
    fn origins() {
        assert!(allowed_origin("tauri://localhost"));
        assert!(allowed_origin("http://tauri.localhost"));
        assert!(!allowed_origin("https://evil.example"));
        assert!(!allowed_origin("null"));
    }

    #[test]
    fn error_status_mapping() {
        use yangpa_rpc::RpcError;
        assert_eq!(
            status_of(&ClientError::Rpc(RpcError::new(1004, "x"))),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&ClientError::Rpc(RpcError::new(1007, "x"))),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_of(&ClientError::Timeout),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            status_of(&ClientError::Closed),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    fn req(blob: BlobId) -> BlobRequest {
        BlobRequest {
            space: crate::testutil::space(),
            blob,
            key_from: crate::testutil::key_from(),
            mime: "video/mp4",
        }
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn serves_whole_blob_across_windows() {
        // 창 두 개 + 조금(여러 번 읽기), 그리고 창의 정확한 배수(끝 판정이 1005로 옴).
        for len in [
            BLOB_READ_MAX_LEN as usize * 2 + 123,
            BLOB_READ_MAX_LEN as usize,
            10,
        ] {
            let data = sample(len);
            let fake = crate::testutil::FakeDaemon::new(data.clone());
            let client = crate::testutil::connect(fake.clone()).await;
            let server = BlobServer::default();
            let s = server
                .serve(&client, &req(crate::testutil::blob()), None)
                .await
                .ok()
                .unwrap();
            assert_eq!(s.status, StatusCode::OK);
            assert!(s.body == data, "길이 {len}");
            assert!(s.content_range.is_none());
            assert_eq!(
                server.sizes.lock().get(&crate::testutil::blob()),
                Some(&(len as u64))
            );
        }
    }

    #[tokio::test]
    async fn range_after_size_is_known_reads_only_window() {
        let data = sample(BLOB_READ_MAX_LEN as usize + 1000);
        let total = data.len() as u64;
        let fake = crate::testutil::FakeDaemon::new(data.clone());
        let client = crate::testutil::connect(fake.clone()).await;
        let server = BlobServer::default();
        let r = req(crate::testutil::blob());
        // 처음: 크기를 몰라 전체를 읽고 구간을 잘라 줍니다.
        let s = server
            .serve(&client, &r, Some(RangeSpec::From(0)))
            .await
            .ok()
            .unwrap();
        assert_eq!(s.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            s.content_range.as_deref(),
            Some(format!("bytes 0-{}/{total}", total - 1).as_str())
        );
        assert!(s.body == data);
        let before = fake.reads();
        // 다음: 요청 구간만 한 번 읽습니다.
        let s = server
            .serve(&client, &r, Some(RangeSpec::FromTo(100, 199)))
            .await
            .ok()
            .unwrap();
        assert_eq!(fake.reads(), before + 1);
        assert_eq!(s.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(s.body, data[100..200]);
        assert_eq!(
            s.content_range.as_deref(),
            Some(format!("bytes 100-199/{total}").as_str())
        );
        // 끝 쪽 구간은 실제 길이에 맞춰 줄입니다.
        let s = server
            .serve(&client, &r, Some(RangeSpec::Suffix(10)))
            .await
            .ok()
            .unwrap();
        assert_eq!(s.body, data[data.len() - 10..]);
        // 범위 밖은 416과 전체 크기.
        let e = server
            .serve(&client, &r, Some(RangeSpec::From(total)))
            .await
            .err()
            .unwrap();
        assert_eq!(e, (StatusCode::RANGE_NOT_SATISFIABLE, Some(total)));
    }

    #[tokio::test]
    async fn range_of_unknown_blob_asks_size_instead_of_reading_all() {
        // 크기를 데몬에 물을 수 있으면 처음 구간도 한 창만 읽습니다(MAX_SERVE보다 큰 영상도 이렇게 틂).
        let data = sample(BLOB_READ_MAX_LEN as usize * 2 + 10);
        let total = data.len() as u64;
        let fake = crate::testutil::FakeDaemon::new(data.clone());
        fake.status_size
            .store(total, std::sync::atomic::Ordering::SeqCst);
        let client = crate::testutil::connect(fake.clone()).await;
        let server = BlobServer::default();
        let s = server
            .serve(
                &client,
                &req(crate::testutil::blob()),
                Some(RangeSpec::From(0)),
            )
            .await
            .ok()
            .unwrap();
        assert_eq!(fake.reads(), 1);
        assert_eq!(s.status, StatusCode::PARTIAL_CONTENT);
        assert!(s.body == data[..BLOB_READ_MAX_LEN as usize]);
        assert_eq!(
            s.content_range.as_deref(),
            Some(format!("bytes 0-{}/{total}", BLOB_READ_MAX_LEN - 1).as_str())
        );
    }

    #[tokio::test]
    async fn full_reads_are_limited() {
        // 크기를 모르는 블롭 여러 개를 한꺼번에 요청해도 전체 읽기는 FULL_READ_CONCURRENCY개까지만 동시에 돕니다.
        let fake = crate::testutil::FakeDaemon::new(sample(10));
        fake.delay_ms.store(30, std::sync::atomic::Ordering::SeqCst);
        let client = crate::testutil::connect(fake.clone()).await;
        let server = Arc::new(BlobServer::default());
        let mut tasks = Vec::new();
        for i in 0..(FULL_READ_CONCURRENCY * 3) {
            let (server, client) = (server.clone(), client.clone());
            tasks.push(tokio::spawn(async move {
                let r = req(BlobId::from_bytes([i as u8 + 10; 32]));
                server.serve(&client, &r, None).await.is_ok()
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap());
        }
        assert_eq!(fake.reads(), FULL_READ_CONCURRENCY * 3);
        let max = fake.max_in_flight.load(std::sync::atomic::Ordering::SeqCst);
        assert!(max <= FULL_READ_CONCURRENCY, "동시 전체 읽기 {max}개");
        assert!(max >= 2, "제한이 너무 좁음: {max}");
    }

    #[tokio::test]
    async fn daemon_errors_become_statuses() {
        let fake = crate::testutil::FakeDaemon::new(sample(10));
        let client = crate::testutil::connect(fake).await;
        let server = BlobServer::default();
        let e = server
            .serve(&client, &req(crate::testutil::missing_blob()), None)
            .await
            .err()
            .unwrap();
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn error_response_headers() {
        let r = error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some("tauri://localhost"),
            Some(10),
        );
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let h = r.headers();
        assert!(
            h[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        assert_eq!(h[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "tauri://localhost");
        let r = error_response(StatusCode::NOT_FOUND, Some("https://evil.example"), None);
        assert!(
            r.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }
}
