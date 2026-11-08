//! 따라잡기 모형: 오래 꺼져 있던 기기 B가 켜진 기기 A에게서 빠진 이벤트를 받는 데 드는 왕복과 바이트.
//!
//! 데몬의 `sync_service::sync_stream`과 같은 순서로 돌립니다: 스트림마다 `GetHeads` → 모르는 머리부터 부모를
//! 따라 `GetEvents`(최대 8번) → 아직 빠진 부모가 있으면 [`yangpa_sync::reconcile`]의 지문 비교(실제
//! [`Reconciler`]·[`MemorySet`]) → 찾은 ID를 512개씩 `GetEvents`. 요청과 지문·ID 응답의 바이트는 실제
//! `YpRequest`/`YpResponse`를 postcard로 직렬화해 셉니다. 이벤트 본문은 크기만 셉니다.
//!
//! 공간 스트림은 [`super::trace`]의 멤버 변동 기록에서(변동 하나 = 상태 이벤트 + 키 이벤트), 채널 스트림은 하루
//! 메시지 수를 채널마다 지프 분포로 나눠 무작위 시각에 만듭니다. 1초 안에 잇따른 이벤트는 서로 모르는 동시
//! 이벤트(같은 논리 시계, 같은 부모)로 둡니다. 데몬은 요청을 하나씩 기다리므로 왕복 수 × RTT를 걸린 시간으로
//! 어림합니다(Tor 회로 만들기·대역폭은 뺌).

use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use yangpa_core::event::StreamId;
use yangpa_core::ids::{ChannelId, EventId, SpaceId};
use yangpa_core::wire::{MAX_EVENTS_PER_MESSAGE, YpRequest, YpResponse};
use yangpa_sync::reconcile::{
    MemorySet, ReconcileRequest, ReconcileResponse, Reconciler, answer_fingerprints, answer_ids,
    full_range,
};

use super::trace::{self, Churn, DAY_MS, HOUR_MS, Op};

/// 데몬의 `CHASE_ROUNDS`.
const CHASE_ROUNDS: usize = 8;
/// 이 간격 안에 잇따른 이벤트는 앞 이벤트를 모른 채 만들어졌다고 봅니다(전파 지연).
const CONCURRENT_MS: u64 = 1_000;

/// 따라잡기 조건.
#[derive(Debug, Clone, Serialize)]
pub struct CatchupSpec {
    /// 크루 기기 수(공간 스트림의 멤버 변동 기록 크기를 정함).
    pub devices: usize,
    /// B가 꺼져 있던 시간들(밀리초).
    pub offline_ms: Vec<u64>,
    /// 꺼지기 전 B도 가진 기록 길이.
    pub history_ms: u64,
    /// 크루 전체 하루 메시지 수와 채널 수, 채널별 몫의 지프 지수.
    pub messages_per_day: f64,
    pub channels: usize,
    pub zipf: f64,
    /// 이벤트 하나의 전송 크기: 메시지, 가입 기록, 그 밖의 상태 이벤트, 키 이벤트.
    pub message_bytes: u32,
    pub join_bytes: u32,
    pub state_bytes: u32,
    pub key_bytes: u32,
    /// 요청 하나의 왕복 시간.
    pub rtt_ms: f64,
    pub seed: u64,
}

impl CatchupSpec {
    /// 기본값: 하루 메시지 10만 개(채널 100개, 지프 1), 가입 기록 14KB·상태 이벤트 1.5KB·키 이벤트 2KB,
    /// 메시지 1KB, RTT 500ms. 크기는 `scale`이 실제로 잰 값으로 바꿔 넣습니다.
    pub fn new(devices: usize, offline_ms: Vec<u64>) -> Self {
        CatchupSpec {
            devices,
            offline_ms,
            history_ms: DAY_MS,
            messages_per_day: 100_000.0,
            channels: 100,
            zipf: 1.0,
            message_bytes: 1_000,
            join_bytes: 14_000,
            state_bytes: 1_500,
            key_bytes: 2_000,
            rtt_ms: 500.0,
            seed: 5,
        }
    }
}

/// 스트림 하나(또는 여럿의 합)를 따라잡은 결과.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamCatchup {
    /// B가 몰랐던 이벤트 수.
    pub new_events: u64,
    /// 요청 수(= 왕복 수): 머리, 이벤트 가져오기, 지문, ID 목록.
    pub heads: u64,
    pub get_events: u64,
    pub fingerprints: u64,
    pub get_ids: u64,
    /// 부모 따라가기 횟수와 지문 비교까지 갔는지.
    pub chase_rounds: u64,
    pub reconciled: u64,
    /// ID 목록 응답에 실려 온 ID 수.
    pub ids_listed: u64,
    /// 이벤트 본문을 뺀 주고받은 바이트(요청 전부 + 머리·지문·ID 응답).
    pub overhead_up: u64,
    pub overhead_down: u64,
    /// 받은 이벤트 바이트.
    pub event_bytes: u64,
    /// 모든 스트림이 빠짐없이 맞춰졌는지.
    pub complete: bool,
}

impl StreamCatchup {
    pub fn requests(&self) -> u64 {
        self.heads + self.get_events + self.fingerprints + self.get_ids
    }

    pub fn bytes(&self) -> u64 {
        self.overhead_up + self.overhead_down + self.event_bytes
    }

    fn add(&mut self, o: &StreamCatchup) {
        self.new_events += o.new_events;
        self.heads += o.heads;
        self.get_events += o.get_events;
        self.fingerprints += o.fingerprints;
        self.get_ids += o.get_ids;
        self.chase_rounds += o.chase_rounds;
        self.reconciled += o.reconciled;
        self.ids_listed += o.ids_listed;
        self.overhead_up += o.overhead_up;
        self.overhead_down += o.overhead_down;
        self.event_bytes += o.event_bytes;
        self.complete &= o.complete;
    }
}

/// 꺼져 있던 시간 하나의 결과.
#[derive(Debug, Clone, Serialize)]
pub struct CatchupRow {
    pub offline_ms: u64,
    pub space: StreamCatchup,
    /// 메시지가 가장 많은 채널.
    pub busiest: StreamCatchup,
    /// 채널 전체의 합과 새 이벤트가 없던 채널 수(그래도 `GetHeads` 한 번씩).
    pub channels: StreamCatchup,
    pub idle_channels: usize,
    /// 공간 + 채널 전체.
    pub total: StreamCatchup,
    /// 왕복 수 × RTT(초).
    pub seconds: f64,
}

/// 이벤트 하나: 시각, 크기.
struct Ev {
    at: u64,
    lamport: u64,
    id: EventId,
    bytes: u32,
}

/// 한 스트림의 전체 기록(A가 가진 것).
struct Stream {
    id: StreamId,
    events: Vec<Ev>,
    /// 논리 시계 L(1부터)의 이벤트 번호들은 `levels[L - 1]`.
    levels: Vec<Vec<u32>>,
    index: HashMap<EventId, u32>,
}

impl Stream {
    /// (시각, 크기) 목록으로 만듭니다. 앞 이벤트와 1초 안이면 같은 논리 시계(동시)로 둡니다.
    fn new(id: StreamId, salt: u64, mut items: Vec<(u64, u32)>) -> Self {
        items.sort_unstable();
        let mut events = Vec::with_capacity(items.len());
        let mut levels: Vec<Vec<u32>> = Vec::new();
        let mut index = HashMap::with_capacity(items.len());
        let mut prev_at = None;
        for (i, (at, bytes)) in items.into_iter().enumerate() {
            let concurrent = prev_at.is_some_and(|p| at - p < CONCURRENT_MS);
            if !concurrent || levels.is_empty() {
                levels.push(Vec::new());
            }
            prev_at = Some(at);
            let lamport = levels.len() as u64;
            let mut seed = [0u8; 16];
            seed[..8].copy_from_slice(&salt.to_le_bytes());
            seed[8..].copy_from_slice(&(i as u64).to_le_bytes());
            let id = EventId::hash_of(&seed);
            levels.last_mut().expect("방금 넣음").push(i as u32);
            index.insert(id, i as u32);
            events.push(Ev {
                at,
                lamport,
                id,
                bytes,
            });
        }
        Stream {
            id,
            events,
            levels,
            index,
        }
    }

    fn all(&self) -> MemorySet {
        self.events.iter().map(|e| (e.lamport, e.id)).collect()
    }

    fn until(&self, cutoff: u64) -> MemorySet {
        self.events
            .iter()
            .filter(|e| e.at <= cutoff)
            .map(|e| (e.lamport, e.id))
            .collect()
    }

    fn heads(&self) -> Vec<EventId> {
        self.levels.last().map_or_else(Vec::new, |l| {
            l.iter().map(|i| self.events[*i as usize].id).collect()
        })
    }
}

fn wire_req(r: &YpRequest) -> u64 {
    postcard::to_stdvec(r).map_or(0, |v| v.len() as u64)
}

fn wire_resp(r: &YpResponse) -> u64 {
    postcard::to_stdvec(r).map_or(0, |v| v.len() as u64)
}

fn varint_len(n: usize) -> u64 {
    let bits = usize::BITS - n.max(1).leading_zeros();
    bits.div_ceil(7).max(1) as u64
}

const SPACE: SpaceId = SpaceId([0x5c; 16]);

/// `ids`를 512개씩 `GetEvents`로 가져와 `b`에 넣습니다.
fn fetch(s: &Stream, ids: &[EventId], b: &mut MemorySet, out: &mut StreamCatchup) {
    for chunk in ids.chunks(MAX_EVENTS_PER_MESSAGE) {
        out.get_events += 1;
        out.overhead_up += wire_req(&YpRequest::GetEvents {
            space: SPACE,
            ids: chunk.to_vec(),
        });
        // `Events(Vec<SignedEvent>)`: 변형 번호 + 길이 + 이벤트들.
        out.overhead_down += 1 + varint_len(chunk.len());
        for id in chunk {
            let e = &s.events[s.index[id] as usize];
            out.event_bytes += e.bytes as u64;
            if b.insert(e.lamport, e.id) {
                out.new_events += 1;
            }
        }
    }
}

/// `cutoff`까지 가진 B가 A를 따라잡습니다.
fn sync(s: &Stream, cutoff: u64) -> StreamCatchup {
    let a = s.all();
    let mut b = s.until(cutoff);
    let mut out = StreamCatchup {
        complete: true,
        ..Default::default()
    };
    let heads = s.heads();
    out.heads += 1;
    out.overhead_up += wire_req(&YpRequest::GetHeads {
        space: SPACE,
        stream: s.id,
    });
    out.overhead_down += wire_resp(&YpResponse::Heads {
        heads: heads.clone(),
        max_lamport: a.max_lamport(),
    });
    let mut want: Vec<EventId> = heads.into_iter().filter(|h| !b.contains(h)).collect();
    while !want.is_empty() && (out.chase_rounds as usize) < CHASE_ROUNDS {
        out.chase_rounds += 1;
        fetch(s, &want, &mut b, &mut out);
        let mut parents: Vec<u64> = want
            .iter()
            .map(|id| s.events[s.index[id] as usize].lamport)
            .filter(|l| *l > 1)
            .map(|l| l - 1)
            .collect();
        parents.sort_unstable();
        parents.dedup();
        want = parents
            .iter()
            .flat_map(|l| s.levels[*l as usize - 1].iter())
            .map(|i| s.events[*i as usize].id)
            .filter(|id| !b.contains(id))
            .collect();
        want.truncate(MAX_EVENTS_PER_MESSAGE);
    }
    // 부모를 끝까지 따라갔으면(모형에선 한 층의 이벤트가 다음 층 모두의 부모) 빠진 것이 없습니다.
    if !want.is_empty() {
        out.reconciled = 1;
        let mut rec = Reconciler::new(full_range(a.max_lamport().max(b.max_lamport())));
        while let Some((token, req)) = rec.next_request() {
            let ok = match req {
                ReconcileRequest::Fingerprints(ranges) => {
                    out.fingerprints += 1;
                    let fps = answer_fingerprints(&a, &ranges).expect("메모리 집합");
                    out.overhead_up += wire_req(&YpRequest::GetFingerprints {
                        space: SPACE,
                        stream: s.id,
                        ranges,
                    });
                    out.overhead_down += wire_resp(&YpResponse::Fingerprints(fps.clone()));
                    rec.on_response(&b, token, ReconcileResponse::Fingerprints(&fps))
                }
                ReconcileRequest::Ids { range, limit } => {
                    out.get_ids += 1;
                    let ids = answer_ids(&a, range, limit).expect("메모리 집합");
                    out.ids_listed += ids.len() as u64;
                    out.overhead_up += wire_req(&YpRequest::GetIds {
                        space: SPACE,
                        stream: s.id,
                        range,
                        limit,
                    });
                    out.overhead_down += wire_resp(&YpResponse::Ids(ids.clone()));
                    rec.on_response(&b, token, ReconcileResponse::Ids(&ids))
                }
            };
            if ok.is_err() {
                out.complete = false;
                break;
            }
            let ids = rec.take_fetch();
            if !ids.is_empty() {
                fetch(s, &ids, &mut b, &mut out);
            }
        }
        out.complete &= rec.is_complete();
    }
    out.complete &= b.len() == a.len();
    out
}

/// 멤버 변동 기록으로 공간 스트림을 만듭니다(변동 하나 = 상태 이벤트 + 1ms 뒤 키 이벤트).
fn space_stream(spec: &CatchupSpec, days: f64) -> Stream {
    let t = trace::generate(&Churn::new(spec.devices, days, spec.seed));
    let mut items = Vec::with_capacity(t.events.len() * 2);
    for e in &t.events {
        let state = match &e.op {
            Op::Join { .. } => spec.join_bytes,
            Op::Ban { users } => spec.state_bytes + 32 * users.len() as u32,
            Op::PartitionStart { .. } | Op::PartitionEnd => continue,
            _ => spec.state_bytes,
        };
        items.push((e.at_ms, state));
        items.push((e.at_ms + 1, spec.key_bytes));
    }
    Stream::new(StreamId::Space, 0, items)
}

/// 채널 `c`(0부터, 0이 가장 바쁨)의 메시지 스트림.
fn channel_stream(spec: &CatchupSpec, c: usize, span_ms: u64, rng: &mut StdRng) -> Stream {
    let h: f64 = (1..=spec.channels)
        .map(|r| (r as f64).powf(-spec.zipf))
        .sum();
    let share = ((c + 1) as f64).powf(-spec.zipf) / h;
    let n = (spec.messages_per_day * share * span_ms as f64 / DAY_MS as f64).round() as usize;
    let items = (0..n)
        .map(|_| (rng.gen_range(0..span_ms), spec.message_bytes))
        .collect();
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&(c as u64).to_le_bytes());
    Stream::new(StreamId::Channel(ChannelId(id)), c as u64 + 1, items)
}

/// 꺼져 있던 시간마다 공간 스트림과 채널 전체를 따라잡습니다.
pub fn catchup(spec: &CatchupSpec) -> Vec<CatchupRow> {
    let longest = spec.offline_ms.iter().copied().max().unwrap_or(HOUR_MS);
    let span = spec.history_ms + longest;
    let space = space_stream(spec, span as f64 / DAY_MS as f64);
    // 공간 기록의 끝을 "지금"으로 맞춥니다(채널도 같은 길이).
    let now = space.events.last().map_or(span, |e| e.at.max(span));
    let mut rows: Vec<CatchupRow> = spec
        .offline_ms
        .iter()
        .map(|off| {
            let s = sync(&space, now.saturating_sub(*off));
            CatchupRow {
                offline_ms: *off,
                space: s,
                busiest: StreamCatchup::default(),
                channels: StreamCatchup {
                    complete: true,
                    ..Default::default()
                },
                idle_channels: 0,
                total: StreamCatchup::default(),
                seconds: 0.0,
            }
        })
        .collect();
    drop(space);
    // 채널은 하나씩 만들고 버려 메모리를 가장 바쁜 채널 하나 크기로 묶습니다.
    let mut rng = StdRng::seed_from_u64(spec.seed ^ 0xc4a7);
    for c in 0..spec.channels {
        let ch = channel_stream(spec, c, now, &mut rng);
        for row in &mut rows {
            let r = sync(&ch, now.saturating_sub(row.offline_ms));
            if r.new_events == 0 {
                row.idle_channels += 1;
            }
            row.channels.add(&r);
            if c == 0 {
                row.busiest = r;
            }
        }
    }
    for row in &mut rows {
        let mut total = row.space.clone();
        total.add(&row.channels);
        row.seconds = total.requests() as f64 * spec.rtt_ms / 1_000.0;
        row.total = total;
    }
    rows
}

/// "1h", "1d", "1w", "30m"을 밀리초로.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    let ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => HOUR_MS,
        "d" => DAY_MS,
        "w" => 7 * DAY_MS,
        _ => return None,
    };
    Some(n * ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 작은 크루에서 1시간·1일·1주 모두 빠짐없이 맞춰지고, 오래 꺼져 있을수록 더 많이 받는지.
    #[test]
    fn catchup_converges() {
        let mut spec = CatchupSpec::new(
            300,
            ["1h", "1d", "1w"]
                .iter()
                .map(|s| parse_duration(s).unwrap())
                .collect(),
        );
        spec.messages_per_day = 3_000.0;
        spec.channels = 6;
        let rows = catchup(&spec);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(r.total.complete, "{r:?}");
            assert!(r.space.heads == 1 && r.channels.heads == 6, "{r:?}");
            assert!(r.total.new_events > 0, "{r:?}");
            assert!(r.total.requests() >= 7, "{r:?}");
        }
        assert!(rows[2].total.new_events > rows[1].total.new_events);
        assert!(rows[1].total.new_events > rows[0].total.new_events);
        // 하루 넘게 꺼져 있으면 바쁜 채널은 부모 따라가기로 끝나지 않고 지문 비교까지 갑니다.
        assert_eq!(rows[2].busiest.reconciled, 1, "{:?}", rows[2].busiest);
        assert!(rows[2].busiest.get_ids > 0, "{:?}", rows[2].busiest);
        assert_eq!(parse_duration("30m"), Some(30 * 60_000));
        assert_eq!(parse_duration("x"), None);
    }
}
