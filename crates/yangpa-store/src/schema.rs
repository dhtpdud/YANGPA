//! DB 스키마와 버전별 이전(migration). `PRAGMA user_version`이 적용된 마지막 버전입니다.
//!
//! 이전 스크립트는 한 번 배포되면 고치지 않고, 바꿀 것이 있으면 새 버전을 뒤에 붙입니다.

use crate::error::StoreError;

/// 버전 1: 처음 스키마.
///
/// `events.stream` 등의 주석("17바이트")은 처음 형식입니다. 그 뒤 키 그룹(2 || 그룹 ID)·받은편지함(3 || 사용자 ID,
/// 33바이트)이 더해졌고(`db::stream_key`), BLOB 열이라 이전은 필요 없습니다. 배포된 스크립트라 주석도 고치지 않습니다.
const V1: &str = r#"
-- 서명된 이벤트(디스크 등급만). 헤더·서명은 DAG 유지를 위해 끝까지 남고, 본문은 메시지 저장 키로 재봉인해 둡니다.
CREATE TABLE events (
    seq           INTEGER PRIMARY KEY,
    id            BLOB NOT NULL UNIQUE,
    space         BLOB NOT NULL,
    stream        BLOB NOT NULL,          -- 17바이트: [0;17] = 공간 수준, [1] || 채널 ID = 채널 스트림
    channel       BLOB,
    kind          INTEGER NOT NULL,
    lane          INTEGER NOT NULL,       -- 1 메시지, 2 키, 3 상태, 4 키 합의(예전 MLS)
    author_device BLOB NOT NULL,
    author_user   BLOB NOT NULL,
    lamport       INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    header        BLOB NOT NULL,          -- 정규 헤더 인코딩(postcard)
    sig           BLOB NOT NULL,
    body_sealed   BLOB,                   -- XChaCha20-Poly1305(메시지 저장 키, AD = 이벤트 ID)
    body_state    INTEGER NOT NULL,       -- 0 미수신, 1 보관 중, 2 폐기(다시 받아도 저장하지 않음)
    seed_excluded INTEGER NOT NULL DEFAULT 0,
    received_at   INTEGER NOT NULL
);
CREATE INDEX events_stream_lamport ON events(space, stream, lamport, id);
CREATE INDEX events_space_lane ON events(space, lane, lamport);
CREATE INDEX events_space_lamport ON events(space, lamport);

-- 메시지 저장 키. 이 행을 지우는 것이 암호학적 삭제입니다.
CREATE TABLE msg_keys (
    event_id BLOB PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    key      BLOB NOT NULL
) WITHOUT ROWID;

-- 스트림별 머리(자식이 아직 없는 이벤트).
CREATE TABLE heads (
    space    BLOB NOT NULL,
    stream   BLOB NOT NULL,
    event_id BLOB NOT NULL,
    lamport  INTEGER NOT NULL,
    PRIMARY KEY (space, stream, event_id)
) WITHOUT ROWID;

-- 받지 않은 기록: 저장된 이벤트가 부모로 가리키지만 아직 없는 이벤트.
CREATE TABLE missing (
    space         BLOB NOT NULL,
    stream        BLOB NOT NULL,
    event_id      BLOB NOT NULL,
    child         BLOB NOT NULL,          -- 이 부모를 가리킨 자식 중 논리 시계가 가장 작은 것
    child_lamport INTEGER NOT NULL,
    since         INTEGER NOT NULL,
    PRIMARY KEY (space, stream, event_id)
) WITHOUT ROWID;
CREATE INDEX missing_event ON missing(event_id);

-- 삭제된 메시지 표시. 원본이 삭제 이벤트보다 늦게 와도 본문을 저장하지 않게 합니다.
CREATE TABLE tombstones (
    event_id   BLOB PRIMARY KEY,
    space      BLOB NOT NULL,
    deleted_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX tombstones_space ON tombstones(space);

-- 복호화한 메시지 보기. 내용은 그 메시지의 저장 키로 봉인합니다.
CREATE TABLE messages (
    event_id       BLOB PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    space          BLOB NOT NULL,
    channel        BLOB NOT NULL,
    thread         BLOB,
    lamport        INTEGER NOT NULL,
    author_user    BLOB NOT NULL,
    author_device  BLOB NOT NULL,
    created_at     INTEGER NOT NULL,
    kind           INTEGER NOT NULL,      -- 1 메시지, 2 포럼 게시글
    content_sealed BLOB,
    edited_at      INTEGER,
    edit_lamport   INTEGER,
    edit_id        BLOB,
    deleted        INTEGER NOT NULL DEFAULT 0,
    mention        INTEGER NOT NULL DEFAULT 0,
    pinned         INTEGER NOT NULL DEFAULT 0,
    locked         INTEGER NOT NULL DEFAULT 0,
    meta_lamport   INTEGER,
    meta_id        BLOB,
    last_activity  INTEGER NOT NULL,
    reply_count    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX messages_list ON messages(space, channel, thread, lamport, event_id);
CREATE INDEX messages_unread ON messages(space, channel, lamport);

-- 수정 이벤트 목록. 원본을 지울 때 같은 내용을 담은 수정 본문도 함께 폐기합니다.
CREATE TABLE message_edits (
    target  BLOB NOT NULL,
    edit_id BLOB NOT NULL,
    PRIMARY KEY (target, edit_id)
) WITHOUT ROWID;

-- 원본 보기 행보다 먼저 도착한 수정. 수정 이벤트 자신의 저장 키로 봉인해 두고, 원본이 오면 적용합니다.
CREATE TABLE pending_edits (
    target  BLOB NOT NULL,
    edit_id BLOB NOT NULL,
    lamport INTEGER NOT NULL,
    at      INTEGER NOT NULL,
    sealed  BLOB NOT NULL,
    PRIMARY KEY (target, edit_id)
) WITHOUT ROWID;

-- 반응 집계. (대상, 이모지, 사용자)마다 마지막 이벤트(논리 시계, ID)가 이깁니다.
CREATE TABLE reactions (
    target    BLOB NOT NULL,
    emoji_key BLOB NOT NULL,
    user      BLOB NOT NULL,
    space     BLOB NOT NULL,
    emoji     BLOB NOT NULL,
    present   INTEGER NOT NULL,
    lamport   INTEGER NOT NULL,
    event_id  BLOB NOT NULL,
    PRIMARY KEY (target, emoji_key, user)
) WITHOUT ROWID;
CREATE INDEX reactions_space ON reactions(space);

CREATE TABLE read_markers (
    space      BLOB NOT NULL,
    channel    BLOB NOT NULL,
    lamport    INTEGER NOT NULL,
    event_id   BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (space, channel)
) WITHOUT ROWID;

-- 상태 키·기록 키·에포크 키. 휘발성 세션 키는 여기 오지 않습니다(VolatileStore).
CREATE TABLE keys (
    kind       INTEGER NOT NULL,
    id         BLOB NOT NULL,
    scope      BLOB NOT NULL,
    space      BLOB,
    epoch      INTEGER,
    not_before INTEGER,
    key        BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (kind, id)
) WITHOUT ROWID;
CREATE INDEX keys_scope ON keys(kind, scope);
CREATE INDEX keys_space ON keys(space);

CREATE TABLE mls_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    state      BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);

-- 신원·기기·네트워크 키 같은 비밀값.
CREATE TABLE secrets (
    name       TEXT PRIMARY KEY,
    value      BLOB NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE friends (
    user     BLOB PRIMARY KEY,
    space    BLOB,
    added_at INTEGER NOT NULL,
    verified INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX friends_space ON friends(space);

CREATE TABLE friend_links (
    token_hash BLOB PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    single_use INTEGER NOT NULL,
    uses       INTEGER NOT NULL DEFAULT 0,
    revoked    INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE friend_requests (
    request_id  BLOB PRIMARY KEY,
    direction   INTEGER NOT NULL,         -- 0 받은 요청, 1 보낸 요청
    user        BLOB NOT NULL,
    profile     BLOB,
    token_hash  BLOB NOT NULL,
    payload     BLOB NOT NULL,
    received_at INTEGER NOT NULL,
    UNIQUE (direction, user)
);
CREATE INDEX friend_requests_token ON friend_requests(token_hash);

CREATE TABLE profiles (
    user       BLOB PRIMARY KEY,
    version    INTEGER NOT NULL,
    profile    BLOB NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE device_lists (
    user       BLOB PRIMARY KEY,
    version    INTEGER NOT NULL,
    list       BLOB NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE notes (
    user       BLOB PRIMARY KEY,
    text       TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

-- 해소된 공간 상태 캐시(SpaceState JSON). 버전 5부터 `state`는 비우고 행으로 나눠 둡니다(V5).
CREATE TABLE spaces (
    space        BLOB PRIMARY KEY,
    state        TEXT,
    dormant      INTEGER NOT NULL DEFAULT 0,
    joined_at    INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_read_at INTEGER
) WITHOUT ROWID;

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE api_tokens (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    token_hash   BLOB NOT NULL UNIQUE,
    scopes       TEXT NOT NULL,
    spaces       TEXT NOT NULL,
    automation   INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    revoked      INTEGER NOT NULL DEFAULT 0,
    last_used_at INTEGER
);

-- 감사 로그(덧붙이기만, 상한을 넘으면 오래된 것부터 지움). 메시지 내용은 넣지 않습니다.
CREATE TABLE audit_log (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    at      INTEGER NOT NULL,
    token   TEXT NOT NULL,
    method  TEXT NOT NULL,
    outcome TEXT NOT NULL,
    detail  TEXT NOT NULL
);

-- AI가 요청한 파괴적 작업(사람 확인 대기).
CREATE TABLE confirmations (
    id         TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    token      TEXT NOT NULL,
    method     TEXT NOT NULL,
    params     TEXT NOT NULL,
    summary    TEXT NOT NULL
);

-- 블롭(이미 암호화된 조각의 파일 저장소) 목록과 가진 조각 비트맵.
CREATE TABLE blobs (
    blob_id      BLOB PRIMARY KEY,
    manifest     BLOB NOT NULL,
    size         INTEGER NOT NULL,
    chunk_size   INTEGER NOT NULL,
    chunk_count  INTEGER NOT NULL,
    have         BLOB NOT NULL,
    have_count   INTEGER NOT NULL DEFAULT 0,
    stored_bytes INTEGER NOT NULL DEFAULT 0,
    complete     INTEGER NOT NULL DEFAULT 0,
    pinned       INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    last_access  INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX blobs_lru ON blobs(pinned, last_access);

-- 블롭이 속한 공간(닫힌 스웜: 그 공간 멤버에게만 제공).
CREATE TABLE blob_spaces (
    blob_id BLOB NOT NULL REFERENCES blobs(blob_id) ON DELETE CASCADE,
    space   BLOB NOT NULL,
    PRIMARY KEY (blob_id, space)
) WITHOUT ROWID;
CREATE INDEX blob_spaces_space ON blob_spaces(space);

-- 최근 연결에 성공한 노드(재실행 시 부트스트랩 조회를 건너뜀).
CREATE TABLE peer_cache (
    peer_id      BLOB PRIMARY KEY,
    addrs        TEXT NOT NULL,
    user         BLOB,
    device       BLOB,
    last_success INTEGER,
    last_attempt INTEGER,
    failures     INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX peer_cache_device ON peer_cache(device);
CREATE INDEX peer_cache_recent ON peer_cache(last_success);

-- 초대 사용 기록(확정 전 중복 가입 처리 방지).
CREATE TABLE invite_redemptions (
    space      BLOB NOT NULL,
    token_hash BLOB NOT NULL,
    user       BLOB NOT NULL,
    at         INTEGER NOT NULL,
    PRIMARY KEY (space, token_hash, user)
) WITHOUT ROWID;
"#;

/// 버전 2: 휘발성 채널 이중 차단, 일반 등급 채팅 수명 만료 검색, 수정 이벤트 단독 폐기용 색인.
const V2: &str = r#"
-- 공간 상태 캐시가 휘발성으로 표시한 채널. 디스크 저장 API가 호출자가 넘긴 보안 등급과 별개로 한 번 더 거릅니다.
-- 채널 보안 등급은 바뀌지 않으므로(상태 해소가 거부) 한 번 들어간 행은 공간을 지울 때까지 남깁니다.
CREATE TABLE volatile_channels (
    space   BLOB NOT NULL,
    channel BLOB NOT NULL,
    PRIMARY KEY (space, channel)
) WITHOUT ROWID;

-- 일반 등급 채널의 채팅 수명(이 기기의 수신 시점 기준) 만료 대상 찾기.
CREATE INDEX events_channel_received ON events(space, channel, received_at) WHERE lane = 1;

-- 수정 이벤트 본문만 폐기될 때 그 키로 봉인해 둔 보관 수정도 함께 지웁니다.
CREATE INDEX pending_edits_edit ON pending_edits(edit_id);
CREATE INDEX message_edits_edit ON message_edits(edit_id);
"#;

/// 버전 3: 감사 로그를 중요·평범 두 종류로 나눠 따로 상한을 둡니다(평범한 호출로 중요한 기록을 밀어내지 못하게).
const V3: &str = r#"
ALTER TABLE audit_log ADD COLUMN important INTEGER NOT NULL DEFAULT 0;
CREATE INDEX audit_log_tier ON audit_log(important, seq);
"#;

/// 버전 4: 연락처 링크로 생긴 임시 연락처·만료된 연락처. `kind` 0 친구, 1 임시 연락처, 2 만료된 연락처. 임시 연락처는
/// 쓴 링크의 해시(`link_hash`)와 그 링크의 만료 시각(`expires_at`)을 남깁니다.
const V4: &str = r#"
ALTER TABLE friends ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;
ALTER TABLE friends ADD COLUMN expires_at INTEGER;
ALTER TABLE friends ADD COLUMN link_hash BLOB;
"#;

/// 버전 5: 공간 상태를 행으로 나눠 둡니다(바뀐 행만 씀). 큰 표를 뺀 머리는 `spaces.head`(postcard), 멤버·기기 기록,
/// 밴, 채널은 열쇠마다 한 행. 기기 키는 키 약속으로 찾는 `device_keys`(내용 주소: 같은 기기라도 키가 다르면 다른
/// 행; 넣은 공간을 지우면 함께 지움).
///
/// 옛 JSON 캐시(`spaces.state`)는 기기 서명 키가 없어 새 상태로 옮길 수 없으므로 비우고, 데몬이 저장된 이벤트를
/// 다시 해소해 행을 처음부터 씁니다(이전 단계의 `volatile_channels`는 그대로 남아 이중 차단이 유지됨). 다만 기기
/// 목록(`DeviceEntry`)에 서명 공개키가 생겨 예전에 저장한 기기 목록·상태 이벤트는 풀리지 않으므로, 예전 형식의 기기
/// 목록이 있는 DB는 이 이전을 적용하지 않고 [`StoreError::IncompatibleData`]로 멈춥니다(`check_v5_data`).
const V5: &str = r#"
ALTER TABLE spaces ADD COLUMN head BLOB;
UPDATE spaces SET state = NULL;

CREATE TABLE space_members (
    space   BLOB NOT NULL,
    user    BLOB NOT NULL,
    member  BLOB,                 -- postcard(Member), 떠났으면 NULL
    devices BLOB,                 -- postcard(MemberDevices)
    PRIMARY KEY (space, user)
) WITHOUT ROWID;

CREATE TABLE space_bans (
    space BLOB NOT NULL,
    user  BLOB NOT NULL,
    ban   BLOB NOT NULL,          -- postcard(BanInfo)
    PRIMARY KEY (space, user)
) WITHOUT ROWID;

CREATE TABLE space_channels (
    space    BLOB NOT NULL,
    channel  BLOB NOT NULL,
    settings BLOB NOT NULL,       -- postcard(ChannelSettings)
    PRIMARY KEY (space, channel)
) WITHOUT ROWID;

CREATE TABLE device_keys (
    key_commit BLOB NOT NULL,     -- BLAKE3 약속(상자 키·서명 키)
    space      BLOB NOT NULL,     -- 이 행을 넣은 공간(공간을 지울 때 함께 지움)
    device     BLOB NOT NULL,
    box_key    BLOB NOT NULL,
    ed_pk      BLOB NOT NULL,
    pq_pk      BLOB NOT NULL,
    PRIMARY KEY (key_commit, space)
);                                -- 행이 2KB 남짓이라 rowid 표(WITHOUT ROWID는 작은 행에만 이득)
-- 공간을 지울 때와 시작 때 이미 있는 행을 고를 때(약속까지 담아 표를 읽지 않음).
CREATE INDEX device_keys_space ON device_keys(space, key_commit);
"#;

/// 버전 6: 그룹 키 합의가 MLS에서 키 그룹(`KeyUpdate`)으로 바뀌어, MLS 그룹 상태(그룹 비밀 포함)와 MLS 에포크 키
/// (`keys` 종류 3)를 디스크에서 지웁니다. 쓰지 않는 비밀이 DB에 남지 않게. 키 그룹 상태는 잠긴 비밀값(`secrets`)에 둡니다.
const V6: &str = r#"
DROP TABLE IF EXISTS mls_state;
DELETE FROM keys WHERE kind = 3;
"#;

/// 버전 7: 메시지 보기에 익명 글 표시(`anonymous`)를 둡니다. 예전에는 목록·검색이 행마다 큰 `events` 행을 찾아
/// 이벤트 종류를 읽었는데(무작위 접근이라 검색 색인을 만들 때 대부분의 시간), 보기 행만 읽으면 되게 합니다. 기존 행은
/// 이벤트 종류로 채웁니다(7 `AnonMessage`, 8 `AnonForumPost`: 전송 형식 코드라 바뀌지 않음).
const V7: &str = r#"
ALTER TABLE messages ADD COLUMN anonymous INTEGER NOT NULL DEFAULT 0;
UPDATE messages SET anonymous = 1 WHERE event_id IN (SELECT id FROM events WHERE kind IN (7, 8));
"#;

/// 순서대로 적용할 이전 스크립트. 인덱스 + 1이 그 스크립트를 적용한 뒤의 `user_version`입니다.
pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6, V7];

/// `volatile_channels`가 생긴 버전. 이 버전으로 올라갈 때 기존 상태 캐시에서 행을 채웁니다.
pub(crate) const VOLATILE_CHANNELS_VERSION: usize = 2;

/// 기기 목록에 서명 공개키가 생긴 버전. 이 버전으로 올라가기 전에 [`check_v5_data`]로 예전 형식을 찾습니다.
pub(crate) const DEVICE_SIGN_KEYS_VERSION: usize = 5;

/// 5판으로 올리기 전 검사: 저장된 기기 목록이 모두 새 형식으로 읽히는지. 신원을 만든 DB에는 내 기기 목록이 늘 있으므로,
/// 예전 판이 쓴 DB는 여기서 걸립니다. 그런 DB를 이어 쓰면 신원을 읽다 '손상'으로 멈추고 상태 이벤트도 풀리지 않아
/// (서명된 이벤트라 고쳐 쓸 수 없음), 아무것도 바꾸지 않은 채 데이터를 지우고 새로 시작하라고 알립니다(예전 판으로는
/// 그대로 열림).
pub(crate) fn check_v5_data(c: &rusqlite::Connection) -> Result<(), StoreError> {
    let mut stmt = c.prepare("SELECT list FROM device_lists")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let bytes: Vec<u8> = r.get(0)?;
        let ok = postcard::from_bytes::<yangpa_core::device::SignedDeviceList>(&bytes)
            .is_ok_and(|l| l.list.devices.iter().all(|d| d.keys_match()));
        if !ok {
            return Err(StoreError::IncompatibleData);
        }
    }
    Ok(())
}
