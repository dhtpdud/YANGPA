//! 저장소 단위 테스트.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use yangpa_core::blob::{BlobManifest, MIN_CHUNK_SIZE};
use yangpa_core::body::{CustomEmoji, LinkEmbed, MessageBody, ReactionEmoji, Secret32};
use yangpa_core::device::{
    BoxPublicKey, DeviceEntry, DeviceKeys, DeviceList, DeviceSignKeys, MemberDevices, Profile,
};
use yangpa_core::event::{EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, EmojiId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelSettings, Member, Permissions, Role, SecurityLevel, SpaceKind, SpaceSettings, SpaceState,
};
use yangpa_core::sign::SigningKeypair;
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{LamportRange, RangeFingerprint};

use crate::testutil::{Fx, persist, view};
use crate::*;

fn temp() -> (Store, tempfile::TempDir) {
    Store::open_temp().expect("임시 저장소")
}

fn count(store: &Store, sql: &str, id: &EventId) -> i64 {
    store
        .read(|c| Ok(c.query_row(sql, [id.as_bytes()], |r| r.get(0))?))
        .unwrap()
}

// ---- 열기·키 ----

#[test]
fn wrong_key_fails_and_right_key_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    let fx = Fx::new();
    let e = fx.msg(vec![], 1, b"hello");
    {
        let s = Store::open(dir.path(), &key).unwrap();
        s.insert_events(&[persist(&e)]).unwrap();
        assert_eq!(
            s.schema_version().unwrap(),
            crate::schema::MIGRATIONS.len() as i64
        );
    }
    assert!(matches!(
        Store::open(dir.path(), &Secret32::random()),
        Err(StoreError::WrongKey)
    ));
    let s = Store::open(dir.path(), &key).unwrap();
    assert_eq!(s.get_event(&e.id()).unwrap(), Some(e));
    // 파일은 평문 SQLite 머리말로 시작하지 않습니다.
    let raw = std::fs::read(dir.path().join(DB_FILE_NAME)).unwrap();
    assert!(!raw.starts_with(b"SQLite format 3"));
}

#[test]
fn newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    {
        let s = Store::open(dir.path(), &key).unwrap();
        s.write(|tx| Ok(tx.execute_batch("PRAGMA user_version = 999")?))
            .unwrap();
    }
    assert!(matches!(
        Store::open(dir.path(), &key),
        Err(StoreError::SchemaTooNew { found: 999, .. })
    ));
}

#[test]
fn pragmas_are_applied() {
    let (s, _d) = temp();
    s.read(|c| {
        let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        assert_eq!(mode, "wal");
        let fk: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        assert_eq!(fk, 1);
        let sd: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
        assert_eq!(sd, 1);
        let ts: i64 = c.query_row("PRAGMA temp_store", [], |r| r.get(0))?;
        assert_eq!(ts, 2);
        Ok(())
    })
    .unwrap();
}

// ---- PersistentEvent ----

#[test]
fn persistent_event_rejects_volatile() {
    let fx = Fx::new();
    let e = fx.msg(vec![], 1, b"x");
    assert!(matches!(
        PersistentEvent::new(e.clone(), SecurityLevel::Volatile),
        Err(StoreError::VolatileRejected)
    ));
    // 휘발성 세션 키로 암호화된 이벤트는 등급을 잘못 넘겨도 거부.
    let v = fx.volatile_msg(1, 1, b"x");
    assert!(matches!(
        PersistentEvent::new(v, SecurityLevel::Normal),
        Err(StoreError::VolatileRejected)
    ));
    // 본문 해시가 헤더와 다르면 거부.
    let mut bad = e.clone();
    bad.body = Some(b"y".to_vec());
    assert!(matches!(
        PersistentEvent::new(bad, SecurityLevel::Normal),
        Err(StoreError::InvalidInput(_))
    ));
    // 공간 수준 생성자는 채널 스트림 이벤트를 거부.
    assert!(PersistentEvent::space_level(e.clone()).is_err());
    let st = fx.kind_event(EventKind::RoleCreate, vec![], 1);
    assert_eq!(
        PersistentEvent::space_level(st.clone()).unwrap().id(),
        st.id()
    );
    // 정상.
    let p = PersistentEvent::new(e.clone(), SecurityLevel::Normal).unwrap();
    assert_eq!(p.id(), e.id());
    assert_eq!(p.into_inner(), e);
}

// ---- 이벤트 저장 ----

#[test]
fn insert_get_roundtrip_and_dedup() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let a = fx.msg(vec![], 1, b"body-a");
    let b = fx.msg(vec![a.id()], 2, b"body-b");
    let out = s
        .insert_events(&[persist(&a), persist(&b), persist(&a)])
        .unwrap();
    assert_eq!(
        out,
        vec![
            InsertOutcome::Inserted,
            InsertOutcome::Inserted,
            InsertOutcome::Duplicate
        ]
    );
    let got = s.get_event(&b.id()).unwrap().unwrap();
    assert_eq!(got, b);
    assert_eq!(got.id(), b.id());
    got.verify_with(&DeviceSignKeys::of(&fx.dev)).unwrap();
    assert_eq!(
        s.get_events(&[b.id(), EventId::random(), a.id()]).unwrap(),
        vec![b.clone(), a.clone()]
    );
    assert_eq!(
        s.has_events(&[a.id(), EventId::random()]).unwrap(),
        vec![true, false]
    );
    assert_eq!(s.event_count(&fx.space).unwrap(), 2);
    // 저장된 본문은 전송 암호문 그대로가 아니라 저장 키로 다시 봉인돼 있습니다.
    let sealed: Vec<u8> = s
        .read(|c| {
            Ok(c.query_row(
                "SELECT body_sealed FROM events WHERE id = ?1",
                [a.id().as_bytes()],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_ne!(sealed, b"body-a");
    assert_eq!(
        sealed.len(),
        b"body-a".len() + yangpa_crypto::aead::OVERHEAD
    );
    let meta = s.event_meta(&a.id()).unwrap().unwrap();
    assert!(meta.has_body && !meta.erased);
    assert_eq!(meta.stream, StreamId::Channel(fx.channel));
}

#[test]
fn header_only_event_gets_body_later() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let e = fx.msg(vec![], 1, b"later");
    let out = s.insert_events(&[persist(&e.without_body())]).unwrap();
    assert_eq!(out, vec![InsertOutcome::Inserted]);
    assert_eq!(s.get_event(&e.id()).unwrap().unwrap().body, None);
    assert_eq!(
        s.insert_events(&[persist(&e)]).unwrap(),
        vec![InsertOutcome::BodyAdded]
    );
    assert_eq!(s.get_event(&e.id()).unwrap(), Some(e.clone()));
    assert_eq!(
        s.insert_events(&[persist(&e)]).unwrap(),
        vec![InsertOutcome::Duplicate]
    );
}

fn heads_set(s: &Store, space: &SpaceId, stream: StreamId) -> BTreeSet<EventId> {
    s.heads(space, stream).unwrap().into_iter().collect()
}

fn missing_set(s: &Store, space: &SpaceId, stream: StreamId) -> BTreeSet<EventId> {
    s.missing_parents(space, stream)
        .unwrap()
        .into_iter()
        .collect()
}

#[test]
fn heads_and_missing_out_of_order() {
    let fx = Fx::new();
    let st = StreamId::Channel(fx.channel);
    // a <- b <- c, b <- d (갈래)
    let a = fx.msg(vec![], 1, b"a");
    let b = fx.msg(vec![a.id()], 2, b"b");
    let c = fx.msg(vec![b.id()], 3, b"c");
    let d = fx.msg(vec![b.id()], 3, b"d");

    let (s, _d) = temp();
    s.insert_events(&[persist(&c)]).unwrap();
    assert_eq!(heads_set(&s, &fx.space, st), [c.id()].into());
    assert_eq!(missing_set(&s, &fx.space, st), [b.id()].into());
    let md = s.missing_details(&fx.space, st).unwrap();
    assert_eq!((md[0].child, md[0].child_lamport), (c.id(), 3));

    s.insert_events(&[persist(&b)]).unwrap();
    assert_eq!(
        heads_set(&s, &fx.space, st),
        [c.id()].into(),
        "자식이 있는 b는 머리가 아님"
    );
    assert_eq!(missing_set(&s, &fx.space, st), [a.id()].into());

    s.insert_events(&[persist(&d)]).unwrap();
    assert_eq!(heads_set(&s, &fx.space, st), [c.id(), d.id()].into());

    s.insert_events(&[persist(&a)]).unwrap();
    assert_eq!(heads_set(&s, &fx.space, st), [c.id(), d.id()].into());
    assert!(missing_set(&s, &fx.space, st).is_empty());

    // 순서대로 넣은 결과와 같고, 한 묶음 안의 역순도 같습니다.
    let (s2, _d2) = temp();
    s2.insert_events(&[persist(&a), persist(&b), persist(&c), persist(&d)])
        .unwrap();
    let (s3, _d3) = temp();
    s3.insert_events(&[persist(&d), persist(&c), persist(&b), persist(&a)])
        .unwrap();
    for other in [&s2, &s3] {
        assert_eq!(heads_set(other, &fx.space, st), [c.id(), d.id()].into());
        assert!(missing_set(other, &fx.space, st).is_empty());
    }
    // 머리 순서: 논리 시계 내림차순.
    let hl = s.heads_with_lamport(&fx.space, st).unwrap();
    assert!(hl.iter().all(|(l, _)| *l == 3));
}

#[test]
fn streams_are_isolated() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let other = ChannelId::random();
    let a = fx.msg(vec![], 1, b"a");
    let x = fx.msg_in(other, vec![], 2, b"x");
    // 다른 채널 이벤트를 부모로 가리키는 잘못된 이벤트: a의 머리 상태를 건드리지 않고 빈 구간도 만들지 않음.
    let bogus = fx.msg_in(other, vec![a.id()], 3, b"bogus");
    let st_event = fx.kind_event(EventKind::ChannelCreate, vec![], 4);
    s.insert_events(&[
        persist(&a),
        persist(&x),
        persist(&bogus),
        persist(&st_event),
    ])
    .unwrap();
    assert_eq!(
        heads_set(&s, &fx.space, StreamId::Channel(fx.channel)),
        [a.id()].into()
    );
    assert_eq!(
        heads_set(&s, &fx.space, StreamId::Channel(other)),
        [x.id(), bogus.id()].into()
    );
    assert!(missing_set(&s, &fx.space, StreamId::Channel(other)).is_empty());
    assert_eq!(
        heads_set(&s, &fx.space, StreamId::Space),
        [st_event.id()].into()
    );
    assert_eq!(s.max_lamport(&fx.space).unwrap(), 4);
    assert_eq!(
        s.stream_max_lamport(&fx.space, StreamId::Channel(fx.channel))
            .unwrap(),
        1
    );
    assert_eq!(s.max_lamport(&SpaceId::random()).unwrap(), 0);
    // 같은 ID의 부모가 나중에 다른 스트림에서 와도 빈 구간이 영원히 남지 않음.
    let fx2 = Fx {
        space: fx.space,
        channel: fx.channel,
        dev: fx.dev.clone(),
        user: fx.user.clone(),
    };
    let later = fx2.msg(vec![], 10, b"later");
    let child = fx2.msg_in(other, vec![later.id()], 11, b"child");
    s.insert_events(&[persist(&child)]).unwrap();
    assert_eq!(
        missing_set(&s, &fx.space, StreamId::Channel(other)),
        [later.id()].into()
    );
    s.insert_events(&[persist(&later)]).unwrap();
    assert!(missing_set(&s, &fx.space, StreamId::Channel(other)).is_empty());
}

#[test]
fn fingerprints_match_naive_and_ranges() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let other = ChannelId::random();
    let mut evs = Vec::new();
    for i in 0..300u64 {
        // 같은 논리 시계도 섞음.
        let l = (i * 7919) % 97 + 1;
        evs.push(fx.msg(vec![], l, &i.to_be_bytes()));
    }
    let noise: Vec<SignedEvent> = (0..50u64)
        .map(|i| fx.msg_in(other, vec![], i + 1, b"n"))
        .collect();
    let all: Vec<PersistentEvent> = evs.iter().chain(noise.iter()).map(persist).collect();
    s.insert_events(&all).unwrap();
    let st = StreamId::Channel(fx.channel);
    let ranges = [
        LamportRange {
            start: 0,
            end: u64::MAX,
        },
        LamportRange { start: 1, end: 10 },
        LamportRange { start: 10, end: 50 },
        LamportRange { start: 50, end: 98 },
        LamportRange {
            start: 98,
            end: 200,
        },
        LamportRange { start: 5, end: 5 },
    ];
    let fps = s.fingerprints(&fx.space, st, &ranges).unwrap();
    for (fp, r) in fps.iter().zip(ranges) {
        let mut naive = RangeFingerprint {
            range: r,
            count: 0,
            xor: [0; 32],
        };
        for e in evs
            .iter()
            .filter(|e| e.header.lamport >= r.start && e.header.lamport < r.end)
        {
            naive.count += 1;
            for (x, b) in naive.xor.iter_mut().zip(e.id().0) {
                *x ^= b;
            }
        }
        assert_eq!(*fp, naive, "구간 {r:?}");
    }
    assert_eq!(fps[0].count, 300);
    // ids_in_range: (논리 시계, ID) 오름차순, 상한.
    let ids = s
        .ids_in_range(&fx.space, st, LamportRange { start: 10, end: 50 }, 1000)
        .unwrap();
    let mut expect: Vec<(u64, EventId)> = evs
        .iter()
        .filter(|e| (10..50).contains(&e.header.lamport))
        .map(|e| (e.header.lamport, e.id()))
        .collect();
    expect.sort();
    assert_eq!(ids, expect);
    let limited = s
        .ids_in_range(&fx.space, st, LamportRange { start: 10, end: 50 }, 5)
        .unwrap();
    assert_eq!(limited, expect[..5].to_vec());
    let range_events = s
        .stream_events(&fx.space, st, LamportRange { start: 10, end: 50 }, 3)
        .unwrap();
    assert_eq!(
        range_events
            .iter()
            .map(|e| (e.header.lamport, e.id()))
            .collect::<Vec<_>>(),
        expect[..3].to_vec()
    );
}

#[test]
fn events_by_lane_and_kind() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 3, b"m");
    let st1 = fx.kind_event(EventKind::SpaceCreate, vec![], 1);
    let st2 = fx.kind_event(EventKind::RoleCreate, vec![st1.id()], 2);
    let ku = fx.kind_event(EventKind::KeyUpdate, vec![st2.id()], 4);
    let ks = fx.kind_event(EventKind::KeyShare, vec![], 5);
    s.insert_events(&[
        persist(&m),
        persist(&ku),
        persist(&st2),
        persist(&st1),
        persist(&ks),
    ])
    .unwrap();
    let state = s
        .events_by_lane(&fx.space, &[Lane::State, Lane::KeyAgreement])
        .unwrap();
    assert_eq!(state, vec![st1.clone(), st2.clone(), ku.clone()]);
    assert_eq!(
        s.events_by_kind(&fx.space, &[Lane::State, Lane::KeyAgreement])
            .unwrap(),
        state,
        "architecture.md 이름도 같은 결과"
    );
    assert_eq!(
        s.events_by_lane(&fx.space, &[Lane::Message]).unwrap(),
        vec![m.clone()]
    );
    assert_eq!(s.events_by_lane(&fx.space, &[]).unwrap(), vec![]);
    assert_eq!(
        s.events_by_kinds(&fx.space, &[EventKind::RoleCreate, EventKind::KeyShare])
            .unwrap(),
        vec![st2, ks]
    );
}

// ---- 암호학적 삭제 ----

#[test]
fn erase_body_is_irrecoverable_but_header_stays() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let e = fx.msg(vec![], 1, b"erase-me-body-marker");
    s.insert_events_with_views(&[persist(&e)], &[view(&e, "erase-me-view-marker")])
        .unwrap();
    assert!(s.get_message(&e.id(), None).unwrap().is_some());
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM msg_keys WHERE event_id = ?1",
            &e.id()
        ),
        1
    );

    assert!(s.erase_body(&e.id()).unwrap());
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM msg_keys WHERE event_id = ?1",
            &e.id()
        ),
        0
    );
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE id = ?1 AND body_sealed IS NOT NULL",
            &e.id()
        ),
        0
    );
    assert!(s.get_message(&e.id(), None).unwrap().is_none());
    let got = s.get_event(&e.id()).unwrap().unwrap();
    assert_eq!(got.body, None);
    assert_eq!(got.header, e.header);
    assert_eq!(got.id(), e.id());
    assert!(s.event_meta(&e.id()).unwrap().unwrap().erased);
    // 다시 받아도 되살아나지 않고, 보기 행도 다시 만들어지지 않음.
    assert_eq!(
        s.insert_events(&[persist(&e)]).unwrap(),
        vec![InsertOutcome::Duplicate]
    );
    assert_eq!(s.get_event(&e.id()).unwrap().unwrap().body, None);
    assert!(!s.put_message_view(&view(&e, "again")).unwrap());
    // DAG는 유지.
    assert_eq!(
        s.heads(&fx.space, StreamId::Channel(fx.channel)).unwrap(),
        vec![e.id()]
    );
    // 디스크 어디에도 남지 않음.
    assert!(
        s.scan_for_marker(b"erase-me-body-marker")
            .unwrap()
            .is_empty()
    );
    assert!(
        s.scan_for_marker(b"erase-me-view-marker")
            .unwrap()
            .is_empty()
    );
    assert!(!s.erase_body(&EventId::random()).unwrap());
}

// ---- 메시지 보기 ----

#[test]
fn list_messages_keyset_pagination() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let mut evs = Vec::new();
    for i in 0..25u64 {
        evs.push(fx.msg(vec![], i / 2 + 1, &i.to_be_bytes())); // 논리 시계가 같은 쌍 포함
    }
    let views: Vec<MessageRecord> = evs
        .iter()
        .enumerate()
        .map(|(i, e)| view(e, &format!("m{i}")))
        .collect();
    let pe: Vec<PersistentEvent> = evs.iter().map(persist).collect();
    s.insert_events_with_views(&pe, &views).unwrap();
    let mut expect: Vec<(u64, EventId)> = evs.iter().map(|e| (e.header.lamport, e.id())).collect();
    expect.sort();
    expect.reverse();

    let mut got = Vec::new();
    let mut before = None;
    loop {
        let page = s.list_messages(&fx.space, &fx.channel, before, 10).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 10);
        before = page.last().map(|m| (m.lamport, m.id));
        got.extend(page.into_iter().map(|m| {
            let text = m.content.as_ref().unwrap().body.text.clone();
            (m.lamport, m.id, text)
        }));
    }
    assert_eq!(
        got.iter().map(|(l, id, _)| (*l, *id)).collect::<Vec<_>>(),
        expect
    );
    // 내용이 올바르게 풀림.
    let by_id: HashMap<EventId, String> = got.into_iter().map(|(_, id, t)| (id, t)).collect();
    for (i, e) in evs.iter().enumerate() {
        assert_eq!(by_id[&e.id()], format!("m{i}"));
    }
    assert_eq!(
        s.message_position(&evs[3].id()).unwrap(),
        Some((evs[3].header.lamport, evs[3].id()))
    );
    // 다른 채널은 비어 있음.
    assert!(
        s.list_messages(&fx.space, &ChannelId::random(), None, 10)
            .unwrap()
            .is_empty()
    );
    // 이미 있는 보기는 바꾸지 않음.
    assert!(!s.put_message_view(&view(&evs[0], "덮어쓰기")).unwrap());
    // 이벤트 없이 보기만 넣을 수 없음.
    let lonely = fx.msg(vec![], 99, b"z");
    assert!(matches!(
        s.put_message_view(&view(&lonely, "x")),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn anonymous_flag_comes_from_the_signed_kind() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let anon = fx.kind_event(EventKind::AnonMessage, vec![], 1);
    let plain = fx.msg(vec![anon.id()], 2, b"x");
    let other = fx.msg(vec![plain.id()], 3, b"y");
    s.insert_events(&[persist(&anon), persist(&plain), persist(&other)])
        .unwrap();
    let v = view(&anon, "익명");
    assert!(v.anonymous);
    assert!(s.put_message_view(&v).unwrap());
    assert!(s.put_message_view(&view(&plain, "이름")).unwrap());
    assert!(s.get_message(&anon.id(), None).unwrap().unwrap().anonymous);
    assert!(!s.get_message(&plain.id(), None).unwrap().unwrap().anonymous);
    let listed = s.list_messages(&fx.space, &fx.channel, None, 10).unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|m| m.anonymous)
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![anon.id()]
    );
    // 헤더와 다른 익명 표시·종류는 거부합니다(이름 붙은 글을 익명으로, 또는 그 반대로 바꾸지 못하게).
    let mut forged = view(&other, "z");
    forged.anonymous = true;
    assert!(matches!(
        s.put_message_view(&forged),
        Err(StoreError::InvalidInput(_))
    ));
    let mut wrong_kind = view(&other, "z");
    wrong_kind.kind = MessageKind::ForumPost;
    assert!(matches!(
        s.put_message_view(&wrong_kind),
        Err(StoreError::InvalidInput(_))
    ));
}

#[test]
fn edits_are_lww_pending_and_erased_with_delete() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"orig");
    let e1 = fx.kind_event(EventKind::Edit, vec![m.id()], 2);
    let e2 = fx.kind_event(EventKind::Edit, vec![e1.id()], 3);
    // 수정이 원본 보기보다 먼저 도착.
    s.insert_events(&[persist(&m), persist(&e1), persist(&e2)])
        .unwrap();
    let up = |e: &SignedEvent, text: &str| EditUpdate {
        event: e.id(),
        lamport: e.header.lamport,
        at: Timestamp(e.header.lamport * 1000),
        text: text.into(),
        embeds: vec![LinkEmbed {
            url: "https://example.com".into(),
            title: String::new(),
            description: String::new(),
            site_name: String::new(),
            thumbnail: vec![],
            player_url: None,
        }],
    };
    assert!(!s.apply_edit(&m.id(), &up(&e2, "second")).unwrap());
    assert!(!s.apply_edit(&m.id(), &up(&e1, "first")).unwrap());
    s.put_message_view(&view(&m, "orig")).unwrap();
    let got = s.get_message(&m.id(), None).unwrap().unwrap();
    assert_eq!(
        got.content.as_ref().unwrap().body.text,
        "second",
        "보관된 수정 중 가장 새 것 적용"
    );
    assert_eq!(got.content.as_ref().unwrap().body.embeds.len(), 1);
    assert_eq!(got.edited_at, Some(Timestamp(3000)));
    // 더 오래된 수정은 무시.
    assert!(!s.apply_edit(&m.id(), &up(&e1, "first")).unwrap());
    let e3 = fx.kind_event(EventKind::Edit, vec![e2.id()], 4);
    s.insert_events(&[persist(&e3)]).unwrap();
    assert!(s.apply_edit(&m.id(), &up(&e3, "third")).unwrap());
    assert_eq!(
        s.get_message(&m.id(), None)
            .unwrap()
            .unwrap()
            .content
            .unwrap()
            .body
            .text,
        "third"
    );

    // 삭제: 원본과 수정 본문 모두 저장 키 폐기, 보기는 '삭제됨'.
    assert!(
        s.mark_message_deleted(&fx.space, &m.id(), Timestamp(5000))
            .unwrap()
    );
    for id in [m.id(), e1.id(), e2.id(), e3.id()] {
        assert_eq!(
            count(&s, "SELECT COUNT(*) FROM msg_keys WHERE event_id = ?1", &id),
            0
        );
        assert_eq!(s.get_event(&id).unwrap().unwrap().body, None);
    }
    let got = s.get_message(&m.id(), None).unwrap().unwrap();
    assert!(got.deleted);
    assert!(got.content.is_none());
    assert!(s.is_deleted(&m.id()).unwrap());
    // 삭제 뒤 온 수정은 바로 폐기.
    let e4 = fx.kind_event(EventKind::Edit, vec![e3.id()], 6);
    s.insert_events(&[persist(&e4)]).unwrap();
    assert!(!s.apply_edit(&m.id(), &up(&e4, "too late")).unwrap());
    assert_eq!(s.get_event(&e4.id()).unwrap().unwrap().body, None);
}

#[test]
fn delete_before_original_arrives() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"secret");
    assert!(
        !s.mark_message_deleted(&fx.space, &m.id(), Timestamp(1))
            .unwrap()
    );
    s.insert_events_with_views(&[persist(&m)], &[view(&m, "secret")])
        .unwrap();
    assert_eq!(s.get_event(&m.id()).unwrap().unwrap().body, None);
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM msg_keys WHERE event_id = ?1",
            &m.id()
        ),
        0
    );
    let got = s.get_message(&m.id(), None).unwrap().unwrap();
    assert!(got.deleted && got.content.is_none());
}

/// 같은 공간·채널의 다른 사용자.
fn peer_of(fx: &Fx) -> Fx {
    Fx {
        dev: SigningKeypair::generate(),
        user: SigningKeypair::generate(),
        space: fx.space,
        channel: fx.channel,
    }
}

#[test]
fn reactions_converge_regardless_of_order() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"m");
    s.insert_events_with_views(&[persist(&m)], &[view(&m, "hi")])
        .unwrap();
    let (alice_fx, bob_fx) = (peer_of(&fx), peer_of(&fx));
    let (alice, bob) = (alice_fx.user_id(), bob_fx.user_id());
    let thumbs = ReactionEmoji::Unicode {
        value: "👍".into()
    };
    let custom = ReactionEmoji::Custom {
        id: EmojiId(42),
        name: "yangpa".into(),
        animated: false,
    };
    // 반응 이벤트를 실제로 저장한 뒤 그 이벤트로 반응을 적용합니다.
    let r = |who: &Fx, emoji: &ReactionEmoji, present: bool, lamport: u64| {
        let kind = if present {
            EventKind::Reaction
        } else {
            EventKind::ReactionRemove
        };
        let ev = who.kind_event(kind, vec![m.id()], lamport);
        s.insert_events(&[persist(&ev)]).unwrap();
        ReactionUpdate {
            space: fx.space,
            target: m.id(),
            user: who.user_id(),
            emoji: emoji.clone(),
            present,
            event: ev.id(),
            lamport,
        }
    };
    assert!(s.apply_reaction(&r(&alice_fx, &thumbs, true, 5)).unwrap());
    // 더 오래된 제거는 무시.
    assert!(!s.apply_reaction(&r(&alice_fx, &thumbs, false, 3)).unwrap());
    assert!(s.apply_reaction(&r(&bob_fx, &thumbs, true, 6)).unwrap());
    assert!(s.apply_reaction(&r(&bob_fx, &custom, true, 7)).unwrap());
    let sum = s.reactions(&m.id(), Some(&alice)).unwrap();
    assert_eq!(sum.len(), 2);
    assert_eq!(
        (sum[0].emoji.clone(), sum[0].count, sum[0].me),
        (thumbs.clone(), 2, true)
    );
    assert_eq!(
        (sum[1].emoji.clone(), sum[1].count, sum[1].me),
        (custom.clone(), 1, false)
    );
    assert!(s.apply_reaction(&r(&alice_fx, &thumbs, false, 8)).unwrap());
    let sum = s.reactions(&m.id(), Some(&alice)).unwrap();
    assert_eq!((sum[0].count, sum[0].me), (1, false));
    assert_eq!(s.reactors(&m.id(), &thumbs).unwrap(), vec![bob]);
    let listed = s
        .list_messages_for(&MessageQuery {
            space: fx.space,
            channel: fx.channel,
            thread: None,
            before: None,
            limit: 10,
            viewer: Some(bob),
        })
        .unwrap();
    assert!(listed[0].reactions.iter().all(|r| r.me));
    // 삭제된 메시지에는 반응이 붙지 않음.
    s.mark_message_deleted(&fx.space, &m.id(), Timestamp(9))
        .unwrap();
    assert!(s.reactions(&m.id(), None).unwrap().is_empty());
    assert!(!s.apply_reaction(&r(&alice_fx, &thumbs, true, 10)).unwrap());
    assert!(
        s.apply_reaction(&r(
            &alice_fx,
            &ReactionEmoji::Unicode {
                value: String::new()
            },
            true,
            11
        ))
        .is_err()
    );
}

#[test]
fn reactions_require_stored_event_of_same_user_and_space() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"m");
    s.insert_events_with_views(&[persist(&m)], &[view(&m, "hi")])
        .unwrap();
    let alice_fx = peer_of(&fx);
    let thumbs = ReactionEmoji::Unicode {
        value: "👍".into()
    };
    let ev = alice_fx.kind_event(EventKind::Reaction, vec![m.id()], 2);
    let base = ReactionUpdate {
        space: fx.space,
        target: m.id(),
        user: alice_fx.user_id(),
        emoji: thumbs.clone(),
        present: true,
        event: ev.id(),
        lamport: 2,
    };
    // 저장되지 않은 이벤트(예: 휘발성 채널의 반응)는 디스크 표로 오지 않음.
    assert!(matches!(s.apply_reaction(&base), Err(StoreError::NotFound)));
    s.insert_events(&[persist(&ev)]).unwrap();
    // 다른 사람 이름으로 반응을 기록할 수 없음.
    assert!(matches!(
        s.apply_reaction(&ReactionUpdate {
            user: fx.user_id(),
            ..base.clone()
        }),
        Err(StoreError::InvalidInput(_))
    ));
    // 다른 공간을 주장할 수 없음.
    assert!(matches!(
        s.apply_reaction(&ReactionUpdate {
            space: SpaceId::random(),
            ..base.clone()
        }),
        Err(StoreError::InvalidInput(_))
    ));
    // 다른 공간의 메시지를 대상으로 할 수 없음.
    let elsewhere = Fx::new();
    let m2 = elsewhere.msg(vec![], 1, b"x");
    s.insert_events(&[persist(&m2)]).unwrap();
    assert!(matches!(
        s.apply_reaction(&ReactionUpdate {
            target: m2.id(),
            ..base.clone()
        }),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(s.reactions(&m2.id(), None).unwrap().is_empty());
    assert!(s.apply_reaction(&base).unwrap());
    // 다른 공간의 메시지 삭제도 거부.
    assert!(matches!(
        s.mark_message_deleted(&fx.space, &m2.id(), Timestamp(5)),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(!s.is_deleted(&m2.id()).unwrap());
}

#[test]
fn forum_posts_threads_and_updates() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let post_ev = fx.kind_event(EventKind::ForumPost, vec![], 1);
    let post_content = MessageContent {
        body: MessageBody {
            text: "본문".into(),
            ..Default::default()
        },
        forum: Some(ForumMeta {
            title: "제목".into(),
            tags: vec![1],
        }),
    };
    let post = MessageRecord::from_header(
        post_ev.id(),
        &post_ev.header,
        MessageKind::ForumPost,
        post_content,
    )
    .unwrap();
    let reply_ev = fx.msg(vec![post_ev.id()], 2, b"r");
    let mut reply = view(&reply_ev, "댓글");
    reply.thread = Some(post_ev.id());
    reply.content.as_mut().unwrap().body.thread = Some(post_ev.id());
    // 댓글이 게시글보다 먼저 와도 댓글 수가 맞음.
    s.insert_events_with_views(&[persist(&reply_ev), persist(&post_ev)], &[reply, post])
        .unwrap();
    let posts = s
        .list_forum_posts(&fx.space, &fx.channel, ForumSort::Activity, false, 10, None)
        .unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].reply_count, 1);
    assert_eq!(posts[0].last_activity, reply_ev.header.created_at);
    // 채널 본 목록에는 댓글이 없고, 댓글 목록에는 있음.
    let main = s.list_messages(&fx.space, &fx.channel, None, 10).unwrap();
    assert_eq!(
        main.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![post_ev.id()]
    );
    let thread = s
        .list_messages_for(&MessageQuery {
            space: fx.space,
            channel: fx.channel,
            thread: Some(post_ev.id()),
            before: None,
            limit: 10,
            viewer: None,
        })
        .unwrap();
    assert_eq!(
        thread.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![reply_ev.id()]
    );
    // 게시글 변경(마지막 변경이 이김). 변경 이벤트를 먼저 저장합니다.
    let u_in = |who: &Fx, lamport: u64, title: &str| {
        let ev = who.kind_event(EventKind::ForumPostUpdate, vec![], lamport);
        s.insert_events(&[persist(&ev)]).unwrap();
        ForumUpdate {
            event: ev.id(),
            lamport,
            title: Some(title.into()),
            tags: Some(vec![2, 3]),
            pinned: Some(true),
            locked: None,
            edited_at: Some(Timestamp(lamport * 1000)),
        }
    };
    let u = |lamport: u64, title: &str| u_in(&fx, lamport, title);
    // 저장되지 않은 변경 이벤트, 다른 공간의 변경 이벤트는 거부.
    assert!(matches!(
        s.apply_forum_update(
            &post_ev.id(),
            &ForumUpdate {
                event: EventId::random(),
                ..u(6, "x")
            }
        ),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        s.apply_forum_update(&post_ev.id(), &u_in(&Fx::new(), 7, "탈취")),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(
        s.apply_forum_update(&post_ev.id(), &u(5, "새 제목"))
            .unwrap()
    );
    assert!(
        !s.apply_forum_update(&post_ev.id(), &u(4, "옛 제목"))
            .unwrap()
    );
    let p = s.get_message(&post_ev.id(), None).unwrap().unwrap();
    assert_eq!(
        p.content.as_ref().unwrap().forum,
        Some(ForumMeta {
            title: "새 제목".into(),
            tags: vec![2, 3]
        })
    );
    assert!(p.pinned && !p.locked);
    assert_eq!(
        p.edited_at,
        Some(Timestamp(5000)),
        "제목·태그를 바꾸면 수정됨"
    );
    // 댓글 삭제 시 댓글 수 갱신.
    s.mark_message_deleted(&fx.space, &reply_ev.id(), Timestamp(9))
        .unwrap();
    assert_eq!(
        s.get_message(&post_ev.id(), None)
            .unwrap()
            .unwrap()
            .reply_count,
        0
    );
}

#[test]
fn unread_counts_and_read_markers() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let me = UserId::random();
    let evs: Vec<SignedEvent> = (1..=5).map(|l| fx.msg(vec![], l, &[l as u8])).collect();
    let mut views: Vec<MessageRecord> = evs.iter().map(|e| view(e, "x")).collect();
    views[4].mentions_me = true;
    s.insert_events_with_views(&evs.iter().map(persist).collect::<Vec<_>>(), &views)
        .unwrap();
    let u = s.unread_counts(&fx.space, &me).unwrap();
    assert_eq!(
        u,
        vec![ChannelUnread {
            channel: fx.channel,
            unread: 5,
            mentions: 1
        }]
    );
    assert!(
        s.set_read_marker(&fx.space, &fx.channel, 3, &evs[2].id())
            .unwrap()
    );
    // 뒤로는 옮기지 않음.
    assert!(
        !s.set_read_marker(&fx.space, &fx.channel, 1, &evs[0].id())
            .unwrap()
    );
    assert_eq!(
        s.read_marker(&fx.space, &fx.channel).unwrap(),
        Some((3, evs[2].id()))
    );
    assert_eq!(s.unread_counts(&fx.space, &me).unwrap()[0].unread, 2);
    // 내가 쓴 메시지는 세지 않음.
    assert!(
        s.unread_counts(&fx.space, &fx.user_id())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unread_excluding_and_recent_by_author() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let other = Fx {
        space: fx.space,
        channel: fx.channel,
        ..Fx::new()
    };
    let me = UserId::random();
    let mine: Vec<SignedEvent> = (1..=3).map(|l| fx.msg(vec![], l, &[l as u8])).collect();
    let theirs: Vec<SignedEvent> = (4..=5).map(|l| other.msg(vec![], l, &[l as u8])).collect();
    let all: Vec<SignedEvent> = mine.iter().chain(theirs.iter()).cloned().collect();
    let views: Vec<MessageRecord> = all.iter().map(|e| view(e, "x")).collect();
    s.insert_events_with_views(&all.iter().map(persist).collect::<Vec<_>>(), &views)
        .unwrap();
    assert_eq!(s.unread_counts(&fx.space, &me).unwrap()[0].unread, 5);
    let u = s
        .unread_counts_excluding(&fx.space, &me, &[other.user_id()])
        .unwrap();
    assert_eq!(u[0].unread, 3);
    assert!(
        s.unread_counts_excluding(&fx.space, &me, &[other.user_id(), fx.user_id()])
            .unwrap()
            .is_empty()
    );
    // 차단 목록이 SQLite 식 깊이 상한(1000)보다 길어도 됩니다(차단은 1만 명까지).
    let mut many: Vec<UserId> = (0..1500).map(|_| UserId::random()).collect();
    many.push(other.user_id());
    let u = s.unread_counts_excluding(&fx.space, &me, &many).unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].unread, 3);
    // 작성 시각은 1_700_000_000_000 + 논리 시계. 끝(밴 시각) 뒤에 쓴 것은 고르지 않습니다.
    let since = Timestamp(1_700_000_000_002);
    let until = Timestamp(1_700_000_000_003);
    let recent = s
        .messages_by_author_between(&fx.space, &fx.user_id(), since, until)
        .unwrap();
    assert_eq!(
        recent,
        vec![(mine[1].id(), fx.channel), (mine[2].id(), fx.channel)]
    );
    let early = s
        .messages_by_author_between(
            &fx.space,
            &fx.user_id(),
            since,
            Timestamp(1_700_000_000_002),
        )
        .unwrap();
    assert_eq!(early, vec![(mine[1].id(), fx.channel)]);
    s.mark_message_deleted(&fx.space, &mine[2].id(), Timestamp(9))
        .unwrap();
    let recent = s
        .messages_by_author_between(&fx.space, &fx.user_id(), since, until)
        .unwrap();
    assert_eq!(recent, vec![(mine[1].id(), fx.channel)]);
    assert!(
        s.messages_by_author_between(
            &SpaceId::random(),
            &fx.user_id(),
            Timestamp(0),
            Timestamp(u64::MAX >> 1)
        )
        .unwrap()
        .is_empty()
    );
}

/// 검색 결과 전부(쪽 하나에 담기는 만큼)의 ID, 최신순.
fn search_ids(s: &Store, query: &str, filter: SearchFilter<'_>) -> Vec<EventId> {
    let page = s.search_messages(query, filter, |_| true, 0, 500).unwrap();
    assert_eq!(page.total, page.ids.len());
    page.ids
}

#[test]
fn search_finds_decrypted_text() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let a = fx.msg(vec![], 1, b"a");
    let b = fx.msg(vec![], 2, b"b");
    s.insert_events_with_views(
        &[persist(&a), persist(&b)],
        &[view(&a, "양파 Hello"), view(&b, "감자")],
    )
    .unwrap();
    let in_space = SearchFilter {
        space: Some(&fx.space),
        ..Default::default()
    };
    assert_eq!(search_ids(&s, "hello", in_space), vec![a.id()]);
    let in_channel = SearchFilter {
        channel: Some(&fx.channel),
        ..Default::default()
    };
    assert_eq!(search_ids(&s, "감자", in_channel), vec![b.id()]);
    let elsewhere = SearchFilter {
        space: Some(&SpaceId::random()),
        ..Default::default()
    };
    assert!(search_ids(&s, "감자", elsewhere).is_empty());
    assert!(search_ids(&s, "   ", SearchFilter::default()).is_empty());
    // 작성자로: 검색어가 비어도 그 사람의 글을 최신순으로, 다른 사람이면 없음.
    let me = fx.user_id();
    let by = SearchFilter {
        author: Some(&me),
        ..Default::default()
    };
    assert_eq!(search_ids(&s, "", by), vec![b.id(), a.id()]);
    let stranger = UserId::random();
    let by_stranger = SearchFilter {
        author: Some(&stranger),
        ..Default::default()
    };
    assert!(search_ids(&s, "", by_stranger).is_empty());
    s.erase_body(&a.id()).unwrap();
    assert!(search_ids(&s, "hello", SearchFilter::default()).is_empty());
}

/// 낱말은 모두 들어 있어야 하고(순서 무관), 큰따옴표는 구절 그대로. 대소문자·전각·줄바꿈 차이는 무시하고, 포럼 제목과
/// 첨부 이름에서도 찾지만 칸(본문·제목·이름)을 넘는 일치는 없습니다.
#[test]
fn search_terms_phrases_and_fields() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let a = fx.msg(vec![], 1, b"a");
    let b = fx.msg(vec![], 2, b"b");
    let c = fx.msg(vec![], 3, b"c");
    let d = fx.msg(vec![], 4, b"d");
    let post = fx.kind_event(EventKind::ForumPost, vec![], 5);
    let mut with_file = view(&d, "자료 올립니다");
    with_file
        .content
        .as_mut()
        .unwrap()
        .body
        .attachments
        .push(yangpa_core::body::Attachment {
            blob: BlobId([7; 32]),
            key: Secret32::random(),
            name: "분기 보고서.PDF".into(),
            mime: "application/pdf".into(),
            size: 1,
            width: None,
            height: None,
            duration_ms: None,
            thumbnail: vec![],
        });
    let post_view = MessageRecord::from_header(
        post.id(),
        &post.header,
        MessageKind::ForumPost,
        MessageContent {
            body: MessageBody {
                text: "본문".into(),
                ..Default::default()
            },
            forum: Some(ForumMeta {
                title: "Rust 스터디 모집".into(),
                tags: vec![],
            }),
        },
    )
    .unwrap();
    s.insert_events_with_views(
        &[
            persist(&a),
            persist(&b),
            persist(&c),
            persist(&d),
            persist(&post),
        ],
        &[
            view(&a, "RUST 회의 자료 공유"),
            view(&b, "회의는 내일, 자료는 모레"),
            view(&c, "ＲＵＳＴ 회의\n\n자료"),
            with_file,
            post_view,
        ],
    )
    .unwrap();
    let all = SearchFilter::default();
    assert_eq!(search_ids(&s, "rust", all), vec![post.id(), c.id(), a.id()]);
    assert_eq!(
        search_ids(&s, "자료 회의", all),
        vec![c.id(), b.id(), a.id()]
    );
    assert_eq!(
        search_ids(&s, "\"회의 자료\"", all),
        vec![c.id(), a.id()],
        "구절: 줄바꿈도 빈칸 하나로"
    );
    assert_eq!(search_ids(&s, "보고서.pdf", all), vec![d.id()]);
    assert_eq!(search_ids(&s, "스터디", all), vec![post.id()]);
    assert!(
        search_ids(&s, "\"본문rust\"", all).is_empty(),
        "본문과 제목 사이를 넘어 맞지 않음"
    );
    assert!(search_ids(&s, "rust 없는말", all).is_empty());
}

/// 개수 제한 없이 모두 세고, 쪽마다 최신순으로 이어지며, 끝을 넘으면 마지막 쪽. 부르는 쪽 거르기가 전체 수에 반영됩니다.
#[test]
fn search_pages_through_all_results() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let msgs: Vec<SignedEvent> = (1..=120).map(|i| fx.msg(vec![], i, b"m")).collect();
    let views: Vec<MessageRecord> = msgs
        .iter()
        .map(|m| view(m, &format!("기록 {}", m.header.lamport)))
        .collect();
    let persisted: Vec<_> = msgs.iter().map(persist).collect();
    s.insert_events_with_views(&persisted, &views).unwrap();
    let newest_first: Vec<EventId> = msgs.iter().rev().map(|m| m.id()).collect();
    let all = SearchFilter::default();
    let page = |offset| {
        s.search_messages("기록", all, |_| true, offset, 50)
            .unwrap()
    };
    let p0 = page(0);
    assert_eq!((p0.total, p0.offset), (120, 0));
    assert_eq!(p0.ids, newest_first[..50]);
    assert_eq!(page(50).ids, newest_first[50..100]);
    let last = page(100);
    assert_eq!((last.offset, last.ids.len()), (100, 20));
    assert_eq!(last.ids, newest_first[100..]);
    let beyond = page(10_000);
    assert_eq!((beyond.total, beyond.offset), (120, 100));
    assert_eq!(beyond.ids, newest_first[100..]);
    // 홀수 번호 글만 받는다고 하면 전체 수도 그만큼.
    let odd: std::collections::HashSet<EventId> = msgs
        .iter()
        .filter(|m| m.header.lamport % 2 == 1)
        .map(|m| m.id())
        .collect();
    let filtered = s
        .search_messages("기록", all, |h| odd.contains(&h.id), 0, 7)
        .unwrap();
    assert_eq!(filtered.total, 60);
    assert_eq!(filtered.ids.len(), 7);
    assert!(filtered.ids.iter().all(|id| odd.contains(id)));
    assert!(
        s.search_messages("없는말", all, |_| true, 0, 50)
            .unwrap()
            .total
            == 0
    );
}

/// 색인을 만든 뒤의 새 글·수정·삭제·만료·공간 지우기·전체 지우기가 다음 검색에 바로 반영됩니다(변경 훅).
#[test]
fn search_index_follows_changes() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let a = fx.msg(vec![], 1, b"a");
    s.insert_events_with_views(&[persist(&a)], &[view(&a, "첫 글 사과")])
        .unwrap();
    let all = SearchFilter::default();
    assert_eq!(search_ids(&s, "사과", all), vec![a.id()]);
    // 새 글
    let b = fx.msg(vec![a.id()], 2, b"b");
    s.insert_events_with_views(&[persist(&b)], &[view(&b, "둘째 글 사과 배")])
        .unwrap();
    assert_eq!(search_ids(&s, "사과", all), vec![b.id(), a.id()]);
    // 수정: 예전 낱말은 빠지고 새 낱말로 찾음
    let e = fx.kind_event(EventKind::Edit, vec![b.id()], 3);
    s.insert_events(&[persist(&e)]).unwrap();
    assert!(
        s.apply_edit(
            &b.id(),
            &EditUpdate {
                event: e.id(),
                lamport: 3,
                at: Timestamp(3000),
                text: "고친 글 포도".into(),
                embeds: vec![],
            },
        )
        .unwrap()
    );
    assert_eq!(search_ids(&s, "사과", all), vec![a.id()]);
    assert_eq!(search_ids(&s, "포도", all), vec![b.id()]);
    // 삭제: 다음 검색을 기다리지 않고 RAM 색인에서도 바로 지움
    assert!(s.search_index_holds("고친 글 포도"));
    s.mark_message_deleted(&fx.space, &b.id(), Timestamp(4))
        .unwrap();
    assert!(!s.search_index_holds("포도"));
    assert!(search_ids(&s, "포도", all).is_empty());
    // 공간 지우기(외래 키 연쇄 삭제)
    let other = Fx::new();
    let c = other.msg(vec![], 9, b"c");
    s.insert_events_with_views(&[persist(&c)], &[view(&c, "다른 공간 사과")])
        .unwrap();
    assert_eq!(search_ids(&s, "사과", all), vec![c.id(), a.id()]);
    s.purge_space(&other.space).unwrap();
    assert_eq!(search_ids(&s, "사과", all), vec![a.id()]);
    // 전체 지우기
    s.wipe_all(&[], &[]).unwrap();
    assert!(search_ids(&s, "사과", all).is_empty());
    let d = fx.msg(vec![], 1, b"d");
    s.insert_events_with_views(&[persist(&d)], &[view(&d, "다시 사과")])
        .unwrap();
    assert_eq!(search_ids(&s, "사과", all), vec![d.id()]);
}

// ---- 키·비밀값 ----

#[test]
fn keys_first_wins_and_latest() {
    let (s, _d) = temp();
    let k1 = Secret32::random();
    let k2 = Secret32::random();
    assert!(s.put_key(KeyKind::History, b"id1", b"chan", &k1).unwrap());
    assert!(
        !s.put_key(KeyKind::History, b"id1", b"chan", &k2).unwrap(),
        "같은 ID는 덮어쓰지 않음"
    );
    assert_eq!(
        s.get_key(KeyKind::History, b"id1").unwrap(),
        Some(k1.clone())
    );
    assert_eq!(s.get_key(KeyKind::State, b"id1").unwrap(), None);
    let space = SpaceId::random();
    for (id, epoch, nb) in [(b"e1", 1u64, 10u64), (b"e2", 2, 5), (b"e3", 2, 20)] {
        s.put_key_entry(&KeyEntry {
            kind: KeyKind::History,
            id: id.to_vec(),
            scope: b"chan2".to_vec(),
            space: Some(space),
            epoch: Some(epoch),
            not_before: Some(Timestamp(nb)),
            key: Secret32::random(),
            created_at: Timestamp(1),
        })
        .unwrap();
    }
    let latest = s.latest_key(KeyKind::History, b"chan2").unwrap().unwrap();
    assert_eq!(latest.id, b"e3");
    assert_eq!(
        s.keys_in_scope(KeyKind::History, b"chan2").unwrap().len(),
        3
    );
    assert!(s.delete_key(KeyKind::History, b"e3").unwrap());
    assert_eq!(
        s.delete_keys_in_scope(KeyKind::History, b"chan2").unwrap(),
        2
    );
    assert!(s.put_key(KeyKind::State, b"", b"x", &k1).is_err());
}

/// 가입자 몫(최근 상태 키 n개)은 순번으로 고릅니다: 늦게 물어 받아 생성 시각이 가장 새로운 옛 키가 최근 키를 밀어내지
/// 않고, 순번 없이 먼저 저장된 키도 순번을 적으면 제자리에 갑니다(이미 적힌 순번은 바꾸지 않음).
#[test]
fn recent_state_keys_follow_the_epoch() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    let put = |id: &[u8], epoch: Option<u64>, created: u64| {
        s.put_key_entry(&KeyEntry {
            kind: KeyKind::State,
            id: id.to_vec(),
            scope: space.as_bytes().to_vec(),
            space: Some(space),
            epoch,
            not_before: None,
            key: Secret32::random(),
            created_at: Timestamp(created),
        })
        .unwrap();
    };
    for e in 3..=6u64 {
        put(&[e as u8; 4], Some(e), 100 + e);
    }
    // 순번 1의 옛 키를 나중에(가장 새 생성 시각으로) 받음, 순번 8의 키는 순번 없이 먼저 들어옴.
    put(&[1; 4], Some(1), 1_000);
    put(&[8; 4], None, 50);
    let ids = |n: usize| -> Vec<u8> {
        s.recent_keys_in_scope(KeyKind::State, space.as_bytes(), n)
            .unwrap()
            .into_iter()
            .map(|e| e.id[0])
            .collect()
    };
    assert_eq!(ids(2), vec![6, 5], "순번이 아닌 생성 시각으로 고름");
    s.set_key_epoch(KeyKind::State, &[8; 4], 8).unwrap();
    assert_eq!(ids(3), vec![8, 6, 5]);
    // 이미 순번이 있으면 그대로(늦게 온 다른 길이 순번을 되돌리지 못함).
    s.set_key_epoch(KeyKind::State, &[6; 4], 0).unwrap();
    assert_eq!(ids(3), vec![8, 6, 5]);
    assert_eq!(ids(100).len(), 6);
    assert_eq!(
        ids(100).last(),
        Some(&1),
        "가장 오래된 순번이 맨 뒤: {:?}",
        ids(100)
    );
}

#[test]
fn secrets_roundtrip() {
    let (s, _d) = temp();
    assert!(s.get_secret(secret_names::IDENTITY).unwrap().is_none());
    s.put_secret(secret_names::IDENTITY, &[7; 32]).unwrap();
    assert_eq!(
        &**s.get_secret(secret_names::IDENTITY).unwrap().unwrap(),
        &[7; 32]
    );
    assert!(s.delete_secret(secret_names::IDENTITY).unwrap());
    assert!(s.get_secret(secret_names::IDENTITY).unwrap().is_none());
}

/// 7판 이전: 메시지 보기의 익명 표시를 이벤트 종류로 채우고, 목록과 검색이 그 표시를 씁니다.
#[test]
fn v7_migration_backfills_anonymous() {
    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    let fx = Fx::new();
    let plain = fx.msg(vec![], 1, b"p");
    let anon = fx.kind_event(EventKind::AnonMessage, vec![], 2);
    let anon_view = MessageRecord::from_header(
        anon.id(),
        &anon.header,
        MessageKind::Message,
        MessageContent {
            body: MessageBody {
                text: "익명 고민".into(),
                ..Default::default()
            },
            forum: None,
        },
    )
    .unwrap();
    assert!(anon_view.anonymous);
    {
        let s = Store::open(dir.path(), &key).unwrap();
        s.insert_events_with_views(
            &[persist(&plain), persist(&anon)],
            &[view(&plain, "보통 고민"), anon_view],
        )
        .unwrap();
        s.write(|tx| {
            Ok(tx.execute_batch(
                "ALTER TABLE messages DROP COLUMN anonymous; PRAGMA user_version = 6;",
            )?)
        })
        .unwrap();
    }
    let s = Store::open(dir.path(), &key).unwrap();
    assert_eq!(
        s.schema_version().unwrap(),
        crate::schema::MIGRATIONS.len() as i64
    );
    assert!(s.get_message(&anon.id(), None).unwrap().unwrap().anonymous);
    assert!(!s.get_message(&plain.id(), None).unwrap().unwrap().anonymous);
    let mut flags = Vec::new();
    s.search_messages(
        "고민",
        SearchFilter::default(),
        |h| {
            flags.push((h.id, h.anonymous));
            true
        },
        0,
        10,
    )
    .unwrap();
    flags.sort();
    let mut want = vec![(plain.id(), false), (anon.id(), true)];
    want.sort();
    assert_eq!(flags, want);
}

/// 6판 이전: 옛 MLS 그룹 상태 표와 에포크 키(종류 3)를 지우고, 다른 키는 그대로 둡니다.
#[test]
fn v6_migration_drops_mls_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    let k = Secret32::random();
    {
        let s = Store::open(dir.path(), &key).unwrap();
        s.put_key(KeyKind::State, b"state-1", b"space", &k).unwrap();
        s.write(|tx| {
            Ok(tx.execute_batch(
                "CREATE TABLE mls_state (id INTEGER PRIMARY KEY CHECK (id = 1), state BLOB NOT NULL,                  updated_at INTEGER NOT NULL);                  INSERT INTO mls_state VALUES (1, x'0102', 1);                  INSERT INTO keys (kind, id, scope, space, epoch, not_before, key, created_at)                  VALUES (3, x'65', x'67', NULL, 1, NULL, x'00', 1);                  ALTER TABLE messages DROP COLUMN anonymous; PRAGMA user_version = 5;",
            )?)
        })
        .unwrap();
    }
    let s = Store::open(dir.path(), &key).unwrap();
    assert_eq!(
        s.schema_version().unwrap(),
        crate::schema::MIGRATIONS.len() as i64
    );
    let mls_tables: i64 = s
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'mls_state'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(mls_tables, 0);
    let epoch_keys: i64 = s
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM keys WHERE kind = 3", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(epoch_keys, 0);
    assert!(s.get_key(KeyKind::State, b"state-1").unwrap().is_some());
}

#[test]
fn delete_refuses_non_message_targets() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let state_ev = fx.kind_event(EventKind::ChannelCreate, vec![], 1);
    let key_ev = fx.kind_event(EventKind::KeyUpdate, vec![], 2);
    s.insert_events(&[persist(&state_ev), persist(&key_ev)])
        .unwrap();
    for ev in [&state_ev, &key_ev] {
        assert!(matches!(
            s.mark_message_deleted(&fx.space, &ev.id(), Timestamp(5)),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(s.get_event(&ev.id()).unwrap().unwrap().body.is_some());
    }
    // 메시지는 그대로 지워집니다.
    let m = fx.msg(vec![], 3, b"hi");
    s.insert_events(&[persist(&m)]).unwrap();
    assert!(
        s.mark_message_deleted(&fx.space, &m.id(), Timestamp(5))
            .unwrap()
    );
    assert!(s.get_event(&m.id()).unwrap().unwrap().body.is_none());
}

// ---- 친구 ----

#[test]
fn friend_links_redeem_rules() {
    let (s, _d) = temp();
    let link = |single: bool, expires: Option<u64>| FriendLinkRecord {
        token_hash: *blake3::hash(&rand_bytes()).as_bytes(),
        created_at: Timestamp(1),
        expires_at: expires.map(Timestamp),
        single_use: single,
        uses: 0,
        revoked: false,
    };
    let once = link(true, None);
    let multi = link(false, Some(1000));
    let revoked = link(false, None);
    for l in [&once, &multi, &revoked] {
        s.create_friend_link(l).unwrap();
    }
    s.revoke_friend_link(&revoked.token_hash).unwrap();
    assert_eq!(
        s.redeem_friend_link(&once.token_hash, Timestamp(5))
            .unwrap(),
        RedeemOutcome::Accepted
    );
    assert_eq!(
        s.redeem_friend_link(&once.token_hash, Timestamp(5))
            .unwrap(),
        RedeemOutcome::AlreadyUsed
    );
    assert_eq!(
        s.redeem_friend_link(&multi.token_hash, Timestamp(5))
            .unwrap(),
        RedeemOutcome::Accepted
    );
    assert_eq!(
        s.redeem_friend_link(&multi.token_hash, Timestamp(6))
            .unwrap(),
        RedeemOutcome::Accepted
    );
    assert_eq!(
        s.redeem_friend_link(&multi.token_hash, Timestamp(1000))
            .unwrap(),
        RedeemOutcome::Expired
    );
    assert_eq!(
        s.redeem_friend_link(&revoked.token_hash, Timestamp(5))
            .unwrap(),
        RedeemOutcome::Revoked
    );
    assert_eq!(
        s.redeem_friend_link(&[0; 32], Timestamp(5)).unwrap(),
        RedeemOutcome::Unknown
    );
    assert_eq!(
        s.get_friend_link(&multi.token_hash).unwrap().unwrap().uses,
        2
    );
    assert_eq!(s.list_friend_links().unwrap().len(), 3);
    // 1회용 사용됨 + 폐기됨 + 만료됨 → 모두 정리.
    assert_eq!(s.prune_friend_links(Timestamp(2000)).unwrap(), 3);
}

fn rand_bytes() -> [u8; 32] {
    Secret32::random().0
}

#[test]
fn friends_requests_profiles_notes() {
    let (s, _d) = temp();
    let id = SigningKeypair::generate();
    let user = UserId(id.public());
    let space = SpaceId::random();
    s.put_friend(&FriendRecord {
        added_at: Timestamp(1),
        ..FriendRecord::friend(user, None)
    })
    .unwrap();
    s.put_friend(&FriendRecord {
        added_at: Timestamp(2),
        ..FriendRecord::friend(user, Some(space))
    })
    .unwrap();
    assert!(s.set_friend_verified(&user, true).unwrap());
    let f = s.get_friend(&user).unwrap().unwrap();
    assert_eq!(
        (f.space, f.verified, f.added_at),
        (Some(space), true, Timestamp(1))
    );
    assert_eq!(s.friend_by_space(&space).unwrap().unwrap().user, user);
    assert_eq!(s.list_friends().unwrap().len(), 1);
    assert!(s.get_friend(&user).unwrap().unwrap().is_friend());

    // 임시 연락처: 만료 시각이 지나거나 링크를 폐기하면 만료된 연락처가 된다.
    let temp_a = UserId(SigningKeypair::generate().public());
    let temp_b = UserId(SigningKeypair::generate().public());
    for (u, at, link) in [(temp_a, 100, [1u8; 32]), (temp_b, 10_000, [2u8; 32])] {
        s.put_friend(&FriendRecord {
            kind: ContactKind::Temporary,
            expires_at: Some(Timestamp(at)),
            link_hash: Some(link),
            ..FriendRecord::friend(u, Some(SpaceId::random()))
        })
        .unwrap();
    }
    assert_eq!(s.count_temporary_contacts().unwrap(), 2);
    assert_eq!(s.expire_due_contacts(Timestamp(50)).unwrap(), vec![]);
    assert_eq!(s.expire_due_contacts(Timestamp(100)).unwrap(), vec![temp_a]);
    let a = s.get_friend(&temp_a).unwrap().unwrap();
    assert_eq!((a.kind, a.is_active()), (ContactKind::Expired, false));
    assert_eq!(s.expire_link_contacts(&[1u8; 32]).unwrap(), vec![]);
    assert_eq!(s.expire_link_contacts(&[2u8; 32]).unwrap(), vec![temp_b]);
    assert_eq!(s.count_temporary_contacts().unwrap(), 0);
    // 친구가 되면 링크 흔적을 지운다.
    assert!(
        s.set_contact_kind(&temp_b, ContactKind::Friend, None, None)
            .unwrap()
    );
    let b = s.get_friend(&temp_b).unwrap().unwrap();
    assert_eq!(
        (b.kind, b.expires_at, b.link_hash),
        (ContactKind::Friend, None, None)
    );
    // 친구는 만료 대상이 아니다.
    assert_eq!(
        s.expire_due_contacts(Timestamp(u64::MAX / 2)).unwrap(),
        vec![]
    );
    for u in [temp_a, temp_b] {
        s.remove_friend(&u).unwrap();
    }

    let p1 = Profile {
        version: 1,
        name: "세영".into(),
        ..Default::default()
    }
    .sign(&id)
    .unwrap();
    let p2 = Profile {
        version: 2,
        name: "세영2".into(),
        ..Default::default()
    }
    .sign(&id)
    .unwrap();
    assert!(s.put_profile(&p2).unwrap());
    assert!(!s.put_profile(&p1).unwrap(), "옛 버전은 무시");
    assert_eq!(s.get_profile(&user).unwrap(), Some(p2.clone()));

    let token = [9u8; 32];
    let req = |rid: u8, dir: RequestDirection| FriendRequestRecord {
        request_id: [rid; 16],
        direction: dir,
        user,
        profile: Some(p2.clone()),
        token_hash: token,
        payload: vec![rid],
        received_at: Timestamp(rid as u64),
    };
    s.put_friend_request(&req(1, RequestDirection::Incoming))
        .unwrap();
    s.put_friend_request(&req(2, RequestDirection::Incoming))
        .unwrap();
    s.put_friend_request(&req(3, RequestDirection::Outgoing))
        .unwrap();
    let incoming = s.list_friend_requests(RequestDirection::Incoming).unwrap();
    assert_eq!(incoming.len(), 1, "같은 사용자의 받은 요청은 하나만");
    assert_eq!(incoming[0].request_id, [2; 16]);
    assert_eq!(
        s.outgoing_request_by_token(&token)
            .unwrap()
            .unwrap()
            .request_id,
        [3; 16]
    );
    assert_eq!(
        s.get_friend_request(&[2; 16]).unwrap().unwrap().profile,
        Some(p2)
    );
    assert!(s.delete_friend_request(&[2; 16]).unwrap());

    let dev = SigningKeypair::generate();
    let list = |v: u64| {
        DeviceList {
            user,
            version: v,
            devices: vec![DeviceEntry {
                device: DeviceId(dev.public()),
                box_key: BoxPublicKey::filled(1),
                sign_keys: DeviceSignKeys::of(&dev),
                name: "PC".into(),
                added_at: Timestamp(1),
                revoked_at: None,
            }],
            otp: None,
        }
        .sign(&id)
        .unwrap()
    };
    // 혼합 서명은 매번 새 난수라 같은 목록도 서명할 때마다 다릅니다(넣은 그 판을 그대로 돌려받는지 봄).
    let v3 = list(3);
    assert!(s.put_device_list(&v3).unwrap());
    assert!(!s.put_device_list(&list(2)).unwrap());
    assert_eq!(s.device_list_version(&user).unwrap(), Some(3));
    assert_eq!(s.get_device_list(&user).unwrap(), Some(v3));

    s.set_note(&user, "게임 친구").unwrap();
    assert_eq!(s.get_note(&user).unwrap().as_deref(), Some("게임 친구"));
    s.set_note(&user, "").unwrap();
    assert_eq!(s.get_note(&user).unwrap(), None);
    assert!(s.remove_friend(&user).unwrap());
}

// ---- 공간 ----

fn sample_state(owner: UserId) -> SpaceState {
    let mut roles = BTreeMap::new();
    roles.insert(
        RoleId::EVERYONE,
        Role::everyone(Permissions::DEFAULT_EVERYONE),
    );
    let mut members = BTreeMap::new();
    members.insert(
        owner,
        Member {
            user: owner,
            roles: vec![],
            nickname: Some("방장".into()),
            automation: false,
        },
    );
    let mut channels = BTreeMap::new();
    channels.insert(ChannelId::random(), ChannelSettings::text("일반"));
    let mut emojis = BTreeMap::new();
    emojis.insert(
        EmojiId(12345678901234),
        CustomEmoji {
            id: EmojiId(12345678901234),
            name: "yangpa".into(),
            animated: false,
            blob: BlobId::random(),
            key: Secret32::random(),
            mime: "image/png".into(),
        },
    );
    SpaceState {
        kind: SpaceKind::Crew,
        owner,
        committer: DeviceId::random(),
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
        devices: Default::default(),
        invites: Default::default(),
        emojis,
        pending_crew_conversion: None,
        bans: Default::default(),
        archives: BTreeMap::new(),
        channel_transport: BTreeMap::new(),
        role_display: Default::default(),
        anonymous_channels: Default::default(),
        crew_servers: Default::default(),
        forum_featured: Default::default(),
        attach_limits: Default::default(),
        edit_rules: Default::default(),
        emoji_limits: Default::default(),
        emoji_by: Default::default(),
        disbanded: false,
        volatile_caps: Default::default(),
    }
}

#[test]
fn space_state_cache_roundtrip() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    assert!(s.ensure_space(&space, Timestamp(10)).unwrap());
    assert!(!s.ensure_space(&space, Timestamp(11)).unwrap());
    let state = sample_state(UserId::random());
    assert_eq!(s.load_space_state(&space).unwrap(), None);
    s.put_space_state(&space, &state).unwrap();
    assert_eq!(s.load_space_state(&space).unwrap(), Some(state));
    let rec = s.get_space(&space).unwrap().unwrap();
    assert_eq!(rec.joined_at, Timestamp(10));
    assert!(!rec.dormant);
    s.set_space_dormant(&space, true).unwrap();
    s.touch_space_read(&space, Timestamp(50)).unwrap();
    s.touch_space_read(&space, Timestamp(40)).unwrap();
    let rec = s.get_space(&space).unwrap().unwrap();
    assert!(rec.dormant);
    assert_eq!(rec.last_read_at, Some(Timestamp(50)));
    assert_eq!(s.list_spaces().unwrap().len(), 1);
    assert_eq!(s.space_ids().unwrap(), vec![space]);
}

/// 상태 행 시험용 큰 상태: 멤버 `n`명(기기 기록 1~2개), 밴·채널·초대·역할 표시까지.
fn big_state(n: usize) -> SpaceState {
    use yangpa_core::model::{BanInfo, InviteState, RoleDisplay};
    let id = |tag: u8, i: usize| {
        let mut b = [tag; 32];
        b[..8].copy_from_slice(&(i as u64).to_le_bytes());
        b
    };
    let owner = UserId(id(1, 0));
    let mut st = sample_state(owner);
    for i in 0..n {
        let u = UserId(id(1, i));
        st.members.insert(
            u,
            Arc::new(Member {
                user: u,
                roles: if i % 7 == 0 {
                    vec![RoleId([7; 16])]
                } else {
                    vec![]
                },
                nickname: (i % 3 == 0).then(|| format!("멤버{i}")),
                automation: false,
            }),
        );
        let records = (0..1 + i % 2)
            .map(|k| {
                let sign_keys = DeviceSignKeys::filled((i + k) as u8);
                DeviceEntry {
                    device: DeviceId(id(2 + k as u8, i)),
                    box_key: BoxPublicKey::filled(3),
                    sign_keys,
                    name: String::new(),
                    added_at: Timestamp(i as u64),
                    revoked_at: (k == 1 && i % 5 == 0).then_some(Timestamp(9)),
                }
                .record()
            })
            .collect();
        st.devices.insert(
            u,
            Arc::new(MemberDevices {
                version: 1 + i as u64 % 4,
                devices: records,
                otp: None,
            }),
        );
    }
    // 떠난 사용자의 기기 기록만 남은 행.
    st.devices.insert(
        UserId(id(9, 0)),
        Arc::new(MemberDevices {
            version: 3,
            devices: vec![],
            otp: None,
        }),
    );
    for i in 0..n / 30 {
        st.bans.insert(
            UserId(id(6, i)),
            BanInfo {
                reason: "도배".into(),
                banned_by: owner,
                banned_at: Timestamp(i as u64),
            },
        );
    }
    for i in 0..40u8 {
        let mut c = ChannelSettings::text(format!("채널{i}"));
        c.position = i32::from(i);
        st.channels.insert(ChannelId([i; 16]), c);
    }
    st.channels
        .insert(ChannelId([200; 16]), volatile_settings());
    st.invites.push_back(InviteState {
        info: yangpa_core::body::InviteInfo {
            token_hash: [1; 32],
            verify_key: [2; 32],
            expires_at: None,
            max_uses: Some(5),
            roles: vec![],
        },
        created_by: owner,
        uses: 1,
    });
    st.role_display.insert(
        RoleId::EVERYONE,
        RoleDisplay {
            visible_to: None,
            see_admins: false,
        },
    );
    st
}

fn space_row_count(s: &Store, table: &str, space: &SpaceId) -> i64 {
    s.read(|c| {
        Ok(c.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE space = ?1"),
            [space.as_bytes()],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

/// 행으로 나눠 쓴 상태를 다시 읽으면 값도, 직렬화 바이트(postcard·JSON)도 같습니다.
#[test]
fn space_state_rows_round_trip_exactly() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    let st = big_state(500);
    let delta = StateDelta::between(None, &st).unwrap();
    s.apply_state_delta(&space, &delta).unwrap();
    let back = s.load_space_state(&space).unwrap().unwrap();
    assert_eq!(back, st);
    assert_eq!(
        postcard::to_stdvec(&back).unwrap(),
        postcard::to_stdvec(&st).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&back).unwrap(),
        serde_json::to_vec(&st).unwrap()
    );
    assert_eq!(space_row_count(&s, "space_members", &space), 501);
    assert_eq!(space_row_count(&s, "volatile_channels", &space), 1);
    // 다시 읽은 판에 비춘 차이는 없음(다시 시작한 뒤 첫 해소가 행을 다시 쓰지 않음).
    assert!(StateDelta::between(Some(&back), &st).unwrap().is_empty());
}

/// 상태가 한 번 바뀌면 바뀐 행만 씁니다(멤버 수와 무관).
#[test]
fn one_state_change_writes_only_changed_rows() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    let st = big_state(3_000);
    s.apply_state_delta(&space, &StateDelta::between(None, &st).unwrap())
        .unwrap();

    // 멤버 한 명의 역할만 바뀜: 멤버 행 하나, 머리·요약은 그대로.
    let mut next = st.clone();
    let u = *next.members.keys().nth(1234).unwrap();
    Arc::make_mut(next.members.get_mut(&u).unwrap()).roles = vec![RoleId([8; 16])];
    let d = StateDelta::between(Some(&st), &next).unwrap();
    assert_eq!(d.rows(), 1);
    assert!(d.value_bytes() < 256, "{}", d.value_bytes());
    s.apply_state_delta(&space, &d).unwrap();
    assert_eq!(s.load_space_state(&space).unwrap().unwrap(), next);

    // 새 멤버 가입(멤버·기기 기록 한 행) 뒤 추방(멤버만 빠지고 기기 기록은 남음).
    let mut joined = next.clone();
    let newbie = UserId([0xEE; 32]);
    joined.members.insert(
        newbie,
        Arc::new(Member {
            user: newbie,
            roles: vec![],
            nickname: None,
            automation: false,
        }),
    );
    joined
        .devices
        .insert(newbie, next.devices.values().next().unwrap().clone());
    let d = StateDelta::between(Some(&next), &joined).unwrap();
    assert_eq!(d.rows(), 1, "멤버 행 하나");
    s.apply_state_delta(&space, &d).unwrap();
    let mut kicked = joined.clone();
    kicked.members.remove(&newbie);
    let d = StateDelta::between(Some(&joined), &kicked).unwrap();
    assert_eq!(d.rows(), 1);
    s.apply_state_delta(&space, &d).unwrap();
    assert_eq!(s.load_space_state(&space).unwrap().unwrap(), kicked);

    // 기기 기록까지 빠지면 행을 지움. 밴 하나는 밴 행 하나.
    let mut gone = kicked.clone();
    gone.devices.remove(&newbie);
    gone.bans.insert(
        newbie,
        yangpa_core::model::BanInfo {
            reason: String::new(),
            banned_by: gone.owner,
            banned_at: Timestamp(1),
        },
    );
    let d = StateDelta::between(Some(&kicked), &gone).unwrap();
    assert_eq!(d.rows(), 2);
    s.apply_state_delta(&space, &d).unwrap();
    assert_eq!(s.load_space_state(&space).unwrap().unwrap(), gone);
    assert_eq!(space_row_count(&s, "space_members", &space), 3_001);

    // 설정이 바뀌면 머리 한 행.
    let mut renamed = gone.clone();
    renamed.settings.name = "새 이름".into();
    let d = StateDelta::between(Some(&gone), &renamed).unwrap();
    assert_eq!(d.rows(), 1);
    s.apply_state_delta(&space, &d).unwrap();
    assert_eq!(s.load_space_state(&space).unwrap().unwrap(), renamed);

    // 공간을 떠나면 행도 모두 지움.
    s.purge_space(&space).unwrap();
    for t in ["space_members", "space_bans", "space_channels"] {
        assert_eq!(space_row_count(&s, t, &space), 0, "{t}");
    }
    assert_eq!(s.load_space_state(&space).unwrap(), None);
}

/// 같은 기기 ID에 다른 상자 키를 실은(위조) 키는 다른 약속의 행에 들어가, 진짜 약속으로는 진짜 키만 나옵니다.
#[test]
fn forged_device_keys_land_in_their_own_row() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    let dev = SigningKeypair::generate();
    let genuine = DeviceKeys {
        device: DeviceId(dev.public()),
        box_key: BoxPublicKey::filled(1),
        sign_keys: DeviceSignKeys::of(&dev),
    };
    let mut forged = genuine.clone();
    forged.box_key = BoxPublicKey::filled(66);
    assert_ne!(genuine.key_commit(), forged.key_commit());
    // 기기 ID와 맞지 않는 서명 키는 넣지 않음.
    let mut mismatched = genuine.clone();
    mismatched.sign_keys = DeviceSignKeys::filled(3);
    assert_eq!(
        s.put_device_keys(
            &space,
            &[forged.clone(), genuine.clone(), mismatched.clone()]
        )
        .unwrap(),
        2
    );
    assert_eq!(
        s.put_device_keys(&space, std::slice::from_ref(&genuine))
            .unwrap(),
        0
    );
    let found = s
        .device_keys(&[genuine.key_commit(), mismatched.key_commit()])
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[&genuine.key_commit()].box_key,
        BoxPublicKey::filled(1)
    );
    assert_eq!(
        s.device_keys(&[forged.key_commit()]).unwrap()[&forged.key_commit()].box_key,
        BoxPublicKey::filled(66)
    );
    // 다른 공간에서 같은 키를 넣어도 그 공간을 지울 때 이 공간 행은 남음.
    let other = SpaceId::random();
    s.put_device_keys(&other, std::slice::from_ref(&genuine))
        .unwrap();
    s.purge_space(&other).unwrap();
    assert_eq!(s.device_keys(&[genuine.key_commit()]).unwrap().len(), 1);
    s.purge_space(&space).unwrap();
    assert!(s.device_keys(&[genuine.key_commit()]).unwrap().is_empty());
}

/// 받은 이벤트와 본문의 기기 키는 한 트랜잭션에 들어가고, 키 행은 공간마다 따로라 한 공간을 지워도 같은 기기가 있는 다른
/// 공간의 행은 남습니다. 시작 때 다시 적용하며 이미 있는 행은 공간의 약속 목록으로 거릅니다.
#[test]
fn event_and_device_keys_are_stored_per_space() {
    let (s, _d) = temp();
    let (x, y) = (Fx::new(), Fx::new());
    let keys = DeviceKeys {
        device: DeviceSignKeys::filled(7).device_id(),
        box_key: BoxPublicKey::filled(7),
        sign_keys: DeviceSignKeys::filled(7),
    };
    let ex = x.msg(vec![], 1, b"x");
    let ey = y.msg(vec![], 1, b"y");
    s.insert_events_with_device_keys(&[persist(&ex)], &x.space, std::slice::from_ref(&keys))
        .unwrap();
    s.insert_events_with_device_keys(&[persist(&ey)], &y.space, std::slice::from_ref(&keys))
        .unwrap();
    assert!(
        s.has_events(&[ex.id(), ey.id()])
            .unwrap()
            .iter()
            .all(|h| *h)
    );
    let commit = keys.key_commit();
    assert!(s.device_key_commits(&x.space).unwrap().contains(&commit));
    assert!(s.device_key_commits(&y.space).unwrap().contains(&commit));
    s.purge_space(&x.space).unwrap();
    assert!(s.device_key_commits(&x.space).unwrap().is_empty());
    assert_eq!(
        s.device_keys(&[commit]).unwrap()[&commit].box_key,
        BoxPublicKey::filled(7)
    );
}

#[test]
fn purge_space_removes_everything() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let keep = Fx::new();
    let m = fx.msg(vec![], 1, b"m");
    let k = keep.msg(vec![], 1, b"k");
    s.insert_events_with_views(&[persist(&m), persist(&k)], &[view(&m, "a"), view(&k, "b")])
        .unwrap();
    s.ensure_space(&fx.space, Timestamp(1)).unwrap();
    s.put_key_entry(&KeyEntry {
        kind: KeyKind::State,
        id: b"s".to_vec(),
        scope: fx.space.as_bytes().to_vec(),
        space: Some(fx.space),
        epoch: None,
        not_before: None,
        key: Secret32::random(),
        created_at: Timestamp(1),
    })
    .unwrap();
    let (blob, data) = sample_blob(3);
    s.blob_register(&fx.space, &blob).unwrap();
    s.blob_write_chunk(&blob.id(), 0, chunk(&data, &blob, 0))
        .unwrap();
    let shared = sample_blob(1).0;
    s.blob_register(&fx.space, &shared).unwrap();
    s.blob_register(&keep.space, &shared).unwrap();

    let removed = s.purge_space(&fx.space).unwrap();
    assert_eq!(removed, vec![blob.id()]);
    assert!(!s.blob_files().path_for(&blob.id()).exists());
    assert!(
        s.blob_status(&shared.id()).unwrap().is_some(),
        "다른 공간도 쓰는 블롭은 남김"
    );
    assert!(s.get_event(&m.id()).unwrap().is_none());
    assert!(s.get_message(&m.id(), None).unwrap().is_none());
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM msg_keys WHERE event_id = ?1",
            &m.id()
        ),
        0
    );
    assert!(
        s.heads(&fx.space, StreamId::Channel(fx.channel))
            .unwrap()
            .is_empty()
    );
    assert!(s.get_key(KeyKind::State, b"s").unwrap().is_none());
    assert!(s.get_space(&fx.space).unwrap().is_none());
    assert_eq!(s.get_event(&k.id()).unwrap(), Some(k));
}

// ---- 설정·토큰·감사·확인·노드 ----

#[test]
fn settings_tokens_audit_confirmations() {
    let dir = tempfile::tempdir().unwrap();
    let opts = StoreOptions {
        audit_cap: 5,
        audit_important_cap: 3,
        ..Default::default()
    };
    let s = Store::open_with(dir.path(), &Secret32::random(), opts).unwrap();
    s.put_setting("seed/policy", &serde_json::json!({"enabled": true}))
        .unwrap();
    s.put_setting("seed/limit", &42u32).unwrap();
    s.put_setting("net/tor", &false).unwrap();
    assert_eq!(s.get_setting::<u32>("seed/limit").unwrap(), Some(42));
    assert_eq!(s.get_setting::<u32>("none").unwrap(), None);
    let seeds = s.settings_with_prefix("seed/").unwrap();
    assert_eq!(
        seeds.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["seed/limit", "seed/policy"]
    );
    assert!(s.delete_setting("net/tor").unwrap());

    let tok = ApiTokenRecord {
        id: "t1".into(),
        name: "claude".into(),
        token_hash: [3; 32],
        scopes: vec!["read".into(), "write".into()],
        spaces: vec![SpaceId::random()],
        automation: true,
        created_at: Timestamp(1),
        revoked: false,
        last_used_at: None,
    };
    s.create_api_token(&tok).unwrap();
    assert_eq!(s.find_api_token(&[3; 32]).unwrap(), Some(tok.clone()));
    s.touch_api_token("t1", Timestamp(9)).unwrap();
    assert_eq!(
        s.list_api_tokens().unwrap()[0].last_used_at,
        Some(Timestamp(9))
    );
    assert!(s.revoke_api_token("t1").unwrap());
    assert!(s.find_api_token(&[3; 32]).unwrap().is_none());

    let entry = |i: u64, method: String, important: bool| AuditEntry {
        seq: 0,
        at: Timestamp(i),
        token: "owner".into(),
        method,
        outcome: "ok".into(),
        detail: String::new(),
        important,
    };
    s.append_audit(&entry(0, "중요1".into(), true)).unwrap();
    for i in 0..8 {
        s.append_audit(&entry(i, format!("m{i}"), false)).unwrap();
    }
    let log = s.list_audit(100, None).unwrap();
    assert_eq!(log.len(), 6, "상한을 넘은 오래된 평범한 줄은 지워짐");
    assert_eq!(log[0].method, "m7");
    // 평범한 줄을 잔뜩 써도 중요한 줄은 남습니다.
    assert!(log.iter().any(|e| e.method == "중요1" && e.important));
    let older = s.list_audit(2, Some(log[1].seq)).unwrap();
    assert_eq!(
        older.iter().map(|e| e.method.as_str()).collect::<Vec<_>>(),
        vec!["m5", "m4"]
    );
    // 중요한 줄도 자기 상한(여기서는 3)을 넘으면 오래된 것부터 지움.
    for i in 2..6 {
        s.append_audit(&entry(i, format!("중요{i}"), true)).unwrap();
    }
    let important: Vec<String> = s
        .list_audit(100, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.important)
        .map(|e| e.method)
        .collect();
    assert_eq!(important, vec!["중요5", "중요4", "중요3"]);

    let c = PendingConfirmation {
        id: "c1".into(),
        created_at: Timestamp(1),
        expires_at: Some(Timestamp(100)),
        token: "claude".into(),
        method: "member.kick".into(),
        params: serde_json::json!({"user": "x"}),
        summary: "추방".into(),
    };
    s.put_confirmation(&c).unwrap();
    assert_eq!(s.list_confirmations().unwrap(), vec![c.clone()]);
    assert_eq!(s.take_confirmation("c1").unwrap(), Some(c.clone()));
    assert_eq!(s.take_confirmation("c1").unwrap(), None, "한 번만 꺼냄");
    s.put_confirmation(&c).unwrap();
    assert_eq!(s.prune_confirmations(Timestamp(100)).unwrap(), 1);
}

#[test]
fn peer_cache_and_invites() {
    let (s, _d) = temp();
    let dev = DeviceId::random();
    let user = UserId::random();
    s.record_peer_success(
        b"peer-a",
        &["/ip4/1.2.3.4/udp/1/quic-v1".into()],
        Timestamp(10),
    )
    .unwrap();
    s.record_peer_success(b"peer-b", &[], Timestamp(20))
        .unwrap();
    s.record_peer_failure(b"peer-a", Timestamp(30)).unwrap();
    s.set_peer_identity(b"peer-a", &user, &dev).unwrap();
    let a = s.get_peer(b"peer-a").unwrap().unwrap();
    assert_eq!((a.failures, a.user, a.device), (1, Some(user), Some(dev)));
    assert_eq!(a.addrs, vec!["/ip4/1.2.3.4/udp/1/quic-v1".to_string()]);
    // 기기가 다른 노드로 옮기면 예전 노드 기록에서 떨어짐.
    s.set_peer_identity(b"peer-b", &user, &dev).unwrap();
    assert_eq!(s.peer_for_device(&dev).unwrap().unwrap().peer_id, b"peer-b");
    assert_eq!(s.get_peer(b"peer-a").unwrap().unwrap().device, None);
    let recent = s.recent_peers(10).unwrap();
    assert_eq!(
        recent.iter().map(|p| p.peer_id.clone()).collect::<Vec<_>>(),
        vec![b"peer-b".to_vec(), b"peer-a".to_vec()]
    );
    assert_eq!(s.prune_peers(Timestamp(15), 10).unwrap(), 1);

    let space = SpaceId::random();
    assert!(
        s.record_invite_redemption(&space, &[1; 32], &user, Timestamp(1))
            .unwrap()
    );
    assert!(
        !s.record_invite_redemption(&space, &[1; 32], &user, Timestamp(2))
            .unwrap()
    );
    assert!(
        s.record_invite_redemption(&space, &[1; 32], &UserId::random(), Timestamp(3))
            .unwrap()
    );
    assert_eq!(s.invite_redemption_count(&space, &[1; 32]).unwrap(), 2);
}

// ---- 블롭 ----

fn sample_blob(chunks: usize) -> (PersistentBlob, Vec<u8>) {
    let size = MIN_CHUNK_SIZE as usize * chunks - 100;
    let salt = rand_bytes();
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8 ^ salt[i % 32]).collect();
    let m = BlobManifest::build(&data, MIN_CHUNK_SIZE);
    (PersistentBlob::new(m, SecurityLevel::Normal).unwrap(), data)
}

fn chunk<'a>(data: &'a [u8], b: &PersistentBlob, i: u32) -> &'a [u8] {
    let cs = b.manifest().chunk_size as usize;
    let start = i as usize * cs;
    &data[start..(start + cs).min(data.len())]
}

#[test]
fn blob_chunks_bitmap_and_corruption() {
    let (s, dir) = temp();
    let space = SpaceId::random();
    let (b, data) = sample_blob(3);
    let m = b.manifest().clone();
    assert!(matches!(
        PersistentBlob::new(m.clone(), SecurityLevel::Volatile),
        Err(StoreError::VolatileRejected)
    ));
    assert!(s.blob_register(&space, &b).unwrap());
    assert!(!s.blob_register(&space, &b).unwrap());
    assert!(s.blob_in_space(&b.id(), &space).unwrap());
    assert!(!s.blob_in_space(&b.id(), &SpaceId::random()).unwrap());
    // 경로: blobs/<2 hex>/<나머지 hex>
    let p = s.blob_files().path_for(&b.id());
    let rel = p
        .strip_prefix(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let parts: Vec<&str> = rel.split(std::path::MAIN_SEPARATOR).collect();
    assert_eq!(parts[0], BLOB_DIR_NAME);
    assert_eq!((parts[1].len(), parts[2].len()), (2, 62));

    // 잘못된 조각은 거부.
    assert!(matches!(
        s.blob_write_chunk(&b.id(), 0, chunk(&data, &b, 1)),
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(
        s.blob_write_chunk(&b.id(), 2, chunk(&data, &b, 2)).unwrap(),
        ChunkWrite::Stored { complete: false }
    );
    assert_eq!(
        s.blob_write_chunk(&b.id(), 2, chunk(&data, &b, 2)).unwrap(),
        ChunkWrite::AlreadyHave
    );
    let (bm, total) = s.blob_have(&b.id()).unwrap().unwrap();
    assert_eq!((bm, total), (vec![0b0010_0000], 3));
    assert_eq!(s.blob_read_chunk(&b.id(), 0).unwrap(), None);
    assert_eq!(
        s.blob_read_chunk(&b.id(), 2).unwrap().as_deref(),
        Some(chunk(&data, &b, 2))
    );
    s.blob_write_chunk(&b.id(), 0, chunk(&data, &b, 0)).unwrap();
    assert_eq!(
        s.blob_write_chunk(&b.id(), 1, chunk(&data, &b, 1)).unwrap(),
        ChunkWrite::Stored { complete: true }
    );
    let st = s.blob_status(&b.id()).unwrap().unwrap();
    assert!(st.complete);
    assert_eq!((st.have_count, st.stored_bytes), (3, data.len() as u64));
    assert_eq!(s.blob_total_size().unwrap(), data.len() as u64);
    assert_eq!(std::fs::read(&p).unwrap(), data);

    // 파일 손상 → 읽을 때 감지하고 다시 받도록 비트를 지움.
    let mut corrupted = data.clone();
    corrupted[5] ^= 0xff;
    std::fs::write(&p, &corrupted).unwrap();
    assert_eq!(s.blob_read_chunk(&b.id(), 0).unwrap(), None);
    let st = s.blob_status(&b.id()).unwrap().unwrap();
    assert!(!st.complete);
    assert_eq!(st.have_count, 2);
    assert_eq!(s.blob_have(&b.id()).unwrap().unwrap().0, vec![0b0110_0000]);

    assert!(s.blob_delete(&b.id()).unwrap());
    assert!(!p.exists());
    assert!(s.blob_status(&b.id()).unwrap().is_none());
    assert!(matches!(
        s.blob_write_chunk(&b.id(), 0, chunk(&data, &b, 0)),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn blob_lru_eviction() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    let mut blobs = Vec::new();
    for _ in 0..4 {
        let (b, data) = sample_blob(1);
        s.blob_register(&space, &b).unwrap();
        s.blob_write_chunk(&b.id(), 0, &data).unwrap();
        blobs.push((b.id(), data.len() as u64, data));
    }
    // 접근 순서를 정함: 0이 가장 오래됨.
    for (i, (id, _, _)) in blobs.iter().enumerate() {
        s.write(|tx| {
            tx.execute(
                "UPDATE blobs SET last_access = ?2 WHERE blob_id = ?1",
                rusqlite::params![id.as_bytes(), i as i64],
            )?;
            Ok(())
        })
        .unwrap();
    }
    s.blob_set_pinned(&blobs[0].0, true).unwrap();
    let one = blobs[0].1;
    // 두 개만 남게: 고정된 0과 보호한 1은 건너뛰고 2, 3을 비움.
    let evicted = s.blob_evict_lru(one * 2, &[blobs[1].0]).unwrap();
    assert_eq!(evicted, vec![blobs[2].0, blobs[3].0]);
    assert_eq!(s.blob_total_size().unwrap(), one * 2);
    assert!(s.blob_evict_lru(one * 2, &[]).unwrap().is_empty());

    // 비운 블롭은 등록·공간 소속(닫힌 스웜)이 남고 데이터만 없어 다시 받을 수 있음.
    let (id2, _, data2) = &blobs[2];
    let st = s.blob_status(id2).unwrap().unwrap();
    assert_eq!((st.stored_bytes, st.have_count, st.complete), (0, 0, false));
    assert!(s.blob_in_space(id2, &space).unwrap());
    assert_eq!(s.blobs_in_space(&space).unwrap().len(), 4);
    assert_eq!(s.blob_have(id2).unwrap(), Some((vec![0], 1)));
    assert_eq!(s.blob_read_chunk(id2, 0).unwrap(), None);
    assert_eq!(s.blob_files().file_size(id2).unwrap(), 0, "파일도 지움");
    assert_eq!(
        s.blob_write_chunk(id2, 0, data2).unwrap(),
        ChunkWrite::Stored { complete: true }
    );
    assert_eq!(
        s.blob_read_chunk(id2, 0).unwrap().as_deref(),
        Some(&data2[..])
    );
    assert_eq!(s.blob_total_size().unwrap(), one * 3);

    // 데이터가 없는 블롭은 비울 것이 없어 결과에 넣지 않음.
    assert_eq!(s.blob_drop_data(&blobs[3].0).unwrap(), 0);
    assert_eq!(s.blob_drop_data(&BlobId::random()).unwrap(), 0);
    let evicted = s.blob_evict_lru(0, &[]).unwrap();
    assert_eq!(evicted, vec![blobs[1].0, blobs[2].0], "고정된 0만 남음");
    assert_eq!(s.blob_total_size().unwrap(), one);
}

#[test]
fn blob_cache_entries_follow_last_access() {
    let (s, _d) = temp();
    let space = SpaceId::random();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (b, data) = sample_blob(1);
        s.blob_register(&space, &b).unwrap();
        s.blob_write_chunk(&b.id(), 0, &data).unwrap();
        ids.push(b.id());
    }
    // 데이터가 없는 블롭은 목록에 없음.
    let (empty, _) = sample_blob(1);
    s.blob_register(&space, &empty).unwrap();
    for (i, id) in ids.iter().enumerate() {
        assert!(
            s.blob_set_last_access(id, Timestamp(100 - i as u64))
                .unwrap()
        );
    }
    assert!(
        !s.blob_set_last_access(&BlobId::random(), Timestamp(1))
            .unwrap()
    );
    let got: Vec<BlobId> = s
        .blob_cache_entries()
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(got, vec![ids[2], ids[1], ids[0]]);
    s.blob_drop_data(&ids[1]).unwrap();
    let got: Vec<BlobId> = s
        .blob_cache_entries()
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(got, vec![ids[2], ids[0]]);
    // 등록된 블롭은 데이터를 비웠거나 받지 않았어도 모두 나옵니다.
    let mut all = s.blob_ids().unwrap();
    all.sort();
    let mut want = vec![ids[0], ids[1], ids[2], empty.id()];
    want.sort();
    assert_eq!(all, want);
}

// ---- 누출 검사 ----

#[test]
fn leak_scan_finds_disk_marker_but_not_volatile() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let disk_marker = "LEAKCHECK-disk-5f1c2a";
    let ram_marker = "LEAKCHECK-ram-9b7e3d";
    let e = fx.msg(vec![], 1, b"ciphertext");
    s.insert_events_with_views(&[persist(&e)], &[view(&e, &format!("hello {disk_marker}"))])
        .unwrap();
    let found = s.scan_for_marker(disk_marker.as_bytes()).unwrap();
    assert!(
        found
            .iter()
            .any(|f| f.starts_with("db:messages.content_sealed")),
        "{found:?}"
    );

    // 휘발성: RAM 저장소에만 들어감(본문에 표식 평문을 넣어 더 엄격하게 검사).
    let vs = VolatileStore::new();
    let v = fx.volatile_msg(1, 2, format!("ram {ram_marker}").as_bytes());
    vs.set_key(
        &fx.space,
        &fx.channel,
        1,
        &Secret32::random(),
        Instant::now(),
    )
    .unwrap();
    assert!(
        vs.insert(
            &fx.space,
            &fx.channel,
            v,
            Some(Duration::from_secs(30)),
            Instant::now()
        )
        .unwrap()
    );
    s.checkpoint().unwrap();
    assert!(s.scan_for_marker(ram_marker.as_bytes()).unwrap().is_empty());
    // 평문 열에 들어간 표식도 찾음.
    s.put_setting("diag/test", &format!("x{ram_marker}x"))
        .unwrap();
    assert_eq!(
        s.scan_for_marker(ram_marker.as_bytes()).unwrap(),
        vec!["db:settings.value".to_string()]
    );
    // 데이터 디렉터리 파일(로그 등)도 훑음.
    std::fs::write(s.dir().join("daemon.log"), format!("..{disk_marker}..")).unwrap();
    let found = s.scan_for_marker(disk_marker.as_bytes()).unwrap();
    assert!(found.contains(&"file:daemon.log".to_string()), "{found:?}");
    assert!(s.scan_for_marker(b"x").is_err());
}

/// 암호화돼 저장된 첨부는 파일 검사로 보이지 않으므로, 키를 아는 블롭은 풀어서 훑습니다.
#[test]
fn leak_scan_decrypts_known_blobs() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let marker = "LEAKCHECK-blob-4e8a1c";
    let key = Secret32::random();
    // 표식이 조각 경계에 걸치게 둡니다.
    let per = MIN_CHUNK_SIZE as usize - 16;
    let mut plain = vec![b'a'; per - 5];
    plain.extend_from_slice(marker.as_bytes());
    plain.extend_from_slice(&[b'b'; 100]);
    let (ct, m) = yangpa_crypto::encrypt_blob(&key, &plain, MIN_CHUNK_SIZE);
    let pb = PersistentBlob::new(m.clone(), SecurityLevel::Normal).unwrap();
    s.blob_register(&fx.space, &pb).unwrap();
    for (i, c) in ct.chunks(m.chunk_size as usize).enumerate() {
        s.blob_write_chunk(&pb.id(), i as u32, c).unwrap();
    }
    s.checkpoint().unwrap();
    assert!(
        s.scan_for_marker(marker.as_bytes()).unwrap().is_empty(),
        "암호문 파일에는 보이지 않음"
    );
    let found = s
        .scan_blobs_for_marker(marker.as_bytes(), &[(pb.id(), key.clone())])
        .unwrap();
    assert_eq!(found, vec![format!("blob:{}", pb.id().short())]);
    assert!(
        s.scan_blobs_for_marker(b"LEAKCHECK-none-000", &[(pb.id(), key)])
            .unwrap()
            .is_empty()
    );
    let wrong = s
        .scan_blobs_for_marker(marker.as_bytes(), &[(pb.id(), Secret32::random())])
        .unwrap();
    assert!(wrong[0].contains("검사 불가"), "{wrong:?}");
}

// ---- 동시성·성능 ----

#[test]
fn concurrent_readers_and_writer() {
    let (s, _d) = temp();
    let s = Arc::new(s);
    let fx = Arc::new(Fx::new());
    let evs: Vec<SignedEvent> = {
        let mut prev: Option<EventId> = None;
        (1..=400u64)
            .map(|l| {
                let e = fx.msg(prev.into_iter().collect(), l, &l.to_be_bytes());
                prev = Some(e.id());
                e
            })
            .collect()
    };
    let writer = {
        let s = s.clone();
        let evs = evs.clone();
        std::thread::spawn(move || {
            for chunk in evs.chunks(20) {
                s.insert_events(&chunk.iter().map(persist).collect::<Vec<_>>())
                    .unwrap();
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let s = s.clone();
            let fx = fx.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let heads = s.heads(&fx.space, StreamId::Channel(fx.channel)).unwrap();
                    assert!(heads.len() <= 1, "직선 사슬의 머리는 하나");
                    let _ = s.max_lamport(&fx.space).unwrap();
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(
        s.heads(&fx.space, StreamId::Channel(fx.channel)).unwrap(),
        vec![evs.last().unwrap().id()]
    );
    assert!(
        s.missing_parents(&fx.space, StreamId::Channel(fx.channel))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn insert_10k_events_quickly() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let channels: Vec<ChannelId> = (0..4).map(|_| ChannelId::random()).collect();
    let mut prev: HashMap<ChannelId, EventId> = HashMap::new();
    let mut evs = Vec::with_capacity(10_000);
    for i in 0..10_000u64 {
        let ch = channels[(i % 4) as usize];
        let parents = prev.get(&ch).copied().into_iter().collect();
        let e = fx.msg_in(ch, parents, i + 1, &[i as u8; 200]);
        prev.insert(ch, e.id());
        evs.push(persist(&e));
    }
    let start = Instant::now();
    for batch in evs.chunks(1000) {
        let out = s.insert_events(batch).unwrap();
        assert!(out.iter().all(|o| *o == InsertOutcome::Inserted));
    }
    let took = start.elapsed();
    eprintln!("10k 이벤트 저장: {took:?}");
    assert!(took < Duration::from_secs(10), "너무 느림: {took:?}");
    assert_eq!(s.event_count(&fx.space).unwrap(), 10_000);
    for ch in &channels {
        assert_eq!(
            s.heads(&fx.space, StreamId::Channel(*ch)).unwrap(),
            vec![prev[ch]]
        );
    }
    let fp = s
        .fingerprints(
            &fx.space,
            StreamId::Channel(channels[0]),
            &[LamportRange {
                start: 0,
                end: u64::MAX,
            }],
        )
        .unwrap();
    assert_eq!(fp[0].count, 2500);
    // 중복 묶음은 더 빨리 걸러짐.
    let start = Instant::now();
    let out = s.insert_events(&evs[..1000]).unwrap();
    assert!(out.iter().all(|o| *o == InsertOutcome::Duplicate));
    eprintln!("1k 중복 거르기: {:?}", start.elapsed());
}

#[test]
fn store_reopens_with_data_and_volatile_key_ref_rejected_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    let fx = Fx::new();
    let e = fx.msg(vec![], 1, b"persist");
    {
        let s = Store::open(dir.path(), &key).unwrap();
        s.insert_events_with_views(&[persist(&e)], &[view(&e, "남는 글")])
            .unwrap();
    }
    let s = Store::open(dir.path(), &key).unwrap();
    let m = s.list_messages(&fx.space, &fx.channel, None, 10).unwrap();
    assert_eq!(m[0].content.as_ref().unwrap().body.text, "남는 글");
    assert!(matches!(e.header.key, KeyRef::Record { .. }));
}

// ---- 검토에서 고친 문제의 회귀 테스트 ----

fn volatile_settings() -> ChannelSettings {
    let mut c = ChannelSettings::text("비밀");
    c.security = SecurityLevel::Volatile;
    c
}

fn table_count(s: &Store, table: &str) -> i64 {
    s.read(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .unwrap()
}

#[test]
fn volatile_channel_events_never_reach_disk() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let mut state = sample_state(fx.user_id());
    state.channels.insert(fx.channel, volatile_settings());
    s.put_space_state(&fx.space, &state).unwrap();
    assert_eq!(table_count(&s, "volatile_channels"), 1);

    // 호출자가 등급을 일반으로 잘못 넘겨도(기록 키 참조) 휘발성 채널 이벤트는 디스크로 가지 않음.
    let e = fx.msg(vec![], 1, b"x");
    assert!(matches!(
        s.insert_events(&[persist(&e)]),
        Err(StoreError::VolatileRejected)
    ));
    assert!(matches!(
        s.insert_events_with_views(&[persist(&e)], &[view(&e, "비밀")]),
        Err(StoreError::VolatileRejected)
    ));
    // 일반 채널 이벤트와 섞인 묶음도 통째로 거부(일부만 저장되지 않음).
    let normal = fx.msg_in(ChannelId::random(), vec![], 1, b"y");
    assert!(matches!(
        s.insert_events(&[persist(&normal), persist(&e)]),
        Err(StoreError::VolatileRejected)
    ));
    assert!(s.get_event(&e.id()).unwrap().is_none());
    assert!(s.get_event(&normal.id()).unwrap().is_none());
    s.insert_events(&[persist(&normal)]).unwrap();
    // 공간 수준 이벤트는 영향 없음(그 채널을 가리키는 상태 이벤트도 공간 스트림이라 저장).
    let st = fx.kind_event(EventKind::RoleCreate, vec![], 1);
    let ch_update = SignedEvent::sign(
        fx.header(EventKind::ChannelUpdate, Some(fx.channel), vec![], 2),
        vec![0; 8],
        &fx.dev,
    )
    .unwrap();
    s.insert_events(&[persist(&st), persist(&ch_update)])
        .unwrap();

    // 상태에서 채널이 빠져도(채널 삭제) 기록은 남아 계속 거부.
    let mut later = state.clone();
    later.channels.remove(&fx.channel);
    s.put_space_state(&fx.space, &later).unwrap();
    assert!(matches!(
        s.insert_events(&[persist(&e)]),
        Err(StoreError::VolatileRejected)
    ));

    // 공간을 떠나면 기록도 지움.
    s.purge_space(&fx.space).unwrap();
    assert_eq!(table_count(&s, "volatile_channels"), 0);
}

/// 5판 이전의 DB(기기 항목에 서명 공개키가 없던 기기 목록)는 이전을 적용하지 않고 멈춥니다. 하나라도 적용했다면 다시
/// 열 때 검사를 건너뛰어 열리므로, 두 번 다 같은 오류인지로 아무것도 바꾸지 않았음을 봅니다.
#[test]
fn v5_migration_refuses_old_device_lists() {
    use yangpa_core::sign::Sig;

    #[derive(serde::Serialize)]
    struct OldEntry {
        device: DeviceId,
        box_key: BoxPublicKey,
        name: String,
        added_at: Timestamp,
        revoked_at: Option<Timestamp>,
    }
    #[derive(serde::Serialize)]
    struct OldList {
        user: UserId,
        version: u64,
        devices: Vec<OldEntry>,
    }
    #[derive(serde::Serialize)]
    struct OldSigned {
        list: OldList,
        sig: Sig,
        cosig: Option<()>,
    }

    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    let user = UserId::random();
    {
        let s = Store::open(dir.path(), &key).unwrap();
        let old = OldSigned {
            list: OldList {
                user,
                version: 1,
                devices: vec![OldEntry {
                    device: DeviceId::random(),
                    box_key: BoxPublicKey::filled(1),
                    name: "PC".into(),
                    added_at: Timestamp(1),
                    revoked_at: None,
                }],
            },
            sig: Sig::from_bytes(&[0; Sig::LEN]).unwrap(),
            cosig: None,
        };
        let bytes = postcard::to_stdvec(&old).unwrap();
        s.write(|tx| {
            tx.execute(
                "INSERT INTO device_lists (user, version, list, updated_at) VALUES (?1, 1, ?2, 1)",
                rusqlite::params![user.as_bytes(), bytes],
            )?;
            Ok(tx.execute_batch(
                "DROP TABLE space_members; DROP TABLE space_bans; DROP TABLE space_channels; \
                 DROP TABLE device_keys; ALTER TABLE spaces DROP COLUMN head; \
                 PRAGMA user_version = 4;",
            )?)
        })
        .unwrap();
    }
    for _ in 0..2 {
        assert!(matches!(
            Store::open(dir.path(), &key),
            Err(StoreError::IncompatibleData)
        ));
    }
}

#[test]
fn v2_migration_backfills_volatile_channels() {
    let dir = tempfile::tempdir().unwrap();
    let key = Secret32::random();
    let fx = Fx::new();
    let broken = SpaceId::random();
    {
        let s = Store::open(dir.path(), &key).unwrap();
        let mut state = sample_state(fx.user_id());
        state.channels.insert(fx.channel, volatile_settings());
        s.put_space_state(&fx.space, &state).unwrap();
        s.ensure_space(&broken, Timestamp(1)).unwrap();
        // 버전 1 데이터베이스로 되돌림(옛 JSON 상태 캐시, 읽을 수 없는 상태 캐시도 하나 둠).
        let json = serde_json::to_string(&state).unwrap();
        s.write(|tx| {
            tx.execute(
                "UPDATE spaces SET state = ?2 WHERE space = ?1",
                rusqlite::params![fx.space.as_bytes(), json],
            )?;
            tx.execute(
                "UPDATE spaces SET state = '{깨짐' WHERE space = ?1",
                [broken.as_bytes()],
            )?;
            Ok(tx.execute_batch(
                "DROP TABLE space_members; DROP TABLE space_bans; DROP TABLE space_channels; \
                 DROP TABLE device_keys; ALTER TABLE spaces DROP COLUMN head; \
                 DROP INDEX events_channel_received; DROP INDEX pending_edits_edit; \
                 DROP INDEX message_edits_edit; DROP TABLE volatile_channels; \
                 DROP INDEX audit_log_tier; ALTER TABLE audit_log DROP COLUMN important; \
                 ALTER TABLE friends DROP COLUMN kind; ALTER TABLE friends DROP COLUMN expires_at; \
                 ALTER TABLE friends DROP COLUMN link_hash; ALTER TABLE messages DROP COLUMN anonymous; \
                 PRAGMA user_version = 1;",
            )?)
        })
        .unwrap();
        assert_eq!(s.schema_version().unwrap(), 1);
    }
    let s = Store::open(dir.path(), &key).unwrap();
    assert_eq!(
        s.schema_version().unwrap(),
        crate::schema::MIGRATIONS.len() as i64
    );
    assert_eq!(table_count(&s, "volatile_channels"), 1);
    // 버전 5: 옛 JSON 캐시는 비우고(이벤트를 다시 해소해 행을 씀) 휘발성 표시는 남김.
    assert_eq!(s.load_space_state(&fx.space).unwrap(), None);
    let left: i64 = s
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM spaces WHERE state IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(left, 0);
    // 버전 3(감사 로그 종류)도 함께 올라감.
    s.append_audit(&AuditEntry {
        seq: 0,
        at: Timestamp(1),
        token: "owner".into(),
        method: "system.status".into(),
        outcome: "ok".into(),
        detail: String::new(),
        important: true,
    })
    .unwrap();
    assert!(s.list_audit(10, None).unwrap()[0].important);
    let e = fx.msg(vec![], 1, b"x");
    assert!(matches!(
        s.insert_events(&[persist(&e)]),
        Err(StoreError::VolatileRejected)
    ));
}

fn set_received(s: &Store, id: &EventId, at: i64) {
    s.write(|tx| {
        tx.execute(
            "UPDATE events SET received_at = ?2 WHERE id = ?1",
            rusqlite::params![id.as_bytes(), at],
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn expire_channel_messages_by_receive_time() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m1 = fx.msg(vec![], 1, b"a");
    let m2 = fx.msg(vec![m1.id()], 2, b"b");
    let m3 = fx.msg(vec![m2.id()], 3, b"c");
    let edit = fx.kind_event(EventKind::Edit, vec![m3.id()], 4);
    let other_channel = fx.msg_in(ChannelId::random(), vec![], 1, b"o");
    let st = fx.kind_event(EventKind::RoleCreate, vec![], 1);
    s.insert_events_with_views(
        &[
            persist(&m1),
            persist(&m2),
            persist(&m3),
            persist(&other_channel),
        ],
        &[
            view(&m1, "a"),
            view(&m2, "b"),
            view(&m3, "c"),
            view(&other_channel, "o"),
        ],
    )
    .unwrap();
    s.insert_events(&[persist(&edit), persist(&st)]).unwrap();
    for (id, at) in [
        (m1.id(), 100),
        (edit.id(), 150),
        (m2.id(), 200),
        (m3.id(), 300),
    ] {
        set_received(&s, &id, at);
    }
    set_received(&s, &other_channel.id(), 0);
    set_received(&s, &st.id(), 0);

    // 받은 시각 순으로, 묶음 크기만큼.
    let cutoff = Some(Timestamp(201));
    assert_eq!(
        s.expire_channel_messages(&fx.space, &fx.channel, cutoff, 1)
            .unwrap(),
        vec![m1.id()]
    );
    assert_eq!(
        s.expire_channel_messages(&fx.space, &fx.channel, cutoff, 100)
            .unwrap(),
        vec![edit.id(), m2.id()]
    );
    assert!(
        s.expire_channel_messages(&fx.space, &fx.channel, cutoff, 100)
            .unwrap()
            .is_empty(),
        "이미 지운 것은 다시 나오지 않음"
    );
    for id in [m1.id(), edit.id(), m2.id()] {
        let ev = s.get_event(&id).unwrap().expect("헤더는 남음");
        assert_eq!(ev.body, None);
        assert!(s.get_message(&id, None).unwrap().is_none());
        assert_eq!(
            count(&s, "SELECT COUNT(*) FROM msg_keys WHERE event_id = ?1", &id),
            0
        );
    }
    assert!(s.get_message(&m3.id(), None).unwrap().is_some());
    assert!(
        s.get_message(&other_channel.id(), None).unwrap().is_some(),
        "다른 채널은 그대로"
    );
    assert!(s.get_event(&st.id()).unwrap().unwrap().body.is_some());

    // 같은 이벤트를 다시 받아도 되살아나지 않음.
    s.insert_events_with_views(&[persist(&m1)], &[view(&m1, "a")])
        .unwrap();
    assert_eq!(s.get_event(&m1.id()).unwrap().unwrap().body, None);
    assert!(s.get_message(&m1.id(), None).unwrap().is_none());

    // 퇴장: 시각 조건 없이 전부(묶음 크기 0은 1로).
    assert_eq!(
        s.expire_channel_messages(&fx.space, &fx.channel, None, 0)
            .unwrap(),
        vec![m3.id()]
    );
    assert!(
        s.expire_channel_messages(&fx.space, &fx.channel, None, u32::MAX)
            .unwrap()
            .is_empty()
    );
}

fn set_created(s: &Store, id: &EventId, at: i64) {
    s.write(|tx| {
        tx.execute(
            "UPDATE events SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![id.as_bytes(), at],
        )?;
        Ok(())
    })
    .unwrap();
}

/// 보관 전용 노드용: 작성 시각 기준 만료, 스트림 통째 지우기, 저장 바이트, 시더 제외 묶음.
#[test]
fn archive_store_helpers() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m1 = fx.msg(vec![], 1, b"a");
    let m2 = fx.msg(vec![m1.id()], 2, b"b");
    let m3 = fx.msg(vec![m2.id()], 3, b"c");
    let other_channel = fx.msg_in(ChannelId::random(), vec![], 1, b"o");
    s.insert_events(&[
        persist(&m1),
        persist(&m2),
        persist(&m3),
        persist(&other_channel),
    ])
    .unwrap();
    // (작성, 받음): m2는 작성 시각이 미래로 꾸며져 받은 시각으로 셈.
    for (id, created, received) in [
        (m1.id(), 100, 500),
        (m2.id(), 900, 300),
        (m3.id(), 600, 700),
        (other_channel.id(), 0, 0),
    ] {
        set_created(&s, &id, created);
        set_received(&s, &id, received);
    }
    let before = s.space_event_bytes(&fx.space).unwrap();
    assert!(before > 0);
    assert_eq!(
        s.expire_channel_messages_authored(&fx.space, &fx.channel, Timestamp(400), 100)
            .unwrap(),
        vec![m2.id(), m1.id()]
    );
    let after = s.space_event_bytes(&fx.space).unwrap();
    assert!(
        after < before,
        "지운 본문만큼 줄어야 함: {before} → {after}"
    );

    assert_eq!(
        s.set_seed_excluded_many(&[m3.id(), m1.id()], true).unwrap(),
        2
    );
    assert!(s.is_seed_excluded(&m3.id()).unwrap());
    assert!(s.event_meta(&m1.id()).unwrap().unwrap().seed_excluded);

    s.mark_message_deleted(&fx.space, &m3.id(), Timestamp(1))
        .unwrap();
    assert_eq!(
        s.purge_stream(&fx.space, StreamId::Channel(fx.channel))
            .unwrap(),
        3
    );
    assert!(!s.has_event(&m1.id()).unwrap());
    assert!(
        s.heads(&fx.space, StreamId::Channel(fx.channel))
            .unwrap()
            .is_empty()
    );
    assert!(
        s.has_event(&other_channel.id()).unwrap(),
        "다른 채널은 그대로"
    );
    assert!(s.is_deleted(&m3.id()).unwrap(), "삭제 표시는 남음");
    // 다시 받으면 처음부터(삭제 표시가 있는 것은 본문 없이).
    s.insert_events(&[persist(&m1), persist(&m3)]).unwrap();
    assert!(s.event_meta(&m1.id()).unwrap().unwrap().has_body);
    assert!(!s.event_meta(&m3.id()).unwrap().unwrap().has_body);
}

fn edit_of(e: &SignedEvent, text: &str) -> EditUpdate {
    EditUpdate {
        event: e.id(),
        lamport: e.header.lamport,
        at: Timestamp(e.header.lamport * 1000),
        text: text.into(),
        embeds: vec![],
    }
}

fn text_of(s: &Store, id: &EventId) -> String {
    s.get_message(id, None)
        .unwrap()
        .unwrap()
        .content
        .unwrap()
        .body
        .text
}

#[test]
fn edits_by_other_users_are_rejected() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let mallory = peer_of(&fx);
    let m = fx.msg(vec![], 1, b"orig");
    // 위조 수정이 논리 시계가 더 커도 이기지 못해야 함.
    let forged = mallory.kind_event(EventKind::Edit, vec![m.id()], 5);
    let own = fx.kind_event(EventKind::Edit, vec![m.id()], 3);
    assert!(matches!(
        s.apply_edit(&m.id(), &edit_of(&own, "own")),
        Err(StoreError::NotFound)
    ));
    s.insert_events(&[persist(&forged), persist(&own)]).unwrap();
    // 원본보다 먼저 오면 작성자를 모르므로 보관만.
    assert!(!s.apply_edit(&m.id(), &edit_of(&forged, "forged")).unwrap());
    assert!(!s.apply_edit(&m.id(), &edit_of(&own, "own")).unwrap());
    s.insert_events_with_views(&[persist(&m)], &[view(&m, "orig")])
        .unwrap();
    assert_eq!(
        text_of(&s, &m.id()),
        "own",
        "다른 사람의 보관 수정은 건너뜀"
    );
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM pending_edits WHERE target = ?1",
            &m.id()
        ),
        0
    );
    // 원본이 있으면 바로 거부.
    assert!(matches!(
        s.apply_edit(&m.id(), &edit_of(&forged, "forged")),
        Err(StoreError::InvalidInput(_))
    ));
    // 같은 사용자라도 다른 공간의 수정은 거부.
    let elsewhere = Fx {
        dev: fx.dev.clone(),
        user: fx.user.clone(),
        space: SpaceId::random(),
        channel: fx.channel,
    };
    let cross = elsewhere.kind_event(EventKind::Edit, vec![], 9);
    s.insert_events(&[persist(&cross)]).unwrap();
    assert!(matches!(
        s.apply_edit(&m.id(), &edit_of(&cross, "cross")),
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(text_of(&s, &m.id()), "own");
}

#[test]
fn erasing_an_edit_drops_its_pending_copy() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"orig");
    let e1 = fx.kind_event(EventKind::Edit, vec![m.id()], 2);
    s.insert_events(&[persist(&e1)]).unwrap();
    assert!(!s.apply_edit(&m.id(), &edit_of(&e1, "pending")).unwrap());
    let q = "SELECT COUNT(*) FROM pending_edits WHERE edit_id = ?1";
    assert_eq!(count(&s, q, &e1.id()), 1);
    // 수정 이벤트 본문을 지우면 그 키로 봉인한 보관 사본도 함께 지움(읽을 수 없는 행이 남지 않게).
    assert!(s.erase_body(&e1.id()).unwrap());
    assert_eq!(count(&s, q, &e1.id()), 0);
    s.insert_events_with_views(&[persist(&m)], &[view(&m, "orig")])
        .unwrap();
    assert_eq!(text_of(&s, &m.id()), "orig");
}

#[test]
fn views_must_match_signed_header() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"x");
    s.insert_events(&[persist(&m)]).unwrap();
    let tamper: [&dyn Fn(&mut MessageRecord); 6] = [
        &|v| v.author = UserId::random(),
        &|v| v.author_device = DeviceId::random(),
        &|v| v.lamport = 99,
        &|v| v.created_at = Timestamp(1),
        &|v| v.kind = MessageKind::ForumPost,
        &|v| v.channel = ChannelId::random(),
    ];
    for f in tamper {
        let mut v = view(&m, "hi");
        f(&mut v);
        assert!(matches!(
            s.put_message_view(&v),
            Err(StoreError::InvalidInput(_))
        ));
    }
    assert!(s.get_message(&m.id(), None).unwrap().is_none());
    // 같은 트랜잭션으로 넣을 때도 이벤트까지 통째로 거부.
    let m2 = fx.msg(vec![m.id()], 2, b"y");
    let mut bad = view(&m2, "y");
    bad.author = UserId::random();
    assert!(s.insert_events_with_views(&[persist(&m2)], &[bad]).is_err());
    assert!(s.get_event(&m2.id()).unwrap().is_none());
    // 이벤트가 없으면 NotFound, 맞으면 저장.
    assert!(matches!(
        s.put_message_view(&view(&m2, "y")),
        Err(StoreError::NotFound)
    ));
    assert!(s.put_message_view(&view(&m, "hi")).unwrap());
    assert_eq!(text_of(&s, &m.id()), "hi");
}

#[test]
fn range_queries_are_clamped() {
    let (s, _d) = temp();
    let fx = Fx::new();
    let n = MAX_RANGE_EVENTS as u64 + 10;
    let evs: Vec<PersistentEvent> = (1..=n).map(|l| persist(&fx.msg(vec![], l, &[1]))).collect();
    for batch in evs.chunks(1000) {
        s.insert_events(batch).unwrap();
    }
    let all = LamportRange {
        start: 0,
        end: u64::MAX,
    };
    let stream = StreamId::Channel(fx.channel);
    assert_eq!(
        s.stream_events(&fx.space, stream, all, u32::MAX)
            .unwrap()
            .len(),
        MAX_RANGE_EVENTS as usize,
        "상대가 큰 limit를 보내도 한 번에 읽는 양은 제한"
    );
    assert_eq!(
        s.ids_in_range(&fx.space, stream, all, u32::MAX)
            .unwrap()
            .len(),
        n as usize
    );
    assert_eq!(s.ids_in_range(&fx.space, stream, all, 7).unwrap().len(), 7);
}

#[test]
fn debug_output_redacts_message_content() {
    let secret = "SECRET-7d1e";
    let fx = Fx::new();
    let m = fx.msg(vec![], 1, b"x");
    let mut rec = view(&m, secret);
    rec.content.as_mut().unwrap().forum = Some(ForumMeta {
        title: secret.into(),
        tags: vec![1],
    });
    let edit = EditUpdate {
        event: m.id(),
        lamport: 2,
        at: Timestamp(2),
        text: secret.into(),
        embeds: vec![],
    };
    let forum = ForumUpdate {
        event: m.id(),
        lamport: 3,
        title: Some(secret.into()),
        tags: None,
        pinned: None,
        locked: None,
        edited_at: None,
    };
    for out in [
        format!("{rec:?}"),
        format!("{:?}", rec.content),
        format!("{edit:?}"),
        format!("{forum:?}"),
    ] {
        assert!(!out.contains(secret), "디버그 출력에 내용이 나옴: {out}");
    }
    assert!(format!("{rec:?}").contains("text_len"));
}

// ---- 폐기된 기기 지우기 ----

#[test]
fn wipe_all_removes_rows_and_blob_files_but_keeps_named_secrets() {
    let (store, _dir) = temp();
    let fx = Fx::new();
    let e = fx.msg(vec![], 1, b"wipe me");
    store.insert_events(&[persist(&e)]).unwrap();
    store.put_secret("net", &[1; 32]).unwrap();
    store.put_secret("identity", &[2; 32]).unwrap();
    store.put_setting("어떤.설정", &5u32).unwrap();
    store.put_setting("net.tor", &true).unwrap();
    let blob = BlobId([9; 32]);
    store.blob_files.write_at(&blob, 0, b"blob bytes").unwrap();
    assert_eq!(store.blob_files.file_size(&blob).unwrap(), 10);

    store.wipe_all(&["net"], &["net."]).unwrap();

    assert!(store.get_event(&e.id()).unwrap().is_none());
    assert!(store.get_secret("identity").unwrap().is_none());
    assert_eq!(
        store
            .get_secret("net")
            .unwrap()
            .as_deref()
            .map(|v| v.to_vec()),
        Some(vec![1u8; 32])
    );
    assert_eq!(store.get_setting::<u32>("어떤.설정").unwrap(), None);
    // 남길 설정은 같은 트랜잭션에서 남습니다(지우기 뒤 다시 쓰지 않음).
    assert_eq!(store.get_setting::<bool>("net.tor").unwrap(), Some(true));
    assert_eq!(store.blob_files.file_size(&blob).unwrap(), 0);
    // 지운 뒤에도 그대로 쓸 수 있습니다.
    store.insert_events(&[persist(&e)]).unwrap();
    assert!(store.get_event(&e.id()).unwrap().is_some());
}

/// 큰 기록 검색 시간(손으로 돌림): `YANGPA_SEARCH_BENCH_N=1000000 cargo test -p yangpa-store --release search_bench -- --ignored --nocapture`.
#[test]
#[ignore = "성능 측정"]
fn search_bench_large() {
    let n: u64 = std::env::var("YANGPA_SEARCH_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let (s, _d) = temp();
    let fx = Fx::new();
    let words = [
        "회의",
        "자료",
        "내일",
        "오늘",
        "점심",
        "프로젝트",
        "배포",
        "버그",
        "고쳤어요",
        "확인",
        "부탁",
        "감사합니다",
        "ㅋㅋㅋ",
        "release",
        "build",
        "test",
        "Tor",
        "양파",
        "메신저",
        "검색",
        "파일",
        "사진",
        "링크",
        "좋아요",
    ];
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let started = Instant::now();
    for chunk in (1..=n).collect::<Vec<_>>().chunks(2000) {
        let mut evs = Vec::with_capacity(chunk.len());
        let mut views = Vec::with_capacity(chunk.len());
        for &i in chunk {
            let e = fx.msg(vec![], i, b"m");
            let len = 3 + next() % 12;
            let text: Vec<&str> = (0..len)
                .map(|_| words[(next() % words.len() as u64) as usize])
                .collect();
            let mut text = text.join(" ");
            if i % 50_000 == 7 {
                text.push_str(" 아주드문낱말");
            }
            views.push(view(&e, &text));
            evs.push(persist(&e));
        }
        s.insert_events_with_views(&evs, &views).unwrap();
    }
    eprintln!("{n}개 넣기 {:?}", started.elapsed());
    let all = SearchFilter::default();
    let time = |label: &str, query: &str, offset: usize| {
        let t = Instant::now();
        let p = s.search_messages(query, all, |_| true, offset, 50).unwrap();
        eprintln!(
            "{label:>16}: {:>9} ms, 전체 {}개, 쪽 {}개",
            t.elapsed().as_secs_f64() * 1000.0,
            p.total,
            p.ids.len()
        );
    };
    time("첫 검색(색인)", "회의", 0);
    time("흔한 낱말", "회의", 0);
    time("흔한 낱말 깊은 쪽", "회의", 100_000);
    time("드문 낱말", "아주드문낱말", 0);
    time("낱말 셋", "회의 자료 배포", 0);
    time("구절", "\"회의 자료\"", 0);
    time("없는 낱말", "없는낱말xyz", 0);
    let me = fx.user_id();
    let t = Instant::now();
    let p = s
        .search_messages(
            "",
            SearchFilter {
                author: Some(&me),
                ..all
            },
            |_| true,
            0,
            50,
        )
        .unwrap();
    eprintln!(
        "{:>16}: {:>9} ms, 전체 {}개",
        "작성자만",
        t.elapsed().as_secs_f64() * 1000.0,
        p.total
    );
    // 새 글 하나가 들어온 뒤의 검색(바뀐 행만 맞춤).
    let e = fx.msg(vec![], n + 1, b"m");
    s.insert_events_with_views(&[persist(&e)], &[view(&e, "회의 새 글")])
        .unwrap();
    time("새 글 뒤", "회의", 0);
    let e = fx.msg(vec![], n + 2, b"m");
    s.insert_events_with_views(&[persist(&e)], &[view(&e, "회의 새 글 둘")])
        .unwrap();
    time("새 글 하나 더 뒤", "회의", 0);
    s.mark_message_deleted(&fx.space, &e.id(), Timestamp(1))
        .unwrap();
    time("지운 뒤", "회의", 0);
}
