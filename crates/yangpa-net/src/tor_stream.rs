//! 데몬이 libp2p 밖에서 Arti로 여는 앱 스트림(Tor로 도는 기기의 링크 미리보기 등)과 Tor 경유 활동 수.
//!
//! - [`TorStream`]: Arti `DataStream`을 감싼 tokio `AsyncRead`/`AsyncWrite`. 이름은 출구 노드가 풀어 로컬 DNS
//!   조회가 없습니다. 루프백·사설 주소와 `localhost`는 Arti가 거절합니다(출구가 어차피 거절하는 주소).
//! - [`TorIsolation`]: 같은 토큰끼리만 회로를 같이 씁니다. libp2p 연결(토큰 없음)과는 회로를 나누지 않아
//!   출구가 미리보기 조회와 노드 연결을 묶어 보지 못하게 합니다.
//! - [`TorActivity`]: 지금 스웜의 Arti 다이얼·스트림 시도 수(누출 시험이 'Tor 경로를 탔다'를 확인하는 데 씀).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Arti 경유 활동 수(지금 스웜 기준, 재시작하면 0부터).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorActivity {
    /// libp2p 전송이 Arti로 건 다이얼 수(onion·출구 경유 모두, 실패 포함).
    pub dials: u64,
    /// [`crate::NetHandle::tor_connect`]로 연 앱 스트림 시도 수(실패 포함).
    pub streams: u64,
}

/// 회로 격리 단위. 같은 값을 넘긴 스트림끼리만 회로를 같이 씁니다.
#[derive(Clone, Copy, Debug)]
pub struct TorIsolation {
    #[cfg(feature = "tor")]
    pub(crate) token: arti_client::IsolationToken,
}

impl TorIsolation {
    /// 새 격리 단위.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        TorIsolation {
            #[cfg(feature = "tor")]
            token: arti_client::IsolationToken::new(),
        }
    }
}

/// Arti로 연 TCP 스트림.
pub struct TorStream {
    #[cfg(feature = "tor")]
    pub(crate) inner: arti_client::DataStream,
    #[cfg(not(feature = "tor"))]
    pub(crate) never: std::convert::Infallible,
}

impl std::fmt::Debug for TorStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 상대 주소는 내용(링크)이므로 보이지 않습니다.
        f.debug_struct("TorStream").finish_non_exhaustive()
    }
}

impl AsyncRead for TorStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        #[cfg(feature = "tor")]
        {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
        #[cfg(not(feature = "tor"))]
        {
            let _ = (cx, buf);
            match self.never {}
        }
    }
}

impl AsyncWrite for TorStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        #[cfg(feature = "tor")]
        {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }
        #[cfg(not(feature = "tor"))]
        {
            let _ = (cx, buf);
            match self.never {}
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        #[cfg(feature = "tor")]
        {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }
        #[cfg(not(feature = "tor"))]
        {
            let _ = cx;
            match self.never {}
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        #[cfg(feature = "tor")]
        {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
        #[cfg(not(feature = "tor"))]
        {
            let _ = cx;
            match self.never {}
        }
    }
}
