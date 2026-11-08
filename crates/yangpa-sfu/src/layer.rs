//! simulcast 층 고르기와 층별 비트율 측정.

use std::time::{Duration, Instant};

use yangpa_core::wire::SfuLayer;

/// 마지막 프레임 뒤 이만큼 조용하면 그 층은 꺼진 것으로 봅니다(보내는 쪽이 층을 끄거나 화면공유를 멈춤).
pub(crate) const LIVE_FOR: Duration = Duration::from_millis(1500);
/// 비트율 측정 창.
const WINDOW: Duration = Duration::from_secs(1);
/// 더 높은 층으로 올릴 때 요구하는 여유(오르내림 반복을 막음).
const UPGRADE_MARGIN_PERCENT: u64 = 115;

/// 받는 쪽에서 본 층 하나의 상태.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerInfo {
    /// 최근에 프레임이 들어옴.
    pub live: bool,
    /// 측정한 비트율(bps). 아직 한 창도 채우지 못했으면 0.
    pub bps: u64,
}

/// 층 하나의 들어오는 비트율을 1초 창으로 잽니다.
#[derive(Clone, Debug, Default)]
pub(crate) struct RateMeter {
    window_start: Option<Instant>,
    bytes: u64,
    bps: u64,
    last: Option<Instant>,
}

impl RateMeter {
    pub(crate) fn record(&mut self, now: Instant, len: usize) {
        let start = *self.window_start.get_or_insert(now);
        self.bytes = self.bytes.saturating_add(len as u64);
        let elapsed = now.saturating_duration_since(start);
        if elapsed >= WINDOW {
            let micros = elapsed.as_micros().max(1) as u64;
            self.bps = self.bytes.saturating_mul(8_000_000) / micros;
            self.window_start = Some(now);
            self.bytes = 0;
        }
        self.last = Some(now);
    }

    pub(crate) fn info(&self, now: Instant) -> LayerInfo {
        let live = self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < LIVE_FOR);
        LayerInfo {
            live,
            bps: if live { self.bps } else { 0 },
        }
    }
}

/// 받는 쪽 하나에 보낼 층을 고릅니다. `layers`는 낮은 층 → 높은 층 순서이고, `budget`은 이 트랙에 쓸 수 있는
/// 대역폭 추정(bps, 모르면 `None`), `current`는 지금 보내는 층입니다. `None`이면 보내지 않습니다.
pub fn choose(
    layers: &[LayerInfo],
    pref: SfuLayer,
    budget: Option<u64>,
    current: Option<usize>,
) -> Option<usize> {
    if pref == SfuLayer::Off {
        return None;
    }
    let live: Vec<usize> = (0..layers.len()).filter(|&i| layers[i].live).collect();
    let Some(&lowest) = live.first() else {
        // 들어오는 층이 하나도 없으면 고르던 층을 그대로 둡니다(다시 들어오면 키프레임부터 이어 보냄).
        return current;
    };
    let highest = *live.last().unwrap_or(&lowest);
    match pref {
        SfuLayer::Off => None,
        SfuLayer::Low => Some(lowest),
        SfuLayer::High => Some(highest),
        SfuLayer::Medium => {
            let middle = (layers.len().saturating_sub(1)) / 2;
            Some(
                live.iter()
                    .rev()
                    .copied()
                    .find(|&i| i <= middle)
                    .unwrap_or(lowest),
            )
        }
        SfuLayer::Auto => {
            let Some(budget) = budget else {
                // 추정이 없으면 지금 층을 지키고, 처음이면 가장 낮은 층부터.
                return Some(current.filter(|c| live.contains(c)).unwrap_or(lowest));
            };
            let mut pick = None;
            for &i in &live {
                let bps = layers[i].bps;
                if bps == 0 {
                    // 아직 재지 못한 층은 건너뜀(재면 다음 평가에서 오름).
                    continue;
                }
                let need = if current.is_some_and(|c| i > c) {
                    bps.saturating_mul(UPGRADE_MARGIN_PERCENT) / 100
                } else {
                    bps
                };
                if need <= budget {
                    pick = Some(i);
                }
            }
            Some(pick.unwrap_or(lowest))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(live: bool, bps: u64) -> LayerInfo {
        LayerInfo { live, bps }
    }

    #[test]
    fn explicit_preferences_pick_live_layers() {
        let three = [l(true, 150_000), l(true, 500_000), l(true, 1_500_000)];
        assert_eq!(choose(&three, SfuLayer::Low, None, None), Some(0));
        assert_eq!(choose(&three, SfuLayer::Medium, None, None), Some(1));
        assert_eq!(choose(&three, SfuLayer::High, None, None), Some(2));
        assert_eq!(choose(&three, SfuLayer::Off, None, Some(2)), None);

        // 높은 층이 꺼지면 남은 층 중 가장 높은 것.
        let no_high = [l(true, 150_000), l(true, 500_000), l(false, 0)];
        assert_eq!(choose(&no_high, SfuLayer::High, None, Some(2)), Some(1));
        // 가운데가 꺼지면 그 아래.
        let no_mid = [l(true, 150_000), l(false, 0), l(true, 1_500_000)];
        assert_eq!(choose(&no_mid, SfuLayer::Medium, None, None), Some(0));
        // 아무것도 안 들어오면 지금 층 유지.
        let none = [l(false, 0), l(false, 0)];
        assert_eq!(choose(&none, SfuLayer::High, None, Some(1)), Some(1));
        // simulcast가 아닌 트랙(층 하나).
        let single = [l(true, 800_000)];
        assert_eq!(choose(&single, SfuLayer::Low, None, None), Some(0));
        assert_eq!(choose(&single, SfuLayer::Auto, Some(10), None), Some(0));
    }

    #[test]
    fn auto_follows_budget_with_upgrade_margin() {
        let three = [l(true, 150_000), l(true, 500_000), l(true, 1_500_000)];
        assert_eq!(choose(&three, SfuLayer::Auto, None, None), Some(0));
        assert_eq!(choose(&three, SfuLayer::Auto, None, Some(1)), Some(1));
        assert_eq!(
            choose(&three, SfuLayer::Auto, Some(3_000_000), None),
            Some(2)
        );
        assert_eq!(
            choose(&three, SfuLayer::Auto, Some(600_000), Some(2)),
            Some(1)
        );
        assert_eq!(
            choose(&three, SfuLayer::Auto, Some(100_000), Some(1)),
            Some(0)
        );
        // 1.5 Mbps 층으로 올리려면 15% 여유가 있어야 함, 이미 그 층이면 그대로.
        assert_eq!(
            choose(&three, SfuLayer::Auto, Some(1_600_000), Some(1)),
            Some(1)
        );
        assert_eq!(
            choose(&three, SfuLayer::Auto, Some(1_600_000), Some(2)),
            Some(2)
        );
        assert_eq!(
            choose(&three, SfuLayer::Auto, Some(1_800_000), Some(1)),
            Some(2)
        );
        // 아직 재지 못한 층은 고르지 않음.
        let unmeasured = [l(true, 150_000), l(true, 0), l(true, 0)];
        assert_eq!(
            choose(&unmeasured, SfuLayer::Auto, Some(5_000_000), None),
            Some(0)
        );
    }

    #[test]
    fn meter_measures_and_expires() {
        let t0 = Instant::now();
        let mut m = RateMeter::default();
        assert!(!m.info(t0).live);
        for i in 0..=30u64 {
            m.record(t0 + Duration::from_millis(i * 1000 / 30), 1250);
        }
        let info = m.info(t0 + Duration::from_secs(1));
        assert!(info.live);
        // 30프레임 × 1250바이트 × 8 ≈ 300 kbps(첫 프레임이 창을 엶).
        assert!((250_000..=350_000).contains(&info.bps), "{}", info.bps);
        assert!(!m.info(t0 + Duration::from_secs(3)).live);
    }
}
