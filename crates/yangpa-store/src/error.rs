use thiserror::Error;

/// 저장소 오류. 메시지에는 내용·키를 넣지 않습니다.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("DB 키가 맞지 않거나 DB 파일이 손상됨")]
    WrongKey,
    #[error("SQLCipher가 아닌 SQLite에 연결됨(암호화되지 않은 빌드)")]
    NotEncrypted,
    #[error("휘발성 채널 데이터는 디스크에 저장할 수 없음")]
    VolatileRejected,
    #[error("잘못된 입력: {0}")]
    InvalidInput(&'static str),
    #[error("찾을 수 없음")]
    NotFound,
    #[error("저장된 데이터가 손상됨: {0}")]
    Corrupt(&'static str),
    #[error("이 버전보다 새 DB 스키마(버전 {found})입니다. 지원 버전: {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error(
        "예전 형식(저장 형식 5판 이전)의 데이터라 이어 쓸 수 없습니다. 기기 목록 형식이 바뀌었으니 데이터를 지우고 새로 시작하세요"
    )]
    IncompatibleData,
    #[error("허용 범위를 넘음: {0}")]
    LimitExceeded(&'static str),
    #[error("SQLite 오류: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("파일 입출력 오류: {0}")]
    Io(#[from] std::io::Error),
    #[error("암호 오류: {0}")]
    Crypto(#[from] yangpa_crypto::CryptoError),
    #[error("직렬화 오류: {0}")]
    Codec(String),
}

impl From<postcard::Error> for StoreError {
    fn from(e: postcard::Error) -> Self {
        StoreError::Codec(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Codec(e.to_string())
    }
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;
