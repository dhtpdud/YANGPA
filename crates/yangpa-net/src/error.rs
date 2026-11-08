use thiserror::Error;

/// 네트워크 계층 오류. 메시지에는 상대 주소·내용을 넣지 않습니다(로그로 새지 않게).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NetError {
    #[error("응답 시간 초과")]
    Timeout,
    #[error("연결 실패: {0}")]
    Dial(String),
    #[error("알려진 주소가 없음")]
    NoAddresses,
    #[error("응답 전에 연결이 끊김")]
    ConnectionClosed,
    #[error("상대가 프로토콜을 지원하지 않음")]
    Unsupported,
    #[error("메시지가 너무 큼: {size}바이트(최대 {max}바이트)")]
    TooLarge { size: usize, max: usize },
    #[error("입출력 오류: {0}")]
    Io(String),
    #[error("찾을 수 없음")]
    NotFound,
    #[error("DHT 오류: {0}")]
    Kad(String),
    #[error("차단된 피어")]
    Banned,
    #[error("gossip 메시지를 받을 피어가 없음")]
    NoPeers,
    #[error("상대 또는 이 노드가 바쁨")]
    Busy,
    #[error("잘못된 입력: {0}")]
    InvalidInput(String),
    #[error("전송 계층 오류: {0}")]
    Transport(String),
    #[error("Tor를 쓸 수 없음: {0}")]
    Tor(String),
    #[error("네트워크 작업이 끝남")]
    Shutdown,
}
