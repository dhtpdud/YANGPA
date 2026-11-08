//! 메시지 검색 색인(RAM 전용).
//!
//! 메시지 보기는 행마다 저장 키로 봉인돼 있어, 검색마다 모든 행을 복호화하면 기록에 비례해 느려집니다. 처음 검색할 때
//! 일반 등급 메시지를 한 번만 복호화해 정규화한 글을 버퍼 하나에 이어 붙여 두고, 그 뒤로는 쓰기 연결의 변경 훅이 알려 준
//! `messages` 행만 다시 읽어 맞춥니다. 찾기는 SIMD 부분 문자열 탐색(`memchr::memmem`)을 버퍼 조각마다 스레드로 나눠
//! 돌리고, 쪽은 전체 정렬 대신 선택 알고리즘(introselect)으로 그 쪽만 골라 정렬합니다.
//!
//! 색인은 디스크·로그로 가지 않습니다. 버퍼를 늘리거나 버릴 때 예전 내용을 0으로 덮고, 지운 글은 그 자리에서 바로 덮습니다.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;

use memchr::memmem::Finder;
use parking_lot::Mutex;
use rusqlite::hooks::Action;
use rusqlite::{Connection, OptionalExtension};
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfkc_quick};
use yangpa_core::body::Secret32;
use yangpa_core::ids::{ChannelId, EventId, SpaceId, UserId};
use zeroize::Zeroizing;

use crate::db::Store;
use crate::error::Result;
use crate::messages::{MAX_PAGE, MessageContent, open_content};

/// 글과 글 사이(UTF-8에 나오지 않는 바이트라 검색어가 글 경계를 넘어 맞을 수 없음). 지운 글도 이 바이트로 덮습니다.
const DOC_SEP: u8 = 0xFF;
/// 한 글 안 칸(본문·제목·첨부 이름) 사이. 칸을 넘는 일치도 막습니다.
const FIELD_SEP: u8 = 0xFE;
/// 바뀐 행 기록 상한. 넘으면 행마다 맞추는 대신 다음 검색에서 색인을 새로 만듭니다(기록이 끝없이 크지 않게).
const MAX_LOGGED_ROWS: usize = 1 << 20;
/// 이보다 작은 버퍼는 한 스레드로 훑습니다(스레드를 띄우는 값이 더 큼).
const PARALLEL_MIN_BYTES: usize = 1 << 20;
/// 훑기·만들기에 쓰는 스레드 상한.
const MAX_THREADS: usize = 8;
/// 색인을 만들 때 작업 스레드에 한 번에 넘기는 행 수(시험에서는 여러 묶음을 거치게 작게).
const BUILD_BATCH: usize = if cfg!(test) { 16 } else { 2048 };
/// 바뀐 행이 이보다 많으면 행마다 맞추지 않고 다시 만듭니다(차례로 훑는 편이 빠름).
const REBUILD_ROWS: usize = 50_000;
/// 지운 글이 버퍼의 이만큼(분모)을 넘으면 버퍼를 다시 채웁니다.
const COMPACT_RATIO: usize = 3;
/// 검색어 낱말 수 상한(낱말마다 글을 한 번 더 훑으므로).
const MAX_TERMS: usize = 16;
/// 삭제 직후 바로 맞추는 행 수 상한(지운 행은 기록 맨 뒤에 있음). 나머지는 다음 검색에서.
const SCRUB_MAX_ROWS: usize = 4096;

const ROW_SELECT: &str = "SELECT m.rowid, m.event_id, m.space, m.channel, m.author_user, m.created_at, \
     m.content_sealed, m.deleted, k.key, m.anonymous FROM messages m \
     LEFT JOIN msg_keys k ON k.event_id = m.event_id";

/// `messages` 표의 바뀐 행(rowid). 쓰기 연결의 변경 훅이 채우고 검색이 비웁니다.
#[derive(Default)]
pub(crate) struct ChangeLog {
    /// 색인이 있거나 만드는 중일 때만 적습니다(색인이 없으면 모을 이유가 없음).
    on: bool,
    rows: Vec<i64>,
    /// 상한을 넘어 버렸음: 다음 검색에서 다시 만듭니다.
    overflow: bool,
}

/// 저장소가 들고 있는 검색 상태. 잠금 순서는 `index` → 쓰기 연결 → `log`입니다(훅은 쓰기 연결을 쥔 채 `log`를 잡음).
#[derive(Default)]
pub(crate) struct SearchState {
    log: Arc<Mutex<ChangeLog>>,
    index: Mutex<Option<SearchIndex>>,
}

impl SearchState {
    /// 쓰기 연결에 변경 훅을 겁니다. 외래 키 연쇄 삭제로 지워지는 행도 훅을 거칩니다.
    pub(crate) fn install(&self, conn: &Connection) -> Result<()> {
        let log = self.log.clone();
        conn.update_hook(Some(move |_: Action, db: &str, table: &str, rowid: i64| {
            if table != "messages" || db != "main" {
                return;
            }
            let mut l = log.lock();
            if !l.on || l.overflow {
                return;
            }
            if l.rows.len() >= MAX_LOGGED_ROWS {
                l.overflow = true;
                l.rows = Vec::new();
            } else {
                l.rows.push(rowid);
            }
        }))?;
        Ok(())
    }
}

/// 검색 범위. 빈 칸은 거르지 않습니다.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchFilter<'a> {
    pub space: Option<&'a SpaceId>,
    pub channel: Option<&'a ChannelId>,
    /// 이 서명자가 쓴 글만(익명 글도 서명자로 거르므로, 가리는 것은 부르는 쪽 몫).
    pub author: Option<&'a UserId>,
}

/// 맞은 글 하나(부르는 쪽 거르기용). `chan`은 (공간, 채널)마다 고정된 작은 번호라 권한 확인을 채널마다 한 번만 하게
/// 해 줍니다.
#[derive(Debug)]
pub struct SearchHit<'a> {
    pub id: EventId,
    pub space: &'a SpaceId,
    pub channel: &'a ChannelId,
    pub chan: u32,
    pub author: &'a UserId,
    /// 익명으로 쓴 글(이벤트 종류 기준).
    pub anonymous: bool,
}

/// 검색 결과 한 쪽.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    /// 거르기까지 마친 전체 결과 수.
    pub total: usize,
    /// 실제 쪽 시작 위치(끝을 넘으면 마지막 쪽으로 당김).
    pub offset: usize,
    /// 이 쪽의 글(최신순: 작성 시각, 같으면 ID 내림차순).
    pub ids: Vec<EventId>,
}

/// 검색어를 낱말로 나눠 정규화합니다. 빈칸으로 나누고, 큰따옴표로 묶은 것은 빈칸까지 한 낱말(구절)입니다.
/// 다른 낱말에 들어 있는 낱말은 뺍니다(어차피 맞음).
pub(crate) fn parse_terms(query: &str) -> Vec<Vec<u8>> {
    let mut raw: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in query.chars() {
        match c {
            '"' | '“' | '”' => {
                raw.push(std::mem::take(&mut cur));
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => raw.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    raw.push(cur);
    let mut terms: Vec<Vec<u8>> = Vec::new();
    for t in raw {
        let mut f = Vec::new();
        fold_into(&mut f, t.trim());
        if !f.is_empty() && !terms.contains(&f) {
            terms.push(f);
        }
    }
    // 긴 낱말부터: 첫 낱말로 훑으므로 드문(긴) 것이 앞이 좋습니다.
    terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
    let mut out: Vec<Vec<u8>> = Vec::new();
    for t in terms {
        if !out.iter().any(|o| memchr::memmem::find(o, &t).is_some()) {
            out.push(t);
        }
    }
    out.truncate(MAX_TERMS);
    out
}

/// 검색용 정규화: NFKC(전각·합자 등 호환 문자를 펼침) + 소문자 + 빈칸 묶음을 한 칸으로. 글과 검색어에 똑같이 씁니다.
pub(crate) fn fold_into(out: &mut Vec<u8>, s: &str) {
    let mut space = false;
    let mut push = |out: &mut Vec<u8>, c: char| {
        if c.is_whitespace() {
            if !space {
                out.push(b' ');
                space = true;
            }
            return;
        }
        space = false;
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase() as u8);
        } else {
            let mut buf = [0u8; 4];
            for l in c.to_lowercase() {
                out.extend_from_slice(l.encode_utf8(&mut buf).as_bytes());
            }
        }
    };
    if s.is_ascii() || is_nfkc_quick(s.chars()) == IsNormalized::Yes {
        // 대부분의 글(영문·한글 완성형)은 이미 NFKC라 펼치지 않습니다.
        for c in s.chars() {
            push(out, c);
        }
    } else {
        for c in s.nfkc() {
            push(out, c);
        }
    }
}

/// 한 글의 검색용 글(본문, 포럼 제목, 첨부 이름).
fn doc_text(content: &MessageContent) -> Zeroizing<Vec<u8>> {
    let title = content.forum.as_ref().map(|f| f.title.as_str());
    let names = content.body.attachments.iter().map(|a| a.name.as_str());
    // 늘리며 옮긴 예전 버퍼는 지워지지 않으므로 처음부터 넉넉히 잡습니다(정규화로 조금 길어질 수 있음).
    let len = content.body.text.len()
        + title.map_or(0, str::len)
        + names.clone().map(|n| n.len() + 1).sum::<usize>();
    let mut out = Zeroizing::new(Vec::with_capacity(len + len / 4 + 8));
    fold_into(&mut out, &content.body.text);
    for field in title.into_iter().chain(names) {
        if !field.is_empty() {
            out.push(FIELD_SEP);
            fold_into(&mut out, field);
        }
    }
    out
}

/// 색인에 넣을 글 하나.
struct Prepared {
    rowid: i64,
    id: [u8; 32],
    space: [u8; 16],
    channel: [u8; 16],
    author: [u8; 32],
    created_at: i64,
    anonymous: bool,
    text: Zeroizing<Vec<u8>>,
}

/// DB에서 읽은 행 그대로(복호화 전).
struct RawRow {
    rowid: i64,
    id: [u8; 32],
    space: [u8; 16],
    channel: [u8; 16],
    author: [u8; 32],
    created_at: i64,
    sealed: Option<Vec<u8>>,
    deleted: bool,
    key: Option<[u8; 32]>,
    anonymous: bool,
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        rowid: r.get(0)?,
        id: r.get(1)?,
        space: r.get(2)?,
        channel: r.get(3)?,
        author: r.get(4)?,
        created_at: r.get(5)?,
        sealed: r.get(6)?,
        deleted: r.get(7)?,
        key: r.get(8)?,
        anonymous: r.get(9)?,
    })
}

/// 복호화해 검색용 글을 만듭니다. 지웠거나 내용·키가 없거나 열리지 않는 행은 `None`(목록처럼 건너뜀).
fn prepare(raw: RawRow) -> Option<Prepared> {
    if raw.deleted {
        return None;
    }
    let (Some(sealed), Some(key)) = (&raw.sealed, &raw.key) else {
        return None;
    };
    let content = open_content(&Secret32(*key), &EventId(raw.id), sealed).ok()?;
    Some(Prepared {
        rowid: raw.rowid,
        id: raw.id,
        space: raw.space,
        channel: raw.channel,
        author: raw.author,
        created_at: raw.created_at,
        anonymous: raw.anonymous,
        text: doc_text(&content),
    })
}

#[derive(Clone, Copy)]
struct Doc {
    id: [u8; 32],
    rowid: i64,
    created_at: i64,
    /// 버퍼 안 시작 위치. 문서는 시작 위치 순으로 놓입니다(뒤에만 붙이고, 다시 채울 때도 순서 유지).
    start: usize,
    len: u32,
    chan: u32,
    author: u32,
    anonymous: bool,
    alive: bool,
}

impl Doc {
    fn end(&self) -> usize {
        self.start + self.len as usize
    }
}

/// 최신순(작성 시각, 같으면 ID 내림차순).
fn newest_first(a: &Doc, b: &Doc) -> Ordering {
    b.created_at
        .cmp(&a.created_at)
        .then_with(|| b.id.cmp(&a.id))
}

#[derive(Default)]
struct SearchIndex {
    text: Zeroizing<Vec<u8>>,
    docs: Vec<Doc>,
    by_row: HashMap<i64, u32>,
    chans: Vec<(SpaceId, ChannelId)>,
    chan_ids: HashMap<(SpaceId, ChannelId), u32>,
    authors: Vec<UserId>,
    author_ids: HashMap<UserId, u32>,
    dead_docs: usize,
    dead_bytes: usize,
}

/// 훑기 조건(공간·채널·작성자). 번호로 바꿔 두어 글마다 비교가 정수 하나입니다.
struct Scope {
    chans: Option<Vec<bool>>,
    author: Option<u32>,
}

impl Scope {
    fn admits(&self, d: &Doc) -> bool {
        d.alive
            && self.author.is_none_or(|a| d.author == a)
            && self
                .chans
                .as_ref()
                .is_none_or(|c| c.get(d.chan as usize).copied().unwrap_or(false))
    }
}

impl SearchIndex {
    fn intern_chan(&mut self, space: SpaceId, channel: ChannelId) -> u32 {
        *self.chan_ids.entry((space, channel)).or_insert_with(|| {
            self.chans.push((space, channel));
            (self.chans.len() - 1) as u32
        })
    }

    fn intern_author(&mut self, user: UserId) -> u32 {
        *self.author_ids.entry(user).or_insert_with(|| {
            self.authors.push(user);
            (self.authors.len() - 1) as u32
        })
    }

    /// 버퍼를 늘립니다. `Vec`의 재할당은 예전 메모리를 지우지 않으므로 새로 잡아 옮기고 예전 것은 0으로 덮습니다.
    fn reserve(&mut self, extra: usize) {
        if self.text.capacity() - self.text.len() >= extra {
            return;
        }
        let want = (self.text.len() + extra)
            .max(self.text.capacity().saturating_mul(2))
            .max(1 << 16);
        let mut next = Zeroizing::new(Vec::with_capacity(want));
        next.extend_from_slice(&self.text);
        // 예전 버퍼는 여기서 해제되며 Zeroizing이 덮습니다.
        self.text = next;
    }

    fn push(&mut self, p: Prepared) {
        // 반응·댓글 수만 바뀐 행도 기록에 오므로, 글이 그대로면 옮기지 않습니다.
        if let Some(&slot) = self.by_row.get(&p.rowid) {
            let d = &self.docs[slot as usize];
            if d.id == p.id && self.text[d.start..d.end()] == p.text[..] {
                return;
            }
        }
        self.remove_row(p.rowid);
        let Ok(len) = u32::try_from(p.text.len()) else {
            return;
        };
        let chan = self.intern_chan(SpaceId(p.space), ChannelId(p.channel));
        let author = self.intern_author(UserId(p.author));
        self.reserve(p.text.len() + 1);
        let start = self.text.len();
        self.text.extend_from_slice(&p.text);
        self.text.push(DOC_SEP);
        self.by_row.insert(p.rowid, self.docs.len() as u32);
        self.docs.push(Doc {
            id: p.id,
            rowid: p.rowid,
            created_at: p.created_at,
            start,
            len,
            chan,
            author,
            anonymous: p.anonymous,
            alive: true,
        });
    }

    /// 행을 색인에서 뺍니다. 내용은 그 자리에서 바로 덮습니다(지운 글이 RAM에 남지 않게).
    fn remove_row(&mut self, rowid: i64) {
        let Some(slot) = self.by_row.remove(&rowid) else {
            return;
        };
        let d = &mut self.docs[slot as usize];
        d.alive = false;
        self.text[d.start..d.end()].fill(DOC_SEP);
        self.dead_docs += 1;
        self.dead_bytes += d.len as usize + 1;
    }

    /// 지운 글이 많으면 버퍼와 문서 표를 다시 채웁니다(순서 유지).
    fn maybe_compact(&mut self) {
        if self.dead_bytes * COMPACT_RATIO <= self.text.len()
            && self.dead_docs * COMPACT_RATIO <= self.docs.len()
        {
            return;
        }
        let live = self.text.len() - self.dead_bytes;
        let mut text = Zeroizing::new(Vec::with_capacity(live + live / 4));
        let mut docs = Vec::with_capacity(self.docs.len() - self.dead_docs);
        self.by_row.clear();
        for d in self.docs.iter().filter(|d| d.alive) {
            let start = text.len();
            text.extend_from_slice(&self.text[d.start..d.end() + 1]);
            self.by_row.insert(d.rowid, docs.len() as u32);
            docs.push(Doc { start, ..*d });
        }
        self.text = text;
        self.docs = docs;
        self.dead_docs = 0;
        self.dead_bytes = 0;
    }

    fn scope(&self, f: &SearchFilter<'_>) -> Option<Scope> {
        let author = match f.author {
            Some(u) => Some(*self.author_ids.get(u)?),
            None => None,
        };
        let chans = if f.space.is_some() || f.channel.is_some() {
            let mask: Vec<bool> = self
                .chans
                .iter()
                .map(|(s, c)| f.space.is_none_or(|x| x == s) && f.channel.is_none_or(|x| x == c))
                .collect();
            if !mask.contains(&true) {
                return None;
            }
            Some(mask)
        } else {
            None
        };
        Some(Scope { chans, author })
    }

    /// 조건과 모든 낱말에 맞는 글 번호(시작 위치 순).
    fn find(&self, terms: &[Vec<u8>], scope: &Scope) -> Vec<u32> {
        let Some((first, rest)) = terms.split_first() else {
            return (0..self.docs.len() as u32)
                .filter(|&i| scope.admits(&self.docs[i as usize]))
                .collect();
        };
        let first = Finder::new(first);
        let rest: Vec<Finder<'_>> = rest.iter().map(Finder::new).collect();
        let threads = if self.text.len() < PARALLEL_MIN_BYTES {
            1
        } else {
            std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .clamp(1, MAX_THREADS)
        };
        // 문서 경계에서 버퍼를 바이트 수가 비슷한 조각으로 나눕니다.
        let mut cuts = vec![0usize];
        for k in 1..threads {
            let at = self.text.len() * k / threads;
            let i = self.docs.partition_point(|d| d.start < at);
            if i > *cuts.last().unwrap_or(&0) && i < self.docs.len() {
                cuts.push(i);
            }
        }
        cuts.push(self.docs.len());
        if cuts.len() == 2 {
            let mut out = Vec::new();
            self.scan(&first, &rest, scope, 0, self.docs.len(), &mut out);
            return out;
        }
        std::thread::scope(|s| {
            let parts: Vec<_> = cuts
                .windows(2)
                .map(|w| {
                    let (lo, hi) = (w[0], w[1]);
                    let (first, rest) = (&first, &rest);
                    s.spawn(move || {
                        let mut out = Vec::new();
                        self.scan(first, rest, scope, lo, hi, &mut out);
                        out
                    })
                })
                .collect();
            let mut out = Vec::new();
            for p in parts {
                out.extend(p.join().unwrap_or_default());
            }
            out
        })
    }

    /// `docs[lo..hi]` 구간을 첫 낱말로 훑고, 맞은 글에서 나머지 낱말을 확인합니다. 한 글에서 맞으면 그 글 끝으로 건너뜁니다.
    fn scan(
        &self,
        first: &Finder<'_>,
        rest: &[Finder<'_>],
        scope: &Scope,
        lo: usize,
        hi: usize,
        out: &mut Vec<u32>,
    ) {
        if lo >= hi {
            return;
        }
        let end = self.docs.get(hi).map_or(self.text.len(), |d| d.start);
        let mut pos = self.docs[lo].start;
        let mut d = lo;
        while pos < end {
            let Some(i) = first.find(&self.text[pos..end]) else {
                break;
            };
            let at = pos + i;
            // 시작 위치가 `at` 이하인 마지막 문서. `d`는 앞으로만 가므로 남은 구간에서 찾습니다.
            d += self.docs[d..hi].partition_point(|x| x.start <= at) - 1;
            let doc = &self.docs[d];
            let body = &self.text[doc.start..doc.end()];
            if scope.admits(doc) && rest.iter().all(|f| f.find(body).is_some()) {
                out.push(d as u32);
            }
            pos = doc.end() + 1;
        }
    }

    fn hit(&self, slot: u32) -> SearchHit<'_> {
        let d = &self.docs[slot as usize];
        let (space, channel) = &self.chans[d.chan as usize];
        SearchHit {
            id: EventId(d.id),
            space,
            channel,
            chan: d.chan,
            author: &self.authors[d.author as usize],
            anonymous: d.anonymous,
        }
    }

    /// `hits`에서 최신순 `offset`번째부터 `limit`개를 골라 정렬합니다. 전체를 정렬하지 않고 선택 알고리즘으로
    /// 경계만 잡아 O(n + limit·log limit)입니다.
    fn page<'h>(&self, hits: &'h mut [u32], offset: usize, limit: usize) -> &'h [u32] {
        let cmp = |a: &u32, b: &u32| newest_first(&self.docs[*a as usize], &self.docs[*b as usize]);
        if offset >= hits.len() {
            return &[];
        }
        if offset > 0 {
            hits.select_nth_unstable_by(offset, cmp);
        }
        let rest = &mut hits[offset..];
        if rest.len() > limit {
            rest.select_nth_unstable_by(limit, cmp);
        }
        let n = limit.min(rest.len());
        let window = &mut rest[..n];
        window.sort_unstable_by(cmp);
        window
    }

    /// 바뀐 행을 다시 읽어 맞춥니다. 쓰기 연결로 읽어 페이지 캐시가 따뜻하고, 커밋된 내용 기준입니다.
    fn refresh(&mut self, c: &Connection, mut rows: Vec<i64>) -> Result<()> {
        rows.sort_unstable();
        rows.dedup();
        let mut stmt = c.prepare_cached(&format!("{ROW_SELECT} WHERE m.rowid = ?1"))?;
        for rowid in rows {
            let raw = stmt.query_row([rowid], read_row).optional()?;
            match raw.and_then(prepare) {
                Some(p) => self.push(p),
                None => self.remove_row(rowid),
            }
        }
        self.maybe_compact();
        Ok(())
    }
}

impl Store {
    /// 로컬 메시지 검색(일반 등급만: 휘발성은 디스크에 없어 애초에 대상이 아님). 대소문자·전각을 무시한 부분 일치이고,
    /// 검색어를 빈칸으로 나눈 낱말이 모두 들어 있어야 맞습니다(큰따옴표로 묶으면 구절 그대로). 본문·포럼 제목·첨부
    /// 이름에서 찾습니다. `filter.author`가 있으면 검색어가 비어도 되고, 둘 다 없으면 빈 결과입니다.
    ///
    /// `accept`로 부르는 쪽이 권한·숨김 등을 거르며, `total`은 거른 뒤의 수입니다. 결과는 최신순이고 `offset`이 끝을
    /// 넘으면 마지막 쪽을 돌려줍니다. 첫 검색에서 색인을 만드느라 기록 크기만큼 걸리고, 그 뒤로는 바뀐 행만 맞춥니다.
    pub fn search_messages(
        &self,
        query: &str,
        filter: SearchFilter<'_>,
        mut accept: impl FnMut(&SearchHit<'_>) -> bool,
        offset: usize,
        limit: usize,
    ) -> Result<SearchPage> {
        let terms = parse_terms(query);
        let limit = limit.clamp(1, MAX_PAGE as usize);
        let empty = SearchPage {
            total: 0,
            offset: 0,
            ids: Vec::new(),
        };
        if terms.is_empty() && filter.author.is_none() {
            return Ok(empty);
        }
        let mut guard = self.search_index()?;
        let Some(idx) = guard.as_mut() else {
            return Ok(empty);
        };
        let Some(scope) = idx.scope(&filter) else {
            return Ok(empty);
        };
        let mut hits = idx.find(&terms, &scope);
        hits.retain(|&s| accept(&idx.hit(s)));
        let total = hits.len();
        let offset = if offset >= total {
            total.saturating_sub(1) / limit * limit
        } else {
            offset
        };
        let ids = idx
            .page(&mut hits, offset, limit)
            .iter()
            .map(|&s| EventId(idx.docs[s as usize].id))
            .collect();
        Ok(SearchPage { total, offset, ids })
    }

    /// 검색 색인을 비웁니다(다음 검색에서 다시 만듦). 표를 통째로 지우는 작업 뒤에 부릅니다.
    pub fn reset_search_index(&self) {
        let mut g = self.search.index.lock();
        *g = None;
        let mut l = self.search.log.lock();
        *l = ChangeLog::default();
    }

    /// 색인을 꺼내 줍니다. 없거나 바뀐 행이 너무 많으면 만들고, 있으면 그동안 바뀐 행을 맞춥니다.
    fn search_index(&self) -> Result<parking_lot::MutexGuard<'_, Option<SearchIndex>>> {
        let mut g = self.search.index.lock();
        let stale = {
            let l = self.search.log.lock();
            l.overflow || l.rows.len() > REBUILD_ROWS
        };
        if g.is_none() || stale {
            *g = None;
            {
                // 쓰기 연결을 잡아 진행 중인 쓰기가 끝난 뒤 기록을 켭니다. 그 뒤에 커밋되는 변경은 모두 기록에 남고,
                // 그 전 변경은 아래에서 읽는 내용에 들어 있습니다.
                let _w = self.writer.lock();
                *self.search.log.lock() = ChangeLog {
                    on: true,
                    ..ChangeLog::default()
                };
            }
            let started = Instant::now();
            match self.build_search_index() {
                Ok(idx) => {
                    tracing::debug!(
                        docs = idx.docs.len(),
                        ms = started.elapsed().as_millis() as u64,
                        "검색 색인 만듦"
                    );
                    *g = Some(idx);
                }
                Err(e) => {
                    *self.search.log.lock() = ChangeLog::default();
                    return Err(e);
                }
            }
        }
        if let Some(idx) = g.as_mut()
            && let Err(e) = self.sync_rows(idx, None)
        {
            // 일부만 맞춘 색인은 믿을 수 없으니 버리고 다음에 다시 만듭니다.
            *g = None;
            *self.search.log.lock() = ChangeLog::default();
            return Err(e);
        }
        Ok(g)
    }

    /// 기록에 든 바뀐 행을 다시 읽어 맞춥니다. 쓰기 연결을 쥔 채 꺼내고 읽으므로 그사이 커밋되는 변경이 없고, 방금 쓴
    /// 페이지가 그 연결의 캐시에 있어 빠릅니다(읽기 연결은 다른 연결이 쓸 때마다 캐시를 비워 페이지를 다시 복호화함).
    /// `max`면 가장 최근 것부터 그만큼만(나머지는 남김).
    fn sync_rows(&self, idx: &mut SearchIndex, max: Option<usize>) -> Result<()> {
        let w = self.writer.lock();
        let rows = {
            let mut l = self.search.log.lock();
            match max {
                Some(m) if l.rows.len() > m => {
                    let at = l.rows.len() - m;
                    l.rows.split_off(at)
                }
                _ => std::mem::take(&mut l.rows),
            }
        };
        if rows.is_empty() {
            return Ok(());
        }
        idx.refresh(&w, rows)
    }

    /// 메시지를 지운 직후 부릅니다. 색인이 있으면 최근에 바뀐 행을 바로 맞춰 지운 글이 RAM 색인에 남지 않게 합니다.
    /// 검색이 색인을 잡고 있으면 기다리지 않고 넘어갑니다(다음 검색이 맞춤).
    pub(crate) fn scrub_search_index(&self) {
        let Some(mut g) = self.search.index.try_lock() else {
            return;
        };
        if g.is_none() {
            return;
        }
        if self.search.log.lock().overflow {
            // 어차피 다시 만들어야 하므로 지금 버려 RAM에서 내립니다.
            *g = None;
            return;
        }
        if let Some(idx) = g.as_mut()
            && let Err(e) = self.sync_rows(idx, Some(SCRUB_MAX_ROWS))
        {
            tracing::debug!(error = %e, "삭제 뒤 검색 색인 맞추기 실패, 색인 버림");
            *g = None;
            *self.search.log.lock() = ChangeLog::default();
        }
    }

    /// 시험용: 색인 버퍼에 이 (정규화한) 글이 남아 있는지.
    #[cfg(test)]
    pub(crate) fn search_index_holds(&self, text: &str) -> bool {
        let mut needle = Vec::new();
        fold_into(&mut needle, text);
        self.search
            .index
            .lock()
            .as_ref()
            .is_some_and(|idx| memchr::memmem::find(&idx.text, &needle).is_some())
    }

    /// 모든 메시지 보기를 한 번 복호화해 색인을 만듭니다. 읽기 연결 하나에서 한 시점으로 저장 키 표와 메시지 표를 각각
    /// 차례로 훑습니다(행마다 키를 찾으면 무작위 접근이라 페이지마다 다시 복호화해 수십 배 느림). 복호화·정규화는 작업
    /// 스레드들이 나눠 하고, 결과는 읽은 순서대로 붙입니다.
    fn build_search_index(&self) -> Result<SearchIndex> {
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, MAX_THREADS);
        let (job_tx, job_rx) = mpsc::sync_channel::<(usize, Vec<RawRow>)>(workers * 2);
        let job_rx = Mutex::new(job_rx);
        let (done_tx, done_rx) = mpsc::channel::<(usize, Vec<Prepared>)>();
        std::thread::scope(|s| {
            for _ in 0..workers {
                let (job_rx, done_tx) = (&job_rx, done_tx.clone());
                s.spawn(move || {
                    loop {
                        let job = job_rx.lock().recv();
                        let Ok((seq, rows)) = job else { break };
                        let out = rows.into_iter().filter_map(prepare).collect();
                        if done_tx.send((seq, out)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(done_tx);
            // 읽기는 따로 한 스레드에서: 이 스레드는 다 된 묶음을 순서대로 붙입니다.
            let reader = s.spawn(move || self.read(|c| read_all_rows(c, &job_tx)));
            let mut idx = SearchIndex::default();
            let mut pending: BTreeMap<usize, Vec<Prepared>> = BTreeMap::new();
            let mut next = 0;
            for (seq, batch) in done_rx {
                pending.insert(seq, batch);
                while let Some(batch) = pending.remove(&next) {
                    for p in batch {
                        idx.push(p);
                    }
                    next += 1;
                }
            }
            reader.join().unwrap_or_else(|_| {
                Err(crate::error::StoreError::Corrupt("검색 색인 읽기 스레드"))
            })?;
            Ok(idx)
        })
    }
}

/// 저장 키 표의 사본(이벤트 ID 순). 색인을 만드는 동안만 두고, 키 버퍼는 늘릴 때와 버릴 때 0으로 덮습니다.
#[derive(Default)]
struct KeyTable {
    ids: Vec<[u8; 32]>,
    keys: Zeroizing<Vec<u8>>,
}

impl KeyTable {
    fn load(c: &Connection) -> Result<KeyTable> {
        let mut t = KeyTable::default();
        // 기본 키 순서로 훑으므로 정렬하지 않고, BLOB 비교(memcmp)가 바이트 배열 순서와 같아 이분 탐색할 수 있습니다.
        let mut stmt = c.prepare("SELECT event_id, key FROM msg_keys ORDER BY event_id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: [u8; 32] = r.get(0)?;
            let key = Zeroizing::new(r.get::<_, [u8; 32]>(1)?);
            if t.keys.capacity() - t.keys.len() < 32 {
                let mut next =
                    Zeroizing::new(Vec::with_capacity((t.keys.capacity() * 2).max(32 << 10)));
                next.extend_from_slice(&t.keys);
                t.keys = next;
            }
            t.ids.push(id);
            t.keys.extend_from_slice(&*key);
        }
        Ok(t)
    }

    fn get(&self, id: &[u8; 32]) -> Option<[u8; 32]> {
        let i = self.ids.binary_search(id).ok()?;
        self.keys[i * 32..(i + 1) * 32].try_into().ok()
    }
}

/// 한 읽기 트랜잭션(한 시점)에서 저장 키 표와 메시지 표를 훑어 묶음으로 넘깁니다.
fn read_all_rows(c: &Connection, jobs: &mpsc::SyncSender<(usize, Vec<RawRow>)>) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    let keys = KeyTable::load(&tx)?;
    let mut stmt = tx.prepare(
        "SELECT rowid, event_id, space, channel, author_user, created_at, content_sealed, anonymous \
         FROM messages WHERE deleted = 0 AND content_sealed IS NOT NULL",
    )?;
    let mut rows = stmt.query([])?;
    let mut batch = Vec::with_capacity(BUILD_BATCH);
    let mut seq = 0;
    while let Some(r) = rows.next()? {
        let id: [u8; 32] = r.get(1)?;
        let Some(key) = keys.get(&id) else { continue };
        batch.push(RawRow {
            rowid: r.get(0)?,
            id,
            space: r.get(2)?,
            channel: r.get(3)?,
            author: r.get(4)?,
            created_at: r.get(5)?,
            sealed: r.get(6)?,
            deleted: false,
            key: Some(key),
            anonymous: r.get(7)?,
        });
        if batch.len() == BUILD_BATCH {
            if jobs.send((seq, std::mem::take(&mut batch))).is_err() {
                return Ok(());
            }
            seq += 1;
        }
    }
    if !batch.is_empty() {
        let _ = jobs.send((seq, batch));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(s: &str) -> String {
        let mut out = Vec::new();
        fold_into(&mut out, s);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn fold_and_terms() {
        assert_eq!(fold("Ｈｅｌｌｏ\t\n  World ﬁle"), "hello world file");
        assert_eq!(fold("ÀÉ 양파"), "àé 양파");
        let terms = |q: &str| -> Vec<String> {
            parse_terms(q)
                .into_iter()
                .map(|t| String::from_utf8(t).unwrap())
                .collect()
        };
        assert_eq!(terms("회의 자료 회의"), ["회의", "자료"]);
        assert_eq!(terms("\"회의  자료\" 회의 자"), ["회의 자료"]);
        assert_eq!(terms("“열린 따옴표"), ["열린 따옴표"]);
        assert!(terms("  \"\" ").is_empty());
    }

    /// 글이 많아 조각으로 나눠 여러 스레드로 훑어도, 하나씩 확인한 결과와 같습니다(조각 경계·글 경계 포함).
    #[test]
    fn parallel_scan_matches_naive() {
        let mut idx = SearchIndex::default();
        let words = ["사과", "배", "포도", "apple", "감", "귤", "딸기"];
        let mut texts = Vec::new();
        for i in 0..40_000u32 {
            let w1 = words[(i % 7) as usize];
            let w2 = words[(i / 7 % 7) as usize];
            let text = format!("{w1} 기록 {i} {w2} 오늘 남긴 메모 조금");
            let mut id = [0u8; 32];
            id[..4].copy_from_slice(&i.to_be_bytes());
            idx.push(Prepared {
                rowid: i64::from(i),
                id,
                space: [1; 16],
                channel: [(i % 3) as u8; 16],
                author: [(i % 5) as u8; 32],
                created_at: i64::from(i % 1000),
                anonymous: false,
                text: Zeroizing::new(fold(&text).into_bytes()),
            });
            texts.push(fold(&text));
        }
        // 일부는 지움(그 자리를 덮음).
        for i in (0..40_000).step_by(11) {
            idx.remove_row(i);
        }
        assert!(idx.text.len() > PARALLEL_MIN_BYTES);
        let all = Scope {
            chans: None,
            author: None,
        };
        for q in ["사과", "포도 감", "기록 1", "\"감 기록\"", "apple 딸기"] {
            let terms = parse_terms(q);
            let got = idx.find(&terms, &all);
            let want: Vec<u32> = (0..40_000u32)
                .filter(|&i| i % 11 != 0)
                .filter(|&i| {
                    terms
                        .iter()
                        .all(|t| memchr::memmem::find(texts[i as usize].as_bytes(), t).is_some())
                })
                .collect();
            assert_eq!(got, want, "{q}");
        }
        // 고르기: 쪽은 전체 정렬한 것의 같은 구간.
        let mut hits = idx.find(&parse_terms("기록"), &all);
        let mut sorted = hits.clone();
        sorted.sort_by(|a, b| newest_first(&idx.docs[*a as usize], &idx.docs[*b as usize]));
        for (offset, limit) in [(0, 50), (50, 50), (1234, 77), (sorted.len() - 3, 50)] {
            let page = idx.page(&mut hits, offset, limit).to_vec();
            let end = (offset + limit).min(sorted.len());
            assert_eq!(page, sorted[offset..end], "{offset}+{limit}");
        }
        // 지운 글이 많아지면 다시 채워도 같은 결과.
        for i in (0..40_000).step_by(2) {
            idx.remove_row(i);
        }
        idx.maybe_compact();
        assert_eq!(idx.dead_docs, 0);
        let got = idx.find(&parse_terms("사과"), &all);
        let want = (0..40_000u32)
            .filter(|&i| i % 11 != 0 && i % 2 != 0 && texts[i as usize].contains("사과"))
            .count();
        assert_eq!(got.len(), want);
    }
}
