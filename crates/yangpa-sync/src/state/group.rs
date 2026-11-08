//! 비공개·덮개 키 그룹의 `KeyUpdate` 사슬(키 그룹 스트림 `KeyGroup(g)`).
//!
//! 키 그룹 스트림은 그룹 멤버만 받는 부분 전달 스트림이라 해소기에 들어가지 않습니다. 그래서 데몬이 그룹마다
//! [`GroupChain`]을 두고 받은 조각을 넣습니다. 이긴 사슬은 받은 후보 집합만의 함수(넣은 순서와 무관)이고, 판정에 쓰는
//! 상태는 후보마다 작성자가 적은 기준점의 상태([`KeyPoints::at`])라 어느 노드에서나 같습니다.
//!
//! 후보 하나는 한 갱신의 조각 모두입니다. 조각마다 자기 기준점 상태로 [`authorize_scoped`]를 통과해야 넣고(작성 기기가
//! 확정 기기, 그 그룹을 쓰는 채널이 있음), 조각이 다 모이면 앞 갱신을 기준으로 판정합니다.
//!
//! - **사슬**: 순번·`prev`(앞 갱신의 첫 조각)·앞 약속이 앞 갱신과 이어지고, 기준점이 앞 갱신의 기준점을 조상으로 둡니다
//!   (작성자가 기준점을 거꾸로 돌려 그때의 권한을 되살리지 못하게).
//! - **크루 상태**: `crew`가 기준점 상태의 크루 사슬 끝(순번·약속)과 같고 앞 갱신의 것보다 작지 않습니다.
//! - **멤버**: 자격 E = 기준점의 크루 멤버 가운데, 주인 사용자가 그 그룹 채널 하나라도 볼 수 있는 기기. 제거 목록은
//!   정확히 (앞 멤버 − E) ∪ (앞 갱신 뒤 크루에서 빠졌다가 다시 들어온 앞 멤버)이고, 가입 ⊆ E − 앞 멤버입니다. 덮개
//!   그룹의 첫 `Refresh`(순번 1)만 목록 없이 멤버 = E입니다. 그래서 그룹 멤버는 늘 크루 멤버이고, 그 사이 자격을 잃었던
//!   기기는 반드시 한 번 빠집니다.
//! - **받는 기기**: 칸 채우기·봉인 상자 수신자 ⊆ 갱신 뒤 멤버.
//! - **크루 표 보유 기기**를 빼는 갱신은 직접 인코딩(깊이 0, 감싸기 없음)이어야 합니다. 비공개 트리 잎과 덮개 감싸기는
//!   크루 잎·노드 키에서 나오는데 보유 기기는 leaf_master로 그것을 다 계산하므로, 크루 전체 재키가 먼저입니다.
//! - **크루 전체 재키 뒤** 첫 갱신은 새 T(`FullRekey`, 또는 깊이 0의 `Rotate`·`Refresh`)여야 합니다(넘긴 표에 그룹의
//!   T도 있었음).
//!
//! 같은 앞 갱신을 잇는 유효한 후보가 여럿이면 (크루 순번이 큰 것, 논리 시계가 작은 것, ID가 작은 것)이 이깁니다. 늦게 온
//! 후보가 이기면 그 순번부터 사슬이 바뀝니다([`ChainChange::dropped`]; 멤버 쪽 `MemberKeys`는 순번마다 상태를 보관했다가
//! 이긴 갈래로 옮깁니다). [`KEEP_SEQS`]보다 깊은 갈래 바꿈은 하지 않습니다(그 앞 후보는 정리).
//!
//! **후보 상한**: 같은 순번에서 작성 기기마다 순위가 좋은 [`MAX_FORKS`]개, 모두 합쳐 [`MAX_SEQ_CANDS`]개만 들고, 넘으면
//! 순위가 가장 나쁜 것을 버립니다. 남는 집합이 받은 후보 집합만의 함수라 도착 순서와 무관하고, 옛 확정 기기가 끝나지
//! 않는 머리를 쏟아내도 자기 몫만 채울 뿐 지금 확정 기기의 후보를 밀어내지 못합니다. 머리보다 먼저 온 이어지는 조각은
//! 작성 기기마다 [`MAX_AUTHOR_ORPHANS`]개까지 들고 있다가 머리가 오면 붙입니다. 같은 번호의 조각이 둘이거나 목록이
//! 겹치는 조각이 붙은 후보는 거부합니다(어느 조각을 먼저 받았든 같은 결과).
//!
//! **빈 구간**: 기준점 상태는 조상에 빠진 이벤트나 본문을 풀지 못한 상태 이벤트가 있어도 그것을 빈 구간으로 보고 셉니다
//! ([`KeyPoints::at`]). 그렇게 낸 판정은 노드마다 다를 수 있으므로, 조상 집합이 바뀌면([`KeyPoints::gap_epoch`]) 그
//! 순번부터 다시 판정합니다. 빈 구간을 낀 상태로는 조각을 거부하지 않고 기다립니다([`AuthError::AnchorUnknown`]).
//!
//! **지금 상태**: 고르는 데는 쓰지 않고(쓰면 판정 시각에 따라 노드마다 갈래가 갈림), 쓸 수 있는지에만 씁니다. 이긴 사슬의
//! 갱신은 작성 기기가 지금도 표를 가질 자격(지금 확정 기기, 또는 소유자·ADMINISTRATOR의 활성 기기)이 있거나, 뒤에 그런
//! 갱신이 이어 붙어 있어야 씁니다([`GroupChain::send_tip`]). 그래서 강등·폐기된 옛 확정 기기가 옛 기준점으로 낸 갱신은
//! 쓰이지 않고, 확정 기기가 바뀌어도 자격이 남은 옛 확정 기기의 정직한 갱신은 그대로 쓰입니다. 쓸 수 있는 끝이 없으면
//! 보낼 키도 없습니다(이미 정리한 옛 바닥으로 돌아가지 않음).
//!
//! **체크포인트**: 체크포인트 본문의 키 머리([`KeyHead`]: 순번, 뿌리 약속, 머리 이벤트, 크루 순번, 새 T의 크루 순번,
//! 멤버 요약, 작성자)가 그 순번까지의 사슬을 정합니다([`GroupChain::apply_head`]). 모든 노드(처음부터 아는 노드, 머리에서
//! 시작한 새 멤버, 다시 시작한 노드, 뒤처진 노드)가 바닥을 머리에서 만든 같은 마디로 바꿉니다: 머리 정보는 처음부터 아는
//! 노드가 그 순번에서 가진 값 그대로이고, 기준점은 비웁니다. 그래서 다음 갱신의 판정(앞 갱신 잇기, 새 T 먼저, 정확한
//! 제거 목록, 작성자 자격)이 노드마다 같습니다. 바닥 멤버 목록은 그룹 멤버가 중계해 주고 요약과 맞는 것만 씁니다
//! ([`GroupChain::learn_members`]); 목록을 알기 전에는 바닥 다음 갱신을 판정하지 않고 기다립니다(느슨하게 판정하면
//! 노드마다 갈림).
//!
//! **기준점과 바탕**: 체크포인트 C가 바탕이면 기준점 이벤트를 C에 비춰 셉니다([`KeyPoints::place`]): C 자신과 C 뒤에 놓인
//! 이벤트(`After`), C의 경계 위 조상(`Covered`, 새 멤버는 논리 시계만 앎), 경계 F 아래(`Below`). 이 분류는 모든 노드가
//! 같이 합니다. 머리가 있는 그룹에서 머리 순번 뒤 갱신은 자기 논리 시계가 F 아래이거나, 기준점에 F 아래 이벤트가
//! 있거나, 기준점이 모두 C의 조상이면(C를 모르고 낸 것) 늦은 이벤트로 거부하고 이미 받은 것도 버립니다
//! ([`AuthError::LateEvent`]). 판정은 기준점 가운데 `After`만으로 셉니다(C의 조상은 C 상태에 이미 들어 있음). 기준점
//! 되돌리기 검사도 앞 갱신 기준점의 `After`만 봅니다: C의 조상은 C 뒤 기준점이 늘 덮고, 그 밖의 비교는 C 뒤 이벤트끼리라
//! 어느 노드에서나 같습니다. 확정 기기는 사슬 끝에서 지금 머리를 기준점으로 다시 냅니다. 머리가 없는 그룹(체크포인트를
//! 쓴 기기가 모름)은 처음부터의 사슬을 그대로 씁니다.
//!
//! 재시작한 노드는 저장한 바닥([`ChainBase`], 체크포인트 머리까지 접은 것)에서 이어 갑니다.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use yangpa_core::checkpoint::{HeadChain, KeyHead, members_digest};
use yangpa_core::event::{EventHeader, EventKind, Scope};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, KeyGroupId, SpaceId, UserId};
use yangpa_core::keyupdate::{KeyUpdatePayload, MAX_PARTS};
use yangpa_core::model::{Permissions, SpaceState};

use super::facts::{KeyFacts, UpdateKind};
use super::machine::keys::{Gathered, KeyView, full};
use super::{AuthError, Resolver, authorize_scoped};

/// 한 시점(기준점, 또는 지금)의 공간 상태와 크루 키 사슬.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPoint {
    pub state: SpaceState,
    pub keys: KeyView,
}

/// 키 그룹 사슬 판정이 묻는 것. 해소기([`Resolver`])가 구현합니다.
pub trait KeyPoints {
    /// 기준점과 그 조상만으로 해소한 상태와 크루 키 사슬, 그리고 빈 구간(빠진 조상·풀지 못한 상태 이벤트)을 끼고 셌는지.
    /// 기준점 ID를 모르면 `None`.
    fn at(&self, anchor: &[EventId]) -> Option<(KeyPoint, bool)>;
    /// 지금 상태.
    fn now(&self) -> Option<&SpaceState>;
    /// `earlier`가 모두 `later` 가운데 하나이거나 그 조상인지.
    fn includes(&self, later: &[EventId], earlier: &[EventId]) -> bool;
    /// 이미 아는 이벤트의 조상 집합이 바뀔 때마다 오르는 값. 바뀌면 빈 구간을 끼고 낸 판정을 다시 합니다.
    fn gap_epoch(&self) -> u64;
    /// 그룹 `group`의 갱신에 쓰는 경계: 바탕 체크포인트에 그 그룹의 키 머리가 있으면 경계 F, 없으면 `None`(머리가 없으면
    /// 새 멤버가 시작할 자리가 없어 경계 아래 갱신도 그대로 받아야 함).
    fn group_floor(&self, _group: &KeyGroupId) -> Option<u64> {
        None
    }
    /// 기준점 이벤트 하나가 바탕 체크포인트에 비춰 어디인지(모든 노드가 같이 셈). 바탕이 없으면 아는 이벤트는 모두
    /// `After`.
    fn place(&self, id: &EventId) -> AnchorPlace;
}

/// 바탕 체크포인트 C에 비춘 기준점 이벤트의 자리.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorPlace {
    /// 모르는 이벤트.
    Unknown,
    /// 경계 F 아래(늦은 이벤트이거나 C의 아주 옛 조상).
    Below,
    /// C의 경계 위 조상(체크포인트 본문의 경계 목록): C 상태에 이미 들어 있음.
    Covered,
    /// C 자신이거나 C 뒤에 놓인 이벤트(바탕이 없으면 아는 이벤트 전부).
    After,
}

impl KeyPoints for Resolver {
    fn at(&self, anchor: &[EventId]) -> Option<(KeyPoint, bool)> {
        self.point_at(anchor)
    }

    fn now(&self) -> Option<&SpaceState> {
        self.state()
    }

    fn includes(&self, later: &[EventId], earlier: &[EventId]) -> bool {
        Resolver::includes(self, later, earlier)
    }

    fn gap_epoch(&self) -> u64 {
        Resolver::gap_epoch(self)
    }

    fn group_floor(&self, group: &KeyGroupId) -> Option<u64> {
        let b = self.base()?;
        b.key_heads
            .iter()
            .any(|k| k.group == *group)
            .then_some(b.floor)
    }

    fn place(&self, id: &EventId) -> AnchorPlace {
        self.anchor_place(id)
    }
}

/// 같은 순번에서 작성 기기 하나가 들 수 있는 후보 수(순위가 좋은 것부터).
pub const MAX_FORKS: usize = 8;
/// 같은 순번에 드는 후보 수 합계 상한(순위가 좋은 것부터). 확정 기기였던 기기가 이만큼 많을 때만 닿습니다.
pub const MAX_SEQ_CANDS: usize = 64;
/// 작성 기기마다 머리를 기다리는 이어지는 조각 수 상한.
pub const MAX_AUTHOR_ORPHANS: usize = 64;
/// 머리를 기다리는 이어지는 조각 수 합계 상한.
pub const MAX_ORPHANS: usize = 1024;
/// 쓸 수 있는 사슬을 이만큼 남기고 그 앞의 후보는 정리합니다. 받는 순번도 바닥에서 2배까지만 받습니다.
pub const KEEP_SEQS: usize = 64;

/// 사슬 끝(받아들인 마지막 갱신 뒤).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupTip {
    /// 순번(아직 없으면 0).
    pub seq: u64,
    /// 그 갱신의 첫 조각(다음 갱신의 `prev`).
    pub head: Option<EventId>,
    pub root_commit: [u8; 32],
    pub members: imbl::OrdSet<DeviceId>,
    /// 그 갱신이 기댄 크루 순번.
    pub crew_seq: u64,
    /// 마지막으로 새 T를 낸 갱신이 기댄 크루 순번. 크루의 마지막 전체 재키가 이보다 뒤면 이 그룹은 새 T가 먼저입니다
    /// (넘긴 표에 이 그룹의 T도 있었으므로 보내는 쪽은 기다림).
    pub fresh_crew: u64,
    /// `members`를 아는지. 체크포인트 키 머리에서 세운 바닥은 멤버 요약만 알아 `false`이고(`members`는 빔), 그동안은
    /// 다음 갱신을 판정하지 않습니다(멤버를 배우면 참).
    pub members_known: bool,
}

/// 저장해 두었다가 다시 시작할 때 쓰는 사슬 바닥(쓸 수 있는 마지막 갱신 뒤). 비밀은 없습니다.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChainBase {
    pub seq: u64,
    pub head: Option<EventId>,
    pub root_commit: [u8; 32],
    pub members: Vec<DeviceId>,
    pub members_known: bool,
    pub crew_seq: u64,
    pub fresh_crew: u64,
    pub anchor: Vec<EventId>,
    pub author: Option<(UserId, DeviceId)>,
    /// 멤버를 모를 때 배울 목록의 요약(체크포인트 키 머리의 것).
    pub expect: Option<[u8; 32]>,
}

/// [`GroupChain::insert`]·[`GroupChain::rejudge`]가 알려 주는 쓸 수 있는 사슬의 변화(갱신 머리 ID, 사슬 순서).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainChange {
    pub accepted: Vec<EventId>,
    /// 전에는 쓸 수 있었지만 이제 사슬에 없는 갱신(늦게 온 후보가 이김).
    pub dropped: Vec<EventId>,
}

/// 같은 작성 기기가 같은 순번에 낸 서로 다른 유효한 갱신 둘(확정 기기의 두 판, 증거로 남김).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Equivocation {
    pub seq: u64,
    pub author: DeviceId,
    pub heads: [EventId; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    tip: GroupTip,
    anchor: Vec<EventId>,
    /// 작성자(사용자, 기기). 바닥의 순번 0에는 없음.
    author: Option<(UserId, DeviceId)>,
    /// 멤버를 모를 때 배울 목록의 요약.
    expect: Option<[u8; 32]>,
}

impl Node {
    /// 다음 갱신의 `prev`가 이 갱신을 가리키는지.
    fn follows(&self, prev: Option<EventId>) -> bool {
        prev == self.tip.head
    }

    /// 체크포인트 키 머리에서 세운 마디(멤버는 모르고 요약만 앎, 기준점은 비움). 머리가 있는 그룹은 모든 노드가 바닥을
    /// 이것으로 바꿉니다.
    fn of_head(head: &KeyHead, c: &HeadChain) -> Self {
        Node {
            tip: GroupTip {
                seq: head.seq,
                head: Some(c.head),
                root_commit: head.root_commit,
                members: imbl::OrdSet::new(),
                crew_seq: c.crew_seq,
                fresh_crew: c.fresh_crew,
                members_known: false,
            },
            anchor: Vec::new(),
            author: Some((c.user, c.device)),
            expect: Some(c.members),
        }
    }
}

#[derive(Clone, Debug)]
struct Cand {
    /// 첫 조각의 헤더.
    header: EventHeader,
    /// 첫 조각의 사실(목록은 `gathered`).
    facts: KeyFacts,
    parts: Vec<Option<EventId>>,
    got: u32,
    gathered: Gathered,
    /// 조각 때문에 거부할 이유(같은 번호의 조각이 둘, 목록이 겹치는 조각). 받은 순서와 무관하게 거부합니다.
    bad: Option<&'static str>,
    /// 판정(아직 못 했으면 `None`: 조각·앞 갱신·기준점 상태를 기다림).
    verdict: Option<Result<Node, AuthError>>,
    /// 판정이 빈 구간을 낀 기준점 상태(또는 그런 앞 갱신)에 기댐.
    gapped: bool,
}

impl Cand {
    fn complete(&self) -> bool {
        full(self.got, self.facts.parts)
    }

    fn rank(&self, id: EventId) -> (Reverse<u64>, u64, EventId) {
        rank_of(&self.facts, &self.header, id)
    }

    fn node(&self) -> Option<&Node> {
        match &self.verdict {
            Some(Ok(n)) => Some(n),
            _ => None,
        }
    }
}

fn rank_of(f: &KeyFacts, h: &EventHeader, id: EventId) -> (Reverse<u64>, u64, EventId) {
    let crew = f.crew.map_or(0, |c| c.seq);
    (Reverse(crew), h.lamport, id)
}

/// 머리보다 먼저 온 이어지는 조각.
#[derive(Clone, Debug)]
struct Orphan {
    id: EventId,
    header: EventHeader,
    facts: KeyFacts,
}

/// 지금 상태에서 갱신을 쓸 수 있는 작성 기기: 지금 확정 기기, 또는 표를 가질 자격(소유자·ADMINISTRATOR 멤버의 활성
/// 기기)이 남은 기기.
fn author_ok(now: &SpaceState, user: &UserId, device: &DeviceId) -> bool {
    now.is_member(user)
        && now.device_belongs(user, device)
        && (*device == now.committer
            || *user == now.owner
            || now
                .base_permissions(user)
                .contains(Permissions::ADMINISTRATOR))
}

/// 키 그룹 하나의 `KeyUpdate` 사슬.
#[derive(Clone, Debug)]
pub struct GroupChain {
    space: SpaceId,
    group: KeyGroupId,
    /// 정리한 앞부분의 끝(처음에는 순번 0).
    base: Node,
    /// 바닥의 작성 기기가 지금도 쓸 수 있는지(마지막 판정 때).
    base_ok: bool,
    cands: HashMap<EventId, Cand>,
    /// 조각 ID → 머리 ID.
    part_head: HashMap<EventId, EventId>,
    by_seq: BTreeMap<u64, Vec<EventId>>,
    /// 머리를 기다리는 이어지는 조각.
    orphans: Vec<Orphan>,
    /// 이긴 사슬(바닥 다음 순번부터의 머리).
    chain: Vec<EventId>,
    /// `chain` 가운데 쓸 수 있는 앞부분의 길이.
    usable: usize,
    /// 마지막으로 본 [`KeyPoints::gap_epoch`].
    epoch: u64,
    /// 이미 알린 두 판(순번, 작성 기기).
    reported: HashSet<(u64, DeviceId)>,
}

impl GroupChain {
    pub fn new(space: SpaceId, group: KeyGroupId) -> Self {
        GroupChain {
            space,
            group,
            base: Node {
                tip: GroupTip {
                    seq: 0,
                    head: None,
                    root_commit: [0; 32],
                    members: imbl::OrdSet::new(),
                    crew_seq: 0,
                    fresh_crew: 0,
                    members_known: true,
                },
                anchor: Vec::new(),
                author: None,
                expect: None,
            },
            base_ok: false,
            cands: HashMap::new(),
            part_head: HashMap::new(),
            by_seq: BTreeMap::new(),
            orphans: Vec::new(),
            chain: Vec::new(),
            usable: 0,
            epoch: 0,
            reported: HashSet::new(),
        }
    }

    pub fn group(&self) -> &KeyGroupId {
        &self.group
    }

    /// 체크포인트 키 머리에서 시작하는 사슬(그 앞 사슬을 모름). 크루 머리처럼 사슬 정보가 없는 머리면 `None`. 바닥 멤버는
    /// 멤버가 중계해 줄 때까지 모르므로([`Self::learn_members`]) 그동안 다음 갱신은 판정하지 않습니다. 그 머리의 키는 이
    /// 기기에 없을 수 있습니다(다음 갱신의 묶음·중계로 얻음).
    pub fn from_head(space: SpaceId, head: &KeyHead) -> Option<Self> {
        let c = head.chain.as_ref()?;
        let mut g = GroupChain::new(space, head.group);
        g.base = Node::of_head(head, c);
        Some(g)
    }

    /// 저장한 바닥에서 다시 시작합니다(재시작). 바닥 앞 사슬은 이미 정리한 것과 같습니다.
    pub fn from_saved(space: SpaceId, group: KeyGroupId, b: ChainBase) -> Self {
        let mut c = GroupChain::new(space, group);
        c.base = Node {
            tip: GroupTip {
                seq: b.seq,
                head: b.head,
                root_commit: b.root_commit,
                members: b.members.into_iter().collect(),
                crew_seq: b.crew_seq,
                fresh_crew: b.fresh_crew,
                members_known: b.members_known,
            },
            anchor: b.anchor,
            author: b.author,
            expect: b.expect,
        };
        c
    }

    /// 다시 시작할 때 쓸 바닥: 접어 둔 앞부분의 끝([`Self::apply_head`]가 체크포인트 머리까지 접음). 다시 시작한
    /// 노드는 저장된 갱신을 다시 넣는데, 바닥 위 갱신 가운데 경계 아래 것은 이미 사슬에서 뺐으므로 같은 사슬이 됩니다.
    pub fn saved_base(&self) -> ChainBase {
        let n = &self.base;
        ChainBase {
            seq: n.tip.seq,
            head: n.tip.head,
            root_commit: n.tip.root_commit,
            members: n.tip.members.iter().copied().collect(),
            members_known: n.tip.members_known,
            crew_seq: n.tip.crew_seq,
            fresh_crew: n.tip.fresh_crew,
            anchor: n.anchor.clone(),
            author: n.author,
            expect: n.expect,
        }
    }

    /// 바닥을 가리키는 값(순번, 뿌리 약속, 멤버를 앎): 바뀌었는지 싸게 볼 때(저장).
    pub fn base_key(&self) -> (u64, [u8; 32], bool) {
        let t = &self.base.tip;
        (t.seq, t.root_commit, t.members_known)
    }

    /// 받아들인 체크포인트의 이 그룹 키 머리에 사슬을 맞춥니다. 모든 노드가 같은 사슬을 보게 하는 규칙입니다(머리에서
    /// 시작한 새 멤버, 그 전부터 있던 멤버, 다시 시작한 멤버, 뒤처진 멤버):
    /// - 머리 순번 뒤 후보 가운데 늦은 것([`Self::late`])은 버립니다: 체크포인트를 쓴 기기가 몰랐던 갱신이라 새 멤버는 그것을
    ///   늦은 이벤트로 거부합니다. 그 확정 기기는 사슬 끝에서 새로 냅니다.
    /// - 바닥이 이미 머리 뒤까지 접혔으면(쓸 수 있는 갱신이 [`KEEP_SEQS`]를 넘음) 그대로 둡니다. 아니면 바닥을 머리에서
    ///   세운 마디로 바꾸고(이긴 사슬이 그 순번의 같은 갱신을 지나도 같은 마디로), 머리 뒤 후보는 그 위에서 다시 판정합니다.
    ///   그 갱신 뒤 멤버를 알고 요약이 맞으면 멤버도 그대로 씁니다.
    ///
    /// 사슬이 바뀌었으면 참(부르는 쪽이 다시 판정).
    pub fn apply_head(&mut self, head: &KeyHead, points: &dyn KeyPoints) -> bool {
        let seq = head.seq;
        let Some(hc) = head.chain.as_ref() else {
            return false;
        };
        if seq == 0 || head.group != self.group {
            return false;
        }
        let late: Vec<EventId> = self
            .cands
            .iter()
            .filter(|(_, c)| c.facts.seq > seq && self.late(&c.header, points))
            .map(|(id, _)| *id)
            .collect();
        let mut changed = !late.is_empty();
        for id in late {
            self.evict(id);
        }
        let before = self.orphans.len();
        let orphans = std::mem::take(&mut self.orphans);
        self.orphans = orphans
            .into_iter()
            .filter(|o| !(o.facts.seq > seq && self.late(&o.header, points)))
            .collect();
        changed |= self.orphans.len() != before;
        // 바탕이 바뀌어 기준점 분류(바탕 뒤인지)가 달라질 수 있어 머리 뒤는 다시 판정합니다.
        self.reset_from(seq + 1);
        self.judge(points);
        if self.base.tip.seq > seq {
            return changed;
        }
        let mut node = Node::of_head(head, hc);
        let mine = if self.base.tip.seq == seq {
            Some(&self.base)
        } else {
            self.chain
                .iter()
                .filter_map(|id| self.cands[id].node())
                .find(|n| n.tip.seq == seq)
        };
        if let Some(n) = mine.filter(|n| {
            n.tip.root_commit == head.root_commit
                && n.tip.head == Some(hc.head)
                && n.tip.members_known
                && members_digest(&self.group, n.tip.members.iter()) == hc.members
        }) {
            node.tip.members = n.tip.members.clone();
            node.tip.members_known = true;
            node.expect = None;
        }
        if self.base == node {
            return changed;
        }
        let mut next = GroupChain::new(self.space, self.group);
        next.base = node;
        next.epoch = self.epoch;
        let keep: Vec<(EventId, Cand)> = self
            .cands
            .drain()
            .filter(|(_, c)| c.facts.seq > seq)
            .collect();
        for (id, mut c) in keep {
            c.verdict = None;
            c.gapped = false;
            for p in c.parts.iter().flatten() {
                next.part_head.insert(*p, id);
            }
            next.by_seq.entry(c.facts.seq).or_default().push(id);
            next.cands.insert(id, c);
        }
        next.orphans = std::mem::take(&mut self.orphans)
            .into_iter()
            .filter(|o| o.facts.seq > seq)
            .collect();
        next.reported = std::mem::take(&mut self.reported);
        next.reported.retain(|(s, _)| *s > seq);
        *self = next;
        true
    }

    /// 늦은 갱신인지(모든 노드가 같이 셈): 자기 논리 시계가 그룹 경계 F 아래이거나, 기준점에 F 아래 이벤트가 있거나, 머리가
    /// 있는 그룹인데 기준점이 모두 바탕 체크포인트의 조상(바탕을 모르고 냄)인 것.
    fn late(&self, header: &EventHeader, points: &dyn KeyPoints) -> bool {
        let floor = points.group_floor(&self.group);
        let a = &header.state_anchor;
        if floor.is_some_and(|f| header.lamport < f)
            || a.iter().any(|x| points.place(x) == AnchorPlace::Below)
        {
            return true;
        }
        floor.is_some() && a.iter().all(|x| points.place(x) == AnchorPlace::Covered)
    }

    /// 판정에 쓰는 기준점: 머리가 있는 그룹이면 바탕 뒤(`After`) 이벤트만(바탕의 조상은 바탕 상태에 이미 들어 있고, 새
    /// 멤버는 그 상태를 셀 수 없음). 모르는 이벤트가 끼면 `None`(받을 때까지 기다림: 빼고 세면 아는 노드와 갈림).
    fn judged_anchor(&self, anchor: &[EventId], points: &dyn KeyPoints) -> Option<Vec<EventId>> {
        if points.group_floor(&self.group).is_none() {
            return Some(anchor.to_vec());
        }
        let mut out = Vec::with_capacity(anchor.len());
        for a in anchor {
            match points.place(a) {
                AnchorPlace::Unknown => return None,
                AnchorPlace::After => out.push(*a),
                AnchorPlace::Covered | AnchorPlace::Below => {}
            }
        }
        Some(out)
    }

    /// 멤버가 알려 준 바닥(순번, 뿌리 약속) 뒤 멤버 목록. 바닥 멤버를 모르고(체크포인트 키 머리에서 세움) 목록이 머리의
    /// 멤버 요약과 맞을 때만 넣고 그 뒤 판정을 다시 합니다(다음 판정에서). 넣었으면 참. 요약과 다른 목록은 누가 보냈든
    /// 버리므로 거짓 목록으로 판정을 흔들 수 없습니다.
    pub fn learn_members(
        &mut self,
        seq: u64,
        root_commit: &[u8; 32],
        members: &[DeviceId],
    ) -> bool {
        let b = &self.base;
        if b.tip.members_known || b.tip.seq != seq || b.tip.root_commit != *root_commit {
            return false;
        }
        let set: imbl::OrdSet<DeviceId> = members.iter().copied().collect();
        if set.len() != members.len() || b.expect != Some(members_digest(&self.group, set.iter())) {
            return false;
        }
        self.base.tip.members = set;
        self.base.tip.members_known = true;
        self.base.expect = None;
        self.reset_from(seq + 1);
        true
    }

    /// 이긴 사슬(바닥 포함)에서 (순번, 뿌리 약속)인 갱신 뒤 멤버 목록(알 때만). 바닥 멤버를 모르는 노드에게 중계합니다.
    pub fn members_at(&self, seq: u64, root_commit: &[u8; 32]) -> Option<Vec<DeviceId>> {
        let n = if self.base.tip.seq == seq {
            &self.base
        } else {
            self.chain
                .iter()
                .filter_map(|id| self.cands[id].node())
                .find(|n| n.tip.seq == seq)?
        };
        (n.tip.root_commit == *root_commit && n.tip.members_known)
            .then(|| n.tip.members.iter().copied().collect())
    }

    /// 체크포인트 본문에 실을 이 그룹의 키 머리(이긴 사슬의 끝). 아직 갱신이 없으면 `None`.
    pub fn key_head(&self) -> Option<KeyHead> {
        let n = match self.chain.last() {
            Some(id) => self.cands[id].node()?,
            None => &self.base,
        };
        let (user, device) = n.author?;
        let members = if n.tip.members_known {
            members_digest(&self.group, n.tip.members.iter())
        } else {
            n.expect?
        };
        Some(KeyHead {
            group: self.group,
            seq: n.tip.seq,
            root_commit: n.tip.root_commit,
            chain: Some(HeadChain {
                head: n.tip.head?,
                crew_seq: n.tip.crew_seq,
                fresh_crew: n.tip.fresh_crew,
                members,
                user,
                device,
            }),
        })
    }

    /// 체크포인트가 이 그룹의 순번 `seq`까지 덮었습니다: 이긴 사슬의 그 앞부분을 바닥으로 접습니다(그 아래로 갈래를
    /// 바꾸지 않음). 아직 쓸 수 없는 갱신까지 접으면 바닥을 쓸 수 있는지는 다음 판정이 정합니다.
    pub fn prune_covered(&mut self, seq: u64) {
        let n = self
            .chain
            .iter()
            .take_while(|id| self.cands[*id].facts.seq <= seq)
            .count();
        self.fold(n);
    }

    fn chain_node(&self, i: usize) -> &Node {
        self.cands[&self.chain[i]]
            .node()
            .expect("사슬에는 유효한 후보만")
    }

    /// 이긴 사슬의 끝(받아들인 마지막 유효한 갱신). 확정 기기는 여기에 이어 냅니다. 지금 보내는 데 쓸 수 있는지는
    /// [`GroupChain::send_tip`].
    pub fn tip(&self) -> GroupTip {
        match self.chain.len().checked_sub(1) {
            Some(i) => self.chain_node(i).tip.clone(),
            None => self.base.tip.clone(),
        }
    }

    /// 지금 보내는 데 쓸 수 있는 사슬 끝. 이긴 사슬이 있는데 쓸 수 있는 갱신이 하나도 없으면 `None`입니다(이미 정리한
    /// 바닥은 그 뒤 갱신이 뺀 기기도 아는 키라 돌아가지 않음). 새 확정 기기가 사슬에 이어 내면 다시 생깁니다.
    pub fn send_tip(&self) -> Option<GroupTip> {
        let t = match self.usable.checked_sub(1) {
            Some(i) => &self.chain_node(i).tip,
            None if self.chain.is_empty() && self.base_ok => &self.base.tip,
            None => return None,
        };
        (t.seq > 0).then(|| t.clone())
    }

    /// 이긴 사슬에서 (순번, 뿌리 약속)이 맞는 갱신의 끝. 지금 상태로는 아직 쓸 수 없는 뒷부분(바뀌기 전 확정 기기가 낸
    /// 갱신)도 봅니다. 그 상태를 넘겨받은 새 확정 기기가 여기에 이어 붙이면 앞 갱신도 쓸 수 있게 됩니다.
    pub fn winning_at(&self, seq: u64, root_commit: &[u8; 32]) -> Option<GroupTip> {
        if self.base.tip.seq == seq && self.base.tip.root_commit == *root_commit {
            return Some(self.base.tip.clone());
        }
        self.chain
            .iter()
            .filter_map(|id| self.cands[id].node())
            .map(|n| &n.tip)
            .find(|t| t.seq == seq && t.root_commit == *root_commit)
            .cloned()
    }

    /// 쓸 수 있는 사슬의 갱신 머리(정리하고 남은 것, 순번 순).
    pub fn winners(&self) -> &[EventId] {
        &self.chain[..self.usable]
    }

    /// 다 모인 갱신의 조각 ID(조각 번호 순). 데몬은 이것으로 페이로드를 합칩니다.
    pub fn parts(&self, head: &EventId) -> Option<Vec<EventId>> {
        let c = self.cands.get(head)?;
        c.parts.iter().copied().collect()
    }

    /// 후보의 판정(`None`이면 모르는 후보이거나 아직 판정하지 못함).
    pub fn verdict(&self, head: &EventId) -> Option<Result<(), AuthError>> {
        match self.cands.get(head)?.verdict.as_ref()? {
            Ok(_) => Some(Ok(())),
            Err(e) => Some(Err(e.clone())),
        }
    }

    /// 아직 알리지 않은 두 판(같은 작성 기기가 같은 순번에 낸 유효한 갱신 둘). 정직한 확정 기기는 거부된 갱신만 같은
    /// 순번으로 다시 내므로, 둘 다 유효하면 갈래를 나눠 보낸 증거입니다.
    pub fn take_equivocations(&mut self) -> Vec<Equivocation> {
        let mut out = Vec::new();
        for (seq, ids) in &self.by_seq {
            let mut first: HashMap<DeviceId, EventId> = HashMap::new();
            let mut ok: Vec<&EventId> = ids
                .iter()
                .filter(|id| self.cands[*id].node().is_some())
                .collect();
            ok.sort_unstable();
            for id in ok {
                let author = self.cands[id].header.author;
                match first.get(&author) {
                    None => {
                        first.insert(author, *id);
                    }
                    Some(a) => {
                        if self.reported.insert((*seq, author)) {
                            out.push(Equivocation {
                                seq: *seq,
                                author,
                                heads: [*a, *id],
                            });
                        }
                    }
                }
            }
        }
        out
    }

    /// 조각 하나를 넣습니다. 이 조각만의 검사(모양, 기준점 상태의 [`authorize_scoped`])에서 걸리면 넣지 않고
    /// 오류입니다. [`AuthError::AnchorUnknown`]이면 기준점을 받은 뒤(또는 빈 구간이 채워진 뒤) 다시 넣으면 됩니다.
    /// 머리보다 먼저 온 이어지는 조각은 들고 있다가 머리가 오면 붙입니다(`Ok`).
    pub fn insert(
        &mut self,
        id: EventId,
        header: &EventHeader,
        payload: &KeyUpdatePayload,
        points: &dyn KeyPoints,
    ) -> Result<ChainChange, AuthError> {
        if header.space != self.space {
            return Err(AuthError::WrongSpace);
        }
        if header.kind != EventKind::KeyUpdate {
            return Err(AuthError::KindMismatch);
        }
        if header.scope != Some(Scope::KeyGroup(self.group)) || payload.group != self.group {
            return Err(AuthError::Invalid("다른 키 그룹의 갱신"));
        }
        if payload.crew.is_none() {
            return Err(AuthError::Invalid("크루 상태가 없는 그룹 갱신"));
        }
        if payload.parts == 0 || payload.parts > MAX_PARTS || payload.part >= payload.parts {
            return Err(AuthError::Malformed);
        }
        header.check_shape().map_err(|_| AuthError::Malformed)?;
        if self.part_head.contains_key(&id) || self.orphans.iter().any(|o| o.id == id) {
            return Ok(ChainChange::default());
        }
        let floor = self.base.tip.seq;
        if payload.seq <= floor {
            return Err(AuthError::DuplicateCommit);
        }
        if payload.seq > floor + 2 * KEEP_SEQS as u64 {
            return Err(AuthError::Limit("키 갱신 순번이 너무 앞섬"));
        }
        if self.late(header, points) {
            return Err(AuthError::LateEvent);
        }
        let judged = self
            .judged_anchor(&header.state_anchor, points)
            .ok_or(AuthError::AnchorUnknown)?;
        if judged.is_empty() {
            return Err(AuthError::LateEvent);
        }
        let (at, gaps) = points.at(&judged).ok_or(AuthError::AnchorUnknown)?;
        if let Err(e) = authorize_scoped(&at.state, &self.space, header) {
            // 빈 구간을 끼고 센 상태로는 거부를 확정하지 않습니다(빠진 이벤트가 오면 다시 넣음).
            return Err(if gaps { AuthError::AnchorUnknown } else { e });
        }
        let f = KeyFacts::of(payload);
        if f.part == 0 {
            let gathered = Gathered::of(&f)?;
            self.admit_head(id, header, f, gathered)?;
            self.adopt_orphans(id);
        } else {
            let head = header
                .parents
                .iter()
                .find(|p| {
                    self.cands.get(p).is_some_and(|c| {
                        c.facts.digest == f.digest && c.header.author == header.author
                    })
                })
                .copied();
            match head {
                Some(head) => self.attach(head, id, &f),
                None => {
                    self.keep_orphan(id, header, f)?;
                    return Ok(ChainChange::default());
                }
            }
        }
        Ok(self.judge(points))
    }

    /// 머리 조각을 후보로 넣습니다. 같은 순번에서 그 작성 기기의 후보가 [`MAX_FORKS`]개(모두 합쳐
    /// [`MAX_SEQ_CANDS`]개)면 순위가 가장 나쁜 것과 비교해, 새것이 더 나쁘면 거부하고 아니면 그것을 버립니다.
    fn admit_head(
        &mut self,
        id: EventId,
        header: &EventHeader,
        f: KeyFacts,
        gathered: Gathered,
    ) -> Result<(), AuthError> {
        let seq = f.seq;
        let rank = rank_of(&f, header, id);
        let ids = self.by_seq.get(&seq).cloned().unwrap_or_default();
        let mine: Vec<EventId> = ids
            .iter()
            .filter(|x| self.cands[*x].header.author == header.author)
            .copied()
            .collect();
        for (pool, cap) in [(mine, MAX_FORKS), (ids, MAX_SEQ_CANDS)] {
            if pool.len() < cap {
                continue;
            }
            let worst = pool
                .iter()
                .copied()
                .filter(|x| self.cands.contains_key(x))
                .max_by_key(|x| self.cands[x].rank(*x));
            match worst {
                Some(w) if self.cands[&w].rank(w) > rank => self.evict(w),
                Some(_) => return Err(AuthError::Limit("같은 순번 키 갱신 갈래")),
                None => {}
            }
        }
        let mut parts = vec![None; usize::from(f.parts)];
        parts[0] = Some(id);
        self.cands.insert(
            id,
            Cand {
                header: header.clone(),
                facts: f,
                parts,
                got: 1,
                gathered,
                bad: None,
                verdict: None,
                gapped: false,
            },
        );
        self.by_seq.entry(seq).or_default().push(id);
        self.part_head.insert(id, id);
        Ok(())
    }

    /// 순위에서 밀린 후보를 버립니다. 그 순번부터 판정을 다시 합니다(버린 후보에 기댄 판정이 있을 수 있음).
    fn evict(&mut self, id: EventId) {
        let Some(c) = self.cands.remove(&id) else {
            return;
        };
        for p in c.parts.iter().flatten() {
            self.part_head.remove(p);
        }
        if let Some(v) = self.by_seq.get_mut(&c.facts.seq) {
            v.retain(|x| *x != id);
        }
        self.reset_from(c.facts.seq);
    }

    /// 이어지는 조각을 후보에 붙입니다. 같은 번호의 다른 조각이 이미 있거나 목록이 겹치면 후보를 거부할 것으로 표시합니다
    /// (먼저 받은 조각을 고르면 노드마다 합친 갱신이 달라짐).
    fn attach(&mut self, head: EventId, id: EventId, f: &KeyFacts) {
        let Some(c) = self.cands.get_mut(&head) else {
            return;
        };
        let i = usize::from(f.part);
        self.part_head.insert(id, head);
        let seq = c.facts.seq;
        match c.parts.get(i).copied() {
            None => c.bad = Some("조각 번호가 조각 수를 넘음"),
            Some(Some(x)) if x != id => c.bad = Some("같은 키 갱신 조각이 두 번"),
            Some(Some(_)) => return,
            Some(None) => {
                if c.gathered.add(f).is_err() {
                    c.bad = Some("키 갱신 조각의 기기 목록이 겹침");
                }
                c.got |= 1u32 << f.part;
                c.parts[i] = Some(id);
            }
        }
        self.reset_from(seq);
    }

    fn keep_orphan(
        &mut self,
        id: EventId,
        header: &EventHeader,
        facts: KeyFacts,
    ) -> Result<(), AuthError> {
        let mine = self
            .orphans
            .iter()
            .filter(|o| o.header.author == header.author)
            .count();
        if mine >= MAX_AUTHOR_ORPHANS || self.orphans.len() >= MAX_ORPHANS {
            return Err(AuthError::Limit("머리를 기다리는 키 갱신 조각"));
        }
        self.orphans.push(Orphan {
            id,
            header: header.clone(),
            facts,
        });
        Ok(())
    }

    /// 방금 넣은 머리를 기다리던 조각을 붙입니다.
    fn adopt_orphans(&mut self, head: EventId) {
        let Some(c) = self.cands.get(&head) else {
            return;
        };
        let (digest, author) = (c.facts.digest, c.header.author);
        let (mine, rest): (Vec<Orphan>, Vec<Orphan>) = std::mem::take(&mut self.orphans)
            .into_iter()
            .partition(|o| {
                o.facts.digest == digest
                    && o.header.author == author
                    && o.header.parents.contains(&head)
            });
        self.orphans = rest;
        for o in mine {
            self.attach(head, o.id, &o.facts);
        }
    }

    /// 순번 `seq`부터 판정을 지웁니다(다음 판정에서 다시 함).
    fn reset_from(&mut self, seq: u64) {
        for c in self.cands.values_mut() {
            if c.facts.seq >= seq {
                c.verdict = None;
                c.gapped = false;
            }
        }
    }

    /// 지금 상태가 바뀌었거나 기다리던 기준점을 받은 뒤 다시 판정합니다.
    pub fn rejudge(&mut self, points: &dyn KeyPoints) -> ChainChange {
        self.judge(points)
    }

    fn judge(&mut self, points: &dyn KeyPoints) -> ChainChange {
        // 0. 조상 집합이 바뀌었으면 빈 구간을 끼고 낸 판정을 그 순번부터 다시 합니다.
        let epoch = points.gap_epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            let first = self
                .cands
                .values()
                .filter(|c| c.gapped && c.verdict.is_some())
                .map(|c| c.facts.seq)
                .min();
            if let Some(s) = first {
                self.reset_from(s);
            }
        }
        // 1. 판정: 순번 오름차순이라 앞 갱신의 판정이 먼저 정해집니다.
        let seqs: Vec<u64> = self.by_seq.keys().copied().collect();
        for s in seqs {
            for id in self.by_seq[&s].clone() {
                let c = &self.cands[&id];
                if c.verdict.is_some() || !c.complete() {
                    continue;
                }
                let parent: Option<(Result<Node, AuthError>, bool)> = if let Some(why) = c.bad {
                    Some((Err(AuthError::Invalid(why)), false))
                } else if s == self.base.tip.seq + 1 {
                    // 바닥 멤버를 모르면(체크포인트 키 머리) 배울 때까지 판정하지 않습니다.
                    self.base
                        .tip
                        .members_known
                        .then(|| (Ok(self.base.clone()), false))
                } else {
                    match c.facts.prev {
                        None => Some((Err(AuthError::Invalid("앞 갱신과 이어지지 않음")), false)),
                        Some(p) => match self.cands.get(&p) {
                            None => None,
                            Some(pc) => match &pc.verdict {
                                None => None,
                                Some(Err(_)) => {
                                    Some((Err(AuthError::Invalid("앞 갱신이 거부됨")), pc.gapped))
                                }
                                Some(Ok(n)) => Some((Ok(n.clone()), pc.gapped)),
                            },
                        },
                    }
                };
                let (verdict, gapped) = match parent {
                    None => (None, false),
                    Some((Err(e), g)) => (Some(Err(e)), g),
                    Some((Ok(p), g)) => {
                        match self.validate(&c.header, &c.facts, &c.gathered, id, &p, points) {
                            None => (None, false),
                            Some((v, gaps)) => (Some(v), g || gaps),
                        }
                    }
                };
                let c = self.cands.get_mut(&id).expect("있음");
                c.verdict = verdict;
                c.gapped = gapped;
            }
        }
        // 2. 이긴 사슬.
        let mut chain = Vec::new();
        let mut cur = self.base.tip.head;
        let mut s = self.base.tip.seq + 1;
        loop {
            let best = self
                .by_seq
                .get(&s)
                .into_iter()
                .flatten()
                .filter(|id| {
                    let c = &self.cands[*id];
                    c.facts.prev == cur && c.node().is_some()
                })
                .min_by_key(|id| self.cands[*id].rank(**id));
            let Some(&b) = best else { break };
            chain.push(b);
            cur = Some(b);
            s += 1;
        }
        // 3. 쓸 수 있는 앞부분: 작성 기기가 지금도 쓸 자격이 있는 마지막 갱신까지.
        let now = points.now();
        let usable = match now {
            None => 0,
            Some(now) => chain
                .iter()
                .rposition(|id| {
                    let h = &self.cands[id].header;
                    author_ok(now, &h.user, &h.author)
                })
                .map_or(0, |i| i + 1),
        };
        self.base_ok = match (now, &self.base.author) {
            (Some(now), Some((u, d))) => author_ok(now, u, d),
            _ => false,
        };
        let old = &self.chain[..self.usable];
        let new = &chain[..usable];
        let common = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let change = ChainChange {
            accepted: new[common..].to_vec(),
            dropped: old[common..].to_vec(),
        };
        self.chain = chain;
        self.usable = usable;
        self.prune();
        change
    }

    /// 쓸 수 있는 사슬이 [`KEEP_SEQS`]를 넘으면 앞부분을 바닥으로 접고 그 순번까지의 후보를 버립니다.
    fn prune(&mut self) {
        let Some(excess) = self.usable.checked_sub(KEEP_SEQS).filter(|e| *e > 0) else {
            return;
        };
        self.fold(excess);
    }

    /// 쓸 수 있는 사슬의 앞 `excess`개를 바닥으로 접고 그 순번까지의 후보를 버립니다.
    fn fold(&mut self, excess: usize) {
        if excess == 0 {
            return;
        }
        let base = self.chain_node(excess - 1).clone();
        self.base = base;
        self.base_ok = excess <= self.usable;
        self.chain.drain(..excess);
        self.usable = self.usable.saturating_sub(excess);
        let floor = self.base.tip.seq;
        let old: Vec<u64> = self.by_seq.range(..=floor).map(|(s, _)| *s).collect();
        for s in old {
            for id in self.by_seq.remove(&s).unwrap_or_default() {
                if let Some(c) = self.cands.remove(&id) {
                    for p in c.parts.into_iter().flatten() {
                        self.part_head.remove(&p);
                    }
                }
            }
        }
        self.orphans.retain(|o| o.facts.seq > floor);
        self.reported.retain(|(s, _)| *s > floor);
    }

    /// 확정 기기가 내기 전에 봅니다: 합친 갱신 `payload`를 기준점 `anchor`, 논리 시계 `lamport`로 `user`의 `author` 기기가
    /// 사슬 끝에 이어 내면 받아들여질지(넣을 때와 같은 늦은 갱신 규칙과 판정). 받아들여지지 않을 갱신을 내면 멤버마다
    /// 갈래 자리만 차지하므로 내지 않고 다시 시도합니다.
    pub fn would_accept(
        &self,
        user: &UserId,
        author: &DeviceId,
        anchor: &[EventId],
        lamport: u64,
        payload: &KeyUpdatePayload,
        points: &dyn KeyPoints,
    ) -> Result<(), AuthError> {
        if payload.group != self.group || payload.crew.is_none() {
            return Err(AuthError::Invalid("다른 키 그룹의 갱신"));
        }
        let parent = match self.chain.len().checked_sub(1) {
            Some(i) => self.chain_node(i).clone(),
            None => self.base.clone(),
        };
        if !parent.tip.members_known {
            // 바닥 멤버를 아직 모름(배울 때까지 아무도 판정하지 않음).
            return Err(AuthError::AnchorUnknown);
        }
        if !parent.follows(payload.prev) {
            return Err(AuthError::Invalid("앞 갱신과 이어지지 않음"));
        }
        // 판정에는 기준점·논리 시계·작성자·범위만 쓰므로 그것만 채운 헤더로 봅니다.
        let probe = EventHeader {
            v: yangpa_core::event::EVENT_VERSION,
            space: self.space,
            channel: None,
            kind: EventKind::KeyUpdate,
            author: *author,
            user: *user,
            parents: Vec::new(),
            lamport,
            created_at: yangpa_core::time::Timestamp(0),
            key: yangpa_core::event::KeyRef::Sealed,
            body_hash: [0; 32],
            body_len: 0,
            scope: Some(Scope::KeyGroup(self.group)),
            state_anchor: anchor.to_vec(),
        };
        if self.late(&probe, points) {
            return Err(AuthError::LateEvent);
        }
        let judged = self
            .judged_anchor(anchor, points)
            .ok_or(AuthError::AnchorUnknown)?;
        if judged.is_empty() {
            return Err(AuthError::LateEvent);
        }
        let (at, _) = points.at(&judged).ok_or(AuthError::AnchorUnknown)?;
        let st = &at.state;
        if !st.is_member(user) || !st.device_belongs(user, author) {
            return Err(AuthError::UnknownDevice);
        }
        if *author != st.committer {
            return Err(AuthError::NotCommitter);
        }
        if !st.has_key_group(&self.space, &self.group) {
            return Err(AuthError::UnknownChannel);
        }
        let f = KeyFacts::of(payload);
        let g = Gathered::of(&f)?;
        match self.validate(&probe, &f, &g, EventId([0; 32]), &parent, points) {
            None => Err(AuthError::AnchorUnknown),
            Some((r, _)) => r.map(|_| ()),
        }
    }

    /// 다 모인 후보를 앞 갱신(`parent`) 뒤로 판정합니다. 기준점 상태를 아직 모르면 `None`. 함께 돌려주는 값은 기준점
    /// 상태가 빈 구간을 끼었는지.
    fn validate(
        &self,
        header: &EventHeader,
        f: &KeyFacts,
        g: &Gathered,
        id: EventId,
        parent: &Node,
        points: &dyn KeyPoints,
    ) -> Option<(Result<Node, AuthError>, bool)> {
        let crew = f.crew.expect("넣을 때 확인");
        if f.seq != parent.tip.seq + 1
            || !parent.follows(f.prev)
            || f.prev_root_commit != parent.tip.root_commit
        {
            return Some((Err(AuthError::Invalid("앞 갱신과 이어지지 않음")), false));
        }
        if crew.seq < parent.tip.crew_seq {
            return Some((
                Err(AuthError::Invalid("크루 상태가 앞 갱신보다 오래됨")),
                false,
            ));
        }
        // 머리가 있는 그룹은 바탕 뒤 기준점만으로 셉니다(모듈 문서 "기준점과 바탕").
        let anchor = self.judged_anchor(&header.state_anchor, points)?;
        if anchor.is_empty() {
            return Some((Err(AuthError::LateEvent), false));
        }
        let (kp, gaps) = points.at(&anchor)?;
        // 되돌리기 검사는 앞 갱신 기준점 가운데 바탕 뒤 것만 봅니다: 바탕의 조상은 바탕 뒤 기준점이 늘 덮고, 바탕 뒤 이벤트의
        // 조상 관계는 어느 노드에서나 같습니다.
        let prev: Vec<EventId> = parent
            .anchor
            .iter()
            .filter(|a| points.place(a) == AnchorPlace::After)
            .copied()
            .collect();
        if !prev.is_empty() && !points.includes(&anchor, &prev) {
            return Some((
                Err(AuthError::Invalid("기준점이 앞 갱신의 기준점을 잇지 않음")),
                gaps,
            ));
        }
        // 넣을 때 빈 구간을 낀 상태로 통과했을 수 있으므로 판정 때도 봅니다.
        if let Err(e) = authorize_scoped(&kp.state, &self.space, header) {
            return Some((Err(e), gaps));
        }
        Some((self.check(header, f, g, id, parent, &kp), gaps))
    }

    fn check(
        &self,
        header: &EventHeader,
        f: &KeyFacts,
        g: &Gathered,
        id: EventId,
        parent: &Node,
        kp: &KeyPoint,
    ) -> Result<Node, AuthError> {
        let crew = f.crew.expect("넣을 때 확인");
        let ck = &kp.keys.crew;
        if ck.seq != crew.seq || ck.root_commit != crew.root_commit {
            return Err(AuthError::Invalid("기준점의 크루 키 사슬 끝과 다름"));
        }
        let channels: Vec<ChannelId> = kp
            .state
            .channels
            .iter()
            .filter(|(_, ch)| {
                kp.state.channel_key_group(&self.space, ch).as_ref() == Some(&self.group)
            })
            .map(|(c, _)| *c)
            .collect();
        let mut by_user: HashMap<UserId, bool> = HashMap::new();
        let mut eligible = |d: &DeviceId| -> bool {
            if !ck.members.contains(d) {
                return false;
            }
            let Some(u) = kp.keys.owners.get(d) else {
                return false;
            };
            *by_user.entry(*u).or_insert_with(|| {
                kp.state.is_member(u)
                    && channels.iter().any(|ch| {
                        kp.state
                            .channel_permissions(u, ch)
                            .contains(Permissions::VIEW_CHANNEL)
                    })
            })
        };
        let before = &parent.tip.members;
        if !parent.tip.members_known {
            // 부르는 쪽(판정)이 멤버를 모르는 바닥 다음은 판정하지 않으므로 오지 않습니다.
            return Err(AuthError::AnchorUnknown);
        }
        let after = if f.seq == 1 && f.kind == UpdateKind::Refresh {
            // 덮개 그룹의 첫 갱신: 목록 없이 멤버 = 자격 있는 크루 기기 전부.
            ck.members.iter().filter(|d| eligible(d)).copied().collect()
        } else {
            let mut required = Vec::new();
            for d in before {
                let rejoined = ck
                    .joined_at
                    .get(d)
                    .is_some_and(|j| *j > parent.tip.crew_seq);
                if rejoined || !eligible(d) {
                    required.push(*d);
                }
            }
            if g.removed.len() != required.len() || required.iter().any(|d| !g.removed.contains(d))
            {
                return Err(AuthError::Invalid("제거 목록이 자격을 잃은 기기와 다름"));
            }
            for d in &g.joined {
                if before.contains(d) {
                    return Err(AuthError::AlreadyMember);
                }
                if !eligible(d) {
                    return Err(AuthError::Invalid("자격 없는 기기를 넣음"));
                }
            }
            let mut after = before.clone();
            for d in &g.removed {
                after.remove(d);
            }
            for d in &g.joined {
                after.insert(*d);
            }
            after
        };
        g.delivers_within(&after)?;
        if g.removed.iter().any(|d| ck.holders.contains(d)) && (f.depth != 0 || g.wraps != 0) {
            return Err(AuthError::Invalid(
                "크루 표 보유 기기는 직접 인코딩으로만 뺌(크루 전체 재키가 먼저)",
            ));
        }
        let fresh = f.seq == 1
            || f.kind.is_full_rekey()
            || (f.depth == 0 && matches!(f.kind, UpdateKind::Rotate | UpdateKind::Refresh));
        if ck.last_rekey > parent.tip.fresh_crew && !fresh {
            return Err(AuthError::Invalid("크루 전체 재키 뒤에는 새 T가 먼저"));
        }
        Ok(Node {
            tip: GroupTip {
                seq: f.seq,
                head: Some(id),
                root_commit: f.root_commit,
                members: after,
                crew_seq: crew.seq,
                fresh_crew: if fresh {
                    crew.seq
                } else {
                    parent.tip.fresh_crew
                },
                members_known: true,
            },
            anchor: header.state_anchor.clone(),
            author: Some((header.user, header.author)),
            expect: None,
        })
    }
}
