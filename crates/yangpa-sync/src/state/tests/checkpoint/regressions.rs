//! 체크포인트 검토에서 찾은 갈림·위조 사례. 저마다 고치기 전에는 노드가 갈리거나 가짜 사슬이 통했습니다.

use yangpa_core::checkpoint::revocation_root;
use yangpa_core::device::DeviceList;

use super::*;
use crate::checkpoint::{HeadAgreement, compare_heads};

/// 소유자 `o`가 앞 체크포인트 뒤 기록을 `n`개 늘리고 그 위에 다음 체크포인트를 씁니다.
fn next_checkpoint(
    cr: &mut Crew,
    r: &mut Resolver,
    o: &User,
    from: u8,
    n: u8,
) -> (DecodedEvent, Vec<u8>) {
    let start = cr.c.events.len();
    grow(&mut cr.c, o, from, n);
    r.insert_events(cr.c.events[start..].to_vec());
    let (c, snap) = issue(&mut cr.c.w, r, o);
    r.insert_events(vec![c.clone()]);
    assert_eq!(r.base().unwrap().event, c.id);
    cr.c.head = Some(c.id);
    (c, snap)
}

/// 소유자 신원 키만 가진 사람(폐기된 기기, 새어 나간 복구 문구)이 새 기기를 지어내 소유자 기기 목록에 더하고 그 기기로
/// 쓴 체크포인트는 새 멤버의 사슬 검증도 받지 않습니다(쓴 기기는 앞 체크포인트의 목록에서 활성이어야 함).
#[test]
fn identity_key_alone_cannot_mint_a_signing_owner_device() {
    let mut cr = crew();
    let o = User::new(1);
    grow(&mut cr.c, &o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &r, &o);
    r.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    grow(&mut cr.c, &o, 10, 2);
    r.insert_events(cr.c.events[cr.c.events.len() - 2..].to_vec());

    let evil = SigningKeypair::from_seed(&[0xEE; 32]);
    let evil_id = DeviceId(evil.public());
    let mut fake = r.clone();
    let fake_dlu = cr.c.w.ev(
        &o,
        &r.heads(),
        EventBody::DeviceListUpdate {
            devices: o.list_with(2, vec![User::entry(&o.device), User::entry(&evil)]),
        },
    );
    let fake_kick = cr.c.w.ev(
        &o,
        &[fake_dlu.id],
        EventBody::MemberKick {
            user: cr.a.id(),
            reason: String::new(),
        },
    );
    fake.insert_events(vec![fake_dlu, fake_kick]);
    let d = fake.checkpoint_draft(KEY).unwrap();
    let evil_signed = DeviceList {
        user: o.id(),
        version: 2,
        devices: vec![User::entry(&o.device), User::entry(&evil)],
        otp: None,
    }
    .sign_with(&o.identity, &evil)
    .unwrap();
    let sc = SignedCheckpoint::sign_owner(body_of(&d, evil_signed, evil_id), &o.identity, &evil)
        .unwrap();
    let forged = cr.c.w.ev_at(
        o.id(),
        evil_id,
        &d.heads,
        d.lamport,
        EventBody::StateCheckpoint(Box::new(sc)),
    );
    let chain = vec![signed(&c1).clone(), signed(&forged).clone()];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, Some(&d.snapshot)),
        Err(ChainError::Invalid {
            seq: 2,
            reason: "앞 체크포인트 소유자의 활성 기기가 쓰지 않음"
        })
    );
    let mut full = r.clone();
    full.insert_events(vec![forged]);
    assert_eq!(full.base().unwrap().event, c1.id);
}

/// 위임이 다른 기기로 바뀌고 옛 위임을 받은 멤버가 쫓겨난 뒤, 옛 위임 인증서로 쓴 체크포인트는 사슬 검증도 받지 않습니다
/// (앞 체크포인트가 밝힌 위임만). 지금 위임받은 기기의 체크포인트는 해소기와 사슬 검증 모두 받습니다.
#[test]
fn stale_delegation_cannot_sign_for_newcomers() {
    let mut cr = crew();
    let (o, a, b) = (User::new(1), User::new(2), User::new(3));
    let cert_a = DelegationCert::create(SPACE, a.dev(), 1000, &o.identity, &o.device);
    cr.c.push(
        &o,
        EventBody::CheckpointDelegation(Box::new(cert_a.clone())),
    );
    let cert_b = DelegationCert::create(SPACE, b.dev(), 1000, &o.identity, &o.device);
    cr.c.push(
        &o,
        EventBody::CheckpointDelegation(Box::new(cert_b.clone())),
    );
    cr.c.push(
        &o,
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    grow(&mut cr.c, &o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    assert_eq!(r.delegation(), Some(cert_b.clone()));
    let (c1, _) = issue(&mut cr.c.w, &r, &o);
    assert_eq!(signed(&c1).body.delegation_hash, Some(cert_b.hash()));
    r.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    grow(&mut cr.c, &o, 10, 2);
    r.insert_events(cr.c.events[cr.c.events.len() - 2..].to_vec());
    let d = r.checkpoint_draft(KEY).unwrap();
    let delegated = |w: &mut World, who: &User, cert: &DelegationCert| {
        let mut body = body_of(&d, o.list(), who.dev());
        body.delegation = Some(cert.clone());
        let sc = SignedCheckpoint::sign_delegated(body, &who.device).unwrap();
        w.ev_at(
            who.id(),
            who.dev(),
            &d.heads,
            d.lamport,
            EventBody::StateCheckpoint(Box::new(sc)),
        )
    };
    let forged = delegated(&mut cr.c.w, &a, &cert_a);
    let chain = vec![signed(&c1).clone(), signed(&forged).clone()];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, Some(&d.snapshot)),
        Err(ChainError::Invalid {
            seq: 2,
            reason: "앞 체크포인트가 밝히지 않은 위임"
        })
    );
    let mut full = r.clone();
    full.insert_events(vec![forged.clone()]);
    assert_eq!(full.base().unwrap().event, c1.id);
    assert!(rejected_with(&full, forged.id).is_some());

    let honest = delegated(&mut cr.c.w, &b, &cert_b);
    let chain = vec![signed(&c1).clone(), signed(&honest).clone()];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, Some(&d.snapshot)),
        Ok(pin(&honest))
    );
    full.insert_events(vec![honest.clone()]);
    assert_eq!(full.base().unwrap().event, honest.id);
}

/// 소유권이 o→a→o로 돌아온 뒤 a가 옛 인증서(o→a)를 다시 실은 체크포인트는 사슬 검증이 받지 않습니다(인증서는 쓴 때의
/// 바탕 체크포인트에 묶임). 정직한 이전 사슬은 받습니다.
#[test]
fn ownership_certificates_cannot_be_replayed() {
    let mut cr = crew();
    let (o, a) = (User::new(1), User::new(2));
    grow(&mut cr.c, &o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &r, &o);
    r.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    let n = cr.c.events.len();
    cr.c.push(&o, transfer_at(&o, a.id(), Some(signed(&c1).id())));
    grow(&mut cr.c, &a, 20, 2);
    r.insert_events(cr.c.events[n..].to_vec());
    let (c2, _) = issue(&mut cr.c.w, &r, &a);
    r.insert_events(vec![c2.clone()]);
    assert_eq!(r.base().unwrap().event, c2.id);
    cr.c.head = Some(c2.id);
    let n = cr.c.events.len();
    // 인증서가 다른 바탕에 묶였으면 해소기가 받지 않습니다.
    let stale =
        cr.c.push(&a, transfer_at(&a, o.id(), Some(signed(&c1).id())));
    cr.c.push(&a, transfer_at(&a, o.id(), Some(signed(&c2).id())));
    grow(&mut cr.c, &o, 30, 2);
    r.insert_events(cr.c.events[n..].to_vec());
    assert!(matches!(
        rejected_with(&r, stale),
        Some(AuthError::Invalid(_))
    ));
    assert_eq!(r.state().unwrap().owner, o.id());
    let (c3, _) = issue(&mut cr.c.w, &r, &o);
    r.insert_events(vec![c3.clone()]);
    assert_eq!(r.base().unwrap().event, c3.id);
    let honest = vec![
        signed(&c1).clone(),
        signed(&c2).clone(),
        signed(&c3).clone(),
    ];
    assert_eq!(validate_chain(&pin(&c1), &honest, None), Ok(pin(&c3)));

    let old_cert = signed(&c2).body.owner_chain[0].clone();
    assert_eq!((old_cert.from, old_cert.to), (o.id(), a.id()));
    let rec = MemberDevices::from_list(&a.list().list);
    let leaf = merkle::leaf(merkle::USERS, a.id().as_bytes(), &user_value(Some(&rec)));
    let p = &signed(&c3).body;
    let mut body = p.clone();
    body.seq = 4;
    body.prev = Some(signed(&c3).id());
    body.owner = a.id();
    body.owner_devices = a.list();
    body.user_root = merkle::root(merkle::USERS, &[leaf]);
    body.users = 1;
    body.owner_proof = MerkleProof::default();
    body.revoked = vec![];
    body.revocation_root = revocation_root(&p.revocation_root, 4, &[]);
    body.owner_chain = vec![old_cert];
    body.author = a.dev();
    let forged = SignedCheckpoint::sign_owner(body, &a.identity, &a.device).unwrap();
    let mut chain = honest;
    chain.push(forged);
    assert_eq!(
        validate_chain(&pin(&c1), &chain, None),
        Err(ChainError::Invalid {
            seq: 4,
            reason: "앞 체크포인트에 묶이지 않은 소유권 인증서"
        })
    );
}

/// 앞 체크포인트 창 앞에 폐기된 새 소유자 기기는 폐기를 되돌린 다음 체크포인트를 쓸 수 없습니다(쓴 기기는 앞
/// 체크포인트 기기 트리의 잎 증명으로 활성이어야 함).
#[test]
fn revoked_device_of_the_new_owner_cannot_sign() {
    let mut cr = crew();
    let (o, a) = (User::new(1), User::new(2));
    let spare = SigningKeypair::from_seed(&[0x5a; 32]);
    cr.c.push(
        &a,
        EventBody::DeviceListUpdate {
            devices: a.list_with(2, vec![User::entry(&a.device), User::entry(&spare)]),
        },
    );
    let mut rev = User::entry(&spare);
    rev.revoked_at = Some(Timestamp(5));
    let a_list = a.list_with(3, vec![User::entry(&a.device), rev]);
    cr.c.push(
        &a,
        EventBody::DeviceListUpdate {
            devices: a_list.clone(),
        },
    );
    grow(&mut cr.c, &o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c0, _) = issue(&mut cr.c.w, &r, &o);
    assert!(signed(&c0).body.revoked.contains(&DeviceId(spare.public())));
    r.insert_events(vec![c0.clone()]);
    cr.c.head = Some(c0.id);
    let (c1, _) = next_checkpoint(&mut cr, &mut r, &o, 10, 3);
    assert!(signed(&c1).body.revoked.is_empty());
    let n = cr.c.events.len();
    cr.c.push(&o, transfer_at(&o, a.id(), Some(signed(&c1).id())));
    r.insert_events(cr.c.events[n..].to_vec());
    let d = r.checkpoint_draft(KEY).unwrap();
    assert_eq!(d.owner, a.id());

    // 정직한 새 소유자 기기의 체크포인트는 받습니다.
    let (c2, snap2) = issue_with(&mut cr.c.w, &r, &a, a_list.clone(), |_| {});
    assert_eq!(
        validate_chain(
            &pin(&c1),
            &[signed(&c1).clone(), signed(&c2).clone()],
            Some(&snap2)
        ),
        Ok(pin(&c2))
    );
    let forged_list = a.list_by(9, vec![User::entry(&a.device), User::entry(&spare)], &spare);
    let rec = MemberDevices::from_list(&forged_list.list);
    let leaf = merkle::leaf(merkle::USERS, a.id().as_bytes(), &user_value(Some(&rec)));
    let mut body = body_of(&d, forged_list, DeviceId(spare.public()));
    body.user_root = merkle::root(merkle::USERS, &[leaf]);
    body.users = 1;
    body.owner_proof = MerkleProof::default();
    let forged = SignedCheckpoint::sign_owner(body, &a.identity, &spare).unwrap();
    let chain = vec![signed(&c1).clone(), forged];
    assert_eq!(
        validate_chain(&pin(&c1), &chain, None),
        Err(ChainError::Invalid {
            seq: 3,
            reason: "새 소유자 기기의 증명"
        })
    );
}

/// 같은 앞 체크포인트를 잇는 둘 가운데 논리 시계가 작은 것이 다음 체크포인트 뒤에 와도, 이미 다음 것을 받아 압축한
/// 노드·아직인 노드·처음부터 해소한 노드가 같은 바탕을 따릅니다(뒤 체크포인트가 이어 받는 쪽이 이김). 진 것은 경고로
/// 알립니다.
#[test]
fn late_competing_checkpoint_does_not_split_nodes() {
    let mut cr = crew();
    let o = User::new(1);
    let ids = grow(&mut cr.c, &o, 1, 40);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (early, _) = issue(&mut cr.c.w, &r, &o);
    cr.c.push(&o, text_channel(ch(90), "더"));
    r.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    let (late, _) = issue(&mut cr.c.w, &r, &o);
    assert!(early.header.lamport < late.header.lamport);
    let mut x = Resolver::with_genesis(cr.genesis);
    x.insert_events(cr.c.events.clone());
    x.insert_events(vec![late.clone()]);
    assert_eq!(x.base().unwrap().event, late.id);
    let mut y = x.clone();
    cr.c.head = Some(late.id);
    let n = cr.c.events.len();
    grow(&mut cr.c, &o, 100, 45);
    let tail: Vec<DecodedEvent> = cr.c.events[n..].to_vec();
    x.insert_events(tail.clone());
    let (d, _) = issue(&mut cr.c.w, &x, &o);
    x.insert_events(vec![d.clone()]);
    assert_eq!(x.base().unwrap().event, d.id);

    // x: d 뒤에 early. y: d 앞에 early(잠시 early가 바탕), 그 뒤 d가 오면 late 쪽으로 돌아감.
    let ux = x.insert_events(vec![early.clone()]);
    assert_eq!(x.base().unwrap().event, d.id);
    assert!(
        ux.conflicts
            .iter()
            .any(|c| c.base == late.id && c.other == early.id && c.same_author),
        "{:?}",
        ux.conflicts
    );
    y.insert_events(vec![early.clone()]);
    assert_eq!(y.base().unwrap().event, early.id);
    y.insert_events(tail.clone());
    let uy = y.insert_events(vec![d.clone()]);
    assert_eq!(uy.checkpoints.last(), Some(&d.id));
    assert_eq!(y.base(), x.base());

    let mut all = cr.c.events.clone();
    all.extend([early.clone(), late.clone(), d.clone()]);
    let mut scratch = Resolver::with_genesis(cr.genesis);
    scratch.insert_events(all.clone());
    assert_eq!(scratch.base(), x.base());
    let mut shuffled = all;
    Rng(11).shuffle(&mut shuffled);
    let mut z = Resolver::with_genesis(cr.genesis);
    for chunk in shuffled.chunks(9) {
        z.insert_events(chunk.to_vec());
    }
    assert_eq!(z.base(), x.base());

    // 경계가 같으므로 옛 부모를 둔 이벤트도 같은 판정입니다.
    let ev =
        cr.c.w
            .ev(&o, &[ids[30], d.id], text_channel(ch(250), "갈림"));
    for r in [&mut x, &mut y, &mut scratch, &mut z] {
        r.insert_events(vec![ev.clone()]);
    }
    assert_eq!(x.floor(), y.floor());
    assert_eq!(digest(&x), digest(&y));
    assert_eq!(digest(&x), digest(&scratch));
    assert_eq!(digest(&x), digest(&z));
}

/// 데몬은 경계 아래 이벤트를 해소기에 넣지 않을 수 있습니다(따라잡기에서 버림, 재시작 때 경계 이상만 다시 넣음). 늦은
/// 이벤트의 판정은 그와 무관하게 전체 기록 노드·새 멤버·재시작한 노드에서 같습니다.
#[test]
fn late_event_verdict_does_not_need_the_old_parent() {
    let mut cr = crew();
    let o = User::new(1);
    let ids = grow(&mut cr.c, &o, 1, 50);
    let mut full = Resolver::with_genesis(cr.genesis);
    full.insert_events(cr.c.events.clone());
    let (c1, snap) = issue(&mut cr.c.w, &full, &o);
    full.insert_events(vec![c1.clone()]);
    let floor = full.floor();
    let late =
        cr.c.w
            .ev(&o, &[ids[2], c1.id], text_channel(ch(200), "늦음"));
    assert!(late.header.lamport >= floor);
    full.insert_events(vec![late.clone()]);
    assert!(full.applied().contains(&late.id));

    let mut fresh = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    fresh.insert_events(vec![late.clone()]);
    // 재시작: 바탕으로 세우고 경계 이상 저장 이벤트만 다시 넣음(spaces::load_all).
    let mut restarted = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    let mut store = cr.c.events.clone();
    store.extend([c1.clone(), late.clone()]);
    restarted.insert_events(
        store
            .iter()
            .filter(|e| e.header.lamport >= floor)
            .cloned()
            .collect(),
    );
    for r in [&fresh, &restarted] {
        assert!(r.applied().contains(&late.id));
        assert_eq!(digest(r), digest(&full));
    }

    // 다음 체크포인트도 모두 받아들입니다.
    let next = cr.c.w.ev(&o, &full.heads(), text_channel(ch(201), "다음"));
    for r in [&mut full, &mut fresh, &mut restarted] {
        r.insert_events(vec![next.clone()]);
    }
    let (c2, _) = issue(&mut cr.c.w, &full, &o);
    for r in [&mut full, &mut fresh, &mut restarted] {
        let up = r.insert_events(vec![c2.clone()]);
        assert_eq!(up.checkpoints, vec![c2.id]);
    }
    assert_eq!(digest(&fresh), digest(&full));
    assert_eq!(digest(&restarted), digest(&full));
}

/// 다음 바탕을 받아 압축한 노드와 아직인 노드, 처음부터 해소한 노드가 옛 부모(압축으로 버린 것)를 둔 늦은 이벤트를 같게
/// 판정합니다.
#[test]
fn compaction_does_not_change_late_verdicts() {
    let mut cr = crew();
    let o = User::new(1);
    let early = grow(&mut cr.c, &o, 1, 45);
    let mut n = Resolver::with_genesis(cr.genesis);
    n.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &n, &o);
    n.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    cr.c.events.push(c1.clone());
    let before = cr.c.events.len();
    grow(&mut cr.c, &o, 100, 45);
    n.insert_events(cr.c.events[before..].to_vec());
    let (c2, _) = issue(&mut cr.c.w, &n, &o);
    let mut m = Resolver::with_genesis(cr.genesis);
    m.insert_events(cr.c.events.clone());
    let x = cr.c.w.ev(
        &o,
        &[early[3], cr.c.events.last().unwrap().id],
        text_channel(ch(250), "x"),
    );
    m.insert_events(vec![x.clone()]);
    m.insert_events(vec![c2.clone()]);
    let u = n.insert_events(vec![c2.clone()]);
    assert_eq!(u.checkpoints, vec![c2.id]);
    assert!(n.stats().compacted > 0);
    n.insert_events(vec![x.clone()]);
    let mut all = cr.c.events.clone();
    all.extend([c2.clone(), x.clone()]);
    let mut once = Resolver::with_genesis(cr.genesis);
    once.insert_events(all);
    assert_eq!(digest(&m), digest(&n));
    assert_eq!(digest(&once), digest(&n));
    assert_eq!(m.base(), n.base());
}

/// 경계 아래 논리 시계로 지어낸 이벤트가 그 부모(경계 위)를 머리에서 숨기지 않습니다: 증분 해소·처음부터 해소가 같고,
/// 다음 체크포인트도 넣는 순서와 무관하며, 정직한 작성자는 계속 체크포인트를 낼 수 있습니다.
#[test]
fn below_floor_child_does_not_hide_its_parent() {
    let mut cr = crew();
    let (o, a) = (User::new(1), User::new(2));
    grow(&mut cr.c, &o, 1, 45);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &r, &o);
    r.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    cr.c.events.push(c1.clone());
    let e1 = cr.c.push(&o, text_channel(ch(100), "e1"));
    r.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    let (c2, _) = issue(&mut cr.c.w, &r, &o);
    assert_eq!(signed(&c2).body.heads, vec![e1]);
    let q = cr.c.w.ev(&a, &[c1.id], text_channel(ch(101), "q"));
    let b =
        cr.c.w
            .ev_at(a.id(), a.dev(), &[q.id], 2, text_channel(ch(102), "b"));
    let mut nn = r.clone();
    nn.insert_events(vec![q.clone()]);
    nn.insert_events(vec![b.clone()]);
    assert!(nn.heads().contains(&q.id), "{:?}", nn.heads());
    assert!(nn.checkpoint_draft(KEY).is_ok());
    nn.insert_events(vec![c2.clone()]);
    let mut mm = r.clone();
    mm.insert_events(vec![c2.clone()]);
    mm.insert_events(vec![q.clone()]);
    mm.insert_events(vec![b.clone()]);
    let mut all = cr.c.events.clone();
    all.extend([q.clone(), b.clone(), c2.clone()]);
    let once = resolve_with(&all, &[], Some(cr.genesis));
    assert_eq!(once.state.as_ref(), mm.state());
    assert_eq!(nn.base(), mm.base());
    assert_eq!(digest(&nn), digest(&mm));

    // 숨은 머리와 동시인 소유자 이벤트의 처리 순서도 처음부터와 같습니다.
    let spare = SigningKeypair::from_seed(&[0x77; 32]);
    let q2 = cr.c.w.ev(
        &a,
        &[c1.id],
        EventBody::DeviceListUpdate {
            devices: a.list_with(2, vec![User::entry(&a.device), User::entry(&spare)]),
        },
    );
    let b2 =
        cr.c.w
            .ev_at(a.id(), a.dev(), &[q2.id], 2, text_channel(ch(103), "b2"));
    let kick = cr.c.w.ev(
        &o,
        &[c1.id],
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    let mut inc = r.clone();
    for e in [&q2, &b2, &kick] {
        inc.insert_events(vec![e.clone()]);
    }
    let mut all = cr.c.events.clone();
    all.extend([q2.clone(), b2.clone(), kick.clone()]);
    let mut once = Resolver::with_genesis(cr.genesis);
    once.insert_events(all);
    assert_eq!(
        inc.applied().contains(&q2.id),
        once.applied().contains(&q2.id)
    );
    assert_eq!(digest(&inc), digest(&once));

    // 저장소처럼 논리 시계를 늘려 가며 해소기 머리에 붙이는 정직한 작성자는 계속 체크포인트를 낼 수 있습니다.
    let mut lam = 100u64;
    for i in 0..5u8 {
        lam += 1;
        let hs = inc.heads();
        let e =
            cr.c.w
                .ev_at(o.id(), o.dev(), &hs, lam, text_channel(ch(150 + i), "h"));
        inc.insert_events(vec![e]);
    }
    assert!(inc.checkpoint_draft(KEY).is_ok());
}

/// 바탕과 동시인 기준점의 상태(`state_at`)는 처음부터 아는 노드와 그 바탕으로 들어온 멤버가 같습니다.
#[test]
fn state_at_concurrent_anchor_matches_newcomer() {
    let mut cr = crew();
    let o = User::new(1);
    let ids = grow(&mut cr.c, &o, 1, 40);
    let mut full = Resolver::with_genesis(cr.genesis);
    full.insert_events(cr.c.events.clone());
    let (c1, snap) = issue(&mut cr.c.w, &full, &o);
    full.insert_events(vec![c1.clone()]);
    let f = full.floor() as usize;
    let mut fresh = Resolver::from_checkpoint(cr.genesis, c1.clone(), &snap).unwrap();
    let e = cr.c.w.ev(&o, &[ids[f + 2]], text_channel(ch(222), "동시"));
    full.insert_events(vec![e.clone()]);
    fresh.insert_events(vec![e.clone()]);
    let channels = |r: &Resolver| match r.state_at(&[e.id], true) {
        AnchorState::Ready { state, .. } => state.map(|s| s.channels.len()),
        AnchorState::Incomplete { .. } => None,
    };
    assert!(channels(&full).is_some());
    assert_eq!(channels(&full), channels(&fresh));
}

/// 소유권을 넘긴 옛 소유자는 넘기기 전에 고정한 링크로 들어오는 새 멤버에게 자기가 소유자인 사슬을 보일 수 있습니다(그
/// 사슬은 그 자체로 맞음). 다른 출처의 머리와 견주면 같은 순번의 다른 체크포인트라 갈라진 것으로 드러납니다.
#[test]
fn former_owner_fork_is_caught_by_a_second_source() {
    let mut cr = crew();
    let (o, a) = (User::new(1), User::new(2));
    grow(&mut cr.c, &o, 1, 3);
    let mut r = Resolver::with_genesis(cr.genesis);
    r.insert_events(cr.c.events.clone());
    let (c1, _) = issue(&mut cr.c.w, &r, &o);
    r.insert_events(vec![c1.clone()]);
    cr.c.head = Some(c1.id);
    let n = cr.c.events.len();
    cr.c.push(&o, transfer_at(&o, a.id(), Some(signed(&c1).id())));
    grow(&mut cr.c, &a, 20, 2);
    r.insert_events(cr.c.events[n..].to_vec());
    let (c2, _) = issue(&mut cr.c.w, &r, &a);
    r.insert_events(vec![c2.clone()]);
    assert_eq!(r.state().unwrap().owner, a.id());

    let mut fake = Resolver::with_genesis(cr.genesis);
    fake.insert_events(cr.c.events[..n].to_vec());
    fake.insert_events(vec![c1.clone()]);
    let x = cr.c.w.ev(&o, &fake.heads(), text_channel(ch(99), "가짜"));
    fake.insert_events(vec![x]);
    let (forged, snap) = issue(&mut cr.c.w, &fake, &o);
    let installed = validate_chain(
        &pin(&c1),
        &[signed(&c1).clone(), signed(&forged).clone()],
        Some(&snap),
    )
    .unwrap();
    assert_eq!(installed, pin(&forged));

    // 정직한 멤버의 답: 같은 순번의 다른 체크포인트.
    let honest = vec![signed(&c2).clone()];
    assert_eq!(
        compare_heads(&installed, &SPACE, &pin(&c2), &honest),
        HeadAgreement::Conflict
    );
    // 정직한 쪽이 더 나아갔어도 내 순번부터의 사슬로 드러납니다.
    let mut longer = r.clone();
    let (c3, _) = next_checkpoint(&mut cr, &mut longer, &a, 40, 2);
    let honest = vec![signed(&c2).clone(), signed(&c3).clone()];
    assert_eq!(
        compare_heads(&installed, &SPACE, &pin(&c3), &honest),
        HeadAgreement::Conflict
    );
    // 같은 사슬이면 같거나 이어짐, 서명이 맞지 않거나 뒤처진 답은 판정하지 않음.
    let mine = pin(&c2);
    assert_eq!(
        compare_heads(&mine, &SPACE, &pin(&c2), &[]),
        HeadAgreement::Same
    );
    assert_eq!(
        compare_heads(&mine, &SPACE, &pin(&c3), &honest),
        HeadAgreement::Extends
    );
    assert_eq!(
        compare_heads(&mine, &SPACE, &pin(&c1), &[signed(&c1).clone()]),
        HeadAgreement::Unknown
    );
    let mut garbage = signed(&forged).clone();
    garbage.body.users += 1;
    assert_eq!(
        compare_heads(&mine, &SPACE, &pin(&forged), &[garbage]),
        HeadAgreement::Unknown
    );
    assert_eq!(
        compare_heads(
            &mine,
            &SpaceId([8; 16]),
            &pin(&forged),
            &[signed(&forged).clone()]
        ),
        HeadAgreement::Unknown
    );
}
