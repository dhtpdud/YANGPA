//! 크루 키 그룹의 `KeyUpdate` 사슬(공간 스트림)과 표 보유 기기.
//!
//! 크루 갱신은 공간 스트림에 실려 해소기가 정한 순서로 판정하므로, 같은 이벤트 집합이면 어느 노드에서나 같은 사슬이
//! 됩니다(같은 순번의 두 갱신은 해소 순서에서 먼저 끝난 것이 이기고 나머지는 [`AuthError::DuplicateCommit`]).
//!
//! - **조각**: 한 갱신은 조각 `0..parts`로 나뉠 수 있습니다. 첫 조각(머리)이 사슬 검사(순번·앞 이벤트·앞 약속·작성 기기)를
//!   받고, 이어지는 조각은 머리를 부모로 두고(그래서 해소 순서에서도 머리 뒤) 같은 작성 기기·같은 머리 해시
//!   ([`KeyFacts::digest`])여야 합니다. 마지막 조각이 들어온 순간 합친 갱신을 판정합니다. 끝나지 않은 갱신은 같은 순번의
//!   새 머리가 오면 버립니다.
//! - **제거**: 사슬 멤버였다가 활성 기기가 아니게 된 기기(폐기·탈퇴·추방·밴)는 해소 순서로 "빚"([`CrewKeys::debt`])에
//!   쌓이고, 자격을 잃게 한 이벤트를 기억합니다([`CrewKeys::lost_by`]). 갱신의 제거 목록은 빚 안이어야 하고(확정 기기가
//!   자격 있는 기기를 몰래 빼지 못함), 자격을 잃게 한 이벤트가 갱신 머리의 조상인 빚은 모두 담아야 합니다(확정 기기가 본
//!   제거는 빠뜨리지 못함). 머리와 동시인 제거는 다음 갱신으로 넘어갑니다. 해소 순서만으로 "정확히 빚"을 요구하면
//!   확정 기기가 볼 수 없었던 동시 탈퇴 하나가 이미 적용된 정직한 갱신을 뒤집어(모든 노드에서 사슬이 물러나고 확정
//!   기기는 전체 재키) 탈퇴가 이어지는 동안 사슬이 서지 못합니다. 조상 관계는 해시로 고정되므로 어느 노드에서나 같습니다.
//! - **가입**: 가입 기기는 지금 활성 기기이고, 그 기기를 활성으로 만든 가입·기기 목록 갱신이 갱신 머리의 조상이어야
//!   합니다(확정 기기가 본 것만 넣음, 조상 검사는 [`super::super::ancestry::reaches`]).
//! - **받는 기기**: 칸 채우기·봉인 상자 수신자는 갱신 뒤 멤버여야 합니다(설계의 "자격이 있었던 기기"보다 좁음: 확정
//!   기기가 그룹 밖 기기에게 키를 주는 길이 없음).
//! - **표 보유 기기**(이 기기 말고 크루 키 표를 가진 기기): 마지막 전체 재키 뒤의 모든 확정 기기(`CommitterChange`·자동
//!   교체)와 예비 기기(`KeyBackupSet`). 하나라도 소유자·ADMINISTRATOR의 활성 기기가 아니게 되면(폐기·탈퇴·강등)
//!   [`CrewKeys::rekey_required`]가 켜지고 다음 크루 갱신은 `FullRekey`여야 합니다. 비공개·덮개 그룹은 크루 전체 재키
//!   뒤 첫 갱신이 새 T여야 합니다(`state::group`).

use std::collections::BTreeSet;
use std::sync::Arc;

use yangpa_core::event::EventHeader;
use yangpa_core::ids::{DeviceId, EventId, KeyGroupId, UserId};
use yangpa_core::model::{Permissions, SharedMap};

use super::super::KeyPoint;
use super::super::ancestry::{self, Dag};
use super::super::facts::{KeyFacts, UpdateKind};
use super::{AuthError, Effect, Machine, R};

/// 크루 키 사슬(해소기가 받아들인 것)과 표 보유 기기. 해소기 상태라 체크포인트 정규 인코딩·상태 해시에 그대로
/// 들어갑니다(체크포인트로 들어온 멤버와 처음부터 아는 노드가 다음 크루 갱신을 같게 판정하도록). 체크포인트 때 조상
/// 검사에 쓰는 이벤트 참조는 [`CrewKeys::seal`]이 정리합니다.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CrewKeys {
    /// 마지막으로 받아들인 갱신의 순번(아직 없으면 0).
    pub seq: u64,
    /// 그 갱신의 첫 조각 이벤트(다음 갱신의 `prev`).
    pub head: Option<EventId>,
    pub root_commit: [u8; 32],
    /// 트리 세대(트리 전체 재키마다 하나씩).
    pub generation: u64,
    pub depth: u8,
    /// 지금 그룹 멤버(기기).
    pub members: imbl::OrdSet<DeviceId>,
    /// 멤버마다 마지막으로 들어온 순번. 비공개·덮개 그룹이 그 사이 크루에서 빠졌다 다시 들어온 기기를 가립니다.
    pub joined_at: SharedMap<DeviceId, u64>,
    /// 멤버였는데 활성 기기가 아니게 된 기기(해소 순서). 갱신이 빼기 전까지 남고, 그 사이 자격을 되찾아도 남습니다.
    pub debt: imbl::OrdSet<DeviceId>,
    /// 빚마다 자격을 잃게 한 이벤트. 갱신 머리의 조상이면 그 갱신이 빼야 합니다.
    pub lost_by: SharedMap<DeviceId, EventId>,
    /// 마지막 전체 재키의 순번(없으면 0).
    pub last_rekey: u64,
    /// 표 보유 기기: 마지막 전체 재키 뒤의 모든 확정 기기와 예비 기기.
    pub holders: imbl::OrdSet<DeviceId>,
    /// `KeyBackupSet`으로 정한 예비 기기.
    pub backup: Option<DeviceId>,
    /// 표 보유 기기가 자격을 잃어 다음 크루 갱신은 전체 재키여야 함.
    pub rekey_required: bool,
    /// 활성 기기가 됐지만 아직 사슬에 들지 않은 기기 → 활성으로 만든 이벤트.
    pub(crate) added_by: SharedMap<DeviceId, EventId>,
    /// 조각을 모으는 중인 갱신.
    pub(crate) pending: Option<Box<Pending>>,
    /// 최근 받아들인 갱신의 조각 ID(순번 → 조각 번호 순, [`RECENT_PARTS`]개까지). 멤버는 도착 순서가 아니라 해소기가
    /// 받아들인 바로 그 조각을 합칩니다(확정 기기가 같은 번호의 조각을 둘 내도 노드마다 같은 키).
    pub(crate) recent_parts: imbl::OrdMap<u64, Arc<[EventId]>>,
    /// 사슬 끝 갱신을 마무리한 조각(해소 순서로 마지막). 그룹 갱신이 기준점에 넣어 그 크루 상태에 기댑니다.
    pub tip_part: Option<EventId>,
}

/// [`CrewKeys::recent_parts`]에 남기는 갱신 수(멤버 쪽 보관 폭과 같음).
pub(crate) const RECENT_PARTS: u64 = 64;

/// 조각을 모으는 중인 크루 갱신.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Pending {
    head: EventId,
    author: DeviceId,
    seq: u64,
    kind: UpdateKind,
    root_commit: [u8; 32],
    depth: u8,
    parts: u16,
    digest: [u8; 32],
    /// 받은 조각 번호(비트).
    got: u32,
    /// 조각 번호 → 이벤트.
    ids: Vec<Option<EventId>>,
    gathered: Gathered,
}

/// 조각들에서 모은 목록.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Gathered {
    pub joined: BTreeSet<DeviceId>,
    pub removed: BTreeSet<DeviceId>,
    pub fills: BTreeSet<DeviceId>,
    pub recipients: BTreeSet<DeviceId>,
    pub unreadable: bool,
    pub wraps: usize,
}

impl Gathered {
    /// 조각 하나의 목록을 더합니다. 다른 조각과 기기가 겹치면(같은 기기가 두 번, 가입과 제거에 함께) 거부하고 그대로
    /// 둡니다.
    pub fn add(&mut self, f: &KeyFacts) -> Result<(), AuthError> {
        let clash = |d: &DeviceId| self.joined.contains(d) || self.removed.contains(d);
        // 조각 안의 겹침은 형식 검사(`KeyUpdatePayload::validate`)가 이미 막지만, 해소기는 디코딩을 거치지 않은 본문도
        // 받으므로 여기서도 봅니다(`KeyFacts::of`가 목록을 정렬해 둠).
        let inner = f.joined.iter().any(|d| f.removed.binary_search(d).is_ok());
        if inner || f.joined.iter().chain(&f.removed).any(clash) {
            return Err(AuthError::Invalid("키 갱신 조각의 기기 목록이 겹침"));
        }
        self.joined.extend(f.joined.iter().copied());
        self.removed.extend(f.removed.iter().copied());
        self.fills.extend(f.fills.iter().copied());
        self.recipients.extend(f.recipients.iter().copied());
        self.unreadable |= f.unreadable;
        self.wraps += f.wraps;
        Ok(())
    }

    pub fn of(f: &KeyFacts) -> Result<Self, AuthError> {
        let mut g = Gathered::default();
        g.add(f)?;
        Ok(g)
    }

    /// 받는 기기(칸 채우기·봉인 상자 수신자)가 모두 `members`에 있는지, 상자를 모두 읽을 수 있는지.
    pub fn delivers_within(&self, members: &imbl::OrdSet<DeviceId>) -> Result<(), AuthError> {
        if self.unreadable {
            return Err(AuthError::Invalid("수신자를 읽을 수 없는 봉인 상자"));
        }
        if self
            .fills
            .iter()
            .chain(&self.recipients)
            .any(|d| !members.contains(d))
        {
            return Err(AuthError::Invalid("그룹 밖 기기에게 키를 줌"));
        }
        Ok(())
    }
}

/// 받은 조각 비트가 다 찼는지.
pub(crate) fn full(got: u32, parts: u16) -> bool {
    let all = if parts >= 32 {
        u32::MAX
    } else {
        (1u32 << parts) - 1
    };
    got == all
}

impl CrewKeys {
    pub(super) fn new(committer: DeviceId) -> Self {
        CrewKeys {
            seq: 0,
            head: None,
            root_commit: [0; 32],
            generation: 0,
            depth: 0,
            members: imbl::OrdSet::new(),
            joined_at: SharedMap::new(),
            debt: imbl::OrdSet::new(),
            last_rekey: 0,
            holders: imbl::OrdSet::unit(committer),
            backup: None,
            rekey_required: false,
            added_by: SharedMap::new(),
            pending: None,
            lost_by: SharedMap::new(),
            recent_parts: imbl::OrdMap::new(),
            tip_part: None,
        }
    }

    /// 받아들인 갱신(순번)의 조각 ID(조각 번호 순). 최근 [`RECENT_PARTS`]개만 압니다.
    pub fn parts_of(&self, seq: u64) -> Option<Arc<[EventId]>> {
        self.recent_parts.get(&seq).cloned()
    }

    /// 조각을 모으는 중인 갱신이 있는지(그 머리 이벤트).
    pub fn pending_head(&self) -> Option<EventId> {
        self.pending.as_ref().map(|p| p.head)
    }

    /// 체크포인트 때의 정리(작성자·검증자·스냅숏으로 들어온 멤버가 같은 값). 조상 검사에 쓰던 이벤트 참조는 모두 체크포인트
    /// 조상이라, 경계 아래 기록이 없는 멤버는 셀 수 없습니다. 그래서 결정적으로 바꿉니다.
    ///
    /// - 가입 기록(`added_by`): 시작 이벤트로(조상 검사 없음). 체크포인트 상태에서 이미 활성인 기기라 어느 노드나 같은
    ///   상태로 확인합니다.
    /// - 자격을 잃게 한 이벤트(`lost_by`): 지웁니다. 남은 빚은 바탕 체크포인트 이벤트가 갱신 머리의 조상일 때 반드시
    ///   담아야 합니다([`Machine::complete`]). 체크포인트를 본 확정 기기는 그 앞 제거를 모두 봤으므로 원래 규칙보다
    ///   느슨하지 않고, 체크포인트와 동시인 갱신만 다음 갱신으로 미룰 수 있습니다.
    ///
    /// 조각 ID 기록(`recent_parts`, 64순번)과 모으는 중인 갱신(`pending`)은 그대로 둡니다(크기가 묶여 있고, 체크포인트에
    /// 걸친 갱신의 나머지 조각도 모든 노드에서 같게 받아들이도록).
    pub(super) fn seal(&mut self, genesis: EventId) {
        let added: Vec<DeviceId> = self
            .added_by
            .iter()
            .filter(|(_, e)| **e != genesis)
            .map(|(d, _)| *d)
            .collect();
        for d in added {
            self.added_by.insert(d, genesis);
        }
        self.lost_by = SharedMap::new();
    }
}

/// 해소한 크루 키 사슬과, 그 시점 멤버의 활성 기기 → 사용자. 데몬(확정 기기·멤버)과 키 그룹 사슬 판정이 씁니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyView {
    pub crew: CrewKeys,
    pub owners: SharedMap<DeviceId, UserId>,
}

impl Machine {
    pub fn key_view(&self) -> KeyView {
        KeyView {
            crew: self.keys.clone(),
            owners: self.device_owner.clone(),
        }
    }

    pub fn point(self) -> KeyPoint {
        KeyPoint {
            keys: self.key_view(),
            state: self.st,
        }
    }

    /// 표 보유 기기·예비 기기의 자격: 소유자나 ADMINISTRATOR 멤버의 활성 기기(확정 기기와 같은 자격).
    fn holder_ok(&self, d: &DeviceId) -> bool {
        self.device_owner.get(d).is_some_and(|u| {
            *u == self.st.owner
                || self
                    .st
                    .base_permissions(u)
                    .contains(Permissions::ADMINISTRATOR)
        })
    }

    /// 사슬이 아직 없으면(키가 없어 표도 없음) 표 보유 기기는 지금 확정 기기와 예비 기기뿐입니다.
    fn reset_empty_holders(&mut self) {
        if self.keys.seq != 0 {
            return;
        }
        let mut h = imbl::OrdSet::unit(self.st.committer);
        if let Some(b) = self.keys.backup {
            h.insert(b);
        }
        if h != self.keys.holders {
            self.keys.holders = h;
        }
        self.keys.rekey_required = false;
    }

    /// 상태가 바뀐 뒤: 지금 확정 기기는 표를 가지므로 보유 기기에 넣고, 자격을 잃은 예비 기기는 풀고, 자격을 잃은 보유
    /// 기기가 있으면 전체 재키를 요구합니다.
    pub(super) fn check_holders(&mut self) {
        if let Some(b) = self.keys.backup
            && (b == self.st.committer || !self.holder_ok(&b))
        {
            self.keys.backup = None;
        }
        if self.keys.seq == 0 {
            self.reset_empty_holders();
            return;
        }
        let committer = self.st.committer;
        if !self.keys.holders.contains(&committer) {
            self.keys.holders.insert(committer);
        }
        if !self.keys.rekey_required && self.keys.holders.iter().any(|h| !self.holder_ok(h)) {
            self.keys.rekey_required = true;
        }
    }

    /// 기기가 활성 기기가 아니게 됨(기기 목록에서 폐기, 멤버 제거).
    pub(super) fn lost_device(&mut self, d: DeviceId) {
        if self.keys.added_by.contains_key(&d) {
            self.keys.added_by.remove(&d);
        }
        if self.keys.members.contains(&d) && !self.keys.debt.contains(&d) {
            self.keys.debt.insert(d);
            self.keys.lost_by.insert(d, self.cur);
        }
    }

    /// 기기가 활성 기기가 됨: 지금 적용 중인 이벤트가 넣은 것으로 기록합니다.
    pub(super) fn gained_device(&mut self, d: DeviceId) {
        self.keys.added_by.insert(d, self.cur);
    }

    pub(super) fn key_backup_set(&mut self, user: &UserId, device: Option<&DeviceId>) -> R {
        if *user != self.st.owner {
            return Err(AuthError::NotOwner);
        }
        if device.copied() == self.keys.backup {
            return Err(AuthError::Invalid("예비 기기가 이미 그대로임"));
        }
        if let Some(d) = device {
            if *d == self.st.committer {
                return Err(AuthError::Invalid("확정 기기는 예비 기기가 될 수 없음"));
            }
            if !self.holder_ok(d) {
                return Err(AuthError::InvalidCommitter);
            }
            // 예비 기기는 표를 받으므로 바로 보유 기기입니다(나중에 풀어도 이미 받은 표는 남아 보유 기기로 셈).
            if !self.keys.holders.contains(d) {
                self.keys.holders.insert(*d);
            }
        }
        self.keys.backup = device.copied();
        self.reset_empty_holders();
        Ok(Effect::key_event())
    }

    /// 크루 `KeyUpdate` 조각 하나. 마지막 조각이면 합친 갱신을 판정해 적용합니다.
    pub(super) fn key_update(
        &mut self,
        id: EventId,
        h: &EventHeader,
        f: &KeyFacts,
        dag: &dyn Dag,
    ) -> R {
        if h.author != self.st.committer {
            return Err(AuthError::NotCommitter);
        }
        if f.group != KeyGroupId::crew(&self.space) || f.crew.is_some() {
            return Err(AuthError::Invalid("공간 스트림의 키 갱신은 크루 그룹만"));
        }
        let k = &self.keys;
        if f.part == 0 {
            if f.seq <= k.seq {
                return Err(AuthError::DuplicateCommit);
            }
            if f.seq != k.seq + 1 {
                return Err(AuthError::Invalid("키 갱신 순번이 이어지지 않음"));
            }
            if f.prev != k.head || f.prev_root_commit != k.root_commit {
                return Err(AuthError::Invalid("앞 키 갱신과 이어지지 않음"));
            }
            if k.rekey_required && !f.kind.is_full_rekey() {
                return Err(AuthError::Invalid(
                    "표 보유 기기가 자격을 잃어 전체 재키가 먼저",
                ));
            }
            // 끝나지 않은 같은 순번 갱신이 있으면 새 머리가 대신합니다(어느 노드나 해소 순서가 같아 같은 결과).
            let mut ids = vec![None; usize::from(f.parts)];
            if let Some(first) = ids.first_mut() {
                *first = Some(id);
            }
            self.keys.pending = Some(Box::new(Pending {
                head: id,
                author: h.author,
                seq: f.seq,
                kind: f.kind,
                root_commit: f.root_commit,
                depth: f.depth,
                parts: f.parts,
                digest: f.digest,
                got: 1,
                ids,
                gathered: Gathered::of(f)?,
            }));
        } else {
            let Some(p) = self.keys.pending.as_mut() else {
                return Err(AuthError::Invalid("키 갱신 머리 조각을 모름"));
            };
            if !h.parents.contains(&p.head) || p.author != h.author || p.digest != f.digest {
                return Err(AuthError::Invalid("키 갱신 머리 조각을 모름"));
            }
            let bit = 1u32 << f.part;
            if p.got & bit != 0 {
                return Err(AuthError::Invalid("같은 키 갱신 조각이 두 번"));
            }
            p.gathered.add(f)?;
            p.got |= bit;
            if let Some(slot) = p.ids.get_mut(usize::from(f.part)) {
                *slot = Some(id);
            }
        }
        let done = self
            .keys
            .pending
            .as_ref()
            .is_some_and(|p| full(p.got, p.parts));
        if !done {
            return Ok(Effect::none());
        }
        let p = self.keys.pending.take().expect("위에서 확인");
        self.complete(*p, id, dag)
    }

    /// 다 모인 크루 갱신을 판정하고 적용합니다. 거부하면 사슬은 그대로입니다(모으던 갱신은 버림).
    fn complete(&mut self, p: Pending, last: EventId, dag: &dyn Dag) -> R {
        let k = &self.keys;
        let g = &p.gathered;
        // 제거 목록은 빚 안이어야 하고(더 넣으면 확정 기기가 자격 있는 기기를 몰래 뺌), 자격을 잃게 한 이벤트가 머리의
        // 조상인 빚은 모두 담아야 합니다(빠뜨리면 확정 기기가 본 나간 기기가 새 키를 받음). 머리와 동시인 제거는 확정
        // 기기가 볼 수 없었으니 다음 갱신으로 넘깁니다.
        if g.removed.iter().any(|d| !k.debt.contains(d)) {
            return Err(AuthError::Invalid("제거 목록이 자격을 잃은 기기와 다름"));
        }
        // 체크포인트 앞에 생긴 빚은 자격을 잃게 한 이벤트 대신 바탕 체크포인트 이벤트로 셉니다([`CrewKeys::seal`]).
        let base_event = self.base.as_ref().map(|b| b.event);
        let left: Vec<(DeviceId, EventId)> = k
            .debt
            .iter()
            .filter(|d| !g.removed.contains(d))
            .filter_map(|d| k.lost_by.get(d).copied().or(base_event).map(|e| (*d, e)))
            .collect();
        if !left.is_empty() {
            let events: Vec<EventId> = left.iter().map(|(_, e)| *e).collect();
            if !ancestry::reached(dag, &[p.head], &events).is_empty() {
                return Err(AuthError::Invalid("제거 목록이 자격을 잃은 기기와 다름"));
            }
        }
        if !p.kind.is_full_rekey() && g.removed.iter().any(|d| k.holders.contains(d)) {
            return Err(AuthError::Invalid("표 보유 기기는 전체 재키로만 뺌"));
        }
        let mut targets = Vec::new();
        for d in &g.joined {
            if k.members.contains(d) {
                return Err(AuthError::AlreadyMember);
            }
            if !self.device_owner.contains_key(d) {
                return Err(AuthError::Invalid("활성 기기가 아닌 기기를 넣음"));
            }
            let Some(by) = k.added_by.get(d) else {
                return Err(AuthError::Invalid("활성 기기가 된 기록이 없는 기기를 넣음"));
            };
            if *by != self.genesis {
                targets.push(*by);
            }
        }
        targets.sort_unstable();
        targets.dedup();
        if !ancestry::reaches(dag, &[p.head], &targets) {
            return Err(AuthError::Invalid(
                "가입 기기를 넣은 이벤트가 갱신의 조상이 아님",
            ));
        }
        let mut after = k.members.clone();
        for d in &g.removed {
            after.remove(d);
        }
        for d in &g.joined {
            after.insert(*d);
        }
        g.delivers_within(&after)?;
        if let UpdateKind::FullRekey { generation } = p.kind {
            // 트리를 새로 만들면 세대가 하나 오르고, 직접 인코딩 전체 재키는 그대로입니다.
            if generation != k.generation + u64::from(p.depth > 0) {
                return Err(AuthError::Invalid("전체 재키 세대가 맞지 않음"));
            }
        }

        let k = &mut self.keys;
        for d in &g.removed {
            k.joined_at.remove(d);
        }
        for d in &g.joined {
            k.joined_at.insert(*d, p.seq);
            if k.added_by.contains_key(d) {
                k.added_by.remove(d);
            }
        }
        k.members = after;
        for d in &g.removed {
            k.debt.remove(d);
            k.lost_by.remove(d);
        }
        k.seq = p.seq;
        k.head = Some(p.head);
        k.tip_part = Some(last);
        let ids: Arc<[EventId]> = p.ids.iter().flatten().copied().collect();
        k.recent_parts.insert(p.seq, ids);
        while let Some(old) = k
            .recent_parts
            .get_min()
            .map(|(s, _)| *s)
            .filter(|s| s + RECENT_PARTS <= p.seq)
        {
            k.recent_parts.remove(&old);
        }
        k.root_commit = p.root_commit;
        k.depth = p.depth;
        if let UpdateKind::FullRekey { generation } = p.kind {
            k.generation = generation;
            k.last_rekey = p.seq;
            k.rekey_required = false;
            let mut holders = imbl::OrdSet::unit(p.author);
            if let Some(b) = k.backup {
                holders.insert(b);
            }
            k.holders = holders;
        }
        Ok(Effect::key_event())
    }
}
