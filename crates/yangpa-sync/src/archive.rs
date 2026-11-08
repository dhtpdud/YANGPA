//! 보관 전용 노드의 순수 규칙(protocol.md "보관 전용 노드"): 보관 범위, 명단 만들기·받기, 양쪽의 요청 검사.
//!
//! 데몬은 이 함수들로 판단만 하고, 네트워크·저장은 `yangpa-daemon`의 `archive` 모듈이 맡습니다.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use yangpa_core::archive::{
    ArchiveGrant, ArchiveRoster, MAX_ANCHOR_AHEAD, MAX_DELTAS, MAX_MEMBER_DELTAS,
    MAX_ROSTER_MEMBERS, ROSTER_VERSION, RosterChannel, RosterMember, SignedArchiveRoster,
    SignedArchiveRosterDelta, roster_hash,
};
use yangpa_core::event::{Lane, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, ChatTtl, HistoryVisibility, Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::WireErrorCode;

use crate::policy::{BUSY_RETRY_MS, SeedRefusal, SeedRequest, SeederPolicy, TokenBucket};

/// 명단 발행 시각이 이만큼 넘게 미래면 받지 않습니다(같은 `anchor`의 다음 명단을 막지 못하게).
pub const MAX_ROSTER_SKEW_MS: u64 = 10 * 60 * 1000;

/// 이 지정으로 보관하는 채널: 휘발성·카테고리를 뺀 채널 중 비공개가 아닌 것과, 지정에 넣은 비공개 채널.
pub fn archived_channels(state: &SpaceState, grant: &ArchiveGrant) -> BTreeSet<ChannelId> {
    state
        .channels
        .iter()
        .filter(|(c, _)| is_archived(state, grant, c))
        .map(|(c, _)| *c)
        .collect()
}

/// 채널 하나가 보관 범위인지.
pub fn is_archived(state: &SpaceState, grant: &ArchiveGrant, channel: &ChannelId) -> bool {
    let Some(ch) = state.channels.get(channel) else {
        return false;
    };
    // P2P 전용 채널의 내용은 참여 기기끼리만 오가므로 보관 노드에도 주지 않습니다.
    if ch.is_volatile() || ch.kind == ChannelKind::Category || state.is_p2p(channel) {
        return false;
    }
    !ch.is_private() || grant.private_channels.contains(channel)
}

/// 요청한 기기가 이 상태에서 지정된 보관 노드면 그 지정.
pub fn grant_for<'a>(
    state: &'a SpaceState,
    user: &UserId,
    device: &DeviceId,
) -> Option<&'a ArchiveGrant> {
    state
        .archives
        .get(device)
        .map(|e| &e.grant)
        .filter(|g| g.user == *user)
}

/// 명단을 낼 수 있는 사용자: 소유자와 `MANAGE_SPACE`가 있는 멤버.
pub fn authorities(state: &SpaceState) -> Vec<UserId> {
    let mut v: Vec<UserId> = state
        .members
        .keys()
        .filter(|u| {
            **u == state.owner
                || state
                    .base_permissions(u)
                    .contains(Permissions::MANAGE_SPACE)
        })
        .copied()
        .collect();
    v.sort();
    v
}

/// 권한자가 아니어도 멤버를 빼는 명단을 낼 수 있는 사용자: `KICK_MEMBERS`·`BAN_MEMBERS`가 있는 멤버.
pub fn wardens(state: &SpaceState) -> Vec<UserId> {
    let mut v: Vec<UserId> = state
        .members
        .keys()
        .filter(|u| {
            let p = state.base_permissions(u);
            p.contains(Permissions::KICK_MEMBERS) || p.contains(Permissions::BAN_MEMBERS)
        })
        .copied()
        .collect();
    v.sort();
    v
}

/// 명단 재료(상태에 없는 것).
#[derive(Clone, Copy)]
pub struct RosterInput<'a> {
    pub space: SpaceId,
    pub genesis: EventId,
    pub archive: DeviceId,
    /// 서명 기기가 가진 그 공간 공간 스트림 이벤트의 최대 논리 시계(`ArchiveRoster::anchor`).
    pub anchor: u64,
    pub issued_at: Timestamp,
    /// 멤버가 가장 최근에 들어온 논리 시계(처음 멤버면 `None`).
    pub since: &'a dyn Fn(&UserId) -> Option<u64>,
}

/// 지금 상태로 보관 노드에 보낼 명단을 만듭니다.
pub fn build_roster(
    state: &SpaceState,
    grant: &ArchiveGrant,
    inp: RosterInput<'_>,
) -> ArchiveRoster {
    let members: Vec<RosterMember> = state
        .members
        .keys()
        .map(|u| RosterMember {
            user: *u,
            devices: state
                .devices
                .get(u)
                .map(|l| l.active().map(|e| e.device).collect())
                .unwrap_or_default(),
            since: (inp.since)(u),
        })
        .collect();
    let moderators = state
        .members
        .keys()
        .filter(|u| {
            state
                .base_permissions(u)
                .contains(Permissions::MANAGE_MESSAGES)
        })
        .copied()
        .collect();
    let channels = archived_channels(state, grant)
        .into_iter()
        .map(|c| {
            let ch = &state.channels[&c];
            let readers: Vec<UserId> = state
                .members
                .keys()
                .filter(|u| {
                    state
                        .channel_permissions(u, &c)
                        .contains(Permissions::VIEW_CHANNEL)
                })
                .copied()
                .collect();
            RosterChannel {
                channel: c,
                history_public: ch.history == HistoryVisibility::Public,
                readers: (readers.len() != state.members.len()).then_some(readers),
                ttl_secs: match ch.ttl {
                    ChatTtl::Seconds(n) => Some(n),
                    ChatTtl::None | ChatTtl::OnLeave => None,
                },
            }
        })
        .collect();
    ArchiveRoster {
        v: ROSTER_VERSION,
        space: inp.space,
        genesis: inp.genesis,
        archive: inp.archive,
        anchor: inp.anchor,
        issued_at: inp.issued_at,
        owner: state.owner,
        authorities: authorities(state),
        moderators,
        wardens: wardens(state),
        members,
        channels,
    }
}

/// 보관 노드가 공간마다 두는 명단 기록.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedRoster {
    /// 지금 적용하는 명단. 받은 명단 그대로이거나, 소유자의 오래된 명단·멤버를 빼는 명단을 반영해 좁힌 것이라 서명은
    /// 따로 두지 않습니다(받을 때 확인함).
    pub roster: ArchiveRoster,
    /// 지금 명단의 순서(`anchor`)를 정한 통째 명단의 서명 기기.
    pub signer: DeviceId,
    /// 소유자 서명 기기: 첫 명단(지정)에서 고정하고, 이 기기들이 서명한 명단으로만 바꿉니다.
    pub owner_signers: Vec<DeviceId>,
    /// 마지막으로 받은 소유자 명단의 순서(`anchor`, 발행 시각).
    pub owner_anchor: u64,
    pub owner_issued: Timestamp,
    /// 차이의 바탕: 마지막으로 통째로 받은 명단의 [`roster_hash`]. 좁혀 받았으면 `None`(통째 명단이 올 때까지 차이를
    /// 받지 않음).
    #[serde(default)]
    pub base: Option<[u8; 32]>,
    /// 이 바탕 위에서 서명 기기마다 마지막으로 받은 차이(순번, 해시).
    #[serde(default)]
    pub deltas: BTreeMap<DeviceId, (u64, [u8; 32])>,
    /// 이 바탕 위에서 받은 권한자 차이 수([`MAX_DELTAS`]까지).
    #[serde(default)]
    pub delta_count: usize,
    /// 이 바탕 위에서 받은, 명단을 바꾼 그 밖의 멤버 차이 수([`MAX_DELTAS`]까지, 권한자 몫과 따로).
    #[serde(default)]
    pub member_delta_count: usize,
}

impl HostedRoster {
    pub fn space(&self) -> SpaceId {
        self.roster.space
    }
}

/// 명단 받기 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RosterVerdict {
    /// 통째로 받음: 보낸 기기의 명단이 그대로 지금 명단이 됨.
    Accepted(Box<HostedRoster>),
    /// 소유자가 지금 순서를 정한 기기를 권한자에서 뺐지만 그보다 오래된 명단: 소유자 명단을 쓰되 멤버는 지금 명단에도
    /// 있는 것만 남깁니다(빼거나 밴한 멤버를 되살리지 않게). 보낸 쪽은 따라잡은 뒤 다시 보내야 합니다.
    Narrowed(Box<HostedRoster>),
    /// 권한자가 아닌 멤버 기기의 명단에서 허용된 멤버·기기 빼기만 반영함.
    Removed(Box<HostedRoster>),
    /// 반영할 것이 없음(권한자가 아닌 기기가 뺄 수 있는 것이 없음).
    Unchanged,
    /// 지금 명단보다 오래됨(보낸 쪽 잘못은 아님). 따라잡은 뒤 다시 보내야 합니다.
    Stale,
    Refused(&'static str),
}

/// 보관 노드가 가진 그 공간의 논리 시계(명단 순서 검사용).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RosterLocal {
    /// 저장한 이벤트 전체의 최대(없으면 0). `anchor` 부풀림 상한의 기준.
    pub max_lamport: u64,
    /// 공간 스트림의 최대(없으면 0). 권한자가 아닌 기기의 명단은 이 값을 넘는 `anchor`를 쓸 수 없습니다.
    pub space_max: u64,
}

fn key(r: &ArchiveRoster) -> (u64, Timestamp) {
    (r.anchor, r.issued_at)
}

/// 서명·크기·대상 확인(받는 규칙 공통).
fn check_common(
    new: &SignedArchiveRoster,
    me: &DeviceId,
    now: Timestamp,
) -> Result<(), &'static str> {
    new.verify().map_err(|_| "명단 서명·크기 오류")?;
    let r = &new.roster;
    if r.archive != *me {
        return Err("다른 보관 노드의 명단");
    }
    if r.issued_at.0 > now.0.saturating_add(MAX_ROSTER_SKEW_MS) {
        return Err("발행 시각이 미래임");
    }
    if !r.authorities.contains(&r.owner) || r.member(&r.owner).is_none() {
        return Err("소유자가 권한자·멤버가 아님");
    }
    Ok(())
}

/// 지정 때 받은 첫 명단. 서명 기기는 지정 요청을 보낸 기기(`designator`)이고 그 사용자가 명단의 권한자여야 합니다.
///
/// 소유자 서명 기기는 이 명단의 소유자 기기로 고정합니다. 보관 노드는 상태를 읽지 못하므로 지정한 권한자가 적은
/// 소유자를 그대로 믿습니다(protocol.md "보관 전용 노드"의 남는 위험).
pub fn accept_initial(
    new: &SignedArchiveRoster,
    me: &DeviceId,
    designator: (&UserId, &DeviceId),
    now: Timestamp,
) -> Result<HostedRoster, &'static str> {
    check_common(new, me, now)?;
    let r = &new.roster;
    let (user, device) = designator;
    if new.signer != *device || !r.has_device(user, device) || !r.is_authority(user) {
        return Err("지정한 기기가 명단의 권한자 기기가 아님");
    }
    Ok(owner_pinned(r.clone(), new.signer))
}

/// 소유자 서명 기기를 이 명단의 소유자 기기로 고정한 기록.
fn owner_pinned(r: ArchiveRoster, signer: DeviceId) -> HostedRoster {
    HostedRoster {
        base: Some(roster_hash(&r)),
        deltas: BTreeMap::new(),
        delta_count: 0,
        member_delta_count: 0,
        owner_signers: r
            .member(&r.owner)
            .map(|m| m.devices.clone())
            .unwrap_or_default(),
        owner_anchor: r.anchor,
        owner_issued: r.issued_at,
        signer,
        roster: r,
    }
}

/// 지정 뒤에 받은 명단.
///
/// - 소유자 서명 기기: 마지막 소유자 명단보다 새로워야 합니다. `anchor`가 지금 명단 이상이면 통째로 받고(소유권 이전이면
///   새 소유자와 그 기기로 고정을 바꿈), 낮으면 지금 순서를 정한 기기를 그 명단이 권한자에서 뺐을 때만 좁혀서 받습니다
///   ([`RosterVerdict::Narrowed`]). 아니면 오래된 명단입니다(재시작한 소유자 기기가 따라잡기 전에 낸 명단이 새 채널을 빼거나
///   밴한 멤버를 되살리지 못하게).
/// - 그 밖의 권한자 기기(지금 명단 기준): 지금 명단보다 새롭고 소유자가 같아야 합니다.
/// - 권한자가 아닌 멤버 기기: `anchor`가 지금 명단 이상이고 보관 노드가 가진 공간 스트림 최대 이하일 때, 자기 사용자의 기기·
///   자기 자신을 빼거나(기기 폐기·탈퇴) `wardens`면 권한자·소유자가 아닌 멤버를 빼는 것만 반영합니다.
pub fn accept_roster(
    cur: &HostedRoster,
    new: &SignedArchiveRoster,
    me: &DeviceId,
    local: RosterLocal,
    now: Timestamp,
) -> RosterVerdict {
    if let Err(e) = check_common(new, me, now) {
        return RosterVerdict::Refused(e);
    }
    let r = &new.roster;
    let old = &cur.roster;
    if r.space != old.space || r.genesis != old.genesis {
        return RosterVerdict::Refused("다른 공간의 명단");
    }
    if local.max_lamport > 0 && r.anchor > local.max_lamport.saturating_add(MAX_ANCHOR_AHEAD) {
        return RosterVerdict::Refused("명단 순서가 너무 앞섬");
    }
    if cur.owner_signers.contains(&new.signer) {
        if key(r) <= (cur.owner_anchor, cur.owner_issued) {
            return RosterVerdict::Stale;
        }
        if r.anchor >= old.anchor {
            return RosterVerdict::Accepted(Box::new(owner_pinned(r.clone(), new.signer)));
        }
        if r.is_authority_device(&cur.signer) {
            return RosterVerdict::Stale;
        }
        // 소유자 항목은 그 명단 그대로 남으므로 고정도 소유자가 직접 적은 기기입니다.
        let mut narrowed = r.clone();
        narrowed.members = keep_known_members(r, old);
        let mut rec = owner_pinned(narrowed, new.signer);
        // 받은 명단과 내용이 달라 그 해시를 바탕으로 한 차이를 받을 수 없습니다.
        rec.base = None;
        return RosterVerdict::Narrowed(Box::new(rec));
    }
    if old.is_authority_device(&new.signer) {
        if r.owner != old.owner {
            return RosterVerdict::Refused("소유자는 소유자 기기만 바꿀 수 있음");
        }
        if key(r) <= key(old) {
            return RosterVerdict::Stale;
        }
        return RosterVerdict::Accepted(Box::new(HostedRoster {
            roster: r.clone(),
            signer: new.signer,
            base: Some(roster_hash(r)),
            deltas: BTreeMap::new(),
            delta_count: 0,
            member_delta_count: 0,
            ..cur.clone()
        }));
    }
    let Some(user) = old.user_of(&new.signer) else {
        return RosterVerdict::Refused("명단의 멤버 기기가 아님");
    };
    if r.anchor < old.anchor || r.anchor > local.space_max {
        return RosterVerdict::Stale;
    }
    match removals(old, r, &user) {
        Some(mut next) => {
            next.anchor = old.anchor.max(r.anchor);
            next.issued_at = old.issued_at.max(r.issued_at);
            RosterVerdict::Removed(Box::new(HostedRoster {
                roster: next,
                ..cur.clone()
            }))
        }
        None => RosterVerdict::Unchanged,
    }
}

/// 명단 차이 받기 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaVerdict {
    /// 반영함.
    Accepted(Box<HostedRoster>),
    /// 이미 받은 차이(같은 서명 기기의 같거나 앞선 순번).
    Unchanged,
    /// 바탕이 다르거나, 같은 기기의 앞 차이가 없거나, 차이가 너무 많음. 보낸 쪽이 바탕 명단(권한자)이나 빠진 차이를
    /// 다시 보내야 합니다. 보낸 쪽 잘못은 아닙니다.
    Expired(&'static str),
    Refused(&'static str),
}

/// 바탕 명단 위의 차이([`ArchiveRosterDelta`]).
///
/// - 바탕(`base`)이 지금 바탕과 같고, 같은 서명 기기의 앞 차이(`seq - 1`, `prev_hash`)에 이어져야 합니다(첫 차이는
///   `seq = 1`, `prev_hash = base`). 차이는 명단의 순서(`anchor`)를 바꾸지 않습니다.
/// - 권한자 기기: 소유자 항목을 빼고 아무 멤버나 넣고·바꾸고·뺄 수 있습니다(권한자·관리자·채널 목록은 통째 명단으로만).
/// - 그 밖의 멤버 기기: 명단에 없는 사용자를 넣되 입장 시점(`since`)이 지금 명단 순서 이상이어야 하고(받아 준 멤버가
///   가입자를 넣음, 입장 이전 기록을 열지 못하게), 자기 항목을 바꾸거나 자기를 뺄 수 있습니다. `wardens`면 권한자·소유자가
///   아닌 멤버도 뺍니다.
pub fn accept_delta(
    cur: &HostedRoster,
    new: &SignedArchiveRosterDelta,
    me: &DeviceId,
    now: Timestamp,
) -> DeltaVerdict {
    if new.verify().is_err() {
        return DeltaVerdict::Refused("명단 차이 서명·크기 오류");
    }
    let d = &new.delta;
    let old = &cur.roster;
    if d.archive != *me {
        return DeltaVerdict::Refused("다른 보관 노드의 명단 차이");
    }
    if d.space != old.space {
        return DeltaVerdict::Refused("다른 공간의 명단 차이");
    }
    if d.issued_at.0 > now.0.saturating_add(MAX_ROSTER_SKEW_MS) {
        return DeltaVerdict::Refused("발행 시각이 미래임");
    }
    let Some(user) = old.user_of(&new.signer) else {
        return DeltaVerdict::Refused("명단의 멤버 기기가 아님");
    };
    if cur.base != Some(d.base) {
        return DeltaVerdict::Expired("바탕 명단이 다름");
    }
    match cur.deltas.get(&new.signer) {
        Some((seq, _)) if d.seq <= *seq => return DeltaVerdict::Unchanged,
        Some((seq, h)) if d.seq != seq + 1 || d.prev_hash != *h => {
            return DeltaVerdict::Expired("같은 기기의 앞 차이가 없음");
        }
        None if d.seq != 1 || d.prev_hash != d.base => {
            return DeltaVerdict::Expired("같은 기기의 앞 차이가 없음");
        }
        _ => {}
    }
    let authority = old.is_authority(&user);
    // 권한자가 아닌 멤버는 기기마다 몫이 있고, 멤버 차이는 권한자 몫과 따로 셉니다: 한 멤버가 차이를 쏟아 가입자 넣기·
    // 권한자 빼기를 막지 못하게.
    if !authority && d.seq > MAX_MEMBER_DELTAS {
        return DeltaVerdict::Expired("이 기기의 차이가 너무 많음(바탕 명단을 기다리세요)");
    }
    let used = if authority {
        cur.delta_count
    } else {
        cur.member_delta_count
    };
    if used >= MAX_DELTAS {
        return DeltaVerdict::Expired("차이가 너무 많음(바탕 명단을 다시 보내세요)");
    }
    let warden = old.is_warden(&user);
    let mut seen = BTreeSet::new();
    for m in &d.add {
        if !seen.insert(m.user) {
            return DeltaVerdict::Refused("같은 사용자를 두 번 넣음");
        }
        if m.user == old.owner {
            return DeltaVerdict::Refused("소유자 항목은 통째 명단으로만 바꿈");
        }
        let known = old.member(&m.user).is_some();
        let ok = if authority {
            true
        } else if m.user == user {
            // 자기 항목: 기기만 바꾸고 입장 시점은 더 느슨하게 바꾸지 못함.
            old.member(&user)
                .is_some_and(|o| stricter(o.since, m.since) == m.since)
        } else {
            // 권한자·관리자 목록에 남은 사용자(빠진 권한자 등)를 되살려 권한을 주지 못하게.
            !known
                && !old.is_authority(&m.user)
                && !old.moderators.contains(&m.user)
                && !old.is_warden(&m.user)
                && m.since.is_some_and(|s| s >= old.anchor)
        };
        if !ok {
            return DeltaVerdict::Refused("넣을 수 없는 멤버");
        }
    }
    for u in &d.remove {
        let ok = *u != old.owner && (authority || *u == user || (warden && !old.is_authority(u)));
        if !ok {
            return DeltaVerdict::Refused("뺄 수 없는 멤버");
        }
    }
    let mut next = cur.clone();
    let r = &mut next.roster;
    let gone: BTreeSet<UserId> = d.remove.iter().copied().collect();
    r.members.retain(|m| !gone.contains(&m.user));
    for m in &d.add {
        match r.members.iter_mut().find(|x| x.user == m.user) {
            Some(x) => *x = m.clone(),
            None => r.members.push(m.clone()),
        }
    }
    if r.members.len() > MAX_ROSTER_MEMBERS {
        return DeltaVerdict::Refused("명단 멤버 수");
    }
    if !gone.is_empty() {
        r.moderators.retain(|u| !gone.contains(u));
        r.wardens.retain(|u| !gone.contains(u));
        if authority {
            r.authorities.retain(|u| !gone.contains(u));
        }
        for c in &mut r.channels {
            if let Some(readers) = &mut c.readers {
                readers.retain(|u| !gone.contains(u));
            }
        }
    }
    let changed = next.roster != cur.roster;
    next.deltas.insert(new.signer, (d.seq, d.hash()));
    if authority {
        next.delta_count += 1;
    } else if changed {
        next.member_delta_count += 1;
    }
    DeltaVerdict::Accepted(Box::new(next))
}

/// 더 늦게 들어온 쪽(입장 이전 기록 검사가 더 엄격한 쪽). 처음 멤버(`None`)가 가장 느슨합니다.
fn stricter(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// 소유자의 (오래된) 명단 멤버 중 지금 명단에도 있는 것만, 기기도 양쪽에 있는 것만. 소유자 항목은 그 명단 그대로.
fn keep_known_members(r: &ArchiveRoster, old: &ArchiveRoster) -> Vec<RosterMember> {
    r.members
        .iter()
        .filter_map(|m| {
            if m.user == r.owner {
                return Some(m.clone());
            }
            let o = old.member(&m.user)?;
            let devices: Vec<DeviceId> = m
                .devices
                .iter()
                .filter(|d| o.devices.contains(d))
                .copied()
                .collect();
            Some(RosterMember {
                user: m.user,
                devices,
                since: stricter(m.since, o.since),
            })
        })
        .collect()
}

/// 권한자가 아닌 `user`의 명단 `r`에서 반영할 수 있는 빼기만 지금 명단 `old`에 적용합니다. 바뀐 것이 없으면 `None`.
fn removals(old: &ArchiveRoster, r: &ArchiveRoster, user: &UserId) -> Option<ArchiveRoster> {
    let warden = old.is_warden(user);
    let may_remove = |u: &UserId| u == user || (warden && *u != old.owner && !old.is_authority(u));
    let mut next = old.clone();
    let mut gone_users = BTreeSet::new();
    let mut changed = false;
    next.members.retain_mut(|m| {
        if !may_remove(&m.user) {
            return true;
        }
        match r.member(&m.user) {
            None => {
                gone_users.insert(m.user);
                changed = true;
                false
            }
            Some(nm) => {
                let before = m.devices.len();
                m.devices.retain(|d| nm.devices.contains(d));
                changed |= m.devices.len() != before;
                true
            }
        }
    });
    if !changed {
        return None;
    }
    if !gone_users.is_empty() {
        next.moderators.retain(|u| !gone_users.contains(u));
        next.wardens.retain(|u| !gone_users.contains(u));
        for c in &mut next.channels {
            if let Some(readers) = &mut c.readers {
                readers.retain(|u| !gone_users.contains(u));
            }
        }
    }
    Some(next)
}

/// 보관 노드가 남의 요청에 답할지(대역폭 검사 전).
///
/// 1. 요청 기기가 명단의 멤버 기기인가 (`NotMember`)
/// 2. 채널 스트림이면 보관 범위인가(`NotFound`), 요청자가 볼 수 있는가(`Forbidden`)
/// 3. 이벤트가 요청자 입장 이전이고 그 채널의 과거 기록이 비공개인가 (`HistoryPrivate`). 시점을 모르면 이전으로 봄.
///    `event_lamport`를 넘기지 않는 머리·지문·ID 요청은 이 단계를 보지 않습니다(`check_meta`와 같음).
pub fn check_archive_serve(
    roster: &ArchiveRoster,
    user: &UserId,
    device: &DeviceId,
    channel: Option<&ChannelId>,
    event_lamport: Option<Option<u64>>,
) -> Result<(), WireErrorCode> {
    let Some(m) = roster.member(user).filter(|m| m.devices.contains(device)) else {
        return Err(WireErrorCode::NotMember);
    };
    let Some(c) = channel else { return Ok(()) };
    let Some(ch) = roster.channel(c) else {
        return Err(WireErrorCode::NotFound);
    };
    if ch.readers.as_ref().is_some_and(|r| !r.contains(user)) {
        return Err(WireErrorCode::Forbidden);
    }
    if let (Some(at), Some(since)) = (event_lamport, m.since)
        && !ch.history_public
        && at.is_none_or(|l| l < since)
    {
        return Err(WireErrorCode::HistoryPrivate);
    }
    Ok(())
}

/// 보관 노드가 부분 전달 스트림을 내줄지. 공간·채널 스트림이면 `None`(보통 검사, [`check_archive_serve`]).
///
/// 받은편지함 u는 명단에 있는 u의 기기에게만 줍니다. 키 그룹은 보관하지 않으므로 아무에게도 주지 않습니다. 자격이
/// 없으면 빈 스트림과 똑같이 답합니다(멤버 시더와 같음, [`crate::policy::may_read_scoped`]).
pub fn archive_may_read_scoped(
    roster: &ArchiveRoster,
    user: &UserId,
    device: &DeviceId,
    stream: &StreamId,
) -> Option<bool> {
    match stream {
        StreamId::Space | StreamId::Channel(_) => None,
        StreamId::Inbox(u) => Some(u == user && roster.has_device(user, device)),
        StreamId::KeyGroup(_) => Some(false),
    }
}

/// 멤버(시더)가 보관 노드의 요청에 답할지. `requester`가 지정된 보관 노드가 아니면 `None`(보통 검사로).
///
/// 상태·키 합의·공간 키 레인은 주고, 채널 데이터는 보관 범위의 채널만(`Forbidden`) 줍니다.
/// 입장 이전 기록 검사는 하지 않고(보관 노드가 요청자마다 대신 함), 시더 범위·대역폭은 멤버에게와 같습니다.
/// 이모지처럼 채널 없는 공간 데이터는 주지 않습니다.
pub fn check_archive_seed(
    policy: &SeederPolicy,
    req: &SeedRequest<'_>,
    bucket: &mut TokenBucket,
) -> Option<Result<(), SeedRefusal>> {
    let grant = grant_for(req.state, &req.requester, &req.requester_device)?;
    Some(archive_seed(policy, req, grant, bucket))
}

fn archive_seed(
    policy: &SeederPolicy,
    req: &SeedRequest<'_>,
    grant: &ArchiveGrant,
    bucket: &mut TokenBucket,
) -> Result<(), SeedRefusal> {
    if req.state.is_member(&req.requester) {
        return Err(SeedRefusal::Denied(WireErrorCode::NotMember));
    }
    let required = matches!(req.lane, Lane::State | Lane::KeyAgreement)
        || (req.lane == Lane::Key && req.channel.is_none());
    if required {
        return Ok(());
    }
    let Some(c) = &req.channel else {
        return Err(SeedRefusal::Denied(WireErrorCode::Forbidden));
    };
    if !is_archived(req.state, grant, c) {
        return Err(SeedRefusal::Denied(WireErrorCode::Forbidden));
    }
    if !policy.seeds(
        req.data_type,
        Some(c),
        req.event.as_ref(),
        req.channel_opened,
    ) {
        return Err(SeedRefusal::Denied(WireErrorCode::NotSeeded));
    }
    if req.in_flight >= policy.max_concurrent.max(1) {
        return Err(SeedRefusal::Busy {
            retry_after_ms: BUSY_RETRY_MS,
        });
    }
    bucket.configure(policy.upload_limit_bps, req.now_ms);
    bucket
        .try_consume(req.bytes, req.now_ms)
        .map_err(|retry_after_ms| SeedRefusal::Busy { retry_after_ms })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::policy::DataType;
    use yangpa_core::archive::ArchiveEntry;
    use yangpa_core::device::{DeviceEntry, DeviceList, MemberDevices};
    use yangpa_core::ids::RoleId;
    use yangpa_core::links::ContactInfo;
    use yangpa_core::model::{
        ChannelSettings, ChannelTransport, Member, OverwriteTarget, PermissionOverwrite, Role,
        SecurityLevel, SpaceKind, SpaceSettings,
    };
    use yangpa_core::sign::SigningKeypair;

    struct Fx {
        state: SpaceState,
        owner: UserId,
        owner_key: SigningKeypair,
        admin: UserId,
        admin_key: SigningKeypair,
        member: UserId,
        member_key: SigningKeypair,
        member_dev: DeviceId,
        warden: UserId,
        warden_key: SigningKeypair,
        grant: ArchiveGrant,
        archive_key: SigningKeypair,
        public_ch: ChannelId,
        secret_ch: ChannelId,
        listed_secret: ChannelId,
        volatile_ch: ChannelId,
    }

    fn list(user: UserId, devices: &[DeviceId]) -> MemberDevices {
        MemberDevices::from_list(&DeviceList {
            user,
            version: 1,
            devices: devices
                .iter()
                .map(|d| DeviceEntry {
                    device: *d,
                    box_key: yangpa_core::device::BoxPublicKey::filled(0),
                    sign_keys: yangpa_core::device::DeviceSignKeys::filled(0),
                    name: "기기".into(),
                    added_at: Timestamp(0),
                    revoked_at: None,
                })
                .collect(),
            otp: None,
        })
    }

    fn fx() -> Fx {
        let owner = UserId([1; 32]);
        let admin = UserId([2; 32]);
        let member = UserId([3; 32]);
        let warden = UserId([4; 32]);
        let owner_key = SigningKeypair::generate();
        let admin_key = SigningKeypair::generate();
        let member_key = SigningKeypair::generate();
        let warden_key = SigningKeypair::generate();
        let member_dev = DeviceId(member_key.public());
        let admin_role = RoleId([5; 16]);
        let warden_role = RoleId([6; 16]);
        let mut members = BTreeMap::new();
        for u in [owner, admin, member, warden] {
            let roles = if u == admin {
                vec![admin_role]
            } else if u == warden {
                vec![warden_role]
            } else {
                vec![]
            };
            members.insert(
                u,
                Member {
                    user: u,
                    roles,
                    nickname: None,
                    automation: false,
                },
            );
        }
        let mut roles = BTreeMap::new();
        roles.insert(
            RoleId::EVERYONE,
            Role::everyone(Permissions::DEFAULT_EVERYONE),
        );
        let mut r = Role::everyone(Permissions::MANAGE_SPACE);
        r.id = admin_role;
        r.name = "관리".into();
        r.rank = 1;
        roles.insert(admin_role, r);
        let mut w = Role::everyone(Permissions::BAN_MEMBERS);
        w.id = warden_role;
        w.name = "운영".into();
        w.rank = 1;
        roles.insert(warden_role, w);
        let deny_all = vec![PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        }];
        let public_ch = ChannelId([1; 16]);
        let secret_ch = ChannelId([2; 16]);
        let listed_secret = ChannelId([3; 16]);
        let volatile_ch = ChannelId([4; 16]);
        let category = ChannelId([5; 16]);
        let mut secret = ChannelSettings::text("비밀");
        secret.overwrites = deny_all.clone();
        let mut listed = ChannelSettings::text("운영");
        listed.overwrites = deny_all;
        let mut vol = ChannelSettings::text("휘발");
        vol.security = SecurityLevel::Volatile;
        let mut cat = ChannelSettings::text("묶음");
        cat.kind = ChannelKind::Category;
        let mut channels = BTreeMap::new();
        let mut public = ChannelSettings::text("일반");
        public.history = HistoryVisibility::Public;
        channels.insert(public_ch, public);
        channels.insert(secret_ch, secret);
        channels.insert(listed_secret, listed);
        channels.insert(volatile_ch, vol);
        channels.insert(category, cat);
        let mut devices = BTreeMap::new();
        devices.insert(owner, list(owner, &[DeviceId(owner_key.public())]));
        devices.insert(admin, list(admin, &[DeviceId(admin_key.public())]));
        devices.insert(member, list(member, &[member_dev, DeviceId([7; 32])]));
        devices.insert(warden, list(warden, &[DeviceId(warden_key.public())]));
        let archive_key = SigningKeypair::generate();
        let grant = ArchiveGrant {
            device: DeviceId(archive_key.public()),
            user: UserId([9; 32]),
            name: "보관".into(),
            contact: ContactInfo::default(),
            private_channels: vec![listed_secret],
        };
        let mut archives = BTreeMap::new();
        archives.insert(
            grant.device,
            ArchiveEntry {
                grant: grant.clone(),
                granted_by: owner,
                granted_at: Timestamp(1),
            },
        );
        let state = SpaceState {
            kind: SpaceKind::Crew,
            owner,
            committer: DeviceId(owner_key.public()),
            settings: SpaceSettings {
                name: "크루".into(),
                description: String::new(),
                icon: None,
                icon_key: None,
                allow_external_emoji: false,
                strict_rotation: false,
            },
            members: members.into(),
            roles,
            channels: channels.into(),
            devices: devices.into(),
            invites: Default::default(),
            emojis: BTreeMap::new(),
            pending_crew_conversion: None,
            bans: Default::default(),
            archives,
            channel_transport: BTreeMap::new(),
            role_display: Default::default(),
            anonymous_channels: Default::default(),
            crew_servers: Default::default(),
            forum_featured: Default::default(),
            attach_limits: Default::default(),
            volatile_caps: Default::default(),
            edit_rules: Default::default(),
            emoji_limits: Default::default(),
            emoji_by: Default::default(),
            disbanded: false,
        };
        Fx {
            state,
            owner,
            owner_key,
            admin,
            admin_key,
            member,
            member_key,
            member_dev,
            warden,
            warden_key,
            grant,
            archive_key,
            public_ch,
            secret_ch,
            listed_secret,
            volatile_ch,
        }
    }

    fn roster(f: &Fx, state: &SpaceState, anchor: u64, at: u64) -> ArchiveRoster {
        let member = f.member;
        build_roster(
            state,
            &f.grant,
            RosterInput {
                space: SpaceId([8; 16]),
                genesis: EventId([8; 32]),
                archive: f.grant.device,
                anchor,
                issued_at: Timestamp(at),
                since: &move |u| (*u == member).then_some(50),
            },
        )
    }

    #[test]
    fn archived_channel_scope() {
        let f = fx();
        let set = archived_channels(&f.state, &f.grant);
        assert!(set.contains(&f.public_ch));
        assert!(set.contains(&f.listed_secret));
        assert!(
            !set.contains(&f.secret_ch),
            "목록에 없는 비공개 채널은 보관하지 않음"
        );
        assert!(
            !set.contains(&f.volatile_ch),
            "휘발성 채널은 어떤 경우에도 보관하지 않음"
        );
        assert_eq!(set.len(), 2, "카테고리도 빠짐");

        // P2P 전용 채널은 공개 채널이든 지정에 넣은 비공개 채널이든 보관하지 않습니다.
        let mut state = f.state.clone();
        for c in [f.public_ch, f.listed_secret] {
            state.channel_transport.insert(c, ChannelTransport::P2p);
            assert!(!is_archived(&state, &f.grant, &c));
        }
        assert!(archived_channels(&state, &f.grant).is_empty());
    }

    #[test]
    fn roster_contents() {
        let f = fx();
        let r = roster(&f, &f.state, 10, 1);
        assert_eq!(r.authorities, {
            let mut v = vec![f.owner, f.admin];
            v.sort();
            v
        });
        assert!(r.has_device(&f.member, &f.member_dev));
        assert_eq!(r.member(&f.member).unwrap().since, Some(50));
        let listed = r.channel(&f.listed_secret).unwrap();
        // 비공개 채널은 소유자(모든 권한)만 볼 수 있음.
        assert_eq!(listed.readers.as_deref(), Some(&[f.owner][..]));
        let public = r.channel(&f.public_ch).unwrap();
        assert!(public.readers.is_none() && public.history_public);
        assert!(r.channel(&f.secret_ch).is_none());
    }

    #[test]
    fn serve_checks() {
        let f = fx();
        let r = roster(&f, &f.state, 10, 1);
        let ok = check_archive_serve(&r, &f.member, &f.member_dev, None, None);
        assert_eq!(ok, Ok(()));
        let stranger = check_archive_serve(&r, &UserId([42; 32]), &f.member_dev, None, None);
        assert_eq!(stranger, Err(WireErrorCode::NotMember));
        let wrong_dev = check_archive_serve(&r, &f.member, &DeviceId([1; 32]), None, None);
        assert_eq!(wrong_dev, Err(WireErrorCode::NotMember));
        let hidden = check_archive_serve(&r, &f.member, &f.member_dev, Some(&f.secret_ch), None);
        assert_eq!(hidden, Err(WireErrorCode::NotFound));
        let forbidden =
            check_archive_serve(&r, &f.member, &f.member_dev, Some(&f.listed_secret), None);
        assert_eq!(forbidden, Err(WireErrorCode::Forbidden));
        // 공개 기록 채널: 입장 이전도 줌.
        let public_old = check_archive_serve(
            &r,
            &f.member,
            &f.member_dev,
            Some(&f.public_ch),
            Some(Some(10)),
        );
        assert_eq!(public_old, Ok(()));

        // 기록 비공개로 바꾸면 입장(50) 이전은 거절, 이후는 줌, 시점 모름은 거절.
        let mut st = f.state.clone();
        st.channels.get_mut(&f.public_ch).unwrap().history = HistoryVisibility::Private;
        let r = roster(&f, &st, 10, 1);
        let ch = Some(&f.public_ch);
        assert_eq!(
            check_archive_serve(&r, &f.member, &f.member_dev, ch, Some(Some(10))),
            Err(WireErrorCode::HistoryPrivate)
        );
        assert_eq!(
            check_archive_serve(&r, &f.member, &f.member_dev, ch, Some(Some(60))),
            Ok(())
        );
        assert_eq!(
            check_archive_serve(&r, &f.member, &f.member_dev, ch, Some(None)),
            Err(WireErrorCode::HistoryPrivate)
        );
        // 처음 멤버(since 없음)는 모두 받음.
        assert_eq!(
            check_archive_serve(
                &r,
                &f.owner,
                &DeviceId(f.owner_key.public()),
                ch,
                Some(Some(1))
            ),
            Ok(())
        );
    }

    #[test]
    fn scoped_serve_checks() {
        let f = fx();
        let r = roster(&f, &f.state, 10, 1);
        let read = |u: &UserId, d: &DeviceId, s: StreamId| archive_may_read_scoped(&r, u, d, &s);
        assert_eq!(read(&f.member, &f.member_dev, StreamId::Space), None);
        // 받은편지함은 명단에 있는 그 사용자의 기기에게만.
        let inbox = StreamId::Inbox(f.member);
        assert_eq!(read(&f.member, &f.member_dev, inbox), Some(true));
        assert_eq!(read(&f.member, &DeviceId([1; 32]), inbox), Some(false));
        let owner_dev = DeviceId(f.owner_key.public());
        assert_eq!(read(&f.owner, &owner_dev, inbox), Some(false));
        assert_eq!(
            read(
                &UserId([42; 32]),
                &f.member_dev,
                StreamId::Inbox(UserId([42; 32]))
            ),
            Some(false)
        );
        // 키 그룹은 보관하지 않음.
        let g = yangpa_core::ids::KeyGroupId::private(&r.space, &[]);
        assert_eq!(
            read(&f.owner, &owner_dev, StreamId::KeyGroup(g)),
            Some(false)
        );
    }

    fn local(space_max: u64) -> RosterLocal {
        RosterLocal {
            max_lamport: space_max,
            space_max,
        }
    }

    fn accepted(v: RosterVerdict) -> HostedRoster {
        match v {
            RosterVerdict::Accepted(h) => *h,
            other => panic!("통째로 받아야 함: {other:?}"),
        }
    }

    #[test]
    fn roster_acceptance() {
        let f = fx();
        let me = f.grant.device;
        let now = Timestamp(1_000);
        let admin_dev = DeviceId(f.admin_key.public());
        let owner_dev = DeviceId(f.owner_key.public());
        let none = RosterLocal::default();

        // 지정: 관리자 기기가 서명한 첫 명단.
        let first = roster(&f, &f.state, 10, 1).sign(&f.admin_key).unwrap();
        assert!(accept_initial(&first, &me, (&f.member, &f.member_dev), now).is_err());
        assert!(
            accept_initial(&first, &DeviceId([0; 32]), (&f.admin, &admin_dev), now).is_err(),
            "다른 보관 노드의 명단"
        );
        let hosted = accept_initial(&first, &me, (&f.admin, &admin_dev), now).unwrap();
        assert_eq!(hosted.owner_signers, vec![owner_dev]);
        assert_eq!(hosted.signer, admin_dev);

        // 관리자의 다음 명단: 더 새로워야 함.
        let same = roster(&f, &f.state, 10, 1).sign(&f.admin_key).unwrap();
        assert_eq!(
            accept_roster(&hosted, &same, &me, none, now),
            RosterVerdict::Stale
        );
        let newer = roster(&f, &f.state, 10, 2).sign(&f.admin_key).unwrap();
        let h2 = accepted(accept_roster(&hosted, &newer, &me, none, now));
        assert_eq!(h2.owner_signers, hosted.owner_signers);

        // 명단에 없는 기기는 명단을 낼 수 없음.
        let stranger = SigningKeypair::generate();
        let by_stranger = roster(&f, &f.state, 20, 3).sign(&stranger).unwrap();
        assert!(matches!(
            accept_roster(&h2, &by_stranger, &me, none, now),
            RosterVerdict::Refused(_)
        ));

        // 관리자가 소유자를 바꾸려 하면 거부.
        let mut st = f.state.clone();
        st.owner = f.admin;
        let coup = roster(&f, &st, 30, 4).sign(&f.admin_key).unwrap();
        assert!(matches!(
            accept_roster(&h2, &coup, &me, none, now),
            RosterVerdict::Refused(_)
        ));

        // 관리자 강등: 소유자 명단은 순서를 정한 관리자 명단보다 낮아도 그 관리자를 뺐으면 (좁혀서) 이김.
        let mut demoted = f.state.clone();
        Arc::make_mut(demoted.members.get_mut(&f.admin).unwrap())
            .roles
            .clear();
        let high = roster(&f, &f.state, 500, 5).sign(&f.admin_key).unwrap();
        let h3 = accepted(accept_roster(&h2, &high, &me, none, now));
        let by_owner = roster(&f, &demoted, 100, 6).sign(&f.owner_key).unwrap();
        let RosterVerdict::Narrowed(h4) = accept_roster(&h3, &by_owner, &me, none, now) else {
            panic!("강등한 소유자 명단은 좁혀서 받아야 함");
        };
        assert!(!h4.roster.is_authority(&f.admin));
        assert_eq!(
            h4.roster.anchor, 100,
            "부풀렸을 수 있는 순서를 소유자 명단으로 되돌림"
        );
        assert_eq!(h4.signer, owner_dev);
        // 강등된 관리자는 이제 일반 멤버라 통째 명단을 낼 수 없음(가진 공간 스트림보다 앞선 순서는 오래된 것과 같이 다룸).
        let late = roster(&f, &f.state, 900, 7).sign(&f.admin_key).unwrap();
        assert_eq!(
            accept_roster(&h4, &late, &me, none, now),
            RosterVerdict::Stale
        );
        // 같은 소유자 명단 다시 보내기는 새롭지 않음.
        assert_eq!(
            accept_roster(&h4, &by_owner, &me, none, now),
            RosterVerdict::Stale
        );

        // 소유권 이전: 소유자 기기가 새 소유자로 낸 명단을 받으면 고정이 새 소유자 기기로 바뀜.
        let mut moved = f.state.clone();
        moved.owner = f.admin;
        let transfer = roster(&f, &moved, 200, 8).sign(&f.owner_key).unwrap();
        let h5 = accepted(accept_roster(&h4, &transfer, &me, none, now));
        assert_eq!(h5.owner_signers, vec![admin_dev]);

        // 순서 부풀림·미래 발행 시각은 거부.
        let far = roster(&f, &f.state, 10 + MAX_ANCHOR_AHEAD + 1, 9)
            .sign(&f.owner_key)
            .unwrap();
        assert!(matches!(
            accept_roster(&h4, &far, &me, local(10), now),
            RosterVerdict::Refused(_)
        ));
        let future = roster(&f, &f.state, 300, now.0 + MAX_ROSTER_SKEW_MS + 1)
            .sign(&f.owner_key)
            .unwrap();
        assert!(matches!(
            accept_roster(&h4, &future, &me, none, now),
            RosterVerdict::Refused(_)
        ));
    }

    fn delta(
        f: &Fx,
        base: [u8; 32],
        seq: u64,
        prev: [u8; 32],
        add: Vec<RosterMember>,
        remove: Vec<UserId>,
    ) -> yangpa_core::archive::ArchiveRosterDelta {
        yangpa_core::archive::ArchiveRosterDelta {
            v: ROSTER_VERSION,
            space: SpaceId([8; 16]),
            archive: f.grant.device,
            base,
            seq,
            prev_hash: prev,
            anchor: 10,
            issued_at: Timestamp(5),
            add,
            remove,
        }
    }

    fn delta_ok(v: DeltaVerdict) -> HostedRoster {
        match v {
            DeltaVerdict::Accepted(h) => *h,
            other => panic!("차이를 받아야 함: {other:?}"),
        }
    }

    /// 바탕 명단 위의 차이: 사슬 잇기, 권한, 바탕이 바뀌면 만료.
    #[test]
    fn roster_delta_acceptance() {
        let f = fx();
        let me = f.grant.device;
        let now = Timestamp(1_000);
        let admin_dev = DeviceId(f.admin_key.public());
        let first = roster(&f, &f.state, 10, 1).sign(&f.admin_key).unwrap();
        let h = accept_initial(&first, &me, (&f.admin, &admin_dev), now).unwrap();
        let base = roster_hash(&first.roster);
        assert_eq!(h.base, Some(base));

        // 일반 멤버(받아 준 사람)가 새 가입자를 넣음: 입장 시점이 명단 순서 이상이어야 함.
        let joiner = UserId([20; 32]);
        let jd = DeviceId([21; 32]);
        let add = |since| RosterMember {
            user: joiner,
            devices: vec![jd],
            since,
        };
        let d1 = delta(&f, base, 1, base, vec![add(Some(11))], vec![]);
        let h1 = delta_ok(accept_delta(
            &h,
            &d1.clone().sign(&f.member_key).unwrap(),
            &me,
            now,
        ));
        assert!(h1.roster.has_device(&joiner, &jd));
        assert_eq!(
            h1.roster.anchor, h.roster.anchor,
            "차이는 순서를 바꾸지 않음"
        );
        // 같은 차이 다시: 그대로.
        assert_eq!(
            accept_delta(&h1, &d1.clone().sign(&f.member_key).unwrap(), &me, now),
            DeltaVerdict::Unchanged
        );
        // 입장 이전 기록을 열려는 넣기(`since` 없음·명단 순서 이전)는 거부.
        for since in [None, Some(3)] {
            let bad = delta(&f, base, 1, base, vec![add(since)], vec![]);
            assert!(matches!(
                accept_delta(&h, &bad.sign(&f.member_key).unwrap(), &me, now),
                DeltaVerdict::Refused(_)
            ));
        }
        // 일반 멤버는 남을 빼거나 권한자 목록의 사용자를 되살리지 못함.
        let kick = delta(&f, base, 2, d1.hash(), vec![], vec![f.warden]);
        assert!(matches!(
            accept_delta(&h1, &kick.sign(&f.member_key).unwrap(), &me, now),
            DeltaVerdict::Refused(_)
        ));
        // 사슬이 끊긴 차이(앞 차이 없음)·다른 바탕은 만료.
        let gap = delta(&f, base, 3, [0; 32], vec![], vec![f.member]);
        assert!(matches!(
            accept_delta(&h1, &gap.sign(&f.member_key).unwrap(), &me, now),
            DeltaVerdict::Expired(_)
        ));
        let other = delta(&f, [7; 32], 1, [7; 32], vec![], vec![f.member]);
        assert!(matches!(
            accept_delta(&h1, &other.sign(&f.admin_key).unwrap(), &me, now),
            DeltaVerdict::Expired(_)
        ));
        // 권한자는 아무나 빼지만 소유자는 못 뺌. 다른 기기의 사슬은 따로.
        let by_admin = delta(&f, base, 1, base, vec![], vec![f.member]);
        let h2 = delta_ok(accept_delta(
            &h1,
            &by_admin.clone().sign(&f.admin_key).unwrap(),
            &me,
            now,
        ));
        assert!(h2.roster.member(&f.member).is_none());
        assert!(h2.roster.has_device(&joiner, &jd));
        let coup = delta(&f, base, 2, by_admin.hash(), vec![], vec![f.owner]);
        assert!(matches!(
            accept_delta(&h2, &coup.sign(&f.admin_key).unwrap(), &me, now),
            DeltaVerdict::Refused(_)
        ));
        // 명단 밖 기기의 차이는 거부.
        let stranger = SigningKeypair::generate();
        let d = delta(&f, base, 1, base, vec![add(Some(11))], vec![]);
        assert!(matches!(
            accept_delta(&h, &d.sign(&stranger).unwrap(), &me, now),
            DeltaVerdict::Refused(_)
        ));
        // 통째 명단을 새로 받으면 바탕이 바뀌어 앞 차이가 만료됨.
        let newer = roster(&f, &f.state, 10, 2).sign(&f.admin_key).unwrap();
        let h3 = accepted(accept_roster(&h2, &newer, &me, RosterLocal::default(), now));
        assert_eq!(h3.base, Some(roster_hash(&newer.roster)));
        assert!(h3.deltas.is_empty() && h3.delta_count == 0);
        let late = delta(&f, base, 2, d1.hash(), vec![], vec![f.member]);
        assert!(matches!(
            accept_delta(&h3, &late.sign(&f.member_key).unwrap(), &me, now),
            DeltaVerdict::Expired(_)
        ));
    }

    /// 권한자가 아닌 멤버는 기기마다 [`MAX_MEMBER_DELTAS`]개까지만 보내고, 멤버 차이는 권한자 몫을 쓰지 않습니다(한
    /// 멤버가 차이를 쏟아 가입자 넣기·권한자 빼기를 막지 못하게).
    #[test]
    fn member_deltas_cannot_exhaust_the_budget() {
        let f = fx();
        let me = f.grant.device;
        let now = Timestamp(1_000);
        let admin_dev = DeviceId(f.admin_key.public());
        let first = roster(&f, &f.state, 10, 1).sign(&f.admin_key).unwrap();
        let mut h = accept_initial(&first, &me, (&f.admin, &admin_dev), now).unwrap();
        let base = h.base.unwrap();
        let own = h.roster.member(&f.member).cloned().unwrap();
        let mut prev = base;
        for seq in 1..=MAX_MEMBER_DELTAS {
            // 자기 항목을 그대로 다시 넣기: 명단을 바꾸지 않아 공용 몫을 쓰지 않음.
            let d = delta(&f, base, seq, prev, vec![own.clone()], vec![]);
            prev = d.hash();
            h = delta_ok(accept_delta(&h, &d.sign(&f.member_key).unwrap(), &me, now));
        }
        assert_eq!((h.delta_count, h.member_delta_count), (0, 0));
        let over = delta(&f, base, MAX_MEMBER_DELTAS + 1, prev, vec![own], vec![]);
        assert!(matches!(
            accept_delta(&h, &over.sign(&f.member_key).unwrap(), &me, now),
            DeltaVerdict::Expired(_)
        ));
        // 권한자 차이는 그대로 받고 권한자 몫으로 셉니다.
        let joiner = RosterMember {
            user: UserId([20; 32]),
            devices: vec![DeviceId([21; 32])],
            since: Some(11),
        };
        let by_admin = delta(&f, base, 1, base, vec![joiner], vec![]);
        let h2 = delta_ok(accept_delta(
            &h,
            &by_admin.sign(&f.admin_key).unwrap(),
            &me,
            now,
        ));
        assert_eq!((h2.delta_count, h2.member_delta_count), (1, 0));
        // 멤버 몫이 다 차도 권한자 차이는 받습니다.
        let full = HostedRoster {
            member_delta_count: MAX_DELTAS,
            ..h.clone()
        };
        let by_admin = delta(&f, base, 1, base, vec![], vec![f.member]);
        delta_ok(accept_delta(
            &full,
            &by_admin.sign(&f.admin_key).unwrap(),
            &me,
            now,
        ));
    }

    /// 재시작한 소유자 기기가 따라잡기 전에 낸 명단(새 채널이 없음)은 관리자의 더 새 명단을 덮지 못함.
    #[test]
    fn stale_owner_roster_does_not_win() {
        let f = fx();
        let me = f.grant.device;
        let now = Timestamp(1_000);
        let none = RosterLocal::default();
        let first = roster(&f, &f.state, 10, 1).sign(&f.owner_key).unwrap();
        let owner_dev = DeviceId(f.owner_key.public());
        let hosted = accept_initial(&first, &me, (&f.owner, &owner_dev), now).unwrap();

        // 소유자가 없는 사이 관리자가 공개 채널을 만들고 멤버를 밴함.
        let mut later = f.state.clone();
        let new_ch = ChannelId([9; 16]);
        later
            .channels
            .insert(new_ch, ChannelSettings::text("새 채널"));
        later.members.remove(&f.member);
        let by_admin = roster(&f, &later, 50, 2).sign(&f.admin_key).unwrap();
        let h = accepted(accept_roster(&hosted, &by_admin, &me, none, now));
        assert!(h.roster.channel(&new_ch).is_some());

        // 소유자의 오래된 명단(발행 시각은 더 늦음): 받지 않음.
        let stale = roster(&f, &f.state, 20, 3).sign(&f.owner_key).unwrap();
        assert_eq!(
            accept_roster(&h, &stale, &me, none, now),
            RosterVerdict::Stale
        );
        // 따라잡은 뒤의 소유자 명단은 받음.
        let caught_up = roster(&f, &later, 50, 4).sign(&f.owner_key).unwrap();
        let h = accepted(accept_roster(&h, &caught_up, &me, none, now));
        assert!(h.roster.channel(&new_ch).is_some());
        assert!(h.roster.member(&f.member).is_none());

        // 소유자가 관리자를 강등하는 오래된 명단: 좁혀서 받되 밴된 멤버를 되살리지 않음.
        let by_admin = roster(&f, &later, 80, 5).sign(&f.admin_key).unwrap();
        let h = accepted(accept_roster(&h, &by_admin, &me, none, now));
        let mut demoted = f.state.clone();
        Arc::make_mut(demoted.members.get_mut(&f.admin).unwrap())
            .roles
            .clear();
        let narrow = roster(&f, &demoted, 60, 6).sign(&f.owner_key).unwrap();
        let RosterVerdict::Narrowed(h) = accept_roster(&h, &narrow, &me, none, now) else {
            panic!("좁혀서 받아야 함");
        };
        assert!(h.roster.member(&f.member).is_none(), "밴된 멤버가 되살아남");
        assert!(h.roster.member(&f.owner).is_some());
        assert!(!h.roster.is_authority(&f.admin));
    }

    /// 다른 권한자(비공개 채널을 못 보는 관리자)의 명단: 지금보다 낮은 순서면 오래된 것으로 답함(보낸 쪽이 다시 보냄).
    #[test]
    fn lower_anchor_admin_roster_is_stale_not_accepted() {
        let f = fx();
        let me = f.grant.device;
        let now = Timestamp(1_000);
        let none = RosterLocal::default();
        let owner_dev = DeviceId(f.owner_key.public());
        let first = roster(&f, &f.state, 100, 1).sign(&f.owner_key).unwrap();
        let hosted = accept_initial(&first, &me, (&f.owner, &owner_dev), now).unwrap();
        let mut banned = f.state.clone();
        banned.members.remove(&f.member);
        let low = roster(&f, &banned, 90, 2).sign(&f.admin_key).unwrap();
        assert_eq!(
            accept_roster(&hosted, &low, &me, none, now),
            RosterVerdict::Stale
        );
        let caught_up = roster(&f, &banned, 101, 3).sign(&f.admin_key).unwrap();
        let h = accepted(accept_roster(&hosted, &caught_up, &me, none, now));
        assert!(h.roster.member(&f.member).is_none());
    }

    /// 권한자가 아닌 기기의 명단: 자기 기기 빼기와(밴·추방 권한이 있으면) 일반 멤버 빼기만 반영.
    #[test]
    fn removal_rosters_from_non_authorities() {
        let f = fx();
        let me = f.grant.device;
        let now = Timestamp(1_000);
        let owner_dev = DeviceId(f.owner_key.public());
        let first = roster(&f, &f.state, 10, 1).sign(&f.owner_key).unwrap();
        let hosted = accept_initial(&first, &me, (&f.owner, &owner_dev), now).unwrap();
        assert!(hosted.roster.is_warden(&f.warden));
        assert!(!hosted.roster.is_warden(&f.member));

        // 밴 권한자가 멤버를 밴: 순서가 지금 이상이고 보관 노드가 가진 공간 스트림 이하여야 함.
        let mut banned = f.state.clone();
        banned.members.remove(&f.member);
        let ban = roster(&f, &banned, 12, 2).sign(&f.warden_key).unwrap();
        assert_eq!(
            accept_roster(&hosted, &ban, &me, local(11), now),
            RosterVerdict::Stale,
            "보관 노드가 아직 그 이벤트를 받지 못함"
        );
        let old = roster(&f, &banned, 9, 2).sign(&f.warden_key).unwrap();
        assert_eq!(
            accept_roster(&hosted, &old, &me, local(12), now),
            RosterVerdict::Stale
        );
        let RosterVerdict::Removed(h) = accept_roster(&hosted, &ban, &me, local(12), now) else {
            panic!("빼기를 반영해야 함");
        };
        assert!(h.roster.member(&f.member).is_none());
        assert_eq!(
            h.roster.anchor, 12,
            "오래된 권한자 명단이 되살리지 못하게 순서를 올림"
        );
        assert_eq!(h.signer, hosted.signer);

        // 밴 권한자라도 권한자·소유자는 뺄 수 없고, 더할 수도 없음.
        let mut coup = f.state.clone();
        coup.members.remove(&f.admin);
        coup.channels.clear();
        let coup = roster(&f, &coup, 12, 3).sign(&f.warden_key).unwrap();
        assert_eq!(
            accept_roster(&hosted, &coup, &me, local(12), now),
            RosterVerdict::Unchanged
        );

        // 일반 멤버: 남은 못 빼고 자기 폐기된 기기만 뺌.
        let mut other = f.state.clone();
        other.members.remove(&f.warden);
        let other = roster(&f, &other, 12, 4).sign(&f.member_key).unwrap();
        assert_eq!(
            accept_roster(&hosted, &other, &me, local(12), now),
            RosterVerdict::Unchanged
        );
        let mut revoked = f.state.clone();
        revoked
            .devices
            .insert(f.member, list(f.member, &[f.member_dev]).into());
        let revoked = roster(&f, &revoked, 12, 5).sign(&f.member_key).unwrap();
        let RosterVerdict::Removed(h) = accept_roster(&hosted, &revoked, &me, local(12), now)
        else {
            panic!("자기 기기 빼기를 반영해야 함");
        };
        assert_eq!(
            h.roster.member(&f.member).unwrap().devices,
            vec![f.member_dev]
        );
    }

    #[test]
    fn member_side_seed_check() {
        let f = fx();
        let policy = SeederPolicy {
            all_channels: true,
            ..SeederPolicy::default()
        };
        let mut bucket = TokenBucket::unlimited();
        let archive_user = f.grant.user;
        let archive_dev = DeviceId(f.archive_key.public());
        let base = SeedRequest {
            state: &f.state,
            requester: archive_user,
            requester_device: archive_dev,
            lane: Lane::Message,
            channel: Some(f.public_ch),
            data_type: DataType::Text,
            event: None,
            event_lamport: Some(1),
            requester_since: Some(u64::MAX),
            channel_opened: false,
            bytes: 100,
            in_flight: 0,
            now_ms: 0,
        };
        let run = |req: SeedRequest<'_>, b: &mut TokenBucket| check_archive_seed(&policy, &req, b);
        assert_eq!(run(base, &mut bucket), Some(Ok(())));
        // 보관 노드가 아니면 보통 검사로.
        let member = SeedRequest {
            requester: f.member,
            requester_device: f.member_dev,
            ..base
        };
        assert_eq!(run(member, &mut bucket), None);
        // 사용자가 다르면 보통 검사(멤버 아님으로 끝남).
        let wrong_user = SeedRequest {
            requester: f.member,
            ..base
        };
        assert_eq!(run(wrong_user, &mut bucket), None);
        let hidden = SeedRequest {
            channel: Some(f.secret_ch),
            ..base
        };
        assert_eq!(
            run(hidden, &mut bucket),
            Some(Err(SeedRefusal::Denied(WireErrorCode::Forbidden)))
        );
        let emoji = SeedRequest {
            channel: None,
            data_type: DataType::Emoji,
            ..base
        };
        assert_eq!(
            run(emoji, &mut bucket),
            Some(Err(SeedRefusal::Denied(WireErrorCode::Forbidden)))
        );
        let state_lane = SeedRequest {
            lane: Lane::State,
            channel: None,
            ..base
        };
        assert_eq!(run(state_lane, &mut bucket), Some(Ok(())));
        // 시더 범위는 그대로 적용.
        let narrow = SeederPolicy::default();
        assert_eq!(
            check_archive_seed(&narrow, &base, &mut bucket),
            Some(Err(SeedRefusal::Denied(WireErrorCode::NotSeeded)))
        );
        let mut excluded = policy.clone();
        excluded.exclude_channels.insert(f.public_ch);
        assert_eq!(
            check_archive_seed(&excluded, &base, &mut bucket),
            Some(Err(SeedRefusal::Denied(WireErrorCode::NotSeeded)))
        );
    }
}
