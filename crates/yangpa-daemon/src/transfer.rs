//! 전송 속도: 앱의 전체 올리기·받기 속도와, 이 기기가 내주는 첨부를 받는 사람별 속도.
//!
//! - 전체: 모든 스웜(Tor·직접 연결·파일 회로)의 요청·응답 바이트(`yangpa_net::Traffic`)를 1초마다 읽어
//!   최근 [`WINDOW_MS`] 평균을 냅니다.
//! - 사람별: 첨부 조각을 내줄 때마다 (블롭, 받는 사람)에 더하고, [`IDLE_MS`] 동안 조각이 없으면 뺍니다.
//!
//! 값이 바뀌면 알림 `transfer.rates`를 보냅니다(모두 0이 되면 한 번 보내고 멈춤). 메모리에만 둡니다.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use yangpa_core::ids::{BlobId, ChannelId, SpaceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::{ANONYMOUS_USER, Notification, TransferRates, UploadRate};

use crate::node::Node;

/// 재는 간격.
const TICK: Duration = Duration::from_secs(1);
/// 평균을 내는 창. 조각이 띄엄띄엄 와도 0으로 깜빡이지 않게 몇 초를 봅니다.
const WINDOW_MS: u64 = 3_000;
/// 이만큼 조각을 보내지 않은 받는 사람은 목록에서 뺍니다.
const IDLE_MS: u64 = 5_000;
/// 받는 사람 목록 상한(알림이 커지지 않게, 빠른 것부터).
const MAX_UPLOADS: usize = 64;

#[derive(Default)]
pub(crate) struct TransferState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// (시각, 보낸 누적, 받은 누적) 표본.
    samples: VecDeque<(u64, u64, u64)>,
    uploads: HashMap<(BlobId, UserId), Upload>,
    /// 마지막으로 알린 값.
    last: TransferRates,
}

struct Upload {
    space: SpaceId,
    channel: Option<ChannelId>,
    /// (시각, 바이트).
    sent: VecDeque<(u64, u64)>,
}

fn now_ms() -> u64 {
    Timestamp::now().0
}

pub(crate) fn start(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            if let Some(r) = sample(&node, now_ms()) {
                node.notify.publish(Notification::TransferRates(r));
            }
        }
    });
}

/// 첨부 조각 하나를 `user`에게 내줌.
pub(crate) fn record_upload(
    node: &Node,
    space: &SpaceId,
    channel: Option<&ChannelId>,
    blob: &BlobId,
    user: &UserId,
    bytes: u64,
) {
    let now = now_ms();
    let mut g = node.transfer.inner.lock();
    let u = g.uploads.entry((*blob, *user)).or_insert_with(|| Upload {
        space: *space,
        channel: channel.copied(),
        sent: VecDeque::new(),
    });
    u.sent.push_back((now, bytes));
}

/// 표본을 하나 더하고, 알린 값과 달라졌으면 새 값.
fn sample(node: &Node, now: u64) -> Option<TransferRates> {
    let (sent, received) = node.net.traffic().totals();
    let mut g = node.transfer.inner.lock();
    g.samples.push_back((now, sent, received));
    while g
        .samples
        .front()
        .is_some_and(|(t, _, _)| now.saturating_sub(*t) > WINDOW_MS)
    {
        g.samples.pop_front();
    }
    let (up_bps, down_bps) = match (g.samples.front(), g.samples.back()) {
        (Some(a), Some(b)) if b.0 > a.0 => {
            let ms = b.0 - a.0;
            (
                per_sec(b.1.saturating_sub(a.1), ms),
                per_sec(b.2.saturating_sub(a.2), ms),
            )
        }
        _ => (0, 0),
    };
    g.uploads.retain(|_, u| {
        while u
            .sent
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > WINDOW_MS)
        {
            u.sent.pop_front();
        }
        u.sent
            .back()
            .is_some_and(|(t, _)| now.saturating_sub(*t) <= IDLE_MS)
    });
    let mut uploads: Vec<UploadRate> = g
        .uploads
        .iter()
        .map(|((blob, user), u)| UploadRate {
            space: u.space,
            channel: u.channel,
            blob: *blob,
            user: *user,
            bps: per_sec(u.sent.iter().map(|(_, b)| b).sum(), WINDOW_MS),
        })
        .collect();
    uploads.sort_by(|a, b| b.bps.cmp(&a.bps).then(a.blob.0.cmp(&b.blob.0)));
    uploads.truncate(MAX_UPLOADS);
    let r = TransferRates {
        up_bps,
        down_bps,
        uploads,
    };
    if r == g.last {
        return None;
    }
    g.last = r.clone();
    drop(g);
    Some(view(node, r))
}

fn per_sec(bytes: u64, ms: u64) -> u64 {
    if ms == 0 {
        return 0;
    }
    (u128::from(bytes) * 1000 / u128::from(ms)).min(u128::from(u64::MAX)) as u64
}

/// 익명 채널의 첨부를 받는 사람은 가립니다(그 채널이 지금 익명이면).
fn view(node: &Node, mut r: TransferRates) -> TransferRates {
    for u in &mut r.uploads {
        if let Some(c) = &u.channel
            && node
                .space_state(&u.space)
                .is_some_and(|st| st.is_anonymous(c))
        {
            u.user = ANONYMOUS_USER;
        }
    }
    r
}

/// `transfer.rates`: 마지막으로 잰 값.
pub(crate) fn current(node: &Node) -> TransferRates {
    let r = node.transfer.inner.lock().last.clone();
    view(node, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_sec_rounds_down_and_handles_zero() {
        assert_eq!(per_sec(3_000, 3_000), 1_000);
        assert_eq!(per_sec(10, 0), 0);
        assert_eq!(per_sec(u64::MAX, 1), u64::MAX);
    }
}
