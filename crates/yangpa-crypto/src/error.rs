use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("복호화 실패")]
    Decrypt,
    #[error("잘못된 형식")]
    Malformed,
    #[error("이 기기를 위한 봉인이 없음")]
    NotARecipient,
    #[error("잠긴 메모리 할당 실패")]
    LockedAlloc,
    #[error("잠긴 키 보관소가 가득 참")]
    KeyringFull,
    #[error("핵심 타입 오류: {0}")]
    Core(#[from] yangpa_core::CoreError),
    /// 양자 내성 암호로 바뀌기 전 판이 만든 데이터. 기기 목록·그룹 키·봉인 상자가 예전 암호라 이어 쓸 수 없습니다.
    #[error("양자 내성 암호로 바뀌기 전 판의 데이터라 열 수 없습니다")]
    PreQuantumData,
    #[error("압축 오류: {0}")]
    Compression(String),
    #[error("허용 크기를 넘음: {0}")]
    TooLarge(&'static str),
    #[error("블롭 조각 해시가 매니페스트와 다름")]
    ChunkHash,
    #[error("블롭 길이가 선언과 다름")]
    BlobLength,
    #[error("입출력 오류: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CryptoError>;
