//! 요청·응답으로 주고받은 바이트 수(앱의 전체 올리기·받기 속도 표시). 데몬은 여러 스웜에 같은 값을 넘겨 합칩니다.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 보낸·받은 바이트 누적값. 복제해도 같은 수를 셉니다.
#[derive(Clone, Debug, Default)]
pub struct Traffic(Arc<Counts>);

#[derive(Debug, Default)]
struct Counts {
    sent: AtomicU64,
    received: AtomicU64,
}

impl Traffic {
    /// (보낸, 받은) 누적 바이트.
    pub fn totals(&self) -> (u64, u64) {
        (
            self.0.sent.load(Ordering::Relaxed),
            self.0.received.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn add_sent(&self, n: usize) {
        self.0.sent.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_received(&self, n: usize) {
        self.0.received.fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_counts() {
        let t = Traffic::default();
        let u = t.clone();
        t.add_sent(10);
        u.add_received(3);
        assert_eq!(u.totals(), (10, 3));
    }
}
