//! `/yangpa/media/1`: 통화의 실시간 미디어 프레임을 노드 사이 연결(Tor 회로 포함)로 나르는 한 방향 전송.
//!
//! WebRTC는 연결을 만드는 것만으로 IP 후보를 모으므로, Tor로 도는 기기의 통화는 앱이 봉인한 소리 패킷을 데몬이 이
//! 프로토콜로 상대 데몬에 넘깁니다. 내용은 앱이 통화 미디어 키로 이미 암호화했으므로 네트워크 계층은 바이트만 나릅니다.
//!
//! - 요청 하나 = 프레임 묶음(`[u16 BE 개수]([u16 BE 길이][바이트])…`), 응답은 0바이트 하나(받았다는 표시)입니다. 압축
//!   (OpenZL)하지 않습니다: 실시간 데이터는 압축 범위에서 빼기로 했고, 내용이 이미 암호문이라 줄지도 않습니다.
//! - 상대마다 동시에 보내는 요청을 [`MAX_IN_FLIGHT`]개로 묶고, 그동안 생긴 프레임은 모아 다음 요청에 함께 싣습니다
//!   (Tor 왕복이 길어도 패킷마다 요청이 쌓이지 않게). 쌓인 양이 [`MAX_QUEUED_BYTES`]를 넘으면 오래된 프레임부터 버립니다
//!   (늦은 소리는 쓸모가 없음).
//! - RPC(`/yangpa/rpc/5`)와 따로 된 요청-응답 동작이라 RPC의 피어당 동시 요청 수를 쓰지 않습니다.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use futures::prelude::*;
use libp2p::{PeerId, StreamProtocol};

use crate::codec::{ReadBudget, read_limited};

pub const MEDIA_PROTOCOL: &str = "/yangpa/media/1";
/// 프레임 하나의 최대 크기(네트워크 계층 한도, 데몬은 더 좁게 봄).
pub const MAX_FRAME: usize = 16 * 1024;
/// 요청 하나(묶음)의 최대 크기.
pub const MAX_BATCH: usize = 64 * 1024;
/// 묶음 하나의 최대 프레임 수.
const MAX_FRAMES: usize = 256;
/// 상대마다 동시에 보내는 요청 수.
pub(crate) const MAX_IN_FLIGHT: usize = 8;
/// 상대마다 보내지 못하고 쌓아 둘 수 있는 바이트.
/// 비상용 화면공유의 키 프레임(수십 KiB)이 한꺼번에 들어갈 만큼.
pub(crate) const MAX_QUEUED_BYTES: usize = 256 * 1024;
/// 노드 전체에서 동시에 읽는 중인 미디어 묶음 바이트 상한. 인증 전 피어도 스트림을 열 수 있으므로(onion에 거는 누구나),
/// 연결 수 × 스트림 수 × 64KiB만큼 메모리를 채우지 못하게 RPC처럼 묶습니다(`codec::MAX_INFLIGHT_READ`와 따로).
pub(crate) const MAX_INFLIGHT_MEDIA: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct MediaCodec {
    budget: Arc<ReadBudget>,
}

impl MediaCodec {
    pub(crate) fn new(budget: Arc<ReadBudget>) -> Self {
        MediaCodec { budget }
    }
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "미디어 묶음이 너무 큼")
}

/// 묶음을 바이트로. 한도를 넘는 프레임은 부르는 쪽이 미리 거릅니다.
pub(crate) fn encode_batch(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + frames.iter().map(|f| 2 + f.len()).sum::<usize>());
    out.extend_from_slice(&(frames.len() as u16).to_be_bytes());
    for f in frames {
        out.extend_from_slice(&(f.len() as u16).to_be_bytes());
        out.extend_from_slice(f);
    }
    out
}

pub(crate) fn decode_batch(data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "미디어 묶음 형식이 잘못됨");
    let (n, mut rest) = data.split_first_chunk::<2>().ok_or_else(bad)?;
    let n = u16::from_be_bytes(*n) as usize;
    if n > MAX_FRAMES {
        return Err(too_large());
    }
    let mut frames = Vec::with_capacity(n);
    for _ in 0..n {
        let (len, r) = rest.split_first_chunk::<2>().ok_or_else(bad)?;
        let len = u16::from_be_bytes(*len) as usize;
        if len > MAX_FRAME || r.len() < len {
            return Err(bad());
        }
        frames.push(r[..len].to_vec());
        rest = &r[len..];
    }
    if !rest.is_empty() {
        return Err(bad());
    }
    Ok(frames)
}

impl libp2p::request_response::Codec for MediaCodec {
    type Protocol = StreamProtocol;
    type Request = Vec<Vec<u8>>;
    type Response = ();

    async fn read_request<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        // 읽는 동안 노드 전체 예산을 잡습니다(모자라면 읽지 않고 스트림을 실패시킴).
        let buf = read_limited(io, MAX_BATCH, Some(self.budget.as_ref())).await?;
        decode_batch(&buf)
    }

    async fn read_response<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<()>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut buf = [0u8; 1];
        io.read_exact(&mut buf).await
    }

    async fn write_request<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&encode_batch(&req)).await?;
        io.close().await
    }

    async fn write_response<T>(&mut self, _: &StreamProtocol, io: &mut T, _: ()) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&[0]).await?;
        io.close().await
    }
}

/// 상대 하나로 보낼 프레임.
#[derive(Default)]
struct Outbox {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    in_flight: usize,
}

/// 상대별 보내기 대기열(스웜 작업이 들고 있음).
#[derive(Default)]
pub(crate) struct MediaOut {
    peers: HashMap<PeerId, Outbox>,
    /// 버린 프레임 수(시험·진단용).
    pub dropped: u64,
}

impl MediaOut {
    /// 프레임을 넣습니다. 지금 보낼 수 있으면 보낼 묶음을 돌려줍니다.
    pub fn push(&mut self, peer: PeerId, frame: Vec<u8>) -> Option<Vec<Vec<u8>>> {
        if frame.is_empty() || frame.len() > MAX_FRAME {
            self.dropped += 1;
            return None;
        }
        let o = self.peers.entry(peer).or_default();
        o.bytes += frame.len();
        o.queue.push_back(frame);
        while o.bytes > MAX_QUEUED_BYTES {
            let Some(old) = o.queue.pop_front() else {
                break;
            };
            o.bytes -= old.len();
            self.dropped += 1;
        }
        Self::take(o)
    }

    /// 요청 하나가 끝났을 때(성공·실패). 쌓인 것이 있으면 다음 묶음을 돌려줍니다.
    pub fn done(&mut self, peer: &PeerId) -> Option<Vec<Vec<u8>>> {
        let o = self.peers.get_mut(peer)?;
        o.in_flight = o.in_flight.saturating_sub(1);
        let next = Self::take(o);
        if o.in_flight == 0 && o.queue.is_empty() {
            self.peers.remove(peer);
        }
        next
    }

    /// 연결이 끊긴 상대의 대기열을 버립니다.
    pub fn forget(&mut self, peer: &PeerId) {
        self.peers.remove(peer);
    }

    fn take(o: &mut Outbox) -> Option<Vec<Vec<u8>>> {
        if o.in_flight >= MAX_IN_FLIGHT || o.queue.is_empty() {
            return None;
        }
        let mut batch = Vec::new();
        let mut size = 2;
        while let Some(f) = o.queue.front() {
            if size + 2 + f.len() > MAX_BATCH || batch.len() >= MAX_FRAMES {
                break;
            }
            size += 2 + f.len();
            let f = o.queue.pop_front().expect("방금 봄");
            o.bytes -= f.len();
            batch.push(f);
        }
        o.in_flight += 1;
        Some(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::request_response::Codec as _;

    #[tokio::test]
    async fn reads_take_the_shared_budget_and_fail_when_it_runs_out() {
        let proto = StreamProtocol::new(MEDIA_PROTOCOL);
        let data = encode_batch(&[vec![7; 1000]]);
        let budget = ReadBudget::new(data.len());
        let mut codec = MediaCodec::new(budget.clone());
        let got = codec
            .read_request(&proto, &mut futures::io::Cursor::new(data.clone()))
            .await
            .unwrap();
        assert_eq!(got, vec![vec![7; 1000]]);
        assert_eq!(budget.used(), 0, "읽은 뒤 돌려줌");
        // 다른 스트림이 예산을 잡고 있으면 읽지 않습니다.
        let mut small = MediaCodec::new(ReadBudget::new(10));
        assert!(
            small
                .read_request(&proto, &mut futures::io::Cursor::new(data))
                .await
                .is_err()
        );
    }

    #[test]
    fn batch_round_trip_and_bad_input() {
        let frames = vec![vec![1, 2, 3], vec![], vec![9; 300]];
        assert_eq!(decode_batch(&encode_batch(&frames)).unwrap(), frames);
        assert!(decode_batch(&[]).is_err());
        assert!(decode_batch(&[0, 1, 0, 5, 1]).is_err());
        assert!(decode_batch(&[0, 0, 7]).is_err());
        assert!(decode_batch(&[0x01, 0x01]).is_err());
    }

    #[test]
    fn frames_are_batched_while_requests_are_in_flight() {
        let peer = PeerId::random();
        let mut out = MediaOut::default();
        for i in 0..MAX_IN_FLIGHT {
            assert_eq!(out.push(peer, vec![i as u8]), Some(vec![vec![i as u8]]));
        }
        // 자리가 없으면 모아 두었다가, 하나 끝나면 모은 것을 한 묶음으로.
        assert_eq!(out.push(peer, vec![100]), None);
        assert_eq!(out.push(peer, vec![101]), None);
        assert_eq!(out.done(&peer), Some(vec![vec![100], vec![101]]));
        assert_eq!(out.done(&peer), None);
    }

    #[test]
    fn old_frames_are_dropped_when_the_queue_is_full() {
        let peer = PeerId::random();
        let mut out = MediaOut::default();
        for _ in 0..MAX_IN_FLIGHT {
            out.push(peer, vec![0; 10]);
        }
        let n = MAX_QUEUED_BYTES / 1000 + 5;
        for i in 0..n {
            out.push(peer, vec![i as u8; 1000]);
        }
        assert!(out.dropped >= 5);
        // 자리가 날 때마다 묶음 하나씩(묶음 한도 안): 끝까지 꺼내면 가장 새 프레임이 남아 있고 합은 대기열 한도 안.
        let mut drained = Vec::new();
        while let Some(batch) = out.done(&peer) {
            assert!(batch.iter().map(|f| f.len() + 2).sum::<usize>() + 2 <= MAX_BATCH);
            drained.extend(batch);
        }
        assert_eq!(
            drained.last().unwrap()[0],
            (n - 1) as u8,
            "새 프레임은 남음"
        );
        assert!(drained.iter().map(Vec::len).sum::<usize>() <= MAX_QUEUED_BYTES);
        assert!(out.push(peer, vec![0; MAX_FRAME + 1]).is_none());
    }
}
