//! 검토에서 나온 키 그룹 사슬 결함의 회귀 시험.

use super::super::super::group::{Equivocation, MAX_SEQ_CANDS};
use super::*;

/// 탈퇴 이벤트를 `parents`에 붙여 사슬에 넣습니다(머리는 그대로).
fn leave_at(c: &mut Chain, u: &User, parents: &[EventId], lamport: u64) -> EventId {
    let e = c.w.ev_at(
        u.id(),
        u.dev(),
        parents,
        lamport,
        EventBody::MemberLeave { user: u.id() },
    );
    let id = e.id;
    c.events.push(e);
    id
}

/// 확정 기기가 볼 수 없었던(갱신 머리의 조상이 아닌) 동시 탈퇴는 이미 적용된 정직한 Rotate를 뒤집지 않고 빚으로 남아
/// 다음 갱신으로 넘어갑니다. 머리의 조상인 제거는 빠뜨릴 수 없고, 빚 밖의 기기는 뺄 수 없습니다.
#[test]
fn concurrent_leave_carries_over_to_next_update() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let carol = User::new(4);
    cr.c.push(o, carol.join_body(None));
    let k1 = cr.c.push(
        o,
        ku(crew_update(1, None, advance(devs(&[o, a, b, &carol])))),
    );
    let kb = cr.c.push(o, kick(b));
    let base = cr.c.w.lamport_after(&[kb]);
    let x = leave_at(&mut cr.c, &carol, &[k1], base + 10);
    // a의 탈퇴는 Rotate와 동시이고 논리 시계가 낮아 해소 순서에서 Rotate보다 먼저입니다.
    let low = cr.c.w.lamport_after(&[k1]);
    let leave = leave_at(&mut cr.c, a, &[k1], low);
    let rot = cr.c.fork(
        o,
        &[kb, x],
        ku(crew_update(
            2,
            Some(k1),
            rotate(vec![b.dev(), carol.dev()], vec![]),
        )),
    );
    let without: Vec<DecodedEvent> =
        cr.c.events
            .iter()
            .filter(|e| e.id != leave)
            .cloned()
            .collect();
    let mut res = Resolver::new();
    res.insert_events(without);
    assert!(res.key_events().contains(&rot));
    let upd = res.insert_events(cr.c.events.clone());
    assert!(!upd.unapplied.contains(&rot), "동시 탈퇴가 Rotate를 뒤집음");
    let r = cr.c.resolve();
    assert_applied(&r, rot);
    let k = crew_keys(&r);
    assert_eq!(k.seq, 2);
    assert_eq!(k.debt, set(&[a.dev()]));
    assert_eq!(k.lost_by.get(&a.dev()), Some(&leave));
    assert_eq!(res.key_view().unwrap().crew, *k);

    // 다음 갱신은 a의 탈퇴를 조상으로 두므로 a를 빼야 하고, 빚 밖의 기기는 뺄 수 없습니다.
    let rr = cr.c.fork(
        o,
        &[rot, leave],
        ku(crew_update(2 + 1, Some(rot), KeyUpdateKind::RRefresh)),
    );
    cr.c.head = Some(rr);
    let extra = cr.c.push(
        o,
        ku(crew_update(
            3,
            Some(rot),
            rotate(vec![a.dev(), o.dev()], vec![]),
        )),
    );
    let good = cr.c.push(
        o,
        ku(crew_update(3, Some(rot), rotate(vec![a.dev()], vec![]))),
    );
    let r = cr.c.resolve();
    let wrong = AuthError::Invalid("제거 목록이 자격을 잃은 기기와 다름");
    assert_rejected(&r, rr, wrong.clone());
    assert_rejected(&r, extra, wrong);
    assert_applied(&r, good);
    let k = crew_keys(&r);
    assert_eq!(k.seq, 3);
    assert!(k.debt.is_empty() && k.lost_by.is_empty());
}

/// 같은 순번 후보 자리는 받은 순서와 무관합니다: 작성 기기마다 순위가 좋은 것만 남으므로 끝나지 않는 머리를 먼저
/// 쏟아 넣어도 결과가 같습니다.
#[test]
fn group_fork_slots_do_not_depend_on_arrival() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let low = w.lamport_after(&[fx.k1]);
    let mut forks = Vec::new();
    for i in 0..MAX_FORKS {
        let mut p = group_update(g, 1, None, 1, advance(devs(&[o, a])));
        p.parts = 2; // 끝나지 않음
        p.salt = [i as u8 + 1; 24];
        forks.push(group_ev(w, o, &[], &[fx.k1], Some(low + 5), p));
    }
    let honest = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        Some(low),
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    let mut ga = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut ga, &honest, &res), accepted(&[honest.id]));
    for f in &forks {
        let _ = put(&mut ga, f, &res);
    }
    let mut gb = GroupChain::new(SPACE, g);
    for f in &forks {
        assert!(put(&mut gb, f, &res).is_ok());
    }
    assert_eq!(put(&mut gb, &honest, &res), accepted(&[honest.id]));
    assert_eq!(ga.winners(), &[honest.id]);
    assert_eq!(gb.winners(), ga.winners());
    for f in &forks {
        assert_eq!(ga.verdict(&f.id).is_some(), gb.verdict(&f.id).is_some());
    }
    const { assert!(MAX_SEQ_CANDS >= MAX_FORKS) };
}

/// 강등된 옛 확정 기기가 옛 기준점으로 끝나지 않는 머리를 순번마다 가득 내도, 지금 확정 기기의 갱신은 자기 자리로
/// 들어가 이깁니다(크루 순번이 커서).
#[test]
fn ex_committer_cannot_fill_fork_slots() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let admin = role(1, 10, Permissions::ADMINISTRATOR);
    fx.cr.c.push(
        o,
        EventBody::RoleCreate {
            role: admin.clone(),
        },
    );
    fx.cr.c.push(o, set_roles(a, vec![admin.id, fx.viewer]));
    let cc = fx
        .cr
        .c
        .push(o, EventBody::CommitterChange { device: a.dev() });
    let mut res = resolver(&fx.cr.c);
    let mut gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        &mut fx.cr.c.w,
        a,
        &[],
        &[cc],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));
    fx.cr.c.push(o, set_roles(a, vec![]));
    sync(&mut res, &fx.cr.c);
    assert_eq!(res.state().unwrap().committer, o.dev());
    let k2 = fx.cr.c.push(
        o,
        ku(crew_update(2, Some(fx.k1), full_rekey(0, vec![], vec![]))),
    );
    sync(&mut res, &fx.cr.c);
    gc.rejudge(&res);
    // a는 자격을 잃어 g1을 쓸 수 없고, 정리한 바닥으로 돌아가지도 않습니다.
    assert_eq!(gc.send_tip(), None);
    assert_eq!(gc.tip().seq, 1);

    for (seq, prev) in [(1, None), (2, Some(g1.id))] {
        for i in 0..MAX_FORKS + 2 {
            let mut junk = group_update(g, seq, prev, 1, KeyUpdateKind::RRefresh);
            junk.parts = 2;
            junk.salt = [i as u8 + 1; 24];
            let e = group_ev(&mut fx.cr.c.w, a, &[], &[cc], Some(1), junk);
            let _ = put(&mut gc, &e, &res);
        }
    }
    let on_g1 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 2, Some(g1.id), 2, rotate(vec![a.dev()], vec![])),
    );
    let r2 = put(&mut gc, &on_g1, &res);
    // a는 자기 자리만 채웁니다(순위가 더 좋은 자기 쓰레기로 자기 g1을 밀어낼 수는 있음). o의 후보는 들어갑니다.
    assert!(r2.is_ok(), "새 확정 기기의 순번 2: {r2:?}");
    let fresh1 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 1, None, 2, advance(devs(&[o]))),
    );
    let r1 = put(&mut gc, &fresh1, &res);
    assert!(r1.is_ok(), "새 확정 기기의 순번 1: {r1:?}");
    // 크루 순번이 큰 fresh1이 순번 1을 가져가 사슬이 그쪽으로 옮깁니다.
    assert_eq!(gc.winners(), &[fresh1.id]);
    assert_eq!(gc.send_tip().map(|t| t.seq), Some(1));
}

/// 아무도 풀 수 없는 상태 이벤트가 기준점의 조상에 끼어도 그룹 갱신을 판정하고(빈 구간으로 봄), 빈 구간을 낀 판정은
/// 그 구간이 채워지면 다시 합니다.
#[test]
fn undecoded_ancestor_does_not_freeze_group_chain() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let l = fx.cr.c.w.lamport_after(&[fx.k1]);
    let junk = LinkEvent {
        id: EventId([0x66; 32]),
        parents: vec![fx.k1],
        lamport: l,
        undecoded: true,
    };
    let later = fx.cr.c.fork(
        o,
        &[junk.id],
        EventBody::RoleCreate {
            role: role(9, 3, Permissions::NONE),
        },
    );
    fx.cr.c.head = Some(later);
    let grant = fx.cr.c.push(o, set_roles(b, vec![fx.viewer]));
    let grant_ev = fx
        .cr
        .c
        .events
        .iter()
        .find(|e| e.id == grant)
        .cloned()
        .expect("있음");
    let first: Vec<DecodedEvent> = fx
        .cr
        .c
        .events
        .iter()
        .filter(|e| e.id != grant)
        .cloned()
        .collect();
    let mut res = Resolver::new();
    res.insert(
        first,
        vec![
            junk.clone(),
            LinkEvent::from_header(grant, &grant_ev.header),
        ],
    );
    let (_, gaps) = res.point_at(&[grant]).expect("빈 구간이 있어도 셈");
    assert!(gaps);
    assert!(res.point_at(&[EventId([0x55; 32])]).is_none());

    let upd = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[grant],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a, b]))),
    );
    let mut gc = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut gc, &upd, &res), Ok(ChainChange::default()));
    // b를 볼 수 있게 한 이벤트를 아직 못 풀어 지금은 거부되지만, 확정하지 않고 본문이 오면 다시 판정합니다.
    assert_eq!(
        gc.verdict(&upd.id),
        Some(Err(AuthError::Invalid("자격 없는 기기를 넣음")))
    );
    let epoch = res.gap_epoch();
    res.insert_events(vec![grant_ev]);
    assert!(res.gap_epoch() > epoch);
    assert_eq!(Ok(gc.rejudge(&res)), accepted(&[upd.id]));
    assert_eq!(gc.tip().members, set(&devs(&[o, a, b])));
}

/// 확정 기기가 바뀌어도(옛 확정 기기는 소유자라 자격이 남음) 쓸 수 있는 사슬은 그대로이고, 작성 기기가 자격을 잃으면
/// 정리한 옛 바닥으로 돌아가지 않고 보낼 키가 없습니다.
#[test]
fn usable_chain_survives_committer_change_and_never_regresses() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let admin = role(1, 10, Permissions::ADMINISTRATOR);
    fx.cr.c.push(
        o,
        EventBody::RoleCreate {
            role: admin.clone(),
        },
    );
    let made_admin = fx.cr.c.push(o, set_roles(a, vec![admin.id]));
    let mut res = resolver(&fx.cr.c);
    let n = KEEP_SEQS as u64 + 6;
    let chain =
        |gc: &mut GroupChain, w: &mut World, author: &User, anchor: EventId, res: &Resolver| {
            let mut prev = None;
            for s in 1..=n {
                let kind = if s == 1 {
                    advance(devs(&[o, a]))
                } else {
                    KeyUpdateKind::RRefresh
                };
                let e = group_ev(
                    w,
                    author,
                    &prev.into_iter().collect::<Vec<_>>(),
                    &[anchor],
                    None,
                    group_update(g, s, prev, 1, kind),
                );
                put(gc, &e, res).expect("유효한 갱신");
                prev = Some(e.id);
            }
        };
    // 1) 소유자 기기 o가 낸 사슬: 확정 기기가 a로 바뀌어도 그대로 씀.
    let mut by_owner = GroupChain::new(SPACE, g);
    chain(&mut by_owner, &mut fx.cr.c.w, o, made_admin, &res);
    assert_eq!(by_owner.tip().seq, n);
    let cc = fx
        .cr
        .c
        .push(o, EventBody::CommitterChange { device: a.dev() });
    sync(&mut res, &fx.cr.c);
    assert_eq!(by_owner.rejudge(&res), ChainChange::default());
    assert_eq!(by_owner.send_tip().map(|t| t.seq), Some(n));
    assert_eq!(by_owner.tip().seq, n);

    // 2) 확정 기기 a가 낸 사슬: a가 관리자에서 내려오면 보낼 키가 없고(옛 바닥 순번 6으로 돌아가지 않음), 사슬 끝은 그대로.
    let mut by_admin = GroupChain::new(SPACE, g);
    chain(&mut by_admin, &mut fx.cr.c.w, a, cc, &res);
    assert_eq!(by_admin.send_tip().map(|t| t.seq), Some(n));
    fx.cr.c.push(o, set_roles(a, vec![]));
    sync(&mut res, &fx.cr.c);
    by_admin.rejudge(&res);
    assert!(by_admin.winners().is_empty());
    assert_eq!(by_admin.send_tip(), None);
    assert_eq!(by_admin.tip().seq, n);
}

/// 같은 작성 기기가 같은 순번에 낸 유효한 갱신 둘은 한 번 알립니다(두 판의 증거).
#[test]
fn group_equivocation_is_reported_once() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let mut gc = GroupChain::new(SPACE, g);
    let one = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    put(&mut gc, &one, &res).expect("받음");
    // 거부된 후보를 다시 내는 것은 두 판이 아닙니다.
    let mut bad = group_update(g, 1, None, 1, advance(devs(&[o])));
    bad.salt = [3; 24];
    bad.sealed = vec![sealed_to(&devs(&[&fx.cr.b]))];
    let bad = group_ev(w, o, &[], &[fx.k1], None, bad);
    put(&mut gc, &bad, &res).expect("받음");
    assert!(gc.take_equivocations().is_empty());
    let mut p = group_update(g, 1, None, 1, advance(devs(&[o])));
    p.salt = [9; 24];
    let two = group_ev(w, o, &[], &[fx.k1], None, p);
    put(&mut gc, &two, &res).expect("받음");
    let mut heads = [one.id, two.id];
    heads.sort();
    assert_eq!(
        gc.take_equivocations(),
        vec![Equivocation {
            seq: 1,
            author: o.dev(),
            heads,
        }]
    );
    assert!(gc.take_equivocations().is_empty());
}

/// 확정 기기는 내기 전에 사슬이 받아들일지 봅니다(받지 않을 갱신을 내면 멤버마다 갈래 자리만 차지).
#[test]
fn would_accept_dry_runs_the_next_update() {
    let fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let gc = GroupChain::new(SPACE, g);
    // 바탕 체크포인트가 없어 논리 시계는 판정에 쓰이지 않습니다(늦은 갱신 규칙은 keys::checkpoint).
    let l = fx.cr.c.w.lamport_after(&[fx.k1]);
    let ok = group_update(g, 1, None, 1, advance(devs(&[o, a])));
    assert_eq!(
        gc.would_accept(&o.id(), &o.dev(), &[fx.k1], l, &ok, &res),
        Ok(())
    );
    let mut stale = ok.clone();
    stale.crew = Some(CrewRef {
        seq: 1,
        root_commit: [7; 32],
    });
    assert_eq!(
        gc.would_accept(&o.id(), &o.dev(), &[fx.k1], l, &stale, &res),
        Err(AuthError::Invalid("기준점의 크루 키 사슬 끝과 다름"))
    );
    let with_b = group_update(g, 1, None, 1, advance(devs(&[o, b])));
    assert_eq!(
        gc.would_accept(&o.id(), &o.dev(), &[fx.k1], l, &with_b, &res),
        Err(AuthError::Invalid("자격 없는 기기를 넣음"))
    );
    assert_eq!(
        gc.would_accept(&a.id(), &a.dev(), &[fx.k1], l, &ok, &res),
        Err(AuthError::NotCommitter)
    );
    let skip = group_update(g, 2, Some(fx.k1), 1, KeyUpdateKind::RRefresh);
    assert_eq!(
        gc.would_accept(&o.id(), &o.dev(), &[fx.k1], l, &skip, &res),
        Err(AuthError::Invalid("앞 갱신과 이어지지 않음"))
    );
    assert_eq!(
        gc.would_accept(&o.id(), &o.dev(), &[EventId([0x55; 32])], l, &ok, &res),
        Err(AuthError::AnchorUnknown)
    );
}
