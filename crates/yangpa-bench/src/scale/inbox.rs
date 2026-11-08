//! 가입 키 묶음을 받은편지함 스트림으로 옮긴 효과(docs/performance.md "가입 키 묶음과 받은편지함").
//!
//! 확정 기기는 가입자의 기기들 앞으로 상태 키 전부와 과거 기록 공개 채널의 기록 키를 64개씩 봉인해 보냅니다. 예전에는
//! 이 묶음이 공간 스트림에 실려 멤버 모두가 gossip·따라잡기로 내려받았고, 이제는 가입자의 받은편지함에만 갑니다(그
//! 사용자의 기기, 받아 준 기기, 보관 노드). 실제 봉인(X-Wing)·서명으로 두 형식의 묶음 이벤트를 만들어 크기를 잽니다.

use anyhow::Result;
use serde::Serialize;
use yangpa_core::body::{EventBody, KeyShareBody, Secret32, SharedKey, state_key_id};
use yangpa_core::event::{EVENT_VERSION, EventHeader, KeyRef, Scope, SignedEvent};
use yangpa_core::ids::{EventId, SpaceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_crypto::{DeviceKeys, seal_body};

/// 키 묶음 이벤트 하나에 싣는 키 수(데몬 `committer::KEYS_PER_SHARE`와 같음).
pub const KEYS_PER_SHARE: usize = 64;
/// 가입자 기기 수(봉인 수). 기록 모형의 사용자는 기기 1~2대라 큰 쪽으로 잽니다.
pub const JOINER_DEVICES: usize = 2;
/// 하루 가입 비율(기록 모형 [`super::trace::Churn::new`]의 기본값과 같음).
pub const JOINS_PER_DAY: f64 = 0.01;

/// 멤버 수·묶음 키 수 하나의 결과. 바이트는 모두 전송 크기(postcard, 머리·서명·봉인 포함).
#[derive(Debug, Clone, Serialize)]
pub struct JoinInboxRow {
    pub members: usize,
    /// 묶음에 든 키 수(상태 키 전부 + 과거 기록 공개 채널의 기록 키).
    pub keys: usize,
    /// 묶음 이벤트 수(키 64개씩).
    pub bundle_events: usize,
    /// 가입 기록 하나(모든 멤버가 받음, 전후 같음).
    pub join_wire: usize,
    /// 묶음 전체, 예전 형식(공간 스트림: 모든 멤버가 받음).
    pub bundle_before: u64,
    /// 묶음 전체, 지금 형식(받은편지함, 범위·기준점 포함: 가입자 기기만 받음).
    pub bundle_after: u64,
    /// 가입자가 아닌 멤버 한 명이 가입 한 번에 내려받는 양.
    pub member_per_join_before: u64,
    pub member_per_join_after: u64,
    /// 하루 가입(멤버의 1%) 동안 멤버 한 명이 내려받는 양.
    pub member_per_day_before: u64,
    pub member_per_day_after: u64,
    /// 가입 한 번에 크루 전체(멤버 수 × 한 명, 가입자 몫 포함)가 내려받는 양.
    pub crew_per_join_before: u64,
    pub crew_per_join_after: u64,
}

/// `keys`개 키를 64개씩 나눠 가입자 기기들 앞으로 봉인한 묶음. `inbox`가 참이면 지금 형식(받은편지함, 같은 스트림
/// 부모·기준점 둘), 거짓이면 예전 형식(공간 스트림, 공간 머리 부모 하나).
fn bundle(keys: usize, inbox: bool) -> Result<Vec<SignedEvent>> {
    let space = SpaceId([0x5c; 16]);
    let committer = DeviceKeys::generate();
    let joiner = UserId([0x77; 32]);
    let rcpts: Vec<_> = (0..JOINER_DEVICES)
        .map(|_| {
            let d = DeviceKeys::generate();
            (d.device_id(), d.box_public())
        })
        .collect();
    let all: Vec<SharedKey> = (0..keys)
        .map(|i| {
            let mut b = [0u8; 32];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let key = Secret32(b);
            SharedKey::State {
                id: state_key_id(&space, &key),
                key,
            }
        })
        .collect();
    let mut out: Vec<SignedEvent> = Vec::new();
    for (i, chunk) in all.chunks(KEYS_PER_SHARE).enumerate() {
        let body = EventBody::KeyShare(KeyShareBody {
            keys: chunk.to_vec(),
            for_user: Some(joiner),
        });
        let lamport = 10_000 + i as u64;
        let (parents, scope, state_anchor) = if inbox {
            // 받은편지함은 같은 스트림의 앞 묶음만 부모로, 기준점은 가입 기록과 공간 머리 하나.
            let parents = out.last().map(SignedEvent::id).into_iter().collect();
            (
                parents,
                Some(Scope::Inbox(joiner)),
                vec![EventId([1; 32]), EventId([2; 32])],
            )
        } else {
            (vec![EventId([3; 32])], None, Vec::new())
        };
        let header = EventHeader {
            v: EVENT_VERSION,
            space,
            channel: None,
            kind: body.kind(),
            author: committer.device_id(),
            user: UserId([0x11; 32]),
            parents,
            lamport,
            created_at: Timestamp(1_800_000_000_000 + lamport),
            key: KeyRef::Sealed,
            body_hash: [0; 32],
            body_len: 0,
            scope,
            state_anchor,
        };
        let sealed = seal_body(&rcpts, &header, &body)?;
        out.push(SignedEvent::sign(header, sealed, &committer.signing)?);
    }
    Ok(out)
}

fn wire(evs: &[SignedEvent]) -> Result<u64> {
    let mut n = 0u64;
    for e in evs {
        n += postcard::to_stdvec(e)?.len() as u64;
    }
    Ok(n)
}

/// 멤버 `members`명 크루에서 키 `keys`개짜리 가입 묶음 하나의 전후. `join_wire`는 가입 기록 하나의 크기
/// ([`super::state`]의 `join_wire`).
pub fn join_inbox(members: usize, keys: usize, join_wire: usize) -> Result<JoinInboxRow> {
    let before = bundle(keys, false)?;
    let after = bundle(keys, true)?;
    let bundle_before = wire(&before)?;
    let bundle_after = wire(&after)?;
    let join = join_wire as u64;
    let joins_per_day = (members as f64 * JOINS_PER_DAY).round() as u64;
    let others = members.saturating_sub(1) as u64;
    Ok(JoinInboxRow {
        members,
        keys,
        bundle_events: before.len(),
        join_wire,
        bundle_before,
        bundle_after,
        member_per_join_before: join + bundle_before,
        member_per_join_after: join,
        member_per_day_before: joins_per_day * (join + bundle_before),
        member_per_day_after: joins_per_day * join,
        crew_per_join_before: (others + 1) * (join + bundle_before),
        crew_per_join_after: (others + 1) * join + bundle_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 묶음은 키 64개씩이고, 가입자가 아닌 멤버는 이제 가입 기록만 받습니다.
    #[test]
    fn non_joiners_only_get_the_join_record() {
        let r = join_inbox(1_000, 130, 9_000).unwrap();
        assert_eq!(r.bundle_events, 3);
        assert_eq!(r.member_per_join_after, 9_000);
        assert_eq!(r.member_per_join_before, 9_000 + r.bundle_before);
        // 범위·기준점이 붙어 묶음 자체는 조금 커지지만 이벤트마다 수백 바이트 안.
        assert!(r.bundle_after > r.bundle_before);
        assert!(r.bundle_after - r.bundle_before < 3 * 256);
        assert_eq!(r.member_per_day_after, 10 * 9_000);
    }
}
