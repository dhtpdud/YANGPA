//! `/yangpa/rpc/5` 요청-응답 코덱(OpenZL로 압축한 CBOR).
//!
//! 스트림 하나에 `yangpa_zl::pack(CBOR)` 하나(줄지 않으면 표시 바이트 + CBOR 그대로). 한도([`MAX_WIRE_MESSAGE`])는
//! 압축 전 CBOR에 겁니다: 보내기 전에 크기를 검사해 상대가 잘린 메시지를 받지 않게 하고, 받을 때는 받은 바이트와
//! 풀린 크기(프레임 머리글로 먼저 봄)가 한도를 넘으면 풀지 않고 바로 거부합니다.
//!
//! 들어오는 요청은 인증 전의 아무 노드나 보낼 수 있으므로, 노드 전체에서 동시에 읽는 중인 요청 바이트에
//! 상한([`MAX_INFLIGHT_READ`])을 둡니다. 연결 수 × 연결당 스트림 수 × 8MiB만큼 메모리를 채우는 공격을 막습니다.
//! 압축된 요청은 풀린 크기만큼 예산을 더 잡은 뒤에 풉니다(작은 프레임으로 큰 메모리를 쓰게 하지 못하게).
//!
//! 코덱은 상대가 누구인지 모르므로 압축된 요청을 풀지 않고 [`Incoming`]에 담아 넘깁니다. 데몬이 보낸 노드를
//! 인증한 것을 확인한 뒤에만 풀어, 인증 전 상대의 입력이 OpenZL(C) 해제기에 닿지 않게 합니다. 인증 전에도 받는
//! 요청([`YpRequest::before_auth`])은 요청·응답 모두 압축하지 않고, 그런 요청의 압축된 응답은 받지 않습니다.
//! libp2p가 스트림마다 코덱을 복제해 요청과 응답을 같은 복제본으로 처리하므로 그 요청의 종류를 기억해 둡니다.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::prelude::*;
use libp2p::StreamProtocol;
use libp2p::request_response;
use serde::Serialize;
use serde::de::DeserializeOwned;
use yangpa_core::wire::{MAX_WIRE_MESSAGE, WireErrorCode, YpRequest, YpResponse};

use crate::Traffic;

/// 노드 전체에서 동시에 읽고 있는(아직 해석 전인) 수신 요청 바이트 상한.
pub(crate) const MAX_INFLIGHT_READ: usize = 128 * 1024 * 1024;
/// 노드 전체에서 풀어 처리 중인 압축 요청의 풀린 바이트 상한([`Opened`]).
const MAX_OPENED: usize = 128 * 1024 * 1024;
/// 한 번에 읽는 조각 크기.
const READ_CHUNK: usize = 32 * 1024;

/// 요청과 그 크기(압축 전 CBOR, 압축된 요청은 받은 바이트). 받은 요청의 크기는 스웜 작업이 데몬 대기열의
/// 메모리 상한을 셀 때 씁니다. 보낼 때는 `len`을 쓰지 않습니다.
#[derive(Debug)]
pub(crate) struct WireRequest {
    pub req: Incoming,
    pub len: usize,
}

impl WireRequest {
    pub fn outbound(req: YpRequest) -> Self {
        WireRequest {
            req: Incoming(Body::Plain(req)),
            len: 0,
        }
    }
}

/// 받은 요청. 압축된 요청은 보낸 노드를 인증한 뒤에만 [`Incoming::open`]으로 풉니다.
#[derive(Debug)]
pub struct Incoming(Body);

enum Body {
    Plain(YpRequest),
    Packed {
        data: Vec<u8>,
        max: usize,
        budget: Arc<ReadBudget>,
    },
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Plain(r) => r.fmt(f),
            Body::Packed { data, .. } => write!(f, "Packed({}바이트)", data.len()),
        }
    }
}

impl Incoming {
    /// 압축된 채 와서 풀어야 하는 요청인지.
    pub fn is_packed(&self) -> bool {
        matches!(self.0, Body::Packed { .. })
    }

    /// 압축되지 않은 요청이면 그 요청.
    pub(crate) fn plain(&self) -> Option<&YpRequest> {
        match &self.0 {
            Body::Plain(r) => Some(r),
            Body::Packed { .. } => None,
        }
    }

    /// 요청을 꺼냅니다. 압축본은 이때 풀고, 풀린 크기만큼 노드 예산을 잡은 표식을 함께 돌려줍니다. 스웜 작업의
    /// 대기 상한은 압축본을 받은 바이트로만 세므로, 처리가 끝날 때까지 이 표식을 쥐어 푼 요청의 메모리를 묶습니다.
    /// 실패하면 상대에게 보낼 답을 돌려줍니다.
    pub fn open(self) -> Result<(YpRequest, Option<Opened>), Box<YpResponse>> {
        let (data, max, budget) = match self.0 {
            Body::Plain(r) => return Ok((r, None)),
            Body::Packed { data, max, budget } => (data, max, budget),
        };
        let bad = || {
            Box::new(YpResponse::error(
                WireErrorCode::BadRequest,
                "요청을 풀 수 없음",
            ))
        };
        let size = yangpa_zl::unpacked_size(&data).map_err(|_| bad())?;
        if size > max {
            return Err(bad());
        }
        if !budget.try_take(size) {
            return Err(Box::new(YpResponse::Busy {
                retry_after_ms: crate::task::BUSY_RETRY_MS,
            }));
        }
        let held = Opened { budget, n: size };
        let plain = yangpa_zl::unpack(&data, max).map_err(|_| bad())?;
        let req = cbor4ii::serde::from_slice(&plain).map_err(|_| bad())?;
        Ok((req, Some(held)))
    }
}

/// 푼 압축 요청이 잡은 예산. 버리면 돌려줍니다.
#[derive(Debug)]
pub struct Opened {
    budget: Arc<ReadBudget>,
    n: usize,
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.n, Ordering::AcqRel);
    }
}

/// 읽기 예산을 넘었다는 표식.
#[derive(Debug, thiserror::Error)]
#[error("수신 요청이 너무 많이 쌓여 있음")]
pub(crate) struct ReadBusy;

/// 동시에 읽는 바이트 예산(노드 하나에 하나, 코덱 복제본이 함께 씀).
#[derive(Debug)]
pub(crate) struct ReadBudget {
    used: AtomicUsize,
    max: usize,
}

impl ReadBudget {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(ReadBudget {
            used: AtomicUsize::new(0),
            max,
        })
    }

    // `fetch_update`는 최신 안정판에서 폐기 예고되었고 대체 함수는 MSRV(1.91)에 없어 직접 CAS 반복으로 씁니다.
    fn try_take(&self, n: usize) -> bool {
        let mut cur = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = cur.checked_add(n).filter(|next| *next <= self.max) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    #[cfg(test)]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

/// 읽는 동안 잡은 예산. 읽기가 끝나거나(성공·실패·취소) 버려지면 돌려줍니다.
struct Held<'a> {
    budget: &'a ReadBudget,
    n: usize,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.n, Ordering::AcqRel);
    }
}

/// 크기 초과 오류 표식. 상위에서 [`crate::NetError::TooLarge`]로 바꿉니다.
#[derive(Debug, thiserror::Error)]
#[error("메시지가 너무 큼: {size}바이트(최대 {max}바이트)")]
pub(crate) struct TooLarge {
    pub size: usize,
    pub max: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct YpCodec {
    max: usize,
    budget: Arc<ReadBudget>,
    /// 압축 요청을 풀 때 잡는 예산([`Opened`]).
    opened: Arc<ReadBudget>,
    traffic: Traffic,
    /// 이 스트림의 요청이 인증 전에도 받는 종류인지(스트림마다 복제되는 코덱이라 그 스트림의 응답에만 쓰임).
    before_auth: bool,
}

impl Default for YpCodec {
    fn default() -> Self {
        Self::new(Traffic::default())
    }
}

impl YpCodec {
    pub(crate) fn new(traffic: Traffic) -> Self {
        YpCodec {
            max: MAX_WIRE_MESSAGE,
            budget: ReadBudget::new(MAX_INFLIGHT_READ),
            opened: ReadBudget::new(MAX_OPENED),
            traffic,
            before_auth: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(max: usize, budget: Arc<ReadBudget>) -> Self {
        YpCodec {
            max,
            opened: budget.clone(),
            budget,
            traffic: Traffic::default(),
            before_auth: false,
        }
    }

    fn encode<T: Serialize>(&self, v: &T) -> io::Result<Vec<u8>> {
        if self.before_auth {
            encode_raw(v, self.max)
        } else {
            encode(v, self.max)
        }
    }
}

/// CBOR로 직렬화해 한도를 검사한 뒤 압축합니다(줄지 않으면 표시 바이트만 붙임).
pub(crate) fn encode<T: Serialize>(v: &T, max: usize) -> io::Result<Vec<u8>> {
    Ok(yangpa_zl::pack(&to_cbor(v, max)?).to_vec())
}

/// [`encode`]와 같은 형식이되 압축하지 않습니다.
fn encode_raw<T: Serialize>(v: &T, max: usize) -> io::Result<Vec<u8>> {
    Ok(yangpa_zl::pack_raw(&to_cbor(v, max)?))
}

fn to_cbor<T: Serialize>(v: &T, max: usize) -> io::Result<Vec<u8>> {
    let data = cbor4ii::serde::to_vec(Vec::new(), v).map_err(io::Error::other)?;
    if data.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            TooLarge {
                size: data.len(),
                max,
            },
        ));
    }
    Ok(data)
}

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// [`encode`]의 역. 풀린(CBOR) 크기를 함께 돌려줍니다. 압축된 프레임은 풀린 크기가 `max`를 넘으면 풀지 않습니다.
pub(crate) fn decode<T: DeserializeOwned>(data: &[u8], max: usize) -> io::Result<(T, usize)> {
    let size = yangpa_zl::unpacked_size(data).map_err(invalid)?;
    if size > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            TooLarge { size, max },
        ));
    }
    if data[0] == yangpa_zl::TAG_RAW {
        let v = cbor4ii::serde::from_slice(&data[1..]).map_err(invalid)?;
        return Ok((v, size));
    }
    let plain = yangpa_zl::unpack(data, max).map_err(invalid)?;
    let v = cbor4ii::serde::from_slice(&plain).map_err(invalid)?;
    Ok((v, size))
}

/// 상대가 쓰기를 닫을 때까지 읽습니다. 한도 + 1바이트까지만 읽어 넘쳤는지 판별하고, `budget`이 있으면
/// 읽은 만큼 노드 전체 예산에서 잡습니다(예산이 모자라면 [`ReadBusy`]).
pub(crate) async fn read_limited<T: AsyncRead + Unpin + Send>(
    io: &mut T,
    max: usize,
    budget: Option<&ReadBudget>,
) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK.min(max + 1)];
    let mut held = budget.map(|b| Held { budget: b, n: 0 });
    loop {
        let want = chunk.len().min(max + 1 - buf.len());
        let n = io.read(&mut chunk[..want]).await?;
        if n == 0 {
            break;
        }
        if buf.len() + n > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                TooLarge {
                    size: buf.len() + n,
                    max,
                },
            ));
        }
        if let Some(h) = held.as_mut() {
            if !h.budget.try_take(n) {
                return Err(io::Error::other(ReadBusy));
            }
            h.n += n;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

impl request_response::Codec for YpCodec {
    type Protocol = StreamProtocol;
    type Request = WireRequest;
    type Response = YpResponse;

    async fn read_request<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<WireRequest>
    where
        T: AsyncRead + Unpin + Send,
    {
        // 받은 바이트는 압축 전 한도 + 표시 바이트 1개까지(줄지 않은 메시지).
        let buf = read_limited(io, self.max + 1, Some(self.budget.as_ref())).await?;
        self.traffic.add_received(buf.len());
        if buf.first() == Some(&yangpa_zl::TAG_ZL) {
            // 풀린 크기를 보는 것도 C 코드라 여기서는 받은 바이트로만 셉니다. 풀 때 풀린 크기만큼 예산을 잡습니다.
            let len = buf.len();
            let req = Incoming(Body::Packed {
                data: buf,
                max: self.max,
                budget: self.opened.clone(),
            });
            return Ok(WireRequest { req, len });
        }
        let (req, len): (YpRequest, _) = decode(&buf, self.max)?;
        self.before_auth = req.before_auth();
        Ok(WireRequest {
            req: Incoming(Body::Plain(req)),
            len,
        })
    }

    async fn read_response<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<YpResponse>
    where
        T: AsyncRead + Unpin + Send,
    {
        // 응답은 이 노드가 보낸 요청에 대한 것이라 동시 개수를 데몬이 정하므로 예산을 쓰지 않습니다.
        let buf = read_limited(io, self.max + 1, None).await?;
        self.traffic.add_received(buf.len());
        if self.before_auth && buf.first() == Some(&yangpa_zl::TAG_ZL) {
            return Err(invalid("인증 전 요청의 응답이 압축되어 옴"));
        }
        decode(&buf, self.max).map(|(res, _)| res)
    }

    async fn write_request<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        req: WireRequest,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let Some(req) = req.req.plain() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "압축된 채인 요청은 보낼 수 없음",
            ));
        };
        self.before_auth = req.before_auth();
        let data = self.encode(req)?;
        io.write_all(&data).await?;
        self.traffic.add_sent(data.len());
        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        res: YpResponse,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        // 응답이 한도를 넘으면 상대가 잘린 데이터를 해석하지 않도록 오류 응답으로 바꿔 보냅니다.
        let data = match self.encode(&res) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => self.encode(
                &YpResponse::error(WireErrorCode::Internal, "응답이 너무 큼"),
            )?,
            Err(e) => return Err(e),
        };
        io.write_all(&data).await?;
        self.traffic.add_sent(data.len());
        Ok(())
    }
}

/// 입출력 오류가 크기 초과인지 확인합니다.
pub(crate) fn as_too_large(e: &io::Error) -> Option<(usize, usize)> {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<TooLarge>())
        .map(|t| (t.size, t.max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::Cursor;
    use request_response::Codec as _;
    use yangpa_core::ids::{EventId, SpaceId};

    fn proto() -> StreamProtocol {
        StreamProtocol::new(yangpa_core::wire::RPC_PROTOCOL)
    }

    #[tokio::test]
    async fn roundtrip_request_and_response() {
        let mut codec = YpCodec::default();
        let req = YpRequest::GetEvents {
            space: SpaceId::random(),
            ids: vec![EventId::hash_of(b"a"), EventId::hash_of(b"b")],
        };
        let mut buf = Vec::new();
        codec
            .write_request(&proto(), &mut buf, WireRequest::outbound(req.clone()))
            .await
            .unwrap();
        let cbor_len = cbor4ii::serde::to_vec(Vec::new(), &req).unwrap().len();
        let back = codec
            .read_request(&proto(), &mut Cursor::new(buf))
            .await
            .unwrap();
        assert_eq!(back.req.open().unwrap().0, req);
        // 받은 요청은 (압축 전) 직렬화 크기를 함께 알려 준다.
        assert_eq!(back.len, cbor_len);
        // 읽기가 끝나면 예산을 돌려준다.
        assert_eq!(codec.budget.used(), 0);

        let res = YpResponse::Chunk {
            data: vec![7; 1000],
        };
        let mut buf = Vec::new();
        codec
            .write_response(&proto(), &mut buf, res.clone())
            .await
            .unwrap();
        let back = codec
            .read_response(&proto(), &mut Cursor::new(buf))
            .await
            .unwrap();
        assert_eq!(back, res);
    }

    #[tokio::test]
    async fn signal_payloads_roundtrip() {
        use yangpa_core::wire::SignalPayload;
        let mut codec = YpCodec::default();
        for payload in [
            SignalPayload::Join {
                muted: true,
                deafened: false,
                screen: true,
                generation: 4,
                reply: true,
                sfu: None,
                session: 0,
            },
            SignalPayload::MediaKey {
                generation: 7,
                sealed: vec![9; 120],
            },
            SignalPayload::Ice {
                candidate: "candidate:1".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: None,
            },
        ] {
            let req = YpRequest::Signal {
                space: SpaceId::random(),
                channel: yangpa_core::ids::ChannelId::random(),
                call: yangpa_core::ids::CallId::random(),
                from: yangpa_core::ids::DeviceId([3; 32]),
                payload,
            };
            let mut buf = Vec::new();
            codec
                .write_request(&proto(), &mut buf, WireRequest::outbound(req.clone()))
                .await
                .unwrap();
            let back = codec
                .read_request(&proto(), &mut Cursor::new(buf))
                .await
                .unwrap();
            assert_eq!(back.req.open().unwrap().0, req);
        }
    }

    #[tokio::test]
    async fn oversized_request_rejected_on_both_sides() {
        let mut codec = YpCodec::with_limits(256, ReadBudget::new(MAX_INFLIGHT_READ));
        let big = YpResponse::Chunk {
            data: vec![1; 1024],
        };
        // 보내는 쪽: 작은 요청은 통과하고 큰 요청은 보내기 전에 오류.
        let small = YpRequest::BlobChunk {
            space: SpaceId::random(),
            blob: yangpa_core::ids::BlobId([0; 32]),
            index: 1,
        };
        let mut buf = Vec::new();
        codec
            .write_request(&proto(), &mut buf, WireRequest::outbound(small))
            .await
            .unwrap();
        let big_req = YpRequest::Signal {
            space: SpaceId::random(),
            channel: yangpa_core::ids::ChannelId::random(),
            call: yangpa_core::ids::CallId::random(),
            from: yangpa_core::ids::DeviceId([0; 32]),
            payload: yangpa_core::wire::SignalPayload::Offer {
                sdp: "v".repeat(1024),
            },
        };
        let mut buf = Vec::new();
        let err = codec
            .write_request(&proto(), &mut buf, WireRequest::outbound(big_req))
            .await
            .unwrap_err();
        assert!(as_too_large(&err).is_some());
        assert!(buf.is_empty(), "한도를 넘으면 한 바이트도 쓰지 않는다");
        // 응답은 오류 응답으로 대체된다.
        let mut buf = Vec::new();
        codec.write_response(&proto(), &mut buf, big).await.unwrap();
        let back = codec
            .read_response(&proto(), &mut Cursor::new(buf))
            .await
            .unwrap();
        assert!(matches!(back, YpResponse::Error { .. }));
        // 받는 쪽: 한도를 넘는 바이트열은 해석하지 않는다.
        let raw = vec![0u8; 300];
        let err = codec
            .read_request(&proto(), &mut Cursor::new(raw))
            .await
            .unwrap_err();
        assert_eq!(as_too_large(&err), Some((258, 257)));
        assert_eq!(codec.budget.used(), 0, "실패해도 예산을 돌려준다");
        // 압축본은 코덱이 풀지 않고, 풀 때 풀린 크기가 한도를 넘으면 풀지 않는다.
        let bomb = yangpa_zl::pack(&vec![0u8; 100_000]).to_vec();
        assert!(bomb.len() < 256);
        let back = codec
            .read_request(&proto(), &mut Cursor::new(bomb))
            .await
            .unwrap();
        assert!(back.req.is_packed());
        assert!(matches!(
            back.req.open().map(|_| ()).map_err(|e| *e),
            Err(YpResponse::Error { .. })
        ));
        assert_eq!(codec.budget.used(), 0);
    }

    #[tokio::test]
    async fn compressible_messages_shrink_and_charge_unpacked_size() {
        let req = YpRequest::Signal {
            space: SpaceId::random(),
            channel: yangpa_core::ids::ChannelId::random(),
            call: yangpa_core::ids::CallId::random(),
            from: yangpa_core::ids::DeviceId([0; 32]),
            payload: yangpa_core::wire::SignalPayload::Offer {
                sdp: "a=candidate 1 udp 2130706431 192.168.0.2 5000 typ host\r\n".repeat(2000),
            },
        };
        let data = encode(&req, MAX_WIRE_MESSAGE).unwrap();
        let cbor_len = cbor4ii::serde::to_vec(Vec::new(), &req).unwrap().len();
        assert_eq!(data[0], yangpa_zl::TAG_ZL);
        assert!(data.len() < cbor_len / 10, "{} / {cbor_len}", data.len());
        // 코덱은 압축본을 풀지 않고 받은 바이트로 센다(상대를 확인한 뒤 데몬이 품).
        let budget = ReadBudget::new(cbor_len - 1);
        let mut codec = YpCodec::with_limits(MAX_WIRE_MESSAGE, budget.clone());
        let back = codec
            .read_request(&proto(), &mut Cursor::new(data.clone()))
            .await
            .unwrap();
        assert!(back.req.is_packed());
        assert_eq!(back.len, data.len());
        // 풀린 크기만큼 예산이 없으면 풀지 않고 바쁘다고 답한다.
        assert!(matches!(
            back.req.open().map(|_| ()).map_err(|e| *e),
            Err(YpResponse::Busy { .. })
        ));
        assert_eq!(budget.used(), 0);
        let budget = ReadBudget::new(cbor_len);
        let mut codec = YpCodec::with_limits(MAX_WIRE_MESSAGE, budget.clone());
        let back = codec
            .read_request(&proto(), &mut Cursor::new(data))
            .await
            .unwrap();
        assert_eq!(back.req.open().unwrap().0, req);
        assert_eq!(budget.used(), 0);
    }

    #[tokio::test]
    async fn before_auth_requests_and_replies_stay_uncompressed() {
        let zeros = YpResponse::Chunk {
            data: vec![0; 10_000],
        };
        // 받는 쪽: 인증 전에도 받는 요청의 응답은 줄어도 압축하지 않는다.
        let mut responder = YpCodec::default();
        let mut buf = Vec::new();
        responder
            .write_request(&proto(), &mut buf, WireRequest::outbound(YpRequest::Ping))
            .await
            .unwrap();
        let mut responder = YpCodec::default();
        responder
            .read_request(&proto(), &mut Cursor::new(buf))
            .await
            .unwrap();
        let mut reply = Vec::new();
        responder
            .write_response(&proto(), &mut reply, zeros.clone())
            .await
            .unwrap();
        assert_eq!(reply[0], yangpa_zl::TAG_RAW);
        // 보낸 쪽: 그런 요청에 압축된 응답이 오면 풀지 않는다.
        let packed = encode(&zeros, MAX_WIRE_MESSAGE).unwrap();
        assert_eq!(packed[0], yangpa_zl::TAG_ZL);
        let mut requester = YpCodec::default();
        requester
            .write_request(
                &proto(),
                &mut Vec::new(),
                WireRequest::outbound(YpRequest::Ping),
            )
            .await
            .unwrap();
        let err = requester
            .read_response(&proto(), &mut Cursor::new(packed.clone()))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            requester
                .read_response(&proto(), &mut Cursor::new(reply))
                .await
                .unwrap(),
            zeros
        );
        // 인증 뒤 요청의 응답은 그대로 압축해 주고받는다.
        let mut requester = YpCodec::default();
        let get = YpRequest::GetHeads {
            space: SpaceId::random(),
            stream: yangpa_core::event::StreamId::Space,
        };
        requester
            .write_request(&proto(), &mut Vec::new(), WireRequest::outbound(get))
            .await
            .unwrap();
        assert_eq!(
            requester
                .read_response(&proto(), &mut Cursor::new(packed))
                .await
                .unwrap(),
            zeros
        );
    }

    #[tokio::test]
    async fn inflight_read_budget_is_shared_and_released() {
        let budget = ReadBudget::new(150);
        let mut codec = YpCodec::with_limits(MAX_WIRE_MESSAGE, budget.clone());
        let req = YpRequest::GetEvents {
            space: SpaceId::random(),
            ids: vec![EventId::hash_of(b"a"), EventId::hash_of(b"b")],
        };
        let data = encode(&req, MAX_WIRE_MESSAGE).unwrap();
        assert!(data.len() > 90 && data.len() <= 150, "{}", data.len());
        // 혼자 읽을 때는 예산 안이다.
        let back = codec
            .read_request(&proto(), &mut Cursor::new(data.clone()))
            .await
            .unwrap();
        assert_eq!(back.req.open().unwrap().0, req);
        assert_eq!(budget.used(), 0);
        // 다른 연결이 예산을 잡고 있으면(읽는 중) 새 요청은 읽지 않고 거절한다.
        assert!(budget.try_take(60));
        let err = codec
            .read_request(&proto(), &mut Cursor::new(data.clone()))
            .await
            .unwrap_err();
        assert!(
            err.get_ref()
                .is_some_and(|e| e.downcast_ref::<ReadBusy>().is_some()),
            "{err:?}"
        );
        assert_eq!(budget.used(), 60, "거절된 읽기는 잡은 만큼 돌려준다");
        budget.used.fetch_sub(60, Ordering::AcqRel);
        // 응답 읽기는 이 노드가 보낸 요청에 대한 것이라 예산과 무관하다.
        assert!(budget.try_take(150));
        let res = YpResponse::Chunk { data: vec![1; 300] };
        let mut buf = Vec::new();
        codec
            .write_response(&proto(), &mut buf, res.clone())
            .await
            .unwrap();
        assert_eq!(
            codec
                .read_response(&proto(), &mut Cursor::new(buf))
                .await
                .unwrap(),
            res
        );
    }

    #[test]
    fn budget_never_exceeds_max() {
        let b = ReadBudget::new(10);
        assert!(b.try_take(10));
        assert!(!b.try_take(1));
        assert!(!b.try_take(usize::MAX));
        b.used.fetch_sub(10, Ordering::AcqRel);
        assert!(!b.try_take(11));
        assert!(b.try_take(4) && b.try_take(6));
        assert_eq!(b.used(), 10);
    }

    #[tokio::test]
    async fn garbage_is_invalid_data() {
        let mut codec = YpCodec::default();
        let err = codec
            .read_request(&proto(), &mut Cursor::new(vec![0xff, 0x00, 0x13]))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encode_reports_size() {
        let err = encode(&YpResponse::Chunk { data: vec![0; 100] }, 10).unwrap_err();
        let (size, max) = as_too_large(&err).unwrap();
        assert!(size > 100);
        assert_eq!(max, 10);
    }
}
