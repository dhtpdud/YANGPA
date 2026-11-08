//! 체크포인트: 경계 판정, 체크포인트로 시작한 새 멤버와 전체 기록 노드의 같음, 사슬 검증, 기기 키 증명.

use yangpa_core::checkpoint::{
    CheckpointBody, CheckpointPin, DelegationCert, MerkleProof, RESOLVER_VERSION, SignedCheckpoint,
    merkle, user_value,
};
use yangpa_core::device::MemberDevices;
use yangpa_core::wire::DeviceKeysItem;

use super::*;
use crate::checkpoint::{ChainError, DeviceTree, accept_device_keys, validate_chain};

const KEY: StateKeyId = StateKeyId([1; 16]);

fn body_of(
    d: &CheckpointDraft,
    owner_devices: SignedDeviceList,
    author: DeviceId,
) -> CheckpointBody {
    CheckpointBody {
        space: SPACE,
        seq: d.seq,
        prev: d.prev,
        heads: d.heads.clone(),
        frontier_lamport: d.frontier_lamport,
        frontier: d.frontier.clone(),
        state_hash: d.state_hash,
        user_root: d.user_root,
        users: d.users,
        device_root: d.device_root,
        devices: d.devices,
        snapshot: BlobId([9; 32]),
        snapshot_len: d.snapshot.len() as u64,
        key_heads: d.key_heads.clone(),
        valid_state_keys: d.valid_state_keys.clone(),
        owner: d.owner,
        owner_devices,
        owner_proof: d.owner_proof.clone(),
        revoked: d.revoked.clone(),
        revocation_root: d.revocation_root,
        owner_chain: d.owner_chain.clone(),
        resolver_version: RESOLVER_VERSION,
        author,
        delegation: None,
        delegation_hash: d.delegation_hash,
        signer_proofs: d.signer_proofs_for(&author),
    }
}

/// 소유자 `o`(첫 기기)가 해소기 `r`의 지금 상태로 쓴 체크포인트 이벤트와 스냅숏 평문.
fn issue(w: &mut World, r: &Resolver, o: &User) -> (DecodedEvent, Vec<u8>) {
    issue_with(w, r, o, o.list(), |_| {})
}

/// [`issue`]에 소유자 기기 목록과 본문 고치기를 더한 것.
pub(super) fn issue_with(
    w: &mut World,
    r: &Resolver,
    o: &User,
    list: SignedDeviceList,
    edit: impl FnOnce(&mut CheckpointBody),
) -> (DecodedEvent, Vec<u8>) {
    let d = r.checkpoint_draft(KEY).expect("체크포인트 초안");
    let mut body = body_of(&d, list, o.dev());
    edit(&mut body);
    let sc = SignedCheckpoint::sign_owner(body, &o.identity, &o.device).unwrap();
    let e = w.ev_at(
        o.id(),
        o.dev(),
        &d.heads,
        d.lamport,
        EventBody::StateCheckpoint(Box::new(sc)),
    );
    (e, d.snapshot)
}

pub(super) fn signed(e: &DecodedEvent) -> &SignedCheckpoint {
    match &e.body {
        EventBody::StateCheckpoint(c) => c,
        _ => panic!("체크포인트가 아님"),
    }
}

fn pin(e: &DecodedEvent) -> CheckpointPin {
    let c = signed(e);
    CheckpointPin {
        seq: c.body.seq,
        id: c.id(),
    }
}

/// 소유자가 채널을 `n`개 만들어 선형 기록을 늘립니다.
pub(super) fn grow(c: &mut Chain, o: &User, from: u8, n: u8) -> Vec<EventId> {
    (0..n)
        .map(|i| c.push(o, text_channel(ch(from.wrapping_add(i)), "채널")))
        .collect()
}

fn rejected_with(r: &Resolver, id: EventId) -> Option<AuthError> {
    r.rejected()
        .iter()
        .find(|(x, _)| *x == id)
        .map(|(_, e)| e.clone())
}

/// 계획서 5단계의 갈림 사례(받은편지함 키 묶음을 부모로 둔 추방)와 초대·재가입을, 전체 기록 노드와 체크포인트로 들어온
/// 새 멤버가 똑같이 판정합니다. 쓴 초대 증명·떠난 사람의 기기 기록이 스냅숏에 없으면 새 멤버만 다르게 판정합니다.
#[test]
fn newcomer_matches_full_history() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let (carol, dave, eve) = (User::new(4), User::new(5), User::new(6));
    let admins = role(1, 10, Permissions::MANAGE_SPACE);
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: admins.clone(),
        },
    );
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![admins.id],
            nickname: None,
        },
    );
    cr.c.push(a, carol.join_body(None));
    let token = LinkToken([9; 16]);
    cr.c.push(o, invite(&token, Some(1), None, vec![]));
    let used = cr.c.push(b, dave.join_body(Some(token)));
    cr.c.push(
        b,
        EventBody::DeviceListUpdate {
            devices: b.list_with(3, vec![User::entry(&b.device)]),
        },
    );
    cr.c.push(b, EventBody::MemberLeave { user: b.id() });
    let mut full = Resolver::with_genesis(cr.genesis);
    full.insert_events(cr.c.events.clone());
    assert_applied(&full.resolved(), used);

    let (c1, snap) = issue(&mut cr.c.w, &full, o);
    let up = full.insert_events(vec![c1.clone()]);
    assert_eq!(up.checkpoints, vec![c1.id]);
    assert_eq!(full.base_snapshot().as_deref(), Some(&snap[..]));
    let mut fresh = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    assert_eq!(fresh.state(), full.state());
    assert_eq!(fresh.base(), full.base());
    assert!(fresh.len() <= full.len());

    // 뒤 기록. K는 받은편지함 이벤트(해소기 밖)라 그것을 부모로 둔 추방은 어디서나 빈 구간을 둡니다.
    let k = key_share_to(&mut cr.c.w, o, &[], &carol, Some(vec![c1.id]));
    let kick = cr.c.w.ev(
        o,
        &[k.id, c1.id],
        EventBody::MemberKick {
            user: carol.id(),
            reason: String::new(),
        },
    );
    let reuse = cr.c.w.ev(a, &[c1.id], eve.join_body(Some(token)));
    let stale = cr.c.w.ev(o, &[c1.id], b.join_body(None));
    let fresh_join = cr.c.w.ev(
        o,
        &[stale.id],
        EventBody::MemberJoin {
            member: b.member(),
            devices: b.list_with(3, vec![User::entry(&b.device)]),
            profile: None,
            invite: None,
        },
    );
    let tail = vec![
        k,
        kick.clone(),
        reuse.clone(),
        stale.clone(),
        fresh_join.clone(),
    ];
    full.insert_events(tail.clone());
    fresh.insert_events(tail.clone());
    for r in [&full, &fresh] {
        assert!(r.applied().contains(&kick.id));
        assert_eq!(rejected_with(r, reuse.id), Some(AuthError::InvalidInvite));
        assert_eq!(rejected_with(r, stale.id), Some(AuthError::StaleDeviceList));
        assert!(r.applied().contains(&fresh_join.id));
    }
    assert_eq!(fresh.state(), full.state());
    // 한 번에 해소한 것과도 같습니다(체크포인트도 같은 경계로).
    let mut all = cr.c.events.clone();
    all.push(c1.clone());
    all.extend(tail);
    let once = resolve_with(&all, &[], Some(cr.genesis));
    assert_eq!(once.state.as_ref(), full.state());
}

/// 체크포인트 경계: 판정은 이벤트 자신의 논리 시계만 봅니다. F 아래 논리 시계의 이벤트는 어디서나 거부되고(새 멤버는
/// 창 아래라 [`AuthError::BelowCheckpoint`]), F 아래 부모를 둔 F 이상 이벤트는 그 부모를 알든 모르든 똑같이
/// 적용됩니다(F 아래 부모는 경계 뒤 효과가 없음). 거부된 것은 지금 머리에 다시 서명하면 적용됩니다.
#[test]
fn late_events_below_the_frontier_are_judged_by_their_own_lamport() {
    let mut cr = crew();
    let o = &cr.o;
    let ids = grow(&mut cr.c, o, 1, 50);
    let mut full = Resolver::with_genesis(cr.genesis);
    full.insert_events(cr.c.events.clone());
    let (c1, snap) = issue(&mut cr.c.w, &full, o);
    full.insert_events(vec![c1.clone()]);
    let floor = full.floor();
    assert!(floor > 10, "경계 {floor}");
    let mut fresh = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();

    // 옛 부모(경계 아래)에 붙인 동시 이벤트와, 경계 아래 논리 시계의 이벤트.
    let old_parent = ids[2];
    let late =
        cr.c.w
            .ev(o, &[old_parent, c1.id], text_channel(ch(200), "늦음"));
    let below = cr.c.w.ev(o, &[old_parent], text_channel(ch(201), "아래"));
    assert!(late.header.lamport >= floor);
    assert!(below.header.lamport < floor);
    full.insert_events(vec![late.clone(), below.clone()]);
    assert!(full.applied().contains(&late.id));
    assert_eq!(rejected_with(&full, below.id), Some(AuthError::LateEvent));

    // 새 멤버는 옛 부모를 모르지만 같은 판정이고, 그 부모를 기다리지 않습니다(체크포인트를 계속 낼 수 있게).
    let up = fresh.insert_events(vec![late.clone(), below.clone()]);
    assert!(
        up.rejected
            .contains(&(below.id, AuthError::BelowCheckpoint))
    );
    assert!(fresh.applied().contains(&late.id));
    assert_eq!(fresh.state(), full.state());
    // 아래 부모의 논리 시계를 알려 주면 빠진 부모에서 빠집니다(데몬이 저장소에서 찾아 넣는 것).
    let parent =
        cr.c.events
            .iter()
            .find(|e| e.id == old_parent)
            .unwrap()
            .clone();
    fresh.insert(
        Vec::new(),
        vec![LinkEvent::without_body(parent.id, &parent.header)],
    );
    assert!(fresh.missing().is_empty(), "{:?}", fresh.missing());
    assert_eq!(fresh.state(), full.state());
    assert!(fresh.checkpoint_draft(KEY).is_ok());
    assert!(full.checkpoint_draft(KEY).is_ok());

    // 다시 시작한 노드(경계 아래를 버리고 창에서 시작)도 같습니다.
    let mut restarted = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    restarted.insert_events(vec![below.clone(), late.clone()]);
    assert!(restarted.applied().contains(&late.id));
    assert_eq!(restarted.state(), full.state());

    // 작성자는 거부된 것을 지금 머리에 다시 서명합니다.
    let heads = full.heads();
    assert_eq!(heads, fresh.heads());
    let again = cr.c.w.ev(o, &heads, text_channel(ch(201), "아래"));
    for r in [&mut full, &mut fresh] {
        let up = r.insert_events(vec![again.clone()]);
        assert_eq!(up.applied, vec![again.id]);
    }
    assert_eq!(fresh.state(), full.state());
}

/// 체크포인트 뒤 이벤트는 체크포인트가 은퇴시킨 상태 키를 쓸 수 없습니다(새 멤버에게는 그 키가 없음).
#[test]
fn retired_state_keys_are_rejected() {
    let mut cr = crew();
    let o = &cr.o;
    let old_key = StateKeyId([2; 16]);
    let mut e = cr.c.w.ev(o, &[cr.genesis], text_channel(ch(1), "옛 키"));
    e.header.key = KeyRef::State(old_key);
    cr.c.push_event(e);
    grow(&mut cr.c, o, 2, 40);
    let mut full = Resolver::with_genesis(cr.genesis);
    full.insert_events(cr.c.events.clone());
    let (c1, snap) = issue(&mut cr.c.w, &full, o);
    assert!(!signed(&c1).body.valid_state_keys.contains(&old_key));
    full.insert_events(vec![c1.clone()]);
    let mut fresh = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    let mut late = cr.c.w.ev(o, &[c1.id], text_channel(ch(100), "은퇴한 키"));
    late.header.key = KeyRef::State(old_key);
    let ok = cr.c.w.ev(o, &[late.id], text_channel(ch(101), "지금 키"));
    for r in [&mut full, &mut fresh] {
        r.insert_events(vec![late.clone(), ok.clone()]);
        assert_eq!(rejected_with(r, late.id), Some(AuthError::RetiredStateKey));
        assert!(r.applied().contains(&ok.id));
    }
    assert_eq!(fresh.state(), full.state());
}

/// 같은 앞 체크포인트를 잇는 둘: (논리 시계, ID)가 앞선 것이 넣는 순서와 무관하게 이기고, 진 것은 겨룬 체크포인트로
/// 한 번 알립니다(같은 기기가 둘 다 썼으면 그렇다고). 이미 지난 순번의 체크포인트는 거부됩니다.
#[test]
fn competing_and_old_checkpoints() {
    let mut cr = crew();
    let o = &cr.o;
    grow(&mut cr.c, o, 1, 6);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (early, _) = issue(&mut cr.c.w, &r, o);
    let more = cr.c.push(o, text_channel(ch(50), "더"));
    r.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    let (late, _) = issue(&mut cr.c.w, &r, o);
    assert!(signed(&late).body.heads.contains(&more));
    assert_eq!(signed(&early).body.prev, signed(&late).body.prev);

    let mut winners = Vec::new();
    for order in [[&early, &late], [&late, &early]] {
        let mut r = Resolver::with_genesis(cr.genesis);
        r.insert_events(cr.c.events.clone());
        let mut conflicts = Vec::new();
        for e in order {
            let up = r.insert_events(vec![(*e).clone()]);
            conflicts.extend(up.conflicts);
        }
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].same_author);
        assert_eq!(
            r.rejected_with_id(late.id),
            Some(AuthError::StaleCheckpoint)
        );
        winners.push((r.base().unwrap().event, r.state().cloned()));
    }
    assert_eq!(winners[0], winners[1]);
    assert_eq!(winners[0].0, early.id);

    // 다음 순번이 받아들여진 뒤 온 순번 1(같은 앞 체크포인트)은 지난 것이라 거부되고 상태는 그대로입니다.
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    r.insert_events(vec![early.clone()]);
    cr.c.head = Some(early.id);
    grow(&mut cr.c, o, 60, 3);
    r.insert_events(cr.c.events[cr.c.events.len() - 3..].to_vec());
    let (second, _) = issue(&mut cr.c.w, &r, o);
    assert_eq!(signed(&second).body.seq, 2);
    r.insert_events(vec![second.clone()]);
    assert_eq!(r.base().unwrap().event, second.id);
    let before = r.state().cloned();
    let mut stale_body = signed(&late).clone();
    stale_body.body.heads = r.heads();
    let stale = SignedCheckpoint::sign_owner(stale_body.body, &o.identity, &o.device).unwrap();
    let parents = r.heads();
    let lamport = cr.c.w.lamport_after(&parents);
    let old = cr.c.w.ev_at(
        o.id(),
        o.dev(),
        &parents,
        lamport,
        EventBody::StateCheckpoint(Box::new(stale)),
    );
    let up = r.insert_events(vec![old.clone()]);
    assert_eq!(up.rejected, vec![(old.id, AuthError::StaleCheckpoint)]);
    // 같은 순번에 받아들인 것과 다른 서명이 맞는 체크포인트는 늦게 와도 한 번 알립니다(갈라 치기 증거).
    assert_eq!(up.conflicts.len(), 1, "{:?}", up.conflicts);
    assert_eq!(up.conflicts[0].base, early.id);
    assert_eq!(up.conflicts[0].other, old.id);
    assert_eq!(r.state().cloned(), before);
    let again = r.insert_events(vec![old.clone()]);
    assert!(again.conflicts.is_empty());
}

impl Resolver {
    fn rejected_with_id(&self, id: EventId) -> Option<AuthError> {
        rejected_with(self, id)
    }
}

/// 상태가 체크포인트 본문과 다르면(해시·소유자 기기 목록·작성자) 받아들이지 않고, 새 멤버도 그런 스냅숏으로 시작하지
/// 않습니다.
#[test]
fn invalid_checkpoints_are_rejected() {
    let mut cr = crew();
    let o = &cr.o;
    grow(&mut cr.c, o, 1, 4);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    type Edit = Box<dyn FnOnce(&mut CheckpointBody)>;
    let cases: Vec<(&str, Edit, AuthError)> = vec![
        (
            "상태 해시",
            Box::new(|b| b.state_hash[0] ^= 1),
            AuthError::InvalidCheckpoint("상태 해시"),
        ),
        (
            "경계",
            Box::new(|b| {
                b.frontier_lamport = 1000;
                b.frontier.clear();
            }),
            AuthError::InvalidCheckpoint("경계"),
        ),
        (
            "경계 위 조상",
            Box::new(|b| {
                b.frontier.pop();
            }),
            AuthError::InvalidCheckpoint("경계 위 조상"),
        ),
        (
            "유효한 키",
            Box::new(|b| b.valid_state_keys = vec![StateKeyId([5; 16])]),
            AuthError::InvalidCheckpoint("확인 실패"),
        ),
    ];
    for (what, edit, want) in cases {
        let (e, _) = issue_with(&mut cr.c.w, &r, o, o.list(), edit);
        let mut r2 = r.clone();
        let up = r2.insert_events(vec![e.clone()]);
        assert!(up.checkpoints.is_empty(), "{what}");
        assert_eq!(rejected_with(&r2, e.id), Some(want), "{what}");
    }
    // 소유자가 아닌 멤버의 체크포인트.
    let a = &cr.a;
    let d = r.checkpoint_draft(KEY).unwrap();
    let mut body = body_of(&d, a.list(), a.dev());
    body.owner = a.id();
    let forged = SignedCheckpoint::sign_owner(body, &a.identity, &a.device).unwrap();
    let e = cr.c.w.ev_at(
        a.id(),
        a.dev(),
        &d.heads,
        d.lamport,
        EventBody::StateCheckpoint(Box::new(forged)),
    );
    let mut r2 = r.clone();
    r2.insert_events(vec![e.clone()]);
    assert!(r2.base().is_none());
    assert!(rejected_with(&r2, e.id).is_some());

    // 새 멤버: 스냅숏을 바꾸거나 시작 이벤트가 다르면 시작하지 않습니다.
    let (c1, mut snap) = issue(&mut cr.c.w, &r, o);
    assert!(Resolver::from_checkpoint(EventId([3; 32]), c1.clone(), &snap).is_err());
    let n = snap.len();
    snap[n - 1] ^= 1;
    assert!(Resolver::from_checkpoint(cr.genesis, c1, &snap).is_err());
}

/// 더 새 해소기 판의 체크포인트는 따르지 않고 업데이트가 필요하다고 알립니다.
#[test]
fn newer_resolver_version_asks_for_update() {
    let mut cr = crew();
    let o = &cr.o;
    grow(&mut cr.c, o, 1, 4);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (e, snap) = issue_with(&mut cr.c.w, &r, o, o.list(), |b| {
        b.resolver_version = RESOLVER_VERSION + 1
    });
    let up = r.insert_events(vec![e.clone()]);
    assert!(up.needs_update);
    assert!(r.base().is_none());
    assert_eq!(
        Resolver::from_checkpoint(cr.genesis, e.clone(), &snap).err(),
        Some(CheckpointError::NewerResolver)
    );
    assert_eq!(
        validate_chain(&pin(&e), &[signed(&e).clone()], None),
        Err(ChainError::NewerResolver)
    );
}

/// 폐기된 소유자 기기는 체크포인트를 쓸 수 없습니다(해소기와 사슬 검증 모두). 뒤 체크포인트가 폐기를 되돌린 소유자 기기
/// 목록을 실으면 사슬 검증이 거부합니다.
#[test]
fn revoked_owner_device_and_unrevoke() {
    let mut cr = crew();
    let o = &cr.o;
    let spare = SigningKeypair::from_seed(&[0x55; 32]);
    let two = o.list_with(2, vec![User::entry(&o.device), User::entry(&spare)]);
    cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: two.clone(),
        },
    );
    let mut revoked_entry = User::entry(&spare);
    revoked_entry.revoked_at = Some(Timestamp(5));
    let three = o.list_with(3, vec![User::entry(&o.device), revoked_entry]);
    cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: three.clone(),
        },
    );
    grow(&mut cr.c, o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());

    // 폐기된 기기 `spare`가 소유자 신원으로 쓴 체크포인트.
    let d = r.checkpoint_draft(KEY).unwrap();
    assert_eq!(d.revoked, vec![DeviceId(spare.public())]);
    let body = body_of(&d, three.clone(), DeviceId(spare.public()));
    let sc = SignedCheckpoint::sign_owner(body, &o.identity, &spare).unwrap();
    let e = cr.c.w.ev_at(
        o.id(),
        DeviceId(spare.public()),
        &d.heads,
        d.lamport,
        EventBody::StateCheckpoint(Box::new(sc)),
    );
    let mut r2 = r.clone();
    r2.insert_events(vec![e.clone()]);
    assert!(r2.base().is_none());
    assert_eq!(rejected_with(&r2, e.id), Some(AuthError::UnknownDevice));
    assert!(matches!(
        validate_chain(&pin(&e), &[signed(&e).clone()], None),
        Err(ChainError::Invalid { .. })
    ));

    // 정상 체크포인트 둘, 그리고 폐기를 되돌린 목록을 실은 뒤 것.
    let (c1, _) = issue_with(&mut cr.c.w, &r, o, three.clone(), |_| {});
    r.insert_events(vec![c1.clone()]);
    assert_eq!(r.base().unwrap().event, c1.id);
    cr.c.head = Some(c1.id);
    grow(&mut cr.c, o, 10, 2);
    r.insert_events(cr.c.events[cr.c.events.len() - 2..].to_vec());
    let (c2, snap2) = issue_with(&mut cr.c.w, &r, o, three.clone(), |_| {});
    let chain = vec![signed(&c1).clone(), signed(&c2).clone()];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, Some(&snap2)),
        Ok(pin(&c2))
    );

    // 소유자 신원을 가진 공격자가 쓴 뒤 체크포인트: 폐기를 되돌린 목록(판은 올림)과 그 목록에 맞춘 사용자 트리.
    let unrevoked = o.list_with(4, vec![User::entry(&o.device), User::entry(&spare)]);
    let rec = MemberDevices::from_list(&unrevoked.list);
    let leaf = merkle::leaf(merkle::USERS, o.id().as_bytes(), &user_value(Some(&rec)));
    let mut body = signed(&c2).body.clone();
    body.owner_devices = unrevoked;
    body.user_root = merkle::root(merkle::USERS, &[leaf]);
    body.users = 1;
    body.owner_proof = MerkleProof::default();
    body.author = DeviceId(spare.public());
    let forged = SignedCheckpoint::sign_owner(body, &o.identity, &spare).unwrap();
    let chain = vec![signed(&c1).clone(), forged];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, None),
        Err(ChainError::Invalid {
            seq: 2,
            reason: "소유자 기기 목록이 앞 것을 잇지 않음"
        })
    );
}

/// 사슬 검증: 고정에서 시작, 앞 체크포인트 잇기, 스냅숏 해시, 소유권 이전(첫 이전은 앞 체크포인트의 소유자 기기로).
#[test]
fn chain_validation() {
    let mut cr = crew();
    let o = &cr.o;
    let a = &cr.a;
    grow(&mut cr.c, o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &r, o);
    r.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    let moved = cr.c.push(o, transfer_at(o, a.id(), Some(signed(&c1).id())));
    grow(&mut cr.c, a, 20, 2);
    r.insert_events(cr.c.events[cr.c.events.len() - 3..].to_vec());
    assert!(r.applied().contains(&moved));
    // 소유권이 바뀐 뒤 체크포인트는 새 소유자가 씁니다.
    let (c2, snap2) = issue(&mut cr.c.w, &r, a);
    assert_eq!(signed(&c2).body.owner_chain.len(), 1);
    r.insert_events(vec![c2.clone()]);
    assert_eq!(r.base().unwrap().event, c2.id);
    let chain = vec![signed(&c1).clone(), signed(&c2).clone()];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, Some(&snap2)),
        Ok(pin(&c2))
    );
    assert_eq!(
        validate_chain(&pin(&c2), &chain, None),
        Err(ChainError::PinMismatch)
    );
    assert_eq!(validate_chain(&pin(&c1), &[], None), Err(ChainError::Empty));
    assert_eq!(
        validate_chain(&pin(&c1), &chain, Some(b"x")),
        Err(ChainError::Snapshot)
    );
    // 순서가 뒤바뀌거나 빠진 사슬.
    let swapped = vec![signed(&c1).clone(), signed(&c1).clone()];
    assert!(matches!(
        validate_chain(&pin(&c1), &swapped, None),
        Err(ChainError::Invalid { seq: 1, .. })
    ));
    // 새 멤버는 마지막 체크포인트로 시작해 같은 상태입니다.
    let fresh = Resolver::from_checkpoint(cr.genesis, c2.clone(), &snap2).unwrap();
    assert_eq!(fresh.state(), r.state());

    // 바탕 뒤 첫 이전은 앞 체크포인트의 소유자 기기 목록에서 활성이던 기기만 씁니다.
    let mut cr2 = crew();
    let o2 = &cr2.o;
    grow(&mut cr2.c, o2, 1, 2);
    let mut r = Resolver::with_genesis(cr2.genesis);
    r.insert_events(cr2.c.events.clone());
    let (b1, _) = issue(&mut cr2.c.w, &r, o2);
    r.insert_events(vec![b1.clone()]);
    cr2.c.head = Some(b1.id);
    let newdev = SigningKeypair::from_seed(&[0x66; 32]);
    cr2.c.push(
        o2,
        EventBody::DeviceListUpdate {
            devices: o2.list_with(2, vec![User::entry(&o2.device), User::entry(&newdev)]),
        },
    );
    // crew()의 소유자는 User::new(1)이라 같은 시드로 신원 키를 다시 만듭니다.
    let via_new = User {
        identity: SigningKeypair::from_seed(&[1; 32]),
        device: newdev,
    };
    let t = cr2.c.push(
        &via_new,
        transfer_at(&via_new, cr2.a.id(), Some(signed(&b1).id())),
    );
    r.insert_events(cr2.c.events[cr2.c.events.len() - 2..].to_vec());
    assert_eq!(
        rejected_with(&r, t),
        Some(AuthError::Invalid("앞 체크포인트에 없던 소유자 기기"))
    );
}

/// 위임: 소유자가 위임한 기기가 소유자 신원 서명 없이 체크포인트를 씁니다. 소유권이 바뀌면 위임은 사라집니다.
#[test]
fn delegated_checkpoints() {
    let mut cr = crew();
    let o = &cr.o;
    let a = &cr.a;
    let cert = DelegationCert::create(SPACE, a.dev(), 10, &o.identity, &o.device);
    cr.c.push(o, EventBody::CheckpointDelegation(Box::new(cert.clone())));
    grow(&mut cr.c, o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let d = r.checkpoint_draft(KEY).unwrap();
    let mut body = body_of(&d, o.list(), a.dev());
    body.delegation = Some(cert.clone());
    let sc = SignedCheckpoint::sign_delegated(body, &a.device).unwrap();
    let e = cr.c.w.ev_at(
        a.id(),
        a.dev(),
        &d.heads,
        d.lamport,
        EventBody::StateCheckpoint(Box::new(sc)),
    );
    let mut ok = r.clone();
    ok.insert_events(vec![e.clone()]);
    assert_eq!(ok.base().unwrap().event, e.id);
    assert_eq!(
        validate_chain(&pin(&e), &[signed(&e).clone()], None),
        Ok(pin(&e))
    );

    // 소유권을 넘긴 뒤에는 옛 위임으로 쓸 수 없습니다.
    cr.c.push(o, transfer(o, cr.b.id()));
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let d = r.checkpoint_draft(KEY).unwrap();
    let b = &cr.b;
    let mut body2 = body_of(&d, b.list(), a.dev());
    body2.delegation = Some(DelegationCert::create(
        SPACE,
        a.dev(),
        10,
        &b.identity,
        &b.device,
    ));
    let sc = SignedCheckpoint::sign_delegated(body2, &a.device).unwrap();
    let e = cr.c.w.ev_at(
        a.id(),
        a.dev(),
        &d.heads,
        d.lamport,
        EventBody::StateCheckpoint(Box::new(sc)),
    );
    r.insert_events(vec![e.clone()]);
    assert!(r.base().is_none());
    assert_eq!(
        rejected_with(&r, e.id),
        Some(AuthError::InvalidCheckpoint("상태에 없는 위임"))
    );
}

/// 체크포인트가 둘 쌓이면 앞 바탕 앞의 항목을 RAM에서 버리고, 결과는 처음부터 해소한 것과 같습니다. 창 앞을 건드리는
/// 늦은 이벤트도 같은 판정입니다.
#[test]
fn compaction_keeps_results() {
    let mut cr = crew();
    let o = &cr.o;
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let mut checkpoints = Vec::new();
    for round in 0..4u8 {
        let before = cr.c.events.len();
        grow(&mut cr.c, o, round * 50, 45);
        r.insert_events(cr.c.events[before..].to_vec());
        let (c, _) = issue(&mut cr.c.w, &r, o);
        let up = r.insert_events(vec![c.clone()]);
        assert_eq!(up.checkpoints, vec![c.id]);
        cr.c.head = Some(c.id);
        cr.c.events.push(c.clone());
        checkpoints.push(c);
        r.check_log();
    }
    assert!(r.stats().compacted > 100, "{:?}", r.stats());
    assert!(r.len() < 120, "항목 {}", r.len());
    let once = resolve_with(&cr.c.events, &[], Some(cr.genesis));
    assert_eq!(once.state.as_ref(), r.state());
    // 섞어 넣어도 같습니다.
    let mut rng = Rng(7);
    let mut shuffled = cr.c.events.clone();
    rng.shuffle(&mut shuffled);
    let mut s = Resolver::with_genesis(cr.genesis);
    for chunk in shuffled.chunks(7) {
        s.insert_events(chunk.to_vec());
        s.check_log();
    }
    assert_eq!(s.state(), r.state());
    assert_eq!(s.base(), r.base());
    // 창 앞(버린 구간)의 옛 이벤트가 늦게 와도 효과가 없습니다.
    let late =
        cr.c.w
            .ev(&cr.a, &[cr.genesis], text_channel(ch(250), "아주 늦음"));
    let up = r.insert_events(vec![late.clone()]);
    assert_eq!(up.rejected, vec![(late.id, AuthError::BelowCheckpoint)]);
    assert_eq!(once.state.as_ref(), r.state());
}

/// `GetDeviceKeys`: 상태의 키 약속이나 체크포인트 기기 트리의 잎 증명이 맞는 키만 받습니다.
#[test]
fn device_key_proofs() {
    let mut cr = crew();
    let o = &cr.o;
    grow(&mut cr.c, o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &r, o);
    r.insert_events(vec![c1.clone()]);
    let proofs = r.device_proofs().unwrap();
    let tree = DeviceTree::of(signed(&c1));
    assert_eq!(proofs.tree, tree);
    let entry = User::entry(&cr.a.device);
    let genuine = DeviceKeysItem {
        keys: entry.keys(),
        proof: proofs.proof(&cr.a.dev()),
    };
    assert!(accept_device_keys(&genuine, None, Some(&tree)));
    // 상자 키를 바꿔 끼운 답.
    let mut forged = genuine.clone();
    forged.keys.box_key = yangpa_core::device::BoxPublicKey::filled(7);
    assert!(!accept_device_keys(&forged, None, Some(&tree)));
    // 다른 기기의 증명을 붙인 답.
    let mut other = genuine.clone();
    other.proof = proofs.proof(&cr.b.dev());
    assert!(!accept_device_keys(&other, None, Some(&tree)));
    // 증명 없이 오면 상태의 키 약속으로만.
    let bare = DeviceKeysItem {
        keys: entry.keys(),
        proof: None,
    };
    assert!(!accept_device_keys(&bare, None, Some(&tree)));
    let rec = entry.record();
    assert!(accept_device_keys(&bare, Some(&rec), None));
    assert!(!accept_device_keys(&forged, Some(&rec), None));
    assert!(proofs.proof(&User::new(77).dev()).is_none());
}

/// 확정 기기의 크루 갱신이 체크포인트 경계 아래로 늦으면(체크포인트를 쓴 기기가 몰랐음) 모든 노드가 늦은 이벤트로
/// 거부하고 크루 사슬은 체크포인트의 머리에 머뭅니다. 확정 기기는 그 갱신을 그대로 다시 서명하지 않고 사슬 끝에서 새
/// 갱신(새 비밀, 새 약속)을 냅니다(데몬은 저장 칸의 앞 상태에서 다시 냄): 그 사이 사슬이 나아갔으면 옛 갱신을 지금
/// 머리에 다시 서명한 것은 순번이 겹쳐 거부됩니다.
#[test]
fn committer_late_key_update_is_reissued_on_the_chain() {
    let mut cr = crew();
    let o = &cr.o;
    let mut all = vec![cr.o.dev(), cr.a.dev(), cr.b.dev()];
    all.sort();
    let k1 = cr.c.push(
        o,
        EventBody::KeyUpdate(crew_update(1, None, KeyUpdateKind::Advance { joined: all })),
    );
    grow(&mut cr.c, o, 1, 50);
    let mut full = Resolver::with_genesis(cr.genesis);
    full.insert_events(cr.c.events.clone());
    assert_eq!(full.key_view().unwrap().crew.seq, 1);
    let (c1, snap) = issue(&mut cr.c.w, &full, o);
    full.insert_events(vec![c1.clone()]);
    let floor = full.floor();
    let mut fresh = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    // 체크포인트와 동시에 k1 바로 뒤에 낸 순번 2(경계 아래 논리 시계).
    let late = cr.c.w.ev(
        o,
        &[k1],
        EventBody::KeyUpdate(crew_update(2, Some(k1), KeyUpdateKind::Refresh)),
    );
    assert!(late.header.lamport < floor);
    for r in [&mut full, &mut fresh] {
        r.insert_events(vec![late.clone()]);
        assert!(!r.applied().contains(&late.id));
        assert_eq!(r.key_view().unwrap().crew.seq, 1);
    }
    assert_eq!(rejected_with(&full, late.id), Some(AuthError::LateEvent));
    // 사슬 끝(순번 1) 위에 새로 낸 갱신.
    let heads = full.heads();
    assert_eq!(heads, fresh.heads());
    let mut p = crew_update(2, Some(k1), KeyUpdateKind::Refresh);
    p.root_commit = [42; 32];
    p.salt = [1; 24];
    let again = cr.c.w.ev(o, &heads, EventBody::KeyUpdate(p));
    for r in [&mut full, &mut fresh] {
        let up = r.insert_events(vec![again.clone()]);
        assert_eq!(up.applied, vec![again.id]);
        let kv = r.key_view().unwrap();
        assert_eq!((kv.crew.seq, kv.crew.root_commit), (2, [42; 32]));
    }
    // 옛 갱신을 지금 머리에 그대로 다시 서명하면 사슬이 이미 나아가 거부됩니다.
    let heads = full.heads();
    let resigned = cr.c.w.ev(o, &heads, late.body.clone());
    for r in [&mut full, &mut fresh] {
        r.insert_events(vec![resigned.clone()]);
        assert!(!r.applied().contains(&resigned.id));
        assert_eq!(r.key_view().unwrap().crew.root_commit, [42; 32]);
    }
    assert_eq!(fresh.state(), full.state());
}

// ---- 속성 검사: 체크포인트가 섞인 무작위 기록 ----

/// 무작위 기록(`churn`)에서 끝내 오지 않는 부모·순서 연결 전용 항목을 빼고, 기록의 1/3·2/3 지점에서 그때 소유자가 쓴
/// 체크포인트를 넣습니다(첫 지점에는 가끔 조금 앞 기록으로 쓴 경쟁 체크포인트도). 뒤 이벤트는 체크포인트를 부모로 두지
/// 않아 경계(늦은 이벤트·경계 아래)를 많이 타고, 끝에 아주 옛 부모를 둔 늦은 이벤트와 논리 시계를 낮게 지어낸 이벤트를
/// 더합니다. 돌려주는 체크포인트와 스냅숏은 같은 차례입니다.
fn checkpointed(seed: u64, n: usize) -> (Churn, Vec<DecodedEvent>, Vec<Vec<u8>>) {
    let mut c = churn(seed, n);
    let known: HashSet<EventId> = c
        .events
        .iter()
        .filter(|e| e.header.scope.is_none())
        .map(|e| e.id)
        .collect();
    for e in &mut c.events {
        e.header.parents.retain(|p| known.contains(p));
    }
    c.links.clear();
    let users = churn_users();
    let mut w = World::new();
    w.n = 1_000_000 + seed % 1000;
    for e in &c.events {
        w.lamports.insert(e.id, e.header.lamport);
    }
    let mut rng = Rng(seed ^ 0x00c0_ffee);
    let mut cps: Vec<DecodedEvent> = Vec::new();
    let mut snaps = Vec::new();
    let len = c.events.len();
    let issue_at = |w: &mut World, prefix: &[DecodedEvent], cps: &[DecodedEvent]| {
        let mut r = Resolver::with_genesis(c.events[0].id);
        let mut evs: Vec<DecodedEvent> = prefix.to_vec();
        evs.extend(cps.iter().cloned());
        r.insert_events(evs);
        let d = r.checkpoint_draft(KEY).ok()?;
        let owner = users.iter().find(|u| u.id() == d.owner)?;
        if !d.owner_record.contains_active(&owner.dev()) {
            return None;
        }
        // 상태의 소유자 기록과 같은 서명된 목록(기록에 실린 것).
        let list = c.events.iter().find_map(|e| match &e.body {
            EventBody::DeviceListUpdate { devices, .. } | EventBody::MemberJoin { devices, .. }
                if MemberDevices::from_list(&devices.list) == *d.owner_record =>
            {
                Some(devices.clone())
            }
            EventBody::SpaceCreate { devices, .. } => devices
                .iter()
                .find(|l| MemberDevices::from_list(&l.list) == *d.owner_record)
                .cloned(),
            _ => None,
        })?;
        let sc = SignedCheckpoint::sign_owner(
            body_of(&d, list, owner.dev()),
            &owner.identity,
            &owner.device,
        )
        .unwrap();
        let e = w.ev_at(
            owner.id(),
            owner.dev(),
            &d.heads,
            d.lamport,
            EventBody::StateCheckpoint(Box::new(sc)),
        );
        Some((e, d.snapshot))
    };
    for (k, cut) in [len / 3, 2 * len / 3].into_iter().enumerate() {
        let Some((e, snap)) = issue_at(&mut w, &c.events[..cut], &cps) else {
            continue;
        };
        let rival = (k == 0 && cut > 4 && rng.below(2) == 0)
            .then(|| issue_at(&mut w, &c.events[..cut - 1 - rng.below(3)], &cps))
            .flatten();
        cps.push(e);
        snaps.push(snap);
        if let Some((e, snap)) = rival {
            cps.push(e);
            snaps.push(snap);
        }
    }
    c.events.extend(cps.iter().cloned());
    let ids: Vec<EventId> = c.events.iter().map(|e| e.id).collect();
    for _ in 0..rng.below(4) {
        let old = ids[rng.below(len / 4 + 1)];
        let recent = ids[ids.len() - 1 - rng.below(ids.len().min(6))];
        let author = &users[rng.below(4)];
        let body = text_channel(ch(200 + rng.below(50) as u8), "늦음");
        let e = if rng.below(2) == 0 {
            w.ev(author, &[old, recent], body)
        } else {
            let lamport = 1 + rng.below(4) as u64;
            w.ev_at(author.id(), author.dev(), &[recent], lamport, body)
        };
        c.events.push(e);
    }
    (c, cps, snaps)
}

/// 해소기 상태 전체(정규 인코딩)의 해시. `SpaceState`에 없는 떠난 멤버·은퇴한 초대·지운 것까지 견줍니다.
fn digest(r: &Resolver) -> Option<[u8; 32]> {
    r.machine.as_ref().map(|m| m.digest().state_hash)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// 체크포인트가 섞여도 증분 해소기(이어 붙이기·재생·압축)가 늘 처음부터 다시 해소하는 해소기와 같은 변화를 알리고,
    /// 끝 상태는 한 번에 해소한 것과 같습니다. 마지막 체크포인트로 시작한 새 멤버도 나머지를 받으면 같은 상태입니다.
    #[test]
    fn checkpoints_resolve_consistently(seed in any::<u64>(), n in 20usize..120, order_seed in any::<u64>()) {
        let (c, cps, snaps) = checkpointed(seed, n);
        let genesis = c.events[0].id;
        let scratch = resolve_with(&c.events, &[], Some(genesis));
        let mut rng = Rng(order_seed);
        let mut fast = Resolver::with_genesis(genesis);
        let mut slow = Resolver::with_genesis(genesis);
        for (evs, lns) in churn_batches(&c, &mut rng) {
            let a = fast.insert(evs.clone(), lns.clone());
            let b = with_flag(&NO_REPLAY, || slow.insert(evs, lns));
            prop_assert_eq!(a, b);
            prop_assert_eq!(fast.state(), slow.state());
            fast.check_log();
        }
        prop_assert_eq!(fast.state(), scratch.state.as_ref());
        prop_assert_eq!(slow.state(), scratch.state.as_ref());
        prop_assert_eq!(fast.base(), slow.base());
        prop_assert_eq!(fast.heads(), slow.heads());
        prop_assert_eq!(digest(&fast), digest(&slow));
        let mut whole = Resolver::with_genesis(genesis);
        whole.insert_events(c.events.clone());
        prop_assert_eq!(whole.base(), fast.base());
        prop_assert_eq!(digest(&whole), digest(&fast));
        // 받아들인 마지막 체크포인트로 시작한 새 멤버와 재시작한 노드: 데몬처럼 경계 이상 이벤트만 받습니다.
        if let Some(base) = fast.base() {
            let k = cps.iter().position(|e| e.id == base.event).unwrap();
            let mut fresh = Resolver::from_checkpoint(genesis, cps[k].clone(), &snaps[k]).unwrap();
            let floor = fresh.floor();
            let mut rest: Vec<DecodedEvent> = c
                .events
                .iter()
                .filter(|e| e.id != base.event && e.header.lamport >= floor)
                .cloned()
                .collect();
            let mut restarted = Resolver::from_checkpoint(genesis, cps[k].clone(), &snaps[k]).unwrap();
            restarted.insert_events(rest.clone());
            rng.shuffle(&mut rest);
            for chunk in rest.chunks(5) {
                fresh.insert_events(chunk.to_vec());
            }
            prop_assert_eq!(fresh.state(), fast.state());
            prop_assert_eq!(fresh.base(), fast.base());
            prop_assert_eq!(digest(&fresh), digest(&fast));
            prop_assert_eq!(restarted.base(), fast.base());
            prop_assert_eq!(digest(&restarted), digest(&fast));
            // 새 멤버가 아는 기준점의 상태는 전체 기록 노드와 같습니다(바탕과 동시인 것 포함).
            let ready = |r: &Resolver, id: EventId| match r.state_at(&[id], true) {
                AnchorState::Ready { state, .. } => Some(state),
                AnchorState::Incomplete { .. } => None,
            };
            for e in rest.iter().take(8) {
                if let Some(mine) = ready(&fresh, e.id) {
                    prop_assert_eq!(Some(mine), ready(&fast, e.id), "기준점 {:?}", e.id);
                }
            }
        }
    }
}

#[test]
fn checkpointed_scenarios_accept_checkpoints() {
    // 생성기가 체크포인트를 실제로 받아들이고, 경쟁 체크포인트와 경계 아래·옛 부모 이벤트를 만드는지(속성 검사가
    // 헛돌지 않게).
    let (mut accepted, mut rivals, mut late, mut old_parent) = (0, 0, 0, 0);
    // 크루 키 사건(KeyUpdate·KeyBackupSet)이 체크포인트 앞(스냅숏의 키 상태로)과 뒤(새 멤버가 이어 적용) 모두에서
    // 받아들여지고, 확정 기기 바꾸기·멤버 제거도 섞이는지.
    let (mut key_before, mut key_after, mut committer, mut removals) = (0, 0, 0, 0);
    for seed in 0..20u64 {
        let (c, cps, _) = checkpointed(seed, 90);
        assert!(cps.len() <= 3);
        rivals += usize::from(cps.len() == 3);
        let mut res = Resolver::with_genesis(c.events[0].id);
        let up = res.insert_events(c.events.clone());
        accepted += up.checkpoints.len();
        late += res
            .rejected()
            .iter()
            .filter(|(_, e)| *e == AuthError::LateEvent)
            .count();
        let floor = res.floor();
        let lamports: HashMap<EventId, u64> =
            c.events.iter().map(|e| (e.id, e.header.lamport)).collect();
        let by_id: HashMap<EventId, &DecodedEvent> = c.events.iter().map(|e| (e.id, e)).collect();
        // 체크포인트 앞 사건은 압축으로 해소 결과에서 빠지므로 체크포인트 없이 전체 기록으로 셉니다.
        let mut full = Resolver::with_genesis(c.events[0].id);
        full.insert_events(
            c.events
                .iter()
                .filter(|e| !matches!(e.body, EventBody::StateCheckpoint(_)))
                .cloned()
                .collect(),
        );
        let after: HashSet<EventId> = res.resolved().applied.into_iter().collect();
        for id in full.resolved().applied {
            let Some(e) = by_id.get(&id) else { continue };
            match &e.body {
                EventBody::KeyUpdate(_) | EventBody::KeyBackupSet { .. } => {
                    if e.header.lamport < floor {
                        key_before += 1;
                    } else if after.contains(&id) {
                        key_after += 1;
                    }
                }
                EventBody::CommitterChange { .. } | EventBody::OwnershipTransfer { .. } => {
                    committer += 1
                }
                EventBody::MemberKick { .. }
                | EventBody::MemberBan { .. }
                | EventBody::MemberLeave { .. } => removals += 1,
                _ => {}
            }
        }
        old_parent += c
            .events
            .iter()
            .filter(|e| {
                e.header.lamport >= floor
                    && e.header
                        .parents
                        .iter()
                        .any(|p| lamports.get(p).is_some_and(|l| *l < floor))
            })
            .count();
    }
    assert!(accepted >= 15, "받아들인 체크포인트 {accepted}");
    assert!(rivals >= 3, "경쟁 체크포인트 {rivals}");
    assert!(late >= 5, "경계 아래 이벤트 {late}");
    assert!(old_parent >= 5, "옛 부모를 둔 이벤트 {old_parent}");
    assert!(key_before >= 10, "체크포인트 앞 키 사건 {key_before}");
    assert!(key_after >= 10, "체크포인트 뒤 키 사건 {key_after}");
    assert!(committer >= 5, "확정 기기 바꾸기 {committer}");
    assert!(removals >= 10, "멤버 제거 {removals}");
}

mod regressions;
