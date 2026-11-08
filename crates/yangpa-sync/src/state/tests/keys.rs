//! 키 갱신(`KeyUpdate`)·예비 기기(`KeyBackupSet`) 판정: 크루 사슬(공간 스트림, 해소기)과 비공개·덮개 그룹 사슬
//! ([`GroupChain`], 키 그룹 스트림).

use yangpa_core::keyupdate::{CrewRef, LayoutOp, SEALED_ENVELOPE_VERSION, SealedBox, Wrap};

use super::super::group::{KEEP_SEQS, MAX_FORKS};
use super::*;

fn devs(users: &[&User]) -> Vec<DeviceId> {
    let mut v: Vec<DeviceId> = users.iter().map(|u| u.dev()).collect();
    v.sort();
    v
}

fn set(devices: &[DeviceId]) -> imbl::OrdSet<DeviceId> {
    devices.iter().copied().collect()
}

fn advance(joined: Vec<DeviceId>) -> KeyUpdateKind {
    KeyUpdateKind::Advance { joined }
}

fn rotate(removed: Vec<DeviceId>, joined: Vec<DeviceId>) -> KeyUpdateKind {
    KeyUpdateKind::Rotate { removed, joined }
}

fn full_rekey(generation: u64, removed: Vec<DeviceId>, joined: Vec<DeviceId>) -> KeyUpdateKind {
    KeyUpdateKind::FullRekey {
        generation,
        removed,
        joined,
    }
}

fn ku(p: KeyUpdatePayload) -> EventBody {
    EventBody::KeyUpdate(p)
}

fn backup(device: Option<DeviceId>) -> EventBody {
    EventBody::KeyBackupSet { device }
}

fn kick(u: &User) -> EventBody {
    EventBody::MemberKick {
        user: u.id(),
        reason: String::new(),
    }
}

fn set_roles(u: &User, roles: Vec<RoleId>) -> EventBody {
    EventBody::MemberUpdate {
        user: u.id(),
        roles,
        nickname: None,
    }
}

fn crew_keys(r: &Resolved) -> &CrewKeys {
    &r.keys.as_ref().expect("키 사슬").crew
}

/// 봉인 상자(`yangpa_crypto::seal_to` 봉투 모양). 해소기는 수신 기기만 읽으므로 나머지는 채움 값입니다.
fn sealed_to(devices: &[DeviceId]) -> SealedBox {
    #[derive(serde::Serialize)]
    struct W {
        device: DeviceId,
        kem_ct: Vec<u8>,
        wrapped: Vec<u8>,
    }
    #[derive(serde::Serialize)]
    struct E {
        v: u8,
        wraps: Vec<W>,
        ct: Vec<u8>,
    }
    let e = E {
        v: SEALED_ENVELOPE_VERSION,
        wraps: devices
            .iter()
            .map(|d| W {
                device: *d,
                kem_ct: vec![1; 8],
                wrapped: vec![2; 8],
            })
            .collect(),
        ct: vec![3; 8],
    };
    SealedBox(postcard::to_stdvec(&e).unwrap())
}

/// 사슬 끝에 붙이되 `extra`도 부모로 둡니다(이어지는 조각은 머리를 부모로 둬야 함).
fn push_after(c: &mut Chain, author: &User, extra: EventId, body: EventBody) -> EventId {
    let head = c.head.expect("머리");
    let id = c.fork(author, &[head, extra], body);
    c.head = Some(id);
    id
}

/// 관리자 역할을 만들어 `u`에게 줍니다.
fn make_admin(c: &mut Chain, owner: &User, u: &User) -> RoleId {
    let admin = role(1, 10, Permissions::ADMINISTRATOR);
    c.push(
        owner,
        EventBody::RoleCreate {
            role: admin.clone(),
        },
    );
    c.push(owner, set_roles(u, vec![admin.id]));
    admin.id
}

// ---- 크루 사슬(공간 스트림) ----

#[test]
fn crew_key_chain_rules() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let all = devs(&[o, a, b]);
    let not_committer = cr.c.push(a, ku(crew_update(1, None, advance(all.clone()))));
    let other_group = cr.c.push(
        o,
        ku(key_payload(
            KeyGroupId::private(&SPACE, b"rule"),
            1,
            None,
            advance(all.clone()),
        )),
    );
    let mut with_crew = crew_update(1, None, advance(all.clone()));
    with_crew.crew = Some(CrewRef {
        seq: 0,
        root_commit: [0; 32],
    });
    let with_crew = cr.c.push(o, ku(with_crew));
    let gap =
        cr.c.push(o, ku(crew_update(2, None, KeyUpdateKind::Refresh)));
    let bad_prev = cr.c.push(
        o,
        ku(crew_update(1, Some(cr.genesis), advance(all.clone()))),
    );
    let outsider = cr.c.push(
        o,
        ku(crew_update(1, None, advance(devs(&[o, &User::new(9)])))),
    );
    let k1 = cr.c.push(o, ku(crew_update(1, None, advance(all.clone()))));
    let dup = cr.c.push(o, ku(crew_update(1, None, advance(all.clone()))));
    let mut bad_commit = crew_update(2, Some(k1), KeyUpdateKind::Refresh);
    bad_commit.prev_root_commit = [9; 32];
    let bad_commit = cr.c.push(o, ku(bad_commit));
    let again =
        cr.c.push(o, ku(crew_update(2, Some(k1), advance(vec![a.dev()]))));
    let k2 =
        cr.c.push(o, ku(crew_update(2, Some(k1), KeyUpdateKind::Refresh)));
    let k3 =
        cr.c.push(o, ku(crew_update(3, Some(k2), KeyUpdateKind::RRefresh)));
    // 키 갱신 본문은 상태 키로만 암호화합니다(크루 R이 바뀌는 갱신도 갱신 전 상태 키).
    let mut sealed = cr.c.w.ev(
        o,
        &[k3],
        ku(crew_update(4, Some(k3), KeyUpdateKind::Refresh)),
    );
    sealed.header.key = KeyRef::Sealed;
    let sealed = cr.c.push_event(sealed);

    let r = cr.c.resolve();
    assert_rejected(&r, not_committer, AuthError::NotCommitter);
    let only_crew = AuthError::Invalid("공간 스트림의 키 갱신은 크루 그룹만");
    assert_rejected(&r, other_group, only_crew.clone());
    assert_rejected(&r, with_crew, only_crew);
    assert_rejected(&r, gap, AuthError::Invalid("키 갱신 순번이 이어지지 않음"));
    let unlinked = AuthError::Invalid("앞 키 갱신과 이어지지 않음");
    assert_rejected(&r, bad_prev, unlinked.clone());
    assert_rejected(
        &r,
        outsider,
        AuthError::Invalid("활성 기기가 아닌 기기를 넣음"),
    );
    assert_rejected(&r, dup, AuthError::DuplicateCommit);
    assert_rejected(&r, bad_commit, unlinked);
    assert_rejected(&r, again, AuthError::AlreadyMember);
    assert_rejected(&r, sealed, AuthError::Malformed);
    assert_eq!(r.key_events, vec![k1, k2, k3]);
    let k = crew_keys(&r);
    assert_eq!((k.seq, k.head, k.root_commit), (3, Some(k3), key_commit(3)));
    assert_eq!(k.members, set(&all));
    assert!(all.iter().all(|d| k.joined_at.get(d) == Some(&1)));
    assert!(k.debt.is_empty() && !k.rekey_required && k.pending_head().is_none());
    assert_eq!(k.holders, set(&[o.dev()]));
    assert_eq!(state(&r).committer, o.dev());

    // 증분 해소기도 같은 사슬을 냅니다.
    let mut res = Resolver::new();
    for e in cr.c.events.clone() {
        res.insert_events(vec![e]);
    }
    assert_eq!(res.resolved(), r);
    assert_eq!(res.key_view(), r.keys);
}

/// 가입 기기는 그 기기를 활성으로 만든 이벤트가 갱신의 조상이어야 넣습니다(확정 기기가 본 가입만).
#[test]
fn joined_device_needs_an_ancestor_join() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let carol = User::new(4);
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    let join = cr.c.push(o, carol.join_body(None));
    // 가입과 동시(가입을 모름)지만 논리 시계가 커서 해소 순서로는 가입 뒤에 오는 갱신.
    let lamport = cr.c.w.lamport_after(&[join]) + 3;
    let blind = cr.c.w.ev_at(
        o.id(),
        o.dev(),
        &[k1],
        lamport,
        ku(crew_update(2, Some(k1), advance(vec![carol.dev()]))),
    );
    let blind = cr.c.push_event(blind);
    let seen = cr.c.fork(
        o,
        &[join, blind],
        ku(crew_update(2, Some(k1), advance(vec![carol.dev()]))),
    );
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        blind,
        AuthError::Invalid("가입 기기를 넣은 이벤트가 갱신의 조상이 아님"),
    );
    assert_applied(&r, seen);
    let k = crew_keys(&r);
    assert!(k.members.contains(&carol.dev()));
    assert_eq!(k.joined_at.get(&carol.dev()), Some(&2));

    // 어느 순서로 넣어도(조상 검사가 처리 묶음 밖의 이벤트를 볼 때도) 같습니다.
    for seed in 0..12 {
        let mut evs = cr.c.events.clone();
        Rng(seed).shuffle(&mut evs);
        let mut res = Resolver::new();
        for e in evs {
            res.insert_events(vec![e]);
        }
        assert_eq!(res.resolved(), r, "순서 {seed}");
    }
}

/// 크루 멤버였다가 활성 기기가 아니게 된 기기(빚)는 다음 갱신이 정확히 빼야 합니다.
#[test]
fn removals_must_cover_lost_devices() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let carol = User::new(4);
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    cr.c.push(o, carol.join_body(None));
    cr.c.push(o, kick(b));
    // 조상에 b의 추방이 있는데 덮지 않은 갱신들.
    let uncovered =
        cr.c.push(o, ku(crew_update(2, Some(k1), advance(vec![carol.dev()]))));
    let refresh =
        cr.c.push(o, ku(crew_update(2, Some(k1), KeyUpdateKind::Refresh)));
    let too_many = cr.c.push(
        o,
        ku(crew_update(
            2,
            Some(k1),
            rotate(vec![a.dev(), b.dev()], vec![carol.dev()]),
        )),
    );
    let too_few = cr.c.push(
        o,
        ku(crew_update(2, Some(k1), rotate(vec![], vec![carol.dev()]))),
    );
    let k2 = cr.c.push(
        o,
        ku(crew_update(
            2,
            Some(k1),
            rotate(vec![b.dev()], vec![carol.dev()]),
        )),
    );
    let r = cr.c.resolve();
    let mismatch = AuthError::Invalid("제거 목록이 자격을 잃은 기기와 다름");
    for id in [uncovered, refresh, too_many, too_few] {
        assert_rejected(&r, id, mismatch.clone());
    }
    assert_applied(&r, k2);
    assert_eq!(crew_keys(&r).members, set(&devs(&[o, a, &carol])));
    assert!(crew_keys(&r).debt.is_empty());

    // a가 추방됐다 다시 들어와도 빚은 남습니다(그 사이 새 키는 못 받았어도 옛 키를 알기 때문). 한 번 뺀 뒤 다시 넣습니다.
    cr.c.push(o, kick(a));
    cr.c.push(o, a.join_body_v(None, 2));
    let r = cr.c.resolve();
    assert_eq!(crew_keys(&r).debt, set(&[a.dev()]));
    assert!(state(&r).device_belongs(&a.id(), &a.dev()));
    let keep =
        cr.c.push(o, ku(crew_update(3, Some(k2), KeyUpdateKind::Refresh)));
    let both = cr.c.push(
        o,
        ku(crew_update(
            3,
            Some(k2),
            rotate(vec![a.dev()], vec![a.dev()]),
        )),
    );
    let k3 = cr.c.push(
        o,
        ku(crew_update(3, Some(k2), rotate(vec![a.dev()], vec![]))),
    );
    let k4 =
        cr.c.push(o, ku(crew_update(4, Some(k3), advance(vec![a.dev()]))));
    let r = cr.c.resolve();
    assert_rejected(&r, keep, mismatch);
    assert_rejected(
        &r,
        both,
        AuthError::Invalid("키 갱신 조각의 기기 목록이 겹침"),
    );
    assert_applied(&r, k3);
    assert_applied(&r, k4);
    let k = crew_keys(&r);
    assert_eq!(k.members, set(&devs(&[o, a, &carol])));
    assert_eq!(k.joined_at.get(&a.dev()), Some(&4));
    assert_eq!(k.joined_at.get(&carol.dev()), Some(&2));
}

/// 강등된 옛 확정 기기는 크루 키 표를 가지므로, 그 뒤 첫 크루 갱신은 전체 재키여야 합니다.
#[test]
fn demoted_ex_committer_forces_full_rekey() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    make_admin(&mut cr.c, o, a);
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    cr.c.push(o, EventBody::CommitterChange { device: a.dev() });
    let k2 =
        cr.c.push(a, ku(crew_update(2, Some(k1), KeyUpdateKind::Refresh)));
    cr.c.push(o, EventBody::CommitterChange { device: o.dev() });
    let r = cr.c.resolve();
    assert_applied(&r, k2);
    assert_eq!(crew_keys(&r).holders, set(&devs(&[o, a])));
    assert!(!crew_keys(&r).rekey_required);

    cr.c.push(o, set_roles(a, vec![]));
    let r = cr.c.resolve();
    assert!(crew_keys(&r).rekey_required);
    let refresh =
        cr.c.push(o, ku(crew_update(3, Some(k2), KeyUpdateKind::Refresh)));
    let rotate_only =
        cr.c.push(o, ku(crew_update(3, Some(k2), KeyUpdateKind::RRefresh)));
    // 직접 인코딩 전체 재키는 세대가 그대로, 트리를 새로 만들면 하나 오릅니다.
    let wrong_gen = cr.c.push(
        o,
        ku(crew_update(3, Some(k2), full_rekey(1, vec![], vec![]))),
    );
    let mut flat_tree = crew_update(3, Some(k2), full_rekey(0, vec![], vec![]));
    flat_tree.depth = 2;
    let flat_tree = cr.c.push(o, ku(flat_tree));
    let full = cr.c.push(
        o,
        ku(crew_update(3, Some(k2), full_rekey(0, vec![], vec![]))),
    );
    let k4 =
        cr.c.push(o, ku(crew_update(4, Some(full), KeyUpdateKind::Refresh)));
    let mut tree = crew_update(5, Some(k4), full_rekey(1, vec![], vec![]));
    tree.depth = 2;
    let k5 = cr.c.push(o, ku(tree));
    let r = cr.c.resolve();
    let first = AuthError::Invalid("표 보유 기기가 자격을 잃어 전체 재키가 먼저");
    assert_rejected(&r, refresh, first.clone());
    assert_rejected(&r, rotate_only, first);
    let generation = AuthError::Invalid("전체 재키 세대가 맞지 않음");
    assert_rejected(&r, wrong_gen, generation.clone());
    assert_rejected(&r, flat_tree, generation);
    for id in [full, k4, k5] {
        assert_applied(&r, id);
    }
    let k = crew_keys(&r);
    assert_eq!(k.holders, set(&[o.dev()]));
    assert!(!k.rekey_required);
    assert_eq!((k.last_rekey, k.generation, k.depth), (5, 1, 2));
}

/// 예비 기기가 쫓겨나면 제거만 하는 회전으로는 안 되고 전체 재키여야 합니다.
#[test]
fn removed_backup_holder_needs_full_rekey() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    make_admin(&mut cr.c, o, a);
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    let set_a = cr.c.push(o, backup(Some(a.dev())));
    let r = cr.c.resolve();
    assert_eq!(r.key_events, vec![k1, set_a]);
    assert_eq!(crew_keys(&r).backup, Some(a.dev()));
    assert_eq!(crew_keys(&r).holders, set(&devs(&[o, a])));

    cr.c.push(o, kick(a));
    let r = cr.c.resolve();
    let k = crew_keys(&r);
    assert_eq!(k.debt, set(&[a.dev()]));
    assert!(k.rekey_required && k.backup.is_none());
    let rotate_out = cr.c.push(
        o,
        ku(crew_update(2, Some(k1), rotate(vec![a.dev()], vec![]))),
    );
    let without = cr.c.push(
        o,
        ku(crew_update(2, Some(k1), full_rekey(0, vec![], vec![]))),
    );
    let full = cr.c.push(
        o,
        ku(crew_update(
            2,
            Some(k1),
            full_rekey(0, vec![a.dev()], vec![]),
        )),
    );
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        rotate_out,
        AuthError::Invalid("표 보유 기기가 자격을 잃어 전체 재키가 먼저"),
    );
    assert_rejected(
        &r,
        without,
        AuthError::Invalid("제거 목록이 자격을 잃은 기기와 다름"),
    );
    assert_applied(&r, full);
    let k = crew_keys(&r);
    assert_eq!(k.members, set(&devs(&[o, b])));
    assert_eq!(k.holders, set(&[o.dev()]));
    assert!(!k.rekey_required && k.debt.is_empty());
}

#[test]
fn key_backup_set_rules() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    make_admin(&mut cr.c, o, a);
    let not_owner = cr.c.push(a, backup(Some(a.dev())));
    let plain = cr.c.push(o, backup(Some(b.dev())));
    let unknown = cr.c.push(o, backup(Some(User::new(9).dev())));
    let committer = cr.c.push(o, backup(Some(o.dev())));
    let unchanged = cr.c.push(o, backup(None));
    let set_a = cr.c.push(o, backup(Some(a.dev())));
    let same = cr.c.push(o, backup(Some(a.dev())));
    let r = cr.c.resolve();
    assert_rejected(&r, not_owner, AuthError::NotOwner);
    assert_rejected(&r, plain, AuthError::InvalidCommitter);
    assert_rejected(&r, unknown, AuthError::InvalidCommitter);
    assert_rejected(
        &r,
        committer,
        AuthError::Invalid("확정 기기는 예비 기기가 될 수 없음"),
    );
    let same_err = AuthError::Invalid("예비 기기가 이미 그대로임");
    assert_rejected(&r, unchanged, same_err.clone());
    assert_rejected(&r, same, same_err);
    assert_eq!(crew_keys(&r).holders, set(&devs(&[o, a])));

    // 사슬이 없을 때 풀면 표를 받은 적이 없으므로 보유 기기에서도 빠집니다.
    let clear = cr.c.push(o, backup(None));
    let r = cr.c.resolve();
    assert_eq!(crew_keys(&r).holders, set(&[o.dev()]));
    let again = cr.c.push(o, backup(Some(a.dev())));
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    // 사슬이 생긴 뒤 풀면 이미 받은 표가 남아 보유 기기로 셉니다.
    let clear2 = cr.c.push(o, backup(None));
    let r = cr.c.resolve();
    assert_eq!(r.key_events, vec![set_a, clear, again, k1, clear2]);
    let k = crew_keys(&r);
    assert_eq!(k.backup, None);
    assert_eq!(k.holders, set(&devs(&[o, a])));
    assert!(!k.rekey_required);
    cr.c.push(o, set_roles(a, vec![]));
    assert!(crew_keys(&cr.c.resolve()).rekey_required);

    // 예비 기기가 확정 기기가 되면 예비 자리는 풀립니다.
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    make_admin(&mut cr.c, o, a);
    cr.c.push(o, backup(Some(a.dev())));
    cr.c.push(o, EventBody::CommitterChange { device: a.dev() });
    let r = cr.c.resolve();
    assert_eq!(crew_keys(&r).backup, None);
    assert_eq!(crew_keys(&r).holders, set(&[a.dev()]));
}

/// 나눈 갱신: 머리 조각이 사슬 검사를 받고, 이어지는 조각은 머리를 부모로 두고 같은 머리 해시여야 하며, 마지막 조각이
/// 들어올 때 합친 갱신을 판정합니다.
#[test]
fn multi_part_crew_update() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let (carol, dave, erin) = (User::new(4), User::new(5), User::new(6));
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    for u in [&carol, &dave, &erin] {
        cr.c.push(o, u.join_body(None));
    }
    let part = |n: u16, joined: Vec<DeviceId>| {
        let mut p = crew_update(2, Some(k1), advance(joined));
        p.parts = 3;
        p.part = n;
        p
    };
    let stray = cr.c.push(o, ku(part(1, vec![dave.dev()])));
    let head = cr.c.push(o, ku(part(0, vec![carol.dev()])));
    let r = cr.c.resolve();
    assert_applied(&r, head);
    assert_eq!(crew_keys(&r).pending_head(), Some(head));
    assert_eq!((crew_keys(&r).seq, r.key_events.clone()), (1, vec![k1]));

    let p1 = push_after(&mut cr.c, o, head, ku(part(1, vec![dave.dev()])));
    let p1_again = push_after(&mut cr.c, o, head, ku(part(1, vec![erin.dev()])));
    let overlap = push_after(&mut cr.c, o, head, ku(part(2, vec![dave.dev()])));
    let by_other = push_after(&mut cr.c, a, head, ku(part(2, vec![erin.dev()])));
    let mut other_salt = part(2, vec![erin.dev()]);
    other_salt.salt = [1; 24];
    let alien = push_after(&mut cr.c, o, head, ku(other_salt));
    // 머리를 부모로 두지 않은 조각.
    let orphan = cr.c.push(o, ku(part(2, vec![erin.dev()])));
    let p2 = push_after(&mut cr.c, o, head, ku(part(2, vec![erin.dev()])));
    let late = push_after(&mut cr.c, o, head, ku(part(2, vec![])));
    let r = cr.c.resolve();
    let unknown_head = AuthError::Invalid("키 갱신 머리 조각을 모름");
    assert_rejected(&r, stray, unknown_head.clone());
    assert_applied(&r, p1);
    assert_rejected(
        &r,
        p1_again,
        AuthError::Invalid("같은 키 갱신 조각이 두 번"),
    );
    assert_rejected(
        &r,
        overlap,
        AuthError::Invalid("키 갱신 조각의 기기 목록이 겹침"),
    );
    assert_rejected(&r, by_other, AuthError::NotCommitter);
    assert_rejected(&r, alien, unknown_head.clone());
    assert_rejected(&r, orphan, unknown_head.clone());
    assert_applied(&r, p2);
    assert_rejected(&r, late, unknown_head.clone());
    // 데몬에 알리는 것은 마지막 조각, 사슬 머리는 첫 조각입니다.
    assert_eq!(r.key_events, vec![k1, p2]);
    let k = crew_keys(&r);
    assert_eq!((k.seq, k.head), (2, Some(head)));
    assert_eq!(k.members, set(&devs(&[o, a, b, &carol, &dave, &erin])));
    assert!(k.pending_head().is_none());
    // 멤버는 해소기가 받아들인 바로 그 조각을 합칩니다(같은 번호의 다른 조각 p1_again이 아님).
    assert_eq!(k.parts_of(2).as_deref(), Some(&[head, p1, p2][..]));
    assert_eq!(k.tip_part, Some(p2));

    // 끝나지 않은 갱신은 같은 순번의 새 머리가 대신합니다.
    let mut unfinished = crew_update(3, Some(head), KeyUpdateKind::Refresh);
    unfinished.parts = 2;
    let unfinished = cr.c.push(o, ku(unfinished));
    let k3 =
        cr.c.push(o, ku(crew_update(3, Some(head), KeyUpdateKind::RRefresh)));
    let mut tail = crew_update(3, Some(head), KeyUpdateKind::Refresh);
    tail.parts = 2;
    tail.part = 1;
    tail.r_ct = None;
    let tail = push_after(&mut cr.c, o, unfinished, ku(tail));
    let r = cr.c.resolve();
    assert_applied(&r, unfinished);
    assert_applied(&r, k3);
    assert_rejected(&r, tail, unknown_head);
    assert_eq!(crew_keys(&r).head, Some(k3));
    assert_eq!(crew_keys(&r).parts_of(3).as_deref(), Some(&[k3][..]));
}

/// 같은 앞 갱신을 잇는 크루 갱신이 여럿이면 해소 순서에서 먼저 끝난 하나만 받아들이고, 어느 노드에서 어느 순서로
/// 받아도 같습니다.
#[test]
fn concurrent_same_seq_crew_updates_one_wins() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let carol = User::new(4);
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    let join = cr.c.push(o, carol.join_body(None));
    let base = cr.c.w.lamport_after(&[join]);
    let at = |cr: &mut Crew, lamport: u64, parents: &[EventId], p: KeyUpdatePayload| {
        let e = cr.c.w.ev_at(cr.o.id(), cr.o.dev(), parents, lamport, ku(p));
        let id = e.id;
        cr.c.events.push(e);
        id
    };
    let x = at(
        &mut cr,
        base,
        &[join],
        crew_update(2, Some(k1), advance(vec![carol.dev()])),
    );
    let mut yp = crew_update(2, Some(k1), KeyUpdateKind::Refresh);
    yp.root_commit = [0xaa; 32];
    let y = at(&mut cr, base + 1, &[join], yp);
    // 두 조각 갱신: 머리가 늦어 지고, 이어지는 조각은 머리를 모르는 채로 거부.
    let mut wp = crew_update(2, Some(k1), KeyUpdateKind::RRefresh);
    wp.parts = 2;
    let w_head = at(&mut cr, base + 2, &[join], wp.clone());
    wp.part = 1;
    wp.r_ct = None;
    let w_tail = at(&mut cr, base + 3, &[w_head], wp);
    let after_x = at(
        &mut cr,
        base + 5,
        &[x, y],
        crew_update(3, Some(x), KeyUpdateKind::RRefresh),
    );
    let mut ayp = crew_update(3, Some(y), KeyUpdateKind::RRefresh);
    ayp.prev_root_commit = [0xaa; 32];
    let after_y = at(&mut cr, base + 5, &[x, y], ayp);

    let r = cr.c.resolve();
    assert_applied(&r, x);
    assert_rejected(&r, y, AuthError::DuplicateCommit);
    assert_rejected(&r, w_head, AuthError::DuplicateCommit);
    assert_rejected(&r, w_tail, AuthError::Invalid("키 갱신 머리 조각을 모름"));
    assert_applied(&r, after_x);
    assert!(!applied(&r, after_y));
    assert_eq!(r.key_events, vec![k1, x, after_x]);

    for seed in 0..24u64 {
        let mut rng = Rng(seed);
        let mut evs = cr.c.events.clone();
        rng.shuffle(&mut evs);
        assert_eq!(resolve(&evs), r, "한 번에, 순서 {seed}");
        let mut res = if seed % 2 == 0 {
            Resolver::new()
        } else {
            Resolver::with_genesis(cr.genesis)
        };
        let mut i = 0;
        while i < evs.len() {
            let take = 1 + rng.below(3);
            res.insert_events(evs[i..(i + take).min(evs.len())].to_vec());
            i += take;
        }
        assert_eq!(res.resolved(), r, "증분, 순서 {seed}");
    }
}

/// 받는 기기(칸 채우기·봉인 상자 수신자)는 갱신 뒤 멤버여야 합니다.
#[test]
fn delivered_keys_stay_inside_the_group() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let carol = User::new(4);
    let all = devs(&[o, a, b]);
    let mut fill_out = crew_update(1, None, advance(all.clone()));
    fill_out.depth = 1;
    fill_out.layout = vec![LayoutOp::Fill {
        slot: 0,
        device: carol.dev(),
    }];
    let fill_out = cr.c.push(o, ku(fill_out));
    let mut seal_out = crew_update(1, None, advance(all.clone()));
    seal_out.sealed = vec![sealed_to(&[o.dev(), carol.dev()])];
    let seal_out = cr.c.push(o, ku(seal_out));
    let mut unreadable = crew_update(1, None, advance(all.clone()));
    unreadable.sealed = vec![SealedBox(vec![1, 2, 3])];
    let unreadable = cr.c.push(o, ku(unreadable));
    let mut k1 = crew_update(1, None, advance(all.clone()));
    k1.sealed = vec![sealed_to(&all)];
    let k1 = cr.c.push(o, ku(k1));
    cr.c.push(o, kick(b));
    let mut to_removed = crew_update(2, Some(k1), rotate(vec![b.dev()], vec![]));
    to_removed.sealed = vec![sealed_to(&all)];
    let to_removed = cr.c.push(o, ku(to_removed));
    let mut k2 = crew_update(2, Some(k1), rotate(vec![b.dev()], vec![]));
    k2.sealed = vec![sealed_to(&devs(&[o, a]))];
    let k2 = cr.c.push(o, ku(k2));
    let r = cr.c.resolve();
    let outside = AuthError::Invalid("그룹 밖 기기에게 키를 줌");
    assert_rejected(&r, fill_out, outside.clone());
    assert_rejected(&r, seal_out, outside.clone());
    assert_rejected(
        &r,
        unreadable,
        AuthError::Invalid("수신자를 읽을 수 없는 봉인 상자"),
    );
    assert_rejected(&r, to_removed, outside);
    assert_applied(&r, k1);
    assert_applied(&r, k2);
}

// ---- 비공개·덮개 그룹 사슬(키 그룹 스트림) ----

/// 비공개 그룹 시험 장치: 역할 viewer만 볼 수 있는 채널 하나(그룹 ID는 보기 규칙에서 나오므로 역할을 가진 사람이
/// 바뀌어도 그대로). a는 viewer, b는 못 봄. 크루 사슬은 k1(o·a·b)까지.
struct GroupFx {
    cr: Crew,
    group: KeyGroupId,
    viewer: RoleId,
    k1: EventId,
}

fn group_fx() -> GroupFx {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let viewer = role(5, 5, Permissions::NONE);
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: viewer.clone(),
        },
    );
    cr.c.push(o, set_roles(a, vec![viewer.id]));
    let mut settings = ChannelSettings::text("비밀");
    settings.overwrites = vec![
        PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        },
        PermissionOverwrite {
            target: OverwriteTarget::Role(viewer.id),
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::NONE,
        },
    ];
    // @everyone 보기를 거부한 채널이라 어떤 상태에서도 비공개 그룹을 씁니다.
    let group = yangpa_core::ids::KeyGroupId::private(&SPACE, &settings.view_rule());
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(9),
            settings,
        },
    );
    let k1 =
        cr.c.push(o, ku(crew_update(1, None, advance(devs(&[o, a, b])))));
    GroupFx {
        cr,
        group,
        viewer: viewer.id,
        k1,
    }
}

fn resolver(c: &Chain) -> Resolver {
    let mut res = Resolver::new();
    res.insert_events(c.events.clone());
    res
}

/// 해소기에 사슬의 새 이벤트를 넣습니다(이미 있는 것은 무시됨).
fn sync(res: &mut Resolver, c: &Chain) {
    res.insert_events(c.events.clone());
}

fn crew_ref(seq: u64) -> CrewRef {
    CrewRef {
        seq,
        root_commit: key_commit(seq),
    }
}

fn group_update(
    group: KeyGroupId,
    seq: u64,
    prev: Option<EventId>,
    crew: u64,
    kind: KeyUpdateKind,
) -> KeyUpdatePayload {
    let mut p = key_payload(group, seq, prev, kind);
    p.crew = Some(crew_ref(crew));
    p
}

/// 키 그룹 스트림 이벤트(해소기 밖). 논리 시계를 주지 않으면 기준점·부모 바로 다음입니다.
fn group_ev(
    w: &mut World,
    author: &User,
    parents: &[EventId],
    anchor: &[EventId],
    lamport: Option<u64>,
    p: KeyUpdatePayload,
) -> DecodedEvent {
    let mut anchor = anchor.to_vec();
    anchor.sort();
    anchor.dedup();
    let l = lamport.unwrap_or_else(|| w.lamport_after(&anchor).max(w.lamport_after(parents)));
    let group = p.group;
    let mut e = w.ev_at(author.id(), author.dev(), parents, l, ku(p));
    e.header.scope = Some(Scope::KeyGroup(group));
    e.header.state_anchor = anchor;
    e
}

fn put(gc: &mut GroupChain, e: &DecodedEvent, res: &Resolver) -> Result<ChainChange, AuthError> {
    let EventBody::KeyUpdate(p) = &e.body else {
        panic!("키 갱신이 아님");
    };
    gc.insert(e.id, &e.header, p, res)
}

fn accepted(ids: &[EventId]) -> Result<ChainChange, AuthError> {
    Ok(ChainChange {
        accepted: ids.to_vec(),
        dropped: Vec::new(),
    })
}

#[test]
fn group_chain_basic_rules() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let mut gc = GroupChain::new(SPACE, g);
    let anchor = [fx.k1];
    let w = &mut fx.cr.c.w;
    let good_p = group_update(g, 1, None, 1, advance(devs(&[o, a])));

    // 조각만의 검사(기준점 상태)에 걸리면 넣지 않습니다.
    let by_a = group_ev(w, a, &[], &anchor, None, good_p.clone());
    assert_eq!(put(&mut gc, &by_a, &res), Err(AuthError::NotCommitter));
    let unknown = group_ev(w, o, &[], &[EventId::hash_of(b"?")], None, good_p.clone());
    assert_eq!(put(&mut gc, &unknown, &res), Err(AuthError::AnchorUnknown));
    let other_p = group_update(
        KeyGroupId::private(&SPACE, b"other"),
        1,
        None,
        1,
        advance(vec![o.dev()]),
    );
    let other = group_ev(w, o, &[], &anchor, None, other_p);
    assert_eq!(
        put(&mut gc, &other, &res),
        Err(AuthError::Invalid("다른 키 그룹의 갱신"))
    );
    let mut no_crew = good_p.clone();
    no_crew.crew = None;
    let no_crew = group_ev(w, o, &[], &anchor, None, no_crew);
    assert_eq!(
        put(&mut gc, &no_crew, &res),
        Err(AuthError::Invalid("크루 상태가 없는 그룹 갱신"))
    );
    let mut sealed = group_ev(w, o, &[], &anchor, None, good_p.clone());
    sealed.header.key = KeyRef::Sealed;
    assert_eq!(put(&mut gc, &sealed, &res), Err(AuthError::Malformed));
    let far = group_ev(
        w,
        o,
        &[],
        &anchor,
        None,
        group_update(g, 2 * KEEP_SEQS as u64 + 1, None, 1, KeyUpdateKind::Refresh),
    );
    assert_eq!(
        put(&mut gc, &far, &res),
        Err(AuthError::Limit("키 갱신 순번이 너무 앞섬"))
    );

    // 다 모인 후보는 넣은 뒤 판정합니다: 자격 없는 기기(b는 채널을 못 봄), 기준점의 크루 사슬 끝과 다른 크루 상태.
    let with_b = group_ev(
        w,
        o,
        &[],
        &anchor,
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, b]))),
    );
    assert_eq!(put(&mut gc, &with_b, &res), Ok(ChainChange::default()));
    assert_eq!(
        gc.verdict(&with_b.id),
        Some(Err(AuthError::Invalid("자격 없는 기기를 넣음")))
    );
    let mut stale = good_p.clone();
    stale.crew = Some(CrewRef {
        seq: 1,
        root_commit: [7; 32],
    });
    let stale = group_ev(w, o, &[], &anchor, None, stale);
    assert_eq!(put(&mut gc, &stale, &res), Ok(ChainChange::default()));
    assert_eq!(
        gc.verdict(&stale.id),
        Some(Err(AuthError::Invalid("기준점의 크루 키 사슬 끝과 다름")))
    );
    let lamport = w.lamport_after(&anchor);
    let good = group_ev(w, o, &[], &anchor, Some(lamport), good_p.clone());
    assert_eq!(put(&mut gc, &good, &res), accepted(&[good.id]));
    assert_eq!(put(&mut gc, &good, &res), Ok(ChainChange::default()));
    let tip = gc.tip();
    assert_eq!((tip.seq, tip.head, tip.crew_seq), (1, Some(good.id), 1));
    assert_eq!(tip.root_commit, key_commit(1));
    assert_eq!(tip.members, set(&devs(&[o, a])));
    assert_eq!(gc.winners(), &[good.id]);

    // 앞 갱신을 아직 모르면 판정을 미루고, 앞 갱신과 이어지지 않으면 거부합니다.
    let orphan = group_ev(
        w,
        o,
        &[],
        &anchor,
        None,
        group_update(
            g,
            2,
            Some(EventId::hash_of(b"prev")),
            1,
            KeyUpdateKind::RRefresh,
        ),
    );
    assert_eq!(put(&mut gc, &orphan, &res), Ok(ChainChange::default()));
    assert_eq!(gc.verdict(&orphan.id), None);
    let mut broken = group_update(g, 2, Some(good.id), 1, KeyUpdateKind::RRefresh);
    broken.prev_root_commit = [5; 32];
    let broken = group_ev(w, o, &[], &anchor, None, broken);
    let no_prev = group_ev(
        w,
        o,
        &[],
        &anchor,
        None,
        group_update(g, 2, None, 1, KeyUpdateKind::RRefresh),
    );
    for e in [&broken, &no_prev] {
        assert_eq!(put(&mut gc, e, &res), Ok(ChainChange::default()));
        assert_eq!(
            gc.verdict(&e.id),
            Some(Err(AuthError::Invalid("앞 갱신과 이어지지 않음")))
        );
    }

    // 같은 순번 갈래는 작성 기기마다 순위가 좋은 MAX_FORKS개만 듭니다(이미 셋: with_b, stale, good). 늦은 논리
    // 시계라 이기지는 못함.
    for i in 0..(MAX_FORKS - 3) {
        let mut p = good_p.clone();
        p.salt = [i as u8 + 1; 24];
        let e = group_ev(w, o, &[], &anchor, Some(lamport + 10), p);
        assert_eq!(put(&mut gc, &e, &res), Ok(ChainChange::default()));
    }
    // 순위가 더 나쁜 것(더 늦은 논리 시계)은 거부되고, 더 좋은 것은 가장 나쁜 것을 밀어냅니다.
    let mut p = good_p.clone();
    p.salt = [0xee; 24];
    let worse = group_ev(w, o, &[], &anchor, Some(lamport + 20), p);
    assert_eq!(
        put(&mut gc, &worse, &res),
        Err(AuthError::Limit("같은 순번 키 갱신 갈래"))
    );
    let mut p = good_p.clone();
    p.salt = [0xef; 24];
    let better = group_ev(w, o, &[], &anchor, Some(lamport + 5), p);
    assert_eq!(put(&mut gc, &better, &res), Ok(ChainChange::default()));
    assert_eq!(gc.verdict(&better.id), Some(Ok(())));
    assert_eq!(gc.winners(), &[good.id]);
}

/// 제거 목록은 정확히 (앞 멤버 − 자격) ∪ (그 사이 크루에서 빠졌다 다시 들어온 앞 멤버)입니다.
#[test]
fn group_removals_are_exact() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let mut res = resolver(&fx.cr.c);
    let mut gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));

    // a가 역할을 잃음: 크루 멤버지만 그룹 자격은 없음.
    let demote = fx.cr.c.push(o, set_roles(a, vec![]));
    sync(&mut res, &fx.cr.c);
    let w = &mut fx.cr.c.w;
    let mismatch = Some(Err(AuthError::Invalid(
        "제거 목록이 자격을 잃은 기기와 다름",
    )));
    let keep = group_ev(
        w,
        o,
        &[],
        &[demote],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    let extra = group_ev(
        w,
        o,
        &[],
        &[demote],
        None,
        group_update(g, 2, Some(g1.id), 1, rotate(devs(&[o, a]), vec![])),
    );
    for e in [&keep, &extra] {
        assert_eq!(put(&mut gc, e, &res), Ok(ChainChange::default()));
        assert_eq!(gc.verdict(&e.id), mismatch);
    }
    let g2 = group_ev(
        w,
        o,
        &[],
        &[demote],
        None,
        group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![])),
    );
    assert_eq!(put(&mut gc, &g2, &res), accepted(&[g2.id]));
    assert_eq!(gc.tip().members, set(&[o.dev()]));

    // 다시 viewer가 되면 가입으로 넣습니다.
    let regrant = fx.cr.c.push(o, set_roles(a, vec![fx.viewer]));
    sync(&mut res, &fx.cr.c);
    let g3 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[regrant],
        None,
        group_update(g, 3, Some(g2.id), 1, advance(vec![a.dev()])),
    );
    assert_eq!(put(&mut gc, &g3, &res), accepted(&[g3.id]));

    // 크루에서 빠졌다 다시 들어온 기기는 지금 자격이 있어도 한 번 빠져야 합니다(그 사이 놓친 제거를 덮음).
    fx.cr.c.push(o, kick(a));
    let k2 = fx.cr.c.push(
        o,
        ku(crew_update(2, Some(fx.k1), rotate(vec![a.dev()], vec![]))),
    );
    fx.cr.c.push(o, a.join_body_v(None, 2));
    fx.cr.c.push(o, set_roles(a, vec![fx.viewer]));
    let k3 = fx
        .cr
        .c
        .push(o, ku(crew_update(3, Some(k2), advance(vec![a.dev()]))));
    sync(&mut res, &fx.cr.c);
    assert_eq!(rejection(&res.resolved(), k3), None);
    let w = &mut fx.cr.c.w;
    let stay = group_ev(
        w,
        o,
        &[],
        &[k3],
        None,
        group_update(g, 4, Some(g3.id), 3, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &stay, &res), Ok(ChainChange::default()));
    assert_eq!(gc.verdict(&stay.id), mismatch);
    let out = group_ev(
        w,
        o,
        &[],
        &[k3],
        None,
        group_update(g, 4, Some(g3.id), 3, rotate(vec![a.dev()], vec![])),
    );
    assert_eq!(put(&mut gc, &out, &res), accepted(&[out.id]));
    let back = group_ev(
        w,
        o,
        &[],
        &[k3],
        None,
        group_update(g, 5, Some(out.id), 3, advance(vec![a.dev()])),
    );
    assert_eq!(put(&mut gc, &back, &res), accepted(&[back.id]));
    assert_eq!(gc.tip().members, set(&devs(&[o, a])));
    assert_eq!(gc.tip().crew_seq, 3);
}

/// 크루 전체 재키 뒤 첫 그룹 갱신은 새 T여야 합니다(넘긴 표에 그룹의 T도 있었음).
#[test]
fn group_needs_fresh_t_after_crew_full_rekey() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let mut res = resolver(&fx.cr.c);
    let mut gc = GroupChain::new(SPACE, g);
    let mut g1 = group_update(g, 1, None, 1, advance(devs(&[o, a])));
    g1.depth = 2;
    let g1 = group_ev(&mut fx.cr.c.w, o, &[], &[fx.k1], None, g1);
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));
    let k2 = fx.cr.c.push(
        o,
        ku(crew_update(2, Some(fx.k1), full_rekey(0, vec![], vec![]))),
    );
    sync(&mut res, &fx.cr.c);
    let w = &mut fx.cr.c.w;
    let not_fresh = Some(Err(AuthError::Invalid("크루 전체 재키 뒤에는 새 T가 먼저")));
    let r_only = group_ev(
        w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 2, Some(g1.id), 2, KeyUpdateKind::RRefresh),
    );
    let mut tree_refresh = group_update(g, 2, Some(g1.id), 2, KeyUpdateKind::Refresh);
    tree_refresh.depth = 2;
    let tree_refresh = group_ev(w, o, &[], &[k2], None, tree_refresh);
    for e in [&r_only, &tree_refresh] {
        assert_eq!(put(&mut gc, e, &res), Ok(ChainChange::default()));
        assert_eq!(gc.verdict(&e.id), not_fresh);
    }
    let fresh = group_ev(
        w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 2, Some(g1.id), 2, KeyUpdateKind::Refresh),
    );
    assert_eq!(put(&mut gc, &fresh, &res), accepted(&[fresh.id]));
    let next = group_ev(
        w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 3, Some(fresh.id), 2, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &next, &res), accepted(&[next.id]));

    // 트리를 새로 만드는 전체 재키도 새 T입니다.
    let k3 = fx.cr.c.push(
        o,
        ku(crew_update(3, Some(k2), full_rekey(0, vec![], vec![]))),
    );
    sync(&mut res, &fx.cr.c);
    let mut rebuilt = group_update(g, 4, Some(next.id), 3, full_rekey(1, vec![], vec![]));
    rebuilt.depth = 2;
    let rebuilt = group_ev(&mut fx.cr.c.w, o, &[], &[k3], None, rebuilt);
    assert_eq!(put(&mut gc, &rebuilt, &res), accepted(&[rebuilt.id]));
}

/// 크루 표 보유 기기(여기서는 강등된 예비 기기)를 그룹에서 빼는 갱신은 직접 인코딩이어야 합니다(보유 기기는 크루
/// 잎·노드 키를 모두 계산할 수 있음).
#[test]
fn group_holder_removal_needs_direct_encoding() {
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
    let set_a = fx.cr.c.push(o, backup(Some(a.dev())));
    let mut res = resolver(&fx.cr.c);
    assert_eq!(res.key_view().unwrap().crew.holders, set(&devs(&[o, a])));
    let mut gc = GroupChain::new(SPACE, g);
    let mut g1 = group_update(g, 1, None, 1, advance(devs(&[o, a])));
    g1.depth = 2;
    let g1 = group_ev(&mut fx.cr.c.w, o, &[], &[set_a], None, g1);
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));

    let demote = fx.cr.c.push(o, set_roles(a, vec![]));
    sync(&mut res, &fx.cr.c);
    assert!(res.key_view().unwrap().crew.rekey_required);
    let w = &mut fx.cr.c.w;
    let needs_direct = Some(Err(AuthError::Invalid(
        "크루 표 보유 기기는 직접 인코딩으로만 뺌(크루 전체 재키가 먼저)",
    )));
    let mut tree = group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![]));
    tree.depth = 2;
    let tree = group_ev(w, o, &[], &[demote], None, tree);
    let mut wrapped = group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![]));
    wrapped.wraps = vec![Wrap {
        tag: [1; 8],
        ct: [2; 48],
    }];
    let wrapped = group_ev(w, o, &[], &[demote], None, wrapped);
    for e in [&tree, &wrapped] {
        assert_eq!(put(&mut gc, e, &res), Ok(ChainChange::default()));
        assert_eq!(gc.verdict(&e.id), needs_direct);
    }
    let mut direct = group_update(g, 2, Some(g1.id), 1, rotate(vec![a.dev()], vec![]));
    direct.sealed = vec![sealed_to(&[o.dev()])];
    let direct = group_ev(w, o, &[], &[demote], None, direct);
    assert_eq!(put(&mut gc, &direct, &res), accepted(&[direct.id]));
    assert_eq!(gc.tip().members, set(&[o.dev()]));
}

/// 같은 앞 갱신을 잇는 유효한 후보 가운데 (크루 순번 큰 것, 논리 시계 작은 것, ID 작은 것)이 이기고, 늦게 온
/// 후보가 이기면 사슬이 바뀝니다. 넣는 순서와 무관합니다.
#[test]
fn group_same_seq_winner_in_any_order() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let k2 = fx
        .cr
        .c
        .push(o, ku(crew_update(2, Some(fx.k1), KeyUpdateKind::Refresh)));
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let l = w.lamport_after(&[fx.k1]);
    let at1 = [fx.k1];
    let bad = group_ev(
        w,
        o,
        &[],
        &at1,
        Some(l),
        group_update(g, 1, None, 1, advance(devs(&[o, b]))),
    );
    let x = group_ev(
        w,
        o,
        &[],
        &at1,
        Some(l + 1),
        group_update(g, 1, None, 1, advance(vec![o.dev()])),
    );
    let y = group_ev(
        w,
        o,
        &[],
        &at1,
        Some(l + 4),
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    let zx = group_ev(
        w,
        o,
        &[],
        &at1,
        Some(l + 5),
        group_update(g, 2, Some(x.id), 1, advance(vec![a.dev()])),
    );
    let zy = group_ev(
        w,
        o,
        &[],
        &at1,
        Some(l + 5),
        group_update(g, 2, Some(y.id), 1, KeyUpdateKind::RRefresh),
    );
    // 더 새 크루 상태에 기댄 후보는 논리 시계가 커도 이깁니다.
    let newer = group_ev(
        w,
        o,
        &[],
        &[k2],
        Some(l + 30),
        group_update(g, 1, None, 2, advance(devs(&[o, a]))),
    );

    // 늦게 온 x가 이기면 y 갈래를 버립니다.
    let mut gc = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut gc, &y, &res), accepted(&[y.id]));
    assert_eq!(put(&mut gc, &zy, &res), accepted(&[zy.id]));
    assert_eq!(put(&mut gc, &bad, &res), Ok(ChainChange::default()));
    assert_eq!(
        put(&mut gc, &x, &res),
        Ok(ChainChange {
            accepted: vec![x.id],
            dropped: vec![y.id, zy.id],
        })
    );
    assert_eq!(put(&mut gc, &zx, &res), accepted(&[zx.id]));
    assert_eq!(gc.tip().members, set(&devs(&[o, a])));
    assert_eq!(
        put(&mut gc, &newer, &res),
        Ok(ChainChange {
            accepted: vec![newer.id],
            dropped: vec![x.id, zx.id],
        })
    );

    let without: Vec<&DecodedEvent> = vec![&bad, &x, &y, &zx, &zy];
    let mut with = without.clone();
    with.push(&newer);
    for seed in 0..30u64 {
        let mut rng = Rng(seed);
        for (set_, want) in [(&without, vec![x.id, zx.id]), (&with, vec![newer.id])] {
            let mut evs = set_.clone();
            rng.shuffle(&mut evs);
            let mut gc = GroupChain::new(SPACE, g);
            for e in evs {
                put(&mut gc, e, &res).unwrap();
            }
            assert_eq!(gc.winners(), want.as_slice(), "순서 {seed}");
        }
    }
}

/// 확정 기기가 바뀌어도 표를 가질 자격(소유자·관리자의 활성 기기)이 남은 옛 확정 기기의 갱신은 그대로 씁니다. 자격을
/// 잃은 작성 기기의 갱신은 쓰지 않다가(그 앞까지만 씀), 새 확정 기기의 갱신이 이어 붙으면 씁니다. 옛 기준점으로 낸
/// 갈래는 새 확정 기기가 먼저 낸 크루 갱신에 기댄 갱신에 집니다.
#[test]
fn group_usable_follows_holder_eligibility() {
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
    let mut gc = GroupChain::new(SPACE, g);
    let g1 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[made_admin],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));
    let t1 = gc.tip();

    // 소유자 기기 o는 확정 기기가 아니게 돼도 자격이 남아 g1을 그대로 씁니다.
    let cc = fx
        .cr
        .c
        .push(o, EventBody::CommitterChange { device: a.dev() });
    sync(&mut res, &fx.cr.c);
    assert_eq!(gc.rejudge(&res), ChainChange::default());
    assert_eq!(gc.winners(), &[g1.id]);
    assert_eq!(gc.send_tip(), Some(t1.clone()));

    // 새 확정 기기 a의 갱신, 그 뒤 a가 관리자에서 내려옴: a의 갱신은 쓸 수 없고 그 앞(g1)까지만 씀. 사슬 끝은 그대로.
    let g2 = group_ev(
        &mut fx.cr.c.w,
        a,
        &[],
        &[cc],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &g2, &res), accepted(&[g2.id]));
    fx.cr.c.push(o, set_roles(a, vec![]));
    sync(&mut res, &fx.cr.c);
    assert_eq!(res.state().unwrap().committer, o.dev());
    assert_eq!(
        gc.rejudge(&res),
        ChainChange {
            accepted: vec![],
            dropped: vec![g2.id],
        }
    );
    assert_eq!(gc.winners(), &[g1.id]);
    assert_eq!(gc.send_tip(), Some(t1.clone()));
    assert_eq!(gc.tip().seq, 2);
    let t2 = gc.tip();
    assert_eq!(gc.winning_at(2, &t2.root_commit), Some(t2.clone()));
    let mut other = t2.root_commit;
    other[0] ^= 1;
    assert_eq!(gc.winning_at(2, &other), None);

    // 내려온 a가 옛 기준점으로 낸 갈래: 그 기준점에서는 유효하지만 지금은 쓸 수 없음.
    let backdated = group_ev(
        &mut fx.cr.c.w,
        a,
        &[],
        &[cc],
        None,
        group_update(g, 3, Some(g2.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &backdated, &res), Ok(ChainChange::default()));
    assert_eq!(gc.verdict(&backdated.id), Some(Ok(())));
    assert_eq!(gc.winners(), &[g1.id]);

    // o가 전체 재키(a가 표 보유 자격을 잃음) 뒤 새 T로 이어 붙이면 앞 갱신도 씁니다.
    let k2 = fx.cr.c.push(
        o,
        ku(crew_update(2, Some(fx.k1), full_rekey(0, vec![], vec![]))),
    );
    sync(&mut res, &fx.cr.c);
    let g3 = group_ev(
        &mut fx.cr.c.w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 3, Some(g2.id), 2, rotate(vec![a.dev()], vec![])),
    );
    assert_eq!(put(&mut gc, &g3, &res), accepted(&[g2.id, g3.id]));
    assert_eq!(gc.winners(), &[g1.id, g2.id, g3.id]);
    let t3 = gc.tip();
    assert_eq!((t3.crew_seq, t3.fresh_crew), (2, 2));
    assert_eq!(gc.send_tip(), Some(t3));
}

/// 기준점은 앞 갱신의 기준점을 이어야 합니다(옛 기준점으로 그때의 권한을 되살리지 못하게).
#[test]
fn group_anchor_cannot_go_back() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let later = fx.cr.c.push(o, text_channel(ch(10), "다른"));
    let res = resolver(&fx.cr.c);
    let mut gc = GroupChain::new(SPACE, g);
    let w = &mut fx.cr.c.w;
    let g1 = group_ev(
        w,
        o,
        &[],
        &[later],
        None,
        group_update(g, 1, None, 1, advance(devs(&[o, a]))),
    );
    assert_eq!(put(&mut gc, &g1, &res), accepted(&[g1.id]));
    let back = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &back, &res), Ok(ChainChange::default()));
    assert_eq!(
        gc.verdict(&back.id),
        Some(Err(AuthError::Invalid(
            "기준점이 앞 갱신의 기준점을 잇지 않음"
        )))
    );
    let fwd = group_ev(
        w,
        o,
        &[],
        &[later],
        None,
        group_update(g, 2, Some(g1.id), 1, KeyUpdateKind::RRefresh),
    );
    assert_eq!(put(&mut gc, &fwd, &res), accepted(&[fwd.id]));
}

/// 덮개 그룹의 첫 갱신은 목록 없는 `Refresh`이고 멤버는 자격 있는 크루 기기 전부입니다.
#[test]
fn cover_group_first_refresh_takes_eligible_devices() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let mut gc = GroupChain::new(SPACE, g);
    let w = &mut fx.cr.c.w;
    let mut leak = group_update(g, 1, None, 1, KeyUpdateKind::Refresh);
    leak.sealed = vec![sealed_to(&devs(&[o, b]))];
    let leak = group_ev(w, o, &[], &[fx.k1], None, leak);
    assert_eq!(put(&mut gc, &leak, &res), Ok(ChainChange::default()));
    assert_eq!(
        gc.verdict(&leak.id),
        Some(Err(AuthError::Invalid("그룹 밖 기기에게 키를 줌")))
    );
    let later = w.lamport_after(&[fx.k1]) + 1;
    let c1 = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        Some(later),
        group_update(g, 1, None, 1, KeyUpdateKind::Refresh),
    );
    assert_eq!(put(&mut gc, &c1, &res), accepted(&[c1.id]));
    assert_eq!(gc.tip().members, set(&devs(&[o, a])));
    // 그 뒤의 `Refresh`는 멤버를 바꾸지 않습니다.
    let c2 = group_ev(
        w,
        o,
        &[],
        &[fx.k1],
        None,
        group_update(g, 2, Some(c1.id), 1, KeyUpdateKind::Refresh),
    );
    assert_eq!(put(&mut gc, &c2, &res), accepted(&[c2.id]));
    assert_eq!(gc.tip().members, set(&devs(&[o, a])));
}

/// 여러 조각 그룹 갱신: 머리보다 먼저 온 조각은 들고 있다가 붙이고, 같은 번호의 조각이 둘이면 받은 순서와 무관하게
/// 후보를 거부합니다.
#[test]
fn multi_part_group_update() {
    let mut fx = group_fx();
    let (o, a) = (&fx.cr.o, &fx.cr.a);
    let g = fx.group;
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let mut head_p = group_update(g, 1, None, 1, advance(vec![o.dev()]));
    head_p.parts = 2;
    let mut tail_p = head_p.clone();
    tail_p.part = 1;
    tail_p.kind = advance(vec![a.dev()]);
    tail_p.r_ct = None;
    let head = group_ev(w, o, &[], &[fx.k1], None, head_p);
    let tail = group_ev(w, o, &[head.id], &[fx.k1], None, tail_p.clone());
    // 머리를 가리키지 않는 조각은 영영 붙지 않습니다.
    let stray = group_ev(w, o, &[], &[fx.k1], None, tail_p.clone());
    let mut again_p = tail_p;
    again_p.kind = advance(vec![]);
    let again = group_ev(w, o, &[head.id], &[fx.k1], None, again_p);

    // 머리 → 조각.
    let mut gc = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut gc, &stray, &res), Ok(ChainChange::default()));
    assert_eq!(put(&mut gc, &head, &res), Ok(ChainChange::default()));
    assert_eq!(gc.verdict(&head.id), None);
    assert_eq!(gc.parts(&head.id), None);
    assert_eq!(put(&mut gc, &tail, &res), accepted(&[head.id]));
    assert_eq!(gc.parts(&head.id), Some(vec![head.id, tail.id]));
    assert_eq!(gc.tip().members, set(&devs(&[o, a])));

    // 조각 → 머리: 먼저 온 조각을 들고 있다가 붙입니다.
    let mut early = GroupChain::new(SPACE, g);
    assert_eq!(put(&mut early, &tail, &res), Ok(ChainChange::default()));
    assert_eq!(put(&mut early, &head, &res), accepted(&[head.id]));
    assert_eq!(early.parts(&head.id), Some(vec![head.id, tail.id]));

    // 같은 번호의 다른 조각: 어느 쪽을 먼저 받았든 후보를 거부합니다.
    let bad = Err(AuthError::Invalid("같은 키 갱신 조각이 두 번"));
    assert_eq!(
        put(&mut gc, &again, &res),
        Ok(ChainChange {
            accepted: vec![],
            dropped: vec![head.id],
        })
    );
    assert_eq!(gc.verdict(&head.id), Some(bad.clone()));
    let mut other = GroupChain::new(SPACE, g);
    for e in [&again, &head, &tail] {
        put(&mut other, e, &res).expect("조각을 받음");
    }
    assert_eq!(other.verdict(&head.id), Some(bad));
    assert!(other.winners().is_empty() && gc.winners().is_empty());
}

/// 여러 갈래·조각·거부될 후보를 어느 순서로 넣어도(머리를 모르는 조각은 나중에 다시 넣음) 같은 사슬이 됩니다.
#[test]
fn group_chain_is_order_independent() {
    let mut fx = group_fx();
    let (o, a, b) = (&fx.cr.o, &fx.cr.a, &fx.cr.b);
    let g = fx.group;
    let demote = fx.cr.c.push(o, set_roles(a, vec![]));
    let k2 = fx
        .cr
        .c
        .push(o, ku(crew_update(2, Some(fx.k1), KeyUpdateKind::Refresh)));
    let res = resolver(&fx.cr.c);
    let w = &mut fx.cr.c.w;
    let mut evs = Vec::new();
    let l = w.lamport_after(&[fx.k1]);
    let s1: Vec<DecodedEvent> = (0..3u8)
        .map(|i| {
            let mut p = group_update(g, 1, None, 1, advance(devs(&[o, a])));
            p.salt = [i; 24];
            group_ev(w, o, &[], &[fx.k1], Some(l + u64::from(i)), p)
        })
        .collect();
    for (i, parent) in s1.iter().enumerate() {
        // 갈래마다 a를 빼는 두 조각 갱신과 거부될 갱신(빼지 않음).
        let mut head_p = group_update(g, 2, Some(parent.id), 1, rotate(vec![a.dev()], vec![]));
        head_p.parts = 2;
        let mut tail_p = head_p.clone();
        tail_p.part = 1;
        tail_p.kind = rotate(vec![], vec![]);
        tail_p.r_ct = None;
        tail_p.sealed = vec![sealed_to(&[o.dev()])];
        let head = group_ev(w, o, &[], &[demote], Some(l + 10 + i as u64), head_p);
        let tail = group_ev(w, o, &[head.id], &[demote], None, tail_p);
        let keep = group_ev(
            w,
            o,
            &[],
            &[demote],
            Some(l + 9),
            group_update(g, 2, Some(parent.id), 1, KeyUpdateKind::RRefresh),
        );
        evs.extend([head, tail, keep]);
    }
    let late = group_ev(
        w,
        o,
        &[],
        &[k2],
        None,
        group_update(g, 1, None, 2, advance(devs(&[o, b]))),
    );
    evs.extend(s1);
    evs.push(late);

    let deliver = |order: &[DecodedEvent]| {
        let mut gc = GroupChain::new(SPACE, g);
        let mut queue: Vec<&DecodedEvent> = order.iter().collect();
        loop {
            let before = queue.len();
            queue.retain(|e| {
                put(&mut gc, e, &res) == Err(AuthError::Invalid("키 갱신 머리 조각을 모름"))
            });
            if queue.is_empty() || queue.len() == before {
                break;
            }
        }
        assert!(queue.is_empty());
        (gc.winners().to_vec(), gc.tip())
    };
    let want = deliver(&evs);
    assert_eq!(want.0.len(), 2);
    assert_eq!(want.1.members, set(&[o.dev()]));
    for seed in 0..40u64 {
        let mut order = evs.clone();
        Rng(seed).shuffle(&mut order);
        assert_eq!(deliver(&order), want, "순서 {seed}");
    }
}

mod regress;

mod checkpoint;
