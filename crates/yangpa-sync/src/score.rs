//! 피어 점수.
//!
//! 서명·멤버십·권한·데이터 검증·전송 빈도를 어긴 피어를 감점하고, 점수가 임계 아래로 내려가면
//! 그 피어의 gossip을 무시(회색 목록)하거나 연결을 끊습니다. 점수는 반감기로 0을 향해 돌아가므로
//! 일시적인 문제는 시간이 지나면 풀리고, 연결을 끊은 피어도 한동안 지나면 다시 받아 줍니다.
//! 시간은 호출자가 밀리초로 넘깁니다.

use std::collections::BTreeMap;

use crate::policy::TokenBucket;

/// 위반 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Penalty {
    /// 서명·본문 해시 검증 실패.
    BadSignature,
    /// 멤버가 아닌 기기가 공간 데이터를 보내거나 요청함.
    NotMember,
    /// 권한 없는 이벤트(상태 해소·메시지 권한 검사 거부).
    Forbidden,
    /// 조각·묶음 검증 실패, 요청과 맞지 않는 응답.
    BadData,
    /// 해석할 수 없는 메시지.
    Malformed,
    /// 요청 빈도 초과(한 번 넘을 때마다).
    Flood,
    /// 응답 시간 초과.
    Timeout,
}

impl Penalty {
    /// 감점 크기. 의도적 조작일 가능성이 큰 것일수록 큽니다.
    pub const fn weight(self) -> f64 {
        match self {
            Penalty::BadSignature => 50.0,
            Penalty::NotMember => 40.0,
            Penalty::Malformed => 30.0,
            Penalty::BadData => 25.0,
            Penalty::Forbidden => 20.0,
            Penalty::Flood => 5.0,
            Penalty::Timeout => 2.0,
        }
    }
}

/// 점수에 따른 처분.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Ok,
    /// gossip 전달·메시 참여에서 빼고 요청 우선순위를 낮춤.
    Graylist,
    /// 연결을 끊고, 점수가 회복될 때까지 다시 받지 않음.
    Disconnect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreConfig {
    pub graylist_below: f64,
    pub disconnect_below: f64,
    /// 좋은 행동으로 쌓을 수 있는 최대 점수(신뢰를 무한히 쌓아 두고 공격하지 못하게).
    pub max_score: f64,
    pub reward: f64,
    /// 점수가 0을 향해 절반으로 돌아가는 시간.
    pub half_life_ms: u64,
    /// 피어 하나의 초당 요청 수와 순간 허용량.
    pub requests_per_sec: u64,
    pub request_burst: u64,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        ScoreConfig {
            graylist_below: -40.0,
            disconnect_below: -100.0,
            max_score: 20.0,
            reward: 0.5,
            half_life_ms: 10 * 60 * 1000,
            requests_per_sec: 50,
            request_burst: 200,
        }
    }
}

impl ScoreConfig {
    pub fn verdict_for(&self, value: f64) -> Verdict {
        if value <= self.disconnect_below {
            Verdict::Disconnect
        } else if value <= self.graylist_below {
            Verdict::Graylist
        } else {
            Verdict::Ok
        }
    }
}

/// 피어 하나의 점수.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerScore {
    value: f64,
    last_ms: u64,
}

impl PeerScore {
    pub fn new(now_ms: u64) -> Self {
        PeerScore {
            value: 0.0,
            last_ms: now_ms,
        }
    }

    fn decay(&mut self, now_ms: u64, cfg: &ScoreConfig) {
        if now_ms <= self.last_ms {
            return;
        }
        let dt = (now_ms - self.last_ms) as f64;
        self.last_ms = now_ms;
        if cfg.half_life_ms > 0 {
            self.value *= 0.5f64.powf(dt / cfg.half_life_ms as f64);
        }
        if self.value.abs() < 1e-6 {
            self.value = 0.0;
        }
    }

    pub fn value(&mut self, now_ms: u64, cfg: &ScoreConfig) -> f64 {
        self.decay(now_ms, cfg);
        self.value
    }

    pub fn penalize(&mut self, p: Penalty, now_ms: u64, cfg: &ScoreConfig) -> Verdict {
        self.decay(now_ms, cfg);
        self.value -= p.weight();
        cfg.verdict_for(self.value)
    }

    pub fn reward(&mut self, now_ms: u64, cfg: &ScoreConfig) -> Verdict {
        self.decay(now_ms, cfg);
        self.value = (self.value + cfg.reward).min(cfg.max_score);
        cfg.verdict_for(self.value)
    }

    pub fn verdict(&mut self, now_ms: u64, cfg: &ScoreConfig) -> Verdict {
        self.decay(now_ms, cfg);
        cfg.verdict_for(self.value)
    }
}

#[derive(Clone, Debug)]
struct Entry {
    score: PeerScore,
    requests: TokenBucket,
}

/// 여러 피어의 점수판. 피어 식별자 타입은 호출자가 정합니다.
#[derive(Clone, Debug)]
pub struct ScoreBoard<P: Ord + Clone> {
    cfg: ScoreConfig,
    peers: BTreeMap<P, Entry>,
}

impl<P: Ord + Clone> ScoreBoard<P> {
    pub fn new(cfg: ScoreConfig) -> Self {
        ScoreBoard {
            cfg,
            peers: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> &ScoreConfig {
        &self.cfg
    }

    fn entry(&mut self, peer: &P, now_ms: u64) -> &mut Entry {
        let cfg = self.cfg;
        self.peers.entry(peer.clone()).or_insert_with(|| Entry {
            score: PeerScore::new(now_ms),
            requests: TokenBucket::with_burst(cfg.requests_per_sec, cfg.request_burst, now_ms),
        })
    }

    pub fn penalize(&mut self, peer: &P, p: Penalty, now_ms: u64) -> Verdict {
        let cfg = self.cfg;
        self.entry(peer, now_ms).score.penalize(p, now_ms, &cfg)
    }

    pub fn reward(&mut self, peer: &P, now_ms: u64) -> Verdict {
        let cfg = self.cfg;
        self.entry(peer, now_ms).score.reward(now_ms, &cfg)
    }

    /// 들어온 요청 하나를 셉니다. 빈도를 넘으면 [`Penalty::Flood`] 감점 후 처분을 돌려줍니다.
    /// `Verdict::Ok`가 아니어도 호출자는 이 요청을 거절하는 것이 좋습니다(`flooding`으로 구분).
    pub fn record_request(&mut self, peer: &P, now_ms: u64) -> RequestCheck {
        let cfg = self.cfg;
        let e = self.entry(peer, now_ms);
        if e.requests.try_consume(1, now_ms).is_ok() {
            RequestCheck {
                flooding: false,
                verdict: e.score.verdict(now_ms, &cfg),
            }
        } else {
            RequestCheck {
                flooding: true,
                verdict: e.score.penalize(Penalty::Flood, now_ms, &cfg),
            }
        }
    }

    pub fn verdict(&mut self, peer: &P, now_ms: u64) -> Verdict {
        let cfg = self.cfg;
        match self.peers.get_mut(peer) {
            Some(e) => e.score.verdict(now_ms, &cfg),
            None => Verdict::Ok,
        }
    }

    pub fn score(&mut self, peer: &P, now_ms: u64) -> f64 {
        let cfg = self.cfg;
        self.peers
            .get_mut(peer)
            .map_or(0.0, |e| e.score.value(now_ms, &cfg))
    }

    /// 새 연결을 받아도 되는지(끊긴 피어는 점수가 회복될 때까지 거절).
    pub fn allow_connection(&mut self, peer: &P, now_ms: u64) -> bool {
        self.verdict(peer, now_ms) != Verdict::Disconnect
    }

    pub fn forget(&mut self, peer: &P) {
        self.peers.remove(peer);
    }

    /// 점수가 거의 0으로 돌아간 피어 기록을 지워 메모리를 묶습니다.
    pub fn prune(&mut self, now_ms: u64) {
        let cfg = self.cfg;
        self.peers
            .retain(|_, e| e.score.value(now_ms, &cfg).abs() >= 0.5);
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// [`ScoreBoard::record_request`] 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestCheck {
    /// 빈도 한도를 넘었음.
    pub flooding: bool,
    pub verdict: Verdict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn penalties_cross_thresholds() {
        let mut b: ScoreBoard<u8> = ScoreBoard::new(ScoreConfig::default());
        assert_eq!(b.penalize(&1, Penalty::Forbidden, 0), Verdict::Ok);
        assert_eq!(b.penalize(&1, Penalty::Forbidden, 0), Verdict::Graylist);
        // -20 -20 -50 = -90: 아직 회색 목록.
        assert_eq!(b.penalize(&1, Penalty::BadSignature, 0), Verdict::Graylist);
        assert_eq!(b.penalize(&1, Penalty::Timeout, 0), Verdict::Graylist);
        // -92 - 30 = -122: 끊음.
        assert_eq!(b.penalize(&1, Penalty::Malformed, 0), Verdict::Disconnect);
        assert!(!b.allow_connection(&1, 0));
        assert!(b.allow_connection(&2, 0));
        assert_eq!(b.verdict(&2, 0), Verdict::Ok);
    }

    #[test]
    fn decay_restores_peer() {
        let cfg = ScoreConfig::default();
        let mut b: ScoreBoard<u8> = ScoreBoard::new(cfg);
        b.penalize(&1, Penalty::BadSignature, 0);
        b.penalize(&1, Penalty::BadSignature, 0);
        assert_eq!(b.score(&1, 0), -100.0);
        assert!(!b.allow_connection(&1, 0));
        // 반감기 한 번 → -50.
        let s = b.score(&1, cfg.half_life_ms);
        assert!((s + 50.0).abs() < 1e-9, "{s}");
        assert_eq!(b.verdict(&1, cfg.half_life_ms), Verdict::Graylist);
        assert!(b.allow_connection(&1, cfg.half_life_ms));
        assert_eq!(b.verdict(&1, cfg.half_life_ms * 2), Verdict::Ok);
        // 시계가 거꾸로 가도 바뀌지 않음.
        let before = b.score(&1, cfg.half_life_ms * 2);
        assert_eq!(b.score(&1, 0), before);
    }

    #[test]
    fn rewards_are_capped() {
        let cfg = ScoreConfig::default();
        let mut b: ScoreBoard<u8> = ScoreBoard::new(cfg);
        for _ in 0..1000 {
            b.reward(&1, 0);
        }
        assert_eq!(b.score(&1, 0), cfg.max_score);
        // 신뢰를 쌓아 둬도 큰 위반 몇 번이면 끊김.
        b.penalize(&1, Penalty::BadSignature, 0);
        b.penalize(&1, Penalty::BadSignature, 0);
        assert_eq!(b.penalize(&1, Penalty::NotMember, 0), Verdict::Disconnect);
    }

    #[test]
    fn flooding_is_penalized() {
        let cfg = ScoreConfig {
            requests_per_sec: 10,
            request_burst: 10,
            ..ScoreConfig::default()
        };
        let mut b: ScoreBoard<u8> = ScoreBoard::new(cfg);
        for _ in 0..10 {
            assert_eq!(
                b.record_request(&1, 0),
                RequestCheck {
                    flooding: false,
                    verdict: Verdict::Ok
                }
            );
        }
        let r = b.record_request(&1, 0);
        assert!(r.flooding);
        assert_eq!(b.score(&1, 0), -Penalty::Flood.weight());
        // 계속 몰아치면 끊김.
        let mut last = Verdict::Ok;
        for _ in 0..30 {
            last = b.record_request(&1, 0).verdict;
        }
        assert_eq!(last, Verdict::Disconnect);
        // 1초 뒤엔 다시 10개 허용(점수는 아직 낮음).
        assert!(!b.record_request(&1, 1000).flooding);
    }

    #[test]
    fn prune_forgets_recovered_peers() {
        let cfg = ScoreConfig::default();
        let mut b: ScoreBoard<u8> = ScoreBoard::new(cfg);
        b.penalize(&1, Penalty::Timeout, 0);
        b.penalize(&2, Penalty::BadSignature, 0);
        assert_eq!(b.len(), 2);
        b.prune(cfg.half_life_ms * 3);
        // 1은 -2 → -0.25로 회복, 2는 -50 → -6.25로 남음.
        assert_eq!(b.len(), 1);
        b.forget(&2);
        assert!(b.is_empty());
    }
}
