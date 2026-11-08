use std::collections::{BTreeMap, HashMap};

use proptest::prelude::*;
use yangpa_core::body::{
    CustomEmoji, EventBody, InviteInfo, InviteProof, KeyShareBody, MessageBody, ReactionEmoji,
    Secret32, SharedKey,
};
use yangpa_core::checkpoint::OwnershipCert;
use yangpa_core::device::{DeviceEntry, DeviceList, Profile, SignedDeviceList};
use yangpa_core::event::{
    EVENT_VERSION, EventHeader, EventKind, KeyRef, MAX_STATE_ANCHOR, Scope, StreamId,
};
use yangpa_core::ids::{
    BlobId, ChannelId, ChannelRootId, DeviceId, EmojiId, EventId, KeyGroupId, LinkToken, RoleId,
    SpaceId, StateKeyId, UserId,
};
use yangpa_core::keyupdate::{KeyUpdateKind, KeyUpdatePayload, RCt};
use yangpa_core::links::{invite_verify_key, token_hash};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, ChannelTransport, DEFAULT_VOLATILE_CAP, EditRules, ForumTag,
    HistoryVisibility, MAX_CREW_SERVER_LEN, MAX_CREW_SERVERS, Member, OverwriteTarget,
    PermissionOverwrite, Permissions, Role, RoleDisplay, SecurityLevel, SpaceKind, SpaceSettings,
    SpaceState,
};
use yangpa_core::sign::SigningKeypair;
use yangpa_core::time::Timestamp;

use super::*;

// ---- 고정 장치 ----

struct User {
    identity: SigningKeypair,
    device: SigningKeypair,
}

impl User {
    fn new(seed: u8) -> Self {
        User {
            identity: SigningKeypair::from_seed(&[seed; 32]),
            device: SigningKeypair::from_seed(&[seed ^ 0x80; 32]),
        }
    }

    fn id(&self) -> UserId {
        UserId(self.identity.public())
    }

    fn dev(&self) -> DeviceId {
        DeviceId(self.device.public())
    }

    fn entry(kp: &SigningKeypair) -> DeviceEntry {
        DeviceEntry {
            device: DeviceId(kp.public()),
            box_key: yangpa_core::device::BoxPublicKey::filled(3),
            sign_keys: yangpa_core::device::DeviceSignKeys::of(kp),
            name: "기기".into(),
            added_at: Timestamp(1),
            revoked_at: None,
        }
    }

    /// 이 사용자의 첫 기기가 공동 서명한 목록.
    fn list_with(&self, version: u64, devices: Vec<DeviceEntry>) -> SignedDeviceList {
        self.list_by(version, devices, &self.device)
    }

    /// `by` 기기가 공동 서명한 목록.
    fn list_by(
        &self,
        version: u64,
        devices: Vec<DeviceEntry>,
        by: &SigningKeypair,
    ) -> SignedDeviceList {
        DeviceList {
            user: self.id(),
            version,
            devices,
            otp: None,
        }
        .sign_with(&self.identity, by)
        .unwrap()
    }

    fn list(&self) -> SignedDeviceList {
        self.list_with(1, vec![Self::entry(&self.device)])
    }

    fn member(&self) -> Member {
        Member {
            user: self.id(),
            roles: vec![],
            nickname: None,
            automation: false,
        }
    }

    fn join_body(&self, invite: Option<LinkToken>) -> EventBody {
        self.join_body_v(invite, 1)
    }

    fn join_body_v(&self, invite: Option<LinkToken>, version: u64) -> EventBody {
        let devices = self.list_with(version, vec![Self::entry(&self.device)]);
        // 가입자가 토큰으로 만든 증명(토큰 원문은 싣지 않음).
        let invite =
            invite.map(|t| InviteProof::create(&t, &SPACE, &devices, Timestamp(version)).unwrap());
        EventBody::MemberJoin {
            member: self.member(),
            devices,
            profile: None,
            invite,
        }
    }
}

fn settings(name: &str) -> SpaceSettings {
    SpaceSettings {
        name: name.into(),
        description: String::new(),
        icon: None,
        icon_key: None,
        allow_external_emoji: false,
        strict_rotation: false,
    }
}

fn create_body(
    kind: SpaceKind,
    owner: &User,
    members: &[&User],
    everyone: Permissions,
) -> EventBody {
    EventBody::SpaceCreate {
        kind,
        settings: settings("공간"),
        owner: owner.id(),
        committer: owner.dev(),
        everyone,
        members: members.iter().map(|u| u.member()).collect(),
        devices: members.iter().map(|u| u.list()).collect(),
    }
}

fn role(n: u8, rank: u32, permissions: Permissions) -> Role {
    Role {
        id: RoleId([n; 16]),
        name: format!("역할{n}"),
        color: 0,
        rank,
        permissions,
        automation: false,
    }
}

/// `from`이 첫 기기로 `to`에게 넘기는 소유권 이전 인증서(체크포인트 전).
fn cert(from: &User, to: UserId) -> OwnershipCert {
    cert_at(from, to, None)
}

/// [`cert`]에 지금 바탕 체크포인트를 넣은 것.
fn cert_at(from: &User, to: UserId, base: Option<CheckpointId>) -> OwnershipCert {
    OwnershipCert::create(SPACE, base, to, Timestamp(1), &from.identity, &from.device)
}

/// `from`이 첫 기기로 쓰는 소유권 이전(체크포인트 전).
fn transfer(from: &User, to: UserId) -> EventBody {
    transfer_at(from, to, None)
}

/// [`transfer`]에 지금 바탕 체크포인트를 넣은 것.
fn transfer_at(from: &User, to: UserId, base: Option<CheckpointId>) -> EventBody {
    EventBody::OwnershipTransfer {
        new_owner: to,
        cert: cert_at(from, to, base),
    }
}

fn ch(n: u8) -> ChannelId {
    ChannelId([n; 16])
}

const SPACE: SpaceId = SpaceId([7; 16]);

/// 이벤트를 만들고 논리 시계를 자동으로 매깁니다.
struct World {
    space: SpaceId,
    n: u64,
    lamports: HashMap<EventId, u64>,
}

impl World {
    fn new() -> Self {
        World {
            space: SPACE,
            n: 0,
            lamports: HashMap::new(),
        }
    }

    fn lamport_after(&self, parents: &[EventId]) -> u64 {
        parents
            .iter()
            .filter_map(|p| self.lamports.get(p))
            .max()
            .map_or(1, |l| l + 1)
    }

    fn ev(&mut self, author: &User, parents: &[EventId], body: EventBody) -> DecodedEvent {
        let l = self.lamport_after(parents);
        self.ev_at(author.id(), author.dev(), parents, l, body)
    }

    fn ev_at(
        &mut self,
        user: UserId,
        device: DeviceId,
        parents: &[EventId],
        lamport: u64,
        body: EventBody,
    ) -> DecodedEvent {
        self.n += 1;
        let mut ps = parents.to_vec();
        ps.sort();
        ps.dedup();
        let kind = body.kind();
        // 상태 레인과 키 갱신(키 합의 레인)은 모두 상태 키로 암호화합니다(키 갱신은 갱신 전 키).
        let key = KeyRef::State(StateKeyId([1; 16]));
        let header = EventHeader {
            v: EVENT_VERSION,
            space: self.space,
            channel: None,
            kind,
            author: device,
            user,
            parents: ps,
            lamport,
            created_at: Timestamp(1_000 + lamport),
            key,
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        };
        let mut seed = self.n.to_le_bytes().to_vec();
        seed.extend_from_slice(self.space.as_bytes());
        let id = EventId::hash_of(&seed);
        self.lamports.insert(id, lamport);
        DecodedEvent { id, header, body }
    }
}

/// 선형 기록(머리 하나)에 이벤트를 이어 붙이는 도우미.
struct Chain {
    w: World,
    events: Vec<DecodedEvent>,
    head: Option<EventId>,
}

impl Chain {
    fn new() -> Self {
        Chain {
            w: World::new(),
            events: Vec::new(),
            head: None,
        }
    }

    fn push(&mut self, author: &User, body: EventBody) -> EventId {
        let parents: Vec<EventId> = self.head.into_iter().collect();
        let e = self.w.ev(author, &parents, body);
        let id = e.id;
        self.events.push(e);
        self.head = Some(id);
        id
    }

    /// 머리를 옮기지 않고 주어진 부모에 이벤트를 붙입니다(동시 이벤트).
    fn fork(&mut self, author: &User, parents: &[EventId], body: EventBody) -> EventId {
        let e = self.w.ev(author, parents, body);
        let id = e.id;
        self.events.push(e);
        id
    }

    fn push_event(&mut self, e: DecodedEvent) -> EventId {
        let id = e.id;
        self.events.push(e);
        self.head = Some(id);
        id
    }

    fn resolve(&self) -> Resolved {
        resolve(&self.events)
    }
}

fn rejection(r: &Resolved, id: EventId) -> Option<AuthError> {
    r.rejected
        .iter()
        .find(|(x, _)| *x == id)
        .map(|(_, e)| e.clone())
}

fn applied(r: &Resolved, id: EventId) -> bool {
    r.applied.contains(&id)
}

#[track_caller]
fn assert_applied(r: &Resolved, id: EventId) {
    assert!(
        applied(r, id),
        "적용돼야 함, 거부 이유: {:?}",
        rejection(r, id)
    );
}

#[track_caller]
fn assert_rejected(r: &Resolved, id: EventId, want: AuthError) {
    assert_eq!(rejection(r, id), Some(want));
}

/// 소유자 o, 멤버 a·b가 있는 크루(@everyone = 기본 권한).
struct Crew {
    c: Chain,
    o: User,
    a: User,
    b: User,
    genesis: EventId,
}

fn crew() -> Crew {
    let (o, a, b) = (User::new(1), User::new(2), User::new(3));
    let mut c = Chain::new();
    let genesis = c.push(
        &o,
        create_body(
            SpaceKind::Crew,
            &o,
            &[&o, &a, &b],
            Permissions::DEFAULT_EVERYONE,
        ),
    );
    Crew {
        c,
        o,
        a,
        b,
        genesis,
    }
}

fn text_channel(id: ChannelId, name: &str) -> EventBody {
    EventBody::ChannelCreate {
        channel: id,
        settings: ChannelSettings::text(name),
    }
}

fn state(r: &Resolved) -> &SpaceState {
    r.state.as_ref().expect("상태가 있어야 함")
}

// ---- 키 갱신 도우미(키 갱신 시험 `keys`와 무작위 기록이 함께 씀) ----

/// 시험용 약속 값. 순번 0은 사슬 시작의 0입니다.
fn key_commit(seq: u64) -> [u8; 32] {
    if seq == 0 {
        [0; 32]
    } else {
        *blake3::hash(&seq.to_le_bytes()).as_bytes()
    }
}

/// 나누지 않은 직접 인코딩 `KeyUpdate` 한 조각. 목록은 형식 규칙대로 정렬해 싣고, 약속 값은 순번에서 나옵니다.
fn key_payload(
    group: KeyGroupId,
    seq: u64,
    prev: Option<EventId>,
    mut kind: KeyUpdateKind,
) -> KeyUpdatePayload {
    match &mut kind {
        KeyUpdateKind::Advance { joined } => joined.sort(),
        KeyUpdateKind::Rotate { removed, joined }
        | KeyUpdateKind::FullRekey {
            removed, joined, ..
        } => {
            removed.sort();
            joined.sort();
        }
        KeyUpdateKind::Refresh | KeyUpdateKind::RRefresh => {}
    }
    KeyUpdatePayload {
        group,
        seq,
        prev,
        r_ct: kind.new_r().then_some(RCt { ct: [0; 48] }),
        kind,
        root_commit: key_commit(seq),
        prev_root_commit: key_commit(seq.saturating_sub(1)),
        crew: None,
        bundle_root: None,
        shards: None,
        depth: 0,
        salt: [0; 24],
        layout: Vec::new(),
        wraps: Vec::new(),
        sealed: Vec::new(),
        part: 0,
        parts: 1,
    }
}

/// 크루 그룹(공간 스트림) 갱신.
fn crew_update(seq: u64, prev: Option<EventId>, kind: KeyUpdateKind) -> KeyUpdatePayload {
    key_payload(KeyGroupId::crew(&SPACE), seq, prev, kind)
}

// ---- 공간 생성 ----

#[test]
fn friend_space_is_equal_and_restricted() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    let g = c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let r = c.resolve();
    let st = state(&r);
    assert_eq!(st.kind, SpaceKind::Space);
    assert_eq!(st.owner, o.id());
    assert_eq!(st.committer, o.dev());
    assert_eq!(st.members.len(), 2);
    // 본문의 @everyone과 무관하게 친구 공간 권한.
    assert_eq!(
        st.roles[&RoleId::EVERYONE].permissions,
        FRIEND_SPACE_EVERYONE
    );
    assert_eq!(r.genesis, Some(g));
    assert_eq!(r.member_since.get(&a.id()), Some(&1));

    let ch_by_a = c.push(&a, text_channel(ch(1), "일반"));
    let update_by_a = c.push(
        &a,
        EventBody::SpaceUpdate {
            settings: settings("우리 방"),
        },
    );
    let role_by_o = c.push(
        &o,
        EventBody::RoleCreate {
            role: role(1, 5, Permissions::NONE),
        },
    );
    let invite_by_o = c.push(
        &o,
        EventBody::InviteCreate {
            invite: InviteInfo {
                token_hash: [1; 32],
                verify_key: [1; 32],
                expires_at: None,
                max_uses: None,
                roles: vec![],
            },
        },
    );
    let kick_by_o = c.push(
        &o,
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    let join = c.push(&o, User::new(9).join_body(None));
    let anon_by_o = c.push(&o, anonymous(ch(1), true));
    let servers_by_o = c.push(&o, servers(&["stun.example.org:3478"], &[]));
    let r = c.resolve();
    assert_applied(&r, ch_by_a);
    assert_applied(&r, update_by_a);
    assert_eq!(state(&r).settings.name, "우리 방");
    for id in [
        role_by_o,
        invite_by_o,
        kick_by_o,
        join,
        anon_by_o,
        servers_by_o,
    ] {
        assert_rejected(&r, id, AuthError::WrongSpaceKind);
    }
}

#[test]
fn space_create_validation() {
    let (o, a, x) = (User::new(1), User::new(2), User::new(3));
    let base = || match create_body(
        SpaceKind::Crew,
        &o,
        &[&o, &a],
        Permissions::DEFAULT_EVERYONE,
    ) {
        EventBody::SpaceCreate {
            kind,
            settings,
            owner,
            committer,
            everyone,
            members,
            devices,
        } => (kind, settings, owner, committer, everyone, members, devices),
        _ => unreachable!(),
    };
    let make = |t: (
        SpaceKind,
        SpaceSettings,
        UserId,
        DeviceId,
        Permissions,
        Vec<Member>,
        Vec<SignedDeviceList>,
    )| {
        EventBody::SpaceCreate {
            kind: t.0,
            settings: t.1,
            owner: t.2,
            committer: t.3,
            everyone: t.4,
            members: t.5,
            devices: t.6,
        }
    };
    let check = |author: &User, body: EventBody, want: AuthError| {
        let mut w = World::new();
        let e = w.ev(author, &[], body);
        assert_eq!(
            resolve(std::slice::from_ref(&e)).rejected,
            vec![(e.id, want)]
        );
    };

    // 작성자 ≠ 소유자.
    check(&a, make(base()), AuthError::NotOwner);
    // 서명이 깨진 기기 목록.
    let mut t = base();
    t.6[1].list.version = 99;
    check(&o, make(t), AuthError::BadDeviceList);
    // 멤버가 아닌 사용자의 기기 목록.
    let mut t = base();
    t.6.push(x.list());
    check(&o, make(t), AuthError::BadDeviceList);
    // 확정 기기가 소유자 기기가 아님.
    let mut t = base();
    t.3 = a.dev();
    check(&o, make(t), AuthError::InvalidCommitter);
    // 1대1인데 세 명.
    let mut t = base();
    t.0 = SpaceKind::Space;
    t.5.push(x.member());
    check(&o, make(t), AuthError::Invalid("처음 멤버 수"));
    // 소유자가 멤버 목록에 없음.
    let mut t = base();
    t.5.remove(0);
    check(&o, make(t), AuthError::Invalid("소유자가 멤버가 아님"));
    // 빈 이름.
    let mut t = base();
    t.1.name = "  ".into();
    check(&o, make(t), AuthError::Invalid("공간 이름"));
    // 같은 기기를 두 사용자가 주장.
    let mut t = base();
    t.6[1] = a.list_with(1, vec![User::entry(&o.device)]);
    check(&o, make(t), AuthError::BadDeviceList);

    // 작성 기기가 소유자 기기 목록에 없음.
    let mut w = World::new();
    let e = w.ev_at(o.id(), a.dev(), &[], 1, make(base()));
    assert_eq!(
        resolve(std::slice::from_ref(&e)).rejected,
        vec![(e.id, AuthError::UnknownDevice)]
    );
}

#[test]
fn create_must_come_first_and_only_once() {
    let mut cr = crew();
    let early =
        cr.c.w
            .ev_at(cr.o.id(), cr.o.dev(), &[], 0, text_channel(ch(1), "먼저"));
    cr.c.events.push(early.clone());
    let second = cr.c.push(
        &cr.o,
        create_body(SpaceKind::Crew, &cr.o, &[&cr.o], Permissions::NONE),
    );
    let r = cr.c.resolve();
    assert_rejected(&r, early.id, AuthError::NoSpace);
    assert_rejected(&r, second, AuthError::AlreadyCreated);
    assert_eq!(r.applied, vec![cr.genesis]);
}

#[test]
fn generic_author_checks() {
    let mut cr = crew();
    let outsider = User::new(50);
    let not_member = cr.c.push(&outsider, text_channel(ch(1), "x"));
    // 멤버 사용자인데 남의 기기로 서명.
    let wrong_dev = {
        let parents: Vec<EventId> = cr.c.head.into_iter().collect();
        let l = cr.c.w.lamport_after(&parents);
        let e =
            cr.c.w
                .ev_at(cr.o.id(), cr.a.dev(), &parents, l, text_channel(ch(1), "x"));
        cr.c.push_event(e)
    };
    // 헤더 종류와 본문이 다름.
    let mismatch = {
        let mut e = cr.c.w.ev(&cr.o, &[cr.genesis], text_channel(ch(2), "x"));
        e.header.kind = EventKind::ChannelUpdate;
        cr.c.push_event(e)
    };
    // 다른 공간.
    let wrong_space = {
        let mut e = cr.c.w.ev(&cr.o, &[cr.genesis], text_channel(ch(3), "x"));
        e.header.space = SpaceId([9; 16]);
        cr.c.push_event(e)
    };
    // 메시지 레인은 해소 대상이 아님.
    let message = {
        let mut e = cr.c.w.ev(
            &cr.o,
            &[cr.genesis],
            EventBody::Message(MessageBody {
                text: "hi".into(),
                ..Default::default()
            }),
        );
        e.header.channel = Some(ch(1));
        cr.c.push_event(e)
    };
    // 헤더 구조 오류.
    let malformed = {
        let mut e = cr.c.w.ev(&cr.o, &[cr.genesis], text_channel(ch(4), "x"));
        e.header.v = 99;
        cr.c.push_event(e)
    };
    let r = cr.c.resolve();
    assert_rejected(&r, not_member, AuthError::NotMember);
    assert_rejected(&r, wrong_dev, AuthError::UnknownDevice);
    assert_rejected(&r, mismatch, AuthError::KindMismatch);
    assert_rejected(&r, wrong_space, AuthError::WrongSpace);
    assert_rejected(&r, message, AuthError::WrongLane);
    assert_rejected(&r, malformed, AuthError::Malformed);
}

// ---- 채널 ----

#[test]
fn channel_lifecycle_and_permissions() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let no_perm = cr.c.push(a, text_channel(ch(1), "일반"));
    let ok = cr.c.push(o, text_channel(ch(1), "일반"));
    let dup = cr.c.push(o, text_channel(ch(1), "또"));
    let cat = cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(2),
            settings: ChannelSettings {
                kind: ChannelKind::Category,
                ..ChannelSettings::text("묶음")
            },
        },
    );
    let child = cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(3),
            settings: ChannelSettings {
                parent: Some(ch(2)),
                ..ChannelSettings::text("자식")
            },
        },
    );
    let bad_parent = cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(4),
            settings: ChannelSettings {
                parent: Some(ch(1)),
                ..ChannelSettings::text("x")
            },
        },
    );
    let unknown_update = cr.c.push(
        o,
        EventBody::ChannelUpdate {
            channel: ch(9),
            settings: ChannelSettings::text("x"),
        },
    );
    let security_change = cr.c.push(
        o,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: ChannelSettings {
                security: SecurityLevel::Volatile,
                ..ChannelSettings::text("일반")
            },
        },
    );
    let rename = cr.c.push(
        o,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: ChannelSettings::text("잡담"),
        },
    );
    let delete_cat = cr.c.push(o, EventBody::ChannelDelete { channel: ch(2) });
    let recreate = cr.c.push(o, text_channel(ch(2), "다시"));
    let delete_unknown = cr.c.push(o, EventBody::ChannelDelete { channel: ch(9) });
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    for id in [ok, cat, child, rename, delete_cat] {
        assert_applied(&r, id);
    }
    assert_rejected(&r, dup, AuthError::ChannelExists);
    assert_rejected(&r, bad_parent, AuthError::Invalid("상위 채널"));
    assert_rejected(&r, unknown_update, AuthError::UnknownChannel);
    assert_rejected(
        &r,
        security_change,
        AuthError::Invalid("채널 종류·보안 등급은 바꿀 수 없음"),
    );
    assert_rejected(&r, recreate, AuthError::ChannelExists);
    assert_rejected(&r, delete_unknown, AuthError::UnknownChannel);
    let st = state(&r);
    assert_eq!(st.channels[&ch(1)].name, "잡담");
    // 지운 카테고리의 자식은 최상위로.
    assert_eq!(st.channels[&ch(3)].parent, None);
    assert!(!st.channels.contains_key(&ch(2)));
}

#[test]
fn channel_overwrites_cannot_escalate() {
    let mut cr = crew();
    let mods = role(1, 10, Permissions::MANAGE_CHANNELS);
    cr.c.push(&cr.o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        &cr.o,
        EventBody::MemberUpdate {
            user: cr.a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let mut grant = ChannelSettings::text("내 방");
    grant.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Member(cr.a.id()),
        allow: Permissions::MANAGE_MESSAGES,
        deny: Permissions::NONE,
    }];
    let escalate = cr.c.push(
        &cr.a,
        EventBody::ChannelCreate {
            channel: ch(1),
            settings: grant,
        },
    );
    let mut mute = ChannelSettings::text("공지");
    mute.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Role(RoleId::EVERYONE),
        allow: Permissions::NONE,
        deny: Permissions::SEND_MESSAGES,
    }];
    let ok = cr.c.push(
        &cr.a,
        EventBody::ChannelCreate {
            channel: ch(2),
            settings: mute,
        },
    );
    // 소유자가 만든 비공개 채널은 운영자도 볼 수 없어 고칠 수 없음.
    let mut secret = ChannelSettings::text("비밀");
    secret.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Role(RoleId::EVERYONE),
        allow: Permissions::NONE,
        deny: Permissions::VIEW_CHANNEL,
    }];
    cr.c.push(
        &cr.o,
        EventBody::ChannelCreate {
            channel: ch(3),
            settings: secret.clone(),
        },
    );
    let blind_update = cr.c.push(
        &cr.a,
        EventBody::ChannelUpdate {
            channel: ch(3),
            settings: ChannelSettings::text("열기"),
        },
    );
    // 모르는 역할을 가리키는 덮어쓰기.
    let mut ghost = ChannelSettings::text("유령");
    ghost.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Role(RoleId([99; 16])),
        allow: Permissions::NONE,
        deny: Permissions::NONE,
    }];
    let unknown_role = cr.c.push(
        &cr.o,
        EventBody::ChannelCreate {
            channel: ch(4),
            settings: ghost,
        },
    );
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        escalate,
        AuthError::MissingPermission(Permissions::MANAGE_MESSAGES),
    );
    assert_applied(&r, ok);
    assert_rejected(
        &r,
        blind_update,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    assert_rejected(&r, unknown_role, AuthError::UnknownRole);
}

// ---- 채널 전송 방식(P2P 전용) ----

fn transport(channel: ChannelId, transport: ChannelTransport) -> EventBody {
    EventBody::ChannelTransportSet { channel, transport }
}

fn kind_channel(id: ChannelId, kind: ChannelKind) -> EventBody {
    EventBody::ChannelCreate {
        channel: id,
        settings: ChannelSettings {
            kind,
            ..ChannelSettings::text("채널")
        },
    }
}

fn anonymous(channel: ChannelId, anonymous: bool) -> EventBody {
    EventBody::ChannelAnonymousSet { channel, anonymous }
}

#[test]
fn channel_anonymous_rules() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    cr.c.push(o, text_channel(ch(1), "일반"));
    cr.c.push(o, kind_channel(ch(2), ChannelKind::Voice));
    cr.c.push(o, kind_channel(ch(3), ChannelKind::Forum));
    cr.c.push(o, kind_channel(ch(4), ChannelKind::Category));
    // a는 이 채널에서만 채널 관리 권한이 있습니다(채널 덮어쓰기).
    let mut mine = ChannelSettings::text("a의 방");
    mine.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Member(a.id()),
        allow: Permissions::MANAGE_CHANNELS,
        deny: Permissions::NONE,
    }];
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(5),
            settings: mine,
        },
    );

    let no_perm = cr.c.push(a, anonymous(ch(1), true));
    let by_channel_perm = cr.c.push(a, anonymous(ch(5), true));
    let unknown = cr.c.push(o, anonymous(ch(9), true));
    let already_off = cr.c.push(o, anonymous(ch(1), false));
    let text = cr.c.push(o, anonymous(ch(1), true));
    let again = cr.c.push(o, anonymous(ch(1), true));
    let voice = cr.c.push(o, anonymous(ch(2), true));
    let forum = cr.c.push(o, anonymous(ch(3), true));
    let category = cr.c.push(o, anonymous(ch(4), true));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    for id in [by_channel_perm, text, voice, forum] {
        assert_applied(&r, id);
    }
    assert_rejected(&r, unknown, AuthError::UnknownChannel);
    for id in [already_off, again] {
        assert_rejected(&r, id, AuthError::Invalid("이미 그 익명 설정"));
    }
    assert_rejected(
        &r,
        category,
        AuthError::Invalid("카테고리는 익명으로 만들 수 없음"),
    );
    let st = state(&r);
    assert_eq!(
        st.anonymous_channels.iter().copied().collect::<Vec<_>>(),
        vec![ch(1), ch(2), ch(3), ch(5)]
    );

    // 끄면 빠지고, 채널을 지워도 빠집니다.
    let off = cr.c.push(o, anonymous(ch(1), false));
    cr.c.push(o, EventBody::ChannelDelete { channel: ch(2) });
    let deleted = cr.c.push(o, anonymous(ch(2), false));
    let r = cr.c.resolve();
    assert_applied(&r, off);
    assert_rejected(&r, deleted, AuthError::UnknownChannel);
    let st = state(&r);
    assert!(!st.is_anonymous(&ch(1)) && !st.is_anonymous(&ch(2)));
    assert!(st.is_anonymous(&ch(3)) && st.is_anonymous(&ch(5)));
}

fn servers(stun: &[&str], turn: &[&str]) -> EventBody {
    EventBody::CrewServersSet {
        stun: stun.iter().map(|s| s.to_string()).collect(),
        turn: turn.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn crew_servers_rules() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let admins = role(7, 1, Permissions::MANAGE_SPACE);
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: admins.clone(),
        },
    );
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![admins.id],
            nickname: None,
        },
    );
    let no_perm = cr.c.push(a, servers(&["stun.example.org:3478"], &[]));
    let by_role = cr.c.push(
        b,
        servers(
            &["stun.example.org:3478"],
            &["turn:u:p@turn.example.org:3478"],
        ),
    );
    let same = cr.c.push(
        o,
        servers(
            &["stun.example.org:3478"],
            &["turn:u:p@turn.example.org:3478"],
        ),
    );
    let many: Vec<String> = (0..=MAX_CREW_SERVERS)
        .map(|i| format!("s{i}.example.org:3478"))
        .collect();
    let too_many = cr.c.push(
        o,
        EventBody::CrewServersSet {
            stun: many,
            turn: Vec::new(),
        },
    );
    let long = cr.c.push(
        o,
        EventBody::CrewServersSet {
            stun: Vec::new(),
            turn: vec!["x".repeat(MAX_CREW_SERVER_LEN + 1)],
        },
    );
    let empty_line = cr.c.push(o, servers(&[""], &[]));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE),
    );
    assert_applied(&r, by_role);
    assert_rejected(&r, same, AuthError::Invalid("이미 그 서버 목록"));
    for id in [too_many, long, empty_line] {
        assert_rejected(&r, id, AuthError::Invalid("크루 서버 목록이 너무 큼"));
    }
    let st = state(&r);
    assert_eq!(st.crew_servers.stun, vec!["stun.example.org:3478"]);
    assert_eq!(st.crew_servers.turn, vec!["turn:u:p@turn.example.org:3478"]);

    // 비우면 없어집니다.
    let clear = cr.c.push(o, servers(&[], &[]));
    let r = cr.c.resolve();
    assert_applied(&r, clear);
    assert!(state(&r).crew_servers.is_empty());
}

fn featured(channel: ChannelId, post: Option<u8>) -> EventBody {
    EventBody::ForumFeaturedSet {
        channel,
        post: post.map(|n| EventId([n; 32])),
    }
}

#[test]
fn forum_featured_rules() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    cr.c.push(o, text_channel(ch(1), "일반"));
    cr.c.push(o, kind_channel(ch(3), ChannelKind::Forum));
    // a는 이 포럼에서만 채널 관리 권한이 있습니다(채널 덮어쓰기).
    let mut mine = ChannelSettings::text("a의 포럼");
    mine.kind = ChannelKind::Forum;
    mine.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Member(a.id()),
        allow: Permissions::MANAGE_CHANNELS,
        deny: Permissions::NONE,
    }];
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(5),
            settings: mine,
        },
    );

    let no_perm = cr.c.push(a, featured(ch(3), Some(1)));
    let by_channel_perm = cr.c.push(a, featured(ch(5), Some(2)));
    let text = cr.c.push(o, featured(ch(1), Some(1)));
    let unknown = cr.c.push(o, featured(ch(9), Some(1)));
    let none_yet = cr.c.push(o, featured(ch(3), None));
    let set = cr.c.push(o, featured(ch(3), Some(1)));
    let again = cr.c.push(o, featured(ch(3), Some(1)));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    assert_applied(&r, by_channel_perm);
    assert_applied(&r, set);
    assert_rejected(&r, text, AuthError::Invalid("포럼 채널이 아님"));
    assert_rejected(&r, unknown, AuthError::UnknownChannel);
    for id in [none_yet, again] {
        assert_rejected(&r, id, AuthError::Invalid("이미 그 대문"));
    }
    let st = state(&r);
    assert_eq!(st.forum_featured.get(&ch(3)), Some(&EventId([1; 32])));
    assert_eq!(st.forum_featured.get(&ch(5)), Some(&EventId([2; 32])));

    // 다른 글로 바꾸고, 빼고, 채널을 지우면 없어집니다.
    let other = cr.c.push(o, featured(ch(3), Some(4)));
    let clear = cr.c.push(o, featured(ch(5), None));
    let r = cr.c.resolve();
    assert_applied(&r, other);
    assert_applied(&r, clear);
    let st = state(&r);
    assert_eq!(st.forum_featured.get(&ch(3)), Some(&EventId([4; 32])));
    assert!(!st.forum_featured.contains_key(&ch(5)));
    cr.c.push(o, EventBody::ChannelDelete { channel: ch(3) });
    let r = cr.c.resolve();
    assert!(state(&r).forum_featured.is_empty());
}

fn attach_limit(channel: ChannelId, max_bytes: Option<u64>) -> EventBody {
    EventBody::ChannelAttachLimitSet { channel, max_bytes }
}

#[test]
fn channel_attach_limit_rules() {
    const MIB: u64 = 1024 * 1024;
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    cr.c.push(o, text_channel(ch(1), "일반"));
    cr.c.push(o, kind_channel(ch(2), ChannelKind::Category));
    cr.c.push(o, kind_channel(ch(3), ChannelKind::Forum));

    let no_perm = cr.c.push(a, attach_limit(ch(1), Some(8 * MIB)));
    let category = cr.c.push(o, attach_limit(ch(2), Some(8 * MIB)));
    let unknown = cr.c.push(o, attach_limit(ch(9), Some(8 * MIB)));
    let tiny = cr.c.push(o, attach_limit(ch(1), Some(MIB - 1)));
    let unlimited_yet = cr.c.push(o, attach_limit(ch(1), None));
    let set = cr.c.push(o, attach_limit(ch(1), Some(8 * MIB)));
    let forum = cr.c.push(o, attach_limit(ch(3), Some(MIB)));
    let again = cr.c.push(o, attach_limit(ch(1), Some(8 * MIB)));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    assert_rejected(
        &r,
        category,
        AuthError::Invalid("카테고리에는 첨부 한도가 없음"),
    );
    assert_rejected(&r, unknown, AuthError::UnknownChannel);
    assert_rejected(&r, tiny, AuthError::Invalid("첨부 한도가 너무 작음"));
    for id in [unlimited_yet, again] {
        assert_rejected(&r, id, AuthError::Invalid("이미 그 첨부 한도"));
    }
    assert_applied(&r, set);
    assert_applied(&r, forum);
    let st = state(&r);
    assert_eq!(st.attach_limits.get(&ch(1)), Some(&(8 * MIB)));
    assert_eq!(st.attach_limits.get(&ch(3)), Some(&MIB));

    // 0은 첨부 금지로 받습니다.
    let zero = cr.c.push(o, attach_limit(ch(1), Some(0)));
    let r = cr.c.resolve();
    assert_applied(&r, zero);
    assert_eq!(state(&r).attach_limits.get(&ch(1)), Some(&0));

    // 무제한으로 되돌리고, 채널을 지우면 없어집니다.
    let clear = cr.c.push(o, attach_limit(ch(1), None));
    cr.c.push(o, EventBody::ChannelDelete { channel: ch(3) });
    let r = cr.c.resolve();
    assert_applied(&r, clear);
    assert!(state(&r).attach_limits.is_empty());
}

fn edit_rules(channel: ChannelId, no_edit: bool, no_delete: bool) -> EventBody {
    EventBody::ChannelEditRulesSet {
        channel,
        no_edit,
        no_delete,
    }
}

#[test]
fn channel_edit_rules_set_rules() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    cr.c.push(o, text_channel(ch(1), "일반"));
    cr.c.push(o, kind_channel(ch(2), ChannelKind::Category));
    cr.c.push(o, kind_channel(ch(3), ChannelKind::Forum));

    let no_perm = cr.c.push(a, edit_rules(ch(1), true, false));
    let category = cr.c.push(o, edit_rules(ch(2), true, true));
    let unknown = cr.c.push(o, edit_rules(ch(9), true, true));
    let off_yet = cr.c.push(o, edit_rules(ch(1), false, false));
    let set = cr.c.push(o, edit_rules(ch(1), true, false));
    let forum = cr.c.push(o, edit_rules(ch(3), false, true));
    let again = cr.c.push(o, edit_rules(ch(1), true, false));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    assert_rejected(
        &r,
        category,
        AuthError::Invalid("카테고리에는 수정·삭제 금지가 없음"),
    );
    assert_rejected(&r, unknown, AuthError::UnknownChannel);
    for id in [off_yet, again] {
        assert_rejected(&r, id, AuthError::Invalid("이미 그 수정·삭제 금지"));
    }
    assert_applied(&r, set);
    assert_applied(&r, forum);
    let st = state(&r);
    assert!(st.edit_rules(&ch(1)).no_edit && !st.edit_rules(&ch(1)).no_delete);
    assert!(!st.edit_rules(&ch(3)).no_edit && st.edit_rules(&ch(3)).no_delete);

    // 둘 다 끄면 표에서 빠지고, 채널을 지우면 없어집니다.
    let off = cr.c.push(o, edit_rules(ch(1), false, false));
    cr.c.push(o, EventBody::ChannelDelete { channel: ch(3) });
    let r = cr.c.resolve();
    assert_applied(&r, off);
    assert!(state(&r).edit_rules.is_empty());
}

fn volatile_cap(channel: ChannelId, cap: Option<u32>) -> EventBody {
    EventBody::ChannelVolatileCapSet { channel, cap }
}

#[test]
fn channel_volatile_cap_set_rules() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    cr.c.push(o, text_channel(ch(1), "일반"));
    let mut vol = ChannelSettings::text("휘발");
    vol.security = SecurityLevel::Volatile;
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(2),
            settings: vol,
        },
    );

    let no_perm = cr.c.push(a, volatile_cap(ch(2), Some(50)));
    let normal = cr.c.push(o, volatile_cap(ch(1), Some(50)));
    let low = cr.c.push(o, volatile_cap(ch(2), Some(1)));
    let high = cr.c.push(o, volatile_cap(ch(2), Some(2001)));
    let unset = cr.c.push(o, volatile_cap(ch(2), None));
    let set = cr.c.push(o, volatile_cap(ch(2), Some(50)));
    let again = cr.c.push(o, volatile_cap(ch(2), Some(50)));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    assert_rejected(&r, normal, AuthError::Invalid("휘발성 채널이 아님"));
    for id in [low, high] {
        assert_rejected(&r, id, AuthError::Invalid("참가 상한 범위 밖"));
    }
    for id in [unset, again] {
        assert_rejected(&r, id, AuthError::Invalid("이미 그 참가 상한"));
    }
    assert_applied(&r, set);
    assert_eq!(state(&r).volatile_cap(&ch(2)), 50);
    assert_eq!(state(&r).volatile_cap(&ch(1)), DEFAULT_VOLATILE_CAP);

    // 채널을 지우면 없어집니다.
    cr.c.push(o, EventBody::ChannelDelete { channel: ch(2) });
    let r = cr.c.resolve();
    assert!(state(&r).volatile_caps.is_empty());
}

#[test]
fn channel_transport_rules() {
    use ChannelTransport::{P2p, Tor};
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    cr.c.push(o, text_channel(ch(1), "일반"));
    cr.c.push(o, kind_channel(ch(2), ChannelKind::Voice));
    cr.c.push(o, kind_channel(ch(3), ChannelKind::Forum));
    cr.c.push(o, kind_channel(ch(4), ChannelKind::Category));
    // a는 이 채널에서만 채널 관리 권한이 있습니다(채널 덮어쓰기).
    let mut mine = ChannelSettings::text("a의 방");
    mine.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Member(a.id()),
        allow: Permissions::MANAGE_CHANNELS,
        deny: Permissions::NONE,
    }];
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: ch(5),
            settings: mine,
        },
    );

    let no_perm = cr.c.push(a, transport(ch(1), P2p));
    let by_channel_perm = cr.c.push(a, transport(ch(5), P2p));
    let unknown = cr.c.push(o, transport(ch(9), P2p));
    let already_tor = cr.c.push(o, transport(ch(1), Tor));
    let text = cr.c.push(o, transport(ch(1), P2p));
    let again = cr.c.push(o, transport(ch(1), P2p));
    let voice = cr.c.push(o, transport(ch(2), P2p));
    let forum = cr.c.push(o, transport(ch(3), P2p));
    let category = cr.c.push(o, transport(ch(4), P2p));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    for id in [by_channel_perm, text, voice] {
        assert_applied(&r, id);
    }
    assert_rejected(&r, unknown, AuthError::UnknownChannel);
    for id in [already_tor, again] {
        assert_rejected(&r, id, AuthError::Invalid("이미 그 전송 방식"));
    }
    for id in [forum, category] {
        assert_rejected(&r, id, AuthError::Invalid("P2P 전용은 텍스트·통화 채널만"));
    }
    let st = state(&r);
    assert!(st.is_p2p(&ch(1)) && st.is_p2p(&ch(2)) && st.is_p2p(&ch(5)));
    assert!(!st.is_p2p(&ch(3)) && !st.is_p2p(&ch(4)));

    // 양쪽으로 바꿀 수 있고, Tor로 돌리면 표에서 빠집니다. 채널을 지워도 빠집니다.
    let to_tor = cr.c.push(o, transport(ch(1), Tor));
    let tor_again = cr.c.push(o, transport(ch(1), Tor));
    let back = cr.c.push(o, transport(ch(1), P2p));
    cr.c.push(o, EventBody::ChannelDelete { channel: ch(2) });
    let deleted = cr.c.push(o, transport(ch(2), Tor));
    let r = cr.c.resolve();
    assert_applied(&r, to_tor);
    assert_rejected(&r, tor_again, AuthError::Invalid("이미 그 전송 방식"));
    assert_applied(&r, back);
    assert_rejected(&r, deleted, AuthError::UnknownChannel);
    let st = state(&r);
    assert!(st.is_p2p(&ch(1)));
    assert_eq!(
        st.channel_transport.keys().copied().collect::<Vec<_>>(),
        vec![ch(1), ch(5)]
    );
}

#[test]
fn channel_transport_toggle_keeps_only_p2p_entries() {
    use ChannelTransport::{P2p, Tor};
    let mut cr = crew();
    let o = &cr.o;
    cr.c.push(o, text_channel(ch(1), "일반"));
    let ids = [
        cr.c.push(o, transport(ch(1), P2p)),
        cr.c.push(o, transport(ch(1), Tor)),
    ];
    let r = cr.c.resolve();
    for id in ids {
        assert_applied(&r, id);
    }
    let st = state(&r);
    assert!(!st.is_p2p(&ch(1)));
    assert!(st.channel_transport.is_empty(), "Tor는 표에 두지 않음");
}

#[test]
fn channel_transport_not_in_self_space_but_in_friend_space() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::SelfSpace, &o, &[&o], Permissions::NONE),
    );
    c.push(&o, text_channel(ch(1), "메모"));
    let own = c.push(&o, transport(ch(1), ChannelTransport::P2p));
    assert_rejected(&c.resolve(), own, AuthError::WrongSpaceKind);

    // 1대1 공간은 두 친구 모두 채널 관리 권한이 있습니다.
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    c.push(&o, text_channel(ch(1), "일반"));
    let by_friend = c.push(&a, transport(ch(1), ChannelTransport::P2p));
    let r = c.resolve();
    assert_applied(&r, by_friend);
    assert!(state(&r).is_p2p(&ch(1)));
}

// ---- 역할 ----

fn display(visible_to: Option<Vec<RoleId>>, see_admins: bool) -> EventBody {
    EventBody::RoleDisplaySet {
        role: RoleId::EVERYONE,
        display: RoleDisplay {
            visible_to,
            see_admins,
        },
    }
}

fn display_of(role: RoleId, visible_to: Option<Vec<RoleId>>, see_admins: bool) -> EventBody {
    let EventBody::RoleDisplaySet { display, .. } = display(visible_to, see_admins) else {
        unreachable!()
    };
    EventBody::RoleDisplaySet { role, display }
}

#[test]
fn role_display_rules() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let mods = role(1, 10, Permissions::MANAGE_ROLES);
    let staff = role(2, 20, Permissions::KICK_MEMBERS);
    let helpers = role(3, 5, Permissions::NONE);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: staff.clone(),
        },
    );
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: helpers.clone(),
        },
    );
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    // 낮은 역할은 역할 관리자가 고치고, 같거나 높은 역할·권한 없는 사람은 못 고침.
    let lower = cr.c.push(
        a,
        display_of(helpers.id, Some(vec![mods.id, mods.id]), true),
    );
    let higher = cr.c.push(a, display_of(staff.id, Some(vec![]), true));
    let no_perm = cr.c.push(b, display_of(helpers.id, None, false));
    let everyone_list = cr.c.push(o, display(Some(vec![mods.id]), true));
    let everyone_admins = cr.c.push(a, display(None, false));
    let unknown = cr.c.push(o, display_of(RoleId([9; 16]), None, false));
    // @everyone을 고르면 '모두'와 같아, 관리자 보기도 끄면(다른 역할의 기본값) 표에서 빠집니다.
    let staff_hidden =
        cr.c.push(o, display_of(staff.id, Some(vec![helpers.id]), true));
    let staff_all = cr.c.push(
        o,
        display_of(staff.id, Some(vec![RoleId::EVERYONE, helpers.id]), false),
    );
    let r = cr.c.resolve();
    assert_applied(&r, lower);
    assert_rejected(&r, higher, AuthError::RankTooLow);
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_ROLES),
    );
    assert_rejected(&r, everyone_list, AuthError::EveryoneProtected);
    assert_applied(&r, everyone_admins);
    assert_rejected(&r, unknown, AuthError::UnknownRole);
    assert_applied(&r, staff_hidden);
    assert_applied(&r, staff_all);
    let st = state(&r);
    assert_eq!(
        st.role_display(&helpers.id),
        RoleDisplay {
            visible_to: Some(vec![mods.id]),
            see_admins: true
        }
    );
    assert!(!st.role_display(&RoleId::EVERYONE).see_admins);
    assert!(!st.role_display.contains_key(&staff.id));

    // 역할을 지우면 그 역할의 설정과 다른 역할의 목록에서 빠집니다.
    cr.c.push(o, EventBody::RoleDelete { role: mods.id });
    let r = cr.c.resolve();
    let st = state(&r);
    assert_eq!(st.role_display(&helpers.id).visible_to, Some(vec![]));
    assert!(!st.role_display.contains_key(&mods.id));
}

/// 관리자 권한 역할은 순위(rank)가 낮아도 모든 일반 역할보다 위입니다. 관리자 역할끼리는 순위를 따릅니다.
#[test]
fn admin_roles_outrank_regular_roles() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let adm = role(1, 1, Permissions::ADMINISTRATOR);
    let reg = role(2, 20, Permissions::NONE);
    let head = role(3, 5, Permissions::ADMINISTRATOR);
    let mods = role(
        7,
        50,
        Permissions::MANAGE_ROLES.union(Permissions::KICK_MEMBERS),
    );
    for r in [&adm, &reg, &head, &mods] {
        cr.c.push(o, EventBody::RoleCreate { role: r.clone() });
    }
    for (user, r) in [(a, &adm), (b, &mods)] {
        cr.c.push(
            o,
            EventBody::MemberUpdate {
                user: user.id(),
                roles: vec![r.id],
                nickname: None,
            },
        );
    }
    let create_above = cr.c.push(
        a,
        EventBody::RoleCreate {
            role: role(4, 30, Permissions::KICK_MEMBERS),
        },
    );
    let edit_regular = cr.c.push(
        a,
        EventBody::RoleUpdate {
            role: Role {
                permissions: Permissions::KICK_MEMBERS,
                ..reg.clone()
            },
        },
    );
    let create_admin_above = cr.c.push(
        a,
        EventBody::RoleCreate {
            role: role(5, 3, Permissions::ADMINISTRATOR),
        },
    );
    let make_admin_above = cr.c.push(
        a,
        EventBody::RoleUpdate {
            role: Role {
                permissions: Permissions::ADMINISTRATOR,
                ..reg.clone()
            },
        },
    );
    let delete_head = cr.c.push(a, EventBody::RoleDelete { role: head.id });
    let manager_deletes_admin = cr.c.push(b, EventBody::RoleDelete { role: adm.id });
    let manager_kicks_admin = cr.c.push(
        b,
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    let admin_kicks_manager = cr.c.push(
        a,
        EventBody::MemberKick {
            user: b.id(),
            reason: String::new(),
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, create_above);
    assert_applied(&r, edit_regular);
    assert_rejected(&r, create_admin_above, AuthError::RankTooLow);
    assert_rejected(&r, make_admin_above, AuthError::RankTooLow);
    assert_rejected(&r, delete_head, AuthError::RankTooLow);
    assert_rejected(&r, manager_deletes_admin, AuthError::RankTooLow);
    assert_rejected(&r, manager_kicks_admin, AuthError::RankTooLow);
    assert_applied(&r, admin_kicks_manager);
}

#[test]
fn role_rules() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let mods = role(
        1,
        10,
        Permissions::MANAGE_ROLES.union(Permissions::KICK_MEMBERS),
    );
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let same_rank = cr.c.push(
        a,
        EventBody::RoleCreate {
            role: role(2, 10, Permissions::KICK_MEMBERS),
        },
    );
    let lower = cr.c.push(
        a,
        EventBody::RoleCreate {
            role: role(3, 5, Permissions::KICK_MEMBERS),
        },
    );
    let escalate = cr.c.push(
        a,
        EventBody::RoleCreate {
            role: role(4, 5, Permissions::MANAGE_SPACE),
        },
    );
    let everyone_ok = cr.c.push(
        a,
        EventBody::RoleUpdate {
            role: Role::everyone(Permissions::DEFAULT_EVERYONE.remove(Permissions::ADD_REACTIONS)),
        },
    );
    let everyone_escalate = cr.c.push(
        a,
        EventBody::RoleUpdate {
            role: Role::everyone(Permissions::DEFAULT_EVERYONE.union(Permissions::MANAGE_SPACE)),
        },
    );
    let mut renamed = Role::everyone(Permissions::DEFAULT_EVERYONE);
    renamed.name = "모두".into();
    let everyone_rename = cr.c.push(o, EventBody::RoleUpdate { role: renamed });
    let mut ranked = Role::everyone(Permissions::DEFAULT_EVERYONE);
    ranked.rank = 1;
    let everyone_rank = cr.c.push(o, EventBody::RoleUpdate { role: ranked });
    let everyone_delete = cr.c.push(
        o,
        EventBody::RoleDelete {
            role: RoleId::EVERYONE,
        },
    );
    let everyone_create = cr.c.push(
        o,
        EventBody::RoleCreate {
            role: Role {
                rank: 3,
                ..Role::everyone(Permissions::NONE)
            },
        },
    );
    let own_role_delete = cr.c.push(a, EventBody::RoleDelete { role: mods.id });
    let raise_own = cr.c.push(
        a,
        EventBody::RoleUpdate {
            role: Role {
                rank: 50,
                ..mods.clone()
            },
        },
    );
    let max_rank = cr.c.push(
        o,
        EventBody::RoleCreate {
            role: role(5, u32::MAX, Permissions::NONE),
        },
    );
    let zero_rank = cr.c.push(
        o,
        EventBody::RoleCreate {
            role: role(6, 0, Permissions::NONE),
        },
    );
    let delete_by_owner = cr.c.push(o, EventBody::RoleDelete { role: mods.id });
    let recreate = cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    let kick_after = cr.c.push(
        a,
        EventBody::MemberKick {
            user: cr.b.id(),
            reason: String::new(),
        },
    );
    let r = cr.c.resolve();
    assert_rejected(&r, same_rank, AuthError::RankTooLow);
    assert_applied(&r, lower);
    assert_rejected(
        &r,
        escalate,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE),
    );
    assert_applied(&r, everyone_ok);
    // 앞서 스스로 뺀 반응 권한도 이제 갖고 있지 않으므로 함께 보고됨.
    assert_rejected(
        &r,
        everyone_escalate,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE.union(Permissions::ADD_REACTIONS)),
    );
    assert_rejected(&r, everyone_rename, AuthError::EveryoneProtected);
    assert_rejected(&r, everyone_rank, AuthError::EveryoneProtected);
    assert_rejected(&r, everyone_delete, AuthError::EveryoneProtected);
    assert_rejected(&r, everyone_create, AuthError::EveryoneProtected);
    assert_rejected(&r, own_role_delete, AuthError::RankTooLow);
    assert_rejected(&r, raise_own, AuthError::RankTooLow);
    assert_rejected(&r, max_rank, AuthError::Invalid("역할 순위"));
    assert_rejected(&r, zero_rank, AuthError::Invalid("역할 순위"));
    assert_applied(&r, delete_by_owner);
    assert_rejected(&r, recreate, AuthError::RoleExists);
    assert_rejected(
        &r,
        kick_after,
        AuthError::MissingPermission(Permissions::KICK_MEMBERS),
    );
    let st = state(&r);
    assert!(st.members[&a.id()].roles.is_empty());
    assert!(
        !st.roles[&RoleId::EVERYONE]
            .permissions
            .contains(Permissions::ADD_REACTIONS)
    );
    assert_eq!(st.roles[&RoleId::EVERYONE].rank, 0);
}

// ---- 초대와 가입 ----

fn invite(
    token: &LinkToken,
    max_uses: Option<u32>,
    expires_at: Option<Timestamp>,
    roles: Vec<RoleId>,
) -> EventBody {
    EventBody::InviteCreate {
        invite: InviteInfo {
            token_hash: token_hash(token),
            verify_key: invite_verify_key(token),
            expires_at,
            max_uses,
            roles,
        },
    }
}

/// 해소기는 가입 본문을 받을 때 초대 증명의 서명자 키 ID만 남깁니다(본문은 버림). 같은 토큰 해시에 다른 확인 키를 단
/// 초대(초대를 만든 쪽이 지어낸 것)로는 들어오지 못하고, 그 초대를 폐기하고 맞는 키로 다시 만들면 같은 증명으로
/// 들어옵니다. 한 번에 해소한 것과 하나씩(다시 해소 포함) 넣은 것이 같아야 합니다.
#[test]
fn invite_proof_is_checked_against_the_current_invite_key() {
    let mut cr = crew();
    let (o, b) = (&cr.o, &cr.b);
    let carol = User::new(4);
    let t = LinkToken([7; 16]);
    let wrong_key = cr.c.push(
        o,
        EventBody::InviteCreate {
            invite: InviteInfo {
                token_hash: token_hash(&t),
                verify_key: invite_verify_key(&LinkToken([8; 16])),
                expires_at: None,
                max_uses: None,
                roles: vec![],
            },
        },
    );
    let join1 = cr.c.push(b, carol.join_body(Some(t)));
    cr.c.push(
        o,
        EventBody::InviteRevoke {
            token_hash: token_hash(&t),
        },
    );
    cr.c.push(o, invite(&t, None, None, vec![]));
    let join2 = cr.c.push(b, carol.join_body(Some(t)));
    let r = cr.c.resolve();
    assert_applied(&r, wrong_key);
    assert_rejected(&r, join1, AuthError::InvalidInvite);
    assert_applied(&r, join2);
    assert!(r.state.as_ref().unwrap().is_member(&carol.id()));

    // 뒤의 것부터 거꾸로 하나씩 넣으면 매번 다시 해소합니다(남긴 사실로 같은 판정이 나야 함).
    let mut res = Resolver::new();
    for e in cr.c.events.iter().rev() {
        res.insert_events(vec![e.clone()]);
    }
    assert_eq!(res.resolved(), r);
}

#[test]
fn invites_and_joins() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let (carol, dave, erin) = (User::new(4), User::new(5), User::new(6));
    let mods = role(
        1,
        10,
        Permissions::MANAGE_ROLES.union(Permissions::CREATE_INVITE),
    );
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    let t1 = LinkToken([1; 16]);
    let t2 = LinkToken([2; 16]);
    let t3 = LinkToken([3; 16]);
    let no_perm_invite = cr.c.push(b, invite(&t1, None, None, vec![]));
    let inv = cr.c.push(
        o,
        invite(
            &t1,
            Some(1),
            None,
            vec![mods.id, RoleId::EVERYONE, RoleId([77; 16])],
        ),
    );
    let dup_invite = cr.c.push(o, invite(&t1, None, None, vec![]));
    let zero_uses = cr.c.push(o, invite(&t3, Some(0), None, vec![]));
    let expired = cr.c.push(o, invite(&t2, None, Some(Timestamp(5)), vec![]));
    // 초대 생성 권한이 없는 멤버도 초대로 가입시킬 수 있음.
    let join_c = cr.c.push(b, carol.join_body(Some(t1)));
    let used_up = cr.c.push(b, dave.join_body(Some(t1)));
    let join_expired = cr.c.push(b, dave.join_body(Some(t2)));
    let wrong_token = cr.c.push(b, dave.join_body(Some(LinkToken([9; 16]))));
    let direct_no_perm = cr.c.push(a, dave.join_body(None));
    let already = cr.c.push(o, carol.join_body(None));
    let bad_list = cr.c.push(
        o,
        EventBody::MemberJoin {
            member: dave.member(),
            devices: erin.list(),
            profile: None,
            invite: None,
        },
    );
    let bad_profile = cr.c.push(
        o,
        EventBody::MemberJoin {
            member: dave.member(),
            devices: dave.list(),
            profile: Some(
                Profile {
                    version: 1,
                    name: "에린".into(),
                    ..Default::default()
                }
                .sign(&erin.identity)
                .unwrap(),
            ),
            invite: None,
        },
    );
    let direct = cr.c.push(
        o,
        EventBody::MemberJoin {
            member: dave.member(),
            devices: dave.list(),
            profile: Some(
                Profile {
                    version: 1,
                    name: "데이브".into(),
                    ..Default::default()
                }
                .sign(&dave.identity)
                .unwrap(),
            ),
            invite: None,
        },
    );
    let carol_acts = cr.c.push(&carol, text_channel(ch(1), "x"));
    // 운영자(순위 10)는 자기 순위 이상의 역할을 주는 초대를 만들 수 없음.
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let high_invite =
        cr.c.push(a, invite(&LinkToken([4; 16]), None, None, vec![mods.id]));
    let a_invite =
        cr.c.push(a, invite(&LinkToken([5; 16]), None, None, vec![]));
    let revoke_other = cr.c.push(
        b,
        EventBody::InviteRevoke {
            token_hash: token_hash(&LinkToken([5; 16])),
        },
    );
    let revoke_unknown = cr.c.push(
        o,
        EventBody::InviteRevoke {
            token_hash: [0; 32],
        },
    );
    // 만든 사람은 권한을 잃어도 자기 초대를 폐기할 수 있음.
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![],
            nickname: None,
        },
    );
    let revoke_own = cr.c.push(
        a,
        EventBody::InviteRevoke {
            token_hash: token_hash(&LinkToken([5; 16])),
        },
    );

    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm_invite,
        AuthError::MissingPermission(Permissions::CREATE_INVITE),
    );
    assert_applied(&r, inv);
    assert_rejected(&r, dup_invite, AuthError::InviteExists);
    assert_rejected(&r, zero_uses, AuthError::Invalid("초대 사용 횟수"));
    assert_applied(&r, expired);
    assert_applied(&r, join_c);
    assert_rejected(&r, used_up, AuthError::InvalidInvite);
    assert_rejected(&r, join_expired, AuthError::InvalidInvite);
    assert_rejected(&r, wrong_token, AuthError::InvalidInvite);
    assert_rejected(
        &r,
        direct_no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE),
    );
    assert_rejected(&r, already, AuthError::AlreadyMember);
    assert_rejected(&r, bad_list, AuthError::BadDeviceList);
    assert_rejected(&r, bad_profile, AuthError::BadProfile);
    assert_applied(&r, direct);
    assert_rejected(
        &r,
        carol_acts,
        AuthError::MissingPermission(Permissions::MANAGE_CHANNELS),
    );
    assert_rejected(&r, high_invite, AuthError::RankTooLow);
    assert_applied(&r, a_invite);
    assert_rejected(
        &r,
        revoke_other,
        AuthError::MissingPermission(Permissions::CREATE_INVITE),
    );
    assert_rejected(&r, revoke_unknown, AuthError::UnknownInvite);
    assert_applied(&r, revoke_own);

    let st = state(&r);
    // 초대의 역할만(모르는 역할·@everyone 제외) 받음.
    assert_eq!(st.members[&carol.id()].roles, vec![mods.id]);
    assert_eq!(
        st.invites
            .iter()
            .find(|i| i.info.token_hash == token_hash(&t1))
            .unwrap()
            .uses,
        1
    );
    assert_eq!(
        st.invites
            .iter()
            .find(|i| i.info.token_hash == token_hash(&t1))
            .unwrap()
            .info
            .roles,
        vec![mods.id]
    );
    assert!(st.device_belongs(&carol.id(), &carol.dev()));
    assert!(
        !st.invites
            .iter()
            .any(|i| i.info.token_hash == token_hash(&LinkToken([5; 16])))
    );
    assert_eq!(r.member_since[&carol.id()], cr.c.w.lamports[&join_c]);
}

#[test]
fn invite_proofs_cannot_be_reused_or_moved_to_others() {
    let mut cr = crew();
    let (o, b) = (&cr.o, &cr.b);
    let (carol, dave) = (User::new(4), User::new(5));
    let t = LinkToken([1; 16]);
    cr.c.push(o, invite(&t, None, None, vec![]));
    // 상태에서 본 carol의 증명을 dave의 가입에 옮겨 붙임.
    let carol_join = carol.join_body(Some(t));
    let EventBody::MemberJoin {
        invite: Some(carol_proof),
        ..
    } = &carol_join
    else {
        unreachable!()
    };
    let stolen = cr.c.push(
        b,
        EventBody::MemberJoin {
            member: dave.member(),
            devices: dave.list(),
            profile: None,
            invite: Some(carol_proof.clone()),
        },
    );
    // 토큰 없이 공개 정보(해시)만으로 만든 증명.
    let forged = cr.c.push(
        b,
        EventBody::MemberJoin {
            member: dave.member(),
            devices: dave.list(),
            profile: None,
            invite: Some(InviteProof {
                token_hash: token_hash(&t),
                at: Timestamp(1),
                sig: yangpa_core::sign::Sig::from_bytes(&[0; yangpa_core::sign::Sig::LEN]).unwrap(),
                joiner: None,
            }),
        },
    );
    let join = cr.c.push(b, carol_join.clone());
    cr.c.push(
        o,
        EventBody::MemberKick {
            user: carol.id(),
            reason: String::new(),
        },
    );
    // 쫓겨난 뒤 다른 멤버가 예전 가입 이벤트(같은 증명)를 다시 냄.
    let replay = cr.c.push(b, carol_join);
    // 토큰을 가진 본인이 새로 만든 증명은 받아들임(초대가 아직 유효).
    let mut fresh = carol.join_body(None);
    if let EventBody::MemberJoin {
        invite, devices, ..
    } = &mut fresh
    {
        *invite = Some(InviteProof::create(&t, &SPACE, devices, Timestamp(99)).unwrap());
    }
    let rejoin = cr.c.push(o, fresh);
    let r = cr.c.resolve();
    assert_rejected(&r, stolen, AuthError::InvalidInvite);
    assert_rejected(&r, forged, AuthError::InvalidInvite);
    assert_applied(&r, join);
    assert_rejected(&r, replay, AuthError::InvalidInvite);
    assert_applied(&r, rejoin);
    let st = state(&r);
    assert!(st.is_member(&carol.id()));
    assert!(!st.is_member(&dave.id()));
    assert_eq!(st.invites[0].uses, 2);
}

#[test]
fn same_time_invite_proofs_of_different_joiners_both_apply() {
    // 같은 초대 링크로 여러 사람이 동시에(같은 시각으로) 들어와도 서로 다른 증명입니다.
    let mut cr = crew();
    let (o, b) = (&cr.o, &cr.b);
    let (carol, dave) = (User::new(4), User::new(5));
    let t = LinkToken([2; 16]);
    cr.c.push(o, invite(&t, Some(2), None, vec![]));
    let c = cr.c.push(b, carol.join_body(Some(t)));
    let d = cr.c.push(b, dave.join_body(Some(t)));
    let r = cr.c.resolve();
    assert_applied(&r, c);
    assert_applied(&r, d);
    let st = state(&r);
    assert!(st.is_member(&carol.id()) && st.is_member(&dave.id()));
    assert_eq!(st.invites[0].uses, 2);
}

// ---- 멤버 갱신·추방·탈퇴 ----

#[test]
fn member_updates() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let mods = role(1, 10, Permissions::MANAGE_ROLES);
    let helper = role(2, 5, Permissions::NONE);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: helper.clone(),
        },
    );
    let self_nick = cr.c.push(
        b,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![],
            nickname: Some("비".into()),
        },
    );
    let other_nick = cr.c.push(
        b,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![],
            nickname: Some("에이".into()),
        },
    );
    let self_role = cr.c.push(
        b,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![helper.id],
            nickname: None,
        },
    );
    let promote = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id, mods.id],
            nickname: None,
        },
    );
    let give_helper = cr.c.push(
        a,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![helper.id],
            nickname: None,
        },
    );
    let give_mods = cr.c.push(
        a,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![helper.id, mods.id],
            nickname: None,
        },
    );
    let touch_owner = cr.c.push(
        a,
        EventBody::MemberUpdate {
            user: o.id(),
            roles: vec![helper.id],
            nickname: None,
        },
    );
    let rename_b = cr.c.push(
        a,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![helper.id],
            nickname: Some("도우미".into()),
        },
    );
    let clear_nick = cr.c.push(
        b,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![helper.id],
            nickname: Some(String::new()),
        },
    );
    let unknown = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: User::new(40).id(),
            roles: vec![],
            nickname: None,
        },
    );
    let long_nick = cr.c.push(
        b,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![helper.id],
            nickname: Some("가".repeat(65)),
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, self_nick);
    assert_rejected(
        &r,
        other_nick,
        AuthError::MissingPermission(Permissions::MANAGE_ROLES),
    );
    assert_rejected(
        &r,
        self_role,
        AuthError::MissingPermission(Permissions::MANAGE_ROLES),
    );
    assert_applied(&r, promote);
    assert_applied(&r, give_helper);
    assert_rejected(&r, give_mods, AuthError::RankTooLow);
    assert_rejected(&r, touch_owner, AuthError::RankTooLow);
    assert_applied(&r, rename_b);
    assert_applied(&r, clear_nick);
    assert_rejected(&r, unknown, AuthError::UnknownMember);
    assert_rejected(&r, long_nick, AuthError::Invalid("별명"));
    let st = state(&r);
    assert_eq!(st.members[&a.id()].roles, vec![mods.id]);
    assert_eq!(st.members[&b.id()].roles, vec![helper.id]);
    assert_eq!(st.members[&b.id()].nickname, None);
    assert_eq!(st.rank_of(&a.id()), 10);
}

#[test]
fn kick_and_leave() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let carol = User::new(4);
    let mods = role(1, 10, Permissions::KICK_MEMBERS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(o, carol.join_body(None));
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: carol.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let no_perm = cr.c.push(
        b,
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    let kick_owner = cr.c.push(
        a,
        EventBody::MemberKick {
            user: o.id(),
            reason: String::new(),
        },
    );
    let kick_peer = cr.c.push(
        a,
        EventBody::MemberKick {
            user: carol.id(),
            reason: String::new(),
        },
    );
    let long_reason = cr.c.push(
        a,
        EventBody::MemberKick {
            user: b.id(),
            reason: "x".repeat(600),
        },
    );
    let kick_b = cr.c.push(
        a,
        EventBody::MemberKick {
            user: b.id(),
            reason: "규칙 위반".into(),
        },
    );
    let b_after = cr.c.push(b, EventBody::MemberLeave { user: b.id() });
    let kick_gone = cr.c.push(
        a,
        EventBody::MemberKick {
            user: b.id(),
            reason: String::new(),
        },
    );
    let owner_leave = cr.c.push(o, EventBody::MemberLeave { user: o.id() });
    let leave_other = cr.c.push(a, EventBody::MemberLeave { user: carol.id() });
    let carol_leaves =
        cr.c.push(&carol, EventBody::MemberLeave { user: carol.id() });
    // 떠난 사람을 옛 기기 목록으로 다시 넣으면 거부(버전 되돌리기 방지).
    let newer = carol.list_with(5, vec![User::entry(&carol.device)]);
    cr.c.push(&carol, EventBody::DeviceListUpdate { devices: newer });
    let rejoin_old = cr.c.push(o, carol.join_body(None));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::KICK_MEMBERS),
    );
    assert_rejected(&r, kick_owner, AuthError::OwnerProtected);
    assert_rejected(&r, kick_peer, AuthError::RankTooLow);
    assert_rejected(&r, long_reason, AuthError::Limit("추방 사유"));
    assert_applied(&r, kick_b);
    assert_rejected(&r, b_after, AuthError::NotMember);
    assert_rejected(&r, kick_gone, AuthError::UnknownMember);
    assert_rejected(&r, owner_leave, AuthError::OwnerProtected);
    assert_rejected(&r, leave_other, AuthError::Invalid("본인만 탈퇴할 수 있음"));
    assert_applied(&r, carol_leaves);
    // 떠난 뒤의 기기 목록 갱신은 멤버가 아니라 거부되므로, 마지막 버전은 1.
    assert_applied(&r, rejoin_old);
    let st = state(&r);
    assert!(!st.is_member(&b.id()));
    assert!(!st.devices.contains_key(&b.id()));
    assert!(!r.member_since.contains_key(&b.id()));
    assert!(st.is_member(&carol.id()));
    assert!(st.members[&carol.id()].roles.is_empty());
}

#[test]
fn rejoin_with_stale_device_list_is_rejected() {
    let mut cr = crew();
    let (o, b) = (&cr.o, &cr.b);
    cr.c.push(
        b,
        EventBody::DeviceListUpdate {
            devices: b.list_with(3, vec![User::entry(&b.device)]),
        },
    );
    cr.c.push(b, EventBody::MemberLeave { user: b.id() });
    let stale = cr.c.push(o, b.join_body(None));
    let fresh = cr.c.push(
        o,
        EventBody::MemberJoin {
            member: b.member(),
            devices: b.list_with(3, vec![User::entry(&b.device)]),
            profile: None,
            invite: None,
        },
    );
    let r = cr.c.resolve();
    assert_rejected(&r, stale, AuthError::StaleDeviceList);
    assert_applied(&r, fresh);
}

// ---- 밴 ----

fn ban(user: &User, reason: &str, delete_since: Option<Timestamp>) -> EventBody {
    EventBody::MemberBan {
        user: user.id(),
        reason: reason.into(),
        delete_since,
    }
}

#[test]
fn bans_remove_and_block_rejoin() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let (carol, dave) = (User::new(4), User::new(5));
    let mods = role(1, 10, Permissions::BAN_MEMBERS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let t1 = LinkToken([1; 16]);
    let t2 = LinkToken([2; 16]);
    cr.c.push(o, invite(&t1, None, None, vec![]));
    let no_perm = cr.c.push(b, ban(a, "", None));
    let ban_owner = cr.c.push(a, ban(o, "", None));
    let ban_self = cr.c.push(a, ban(a, "", None));
    let long_reason = cr.c.push(a, ban(b, &"x".repeat(600), None));
    // 작성 시각(1000 + 논리 시계)보다 뒤거나 7일보다 앞선 구간은 거부.
    let future = cr.c.push(a, ban(b, "", Some(Timestamp(u64::MAX))));
    let too_old = {
        let parents: Vec<EventId> = cr.c.head.into_iter().collect();
        let mut e = cr.c.w.ev(a, &parents, ban(b, "", Some(Timestamp(0))));
        e.header.created_at = Timestamp(limits::MAX_BAN_DELETE_WINDOW_MS + 10_000);
        cr.c.push_event(e)
    };
    let ban_b = cr.c.push(a, ban(b, "도배", Some(Timestamp(1_000))));
    let again = cr.c.push(a, ban(b, "", None));
    let b_after = cr.c.push(b, EventBody::MemberLeave { user: b.id() });
    let rejoin_invite = cr.c.push(o, b.join_body(Some(t1)));
    let rejoin_direct = cr.c.push(o, b.join_body(None));
    // 멤버가 아닌 사람도 미리 밴할 수 있음.
    let ban_carol = cr.c.push(a, ban(&carol, "", None));
    let carol_join = cr.c.push(o, carol.join_body(Some(t1)));
    let unban_unknown = cr.c.push(a, EventBody::MemberUnban { user: dave.id() });
    let unban_no_perm = cr.c.push(o, EventBody::MemberUnban { user: b.id() });
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::BAN_MEMBERS),
    );
    assert_rejected(&r, ban_owner, AuthError::OwnerProtected);
    assert_rejected(&r, ban_self, AuthError::RankTooLow);
    assert_rejected(&r, long_reason, AuthError::Limit("밴 사유"));
    assert_rejected(&r, future, AuthError::Invalid("최근 메시지 삭제 구간"));
    assert_rejected(&r, too_old, AuthError::Invalid("최근 메시지 삭제 구간"));
    assert_applied(&r, ban_b);
    assert_rejected(&r, again, AuthError::Banned);
    assert_rejected(&r, b_after, AuthError::NotMember);
    assert_rejected(&r, rejoin_invite, AuthError::Banned);
    assert_rejected(&r, rejoin_direct, AuthError::Banned);
    assert_applied(&r, ban_carol);
    assert_rejected(&r, carol_join, AuthError::Banned);
    assert_rejected(&r, unban_unknown, AuthError::NotBanned);
    // 소유자는 모든 권한을 가지므로 풀 수 있음.
    assert_applied(&r, unban_no_perm);
    let st = state(&r);
    assert!(!st.is_member(&b.id()));
    assert!(!st.devices.contains_key(&b.id()));
    assert!(!st.bans.contains_key(&b.id()));
    let info = &st.bans[&carol.id()];
    assert_eq!(info.banned_by, a.id());

    // 밴을 풀어도 다시 들어오려면 새 초대(또는 직접 추가)가 있어야 합니다.
    cr.c.push(o, invite(&t2, None, None, vec![]));
    let back = cr.c.push(o, b.join_body(Some(t2)));
    let r = cr.c.resolve();
    assert_applied(&r, back);
    assert!(state(&r).is_member(&b.id()));
}

#[test]
fn ban_records_reason_and_needs_crew() {
    let mut cr = crew();
    let (o, b) = (&cr.o, &cr.b);
    let id = cr.c.push(o, ban(b, "규칙 위반", None));
    let r = cr.c.resolve();
    assert_applied(&r, id);
    let info = &state(&r).bans[&b.id()];
    assert_eq!(info.reason, "규칙 위반");
    assert_eq!(info.banned_by, o.id());
    assert_eq!(
        info.banned_at,
        cr.c.events.last().unwrap().header.created_at
    );

    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let in_friend_space = c.push(&o, ban(&a, "", None));
    let unban_friend = c.push(&o, EventBody::MemberUnban { user: a.id() });
    let r = c.resolve();
    assert_rejected(&r, in_friend_space, AuthError::WrongSpaceKind);
    assert_rejected(&r, unban_friend, AuthError::WrongSpaceKind);
}

#[test]
fn concurrent_ban_beats_join() {
    let mut cr = crew();
    let (o, b) = (&cr.o, &cr.b);
    let dave = User::new(5);
    let t1 = LinkToken([1; 16]);
    let inv = cr.c.push(o, invite(&t1, None, None, vec![]));
    let li = cr.c.w.lamports[&inv];
    // b가 초대로 dave를 넣는 것과 소유자의 밴이 동시. 소유자 순위가 높아 밴이 먼저 적용됩니다.
    let join = {
        let e =
            cr.c.w
                .ev_at(b.id(), b.dev(), &[inv], li + 1, dave.join_body(Some(t1)));
        cr.c.events.push(e.clone());
        e.id
    };
    let banned = {
        let e =
            cr.c.w
                .ev_at(o.id(), o.dev(), &[inv], li + 5, ban(&dave, "", None));
        cr.c.events.push(e.clone());
        e.id
    };
    let r = cr.c.resolve();
    assert_applied(&r, banned);
    assert_rejected(&r, join, AuthError::Banned);
    assert!(!state(&r).is_member(&dave.id()));
}

// ---- 이모지 ----

#[test]
fn emoji_rules() {
    let mut cr = crew();
    let emoji = |name: &str, mime: &str, id: u64| CustomEmoji {
        id: EmojiId(id),
        name: name.into(),
        animated: false,
        blob: BlobId([1; 32]),
        key: Secret32([2; 32]),
        mime: mime.into(),
    };
    let no_perm = cr.c.push(
        &cr.a,
        EventBody::EmojiAdd {
            emoji: emoji("양파", "image/png", 1),
        },
    );
    let ok = cr.c.push(
        &cr.o,
        EventBody::EmojiAdd {
            emoji: emoji("양파", "image/png", 1),
        },
    );
    let dup = cr.c.push(
        &cr.o,
        EventBody::EmojiAdd {
            emoji: emoji("양파2", "image/png", 1),
        },
    );
    let bad_name = cr.c.push(
        &cr.o,
        EventBody::EmojiAdd {
            emoji: emoji("a b", "image/png", 2),
        },
    );
    let bad_mime = cr.c.push(
        &cr.o,
        EventBody::EmojiAdd {
            emoji: emoji("svg", "image/svg+xml", 3),
        },
    );
    let remove = cr.c.push(&cr.o, EventBody::EmojiRemove { id: EmojiId(1) });
    let remove_again = cr.c.push(&cr.o, EventBody::EmojiRemove { id: EmojiId(1) });
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_EMOJI),
    );
    assert_applied(&r, ok);
    assert_rejected(&r, dup, AuthError::EmojiExists);
    assert_rejected(&r, bad_name, AuthError::Invalid("이모지 이름"));
    assert_rejected(&r, bad_mime, AuthError::Invalid("이모지 형식"));
    assert_applied(&r, remove);
    assert_rejected(&r, remove_again, AuthError::UnknownEmoji);
    assert!(state(&r).emojis.is_empty());
}

#[test]
fn emoji_limits_and_uploader() {
    let mut cr = crew();
    let emoji = |id: u64| EventBody::EmojiAdd {
        emoji: CustomEmoji {
            id: EmojiId(id),
            name: format!("e{id}"),
            animated: false,
            blob: BlobId([id as u8; 32]),
            key: Secret32([2; 32]),
            mime: "image/png".into(),
        },
    };
    let limits = |n: u32| EventBody::EmojiLimitsSet {
        limits: yangpa_core::model::EmojiLimits {
            max_count: Some(n),
            max_bytes: Some(1024),
        },
    };
    let no_perm = cr.c.push(&cr.a, limits(1));
    let zero = cr.c.push(&cr.o, limits(0));
    let over = cr.c.push(&cr.o, limits(limits::MAX_EMOJIS as u32 + 1));
    let ok = cr.c.push(&cr.o, limits(1));
    let same = cr.c.push(&cr.o, limits(1));
    let first = cr.c.push(&cr.o, emoji(10));
    let second = cr.c.push(&cr.o, emoji(11));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        no_perm,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE),
    );
    assert_rejected(&r, zero, AuthError::Invalid("이모지 한도"));
    assert_rejected(&r, over, AuthError::Invalid("이모지 한도"));
    assert_applied(&r, ok);
    assert_rejected(&r, same, AuthError::Invalid("이미 그 이모지 한도"));
    assert_applied(&r, first);
    assert_rejected(&r, second, AuthError::Limit("이모지 수"));
    let st = state(&r);
    assert_eq!(st.emoji_limits.max_bytes, Some(1024));
    assert_eq!(st.emoji_by.get(&EmojiId(10)), Some(&cr.o.id()));

    // 지우면 올린 사람 기록도 빠집니다.
    cr.c.push(&cr.o, EventBody::EmojiRemove { id: EmojiId(10) });
    assert!(state(&cr.c.resolve()).emoji_by.is_empty());
}

#[test]
fn crew_disband_freezes_state() {
    let mut cr = crew();
    cr.c.push(&cr.o, text_channel(ch(1), "일반"));
    let not_owner = cr.c.push(&cr.a, EventBody::CrewDisband);
    let ok = cr.c.push(&cr.o, EventBody::CrewDisband);
    let again = cr.c.push(&cr.o, EventBody::CrewDisband);
    let after = cr.c.push(&cr.o, text_channel(ch(2), "해산 뒤"));
    let leave = cr.c.push(&cr.b, EventBody::MemberLeave { user: cr.b.id() });
    let r = cr.c.resolve();
    assert_rejected(&r, not_owner, AuthError::NotOwner);
    assert_applied(&r, ok);
    for id in [again, after, leave] {
        assert_rejected(&r, id, AuthError::Invalid("해산한 크루"));
    }
    // 멤버는 남아(늦게 온 멤버에게 해산을 전함) 있지만 소유자도 권한이 없습니다.
    let st = state(&r);
    assert!(st.disbanded && st.is_member(&cr.b.id()));
    assert_eq!(st.base_permissions(&cr.o.id()), Permissions::NONE);
    assert_eq!(
        st.channel_permissions(&cr.a.id(), &ch(1)),
        Permissions::NONE
    );

    // 정규 인코딩: 해산 표시는 참일 때만 끝에 한 바이트로 붙어(해산하지 않은 크루는 옛 바이트 그대로) 되살아납니다.
    let mut res = Resolver::with_genesis(cr.genesis);
    res.insert_events(cr.c.events.clone());
    let m = res.machine.as_ref().expect("해소기 상태");
    let bytes = m.encode();
    assert!(
        Machine::decode(&bytes, Limits::default())
            .unwrap()
            .st
            .disbanded
    );
    let mut before = m.clone();
    before.st.disbanded = false;
    let old = before.encode();
    assert_eq!([old.as_slice(), &[1]].concat(), bytes);
    assert!(
        !Machine::decode(&old, Limits::default())
            .unwrap()
            .st
            .disbanded
    );

    // 1:1 공간은 해산하지 않습니다(친구 삭제로 나감).
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let dm = c.push(&o, EventBody::CrewDisband);
    assert_rejected(&c.resolve(), dm, AuthError::WrongSpaceKind);
}

// ---- 크루 전환·소유권·확정 기기 ----

#[test]
fn crew_conversion() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    c.push(&o, text_channel(ch(1), "일반"));
    let accept_none = c.push(&o, EventBody::CrewConvertAccept);
    let req = c.push(
        &a,
        EventBody::CrewConvertRequest {
            name: "게임 모임".into(),
        },
    );
    let self_accept = c.push(&a, EventBody::CrewConvertAccept);
    let counter = c.push(
        &o,
        EventBody::CrewConvertRequest {
            name: String::new(),
        },
    );
    let accept = c.push(&o, EventBody::CrewConvertAccept);
    let again = c.push(
        &a,
        EventBody::CrewConvertRequest {
            name: String::new(),
        },
    );
    // 전환 뒤 관리자(o)는 역할을 만들 수 있음.
    let admin_role = c.push(
        &o,
        EventBody::RoleCreate {
            role: role(1, 50, Permissions::KICK_MEMBERS),
        },
    );
    let r = c.resolve();
    assert_rejected(&r, accept_none, AuthError::NoPendingConversion);
    assert_applied(&r, req);
    assert_rejected(
        &r,
        self_accept,
        AuthError::Invalid("요청한 사람은 수락할 수 없음"),
    );
    assert_rejected(&r, counter, AuthError::ConversionPending);
    assert_applied(&r, accept);
    assert_rejected(&r, again, AuthError::WrongSpaceKind);
    assert_applied(&r, admin_role);
    let st = state(&r);
    let admin = admin_role_id(&SPACE);
    assert_eq!(st.kind, SpaceKind::Crew);
    assert_eq!(st.owner, a.id());
    assert_eq!(st.settings.name, "게임 모임");
    assert_eq!(st.members[&o.id()].roles, vec![admin]);
    assert_eq!(st.roles[&admin].rank, ADMIN_ROLE_RANK);
    assert_eq!(st.roles[&admin].name, ADMIN_ROLE_NAME);
    assert_eq!(st.roles[&admin].permissions, Permissions::ADMINISTRATOR);
    assert_eq!(
        st.roles[&RoleId::EVERYONE].permissions,
        Permissions::DEFAULT_EVERYONE
    );
    assert_eq!(st.pending_crew_conversion, None);
    // 확정 기기는 여전히 o의 기기(관리자라 자격 유지).
    assert_eq!(st.committer, o.dev());
    // 채널 그대로.
    assert!(st.channels.contains_key(&ch(1)));
    // 관리자 역할 ID는 공간 ID에서 결정론적으로 파생.
    assert_eq!(admin_role_id(&SPACE), admin);
    assert_ne!(admin_role_id(&SpaceId([8; 16])), admin);
}

#[test]
fn conversion_makes_old_channels_private_until_both_agree() {
    let (o, a, carol) = (User::new(1), User::new(2), User::new(4));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    // 1:1일 때 한쪽이 몰래 과거 기록 공개로 바꿔 둔 채널.
    let mut public = ChannelSettings::text("일반");
    public.history = HistoryVisibility::Public;
    c.push(
        &o,
        EventBody::ChannelCreate {
            channel: ch(1),
            settings: public.clone(),
        },
    );
    c.push(
        &o,
        EventBody::CrewConvertRequest {
            name: "크루".into(),
        },
    );
    c.push(&a, EventBody::CrewConvertAccept);
    let after = c.resolve();
    assert_eq!(
        state(&after).channels[&ch(1)].history,
        HistoryVisibility::Private,
        "전환하면 기존 채널은 과거 기록 비공개"
    );
    // 전환 뒤 새로 들어온 관리자는 기존 채널을 공개로 못 바꿈.
    c.push(&o, carol.join_body(None));
    c.push(
        &o,
        EventBody::MemberUpdate {
            user: carol.id(),
            roles: vec![admin_role_id(&SPACE)],
            nickname: None,
        },
    );
    let by_carol = c.push(
        &carol,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: public.clone(),
        },
    );
    // o(소유자)가 먼저 바꾸면 동의로만 기록되고, 이름 같은 다른 항목은 바뀜.
    let mut renamed = public.clone();
    renamed.name = "옛 대화".into();
    let by_o = c.push(
        &o,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: renamed.clone(),
        },
    );
    let r = c.resolve();
    assert_rejected(
        &r,
        by_carol,
        AuthError::Invalid("전환 전 채널의 과거 기록 공개는 원래 두 사람만 바꿀 수 있음"),
    );
    assert_applied(&r, by_o);
    let st = state(&r);
    assert_eq!(st.channels[&ch(1)].history, HistoryVisibility::Private);
    assert_eq!(st.channels[&ch(1)].name, "옛 대화");
    // a도 바꾸면 그때 공개.
    c.push(
        &a,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: renamed.clone(),
        },
    );
    let r = c.resolve();
    assert_eq!(
        state(&r).channels[&ch(1)].history,
        HistoryVisibility::Public
    );
    // 전환 뒤 만든 채널은 한 사람이 바꿀 수 있음.
    c.push(&o, text_channel(ch(2), "새 채널"));
    let mut new_public = ChannelSettings::text("새 채널");
    new_public.history = HistoryVisibility::Public;
    let one_side = c.push(
        &o,
        EventBody::ChannelUpdate {
            channel: ch(2),
            settings: new_public,
        },
    );
    let r = c.resolve();
    assert_applied(&r, one_side);
    assert_eq!(
        state(&r).channels[&ch(2)].history,
        HistoryVisibility::Public
    );
}

#[test]
fn legacy_history_consent_can_be_withdrawn() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    c.push(&o, text_channel(ch(1), "일반"));
    c.push(
        &o,
        EventBody::CrewConvertRequest {
            name: "크루".into(),
        },
    );
    c.push(&a, EventBody::CrewConvertAccept);
    let mut public = ChannelSettings::text("일반");
    public.history = HistoryVisibility::Public;
    c.push(
        &o,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: public.clone(),
        },
    );
    // o가 마음을 바꿈(공개가 아닌 변경) → 동의 철회.
    c.push(
        &o,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: ChannelSettings::text("일반"),
        },
    );
    c.push(
        &a,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: public,
        },
    );
    let r = c.resolve();
    assert_eq!(
        state(&r).channels[&ch(1)].history,
        HistoryVisibility::Private,
        "철회한 동의로 공개됨"
    );
}

#[test]
fn ownership_and_committer() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let not_owner = cr.c.push(a, transfer(a, a.id()));
    let to_outsider = cr.c.push(o, transfer(o, User::new(40).id()));
    let to_self = cr.c.push(o, transfer(o, o.id()));
    // 다른 사람에게 준 인증서, 다른 기기 이름의 인증서, 깨진 서명은 거부.
    let wrong_to = cr.c.push(
        o,
        EventBody::OwnershipTransfer {
            new_owner: a.id(),
            cert: cert(o, b.id()),
        },
    );
    let mut forged = cert(o, a.id());
    forged.device = a.dev();
    let wrong_device = cr.c.push(
        o,
        EventBody::OwnershipTransfer {
            new_owner: a.id(),
            cert: forged,
        },
    );
    let mut broken = cert(o, a.id());
    broken.at = Timestamp(99);
    let bad_sig = cr.c.push(
        o,
        EventBody::OwnershipTransfer {
            new_owner: a.id(),
            cert: broken,
        },
    );
    let committer_plain = cr.c.push(o, EventBody::CommitterChange { device: b.dev() });
    let committer_unknown = cr.c.push(
        o,
        EventBody::CommitterChange {
            device: User::new(40).dev(),
        },
    );
    let moved = cr.c.push(o, transfer(o, a.id()));
    let old_owner = cr.c.push(o, EventBody::CommitterChange { device: o.dev() });
    let admin = role(1, 10, Permissions::ADMINISTRATOR);
    cr.c.push(
        a,
        EventBody::RoleCreate {
            role: admin.clone(),
        },
    );
    cr.c.push(
        a,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![admin.id],
            nickname: None,
        },
    );
    let to_admin = cr.c.push(a, EventBody::CommitterChange { device: b.dev() });
    let r = cr.c.resolve();
    assert_rejected(&r, not_owner, AuthError::NotOwner);
    assert_rejected(&r, to_outsider, AuthError::UnknownMember);
    assert_rejected(&r, to_self, AuthError::Invalid("이미 소유자"));
    assert_rejected(&r, wrong_to, AuthError::Invalid("소유권 인증서"));
    assert_rejected(&r, wrong_device, AuthError::Invalid("소유권 인증서"));
    assert_rejected(&r, bad_sig, AuthError::Invalid("소유권 인증서 서명"));
    assert_rejected(&r, committer_plain, AuthError::InvalidCommitter);
    assert_rejected(&r, committer_unknown, AuthError::InvalidCommitter);
    assert_applied(&r, moved);
    assert_rejected(&r, old_owner, AuthError::NotOwner);
    assert_applied(&r, to_admin);
    let st = state(&r);
    assert_eq!(st.owner, a.id());
    assert_eq!(st.committer, b.dev());

    // 소유권을 넘기면 옛 소유자 기기는 자격을 잃어 새 소유자의 기기로 자동으로 넘어갑니다.
    let mut cr = crew();
    let body = transfer(&cr.o, cr.a.id());
    cr.c.push(&cr.o, body);
    let r = cr.c.resolve();
    assert_eq!(state(&r).committer, cr.a.dev());
}

#[test]
fn device_list_updates_and_committer_repair() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let d2 = SigningKeypair::from_seed(&[0x55; 32]);
    let v2 = o.list_with(2, vec![User::entry(&o.device), User::entry(&d2)]);
    let add = cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: v2.clone(),
        },
    );
    let same_version = cr.c.push(o, EventBody::DeviceListUpdate { devices: v2 });
    let forged = {
        let mut l = o.list_with(3, vec![User::entry(&o.device)]);
        l.list.devices.push(User::entry(&a.device));
        cr.c.push(o, EventBody::DeviceListUpdate { devices: l })
    };
    let for_other = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_with(9, vec![User::entry(&o.device)]),
        },
    );
    let steal = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: a.list_with(2, vec![User::entry(&a.device), User::entry(&d2)]),
        },
    );
    // 새 기기로 작성.
    let by_d2 = {
        let parents: Vec<EventId> = cr.c.head.into_iter().collect();
        let l = cr.c.w.lamport_after(&parents);
        let e = cr.c.w.ev_at(
            o.id(),
            DeviceId(d2.public()),
            &parents,
            l,
            text_channel(ch(1), "새 기기"),
        );
        cr.c.push_event(e)
    };
    // 새 기기에서 원래 기기(확정 기기)를 폐기.
    let revoke = {
        let mut old = User::entry(&o.device);
        old.revoked_at = Some(Timestamp(10));
        let l3 = o.list_with(3, vec![old, User::entry(&d2)]);
        let parents: Vec<EventId> = cr.c.head.into_iter().collect();
        let l = cr.c.w.lamport_after(&parents);
        let e = cr.c.w.ev_at(
            o.id(),
            DeviceId(d2.public()),
            &parents,
            l,
            EventBody::DeviceListUpdate { devices: l3 },
        );
        cr.c.push_event(e)
    };
    let by_revoked = cr.c.push(o, text_channel(ch(2), "폐기된 기기"));
    let r = cr.c.resolve();
    assert_applied(&r, add);
    assert_rejected(&r, same_version, AuthError::StaleDeviceList);
    assert_rejected(&r, forged, AuthError::BadDeviceList);
    assert_rejected(&r, for_other, AuthError::BadDeviceList);
    assert_rejected(&r, steal, AuthError::BadDeviceList);
    assert_applied(&r, by_d2);
    assert_applied(&r, revoke);
    assert_rejected(&r, by_revoked, AuthError::UnknownDevice);
    let st = state(&r);
    assert_eq!(st.devices[&o.id()].version, 3);
    // 확정 기기가 폐기되어 소유자의 남은 활성 기기로 넘어감.
    assert_eq!(st.committer, DeviceId(d2.public()));
}

/// 신원 키로 제대로 서명했어도, 기기 ID와 다른 기기의 서명 키를 실은 항목이 있으면 받지 않고, 상태의 기록은 진짜 키의
/// 약속을 그대로 둡니다.
#[test]
fn device_lists_with_foreign_sign_keys_are_rejected() {
    let mut cr = crew();
    let o = &cr.o;
    let d2 = SigningKeypair::from_seed(&[0x57; 32]);
    let d3 = SigningKeypair::from_seed(&[0x58; 32]);
    let mut wrong = User::entry(&d2);
    wrong.sign_keys = yangpa_core::device::DeviceSignKeys::of(&d3);
    let bad = cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: o.list_with(2, vec![User::entry(&o.device), wrong]),
        },
    );
    let r = cr.c.resolve();
    assert_rejected(&r, bad, AuthError::BadDeviceList);
    let rec = &state(&r).devices[&o.id()];
    assert_eq!(rec.version, 1);
    assert_eq!(
        rec.get(&o.dev()).map(|d| d.key_commit),
        Some(User::entry(&o.device).key_commit())
    );
}

#[test]
fn members_relay_device_lists_that_only_grow() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    // 복구한 새 기기는 아직 상태에 없어 직접 쓸 수 없으므로 다른 멤버(a)가 o의 서명 목록을 전합니다.
    let d2 = SigningKeypair::from_seed(&[0x66; 32]);
    let merged = o.list_with(2, vec![User::entry(&o.device), User::entry(&d2)]);
    let relayed =
        cr.c.push(a, EventBody::DeviceListUpdate { devices: merged });
    // 기기를 빼는 목록(복구 기기만 담은 처음 목록)은 판이 높아도 거부.
    let shrinking = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_with(9, vec![User::entry(&d2)]),
        },
    );
    // 폐기를 되돌리는 목록도 거부.
    let mut revoked = User::entry(&o.device);
    revoked.revoked_at = Some(Timestamp(5));
    let revoke = cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: o.list_with(3, vec![revoked, User::entry(&d2)]),
        },
    );
    let unrevoke = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_with(4, vec![User::entry(&o.device), User::entry(&d2)]),
        },
    );
    // 멤버가 아닌 사람의 목록은 전할 수 없음.
    let outsider = User::new(9);
    let not_member = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: outsider.list_with(2, vec![User::entry(&outsider.device)]),
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, relayed);
    assert_rejected(&r, shrinking, AuthError::BadDeviceList);
    assert_applied(&r, revoke);
    assert_rejected(&r, unrevoke, AuthError::BadDeviceList);
    assert_rejected(&r, not_member, AuthError::UnknownMember);
    let st = state(&r);
    assert_eq!(st.devices[&o.id()].version, 3);
    assert!(!st.device_belongs(&o.id(), &o.dev()));
    assert!(st.device_belongs(&o.id(), &DeviceId(d2.public())));
}

#[test]
fn revoked_device_cannot_sign_new_device_lists() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let d2 = SigningKeypair::from_seed(&[0x57; 32]);
    let d3 = SigningKeypair::from_seed(&[0x58; 32]);
    // 원래 기기가 새 기기를 더하고(공동 서명 o), 새 기기가 원래 기기를 폐기(공동 서명 d2).
    let add = cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: o.list_with(2, vec![User::entry(&o.device), User::entry(&d2)]),
        },
    );
    let mut old = User::entry(&o.device);
    old.revoked_at = Some(Timestamp(10));
    let revoke = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_by(3, vec![old.clone(), User::entry(&d2)], &d2),
        },
    );
    // 폐기된 기기가 신원 키로 기기를 더한 판(자기 공동 서명): 다른 멤버가 전해도 거부.
    let by_revoked = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_by(
                4,
                vec![old.clone(), User::entry(&d2), User::entry(&d3)],
                &o.device,
            ),
        },
    );
    // 공동 서명 없는 판도 거부.
    let unsigned = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: DeviceList {
                user: o.id(),
                version: 5,
                devices: vec![old.clone(), User::entry(&d2), User::entry(&d3)],
                otp: None,
            }
            .sign(&o.identity)
            .unwrap(),
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, add);
    assert_applied(&r, revoke);
    assert_rejected(&r, by_revoked, AuthError::BadDeviceList);
    assert_rejected(&r, unsigned, AuthError::BadDeviceList);
    let st = state(&r);
    assert_eq!(st.devices[&o.id()].version, 3);
    assert!(!st.device_belongs(&o.id(), &DeviceId(d3.public())));
}

#[test]
fn recovery_lists_are_relayed_by_other_members_only() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    // 살아 있는 기기 없이 복구한 새 기기 x가 아는 목록에 자기만 더해 공동 서명한 판.
    let x = SigningKeypair::from_seed(&[0x59; 32]);
    let rec = o.list_by(2, vec![User::entry(&o.device), User::entry(&x)], &x);
    // 대상 사용자 자신의 기기가 내면 거부(자기 기기라면 승인 판을 공동 서명했을 것).
    let by_self = cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: rec.clone(),
        },
    );
    // 복구하며 아는 기기를 폐기하는 판은 누가 전해도 거부.
    let mut gone = User::entry(&o.device);
    gone.revoked_at = Some(Timestamp(3));
    let hostile = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_by(2, vec![gone, User::entry(&x)], &x),
        },
    );
    let relayed = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: rec.clone(),
        },
    );
    let r = cr.c.resolve();
    assert_rejected(&r, by_self, AuthError::BadDeviceList);
    assert_rejected(&r, hostile, AuthError::BadDeviceList);
    // 멤버 한 명(a)이 전한 것만으로는 아직 넣지 않습니다(o 말고 멤버가 둘: a·b).
    assert_applied(&r, relayed);
    let st = state(&r);
    assert!(!st.device_belongs(&o.id(), &DeviceId(x.public())));

    // 그 사이 x는 o의 기기를 폐기하지 못합니다(공동 서명 기기가 아직 활성이 아님). 복구 문구를 훔친 사람과 짠 멤버 a가
    // 같은 판을 다시 전해도 한 명으로 셉니다.
    let mut gone = User::entry(&o.device);
    gone.revoked_at = Some(Timestamp(4));
    let takeover = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: o.list_by(3, vec![gone, User::entry(&x)], &x),
        },
    );
    let again = cr.c.push(
        a,
        EventBody::DeviceListUpdate {
            devices: rec.clone(),
        },
    );
    let r = cr.c.resolve();
    assert_rejected(&r, takeover, AuthError::BadDeviceList);
    assert_applied(&r, again);
    assert!(!state(&r).device_belongs(&o.id(), &DeviceId(x.public())));

    // 다른 멤버 b도 (자기 유예가 지나) 전하면 넣습니다.
    let second =
        cr.c.push(&cr.b, EventBody::DeviceListUpdate { devices: rec });
    let r = cr.c.resolve();
    assert_applied(&r, second);
    let st = state(&r);
    assert!(st.device_belongs(&o.id(), &DeviceId(x.public())));
    assert!(st.device_belongs(&o.id(), &o.dev()));
}

#[test]
fn recovery_relay_needs_only_the_other_member_in_two_member_spaces() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let x = SigningKeypair::from_seed(&[0x5A; 32]);
    let rec = o.list_by(2, vec![User::entry(&o.device), User::entry(&x)], &x);
    let relayed = c.push(&a, EventBody::DeviceListUpdate { devices: rec });
    let r = c.resolve();
    assert_applied(&r, relayed);
    assert!(state(&r).device_belongs(&o.id(), &DeviceId(x.public())));
}

/// OTP를 켠 사용자의 복구 판은 멤버가 전해도 OTP 키 서명이 있어야 상태에 들어가고, OTP를 끄는 복구 판은 거부합니다(복구
/// 문구를 훔친 사람이 멤버와 짜도 기기를 넣지 못함).
#[test]
fn recovery_lists_need_the_otp_signature_once_otp_is_on() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let otp = yangpa_core::device::otp_signing_key(b"resolver otp secret!");
    let list = |version, devices, key, by: &SigningKeypair| {
        DeviceList {
            user: o.id(),
            version,
            devices,
            otp: key,
        }
        .sign_with(&o.identity, by)
        .unwrap()
    };
    let on = c.push(
        &o,
        EventBody::DeviceListUpdate {
            devices: list(
                2,
                vec![User::entry(&o.device)],
                Some(otp.public()),
                &o.device,
            ),
        },
    );
    let x = SigningKeypair::from_seed(&[0x5B; 32]);
    let devices = vec![User::entry(&o.device), User::entry(&x)];
    let bare = c.push(
        &a,
        EventBody::DeviceListUpdate {
            devices: list(3, devices.clone(), Some(otp.public()), &x),
        },
    );
    let off = c.push(
        &a,
        EventBody::DeviceListUpdate {
            devices: list(3, devices.clone(), None, &x),
        },
    );
    let r = c.resolve();
    assert_applied(&r, on);
    assert_rejected(&r, bare, AuthError::BadDeviceList);
    assert_rejected(&r, off, AuthError::BadDeviceList);
    assert!(!state(&r).device_belongs(&o.id(), &DeviceId(x.public())));

    let signed = c.push(
        &a,
        EventBody::DeviceListUpdate {
            devices: list(3, devices, Some(otp.public()), &x)
                .with_otp_sig(&otp)
                .unwrap(),
        },
    );
    let r = c.resolve();
    assert_applied(&r, signed);
    let st = state(&r);
    assert!(st.device_belongs(&o.id(), &DeviceId(x.public())));
    assert_eq!(st.devices[&o.id()].otp, Some(otp.public()));
}

#[test]
fn committer_falls_back_to_an_admin_when_owner_has_no_device() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let admin = role(1, 10, Permissions::ADMINISTRATOR);
    cr.c.push(
        o,
        EventBody::RoleCreate {
            role: admin.clone(),
        },
    );
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![admin.id],
            nickname: None,
        },
    );
    // 소유자의 유일한 기기(확정 기기)를 폐기.
    let mut old = User::entry(&o.device);
    old.revoked_at = Some(Timestamp(10));
    let revoke = cr.c.push(
        o,
        EventBody::DeviceListUpdate {
            devices: o.list_with(2, vec![old]),
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, revoke);
    assert_eq!(state(&r).committer, a.dev());
    // 소유자가 아닌 멤버는 확정 기기를 지정하지 못합니다.
    let by_b =
        cr.c.push(&cr.b, EventBody::CommitterChange { device: cr.b.dev() });
    let r = cr.c.resolve();
    assert_rejected(&r, by_b, AuthError::NotOwner);
    assert_eq!(state(&r).committer, a.dev());
}

#[test]
fn member_profile_sets_own_nickname_without_touching_roles() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let mods = role(1, 10, Permissions::MANAGE_CHANNELS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    let base = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    // a가 별명을 바꾸는 사이 o가 a의 역할을 뺍니다(동시). 별명 이벤트는 역할을 싣지 않으므로 역할 변경을 되돌리지
    // 않고 거부되지도 않습니다.
    let nick = cr.c.fork(
        a,
        &[base],
        EventBody::MemberProfile {
            nickname: Some("양파".into()),
            profile: None,
        },
    );
    let demote = cr.c.fork(
        o,
        &[base],
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![],
            nickname: None,
        },
    );
    // 서명된 내 프로필만 실은 이벤트는 상태를 바꾸지 않고 받아들임. 남의 프로필이나 서명이 틀린 프로필은 거부.
    let mine = Profile {
        version: 5,
        name: "에이".into(),
        ..Default::default()
    }
    .sign(&a.identity)
    .unwrap();
    let with_profile = cr.c.fork(
        a,
        &[nick, demote],
        EventBody::MemberProfile {
            nickname: None,
            profile: Some(mine.clone()),
        },
    );
    let others = cr.c.push(
        o,
        EventBody::MemberProfile {
            nickname: None,
            profile: Some(mine.clone()),
        },
    );
    let mut forged = mine.clone();
    forged.profile.name = "바꿈".into();
    let bad_sig = cr.c.push(
        a,
        EventBody::MemberProfile {
            nickname: None,
            profile: Some(forged),
        },
    );
    let empty = cr.c.push(
        a,
        EventBody::MemberProfile {
            nickname: None,
            profile: None,
        },
    );
    let bad_nick = cr.c.push(
        a,
        EventBody::MemberProfile {
            nickname: Some("\u{7}".into()),
            profile: None,
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, nick);
    assert_applied(&r, demote);
    assert_applied(&r, with_profile);
    assert_rejected(&r, others, AuthError::BadProfile);
    assert_rejected(&r, bad_sig, AuthError::BadProfile);
    assert_rejected(&r, empty, AuthError::Invalid("바꿀 것이 없음"));
    assert!(rejection(&r, bad_nick).is_some());
    let st = state(&r);
    let m = &st.members[&a.id()];
    assert_eq!(m.nickname.as_deref(), Some("양파"));
    assert!(m.roles.is_empty());

    // 1:1 공간에는 별명이 없지만 프로필은 실을 수 있음.
    let (x, y) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &x,
        create_body(SpaceKind::Space, &x, &[&x, &y], Permissions::NONE),
    );
    let nick_in_friend = c.push(
        &y,
        EventBody::MemberProfile {
            nickname: Some("별명".into()),
            profile: None,
        },
    );
    let profile_in_friend = c.push(
        &y,
        EventBody::MemberProfile {
            nickname: None,
            profile: Some(Profile::default().sign(&y.identity).unwrap()),
        },
    );
    let r = c.resolve();
    assert_rejected(&r, nick_in_friend, AuthError::WrongSpaceKind);
    assert_applied(&r, profile_in_friend);
}

#[test]
fn member_badges_carry_only_own_signed_card() {
    use yangpa_core::badge::BadgeCard;
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Crew, &o, &[&o, &a], Permissions::NONE),
    );
    let card = BadgeCard {
        version: 1,
        earned: 1 << 4,
        ..Default::default()
    }
    .sign(&a.identity)
    .unwrap();
    let mine = c.push(&a, EventBody::MemberBadges { card: card.clone() });
    let others = c.push(&o, EventBody::MemberBadges { card: card.clone() });
    let mut forged = card;
    forged.card.earned = u64::MAX;
    let bad_sig = c.push(&a, EventBody::MemberBadges { card: forged });
    let r = c.resolve();
    assert_applied(&r, mine);
    assert_rejected(&r, others, AuthError::BadProfile);
    assert_rejected(&r, bad_sig, AuthError::BadProfile);
}

#[test]
fn friend_space_owner_may_leave() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let leave = c.push(&o, EventBody::MemberLeave { user: o.id() });
    let after = c.push(&o, text_channel(ch(1), "나간 뒤"));
    // 남은 사람(새 소유자)은 혼자 남은 공간에서 나갈 수 없음.
    let last = c.push(&a, EventBody::MemberLeave { user: a.id() });
    let r = c.resolve();
    assert_applied(&r, leave);
    assert_rejected(&r, after, AuthError::NotMember);
    assert_rejected(&r, last, AuthError::OwnerProtected);
    let st = state(&r);
    assert_eq!(st.owner, a.id());
    assert_eq!(st.committer, a.dev());
    assert!(!st.is_member(&o.id()));
}

// ---- 동시 이벤트와 순서 ----

#[test]
fn higher_rank_wins_concurrent_kicks() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let mods = role(1, 10, Permissions::KICK_MEMBERS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    let x = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    // a는 b를, 소유자는 a를 동시에 추방. a의 이벤트가 논리 시계는 더 작지만 순위가 낮아 나중에 적용됩니다.
    let lx = cr.c.w.lamports[&x];
    let a_kicks_b = {
        let e = cr.c.w.ev_at(
            a.id(),
            a.dev(),
            &[x],
            lx + 1,
            EventBody::MemberKick {
                user: b.id(),
                reason: String::new(),
            },
        );
        cr.c.events.push(e.clone());
        e.id
    };
    let o_kicks_a = {
        let e = cr.c.w.ev_at(
            o.id(),
            o.dev(),
            &[x],
            lx + 9,
            EventBody::MemberKick {
                user: a.id(),
                reason: String::new(),
            },
        );
        cr.c.events.push(e.clone());
        e.id
    };
    let r = cr.c.resolve();
    assert_applied(&r, o_kicks_a);
    assert_rejected(&r, a_kicks_b, AuthError::NotMember);
    let pos = |id| r.applied.iter().position(|x| *x == id);
    assert!(pos(o_kicks_a).is_some());
    let st = state(&r);
    assert!(st.is_member(&b.id()));
    assert!(!st.is_member(&a.id()));
}

#[test]
fn equal_rank_mods_cannot_kick_each_other() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let mods = role(1, 10, Permissions::KICK_MEMBERS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let x = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: b.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let ab = cr.c.fork(
        a,
        &[x],
        EventBody::MemberKick {
            user: b.id(),
            reason: String::new(),
        },
    );
    let ba = cr.c.fork(
        b,
        &[x],
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    let r = cr.c.resolve();
    assert_rejected(&r, ab, AuthError::RankTooLow);
    assert_rejected(&r, ba, AuthError::RankTooLow);
}

#[test]
fn demotion_beats_concurrent_action() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let mods = role(1, 10, Permissions::KICK_MEMBERS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    let x = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let kick = cr.c.fork(
        a,
        &[x],
        EventBody::MemberKick {
            user: b.id(),
            reason: String::new(),
        },
    );
    let demote = cr.c.fork(
        o,
        &[x],
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![],
            nickname: None,
        },
    );
    let r = cr.c.resolve();
    assert_applied(&r, demote);
    assert_rejected(
        &r,
        kick,
        AuthError::MissingPermission(Permissions::KICK_MEMBERS),
    );
    assert!(state(&r).is_member(&b.id()));
}

#[test]
fn concurrent_same_rank_ordered_by_lamport_then_id() {
    let mut cr = crew();
    let o = &cr.o;
    let g = cr.genesis;
    // 같은 작성자(같은 순위)의 동시 이벤트: 논리 시계가 작은 쪽이 먼저 채널을 만듦.
    let late = {
        let e =
            cr.c.w
                .ev_at(o.id(), o.dev(), &[g], 5, text_channel(ch(1), "나중"));
        cr.c.events.push(e.clone());
        e.id
    };
    let early = {
        let e =
            cr.c.w
                .ev_at(o.id(), o.dev(), &[g], 3, text_channel(ch(1), "먼저"));
        cr.c.events.push(e.clone());
        e.id
    };
    let r = cr.c.resolve();
    assert_applied(&r, early);
    assert_rejected(&r, late, AuthError::ChannelExists);
    assert_eq!(state(&r).channels[&ch(1)].name, "먼저");
}

#[test]
fn gaps_are_tolerated() {
    let mut cr = crew();
    let missing = EventId([0xEE; 32]);
    let after_gap = {
        let e = cr.c.w.ev_at(
            cr.o.id(),
            cr.o.dev(),
            &[missing],
            10,
            text_channel(ch(1), "빈 구간 뒤"),
        );
        cr.c.push_event(e)
    };
    let r = cr.c.resolve();
    assert_applied(&r, after_gap);
}

#[test]
fn bad_lamport_and_cycles() {
    let mut cr = crew();
    let g = cr.genesis;
    let not_after = {
        let e =
            cr.c.w
                .ev_at(cr.o.id(), cr.o.dev(), &[g], 1, text_channel(ch(1), "x"));
        cr.c.events.push(e.clone());
        e.id
    };
    // 가짜 ID로 순환을 만듦(실제 이벤트에선 불가능).
    let mut e1 = cr.c.w.ev(&cr.o, &[g], text_channel(ch(2), "a"));
    let mut e2 = cr.c.w.ev(&cr.o, &[g], text_channel(ch(3), "b"));
    e1.header.parents = vec![g, e2.id];
    e1.header.parents.sort();
    e2.header.parents = vec![g, e1.id];
    e2.header.parents.sort();
    e1.header.lamport = 50;
    e2.header.lamport = 60;
    let (i1, i2) = (e1.id, e2.id);
    cr.c.events.push(e1);
    cr.c.events.push(e2);
    let self_parent = {
        let mut e = cr.c.w.ev(&cr.o, &[g], text_channel(ch(4), "c"));
        e.header.parents = vec![e.id];
        cr.c.events.push(e.clone());
        e.id
    };
    let r = cr.c.resolve();
    assert_rejected(&r, not_after, AuthError::BadLamport);
    assert_rejected(&r, i1, AuthError::Cycle);
    assert_rejected(&r, i2, AuthError::Cycle);
    assert_rejected(&r, self_parent, AuthError::BadLamport);
    // 순환 거부는 (논리 시계, ID) 순.
    let tail: Vec<EventId> = r.rejected.iter().rev().take(2).map(|(id, _)| *id).collect();
    assert_eq!(tail, vec![i2, i1]);
}

/// 공간 스트림의 `KeyShare`를 빼면 인과가 끊겨 순서가 뒤집힐 수 있어, 순서 연결로 넘깁니다.
#[test]
fn link_events_preserve_causality() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let mods = role(1, 10, Permissions::MANAGE_CHANNELS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let create = cr.c.push(a, text_channel(ch(1), "a가 만듦"));
    // 공간 수준 KeyShare(본문을 풀 수 없을 수도 있음).
    let ks = {
        let mut e = cr.c.w.ev(
            o,
            &[create],
            EventBody::KeyShare(KeyShareBody {
                keys: vec![SharedKey::State {
                    id: StateKeyId([2; 16]),
                    key: Secret32([0; 32]),
                }],
                for_user: None,
            }),
        );
        e.header.key = KeyRef::Sealed;
        e
    };
    let update = cr.c.w.ev(
        o,
        &[ks.id],
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: ChannelSettings::text("o가 고침"),
        },
    );
    let mut events = cr.c.events.clone();
    events.push(update.clone());

    // 연결 없이: o의 고침이 (순위가 높아) a의 생성보다 먼저 적용되어 거부됨.
    let r = resolve(&events);
    assert_rejected(&r, update.id, AuthError::UnknownChannel);

    // 헤더만으로 연결.
    let r = resolve_with(&events, &[LinkEvent::from_header(ks.id, &ks.header)], None);
    assert_applied(&r, update.id);
    assert_eq!(state(&r).channels[&ch(1)].name, "o가 고침");

    // 본문을 푼 KeyShare를 그대로 넘겨도 연결로만 쓰고 적용·거부 목록에 넣지 않음.
    let mut with_ks = events.clone();
    with_ks.push(ks.clone());
    let r = resolve(&with_ks);
    assert_applied(&r, update.id);
    assert!(!applied(&r, ks.id));
    assert!(rejection(&r, ks.id).is_none());
}

#[test]
fn genesis_pin_blocks_forged_create() {
    let (o, a) = (User::new(1), User::new(2));
    let mut w = World::new();
    let real = w.ev_at(
        o.id(),
        o.dev(),
        &[],
        5,
        create_body(
            SpaceKind::Crew,
            &o,
            &[&o, &a],
            Permissions::DEFAULT_EVERYONE,
        ),
    );
    // 멤버 a가 같은 공간 ID로 자기를 소유자로 하는 가짜 생성 이벤트를 더 작은 논리 시계로 만듦.
    let forged = w.ev_at(
        a.id(),
        a.dev(),
        &[],
        1,
        create_body(
            SpaceKind::Crew,
            &a,
            &[&a, &o],
            Permissions::DEFAULT_EVERYONE,
        ),
    );
    let events = vec![real.clone(), forged.clone()];
    // 고정하지 않으면 결정론적으로 먼저 오는 쪽이 이김(그래서 데몬은 고정해야 함).
    let r = resolve(&events);
    assert_eq!(state(&r).owner, a.id());
    let r = resolve_with(&events, &[], Some(real.id));
    assert_eq!(state(&r).owner, o.id());
    assert_rejected(&r, forged.id, AuthError::NotGenesis);
    assert_eq!(r.genesis, Some(real.id));
    let mut res = Resolver::with_genesis(real.id);
    res.insert_events(vec![forged, real.clone()]);
    assert_eq!(res.state().unwrap().owner, o.id());
    assert_eq!(res.genesis(), Some(real.id));
}

#[test]
fn forum_post_update_in_state_lane() {
    let mut cr = crew();
    let forum = ChannelSettings {
        kind: ChannelKind::Forum,
        forum_tags: vec![ForumTag {
            id: 1,
            name: "질문".into(),
            emoji: None,
        }],
        ..ChannelSettings::text("포럼")
    };
    cr.c.push(
        &cr.o,
        EventBody::ChannelCreate {
            channel: ch(1),
            settings: forum,
        },
    );
    let upd = |pinned: Option<bool>, tags: Option<Vec<u32>>| EventBody::ForumPostUpdate {
        post: EventId([1; 32]),
        title: None,
        tags,
        pinned,
        locked: None,
    };
    let with_channel = |c: &mut Chain, u: &User, body: EventBody| {
        let parents: Vec<EventId> = c.head.into_iter().collect();
        let mut e = c.w.ev(u, &parents, body);
        e.header.channel = Some(ch(1));
        c.push_event(e)
    };
    let pin_by_member = with_channel(&mut cr.c, &cr.a, upd(Some(true), None));
    let pin_by_owner = with_channel(&mut cr.c, &cr.o, upd(Some(true), None));
    let tags_by_member = with_channel(&mut cr.c, &cr.a, upd(None, Some(vec![1])));
    let bad_tag = with_channel(&mut cr.c, &cr.a, upd(None, Some(vec![2])));
    let no_channel = cr.c.push(&cr.o, upd(Some(false), None));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        pin_by_member,
        AuthError::MissingPermission(Permissions::MANAGE_MESSAGES),
    );
    assert_applied(&r, pin_by_owner);
    // 작성자 확인은 데몬이 authorize_message로 합니다.
    assert_applied(&r, tags_by_member);
    assert_rejected(&r, bad_tag, AuthError::Invalid("게시글 태그"));
    assert_rejected(&r, no_channel, AuthError::Invalid("채널 없음"));
}

// ---- 증분 해소기 ----

#[test]
fn resolver_appends_incrementally() {
    let mut cr = crew();
    for i in 0..30u8 {
        cr.c.push(&cr.o, text_channel(ch(i + 1), "채널"));
    }
    let mut res = Resolver::new();
    for e in cr.c.events.clone() {
        let u = res.insert_events(vec![e.clone()]);
        assert!(!u.full);
        assert_eq!(u.applied, vec![e.id]);
        assert!(u.state_changed);
    }
    assert_eq!(
        res.stats(),
        ResolverStats {
            incremental: 31,
            ..ResolverStats::default()
        }
    );
    assert_eq!(res.resolved(), cr.c.resolve());
    // 중복은 무시.
    assert_eq!(
        res.insert_events(vec![cr.c.events[3].clone()]),
        ResolveUpdate::default()
    );
    assert_eq!(res.len(), 31);
    assert_eq!(res.heads(), vec![cr.c.head.unwrap()]);
    assert!(res.missing().is_empty());
}

#[test]
fn resolver_rebuilds_when_order_changes() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let mods = role(1, 10, Permissions::KICK_MEMBERS);
    cr.c.push(o, EventBody::RoleCreate { role: mods.clone() });
    let x = cr.c.push(
        o,
        EventBody::MemberUpdate {
            user: a.id(),
            roles: vec![mods.id],
            nickname: None,
        },
    );
    let mut res = Resolver::new();
    res.insert_events(cr.c.events.clone());
    // a가 b를 추방(증분).
    let kick_b = cr.c.fork(
        a,
        &[x],
        EventBody::MemberKick {
            user: b.id(),
            reason: String::new(),
        },
    );
    let u = res.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    assert!(!u.full);
    assert_eq!(u.applied, vec![kick_b]);
    assert!(!res.state().unwrap().is_member(&b.id()));
    // 늦게 도착한 소유자의 동시 추방: 머리를 덮지 않으므로 전체 재해소, a의 추방이 취소됨.
    let kick_a = cr.c.fork(
        o,
        &[x],
        EventBody::MemberKick {
            user: a.id(),
            reason: String::new(),
        },
    );
    let u = res.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    assert!(u.full);
    assert_eq!(u.applied, vec![kick_a]);
    assert_eq!(u.unapplied, vec![kick_b]);
    assert_eq!(u.rejected, vec![(kick_b, AuthError::NotMember)]);
    assert!(u.state_changed);
    assert!(res.state().unwrap().is_member(&b.id()));
    assert_eq!(res.resolved(), cr.c.resolve());
    assert_eq!(res.stats().full, 1);
}

#[test]
fn resolver_fills_gaps_with_rebuild() {
    let mut cr = crew();
    let c1 = cr.c.push(&cr.o, text_channel(ch(1), "하나"));
    let c2 = cr.c.push(&cr.o, text_channel(ch(2), "둘"));
    let mut res = Resolver::new();
    let all = cr.c.events.clone();
    // 부모(c1)를 빼고 넣음 → c2는 빈 구간 뒤에서도 적용.
    res.insert_events(vec![all[0].clone(), all[2].clone()]);
    assert_eq!(res.missing(), vec![c1]);
    assert!(res.resolved().applied.contains(&c2));
    let u = res.insert_events(vec![all[1].clone()]);
    assert!(u.full);
    assert!(res.missing().is_empty());
    assert_eq!(res.resolved(), resolve(&all));
    assert_eq!(res.member_since(&cr.a.id()), Some(1));
    assert_eq!(res.space(), Some(SPACE));
    assert!(res.contains(&c1));
}

#[test]
fn resolver_with_links_stays_incremental() {
    let mut cr = crew();
    let o = &cr.o;
    let mut res = Resolver::new();
    res.insert_events(cr.c.events.clone());
    // 공간 KeyShare를 연결로 넣고 그 뒤 상태 이벤트.
    let ks = cr.c.w.ev(
        o,
        &[cr.genesis],
        EventBody::KeyShare(KeyShareBody {
            keys: vec![],
            for_user: None,
        }),
    );
    let link = LinkEvent::from_header(ks.id, &ks.header);
    let u = res.insert(vec![], vec![link.clone()]);
    assert!(!u.full);
    assert!(u.applied.is_empty());
    let after = cr.c.w.ev(o, &[ks.id], text_channel(ch(1), "x"));
    let u = res.insert_events(vec![after.clone()]);
    assert!(!u.full);
    assert_eq!(u.applied, vec![after.id]);
    assert_eq!(res.stats().full, 0);
    let mut events = cr.c.events.clone();
    events.push(after);
    assert_eq!(res.resolved(), resolve_with(&events, &[link], None));
}

/// 본문을 아직 못 풀어 순서 연결로 넣은 상태 이벤트가, 나중에 본문과 함께 오면 적용됩니다.
#[test]
fn resolver_upgrades_link_to_event() {
    let mut cr = crew();
    let c1 = cr.c.push(&cr.o, text_channel(ch(1), "하나"));
    let c2 = cr.c.push(&cr.o, text_channel(ch(2), "둘"));
    let all = cr.c.events.clone();
    let mut res = Resolver::new();
    res.insert(
        vec![all[0].clone(), all[2].clone()],
        vec![LinkEvent::from_header(c1, &all[1].header)],
    );
    assert!(!res.state().unwrap().channels.contains_key(&ch(1)));
    assert!(res.resolved().applied.contains(&c2));
    let u = res.insert_events(vec![all[1].clone()]);
    assert!(u.full);
    assert_eq!(u.applied, vec![c1]);
    assert!(res.state().unwrap().channels.contains_key(&ch(1)));
    assert_eq!(res.resolved(), resolve(&all));
    assert_eq!(res.len(), 3);
    // 같은 본문을 다시 넣어도 아무 일 없음.
    assert_eq!(
        res.insert_events(vec![all[1].clone()]),
        ResolveUpdate::default()
    );
    // 이미 본문으로 아는 이벤트를 연결로 다시 넣어도 무시.
    assert_eq!(
        res.insert(vec![], vec![LinkEvent::from_header(c2, &all[2].header)]),
        ResolveUpdate::default()
    );
    assert_eq!(res.resolved(), resolve(&all));
}

/// 고정하지 않은 해소기도 처음 적용한 공간 생성 이벤트로 고정해, 나중에 온 가짜 생성 이벤트가
/// 재해소를 일으켜도 소유자가 바뀌지 않습니다.
#[test]
fn resolver_pins_first_genesis() {
    let mut cr = crew();
    let c1 = cr.c.push(&cr.o, text_channel(ch(1), "하나"));
    let mut res = Resolver::new();
    res.insert_events(cr.c.events.clone());
    assert_eq!(res.genesis(), Some(cr.genesis));
    // 멤버 a가 자기를 소유자로 한 가짜 생성 이벤트(논리 시계 0, 부모 없음)를 퍼뜨림.
    let forged = cr.c.w.ev_at(
        cr.a.id(),
        cr.a.dev(),
        &[],
        0,
        create_body(
            SpaceKind::Crew,
            &cr.a,
            &[&cr.a, &cr.o],
            Permissions::DEFAULT_EVERYONE,
        ),
    );
    // 고정이 없으면 가짜가 이김(그래서 고정이 필요함).
    let mut with_forged = cr.c.events.clone();
    with_forged.push(forged.clone());
    assert_eq!(state(&resolve(&with_forged)).owner, cr.a.id());

    let u = res.insert_events(vec![forged.clone()]);
    assert!(u.full);
    assert_eq!(u.rejected, vec![(forged.id, AuthError::NotGenesis)]);
    assert!(u.unapplied.is_empty());
    assert_eq!(res.state().unwrap().owner, cr.o.id());
    assert!(res.state().unwrap().channels.contains_key(&ch(1)));
    assert!(res.applied().contains(&c1));
    assert_eq!(res.genesis(), Some(cr.genesis));
}

/// 상태 레인은 상태 키만 받습니다(기록 키·봉인 본문이면 모든 멤버가 같은 키로 풀지 못함).
#[test]
fn state_events_need_protocol_key() {
    let mut cr = crew();
    let record = {
        let parents: Vec<EventId> = cr.c.head.into_iter().collect();
        let mut e = cr.c.w.ev(&cr.o, &parents, text_channel(ch(1), "기록 키"));
        e.header.key = KeyRef::Record {
            root: ChannelRootId([1; 16]),
        };
        cr.c.push_event(e)
    };
    let sealed = {
        let parents: Vec<EventId> = cr.c.head.into_iter().collect();
        let mut e = cr.c.w.ev(&cr.o, &parents, text_channel(ch(2), "봉인"));
        e.header.key = KeyRef::Sealed;
        cr.c.push_event(e)
    };
    let ok = cr.c.push(&cr.o, text_channel(ch(3), "정상"));
    let r = cr.c.resolve();
    assert_rejected(&r, record, AuthError::WrongKey);
    assert_rejected(&r, sealed, AuthError::WrongKey);
    assert_applied(&r, ok);
    // 공간 생성도 상태 키로 암호화해야 함.
    let o = User::new(1);
    let mut w = World::new();
    let mut g = w.ev(
        &o,
        &[],
        create_body(SpaceKind::Crew, &o, &[&o], Permissions::DEFAULT_EVERYONE),
    );
    g.header.key = KeyRef::Sealed;
    let r = resolve(std::slice::from_ref(&g));
    assert_eq!(r.rejected, vec![(g.id, AuthError::WrongKey)]);
    assert!(r.state.is_none());
}

/// 서명 검증 결과는 이벤트마다 기억하지만, 재해소 결과(거부 이유 포함)는 처음 검증할 때와 같습니다.
#[test]
fn bad_signatures_stay_rejected_across_rebuilds() {
    let mut cr = crew();
    let x = User::new(20);
    let mut forged_list = x.list();
    forged_list.list.version = 7; // 서명 뒤에 바꿈 → 서명 불일치
    let bad_join = cr.c.push(
        &cr.o,
        EventBody::MemberJoin {
            member: x.member(),
            devices: forged_list,
            profile: None,
            invite: None,
        },
    );
    let bad_profile = {
        let y = User::new(21);
        let mut p = Profile {
            version: 1,
            name: "와이".into(),
            ..Default::default()
        }
        .sign(&y.identity)
        .unwrap();
        p.profile.name = "바꿈".into();
        cr.c.push(
            &cr.o,
            EventBody::MemberJoin {
                member: y.member(),
                devices: y.list(),
                profile: Some(p),
                invite: None,
            },
        )
    };
    let mut res = Resolver::new();
    res.insert_events(cr.c.events.clone());
    // 동시 이벤트로 재해소를 일으킴.
    let concurrent = cr.c.fork(
        &cr.a,
        &[cr.genesis],
        EventBody::MemberLeave { user: cr.a.id() },
    );
    let u = res.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    assert!(u.full);
    let r = res.resolved();
    assert_rejected(&r, bad_join, AuthError::BadDeviceList);
    assert_rejected(&r, bad_profile, AuthError::BadProfile);
    assert_applied(&r, concurrent);
    assert_eq!(r, cr.c.resolve());
}

#[test]
fn large_history_resolves_quickly() {
    let mut cr = crew();
    cr.c.push(&cr.o, text_channel(ch(1), "채널"));
    for i in 0..3000u32 {
        cr.c.push(
            &cr.o,
            EventBody::ChannelUpdate {
                channel: ch(1),
                settings: ChannelSettings::text(format!("이름{i}")),
            },
        );
    }
    let r = cr.c.resolve();
    assert_eq!(r.applied.len(), 3002);
    let mut res = Resolver::new();
    for e in cr.c.events.clone() {
        res.insert_events(vec![e]);
    }
    assert_eq!(res.stats().full, 0);
    assert_eq!(res.resolved(), r);
}

// ---- 메시지 권한 ----

struct MsgFx {
    st: SpaceState,
    o: User,
    a: User,
    b: User,
}

const TEXT: ChannelId = ChannelId([1; 16]);
const SECRET: ChannelId = ChannelId([2; 16]);
const VOLATILE: ChannelId = ChannelId([3; 16]);
const FORUM: ChannelId = ChannelId([4; 16]);
const NO_FILES: ChannelId = ChannelId([5; 16]);
const CATEGORY: ChannelId = ChannelId([6; 16]);

fn msg_fx() -> MsgFx {
    let mut cr = crew();
    let o = &cr.o;
    let deny_everyone = |p: Permissions| {
        vec![PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::NONE,
            deny: p,
        }]
    };
    cr.c.push(o, text_channel(TEXT, "일반"));
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: SECRET,
            settings: ChannelSettings {
                overwrites: deny_everyone(Permissions::VIEW_CHANNEL),
                ..ChannelSettings::text("비밀")
            },
        },
    );
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: VOLATILE,
            settings: ChannelSettings {
                security: SecurityLevel::Volatile,
                ..ChannelSettings::text("휘발")
            },
        },
    );
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: FORUM,
            settings: ChannelSettings {
                kind: ChannelKind::Forum,
                forum_tags: vec![ForumTag {
                    id: 7,
                    name: "공지".into(),
                    emoji: None,
                }],
                ..ChannelSettings::text("포럼")
            },
        },
    );
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: NO_FILES,
            settings: ChannelSettings {
                overwrites: deny_everyone(
                    Permissions::ATTACH_FILES.union(Permissions::EMBED_LINKS),
                ),
                ..ChannelSettings::text("파일 금지")
            },
        },
    );
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: CATEGORY,
            settings: ChannelSettings {
                kind: ChannelKind::Category,
                ..ChannelSettings::text("묶음")
            },
        },
    );
    cr.c.push(
        o,
        EventBody::EmojiAdd {
            emoji: CustomEmoji {
                id: EmojiId(42),
                name: "양파".into(),
                animated: false,
                blob: BlobId([1; 32]),
                key: Secret32([0; 32]),
                mime: "image/png".into(),
            },
        },
    );
    let r = cr.c.resolve();
    assert!(r.rejected.is_empty(), "{:?}", r.rejected);
    MsgFx {
        st: r.state.unwrap(),
        o: cr.o,
        a: cr.a,
        b: cr.b,
    }
}

fn mheader(u: &User, channel: Option<ChannelId>, body: &EventBody, key: KeyRef) -> EventHeader {
    EventHeader {
        v: EVENT_VERSION,
        space: SPACE,
        channel,
        kind: body.kind(),
        author: u.dev(),
        user: u.id(),
        parents: vec![],
        lamport: 100,
        created_at: Timestamp(0),
        key,
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    }
}

const HKEY: KeyRef = KeyRef::Record {
    root: ChannelRootId([1; 16]),
};

fn check(
    f: &MsgFx,
    u: &User,
    channel: ChannelId,
    body: EventBody,
    target: Option<UserId>,
) -> Result<(), AuthError> {
    let key = if channel == VOLATILE {
        KeyRef::Volatile { generation: 1 }
    } else {
        HKEY
    };
    authorize_message(&f.st, &mheader(u, Some(channel), &body, key), &body, target)
}

fn text(s: &str) -> EventBody {
    EventBody::Message(MessageBody {
        text: s.into(),
        ..Default::default()
    })
}

fn attachment() -> yangpa_core::body::Attachment {
    yangpa_core::body::Attachment {
        blob: BlobId([1; 32]),
        key: Secret32([1; 32]),
        name: "사진.png".into(),
        mime: "image/png".into(),
        size: 10,
        width: None,
        height: None,
        duration_ms: None,
        thumbnail: vec![],
    }
}

#[test]
fn message_permissions() {
    let f = msg_fx();
    let (o, a) = (&f.o, &f.a);
    assert_eq!(check(&f, a, TEXT, text("안녕"), None), Ok(()));
    assert_eq!(check(&f, a, VOLATILE, text("사라질 말"), None), Ok(()));
    assert_eq!(
        check(&f, &User::new(40), TEXT, text("x"), None),
        Err(AuthError::NotMember)
    );
    // 익명 표시(헤더 종류)는 같은 본문·같은 권한으로 검사하고, 다른 본문에는 붙일 수 없습니다.
    let body = text("익명으로");
    let mut h = mheader(a, Some(TEXT), &body, HKEY);
    h.kind = EventKind::AnonMessage;
    assert_eq!(authorize_message(&f.st, &h, &body, None), Ok(()));
    h.kind = EventKind::AnonForumPost;
    assert_eq!(
        authorize_message(&f.st, &h, &body, None),
        Err(AuthError::KindMismatch)
    );
    h.kind = EventKind::AnonMessage;
    h.channel = Some(SECRET);
    assert_eq!(
        authorize_message(&f.st, &h, &body, None),
        Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL))
    );
    // 남의 기기.
    let body = text("x");
    let mut h = mheader(a, Some(TEXT), &body, HKEY);
    h.author = o.dev();
    assert_eq!(
        authorize_message(&f.st, &h, &body, None),
        Err(AuthError::UnknownDevice)
    );
    // 채널 없음·모르는 채널·볼 수 없는 채널·카테고리.
    assert_eq!(
        authorize_message(&f.st, &mheader(a, None, &body, HKEY), &body, None),
        Err(AuthError::Invalid("채널 없음"))
    );
    assert_eq!(
        check(&f, a, ChannelId([99; 16]), text("x"), None),
        Err(AuthError::UnknownChannel)
    );
    assert_eq!(
        check(&f, a, SECRET, text("x"), None),
        Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL))
    );
    assert_eq!(check(&f, o, SECRET, text("x"), None), Ok(()));
    assert_eq!(
        check(&f, o, CATEGORY, text("x"), None),
        Err(AuthError::Invalid("카테고리에는 메시지를 쓸 수 없음"))
    );
    // 첨부.
    let with_file = EventBody::Message(MessageBody {
        text: String::new(),
        attachments: vec![attachment()],
        ..Default::default()
    });
    assert_eq!(check(&f, a, TEXT, with_file.clone(), None), Ok(()));
    assert_eq!(
        check(&f, a, NO_FILES, with_file, None),
        Err(AuthError::MissingPermission(Permissions::ATTACH_FILES))
    );
    // 링크 미리보기는 EMBED_LINKS가 필요합니다(메시지·게시글 첫 메시지·수정 모두).
    let embed = yangpa_core::body::LinkEmbed {
        url: "https://example.com/".into(),
        title: "예시".into(),
        description: String::new(),
        site_name: String::new(),
        thumbnail: Vec::new(),
        player_url: None,
    };
    let with_embed = EventBody::Message(MessageBody {
        text: "https://example.com/".into(),
        embeds: vec![embed.clone()],
        ..Default::default()
    });
    assert_eq!(check(&f, a, TEXT, with_embed.clone(), None), Ok(()));
    assert_eq!(
        check(&f, a, NO_FILES, with_embed, None),
        Err(AuthError::MissingPermission(Permissions::EMBED_LINKS))
    );
    assert_eq!(check(&f, a, NO_FILES, text("링크 없음"), None), Ok(()));
    let edit_embed = EventBody::Edit {
        target: EventId([1; 32]),
        text: "https://example.com/".into(),
        embeds: vec![embed],
    };
    assert_eq!(check(&f, a, TEXT, edit_embed.clone(), Some(a.id())), Ok(()));
    assert_eq!(
        check(&f, a, NO_FILES, edit_embed, Some(a.id())),
        Err(AuthError::MissingPermission(Permissions::EMBED_LINKS))
    );
    // 빈 메시지·너무 긴 메시지·첨부 수.
    assert_eq!(
        check(&f, a, TEXT, text("  "), None),
        Err(AuthError::Invalid("빈 메시지"))
    );
    assert_eq!(
        check(&f, a, TEXT, text(&"가".repeat(8001)), None),
        Err(AuthError::Limit("메시지"))
    );
    let many = EventBody::Message(MessageBody {
        text: "x".into(),
        attachments: vec![attachment(); 11],
        ..Default::default()
    });
    assert_eq!(
        check(&f, a, TEXT, many, None),
        Err(AuthError::Limit("첨부 수"))
    );
    // 상태 레인 이벤트는 여기서 검사하지 않음.
    let st_body = text_channel(ch(9), "x");
    assert_eq!(
        authorize_message(
            &f.st,
            &mheader(o, None, &st_body, KeyRef::State(StateKeyId([1; 16]))),
            &st_body,
            None
        ),
        Err(AuthError::WrongLane)
    );
    // 헤더·본문 종류 불일치.
    let mut h = mheader(a, Some(TEXT), &body, HKEY);
    h.kind = EventKind::Edit;
    assert_eq!(
        authorize_message(&f.st, &h, &body, None),
        Err(AuthError::KindMismatch)
    );
}

#[test]
fn message_key_must_match_channel_security() {
    let f = msg_fx();
    let a = &f.a;
    let body = text("x");
    let vol_key = KeyRef::Volatile { generation: 3 };
    assert_eq!(
        authorize_message(&f.st, &mheader(a, Some(VOLATILE), &body, HKEY), &body, None),
        Err(AuthError::WrongKey)
    );
    assert_eq!(
        authorize_message(&f.st, &mheader(a, Some(TEXT), &body, vol_key), &body, None),
        Err(AuthError::WrongKey)
    );
    assert_eq!(
        authorize_message(
            &f.st,
            &mheader(a, Some(TEXT), &body, KeyRef::State(StateKeyId([0; 16]))),
            &body,
            None
        ),
        Err(AuthError::WrongKey)
    );
}

#[test]
fn edit_delete_reaction_rules() {
    let f = msg_fx();
    let (o, a, b) = (&f.o, &f.a, &f.b);
    let edit = EventBody::Edit {
        target: EventId([1; 32]),
        text: "고침".into(),
        embeds: vec![],
    };
    assert_eq!(check(&f, a, TEXT, edit.clone(), Some(a.id())), Ok(()));
    assert_eq!(
        check(&f, a, TEXT, edit.clone(), Some(b.id())),
        Err(AuthError::NotAuthor)
    );
    assert_eq!(
        check(&f, o, TEXT, edit.clone(), Some(a.id())),
        Err(AuthError::NotAuthor)
    );
    assert_eq!(
        check(&f, a, TEXT, edit, None),
        Err(AuthError::UnknownTarget)
    );

    let del = EventBody::Delete {
        target: EventId([1; 32]),
    };
    assert_eq!(check(&f, a, TEXT, del.clone(), Some(a.id())), Ok(()));
    assert_eq!(
        check(&f, a, TEXT, del.clone(), Some(b.id())),
        Err(AuthError::MissingPermission(Permissions::MANAGE_MESSAGES))
    );
    assert_eq!(
        check(&f, a, TEXT, del.clone(), None),
        Err(AuthError::UnknownTarget)
    );
    assert_eq!(check(&f, o, TEXT, del.clone(), Some(a.id())), Ok(()));
    assert_eq!(check(&f, o, TEXT, del, None), Ok(()));

    let react = |emoji: ReactionEmoji| EventBody::Reaction {
        target: EventId([1; 32]),
        emoji,
    };
    assert_eq!(
        check(
            &f,
            a,
            TEXT,
            react(ReactionEmoji::Unicode {
                value: "👍".into()
            }),
            None
        ),
        Ok(())
    );
    assert_eq!(
        check(
            &f,
            a,
            TEXT,
            react(ReactionEmoji::Unicode {
                value: String::new()
            }),
            None
        ),
        Err(AuthError::Invalid("이모지"))
    );
    let local = ReactionEmoji::Custom {
        id: EmojiId(42),
        name: "양파".into(),
        animated: false,
    };
    let external = ReactionEmoji::Custom {
        id: EmojiId(43),
        name: "남의것".into(),
        animated: false,
    };
    assert_eq!(check(&f, a, TEXT, react(local), None), Ok(()));
    assert_eq!(
        check(&f, o, TEXT, react(external.clone()), None),
        Err(AuthError::ExternalEmojiDisabled)
    );
    let mut open = f.st.clone();
    open.settings.allow_external_emoji = true;
    let f2 = MsgFx {
        st: open,
        o: User::new(1),
        a: User::new(2),
        b: User::new(3),
    };
    assert_eq!(
        check(&f2, &f2.o, TEXT, react(external.clone()), None),
        Ok(())
    );
    assert_eq!(
        check(&f2, &f2.a, TEXT, react(external.clone()), None),
        Err(AuthError::MissingPermission(
            Permissions::USE_EXTERNAL_EMOJI
        ))
    );
    // 반응 지우기는 보기 권한이면 충분.
    assert_eq!(
        check(
            &f,
            a,
            TEXT,
            EventBody::ReactionRemove {
                target: EventId([1; 32]),
                emoji: external
            },
            None
        ),
        Ok(())
    );
    // 반응 권한을 뺀 상태.
    let mut no_react = f.st.clone();
    no_react
        .roles
        .get_mut(&RoleId::EVERYONE)
        .unwrap()
        .permissions = Permissions::DEFAULT_EVERYONE.remove(Permissions::ADD_REACTIONS);
    let f3 = MsgFx {
        st: no_react,
        o: User::new(1),
        a: User::new(2),
        b: User::new(3),
    };
    assert_eq!(
        check(
            &f3,
            &f3.a,
            TEXT,
            react(ReactionEmoji::Unicode {
                value: "👍".into()
            }),
            None
        ),
        Err(AuthError::MissingPermission(Permissions::ADD_REACTIONS))
    );
}

/// 1대1 스페이스에서는 두 친구가 동등: 소유자도 상대 메시지를 지우지 못합니다.
#[test]
fn edit_rules_block_edits_and_own_deletes() {
    let mut f = msg_fx();
    f.st.edit_rules.insert(
        TEXT,
        EditRules {
            no_edit: true,
            no_delete: true,
        },
    );
    let (o, a) = (&f.o, &f.a);
    let edit = EventBody::Edit {
        target: EventId([1; 32]),
        text: "고침".into(),
        embeds: vec![],
    };
    assert_eq!(
        check(&f, a, TEXT, edit, Some(a.id())),
        Err(AuthError::Invalid("수정 금지 채널"))
    );
    let del = EventBody::Delete {
        target: EventId([1; 32]),
    };
    assert_eq!(
        check(&f, a, TEXT, del.clone(), Some(a.id())),
        Err(AuthError::Invalid("삭제 금지 채널"))
    );
    // 운영 삭제(MANAGE_MESSAGES)는 됩니다.
    assert_eq!(check(&f, o, TEXT, del, Some(a.id())), Ok(()));
    // 반응은 상관없습니다.
    let react = EventBody::Reaction {
        target: EventId([1; 32]),
        emoji: ReactionEmoji::Unicode {
            value: "👍".into()
        },
    };
    assert_eq!(check(&f, a, TEXT, react, None), Ok(()));
}

#[test]
fn friend_space_delete_is_symmetric() {
    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    c.push(&a, text_channel(TEXT, "일반"));
    let r = c.resolve();
    let f = MsgFx {
        st: r.state.unwrap(),
        o,
        a,
        b: User::new(3),
    };
    let del = EventBody::Delete {
        target: EventId([1; 32]),
    };
    assert_eq!(
        check(&f, &f.o, TEXT, del.clone(), Some(f.a.id())),
        Err(AuthError::MissingPermission(Permissions::MANAGE_MESSAGES))
    );
    assert_eq!(
        check(&f, &f.a, TEXT, del.clone(), Some(f.o.id())),
        Err(AuthError::MissingPermission(Permissions::MANAGE_MESSAGES))
    );
    assert_eq!(check(&f, &f.o, TEXT, del.clone(), Some(f.o.id())), Ok(()));
    assert_eq!(check(&f, &f.a, TEXT, del, Some(f.a.id())), Ok(()));
}

#[test]
fn forum_rules() {
    let f = msg_fx();
    let (o, a, b) = (&f.o, &f.a, &f.b);
    let post = |tags: Vec<u32>| EventBody::ForumPost {
        title: "첫 글".into(),
        tags,
        first: MessageBody {
            text: "본문".into(),
            ..Default::default()
        },
    };
    assert_eq!(check(&f, a, FORUM, post(vec![7]), None), Ok(()));
    assert_eq!(
        check(&f, a, FORUM, post(vec![8]), None),
        Err(AuthError::Invalid("게시글 태그"))
    );
    assert_eq!(
        check(&f, a, TEXT, post(vec![]), None),
        Err(AuthError::Invalid("포럼 채널이 아님"))
    );
    assert_eq!(
        check(&f, a, FORUM, text("게시글 밖"), None),
        Err(AuthError::Invalid("포럼 채널의 메시지는 게시글 안에만"))
    );
    let comment = EventBody::Message(MessageBody {
        text: "댓글".into(),
        thread: Some(EventId([1; 32])),
        ..Default::default()
    });
    assert_eq!(check(&f, a, FORUM, comment, None), Ok(()));
    let mut no_posts = f.st.clone();
    no_posts
        .roles
        .get_mut(&RoleId::EVERYONE)
        .unwrap()
        .permissions = Permissions::DEFAULT_EVERYONE.remove(Permissions::CREATE_POSTS);
    let f2 = MsgFx {
        st: no_posts,
        o: User::new(1),
        a: User::new(2),
        b: User::new(3),
    };
    assert_eq!(
        check(&f2, &f2.a, FORUM, post(vec![]), None),
        Err(AuthError::MissingPermission(Permissions::CREATE_POSTS))
    );

    let upd = |title: Option<&str>, pinned: Option<bool>| EventBody::ForumPostUpdate {
        post: EventId([1; 32]),
        title: title.map(str::to_owned),
        tags: None,
        pinned,
        locked: None,
    };
    let state_key = KeyRef::State(StateKeyId([1; 16]));
    let auth = |u: &User, body: EventBody, target| {
        authorize_message(
            &f.st,
            &mheader(u, Some(FORUM), &body, state_key),
            &body,
            target,
        )
    };
    assert_eq!(auth(a, upd(Some("새 제목"), None), Some(a.id())), Ok(()));
    assert_eq!(
        auth(b, upd(Some("새 제목"), None), Some(a.id())),
        Err(AuthError::NotAuthor)
    );
    assert_eq!(
        auth(b, upd(Some("새 제목"), None), None),
        Err(AuthError::UnknownTarget)
    );
    assert_eq!(auth(o, upd(Some("새 제목"), None), Some(a.id())), Ok(()));
    assert_eq!(
        auth(a, upd(None, Some(true)), Some(a.id())),
        Err(AuthError::MissingPermission(Permissions::MANAGE_MESSAGES))
    );
    assert_eq!(auth(o, upd(None, Some(true)), Some(a.id())), Ok(()));
    assert_eq!(
        auth(o, upd(None, None), Some(a.id())),
        Err(AuthError::Invalid("바꿀 내용 없음"))
    );
}

#[test]
fn key_share_rules() {
    let f = msg_fx();
    let (o, a) = (&f.o, &f.a);
    let hist = |c: ChannelId| SharedKey::ChannelRoot {
        id: yangpa_core::body::channel_root_id(&SPACE, &c, &Secret32([0; 32])),
        channel: c,
        key: Secret32([0; 32]),
    };
    let share = |keys: Vec<SharedKey>| {
        EventBody::KeyShare(KeyShareBody {
            keys,
            for_user: None,
        })
    };
    let state_key = KeyRef::State(StateKeyId([1; 16]));
    let auth = |u: &User, channel: Option<ChannelId>, body: EventBody| {
        authorize_message(&f.st, &mheader(u, channel, &body, state_key), &body, None)
    };
    assert_eq!(auth(a, Some(TEXT), share(vec![hist(TEXT)])), Ok(()));
    assert_eq!(
        auth(a, Some(TEXT), share(vec![])),
        Err(AuthError::Invalid("빈 키 묶음"))
    );
    assert_eq!(
        auth(a, Some(TEXT), share(vec![hist(NO_FILES)])),
        Err(AuthError::Invalid("다른 채널의 키"))
    );
    assert_eq!(
        auth(a, Some(SECRET), share(vec![hist(SECRET)])),
        Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL))
    );
    assert_eq!(auth(o, Some(SECRET), share(vec![hist(SECRET)])), Ok(()));
    assert_eq!(
        auth(a, Some(VOLATILE), share(vec![hist(VOLATILE)])),
        Err(AuthError::WrongKey)
    );
    let vkey = SharedKey::Volatile {
        channel: VOLATILE,
        generation: 1,
        key: Secret32([0; 32]),
    };
    // 휘발성 세션 키: 그 채널 스트림에서 봉인 상자로만. 그룹 키로 감싸거나(채널 밖 멤버도 풂)
    // 공간 스트림(디스크 저장)에 실으면 거부.
    assert_eq!(
        auth(a, Some(VOLATILE), share(vec![vkey.clone()])),
        Err(AuthError::WrongKey)
    );
    let sealed = |u: &User, channel: Option<ChannelId>, body: EventBody| {
        authorize_message(
            &f.st,
            &mheader(u, channel, &body, KeyRef::Sealed),
            &body,
            None,
        )
    };
    assert_eq!(sealed(a, Some(VOLATILE), share(vec![vkey.clone()])), Ok(()));
    assert_eq!(
        sealed(a, None, share(vec![vkey.clone()])),
        Err(AuthError::WrongKey)
    );
    // 키 묶음을 휘발성 키나 기록 키로 보내면 거부.
    for key in [KeyRef::Volatile { generation: 1 }, HKEY] {
        let body = share(vec![hist(TEXT)]);
        assert_eq!(
            authorize_message(&f.st, &mheader(a, Some(TEXT), &body, key), &body, None),
            Err(AuthError::WrongKey)
        );
    }
    let body = share(vec![hist(TEXT)]);
    assert_eq!(
        authorize_message(
            &f.st,
            &mheader(a, Some(TEXT), &body, KeyRef::State(StateKeyId([1; 16]))),
            &body,
            None
        ),
        Ok(())
    );
    // 공간 수준: 여러 채널의 기록 키(새 멤버에게), 상태 키는 확정 기기·소유자·관리자만.
    assert_eq!(
        auth(a, None, share(vec![hist(TEXT), hist(NO_FILES)])),
        Ok(())
    );
    assert_eq!(
        auth(a, None, share(vec![hist(SECRET)])),
        Err(AuthError::MissingPermission(Permissions::VIEW_CHANNEL))
    );
    let skey = SharedKey::State {
        id: yangpa_core::body::state_key_id(&SPACE, &Secret32([0; 32])),
        key: Secret32([0; 32]),
    };
    // ID가 키에서 나오지 않은 상태 키는 누구든 거부(쓰는 ID에 다른 키를 끼워 넣지 못하게).
    let unbound = SharedKey::State {
        id: StateKeyId([5; 16]),
        key: Secret32([0; 32]),
    };
    assert_eq!(
        auth(o, None, share(vec![unbound])),
        Err(AuthError::Invalid("상태 키 ID가 키와 맞지 않음"))
    );
    assert_eq!(
        auth(a, None, share(vec![skey.clone()])),
        Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR))
    );
    assert_eq!(auth(o, None, share(vec![skey.clone()])), Ok(()));
    // 확정 기기의 사용자(여기서는 소유자 o)에게 봉인해 넘기는 인계는 멤버 누구나. 다른 사용자 앞이거나 봉인이 아니면 거부.
    let handover = |to: &User, key: KeyRef| {
        let body = EventBody::KeyShare(KeyShareBody {
            keys: vec![skey.clone()],
            for_user: Some(to.id()),
        });
        authorize_message(&f.st, &mheader(a, None, &body, key), &body, None)
    };
    assert_eq!(f.st.committer, o.dev());
    assert_eq!(handover(o, KeyRef::Sealed), Ok(()));
    // 인계로만 통과한 묶음(작성자에게 권한 없음)은 받는 쪽이 지금 키로 쓰지 않습니다.
    let hdr = |u: &User| mheader(u, None, &share(vec![skey.clone()]), KeyRef::Sealed);
    assert!(state_key_handover_only(&f.st, &hdr(a)));
    assert!(!state_key_handover_only(&f.st, &hdr(o)));
    assert_eq!(
        handover(a, KeyRef::Sealed),
        Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR))
    );
    assert_eq!(
        handover(o, state_key),
        Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR))
    );
    assert_eq!(
        auth(o, Some(TEXT), share(vec![skey])),
        Err(AuthError::Invalid("채널 키 묶음에 상태 키"))
    );
    assert_eq!(
        auth(a, Some(ChannelId([99; 16])), share(vec![hist(TEXT)])),
        Err(AuthError::UnknownChannel)
    );
}

// ---- 결정론 속성 검사 ----

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

/// 무작위지만 의미 있는(상당수가 통과하는) 동시 상태 이벤트 기록을 만듭니다.
fn scenario(seed: u64, n: usize) -> (Vec<DecodedEvent>, Vec<LinkEvent>) {
    let mut rng = Rng(seed);
    let users: Vec<User> = (1..=6u8).map(User::new).collect();
    let mut w = World::new();
    let initial: Vec<&User> = users[..4].iter().collect();
    let genesis = w.ev(
        &users[0],
        &[],
        create_body(
            SpaceKind::Crew,
            &users[0],
            &initial,
            Permissions::DEFAULT_EVERYONE,
        ),
    );
    let mut ids = vec![genesis.id];
    let mut events = vec![genesis];
    let mut links = Vec::new();
    let roles = [RoleId([1; 16]), RoleId([2; 16]), RoleId([3; 16])];
    let perm_menu = [
        Permissions::KICK_MEMBERS,
        Permissions::MANAGE_ROLES.union(Permissions::KICK_MEMBERS),
        Permissions::MANAGE_CHANNELS.union(Permissions::CREATE_INVITE),
        Permissions::ADMINISTRATOR,
    ];
    for _ in 0..n {
        let k = 1 + rng.below(3);
        let mut parents = Vec::new();
        for _ in 0..k {
            let back = rng.below(ids.len().min(6));
            parents.push(ids[ids.len() - 1 - back]);
        }
        if rng.below(12) == 0 {
            parents.push(EventId::hash_of(&rng.next().to_le_bytes()));
        }
        // 작성자는 대체로 처음 멤버(특히 소유자) 쪽으로 치우쳐야 적용되는 사건이 충분히 생깁니다.
        let author = match rng.below(7) {
            0..=2 => &users[0],
            3..=5 => &users[1 + rng.below(3)],
            _ => &users[4 + rng.below(2)],
        };
        let pick_user = |rng: &mut Rng| users[rng.below(users.len())].id();
        let body = match rng.below(19) {
            0 | 1 => text_channel(ch(1 + rng.below(4) as u8), "채널"),
            15 => EventBody::ChannelDelete {
                channel: ch(1 + rng.below(4) as u8),
            },
            16 | 17 => {
                // 나갔던 사람도 다시 들어옵니다. 기기 목록 판이 낮으면 거부되는 경우도 섞입니다.
                let joiner = &users[1 + rng.below(5)];
                joiner.join_body_v(None, 1 + rng.below(6) as u64)
            }
            2 => EventBody::ChannelUpdate {
                channel: ch(1 + rng.below(4) as u8),
                settings: ChannelSettings::text(format!("이름{}", rng.below(9))),
            },
            3 => EventBody::RoleCreate {
                role: Role {
                    id: roles[rng.below(3)],
                    name: "역할".into(),
                    color: 0,
                    rank: 1 + rng.below(200) as u32,
                    permissions: perm_menu[rng.below(perm_menu.len())],
                    automation: false,
                },
            },
            4 => {
                let mut rs: Vec<RoleId> = roles
                    .iter()
                    .filter(|_| rng.below(2) == 0)
                    .copied()
                    .collect();
                rs.sort();
                EventBody::MemberUpdate {
                    user: pick_user(&mut rng),
                    roles: rs,
                    nickname: None,
                }
            }
            5 => EventBody::MemberKick {
                user: pick_user(&mut rng),
                reason: String::new(),
            },
            6 if rng.below(2) == 0 => EventBody::MemberLeave { user: author.id() },
            6 => EventBody::MemberUpdate {
                user: author.id(),
                roles: vec![],
                nickname: Some(format!("별명{}", rng.below(5))),
            },
            7 => invite(
                &LinkToken([rng.below(3) as u8; 16]),
                Some(1 + rng.below(2) as u32),
                None,
                vec![roles[rng.below(3)]],
            ),
            8 => {
                let joiner = &users[4 + rng.below(2)];
                let token = if rng.below(2) == 0 {
                    Some(LinkToken([rng.below(3) as u8; 16]))
                } else {
                    None
                };
                joiner.join_body(token)
            }
            // 소유권 이전은 드물게: 너무 잦으면 대부분의 사건이 권한 부족으로 끝납니다.
            9 if rng.below(4) == 0 => transfer(author, pick_user(&mut rng)),
            9 => EventBody::MemberUpdate {
                user: pick_user(&mut rng),
                roles: vec![roles[rng.below(3)]],
                nickname: None,
            },
            10 => EventBody::RoleDelete {
                role: roles[rng.below(3)],
            },
            11 => EventBody::CommitterChange {
                device: users[rng.below(users.len())].dev(),
            },
            12 => {
                // 크루 첫 키 갱신(순번 1): 동시에 여럿이어도 해소 순서로 하나만 받아들입니다(뽑기는 한 번 그대로).
                let mut joined: Vec<DeviceId> = match rng.below(4) {
                    0 => initial.iter().map(|u| u.dev()).collect(),
                    1 => vec![users[0].dev()],
                    2 => users.iter().map(|u| u.dev()).collect(),
                    _ => Vec::new(),
                };
                joined.sort();
                let kind = if joined.is_empty() {
                    KeyUpdateKind::Refresh
                } else {
                    KeyUpdateKind::Advance { joined }
                };
                EventBody::KeyUpdate(crew_update(1, None, kind))
            }
            13 => EventBody::RoleDisplaySet {
                role: roles[rng.below(3)],
                display: RoleDisplay {
                    visible_to: (rng.below(2) == 0).then(|| vec![roles[rng.below(3)]]),
                    see_admins: rng.below(2) == 0,
                },
            },
            14 => anonymous(ch(5 + rng.below(4) as u8), rng.below(2) == 0),
            _ => text_channel(ch(5 + rng.below(4) as u8), "채널"),
        };
        let mut lamport = w.lamport_after(&parents);
        lamport += rng.below(3) as u64;
        let e = w.ev_at(author.id(), author.dev(), &parents, lamport, body);
        ids.push(e.id);
        if rng.below(10) == 0 {
            links.push(LinkEvent::from_header(e.id, &e.header));
        } else {
            events.push(e);
        }
    }
    (events, links)
}

#[test]
fn scenario_is_meaningful() {
    // 생성기가 충분히 많은 적용·거부를 만드는지(속성 검사가 헛돌지 않게).
    let (events, links) = scenario(12345, 300);
    let r = resolve_with(&events, &links, None);
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for (_, e) in &r.rejected {
        *reasons.entry(format!("{e:?}")).or_default() += 1;
    }
    assert!(r.applied.len() > 25, "적용 {}", r.applied.len());
    assert!(reasons.len() >= 8, "거부 이유가 다양해야 함: {reasons:?}");
    assert!(r.rejected.len() > 40, "거부 {}", r.rejected.len());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 입력 순서가 달라도 해소 결과(상태·적용·거부·키 이벤트 순서)가 같습니다.
    #[test]
    fn resolve_is_order_independent(seed in any::<u64>(), n in 1usize..150, shuffle_seed in any::<u64>()) {
        let (events, links) = scenario(seed, n);
        let base = resolve_with(&events, &links, None);
        let mut rng = Rng(shuffle_seed);
        for _ in 0..3 {
            let mut ev = events.clone();
            let mut ln = links.clone();
            rng.shuffle(&mut ev);
            rng.shuffle(&mut ln);
            prop_assert_eq!(&resolve_with(&ev, &ln, None), &base);
        }
    }

    /// 증분 해소기에 어떤 순서·묶음으로 넣어도 한 번에 해소한 것과 같습니다.
    #[test]
    fn resolver_matches_full_resolution(seed in any::<u64>(), n in 1usize..120, order_seed in any::<u64>()) {
        let (events, links) = scenario(seed, n);
        let base = resolve_with(&events, &links, None);
        let mut rng = Rng(order_seed);
        // 절반은 작성 순서 그대로(증분 경로), 절반은 섞어서(재해소 경로).
        let mut items: Vec<(Option<DecodedEvent>, Option<LinkEvent>)> =
            events.iter().cloned().map(|e| (Some(e), None)).chain(links.iter().cloned().map(|l| (None, Some(l)))).collect();
        if order_seed % 2 == 0 {
            rng.shuffle(&mut items);
        } else {
            items.sort_by_key(|(e, l)| e.as_ref().map(|e| e.header.lamport).or(l.as_ref().map(|l| l.lamport)));
        }
        let mut res = Resolver::new();
        let mut i = 0;
        while i < items.len() {
            let take = 1 + rng.below(4);
            let batch = &items[i..(i + take).min(items.len())];
            let evs: Vec<DecodedEvent> = batch.iter().filter_map(|(e, _)| e.clone()).collect();
            let lns: Vec<LinkEvent> = batch.iter().filter_map(|(_, l)| l.clone()).collect();
            res.insert(evs, lns);
            i += take;
        }
        prop_assert_eq!(&res.resolved(), &base);
        let st = res.stats();
        prop_assert!(st.incremental + st.full + st.replay > 0);
    }
}

// ---- 순위 재정렬 범위·스냅숏 재생 ----

/// 시험 전용 전환(`FORCE_RANK_ALL`, `NO_REPLAY`)을 켠 채 `f`를 부르고, 실패해도 끕니다.
fn with_flag<T>(
    flag: &'static std::thread::LocalKey<std::cell::Cell<bool>>,
    f: impl FnOnce() -> T,
) -> T {
    struct Off(&'static std::thread::LocalKey<std::cell::Cell<bool>>);
    impl Drop for Off {
        fn drop(&mut self) {
            self.0.set(false);
        }
    }
    flag.set(true);
    let _off = Off(flag);
    f()
}

/// 무작위 기록에 쓰는 사용자 여덟(키 생성이 무거워 한 번만 만듦).
fn churn_users() -> &'static [User] {
    static USERS: std::sync::OnceLock<Vec<User>> = std::sync::OnceLock::new();
    USERS.get_or_init(|| (1..=8u8).map(User::new).collect())
}

/// 순위가 자주 바뀌는 동시 상태 기록: 역할 만들기·등급 바꾸기·지우기·부여, 소유권 이전, 가입·추방·밴·밴 풀기·탈퇴.
/// 1대1 공간에서 시작하면 크루 전환 요청·수락(소유자와 관리자 역할이 한꺼번에 바뀜)과 소유자 탈퇴(남은 사람이
/// 소유자가 됨)도 섞습니다.
struct Churn {
    events: Vec<DecodedEvent>,
    /// 끝내 본문 없이 순서 연결로만 아는 것.
    links: Vec<LinkEvent>,
    /// `events` 가운데 먼저 순서 연결로 넣었다가 나중에 본문을 넣을 것의 번호.
    upgrade: Vec<usize>,
}

/// 셋에 하나는 1대1 공간에서 시작합니다.
fn churn(seed: u64, n: usize) -> Churn {
    churn_kind(seed, n, seed.is_multiple_of(3))
}

fn churn_kind(seed: u64, n: usize, friend: bool) -> Churn {
    let mut rng = Rng(seed);
    let users = churn_users();
    let mut w = World::new();
    let create = if friend {
        create_body(
            SpaceKind::Space,
            &users[0],
            &[&users[0], &users[1]],
            FRIEND_SPACE_EVERYONE,
        )
    } else {
        let initial: Vec<&User> = users[..4].iter().collect();
        create_body(
            SpaceKind::Crew,
            &users[0],
            &initial,
            Permissions::DEFAULT_EVERYONE,
        )
    };
    let genesis = w.ev(&users[0], &[], create);
    let mut ids = vec![genesis.id];
    let mut c = Churn {
        events: vec![genesis],
        links: Vec::new(),
        upgrade: Vec::new(),
    };
    let roles = [RoleId([1; 16]), RoleId([2; 16]), RoleId([3; 16])];
    let perms = [
        Permissions::KICK_MEMBERS.union(Permissions::BAN_MEMBERS),
        Permissions::MANAGE_ROLES.union(Permissions::KICK_MEMBERS),
        Permissions::MANAGE_ROLES
            .union(Permissions::MANAGE_SPACE)
            .union(Permissions::BAN_MEMBERS),
        Permissions::ADMINISTRATOR,
    ];
    let some_role = |rng: &mut Rng| Role {
        id: roles[rng.below(3)],
        name: "역할".into(),
        color: 0,
        rank: 1 + rng.below(50) as u32,
        permissions: perms[rng.below(perms.len())],
        automation: false,
    };
    // 덧붙이는 사건(기기 목록 갱신, 해소기에 들지 않는 부분 전달 이벤트와 그것을 잘못 부모로 고른 상태 이벤트)은 따로
    // 뽑아 위의 뽑기 순서를 바꾸지 않습니다.
    let mut extra = Rng(seed ^ 0x5c0b_ed00_d00d_f00d);
    let mut scoped_ids: Vec<EventId> = Vec::new();
    // 크루 키 갱신·예비 기기도 따로 뽑습니다. 순번은 이미 만든 갱신 머리를 잇게 골라 같은 순번 갈래, 두 조각 갱신, 빚
    // (자격을 잃은 멤버), 표 보유 기기 규칙이 함께 섞입니다.
    let mut keyr = Rng(seed ^ 0x6b65_7975_7064_6174);
    let mut key_heads: Vec<(u64, EventId)> = Vec::new();
    for _ in 0..n {
        let mut parents = Vec::new();
        if !scoped_ids.is_empty() && extra.below(8) == 0 {
            // 정상 작성자는 고르지 않지만, 골랐어도 어느 노드에서나 빠진 부모여야 합니다.
            parents.push(scoped_ids[extra.below(scoped_ids.len())]);
        }
        for _ in 0..1 + rng.below(3) {
            // 대개 최근 것(동시 갈래), 가끔 아주 옛것.
            let back = if rng.below(8) == 0 {
                rng.below(ids.len())
            } else {
                rng.below(ids.len().min(4))
            };
            parents.push(ids[ids.len() - 1 - back]);
        }
        if rng.below(16) == 0 {
            // 끝내 오지 않는 부모(충족된 것으로 봄).
            parents.push(EventId::hash_of(&rng.next().to_le_bytes()));
        }
        // 1대1이면 셋에 하나는 두 사람 가운데 하나가 전환 요청·수락, 드물게 탈퇴.
        let convert = friend && rng.below(3) == 0;
        let author = if convert {
            &users[rng.below(2)]
        } else if friend {
            match rng.below(8) {
                0..=5 => &users[rng.below(2)],
                _ => &users[2 + rng.below(6)],
            }
        } else {
            match rng.below(8) {
                0..=2 => &users[0],
                3..=5 => &users[1 + rng.below(3)],
                _ => &users[4 + rng.below(4)],
            }
        };
        let target = users[rng.below(users.len())].id();
        let channel_update = |rng: &mut Rng| EventBody::ChannelUpdate {
            channel: ch(1 + rng.below(4) as u8),
            settings: ChannelSettings::text("바꾼 채널"),
        };
        let body = if convert {
            match rng.below(20) {
                0..=7 => EventBody::CrewConvertRequest {
                    name: String::new(),
                },
                8..=15 => EventBody::CrewConvertAccept,
                16 | 17 => EventBody::MemberLeave { user: author.id() },
                _ => channel_update(&mut rng),
            }
        } else {
            match rng.below(17) {
                0 | 1 => EventBody::RoleCreate {
                    role: some_role(&mut rng),
                },
                2 | 3 => EventBody::RoleUpdate {
                    role: some_role(&mut rng),
                },
                4 => EventBody::RoleDelete {
                    role: roles[rng.below(3)],
                },
                5..=7 => {
                    let mut rs: Vec<RoleId> = roles
                        .iter()
                        .filter(|_| rng.below(2) == 0)
                        .copied()
                        .collect();
                    rs.sort();
                    EventBody::MemberUpdate {
                        user: target,
                        roles: rs,
                        nickname: None,
                    }
                }
                8 if rng.below(3) == 0 => transfer(author, target),
                8..=10 => users[1 + rng.below(7)].join_body_v(None, 1 + rng.below(3) as u64),
                11 => EventBody::MemberKick {
                    user: target,
                    reason: String::new(),
                },
                12 => EventBody::MemberBan {
                    user: target,
                    reason: String::new(),
                    delete_since: None,
                },
                13 => EventBody::MemberUnban { user: target },
                // 1대1에서 탈퇴는 공간을 끝내므로 위에서 드물게만.
                14 if !friend => EventBody::MemberLeave { user: author.id() },
                // 만들기와 바꾸기가 섞이면 순서가 달라졌을 때 결과에 드러납니다.
                _ if friend && rng.below(2) == 0 => channel_update(&mut rng),
                _ => text_channel(ch(1 + rng.below(4) as u8), "채널"),
            }
        };
        let lamport = w.lamport_after(&parents) + rng.below(2) as u64;
        let e = w.ev_at(author.id(), author.dev(), &parents, lamport, body);
        ids.push(e.id);
        match rng.below(12) {
            // 본문을 기다리는 순서 연결과, 본문 없이 받아 풀릴 일이 없는 순서 연결(기준점 상태가 기다리지 않음).
            0 if rng.below(2) == 0 => c.links.push(LinkEvent::from_header(e.id, &e.header)),
            0 => c.links.push(LinkEvent::without_body(e.id, &e.header)),
            1 | 2 => {
                c.upgrade.push(c.events.len());
                c.events.push(e);
            }
            _ => c.events.push(e),
        }
        if keyr.below(4) == 0 {
            key_churn(&mut c, &mut w, &mut keyr, &mut key_heads, &ids, friend);
        }
        if extra.below(6) == 0 {
            // 기기 목록 갱신(해소기는 간추린 기록만 들고 있음): 판을 올리거나 되돌림. 덧붙인 것은 부모 후보(`ids`)에
            // 넣지 않아 위 뽑기로 만든 모양을 바꾸지 않습니다.
            let parent = [*ids.last().expect("시작 이벤트")];
            let d = w.ev_at(
                author.id(),
                author.dev(),
                &parent,
                w.lamport_after(&parent),
                EventBody::DeviceListUpdate {
                    devices: author
                        .list_with(1 + extra.below(4) as u64, vec![User::entry(&author.device)]),
                },
            );
            c.events.push(d);
        }
        if extra.below(6) == 0 {
            // 부분 전달 이벤트(받은편지함·키 그룹): 해소기는 받지 않고, 기준점은 최근 공간 스트림 이벤트.
            let to = &users[extra.below(users.len())];
            let anchor = vec![ids[ids.len() - 1 - extra.below(ids.len().min(4))]];
            let lamport = w.lamport_after(&anchor);
            let mut k = w.ev_at(
                author.id(),
                author.dev(),
                &[],
                lamport,
                EventBody::KeyShare(KeyShareBody {
                    keys: vec![],
                    for_user: Some(to.id()),
                }),
            );
            k.header.key = KeyRef::Sealed;
            k.header.scope = Some(if extra.below(2) == 0 {
                Scope::Inbox(to.id())
            } else {
                Scope::KeyGroup(KeyGroupId::private(&SPACE, b"rule"))
            });
            k.header.state_anchor = anchor;
            scoped_ids.push(k.id);
            c.events.push(k);
        }
    }
    c
}

/// 무작위 기록에 크루 키 사건 하나를 덧붙입니다: 예비 기기 정하기, 또는 앞서 만든 갱신 머리를 잇는 `KeyUpdate`(가끔
/// 두 조각). 부모는 가장 최근 상태 이벤트와 잇는 갱신 머리입니다.
fn key_churn(
    c: &mut Churn,
    w: &mut World,
    rng: &mut Rng,
    heads: &mut Vec<(u64, EventId)>,
    ids: &[EventId],
    friend: bool,
) {
    let users = churn_users();
    // 대개 처음 확정 기기(소유자의 기기), 가끔 다른 사람.
    let author = &users[if rng.below(4) == 0 {
        1 + rng.below(3)
    } else {
        0
    }];
    let mut parents = vec![*ids.last().expect("시작 이벤트")];
    if rng.below(6) == 0 {
        let device = (rng.below(3) != 0).then(|| users[rng.below(4)].dev());
        let lamport = w.lamport_after(&parents);
        let e = w.ev_at(
            author.id(),
            author.dev(),
            &parents,
            lamport,
            EventBody::KeyBackupSet { device },
        );
        c.events.push(e);
        return;
    }
    let (seq, prev) = match heads.len() {
        0 => (1, None),
        _ if rng.below(8) == 0 => (1, None),
        n => {
            let (s, h) = heads[n - 1 - rng.below(n.min(3))];
            (s + 1, Some(h))
        }
    };
    parents.extend(prev);
    let pick = |rng: &mut Rng| -> Vec<DeviceId> {
        let mut v: Vec<DeviceId> = users
            .iter()
            .filter(|_| rng.below(4) == 0)
            .map(|u| u.dev())
            .collect();
        v.sort();
        v
    };
    let initial = if friend { 2 } else { 4 };
    let kind = match rng.below(7) {
        _ if seq == 1 && rng.below(2) == 0 => KeyUpdateKind::Advance {
            joined: users[..initial].iter().map(|u| u.dev()).collect(),
        },
        0 | 1 => KeyUpdateKind::Advance { joined: pick(rng) },
        2 => KeyUpdateKind::Refresh,
        3 => KeyUpdateKind::RRefresh,
        4 | 5 => {
            let removed = pick(rng);
            let joined = pick(rng)
                .into_iter()
                .filter(|d| !removed.contains(d))
                .collect();
            KeyUpdateKind::Rotate { removed, joined }
        }
        _ => KeyUpdateKind::FullRekey {
            generation: 0,
            removed: pick(rng),
            joined: Vec::new(),
        },
    };
    // 빈 목록의 가입·회전은 형식 검사(`KeyUpdatePayload::validate`)가 거르는 모양이라 만들지 않습니다.
    let kind = match kind {
        KeyUpdateKind::Advance { joined } if joined.is_empty() => KeyUpdateKind::Refresh,
        KeyUpdateKind::Rotate { removed, joined } if removed.is_empty() && joined.is_empty() => {
            KeyUpdateKind::Refresh
        }
        k => k,
    };
    let mut p = crew_update(seq, prev, kind);
    // 두 조각: 가입 목록 뒤쪽 반을 이어지는 조각으로(첫 조각에는 가입 기기가 하나는 남음).
    let split = rng.below(4) == 0;
    let mut tail = None;
    if split {
        p.parts = 2;
        let mut t = p.clone();
        t.part = 1;
        t.r_ct = None;
        if let (KeyUpdateKind::Advance { joined }, KeyUpdateKind::Advance { joined: rest }) =
            (&mut p.kind, &mut t.kind)
        {
            *rest = joined.split_off(joined.len().div_ceil(2));
        } else if let KeyUpdateKind::Rotate { removed, joined }
        | KeyUpdateKind::FullRekey {
            removed, joined, ..
        } = &mut t.kind
        {
            removed.clear();
            joined.clear();
        }
        tail = Some(t);
    }
    let lamport = w.lamport_after(&parents) + rng.below(2) as u64;
    let head = w.ev_at(
        author.id(),
        author.dev(),
        &parents,
        lamport,
        EventBody::KeyUpdate(p),
    );
    let head_id = head.id;
    heads.push((seq, head_id));
    c.events.push(head);
    if let Some(t) = tail {
        let lamport = w.lamport_after(&[head_id]);
        let e = w.ev_at(
            author.id(),
            author.dev(),
            &[head_id],
            lamport,
            EventBody::KeyUpdate(t),
        );
        if rng.below(3) == 0 {
            c.upgrade.push(c.events.len());
        }
        c.events.push(e);
    }
}

/// 넣을 묶음들. 대개 논리 시계 순(증분 경로)이되 다섯에 하나는 늦게 넣어 빈 구간 채움·동시 이벤트(재생 경로)를
/// 만들고, 넷에 하나는 전부 섞습니다. 본문을 나중에 받을 이벤트는 순서 연결을 먼저 넣습니다.
fn churn_batches(c: &Churn, rng: &mut Rng) -> Vec<(Vec<DecodedEvent>, Vec<LinkEvent>)> {
    enum Step {
        Event(usize),
        Link(LinkEvent),
    }
    let mut steps: Vec<(u64, Step)> = c
        .events
        .iter()
        .enumerate()
        .map(|(i, e)| (e.header.lamport, Step::Event(i)))
        .chain(c.links.iter().map(|l| (l.lamport, Step::Link(l.clone()))))
        .collect();
    steps.sort_by_key(|(l, _)| *l);
    let shuffle = rng.below(4) == 0;
    let mut keyed: Vec<(u64, usize, Step)> = Vec::new();
    for (k, (_, s)) in steps.into_iter().enumerate() {
        let base = if shuffle {
            rng.next() % (1 << 20)
        } else {
            k as u64 * 4
        };
        let at = base
            + if rng.below(5) == 0 {
                4 * (1 + rng.below(12)) as u64
            } else {
                0
            };
        if let Step::Event(i) = s
            && c.upgrade.contains(&i)
        {
            let e = &c.events[i];
            let j = keyed.len();
            keyed.push((at, j, Step::Link(LinkEvent::from_header(e.id, &e.header))));
            let later = at + 4 * (1 + rng.below(16)) as u64;
            keyed.push((later, j + 1, s));
        } else {
            let j = keyed.len();
            keyed.push((at, j, s));
        }
    }
    keyed.sort_by_key(|(at, j, _)| (*at, *j));
    let mut out = Vec::new();
    let mut i = 0;
    while i < keyed.len() {
        let take = 1 + rng.below(3);
        let (mut evs, mut lns) = (Vec::new(), Vec::new());
        for (_, _, s) in &keyed[i..(i + take).min(keyed.len())] {
            match s {
                Step::Event(j) => evs.push(c.events[*j].clone()),
                Step::Link(l) => lns.push(l.clone()),
            }
        }
        out.push((evs, lns));
        i += take;
    }
    out
}

#[test]
fn churn_is_meaningful() {
    // 생성기가 역할·순위 변화와 재생을 충분히 만드는지(속성 검사가 헛돌지 않게).
    let c = churn_kind(777, 200, false);
    let r = resolve_with(&c.events, &c.links, None);
    let kinds: BTreeMap<String, usize> = c
        .events
        .iter()
        .filter(|e| r.applied.contains(&e.id))
        .fold(BTreeMap::new(), |mut m, e| {
            *m.entry(format!("{:?}", e.header.kind)).or_default() += 1;
            m
        });
    for k in [
        "RoleCreate",
        "RoleUpdate",
        "MemberUpdate",
        "MemberJoin",
        "MemberKick",
        "MemberBan",
        "DeviceListUpdate",
        "KeyUpdate",
    ] {
        assert!(kinds.contains_key(k), "{k}가 적용되지 않음: {kinds:?}");
    }
    // 크루 키 갱신이 사슬로 이어지고(순번 2 이상), 거부도 섞입니다.
    assert!(r.key_events.len() >= 2, "키 이벤트 {:?}", r.key_events);
    let keys = r.keys.as_ref().expect("키 사슬");
    assert!(keys.crew.seq >= 2, "크루 순번 {}", keys.crew.seq);
    assert!(
        c.events
            .iter()
            .any(|e| e.header.kind == EventKind::KeyUpdate && !r.applied.contains(&e.id))
    );
    // 여러 기록에 걸쳐 두 조각 갱신이 다 모여 받아들여지고, 같은 순번 갈래는 하나만 받아들여집니다.
    let (mut split_done, mut dup_seq) = (0, 0);
    for seed in 0..20 {
        let c = churn_kind(seed, 120, false);
        let r = resolve_with(&c.events, &c.links, None);
        for e in &c.events {
            let EventBody::KeyUpdate(p) = &e.body else {
                continue;
            };
            if p.part == 1 && r.key_events.contains(&e.id) {
                split_done += 1;
            }
            if rejection(&r, e.id) == Some(AuthError::DuplicateCommit) {
                dup_seq += 1;
            }
        }
    }
    assert!(
        split_done > 0 && dup_seq > 0,
        "두 조각 {split_done}, 같은 순번 {dup_seq}"
    );
    // 부분 전달 이벤트와, 그것을 (잘못) 부모로 고른 상태 이벤트도 생깁니다.
    let scoped: HashSet<EventId> = c
        .events
        .iter()
        .filter(|e| e.header.scope.is_some())
        .map(|e| e.id)
        .collect();
    assert!(!scoped.is_empty());
    assert!(
        c.events
            .iter()
            .any(|e| e.header.parents.iter().any(|p| scoped.contains(p)))
    );
    let mut res = Resolver::new();
    let mut rng = Rng(5);
    for (evs, lns) in churn_batches(&c, &mut rng) {
        res.insert(evs, lns);
    }
    let st = res.stats();
    assert!(st.replay > 0 && st.incremental > 0, "{st:?}");
    assert_eq!(res.resolved(), r);

    // 1대1에서 시작한 기록은 크루 전환(소유자·관리자 역할이 한꺼번에 바뀜)과 1대1 소유자 탈퇴(남은 사람이 소유자)를
    // 실제로 적용하고, 전환 뒤에는 크루 쪽 변화도 이어집니다.
    let users = churn_users();
    let (mut converted, mut owner_left, mut crew_kinds) = (0, 0, BTreeMap::<String, usize>::new());
    for seed in 0..30 {
        let c = churn_kind(seed, 120, true);
        let r = resolve_with(&c.events, &c.links, None);
        let st = state(&r);
        if st.kind == SpaceKind::Crew {
            converted += 1;
        } else if st.owner == users[1].id() {
            owner_left += 1;
        }
        for e in c.events.iter().filter(|e| r.applied.contains(&e.id)) {
            *crew_kinds
                .entry(format!("{:?}", e.header.kind))
                .or_default() += 1;
        }
    }
    assert!(
        converted >= 10 && owner_left >= 1,
        "전환 {converted}, 소유자 탈퇴 {owner_left}"
    );
    for k in [
        "CrewConvertAccept",
        "RoleCreate",
        "MemberUpdate",
        "MemberJoin",
        "ChannelUpdate",
    ] {
        assert!(
            crew_kinds.contains_key(k),
            "{k}가 적용되지 않음: {crew_kinds:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 순위가 바뀐 범위만 다시 정렬해도, 적용마다 준비 집합 전체를 다시 정렬한 것과 해소 결과가 같습니다.
    #[test]
    fn targeted_rerank_equals_forced_all(seed in any::<u64>(), n in 1usize..150) {
        let c = churn(seed, n);
        let targeted = resolve_with(&c.events, &c.links, None);
        let forced = with_flag(&FORCE_RANK_ALL, || resolve_with(&c.events, &c.links, None));
        prop_assert_eq!(&targeted, &forced);
        let (events, links) = scenario(seed, n);
        let targeted = resolve_with(&events, &links, None);
        let forced = with_flag(&FORCE_RANK_ALL, || resolve_with(&events, &links, None));
        prop_assert_eq!(&targeted, &forced);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// 증분 해소기(이어 붙이기·스냅숏 재생)에 어떤 순서·묶음으로 넣어도, 넣을 때마다 알리는 변화가 늘 처음부터 다시
    /// 해소하는 해소기와 같고, 끝 결과는 모든 이벤트를 처음부터 해소한 것과 같습니다.
    #[test]
    fn incremental_equals_scratch(seed in any::<u64>(), n in 1usize..120, order_seed in any::<u64>()) {
        let c = churn(seed, n);
        let mut rng = Rng(order_seed);
        let genesis = c.events[0].id;
        let (mut fast, mut slow) = if order_seed % 4 == 0 {
            (Resolver::new(), Resolver::new())
        } else {
            (Resolver::with_genesis(genesis), Resolver::with_genesis(genesis))
        };
        for (evs, lns) in churn_batches(&c, &mut rng) {
            let a = fast.insert(evs.clone(), lns.clone());
            let b = with_flag(&NO_REPLAY, || slow.insert(evs, lns));
            prop_assert_eq!(&a, &b);
            fast.check_log();
        }
        let scratch = resolve_with(&c.events, &c.links, None);
        prop_assert_eq!(&fast.resolved(), &scratch);
        prop_assert_eq!(&slow.resolved(), &scratch);
        // 부분 전달 이벤트는 해소기에 들지 않아, 그것을 받지 못한 노드(빼고 해소)와 결과가 같습니다.
        let unscoped: Vec<DecodedEvent> =
            c.events.iter().filter(|e| e.header.scope.is_none()).cloned().collect();
        prop_assert_eq!(&resolve_with(&unscoped, &c.links, None), &scratch);
        prop_assert_eq!(fast.heads(), slow.heads());
        prop_assert_eq!(fast.missing(), slow.missing());
        prop_assert_eq!(slow.stats().replay, 0);
    }
}

/// 옛 부모를 고른 이벤트는 처음부터가 아니라 그 앞의 스냅숏에서 다시 해소합니다.
#[test]
fn resolver_replays_from_snapshot() {
    let mut cr = crew();
    let mut ids = vec![cr.genesis];
    for i in 0..40u8 {
        ids.push(cr.c.push(&cr.o, text_channel(ch(i + 1), "채널")));
    }
    let mut res = Resolver::with_genesis(cr.genesis);
    for e in cr.c.events.clone() {
        res.insert_events(vec![e]);
    }
    res.check_log();
    // a가 다섯 개 앞을 부모로 별명을 바꿈(동시 이벤트).
    let stale = cr.c.fork(
        &cr.a,
        &[ids[36]],
        EventBody::MemberUpdate {
            user: cr.a.id(),
            roles: vec![],
            nickname: Some("에이".into()),
        },
    );
    let u = res.insert_events(vec![cr.c.events.last().unwrap().clone()]);
    assert!(u.full);
    assert_eq!(u.applied, vec![stale]);
    assert!(u.state_changed);
    let st = res.stats();
    assert_eq!((st.full, st.replay), (0, 1), "{st:?}");
    // 끝에서 다섯 앞(p = 37)부터: 재생 길이는 거리의 몇 배 안.
    assert!(
        st.replayed <= 2 * (41 - 37 + SNAPSHOT_EVERY as u64),
        "{st:?}"
    );
    assert_eq!(res.resolved(), cr.c.resolve());
    res.check_log();
    // 두 머리를 합치는 다음 이벤트는 다시 증분.
    let merge =
        cr.c.w
            .ev(&cr.o, &[ids[40], stale], text_channel(ch(99), "합침"));
    cr.c.push_event(merge.clone());
    let u = res.insert_events(vec![merge]);
    assert!(!u.full);
    assert_eq!(res.resolved(), cr.c.resolve());
    // 빈 구간: 부모를 빼고 자식을 넣으면 자식은 처음부터 준비된 것으로 보므로 처음부터 다시 해소하고, 부모가 오면
    // 자식 자리부터 재생합니다.
    let p1 = cr.c.push(&cr.o, text_channel(ch(100), "부모"));
    let c1 = cr.c.push(&cr.o, text_channel(ch(101), "자식"));
    let n = cr.c.events.len();
    res.insert_events(vec![cr.c.events[n - 1].clone()]);
    assert_eq!(res.missing(), vec![p1]);
    assert_eq!(res.stats().full, 1);
    let u = res.insert_events(vec![cr.c.events[n - 2].clone()]);
    assert!(u.full);
    assert_eq!(u.applied, vec![p1]);
    assert!(res.applied().contains(&c1));
    let st = res.stats();
    assert_eq!((st.full, st.replay), (1, 2), "{st:?}");
    assert_eq!(res.resolved(), cr.c.resolve());
    res.check_log();
}

/// 동시 갈래를 그 갈래 끝 뒤로 이어 가면(다른 머리는 덮지 않음) 처리 기록 전체가 그대로라 스냅숏으로 돌아가지 않고 새
/// 항목만 처리합니다.
#[test]
fn resolver_extends_concurrent_branch_without_replay() {
    let mut cr = crew();
    let mut ids = vec![cr.genesis];
    for i in 0..10u8 {
        ids.push(cr.c.push(&cr.o, text_channel(ch(i + 1), "채널")));
    }
    let mut res = Resolver::with_genesis(cr.genesis);
    for e in cr.c.events.clone() {
        res.insert_events(vec![e]);
    }
    // a의 갈래: 셋째 채널 뒤에서 갈라져 별명을 세 번 바꿈. 소유자 이벤트가 먼저라 갈래는 늘 기록 끝에 처리됩니다.
    let mut parent = ids[3];
    for k in 0..3 {
        let id = cr.c.fork(
            &cr.a,
            &[parent],
            EventBody::MemberUpdate {
                user: cr.a.id(),
                roles: vec![],
                nickname: Some(format!("에이{k}")),
            },
        );
        let before = res.stats();
        let u = res.insert_events(vec![cr.c.events.last().unwrap().clone()]);
        assert!(u.full);
        assert_eq!(u.applied, vec![id]);
        let st = res.stats();
        assert_eq!((st.full, st.replay), (before.full, before.replay + 1));
        if k > 0 {
            assert_eq!(st.replayed, before.replayed + 1, "{st:?}");
        } else {
            assert!(st.replayed > before.replayed + 1, "{st:?}");
        }
        assert_eq!(res.resolved(), cr.c.resolve());
        res.check_log();
        parent = id;
    }
}

/// 스냅숏은 상한을 넘지 않고, 최근 것은 남으며, 간격이 끝에서의 거리에 비례해 재생 길이가 거리의 몇 배 안입니다.
#[test]
fn snapshots_thin_out_geometrically() {
    let mut snaps = Vec::new();
    let last = 200 * SNAPSHOT_EVERY;
    for k in 1..=200 {
        push_snapshot(
            &mut snaps,
            Snapshot {
                mark: Mark {
                    at: k * SNAPSHOT_EVERY,
                    ..Mark::default()
                },
                machine: None,
            },
        );
        assert!(snaps.len() <= MAX_SNAPSHOTS);
        assert_eq!(snaps.last().unwrap().mark.at, k * SNAPSHOT_EVERY);
    }
    let at: Vec<usize> = snaps.iter().map(|s| s.mark.at).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{at:?}");
    for d in 1..last {
        let start = at
            .iter()
            .rev()
            .find(|&&a| a <= last - d)
            .copied()
            .unwrap_or(0);
        assert!(last - start <= 4 * (d + SNAPSHOT_EVERY), "d={d}, {at:?}");
    }
}

/// 실제 상수(1024항목마다, 16개)로 300만 항목까지 솎아도, 끝에서 d만큼 앞에서 다시 해소할 때 재생 길이가 늘
/// 2.2(d + 1024) 안입니다(`MAX_SNAPSHOTS`의 문서). 약 90만 항목까지는 2배 안입니다.
#[test]
fn snapshot_bound_with_release_constants() {
    let (every, max) = (RELEASE_SNAPSHOT_EVERY, RELEASE_MAX_SNAPSHOTS);
    // 스냅숏 집합이 같은 동안 기록이 길어지면 재생 길이와 d가 같이 늘어 비가 줄므로, 집합마다 기록 길이가 가장 짧을
    // 때(마지막 스냅숏 위치)만 보면 됩니다. p가 [앞 스냅숏, 다음 스냅숏) 안이면 앞 스냅숏에서 다시 해소하고, 가장
    // 나쁜 p는 다음 스냅숏 바로 앞입니다.
    let worst = |snaps: &[Snapshot], len: usize| {
        let mut worst = 0f64;
        let mut start = 0;
        for next in snaps.iter().map(|s| s.mark.at).chain([len + 1]) {
            let p = (next - 1).min(len);
            if p >= start {
                let ratio = (len - start) as f64 / (len - p + every) as f64;
                worst = worst.max(ratio);
            }
            start = next;
        }
        worst
    };
    let mut snaps = Vec::new();
    let (mut upto_900k, mut upto_3m) = (0f64, 0f64);
    for k in 1..=3_000_000 / every {
        let at = k * every;
        let snap = Snapshot {
            mark: Mark {
                at,
                ..Mark::default()
            },
            machine: None,
        };
        push_snapshot_with(&mut snaps, snap, every, max);
        assert!(snaps.len() <= max);
        let w = worst(&snaps, at);
        upto_3m = upto_3m.max(w);
        if at <= 900_000 {
            upto_900k = upto_900k.max(w);
        }
    }
    assert!(upto_900k <= 2.0, "{upto_900k}");
    assert!(upto_3m <= 2.2, "{upto_3m}");
}

/// 멤버 수 상한은 해소기마다 바꿀 수 있고, 기본값은 상수입니다.
#[test]
fn member_limit_is_configurable() {
    assert_eq!(Limits::default().max_members, limits::MAX_MEMBERS);
    let mut cr = crew();
    let joiners: Vec<User> = (30..33u8).map(User::new).collect();
    let joins: Vec<EventId> = joiners
        .iter()
        .map(|u| cr.c.push(&cr.o, u.join_body(None)))
        .collect();
    let mut small = Resolver::with_genesis_limits(cr.genesis, Limits { max_members: 4 });
    small.insert_events(cr.c.events.clone());
    let r = small.resolved();
    assert_applied(&r, joins[0]);
    assert_rejected(&r, joins[1], AuthError::Limit("멤버 수"));
    assert_eq!(state(&r).members.len(), 4);
    // 공간 생성의 처음 멤버 수도 같은 상한.
    let mut tiny = Resolver::with_genesis_limits(cr.genesis, Limits { max_members: 2 });
    tiny.insert_events(cr.c.events.clone());
    assert_eq!(tiny.resolved().state, None);
    let mut default = Resolver::with_genesis(cr.genesis);
    default.insert_events(cr.c.events.clone());
    assert_eq!(default.resolved(), cr.c.resolve());
    assert_eq!(default.state().unwrap().members.len(), 6);
}

/// 1대1 공간에서 소유자가 나가면 남은 사람이 소유자 순위가 되므로 두 사람 모두 순위 변화로 알립니다.
#[test]
fn owner_leaving_friend_space_reranks_both() {
    let (o, f) = (User::new(1), User::new(2));
    let mut w = World::new();
    let genesis = w.ev(
        &o,
        &[],
        create_body(SpaceKind::Space, &o, &[&o, &f], FRIEND_SPACE_EVERYONE),
    );
    let leave = w.ev(&o, &[genesis.id], EventBody::MemberLeave { user: o.id() });
    let memo = SigMemo::default();
    let mut m = Machine::create(&Stored::new(genesis), None, Limits::default(), &memo).unwrap();
    m.apply(&Stored::new(leave), &memo, &NoDag).unwrap();
    let RankChange::Users(mut users) = m.take_rank_change() else {
        panic!("사용자 범위가 아님");
    };
    users.sort();
    let mut want = vec![o.id(), f.id()];
    want.sort();
    assert_eq!(users, want);
    assert_eq!(m.st.owner, f.id());
}

/// 순위가 바뀐 뒤 같은 사람의 두 준비 이벤트: 바뀌기 전에 준비된 `early`(논리 시계가 작음)와 바뀐 뒤 준비된
/// `late`. 순위 변화를 남기지 않으면 `early`의 정렬 키가 옛 순위로 남아 `late`가 먼저 처리되고, `late`가 `early`가
/// 만든 채널을 바꾸므로 거부됩니다. 처음부터 해소·적용마다 전부 재정렬·증분 해소기가 모두 둘 다 적용해야 합니다.
#[track_caller]
fn assert_rerank_order(events: &[DecodedEvent], early: EventId, late: EventId) {
    let r = resolve(events);
    assert_applied(&r, early);
    assert_applied(&r, late);
    let forced = with_flag(&FORCE_RANK_ALL, || resolve(events));
    assert_eq!(r, forced);
    // 하나씩 넣어도(이어 붙이기·재생) 같습니다.
    let mut res = Resolver::new();
    for e in events {
        res.insert_events(vec![e.clone()]);
    }
    assert_eq!(res.resolved(), r);
}

/// 크루 전환을 수락한 사람은 관리자 역할을 받아 순위가 오르므로, 수락 전에 준비된 그 사람의 이벤트도 다시 정렬합니다.
#[test]
fn crew_conversion_reranks_ready_events() {
    let (o, f) = (User::new(1), User::new(2));
    let mut w = World::new();
    let g = w.ev(
        &o,
        &[],
        create_body(SpaceKind::Space, &o, &[&o, &f], FRIEND_SPACE_EVERYONE),
    );
    let req = w.ev(
        &o,
        &[g.id],
        EventBody::CrewConvertRequest {
            name: String::new(),
        },
    );
    // 요청 뒤 준비 집합: 수락(f, 논리 시계 3)과 early(f, 4). 순위가 같아 수락이 먼저입니다.
    let accept = w.ev_at(f.id(), f.dev(), &[req.id], 3, EventBody::CrewConvertAccept);
    let early = w.ev_at(f.id(), f.dev(), &[req.id], 4, text_channel(ch(1), "먼저"));
    let late = w.ev_at(
        f.id(),
        f.dev(),
        &[accept.id],
        5,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: ChannelSettings::text("나중"),
        },
    );
    let (early_id, late_id) = (early.id, late.id);
    let events = [g, req, accept, early, late];
    assert_rerank_order(&events, early_id, late_id);
    let r = resolve(&events);
    assert_eq!(state(&r).kind, SpaceKind::Crew);
    assert_eq!(state(&r).channels[&ch(1)].name, "나중");
}

/// 1대1 공간에서 소유자가 나가면 남은 사람이 소유자 순위가 되므로, 그 전에 준비된 남은 사람의 이벤트도 다시 정렬합니다.
#[test]
fn friend_owner_leave_reranks_ready_events() {
    let (o, f) = (User::new(1), User::new(2));
    let mut w = World::new();
    let g = w.ev(
        &o,
        &[],
        create_body(SpaceKind::Space, &o, &[&o, &f], FRIEND_SPACE_EVERYONE),
    );
    // 시작 뒤 준비 집합: 탈퇴(o, 소유자)와 early(f, 3). 소유자가 먼저입니다.
    let leave = w.ev(&o, &[g.id], EventBody::MemberLeave { user: o.id() });
    let early = w.ev_at(f.id(), f.dev(), &[g.id], 3, text_channel(ch(1), "먼저"));
    let late = w.ev_at(
        f.id(),
        f.dev(),
        &[leave.id],
        4,
        EventBody::ChannelUpdate {
            channel: ch(1),
            settings: ChannelSettings::text("나중"),
        },
    );
    let (early_id, late_id) = (early.id, late.id);
    let events = [g, leave, early, late];
    assert_rerank_order(&events, early_id, late_id);
    let r = resolve(&events);
    assert_eq!(state(&r).owner, f.id());
    assert_eq!(state(&r).channels[&ch(1)].name, "나중");
}

/// 크루(소유자 o: 역할 5, a: 역할 30·5, b: 역할 20, 모두 채널 관리)의 머리에서 동시에: o의 `update`(논리 시계 9),
/// `creator`의 채널 만들기(11), `updater`의 같은 채널 바꾸기(10). `update`가 먼저 처리되고, 그 뒤 두 이벤트의 순서는
/// 바뀐 순위로 정해집니다(바꾸기가 먼저면 채널이 없어 거부). 처음부터 해소·적용마다 전부 재정렬·하나씩 넣는 증분
/// 해소기가 같은지 보고, 바꾸기가 적용됐는지 돌려줍니다.
fn rank_race(
    update: impl FnOnce(&[User], &[Role]) -> EventBody,
    creator: usize,
    updater: usize,
) -> bool {
    let users: Vec<User> = (1..=4u8).map(User::new).collect();
    let [o, a, b, _] = &users[..] else {
        unreachable!()
    };
    let manage = Permissions::MANAGE_CHANNELS;
    let roles = [
        role(1, 30, manage),
        role(2, 20, manage),
        role(3, 5, manage),
        role(4, 40, manage),
    ];
    let [hi, mid, lo, _] = &roles;
    let mut c = Chain::new();
    c.push(
        o,
        create_body(
            SpaceKind::Crew,
            o,
            &[o, a, b],
            Permissions::DEFAULT_EVERYONE,
        ),
    );
    for r in &roles {
        c.push(o, EventBody::RoleCreate { role: r.clone() });
    }
    for (u, rs) in [(a, vec![hi.id, lo.id]), (b, vec![mid.id]), (o, vec![lo.id])] {
        c.push(
            o,
            EventBody::MemberUpdate {
                user: u.id(),
                roles: rs,
                nickname: None,
            },
        );
    }
    let head = c.head.unwrap();
    let u =
        c.w.ev_at(o.id(), o.dev(), &[head], 9, update(&users, &roles));
    let (cu, uu) = (&users[creator], &users[updater]);
    let create =
        c.w.ev_at(cu.id(), cu.dev(), &[head], 11, text_channel(ch(9), "만듦"));
    let change = c.w.ev_at(
        uu.id(),
        uu.dev(),
        &[head],
        10,
        EventBody::ChannelUpdate {
            channel: ch(9),
            settings: ChannelSettings::text("바꿈"),
        },
    );
    let (u_id, create_id, change_id) = (u.id, create.id, change.id);
    for e in [u, create, change] {
        c.push_event(e);
    }
    let r = c.resolve();
    assert_applied(&r, u_id);
    assert_applied(&r, create_id);
    assert_eq!(r, with_flag(&FORCE_RANK_ALL, || c.resolve()));
    let mut res = Resolver::new();
    for e in &c.events {
        res.insert_events(vec![e.clone()]);
    }
    assert_eq!(res.resolved(), r);
    applied(&r, change_id)
}

/// 사용자·역할 목록으로 순위를 바꾸는 이벤트 본문을 만듭니다.
type RankUpdate = Box<dyn FnOnce(&[User], &[Role]) -> EventBody>;

/// 순위를 바꾸는 이벤트마다(역할 등급 올림·내림, 역할 지우기, 역할 부여·회수, 소유권 이전의 새·옛 소유자, 역할을 받은
/// 가입), 그 전에 준비된 이벤트의 순서가 바뀐 순위를 따릅니다. 범위를 하나라도 남기지 않으면 옛 순위로 처리됩니다.
#[test]
fn every_rank_change_reorders_ready_events() {
    let (o, a, b, c) = (0, 1, 2, 3);
    // a가 b 아래로: b의 바꾸기가 먼저라 거부.
    let down: [(&str, RankUpdate); 3] = [
        (
            "역할 등급 내림",
            Box::new(move |_, r| EventBody::RoleUpdate {
                role: Role {
                    rank: 10,
                    ..r[0].clone()
                },
            }),
        ),
        (
            "역할 지우기",
            Box::new(move |_, r| EventBody::RoleDelete { role: r[0].id }),
        ),
        (
            "역할 회수",
            Box::new(move |u, r| EventBody::MemberUpdate {
                user: u[a].id(),
                roles: vec![r[2].id],
                nickname: None,
            }),
        ),
    ];
    for (what, update) in down {
        assert!(!rank_race(update, a, b), "{what}");
    }
    // b가 a 위로: b의 만들기가 먼저라 a의 바꾸기 적용.
    let up: [(&str, RankUpdate); 3] = [
        (
            "역할 등급 올림",
            Box::new(move |_, r| EventBody::RoleUpdate {
                role: Role {
                    rank: 40,
                    ..r[1].clone()
                },
            }),
        ),
        (
            "역할 부여",
            Box::new(move |u, r| EventBody::MemberUpdate {
                user: u[b].id(),
                roles: vec![r[1].id, r[3].id],
                nickname: None,
            }),
        ),
        (
            "새 소유자",
            Box::new(move |u, _| transfer(&u[0], u[b].id())),
        ),
    ];
    for (what, update) in up {
        assert!(rank_race(update, b, a), "{what}");
    }
    // 옛 소유자 o가 a 아래로(역할 5만 남음): a의 바꾸기가 먼저라 거부.
    assert!(!rank_race(|u, _| transfer(&u[0], u[b].id()), o, a));
    // 역할 40을 받고 들어온 c가 a 위로: c의 만들기가 먼저.
    assert!(rank_race(
        |u, r| {
            let mut body = u[c].join_body(None);
            if let EventBody::MemberJoin { member, .. } = &mut body {
                member.roles = vec![r[3].id];
            }
            body
        },
        c,
        a
    ));
}

// ---- 보관 전용 노드 ----

fn archive_grant(node: &User, private_channels: Vec<ChannelId>) -> EventBody {
    EventBody::ArchiveGrant(yangpa_core::archive::ArchiveGrant {
        device: node.dev(),
        user: node.id(),
        name: "보관함".into(),
        contact: yangpa_core::links::ContactInfo {
            peer_id: vec![1; 38],
            relay_addrs: vec![],
            onion: None,
        },
        private_channels,
    })
}

#[test]
fn archive_grant_and_revoke_rules() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let (arch, arch2, carol) = (User::new(20), User::new(21), User::new(22));
    let secret = ch(1);
    let vol = ch(2);
    let mut secret_settings = ChannelSettings::text("운영진");
    secret_settings.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Role(RoleId::EVERYONE),
        allow: Permissions::NONE,
        deny: Permissions::VIEW_CHANNEL,
    }];
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: secret,
            settings: secret_settings,
        },
    );
    let mut vol_settings = ChannelSettings::text("휘발");
    vol_settings.security = SecurityLevel::Volatile;
    cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: vol,
            settings: vol_settings,
        },
    );
    cr.c.push(o, text_channel(ch(3), "일반"));
    // a는 MANAGE_SPACE가 있지만 비공개 채널은 볼 수 없음.
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
    let by_member = cr.c.push(b, archive_grant(&arch, vec![]));
    let member_as_archive = cr.c.push(o, archive_grant(b, vec![]));
    let not_private = cr.c.push(o, archive_grant(&arch, vec![ch(3)]));
    let volatile_listed = cr.c.push(o, archive_grant(&arch, vec![vol]));
    let unknown = cr.c.push(o, archive_grant(&arch, vec![ch(9)]));
    let cannot_view = cr.c.push(a, archive_grant(&arch, vec![secret]));
    let mut own_ip = archive_grant(&arch, vec![]);
    if let EventBody::ArchiveGrant(g) = &mut own_ip {
        g.contact.relay_addrs = vec!["/ip4/1.2.3.4/tcp/1".into()];
    }
    let own_ip = cr.c.push(o, own_ip);
    let ok = cr.c.push(a, archive_grant(&arch, vec![]));
    let with_secret = cr.c.push(o, archive_grant(&arch, vec![secret]));
    let second = cr.c.push(o, archive_grant(&arch2, vec![]));
    let revoke_by_member = cr.c.push(
        b,
        EventBody::ArchiveRevoke {
            device: arch2.dev(),
        },
    );
    let revoke = cr.c.push(
        a,
        EventBody::ArchiveRevoke {
            device: arch2.dev(),
        },
    );
    let revoke_again = cr.c.push(
        a,
        EventBody::ArchiveRevoke {
            device: arch2.dev(),
        },
    );
    cr.c.push(o, ban(&carol, "", None));
    let banned = cr.c.push(o, archive_grant(&carol, vec![]));
    let r = cr.c.resolve();
    assert_rejected(
        &r,
        by_member,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE),
    );
    assert_rejected(
        &r,
        member_as_archive,
        AuthError::Invalid("보관 노드가 멤버임"),
    );
    for id in [not_private, volatile_listed] {
        assert_rejected(&r, id, AuthError::Invalid("보관할 수 없는 비공개 채널"));
    }
    assert_rejected(&r, unknown, AuthError::UnknownChannel);
    assert_rejected(
        &r,
        cannot_view,
        AuthError::MissingPermission(Permissions::VIEW_CHANNEL),
    );
    assert_rejected(&r, own_ip, AuthError::Invalid("보관 노드 접속 정보"));
    assert_rejected(&r, banned, AuthError::Banned);
    for id in [ok, with_secret, second, revoke] {
        assert_applied(&r, id);
    }
    assert_rejected(
        &r,
        revoke_by_member,
        AuthError::MissingPermission(Permissions::MANAGE_SPACE),
    );
    assert_rejected(
        &r,
        revoke_again,
        AuthError::Invalid("지정되지 않은 보관 노드"),
    );
    let st = state(&r);
    assert_eq!(st.archives.len(), 1);
    let e = &st.archives[&arch.dev()];
    assert_eq!(e.grant.private_channels, vec![secret]);
    assert_eq!(e.granted_by, o.id());

    // 보관 노드의 사용자가 멤버가 되면 지정이 사라짐.
    let join = cr.c.push(o, arch.join_body(None));
    let r = cr.c.resolve();
    assert_applied(&r, join);
    assert!(state(&r).archives.is_empty());
}

#[test]
fn archive_grant_limits_and_kind() {
    let mut cr = crew();
    let o = &cr.o;
    let nodes: Vec<User> = (30..30 + yangpa_core::archive::MAX_ARCHIVES as u8 + 1)
        .map(User::new)
        .collect();
    let ids: Vec<EventId> = nodes
        .iter()
        .map(|n| cr.c.push(o, archive_grant(n, vec![])))
        .collect();
    // 이미 지정된 기기는 꽉 차도 바꿀 수 있음.
    let update = cr.c.push(o, archive_grant(&nodes[0], vec![]));
    let r = cr.c.resolve();
    for id in &ids[..yangpa_core::archive::MAX_ARCHIVES] {
        assert_applied(&r, *id);
    }
    assert_rejected(&r, *ids.last().unwrap(), AuthError::Limit("보관 노드 수"));
    assert_applied(&r, update);

    let (o, a) = (User::new(1), User::new(2));
    let mut c = Chain::new();
    c.push(
        &o,
        create_body(SpaceKind::Space, &o, &[&o, &a], Permissions::NONE),
    );
    let in_friend_space = c.push(&o, archive_grant(&User::new(40), vec![]));
    let r = c.resolve();
    assert_rejected(&r, in_friend_space, AuthError::WrongSpaceKind);
}

// ---- 부분 전달 스트림(받은편지함·키 그룹) ----

/// `to` 앞으로 봉인한 키 묶음. `inbox_anchor`가 있으면 받은편지함 이벤트, 없으면 예전처럼 공간 스트림의 순서 연결.
fn key_share_to(
    w: &mut World,
    author: &User,
    parents: &[EventId],
    to: &User,
    inbox_anchor: Option<Vec<EventId>>,
) -> DecodedEvent {
    let body = EventBody::KeyShare(KeyShareBody {
        keys: vec![],
        for_user: Some(to.id()),
    });
    let mut e = w.ev(author, parents, body);
    e.header.key = KeyRef::Sealed;
    if let Some(anchor) = inbox_anchor {
        e.header.scope = Some(Scope::Inbox(to.id()));
        e.header.state_anchor = anchor;
    }
    e
}

/// 회귀: 일부 노드만 가진 키 묶음이 상태 이벤트의 처리 순서를 갈라서는 안 됩니다.
///
/// 예전에는 가입자 앞 키 묶음 K가 공간 스트림의 순서 연결이었습니다. K를 부모로 둔 소유자의 추방 S는 K를 가진
/// 노드에서는 가입 → K → S 순서라 적용되고, K를 못 받은 노드에서는(빠진 부모는 충족된 것으로 봄) 소유자 순위 덕에
/// 가입보다 먼저 처리돼 거부됐습니다. K를 받는 사람에게만 보내면 이 갈림이 정상 동작에서도 생깁니다. 이제 K는
/// 받은편지함 이벤트라 해소기가 받지 않고, 그것을 가리킨 부모는 어느 노드에서나 똑같이 빠진 부모입니다.
#[test]
fn inbox_key_share_cannot_split_state() {
    let mut cr = crew();
    let (o, a) = (&cr.o, &cr.a);
    let carol = User::new(4);
    // a는 직접 가입시킬 수 있지만 순위는 소유자보다 낮음.
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
    let join = cr.c.push(a, carol.join_body(None));
    let base = cr.c.events.clone();
    let kick = || EventBody::MemberKick {
        user: carol.id(),
        reason: String::new(),
    };

    // 대조군: 예전 형식(공간 스트림의 키 묶음)이면 K가 있는지에 따라 결과가 갈립니다.
    let old_k = key_share_to(&mut cr.c.w, o, &[join], &carol, None);
    let old_s = cr.c.w.ev(o, &[old_k.id], kick());
    let with_k = resolve(&[base.clone(), vec![old_k, old_s.clone()]].concat());
    let without_k = resolve(&[base.clone(), vec![old_s.clone()]].concat());
    assert_applied(&with_k, old_s.id);
    assert_rejected(&without_k, old_s.id, AuthError::UnknownMember);
    assert_ne!(with_k.state, without_k.state);

    // 지금 형식: K는 받은편지함 이벤트(기준점 = 가입 시점의 공간 스트림 머리, 부모는 같은 스트림에서만).
    let k = key_share_to(&mut cr.c.w, o, &[], &carol, Some(vec![join]));
    k.header.check_shape().unwrap();
    assert_eq!(k.header.stream(), StreamId::Inbox(carol.id()));
    // 정상 작성자는 부분 전달 이벤트를 부모로 고르지 않지만, 골랐더라도 노드마다 갈리지 않아야 합니다.
    let s = cr.c.w.ev(o, &[k.id], kick());
    let has_k = resolve(&[base.clone(), vec![k.clone(), s.clone()]].concat());
    let no_k = resolve(&[base.clone(), vec![s.clone()]].concat());
    assert_eq!(has_k, no_k);
    // 증분 해소기도 같고, K는 순서 연결로도 들어가지 않습니다.
    let mut fast = Resolver::with_genesis(cr.genesis);
    fast.insert_events(base.clone());
    fast.insert_events(vec![k.clone()]);
    fast.insert_events(vec![s.clone()]);
    let mut slow = Resolver::with_genesis(cr.genesis);
    slow.insert_events(base);
    slow.insert_events(vec![s]);
    assert!(!fast.contains(&k.id));
    assert_eq!(fast.resolved(), slow.resolved());
    assert_eq!(fast.resolved(), no_k);
}

/// 부분 전달 이벤트는 기준점 상태로 검사합니다: 그 뒤에 무엇이 바뀌었든 같은 답.
#[test]
fn scoped_events_are_checked_at_anchor_state() {
    let mut cr = crew();
    let (o, a, b) = (&cr.o, &cr.a, &cr.b);
    let secret = ch(9);
    let mut settings = ChannelSettings::text("비밀");
    settings.overwrites = vec![PermissionOverwrite {
        target: OverwriteTarget::Role(RoleId::EVERYONE),
        allow: Permissions::NONE,
        deny: Permissions::VIEW_CHANNEL,
    }];
    let group = KeyGroupId::private(&SPACE, &settings.view_rule());
    let made = cr.c.push(
        o,
        EventBody::ChannelCreate {
            channel: secret,
            settings,
        },
    );
    let mut res = Resolver::with_genesis(cr.genesis);
    res.insert_events(cr.c.events.clone());
    let AnchorState::Ready {
        state: Some(st),
        gaps: false,
    } = res.state_at(&[made], false)
    else {
        panic!("기준점 상태가 서야 함");
    };
    let stranger = User::new(9);
    let header = |w: &mut World, author: &User, scope: Scope| {
        let body = EventBody::KeyShare(KeyShareBody {
            keys: vec![],
            for_user: None,
        });
        let mut e = w.ev(author, &[], body);
        e.header.key = KeyRef::Sealed;
        e.header.scope = Some(scope);
        e.header.state_anchor = vec![made];
        e.header
    };
    let check = |h: &EventHeader| authorize_scoped(&st, &SPACE, h);
    // 확정 기기(소유자)는 누구의 받은편지함에나 씁니다.
    let h = header(&mut cr.c.w, o, Scope::Inbox(b.id()));
    assert_eq!(check(&h), Ok(()));
    // 나눌 권한이 없는 멤버는 남의 받은편지함에 못 쓰고, 확정 기기 주인에게 넘기는 것(인계)만 됩니다.
    let h = header(&mut cr.c.w, a, Scope::Inbox(b.id()));
    assert_eq!(
        check(&h),
        Err(AuthError::MissingPermission(Permissions::ADMINISTRATOR))
    );
    let h = header(&mut cr.c.w, a, Scope::Inbox(o.id()));
    assert_eq!(check(&h), Ok(()));
    // 멤버가 아닌 사람의 받은편지함, 멤버가 아닌 작성자.
    let h = header(&mut cr.c.w, o, Scope::Inbox(stranger.id()));
    assert_eq!(check(&h), Err(AuthError::UnknownMember));
    let h = header(&mut cr.c.w, &stranger, Scope::Inbox(b.id()));
    assert_eq!(check(&h), Err(AuthError::NotMember));
    // 키 그룹은 확정 기기만, 그 상태에 있는 그룹에만.
    let h = header(&mut cr.c.w, o, Scope::KeyGroup(group));
    assert_eq!(check(&h), Ok(()));
    let h = header(&mut cr.c.w, a, Scope::KeyGroup(group));
    assert_eq!(check(&h), Err(AuthError::NotCommitter));
    let h = header(&mut cr.c.w, o, Scope::KeyGroup(KeyGroupId::random()));
    assert_eq!(check(&h), Err(AuthError::UnknownChannel));
    // 범위 없는 헤더는 이 검사로 통과하지 않음.
    let mut h = header(&mut cr.c.w, o, Scope::Inbox(b.id()));
    h.scope = None;
    assert!(check(&h).is_err());

    // 기준점 뒤에 b가 추방돼도 기준점 상태의 답은 그대로이고, 기준점 자신을 모르면 기다립니다.
    let kicked = cr.c.push(
        o,
        EventBody::MemberKick {
            user: b.id(),
            reason: String::new(),
        },
    );
    res.insert_events(cr.c.events.clone());
    assert!(!res.state().unwrap().is_member(&b.id()));
    assert_eq!(
        res.state_at(&[made], false),
        AnchorState::Ready {
            state: Some(st.clone()),
            gaps: false
        }
    );
    let AnchorState::Ready {
        state: Some(now), ..
    } = res.state_at(&[kicked], false)
    else {
        panic!("기준점 상태가 서야 함");
    };
    assert!(!now.is_member(&b.id()));
    let unknown = EventId::hash_of(b"unknown anchor");
    assert_eq!(
        res.state_at(&[made, unknown], false),
        AnchorState::Incomplete {
            unknown: vec![unknown],
            undecoded: vec![],
        }
    );
    // 빈 구간을 허용하면 모르는 기준점은 빼고 아는 기준점으로 세웁니다(하나도 모르면 여전히 기다림).
    assert_eq!(
        res.state_at(&[made, unknown], true),
        AnchorState::Ready {
            state: Some(st.clone()),
            gaps: true
        }
    );
    assert!(matches!(
        res.state_at(&[unknown], true),
        AnchorState::Incomplete { .. }
    ));

    // 본문 없이 받은 상태 이벤트(헤더만)는 풀릴 일이 없어 기다리지 않고(아무 멤버나 헤더만 낸 이벤트 하나로 뒤의 모든
    // 기준점을 붙잡지 못하게), 키를 기다리는 상태 이벤트는 기다립니다.
    let profile = |nick: &str| EventBody::MemberProfile {
        nickname: Some(nick.into()),
        profile: None,
    };
    let bare = cr.c.w.ev(a, &[kicked], profile("헤더만"));
    res.insert(vec![], vec![LinkEvent::without_body(bare.id, &bare.header)]);
    assert!(!res.awaiting_body(&bare.id));
    assert!(matches!(
        res.state_at(&[bare.id], false),
        AnchorState::Ready { gaps: false, .. }
    ));
    let locked = cr.c.w.ev(a, &[kicked], profile("키 대기"));
    res.insert(
        vec![],
        vec![LinkEvent::from_header(locked.id, &locked.header)],
    );
    assert!(res.awaiting_body(&locked.id));
    assert_eq!(
        res.state_at(&[locked.id], false),
        AnchorState::Incomplete {
            unknown: vec![],
            undecoded: vec![locked.id],
        }
    );
    // 빈 구간을 허용하면 세우되 빈 구간이 있었다고 알립니다(거부를 기억하지 않게).
    assert!(matches!(
        res.state_at(&[locked.id], true),
        AnchorState::Ready { gaps: true, .. }
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// 기준점 상태는 기준점과 그 조상만 처음부터 해소한 상태와 같습니다(증분·스냅숏 재생으로 쌓은 해소기에서도).
    /// 빈 구간(모르는 부모·본문 없는 상태 이벤트)이 있으면 허용할 때만 해소합니다.
    #[test]
    fn state_at_equals_resolving_ancestors(seed in any::<u64>(), n in 1usize..100, pick in any::<u64>()) {
        let c = churn(seed, n);
        let genesis = c.events[0].id;
        let mut rng = Rng(pick);
        let mut res = Resolver::with_genesis(genesis);
        for (evs, lns) in churn_batches(&c, &mut rng) {
            res.insert(evs, lns);
        }
        // 부분 전달 이벤트는 공간 DAG 밖이라 해소기에게는 모르는 이벤트입니다(그것을 가리킨 부모는 빈 구간).
        let mut parents: HashMap<EventId, &[EventId]> = HashMap::new();
        for e in c.events.iter().filter(|e| e.header.scope.is_none()) {
            parents.insert(e.id, &e.header.parents);
        }
        for l in &c.links {
            parents.insert(l.id, &l.parents);
        }
        let mut all: Vec<EventId> = parents.keys().copied().collect();
        all.sort();
        let anchor: Vec<EventId> = (0..1 + rng.below(MAX_STATE_ANCHOR))
            .map(|_| all[rng.below(all.len())])
            .collect();
        let mut inside = HashSet::new();
        let mut stack = anchor.clone();
        while let Some(id) = stack.pop() {
            if let Some(ps) = parents.get(&id)
                && inside.insert(id)
            {
                stack.extend_from_slice(ps);
            }
        }
        let evs: Vec<DecodedEvent> =
            c.events.iter().filter(|e| inside.contains(&e.id)).cloned().collect();
        let lns: Vec<LinkEvent> =
            c.links.iter().filter(|l| inside.contains(&l.id)).cloned().collect();
        let want = resolve_with(&evs, &lns, Some(genesis)).state;
        let gaps = lns.iter().any(|l| l.undecoded)
            || inside
                .iter()
                .any(|id| parents[id].iter().any(|p| !parents.contains_key(p)));
        match res.state_at(&anchor, true) {
            AnchorState::Ready { state, gaps: g } => {
                prop_assert_eq!(state.map(|b| *b), want);
                prop_assert_eq!(g, gaps);
            }
            other => prop_assert!(false, "기준점 상태가 서야 함: {:?}", other),
        }
        let waits = matches!(res.state_at(&anchor, false), AnchorState::Incomplete { .. });
        prop_assert_eq!(waits, gaps);
    }
}

mod checkpoint;
mod keys;
