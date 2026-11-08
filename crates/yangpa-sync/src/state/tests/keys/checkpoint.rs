//! 체크포인트와 키 그룹 사슬: 경계 아래 갱신, 키 머리에서 시작한 사슬, 저장한 바닥, 체크포인트가 덮은 앞부분, 그리고
//! 처음부터 아는 노드·머리에서 시작한 새 멤버·다시 시작한 노드가 머리 뒤 갱신을 똑같이 판정하는지(진짜 해소기로).

use yangpa_core::checkpoint::{KeyHead, members_digest};

use super::super::checkpoint::{grow, issue_with, signed};
use super::*;

/// 해소기에 경계·바탕 자리를 덧씌운 기준점 조회(압축한 노드·체크포인트로 들어온 노드 흉내).
struct Floored<'a> {
    res: &'a Resolver,
    /// 그룹 경계(바탕 체크포인트에 그 그룹 머리가 있을 때).
    floor: Option<u64>,
    /// 경계 아래라고 아는 이벤트.
    below: Vec<EventId>,
    /// 바탕의 경계 위 조상.
    covered: Vec<EventId>,
    /// 모르는 이벤트(압축으로 빠졌거나 체크포인트 앞이라 받은 적 없음).
    forgotten: Vec<EventId>,
}

impl<'a> Floored<'a> {
    fn new(res: &'a Resolver) -> Self {
        Floored {
            res,
            floor: None,
            below: Vec::new(),
            covered: Vec::new(),
            forgotten: Vec::new(),
        }
    }
}

impl KeyPoints for Floored<'_> {
    fn at(&self, anchor: &[EventId]) -> Option<(KeyPoint, bool)> {
        if anchor.iter().any(|a| self.forgotten.contains(a)) {
            return None;
        }
        self.res.point_at(anchor)
    }

    fn now(&self) -> Option<&SpaceState> {
        self.res.state()
    }

    fn includes(&self, later: &[EventId], earlier: &[EventId]) -> bool {
        self.res.includes(later, earlier)
    }

    fn gap_epoch(&self) -> u64 {
        self.res.gap_epoch()
    }

    fn group_floor(&self, _group: &KeyGroupId) -> Option<u64> {
        self.floor
    }

    fn place(&self, id: &EventId) -> AnchorPlace {
        if self.forgotten.contains(id) || !self.res.contains(id) {
            AnchorPlace::Unknown
        } else if self.below.contains(id) {
            AnchorPlace::Below
        } else if self.covered.contains(id) {
            AnchorPlace::Covered
        } else {
            AnchorPlace::After
        }
    }
}

fn put_at(
    gc: &mut GroupChain,
    e: &DecodedEvent,
    points: &dyn KeyPoints,
) -> Result<ChainChange, AuthError> {
    let EventBody::KeyUpdate(p) = &e.body else {
        panic!("키 갱신이 아님");
    };
    gc.insert(e.id, &e.header, p, points)
}

/// 경계 아래 갱신(자기 논리 시계가 F 아래이거나 기준점이 F 아래)과 기준점이 모두 바탕의 조상인 갱신은 늦은 이벤트입니다:
/// 압축한 노드와 체크포인트로 들어온 노드가 그 기준점 상태를 셀 수 없어, 받아들이면 노드마다 사슬이 갈립니다. 바탕의
/// 조상을 끼더라도 바탕 뒤 이벤트가 있으면 그것만으로 셉니다.
#[test]
fn group_updates_below_the_floor_are_late() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let mid = fx.cr.c.push(o, text_channel(ch(11), "가운데"));
    let later = fx.cr.c.push(o, text_channel(ch(10), "나중"));
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let l_later = w.lamport_after(&[later]);
    let mut points = Floored::new(&res);
    points.floor = Some(l_later);
    points.below.push(fx.k1);
    points.covered.push(mid);
    let mut gc = GroupChain::new(SPACE, g);
    let p1 = group_update(g, 1, None, 1, advance(devs(&[o, a])));
    // 기준점(k1)이 경계 아래.
    let old_anchor = group_ev(w, o, &[], &[fx.k1], Some(l_later + 1), p1.clone());
    assert_eq!(
        put_at(&mut gc, &old_anchor, &points),
        Err(AuthError::LateEvent)
    );
    // 자기 논리 시계가 경계 아래.
    let low = group_ev(w, o, &[], &[later], Some(l_later - 1), p1.clone());
    assert_eq!(put_at(&mut gc, &low, &points), Err(AuthError::LateEvent));
    // 기준점이 모두 바탕의 조상: 바탕을 모르고 낸 것.
    let covered = group_ev(w, o, &[], &[mid], Some(l_later + 1), p1.clone());
    assert_eq!(
        put_at(&mut gc, &covered, &points),
        Err(AuthError::LateEvent)
    );
    // 확정 기기도 내기 전에 같은 규칙으로 봅니다.
    let wa = |anchor: &[EventId], lamport| {
        gc.would_accept(&o.id(), &o.dev(), anchor, lamport, &p1, &points)
    };
    assert_eq!(wa(&[later], l_later - 1), Err(AuthError::LateEvent));
    assert_eq!(wa(&[mid], l_later + 1), Err(AuthError::LateEvent));
    assert_eq!(wa(&[mid, later], l_later + 1), Ok(()));
    // 경계 위 기준점으로 다시 낸 것은 받습니다(바탕의 조상을 껴도).
    let fresh = group_ev(w, o, &[], &[mid, later], None, p1);
    assert_eq!(put_at(&mut gc, &fresh, &points), accepted(&[fresh.id]));
}

/// 체크포인트 키 머리에서 시작한 사슬: 바닥은 머리 정보(머리 이벤트, 크루 순번, 새 T의 크루 순번, 작성자)로 처음부터
/// 아는 노드와 같고, 바닥 멤버만 모릅니다. 멤버를 배우기 전에는 다음 갱신을 판정하지 않고(느슨하게 판정하면 노드마다
/// 갈림), 머리의 멤버 요약과 맞는 목록만 배웁니다. 배운 뒤로는 처음부터 아는 사슬과 같이 판정합니다.
#[test]
fn chain_from_a_checkpoint_head() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let mut res = resolver(&fx.cr.c);
    let mut full = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut full, &g1, &res), accepted(&[g1.id]));
    let head = full.key_head().expect("그룹 머리");
    let hc = head.chain.expect("그룹 머리의 사슬 정보");
    assert_eq!(
        (hc.head, hc.crew_seq, hc.fresh_crew, hc.user, hc.device),
        (g1.id, 1, 1, o.id(), o.dev())
    );
    assert_eq!(hc.members, members_digest(&g, devs(&[o, a]).iter()));
    assert!(
        GroupChain::from_head(
            SPACE,
            &KeyHead {
                chain: None,
                ..head
            }
        )
        .is_none(),
        "사슬 정보 없는 머리(크루 머리 모양)로는 세우지 않음"
    );
    let mut head_gc = GroupChain::from_head(SPACE, &head).expect("그룹 머리");
    head_gc.rejudge(&res);
    let t = head_gc.send_tip().expect("머리 작성자가 지금도 자격 있음");
    assert!(!t.members_known && t.members.is_empty());
    assert_eq!(
        (t.seq, t.head, t.root_commit, t.crew_seq, t.fresh_crew),
        (1, Some(g1.id), key_commit(1), 1, 1)
    );

    // a가 역할을 잃음: 빼는 회전.
    let demote = fx.cr.c.push(o, set_roles(a, vec![]));
    sync(&mut res, &fx.cr.c);
    let w = &mut fx.cr.c.w;
    let reject = |e: &'static str| Some(Err(AuthError::Invalid(e)));
    let no_prev = group_ev(
        w,
        o,
        &[],
        &[demote],
        None,
        group_update(g, 2, None, 1, rotate(vec![a.dev()], vec![])),
    );
    let mut wrong_p = group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![]));
    wrong_p.prev_root_commit = [9; 32];
    let wrong_root = group_ev(w, o, &[], &[demote], None, wrong_p);
    let with_b = group_ev(
        w,
        o,
        &[],
        &[demote],
        None,
        group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![b.dev()])),
    );
    let mut leak_p = group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![]));
    leak_p.sealed = vec![sealed_to(&devs(&[o, b]))];
    let leak = group_ev(w, o, &[], &[demote], None, leak_p);
    let bad = [
        (&no_prev, "앞 갱신과 이어지지 않음"),
        (&wrong_root, "앞 갱신과 이어지지 않음"),
        (&with_b, "자격 없는 기기를 넣음"),
        (&leak, "그룹 밖 기기에게 키를 줌"),
    ];
    for (e, why) in bad {
        assert_eq!(put(&mut full, e, &res), Ok(ChainChange::default()));
        assert_eq!(full.verdict(&e.id), reject(why), "{why}");
        assert_eq!(put(&mut head_gc, e, &res), Ok(ChainChange::default()));
        assert_eq!(
            head_gc.verdict(&e.id),
            None,
            "멤버를 배울 때까지 판정하지 않음"
        );
    }
    let g2 = group_ev(
        w,
        o,
        &[],
        &[demote],
        None,
        group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![])),
    );
    assert_eq!(put(&mut full, &g2, &res), accepted(&[g2.id]));
    assert_eq!(put(&mut head_gc, &g2, &res), Ok(ChainChange::default()));
    assert_eq!(head_gc.verdict(&g2.id), None);

    // 요약과 다른 목록(누가 보냈든)·다른 순번·약속은 버립니다.
    assert!(!head_gc.learn_members(1, &key_commit(1), &devs(&[o, a, b])));
    assert!(!head_gc.learn_members(1, &key_commit(1), &devs(&[o])));
    assert!(!head_gc.learn_members(1, &[7; 32], &devs(&[o, a])));
    assert!(!head_gc.learn_members(2, &key_commit(2), &devs(&[o])));
    let list = full
        .members_at(1, &key_commit(1))
        .expect("처음부터 아는 노드는 앎");
    assert!(head_gc.learn_members(1, &key_commit(1), &list));
    assert!(!head_gc.learn_members(1, &key_commit(1), &list), "이미 앎");
    assert_eq!(Ok(head_gc.rejudge(&res)), accepted(&[g2.id]));
    for (e, why) in bad {
        assert_eq!(head_gc.verdict(&e.id), reject(why), "{why}");
    }
    assert_eq!(head_gc.tip(), full.tip());

    // 이제 제거 목록도 정확해야 합니다: 이미 멤버인 o를 다시 넣으면 둘 다 거부.
    let regrant = fx.cr.c.push(o, set_roles(a, vec![fx.viewer]));
    sync(&mut res, &fx.cr.c);
    let w = &mut fx.cr.c.w;
    let again = group_ev(
        w,
        o,
        &[],
        &[regrant],
        None,
        group_update(g, 3, Some(g2.id), 1, advance(devs(&[o, a]))),
    );
    for gc in [&mut full, &mut head_gc] {
        assert_eq!(put(gc, &again, &res), Ok(ChainChange::default()));
        assert_eq!(gc.verdict(&again.id), Some(Err(AuthError::AlreadyMember)));
    }
    let g3 = group_ev(
        w,
        o,
        &[],
        &[regrant],
        None,
        group_update(g, 3, Some(g2.id), 1, advance(vec![a.dev()])),
    );
    assert_eq!(put(&mut full, &g3, &res), accepted(&[g3.id]));
    assert_eq!(put(&mut head_gc, &g3, &res), accepted(&[g3.id]));
    assert_eq!(head_gc.tip(), full.tip());
}

/// 셀 수 없는 갱신(모르는 기준점)에 막힌 사슬은 체크포인트 키 머리로 바닥을 옮겨 그 뒤 갱신을 판정합니다(바닥 멤버를
/// 배운 뒤).
#[test]
fn adopting_a_checkpoint_head_unblocks_the_chain() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let later = fx.cr.c.push(o, text_channel(ch(10), "나중"));
    let res = resolver(&fx.cr.c);
    let mut points = Floored::new(&res);
    points.forgotten.push(fx.k1);
    let w = &mut fx.cr.c.w;
    let mut gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put_at(&mut gc, &g1, &points), Err(AuthError::AnchorUnknown));
    let g2 = group_ev(
        w,
        o,
        &[],
        &[later],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put_at(&mut gc, &g2, &points), Ok(ChainChange::default()));
    assert_eq!(gc.verdict(&g2.id), None, "앞 갱신을 기다림");
    // 처음부터 아는 노드가 쓴 머리(순번 1).
    let mut writer = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut writer, &g1, &res), accepted(&[g1.id]));
    let head = writer.key_head().expect("머리");
    assert!(gc.apply_head(&head, &points));
    assert_eq!(gc.rejudge(&points), ChainChange::default());
    assert_eq!(gc.verdict(&g2.id), None, "바닥 멤버를 배울 때까지 기다림");
    let list = writer.members_at(1, &key_commit(1)).expect("멤버");
    assert!(gc.learn_members(1, &key_commit(1), &list));
    assert_eq!(Ok(gc.rejudge(&points)), accepted(&[g2.id]));
    assert!(!gc.apply_head(&head, &points), "이미 그 머리");
    assert_eq!(gc.tip().seq, 2);
    // 다음 체크포인트가 순번 2를 머리로 실으면 그 갱신까지 접고 멤버도 그대로 씁니다.
    let head2 = gc.key_head().expect("머리");
    assert!(gc.apply_head(&head2, &points));
    let base = gc.saved_base();
    assert_eq!((base.seq, base.head), (2, Some(g2.id)));
    assert!(base.members_known && base.anchor.is_empty());
    // 바닥 아래 갱신은 이미 덮인 것입니다.
    assert_eq!(
        put_at(&mut gc, &g1, &Floored::new(&res)),
        Err(AuthError::DuplicateCommit)
    );
}

/// 재시작: 저장한 바닥(머리에서 세운 마디, 기준점은 비움)에서 이어 가고, 앞 갱신의 기준점이 압축으로 빠졌어도 다음
/// 갱신을 받습니다(머리 마디의 기준점은 비어 되돌리기 검사가 없음).
#[test]
fn saved_base_resumes_after_restart_and_compaction() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let later = fx.cr.c.push(o, text_channel(ch(10), "나중"));
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let mut gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));
    assert_eq!(gc.saved_base().seq, 0, "접기 전");
    let head = gc.key_head().expect("머리");
    assert!(gc.apply_head(&head, &res));
    gc.rejudge(&res);
    let saved = gc.saved_base();
    assert_eq!((saved.seq, saved.head), (1, Some(g1.id)));
    assert!(saved.members_known && saved.anchor.is_empty());
    let bytes = postcard::to_stdvec(&saved).unwrap();
    let back: ChainBase = postcard::from_bytes(&bytes).unwrap();
    let mut restarted = GroupChain::from_saved(SPACE, g, back);
    assert!(
        !restarted.apply_head(&head, &res),
        "저장한 바닥이 이미 그 머리"
    );
    restarted.rejudge(&res);
    assert_eq!(restarted.send_tip(), gc.send_tip());
    assert_eq!(
        put(&mut restarted, &g1, &res),
        Err(AuthError::DuplicateCommit)
    );
    let g2 = group_ev(
        w,
        o,
        &[],
        &[later],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    let mut points = Floored::new(&res);
    points.forgotten.push(fx.k1);
    assert_eq!(put_at(&mut restarted, &g2, &points), accepted(&[g2.id]));
    assert_eq!(put(&mut gc, &g2, &res), accepted(&[g2.id]));
    assert_eq!(restarted.tip(), gc.tip());
}

/// 체크포인트가 덮은 순번 아래로는 갈래를 바꾸지 않습니다(그 앞 후보를 정리).
#[test]
fn covered_prefix_is_folded() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let mut gc = GroupChain::new(SPACE, g);
    let mut prev = None;
    let mut heads = Vec::new();
    for s in 1..=3u64 {
        let kind = if s == 1 {
            advance(devs(&[o, a]))
        } else {
            KeyUpdateKind::RRefresh
        };
        let e = group_ev(w, o, &[], &[fx.k1], None, group_update(g, s, prev, 1, kind));
        assert_eq!(put(&mut gc, &e, &res), accepted(&[e.id]));
        prev = Some(e.id);
        heads.push(e.id);
    }
    gc.prune_covered(2);
    assert_eq!(gc.winners(), &heads[2..]);
    assert_eq!(gc.saved_base().seq, 2);
    let rival = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        Some(1),
        group_update(g, 2, Some(heads[0]), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &rival, &res), Err(AuthError::DuplicateCommit));
}

/// 머리 순번 뒤 갱신 가운데 경계 아래 것(체크포인트를 쓴 기기가 몰랐던 것)은 이미 받은 노드도 버립니다: 머리에서
/// 시작한 새 멤버는 그것을 늦은 이벤트로 거부하므로, 그래야 모두 같은 사슬을 봅니다. 확정 기기가 경계 위에서 다시
/// 낸 갱신은 양쪽 모두 받습니다.
#[test]
fn late_updates_above_the_head_are_dropped() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let later = fx.cr.c.push(o, text_channel(ch(10), "나중"));
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let f = w.lamport_after(&[later]);
    let mut old = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut old, &g1, &res), accepted(&[g1.id]));
    // 체크포인트를 쓴 기기는 순번 1까지만 앎.
    let head = old.key_head().expect("머리");
    // 체크포인트를 쓴 기기가 모르던 순번 2(경계 아래 논리 시계).
    let hidden = group_ev(
        w,
        o,
        &[],
        &[later],
        Some(f - 1),
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut old, &hidden, &res), accepted(&[hidden.id]));
    let mut points = Floored::new(&res);
    points.floor = Some(f);
    assert!(old.apply_head(&head, &points));
    old.rejudge(&points);
    assert_eq!(old.tip().seq, 1, "늦은 갱신을 버림");
    assert_eq!(old.verdict(&hidden.id), None);
    let mut fresh = GroupChain::from_head(SPACE, &head).expect("머리");
    let list = old.members_at(1, &key_commit(1)).expect("멤버");
    assert!(fresh.learn_members(1, &key_commit(1), &list));
    assert_eq!(fresh.saved_base(), old.saved_base(), "같은 바닥");
    assert_eq!(
        put_at(&mut fresh, &hidden, &points),
        Err(AuthError::LateEvent)
    );
    assert_eq!(
        put_at(&mut old, &hidden, &points),
        Err(AuthError::LateEvent)
    );
    // 확정 기기가 경계 위에서 다시 냅니다.
    let again = group_ev(
        w,
        o,
        &[],
        &[later],
        Some(f + 1),
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put_at(&mut old, &again, &points), accepted(&[again.id]));
    assert_eq!(put_at(&mut fresh, &again, &points), accepted(&[again.id]));
    assert_eq!(old.tip().seq, 2);
    assert_eq!(fresh.tip(), old.tip());
    // 머리가 없는 그룹(경계 없음)은 경계 아래 갱신도 그대로 받습니다.
    let mut headless = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut headless, &g1, &res), accepted(&[g1.id]));
    assert_eq!(put(&mut headless, &hidden, &res), accepted(&[hidden.id]));
}

// ---- 진짜 해소기로 노드마다 같은 판정 ----

/// 노드 하나: 해소기와 그 그룹 사슬.
struct Peer {
    name: &'static str,
    res: Resolver,
    gc: GroupChain,
}

/// 상태 이벤트를 모든 노드에 넣고 사슬을 다시 판정합니다.
fn insert_all(peers: &mut [Peer], evs: &[DecodedEvent]) {
    for p in peers.iter_mut() {
        p.res.insert_events(evs.to_vec());
        p.gc.rejudge(&p.res);
    }
}

/// 그룹 갱신을 모든 노드에 넣고, 넣은 결과·판정·사슬 끝·쓸 수 있는 끝이 노드마다 같은지 봅니다.
fn same_everywhere(peers: &mut [Peer], e: &DecodedEvent) -> Result<ChainChange, AuthError> {
    let got: Vec<_> = peers
        .iter_mut()
        .map(|p| put(&mut p.gc, e, &p.res))
        .collect();
    let (first, rest) = peers.split_first().expect("노드");
    for (p, r) in rest.iter().zip(&got[1..]) {
        assert_eq!(r, &got[0], "{}: 넣은 결과가 {}와 다름", p.name, first.name);
        assert_eq!(p.gc.verdict(&e.id), first.gc.verdict(&e.id), "{}", p.name);
        assert_eq!(p.gc.tip(), first.gc.tip(), "{}: 사슬 끝", p.name);
        assert_eq!(p.gc.send_tip(), first.gc.send_tip(), "{}", p.name);
    }
    got.into_iter().next().expect("노드")
}

/// 체크포인트에 그룹 머리를 실어 냅니다(쓴 기기의 사슬 끝).
fn issue_with_head(
    w: &mut World,
    r: &Resolver,
    o: &User,
    head: KeyHead,
) -> (DecodedEvent, Vec<u8>) {
    issue_with(w, r, o, o.list(), |b| {
        b.key_heads.push(head);
        b.key_heads.sort_by_key(|k| k.group);
    })
}

/// 처음부터 아는 노드, 체크포인트로 들어온 새 멤버, 저장한 바닥으로 다시 시작한 노드, 바닥 없이 다시 시작한 노드가
/// 머리 뒤 갱신을 똑같이 판정합니다(검토에서 노드마다 갈렸던 사례):
/// - 경계 아래 논리 시계·경계 아래 기준점·바탕의 조상만 가리킨 기준점은 어디서나 늦은 갱신(확정 기기의 미리 보기도).
/// - 머리 앞에 크루 전체 재키가 있었으면 머리 다음은 어디서나 새 T가 먼저.
/// - 바탕의 조상을 낀 기준점은 바탕 뒤 것만으로 세고, 되돌리기 검사도 바탕 뒤 것끼리라 어디서나 같음.
/// - 거짓 멤버 목록은 요약과 달라 버리고, 머리에서 배운 바닥의 크루 순번은 진짜 값(재가입 판정이 맞음).
#[test]
fn group_heads_judge_alike_on_every_node() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let genesis = fx.cr.genesis;
    let mut full = Resolver::with_genesis(genesis);
    full.insert_events(fx.cr.c.events.clone());
    let mut full_gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut full_gc, &g1, &full), accepted(&[g1.id]));
    // 크루 전체 재키(그룹은 다음에 새 T가 먼저) 뒤 기록을 늘려 경계를 k1 위로 올립니다.
    fx.cr.c.push(
        o,
        ku(crew_update(2, Some(fx.k1), full_rekey(0, vec![], vec![]))),
    );
    grow(&mut fx.cr.c, o, 20, 50);
    full.insert_events(fx.cr.c.events.clone());
    assert_eq!(full.key_view().expect("키").crew.seq, 2);
    full_gc.rejudge(&full);
    let head = full_gc.key_head().expect("그룹 머리");
    let (c1, snap) = issue_with_head(&mut fx.cr.c.w, &full, o, head);
    let phi = signed(&c1)
        .body
        .frontier
        .iter()
        .max_by_key(|f| f.lamport)
        .expect("경계 위 조상")
        .id;
    // 체크포인트를 받기 전에 받은 갱신(기준점이 바탕의 조상뿐): 머리를 맞출 때 늦은 갱신으로 버립니다.
    let early = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[phi],
        None,
        group_update(g, 2, Some(g1.id), 2, KeyUpdateKind::Refresh),
    );
    assert_eq!(put(&mut full_gc, &early, &full), accepted(&[early.id]));
    full.insert_events(vec![c1.clone()]);
    assert_eq!(full.base().map(|b| b.event), Some(c1.id));
    let floor = full.floor();
    assert_eq!(full.anchor_place(&phi), AnchorPlace::Covered);
    assert_eq!(full.anchor_place(&fx.k1), AnchorPlace::Below);
    assert!(full_gc.apply_head(&head, &full));
    full_gc.rejudge(&full);
    assert_eq!(full_gc.verdict(&early.id), None, "늦은 갱신으로 버림");
    assert_eq!(full_gc.tip().seq, 1);
    let saved = full_gc.saved_base();
    assert!(saved.members_known && saved.anchor.is_empty());
    assert_eq!((saved.seq, saved.crew_seq, saved.fresh_crew), (1, 1, 1));
    let list = full_gc
        .members_at(1, &head.root_commit)
        .expect("처음부터 아는 노드는 앎");

    // 새 멤버: 바닥 아래 기록은 논리 시계만 압니다(따라잡기로 받은 것).
    let mut fresh = Resolver::from_checkpoint(genesis, c1.clone(), &snap).unwrap();
    fresh.insert_events(fx.cr.c.events.clone());
    assert_eq!(fresh.anchor_place(&phi), AnchorPlace::Covered);
    assert_eq!(fresh.anchor_place(&fx.k1), AnchorPlace::Below);
    let mut new_gc = GroupChain::from_head(SPACE, &head).expect("그룹 머리");
    assert!(
        !new_gc.learn_members(1, &head.root_commit, &devs(&[o, a, b])),
        "거짓 목록(요약과 다름)"
    );
    assert!(!new_gc.learn_members(1, &head.root_commit, &devs(&[o])));
    assert!(new_gc.learn_members(1, &head.root_commit, &list));

    // 저장한 바닥으로 다시 시작한 노드.
    let mut again = Resolver::from_checkpoint(genesis, c1.clone(), &snap).unwrap();
    again.insert_events(fx.cr.c.events.clone());
    let back: ChainBase = postcard::from_bytes(&postcard::to_stdvec(&saved).unwrap()).unwrap();
    let mut again_gc = GroupChain::from_saved(SPACE, g, back);
    assert!(!again_gc.apply_head(&head, &again), "이미 그 머리");
    assert_eq!(
        put(&mut again_gc, &g1, &again),
        Err(AuthError::DuplicateCommit)
    );

    // 바닥을 저장하기 전에 다시 시작한 노드: 저장된 g1은 경계 아래라 늦은 갱신이고, 머리에서 바닥을 세워 멤버를 배움.
    let mut cold = Resolver::from_checkpoint(genesis, c1.clone(), &snap).unwrap();
    cold.insert_events(fx.cr.c.events.clone());
    let mut cold_gc = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut cold_gc, &g1, &cold), Err(AuthError::LateEvent));
    assert!(cold_gc.apply_head(&head, &cold));
    assert!(cold_gc.learn_members(1, &head.root_commit, &list));

    let mut peers = vec![
        Peer {
            name: "처음부터",
            res: full,
            gc: full_gc,
        },
        Peer {
            name: "새 멤버",
            res: fresh,
            gc: new_gc,
        },
        Peer {
            name: "저장한 바닥으로 다시 시작",
            res: again,
            gc: again_gc,
        },
        Peer {
            name: "바닥 없이 다시 시작",
            res: cold,
            gc: cold_gc,
        },
    ];
    insert_all(&mut peers, &[]);
    for p in &peers {
        assert_eq!(p.gc.saved_base(), saved, "{}: 바닥", p.name);
        assert_eq!(p.gc.send_tip(), peers[0].gc.send_tip(), "{}", p.name);
    }

    // 바탕 뒤 기록: x1 → x2, 그리고 바탕과 동시인 곁가지(바탕의 조상 φ에서 갈라짐).
    let w = &mut fx.cr.c.w;
    let x1 = w.ev(o, &[c1.id], text_channel(ch(90), "뒤 1"));
    let side = w.ev(o, &[phi], text_channel(ch(91), "곁가지"));
    let x2 = w.ev(o, &[x1.id], text_channel(ch(92), "뒤 2"));
    insert_all(&mut peers, &[x1.clone(), side.clone(), x2.clone()]);
    for p in &peers {
        for id in [x1.id, side.id, x2.id] {
            assert_eq!(p.res.anchor_place(&id), AnchorPlace::After, "{}", p.name);
        }
    }
    let up = |seq, prev, kind| group_update(g, seq, prev, 2, kind);
    let l = w.lamport_after(&[x1.id]);

    // 늦은 갱신: 경계 아래 논리 시계, 경계 아래 기준점, 바탕의 조상만 가리킨 기준점.
    let refresh = up(2, Some(g1.id), KeyUpdateKind::Refresh);
    let low = group_ev(w, o, &[], &[x1.id], Some(floor - 1), refresh.clone());
    let below = group_ev(w, o, &[], &[fx.k1], Some(l), refresh.clone());
    let only_phi = group_ev(w, o, &[], &[phi], Some(l), refresh.clone());
    for e in [&low, &below, &only_phi, &early] {
        assert_eq!(same_everywhere(&mut peers, e), Err(AuthError::LateEvent));
    }
    for p in &peers {
        let wa = |anchor: &[EventId], lamport| {
            p.gc.would_accept(&o.id(), &o.dev(), anchor, lamport, &refresh, &p.res)
        };
        assert_eq!(
            wa(&[x1.id], floor - 1),
            Err(AuthError::LateEvent),
            "{}",
            p.name
        );
        assert_eq!(wa(&[phi], l), Err(AuthError::LateEvent), "{}", p.name);
        assert_eq!(wa(&[phi, x1.id], l), Ok(()), "{}", p.name);
    }

    // 머리 앞 크루 전체 재키: 새 T가 아닌 갱신은 어디서나 거부.
    let not_fresh = group_ev(
        w,
        o,
        &[],
        &[x1.id],
        None,
        up(2, Some(g1.id), KeyUpdateKind::RRefresh),
    );
    assert_eq!(
        same_everywhere(&mut peers, &not_fresh),
        Ok(ChainChange::default())
    );
    assert_eq!(
        peers[0].gc.verdict(&not_fresh.id),
        Some(Err(AuthError::Invalid("크루 전체 재키 뒤에는 새 T가 먼저")))
    );

    // 바탕의 조상(φ)을 낀 기준점은 바탕 뒤 것(x1)만으로 셉니다. a는 크루 순번 1에 들어와 재가입이 아님(머리의 크루
    // 순번이 진짜 값이어야 정직한 갱신을 받음).
    let g2 = group_ev(w, o, &[], &[phi, x1.id], None, refresh.clone());
    assert_eq!(same_everywhere(&mut peers, &g2), accepted(&[g2.id]));

    // 되돌리기 검사는 앞 갱신 기준점의 바탕 뒤 것(x1)만: x1을 잇지 않는 곁가지는 어디서나 거부, 잇는 x2는 어디서나 받음.
    let back = group_ev(
        w,
        o,
        &[],
        &[side.id],
        None,
        up(3, Some(g2.id), KeyUpdateKind::RRefresh),
    );
    assert_eq!(
        same_everywhere(&mut peers, &back),
        Ok(ChainChange::default())
    );
    assert_eq!(
        peers[0].gc.verdict(&back.id),
        Some(Err(AuthError::Invalid(
            "기준점이 앞 갱신의 기준점을 잇지 않음"
        )))
    );
    let g3 = group_ev(
        w,
        o,
        &[],
        &[x2.id],
        None,
        up(3, Some(g2.id), KeyUpdateKind::RRefresh),
    );
    assert_eq!(same_everywhere(&mut peers, &g3), accepted(&[g3.id]));
    let t = peers[0].gc.tip();
    assert_eq!((t.seq, t.crew_seq, t.fresh_crew), (3, 2, 2));
    assert_eq!(t.members, set(&devs(&[o, a])));
}

/// 머리를 쓴 확정 기기가 체크포인트 뒤 관리자에서 내려오면, 그 머리는 어느 노드에서도 쓸 수 없습니다(머리에 작성자를
/// 실음). 지금 확정 기기가 이어 내면 다시 씁니다.
#[test]
fn demoted_head_author_is_unusable_everywhere() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let genesis = fx.cr.genesis;
    let admin = role(1, 10, Permissions::ADMINISTRATOR);
    fx.cr.c.push(
        o,
        EventBody::RoleCreate {
            role: admin.clone(),
        },
    );
    fx.cr.c.push(o, set_roles(a, vec![fx.viewer, admin.id]));
    let cc = fx
        .cr
        .c
        .push(o, EventBody::CommitterChange { device: a.dev() });
    let mut full = Resolver::with_genesis(genesis);
    full.insert_events(fx.cr.c.events.clone());
    let mut full_gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        &mut fx.cr.c.w,
        a,
        &[],
        &[cc],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut full_gc, &g1, &full), accepted(&[g1.id]));
    grow(&mut fx.cr.c, o, 20, 50);
    full.insert_events(fx.cr.c.events.clone());
    let head = full_gc.key_head().expect("머리");
    assert_eq!(
        head.chain.map(|c| (c.user, c.device)),
        Some((a.id(), a.dev()))
    );
    let (c1, snap) = issue_with_head(&mut fx.cr.c.w, &full, o, head);
    full.insert_events(vec![c1.clone()]);
    assert!(full_gc.apply_head(&head, &full));
    let list = full_gc.members_at(1, &head.root_commit).expect("멤버");
    let fresh = Resolver::from_checkpoint(genesis, c1.clone(), &snap).unwrap();
    let mut new_gc = GroupChain::from_head(SPACE, &head).expect("머리");
    assert!(new_gc.learn_members(1, &head.root_commit, &list));
    let mut peers = vec![
        Peer {
            name: "처음부터",
            res: full,
            gc: full_gc,
        },
        Peer {
            name: "새 멤버",
            res: fresh,
            gc: new_gc,
        },
    ];
    insert_all(&mut peers, &[]);
    for p in &peers {
        assert_eq!(
            p.gc.send_tip().map(|t| t.seq),
            Some(1),
            "{}: 작성자가 아직 관리자",
            p.name
        );
    }
    // a를 관리자에서 내림(확정 기기는 소유자 기기로 돌아감).
    let demote = fx.cr.c.w.ev(o, &[c1.id], set_roles(a, vec![fx.viewer]));
    insert_all(&mut peers, std::slice::from_ref(&demote));
    for p in &peers {
        assert_eq!(p.res.state().expect("상태").committer, o.dev());
        assert_eq!(p.gc.send_tip(), None, "{}: 내려온 작성자의 머리", p.name);
    }
    let g2 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[demote.id],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(same_everywhere(&mut peers, &g2), accepted(&[g2.id]));
    for p in &peers {
        assert_eq!(p.gc.send_tip().map(|t| t.seq), Some(2), "{}", p.name);
    }
}
