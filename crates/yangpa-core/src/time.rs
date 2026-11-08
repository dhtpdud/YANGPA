use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 유닉스 시각(밀리초). 기기 간 시계는 맞지 않을 수 있어 표시·만료 판단에만 쓰고,
/// 순서 결정에는 논리 시계(lamport)를 씁니다.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn now() -> Self {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        Timestamp(d.as_millis() as u64)
    }

    pub fn as_millis(self) -> u64 {
        self.0
    }

    pub fn saturating_add(self, d: Duration) -> Self {
        Timestamp(self.0.saturating_add(d.as_millis() as u64))
    }

    pub fn is_past(self) -> bool {
        Timestamp::now() >= self
    }
}
