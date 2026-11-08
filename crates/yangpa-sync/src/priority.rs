//! 동기화 요청 순서 큐.
//!
//! 기획서의 요청 순서: 상태 동기화 → 보고 있는 채널의 최근 이벤트 → 우선순위에 따른 첨부 →
//! 스크롤할 때 과거 기록. 이를 네 단계로 둡니다.
//!
//! 1. 상태(상태·키 합의 레인, 공간 수준 키). 모든 멤버에게 필수이고 다른 것의 전제라 항상 먼저.
//! 2. 지금 보고 있는 채널의 최근 이벤트.
//! 3. 피어 설정의 종류별 우선순위 순: 첨부(이미지·영상·파일·이모지·과거 기록 묶음)와 다른 채널의 최근 이벤트
//!    (텍스트). 같은 우선순위면 보고 있는 채널 → 보고 있는 공간 → 나머지. 사용자가 직접 누른 다운로드는 맨 앞.
//! 4. 과거 기록(스크롤로 요청). 보고 있는 채널 먼저.
//!
//! 보고 있는 채널·설정이 바뀌면 순서를 다시 계산합니다. 같은 자리 안에서는 넣은 순서(FIFO)입니다.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use yangpa_core::ids::{ChannelId, SpaceId};

use crate::policy::{DataType, PeerPolicy};

/// 요청할 일의 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Work {
    /// 상태·키 합의 레인과 공간 수준 키.
    State { space: SpaceId },
    /// 채널의 최근 이벤트(따라잡기).
    Recent { space: SpaceId, channel: ChannelId },
    /// 블롭(첨부·이모지·과거 기록 묶음). 공간 이모지는 채널 없음.
    Blob {
        space: SpaceId,
        channel: Option<ChannelId>,
        data_type: DataType,
        size: u64,
    },
    /// 스크롤할 때 불러오는 과거 기록.
    History { space: SpaceId, channel: ChannelId },
}

impl Work {
    pub fn space(&self) -> SpaceId {
        match self {
            Work::State { space }
            | Work::Recent { space, .. }
            | Work::Blob { space, .. }
            | Work::History { space, .. } => *space,
        }
    }

    pub fn channel(&self) -> Option<ChannelId> {
        match self {
            Work::State { .. } => None,
            Work::Recent { channel, .. } | Work::History { channel, .. } => Some(*channel),
            Work::Blob { channel, .. } => *channel,
        }
    }

    /// 피어 설정에서 이 일이 속하는 데이터 종류(상태는 종류 설정과 무관하게 필수라 `None`).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Work::State { .. } => None,
            Work::Recent { .. } | Work::History { .. } => Some(DataType::Text),
            Work::Blob { data_type, .. } => Some(*data_type),
        }
    }
}

/// 큐에 넣지 않은 이유.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// 피어 설정에서 받지 않는 종류.
    NotWanted,
    /// 자동 다운로드 한도보다 큼(사용자가 직접 요청해야 받음).
    TooLarge,
}

/// (단계, 직접 요청이면 0, 우선순위 내림차순, 가까움, 넣은 순서). 직접 요청 여부를 우선순위 값과 따로 두어,
/// 사용자가 어떤 종류의 우선순위를 최대(255)로 올려도 직접 누른 다운로드가 그 단계 맨 앞에 옵니다.
type QKey = (u8, u8, Reverse<u8>, u8, u64);

#[derive(Clone, Debug)]
struct Entry<T> {
    work: Work,
    manual: bool,
    payload: T,
}

/// 요청 순서 큐. `T`는 호출자가 붙이는 요청 내용(ID 목록 등).
#[derive(Clone, Debug)]
pub struct RequestQueue<T> {
    policy: PeerPolicy,
    viewed: Option<(SpaceId, ChannelId)>,
    entries: BTreeMap<QKey, Entry<T>>,
    seq: u64,
}

impl<T> RequestQueue<T> {
    pub fn new(policy: PeerPolicy) -> Self {
        RequestQueue {
            policy,
            viewed: None,
            entries: BTreeMap::new(),
            seq: 0,
        }
    }

    pub fn policy(&self) -> &PeerPolicy {
        &self.policy
    }

    /// 피어 설정을 바꿉니다. 이제 받지 않는 종류의 대기 항목은 버립니다(직접 요청한 것은 유지).
    pub fn set_policy(&mut self, policy: PeerPolicy) {
        self.policy = policy;
        let p = &self.policy;
        self.entries
            .retain(|_, e| e.manual || e.work.data_type().is_none_or(|t| p.wants(t)));
        self.rekey();
    }

    /// 지금 보고 있는 채널(없으면 `None`).
    pub fn set_viewed(&mut self, viewed: Option<(SpaceId, ChannelId)>) {
        if self.viewed != viewed {
            self.viewed = viewed;
            self.rekey();
        }
    }

    pub fn viewed(&self) -> Option<(SpaceId, ChannelId)> {
        self.viewed
    }

    /// 자동 요청을 넣습니다(피어 설정의 종류·자동 다운로드 한도 적용).
    pub fn push(&mut self, work: Work, payload: T) -> Result<(), Skipped> {
        if let Some(t) = work.data_type() {
            if !self.policy.wants(t) {
                return Err(Skipped::NotWanted);
            }
            if let Work::Blob { size, .. } = work
                && !self.policy.auto_download(t, size)
            {
                return Err(Skipped::TooLarge);
            }
        }
        self.insert(work, false, payload);
        Ok(())
    }

    /// 사용자가 직접 요청한 일(자동 다운로드 한도·종류 설정 무시, 같은 단계에서 맨 앞).
    pub fn push_manual(&mut self, work: Work, payload: T) {
        self.insert(work, true, payload);
    }

    fn insert(&mut self, work: Work, manual: bool, payload: T) {
        let seq = self.seq;
        self.seq += 1;
        let key = self.key(&work, manual, seq);
        self.entries.insert(
            key,
            Entry {
                work,
                manual,
                payload,
            },
        );
    }

    fn locality(&self, space: SpaceId, channel: Option<ChannelId>) -> u8 {
        match self.viewed {
            Some((s, c)) if s == space && channel == Some(c) => 0,
            Some((s, _)) if s == space => 1,
            _ => 2,
        }
    }

    fn key(&self, work: &Work, manual: bool, seq: u64) -> QKey {
        let m = u8::from(!manual);
        match *work {
            Work::State { .. } => (0, m, Reverse(0), 0, seq),
            Work::Recent { space, channel } => {
                let loc = self.locality(space, Some(channel));
                if loc == 0 {
                    (1, m, Reverse(0), 0, seq)
                } else {
                    (
                        2,
                        m,
                        Reverse(self.policy.priority(DataType::Text)),
                        loc,
                        seq,
                    )
                }
            }
            Work::Blob {
                space,
                channel,
                data_type,
                ..
            } => (
                2,
                m,
                Reverse(self.policy.priority(data_type)),
                self.locality(space, channel),
                seq,
            ),
            Work::History { space, channel } => {
                (3, m, Reverse(0), self.locality(space, Some(channel)), seq)
            }
        }
    }

    fn rekey(&mut self) {
        let old = std::mem::take(&mut self.entries);
        for ((_, _, _, _, seq), e) in old {
            let key = self.key(&e.work, e.manual, seq);
            self.entries.insert(key, e);
        }
    }

    /// 다음 요청.
    pub fn pop(&mut self) -> Option<(Work, T)> {
        self.entries.pop_first().map(|(_, e)| (e.work, e.payload))
    }

    /// 다음에 꺼낼 일.
    pub fn peek(&self) -> Option<&Work> {
        self.entries.values().next().map(|e| &e.work)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 조건에 맞는 것만 남깁니다(예: 이미 받은 블롭 취소).
    pub fn retain(&mut self, mut keep: impl FnMut(&Work, &T) -> bool) {
        self.entries.retain(|_, e| keep(&e.work, &e.payload));
    }

    /// 공간을 떠나거나 휴면으로 바꿀 때 그 공간의 대기 항목을 버립니다.
    pub fn remove_space(&mut self, space: &SpaceId) {
        self.entries.retain(|_, e| e.work.space() != *space);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: SpaceId = SpaceId([1; 16]);
    const S2: SpaceId = SpaceId([2; 16]);
    const A: ChannelId = ChannelId([10; 16]);
    const B: ChannelId = ChannelId([11; 16]);

    fn blob(space: SpaceId, channel: Option<ChannelId>, t: DataType, size: u64) -> Work {
        Work::Blob {
            space,
            channel,
            data_type: t,
            size,
        }
    }

    fn drain(q: &mut RequestQueue<u32>) -> Vec<u32> {
        std::iter::from_fn(|| q.pop().map(|(_, p)| p)).collect()
    }

    #[test]
    fn plan_order() {
        let mut q = RequestQueue::new(PeerPolicy::default());
        q.set_viewed(Some((S1, A)));
        q.push(
            Work::History {
                space: S1,
                channel: A,
            },
            1,
        )
        .unwrap();
        q.push(blob(S1, Some(B), DataType::File, 10), 2).unwrap();
        q.push(blob(S1, Some(A), DataType::Image, 10), 3).unwrap();
        q.push(
            Work::Recent {
                space: S1,
                channel: B,
            },
            4,
        )
        .unwrap();
        q.push(
            Work::Recent {
                space: S1,
                channel: A,
            },
            5,
        )
        .unwrap();
        q.push(Work::State { space: S2 }, 6).unwrap();
        q.push(
            Work::History {
                space: S1,
                channel: B,
            },
            7,
        )
        .unwrap();
        // 상태 → 보는 채널 최근 → (텍스트 > 이미지 > 파일 기본 우선순위) → 과거 기록(보는 채널 먼저).
        assert_eq!(drain(&mut q), vec![6, 5, 4, 3, 2, 1, 7]);
    }

    #[test]
    fn switching_view_reorders() {
        let mut q = RequestQueue::new(PeerPolicy::default());
        q.set_viewed(Some((S1, A)));
        q.push(
            Work::Recent {
                space: S1,
                channel: A,
            },
            1,
        )
        .unwrap();
        q.push(
            Work::Recent {
                space: S1,
                channel: B,
            },
            2,
        )
        .unwrap();
        assert_eq!(
            q.peek(),
            Some(&Work::Recent {
                space: S1,
                channel: A
            })
        );
        q.set_viewed(Some((S1, B)));
        assert_eq!(drain(&mut q), vec![2, 1]);
    }

    #[test]
    fn peer_priorities_and_locality() {
        let mut policy = PeerPolicy::default();
        policy.priorities.insert(DataType::Video, 255);
        let mut q = RequestQueue::new(policy);
        q.set_viewed(Some((S1, A)));
        q.push(blob(S2, None, DataType::Image, 1), 1).unwrap();
        q.push(blob(S1, Some(B), DataType::Image, 1), 2).unwrap();
        q.push(blob(S1, Some(A), DataType::Image, 1), 3).unwrap();
        q.push(blob(S2, None, DataType::Video, 1), 4).unwrap();
        // 영상 우선순위를 올렸으니 영상 먼저, 이미지는 보는 채널 → 보는 공간 → 다른 공간.
        assert_eq!(drain(&mut q), vec![4, 3, 2, 1]);
    }

    #[test]
    fn policy_filters_and_manual_override() {
        let mut policy = PeerPolicy::default();
        policy.types.remove(&DataType::Video);
        policy.auto_download_max_bytes = 100;
        let mut q = RequestQueue::new(policy.clone());
        assert_eq!(
            q.push(blob(S1, Some(A), DataType::Video, 1), 1),
            Err(Skipped::NotWanted)
        );
        assert_eq!(
            q.push(blob(S1, Some(A), DataType::File, 101), 2),
            Err(Skipped::TooLarge)
        );
        q.push(blob(S1, Some(A), DataType::File, 100), 3).unwrap();
        q.push_manual(blob(S1, Some(A), DataType::Video, 10_000), 4);
        q.push(Work::State { space: S1 }, 5).unwrap();
        assert_eq!(q.len(), 3);
        // 상태는 여전히 먼저, 직접 요청은 첨부 중 맨 앞.
        assert_eq!(drain(&mut q), vec![5, 4, 3]);

        // 종류 우선순위를 최대로 올려도 직접 요청이 앞(같은 단계의 최근 텍스트보다도).
        let mut top = PeerPolicy::default();
        top.priorities.insert(DataType::Image, u8::MAX);
        top.priorities.insert(DataType::Text, u8::MAX);
        let mut q = RequestQueue::new(top);
        q.push(blob(S1, Some(A), DataType::Image, 1), 1).unwrap();
        q.push(
            Work::Recent {
                space: S1,
                channel: B,
            },
            2,
        )
        .unwrap();
        q.push_manual(blob(S2, None, DataType::File, 1), 3);
        q.set_viewed(Some((S1, A)));
        assert_eq!(drain(&mut q), vec![3, 1, 2]);

        // 설정에서 텍스트를 끄면 대기 중인 최근·과거 기록은 버리고 직접 요청은 유지.
        let mut q = RequestQueue::new(PeerPolicy::default());
        q.push(
            Work::Recent {
                space: S1,
                channel: A,
            },
            1,
        )
        .unwrap();
        q.push_manual(
            Work::History {
                space: S1,
                channel: A,
            },
            2,
        );
        let mut no_text = PeerPolicy::default();
        no_text.types.remove(&DataType::Text);
        q.set_policy(no_text);
        assert_eq!(drain(&mut q), vec![2]);
        assert_eq!(
            q.push(
                Work::Recent {
                    space: S1,
                    channel: A
                },
                3
            ),
            Err(Skipped::NotWanted)
        );
        q.push(Work::State { space: S1 }, 4).unwrap();
        assert_eq!(drain(&mut q), vec![4]);
    }

    #[test]
    fn retain_and_remove_space() {
        let mut q = RequestQueue::new(PeerPolicy::default());
        q.push(Work::State { space: S1 }, 1).unwrap();
        q.push(Work::State { space: S2 }, 2).unwrap();
        q.push(
            Work::Recent {
                space: S2,
                channel: A,
            },
            3,
        )
        .unwrap();
        q.remove_space(&S2);
        assert_eq!(q.len(), 1);
        q.retain(|_, p| *p != 1);
        assert!(q.is_empty());
        assert!(q.pop().is_none());
    }
}
