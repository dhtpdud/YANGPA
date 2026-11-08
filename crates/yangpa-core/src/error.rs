use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("잘못된 길이: {expected}바이트가 필요하지만 {got}바이트")]
    InvalidLength { expected: usize, got: usize },
    #[error("잘못된 인코딩")]
    InvalidEncoding,
    #[error("서명 검증 실패")]
    BadSignature,
    #[error("직렬화 실패: {0}")]
    Codec(String),
    #[error("지원하지 않는 버전: {0}")]
    UnsupportedVersion(u8),
    #[error("잘못된 링크: {0}")]
    InvalidLink(String),
    #[error("만료됨")]
    Expired,
    #[error("허용 범위를 넘음: {0}")]
    LimitExceeded(&'static str),
}

impl From<postcard::Error> for CoreError {
    fn from(e: postcard::Error) -> Self {
        CoreError::Codec(e.to_string())
    }
}
