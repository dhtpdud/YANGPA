//! 보내는 쪽: 파일·바이트를 암호화된 블롭으로 만들어 저장합니다(첨부, 프로필 이미지, 이모지).
//!
//! 파일은 두 번 읽습니다. 처음엔 조각 해시만 계산해 매니페스트(블롭 ID)를 정하고 등록한 뒤, 같은 키로 다시
//! 암호화하며 조각을 저장합니다(파일 전체를 메모리에 올리지 않음). 두 번째에 파일이 바뀌었으면 저장소가 조각
//! 해시로 거절하므로, 바뀐 내용의 암호문은 저장되지도 나가지도 않고 블롭을 지운 뒤 실패합니다.
//! 썸네일: 32KiB 이하 png·jpeg·gif·webp는 원본 자체를, 더 큰 것은 풀어서 긴 변 320px 이하 JPEG로 줄여 싣습니다
//! (받는 쪽이 원본 없이 미리보기를 봄). 영상은 디코더가 없어 MP4·MOV의 재생 길이만 채우고, 동영상·음성의
//! 썸네일·크기·길이는 앱이 만들어 넘긴 것을 검사해 싣습니다(`hints`).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use yangpa_core::body::{Attachment, Secret32};
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_core::model::SecurityLevel;
use yangpa_crypto::{BlobEncryptor, CryptoError, pack_blob, packed_source};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::AttachmentInput;
use yangpa_store::{PersistentBlob, StoreError};
use yangpa_sync::state::limits::{MAX_ATTACHMENTS, MAX_FILE_NAME_CHARS, MAX_THUMBNAIL_BYTES};

use super::hints::{self, Hints};
use super::media::{
    LOCAL_DECODE, THUMBNAIL_MIMES, has_mp4_duration, image_size, make_thumbnail, mp4_duration_ms,
    sniff_magic, sniff_mime,
};
use super::{IMAGE_SECRET_PREFIX, MAX_PROFILE_IMAGE_BYTES, PROFILE_SPACE};
use crate::error::Error;
use crate::node::Node;
use crate::paste::{Shared, Source};

/// 형식 판별·이미지 크기에 쓰는 머리 바이트.
const HEAD_BYTES: usize = 64 * 1024;
/// 원본을 그대로 썸네일로 싣는 이미지 최대 크기(이벤트 본문이 커지지 않게, 검증 한도 64KiB보다 작게).
const INLINE_THUMBNAIL_BYTES: u64 = 32 * 1024;
/// 줄인 썸네일 최대 크기(검증 한도 안).
const MADE_THUMBNAIL_BYTES: usize = if MAX_THUMBNAIL_BYTES < 48 * 1024 {
    MAX_THUMBNAIL_BYTES
} else {
    48 * 1024
};
/// 썸네일을 만들려고 풀어 볼 이미지 파일 최대 크기.
const THUMBNAIL_SOURCE_BYTES: u64 = 50 * 1024 * 1024;

fn changed() -> RpcError {
    RpcError::conflict("파일이 읽는 중에 바뀌었습니다. 다시 시도하세요")
}

fn crypto_err(e: CryptoError) -> RpcError {
    match e {
        CryptoError::BlobLength => changed(),
        CryptoError::TooLarge(_) => RpcError::invalid_input("파일이 너무 큽니다"),
        other => RpcError::from(Error::from(other)),
    }
}

fn store_err(e: StoreError) -> RpcError {
    RpcError::from(Error::from(e))
}

/// 절대 경로의 보통 파일을 엽니다. (파일, 길이).
fn open_file(path: &Path, max: u64, what: &str) -> Result<(File, u64), RpcError> {
    if !path.is_absolute() {
        return Err(RpcError::invalid_input(format!(
            "{what} 경로는 절대 경로여야 합니다"
        )));
    }
    let f = File::open(path)
        .map_err(|_| RpcError::invalid_input(format!("{what} 파일을 열 수 없습니다")))?;
    let meta = f
        .metadata()
        .map_err(|_| RpcError::invalid_input(format!("{what} 파일을 열 수 없습니다")))?;
    if !meta.is_file() {
        return Err(RpcError::invalid_input(format!(
            "{what}는 보통 파일이어야 합니다"
        )));
    }
    if meta.len() > max {
        return Err(RpcError::invalid_input(format!(
            "{what} 파일이 너무 큽니다(최대 {max}바이트)"
        )));
    }
    Ok((f, meta.len()))
}

fn read_head<R: Read + Seek>(f: &mut R) -> Result<Vec<u8>, RpcError> {
    let mut head = Vec::with_capacity(HEAD_BYTES);
    f.by_ref()
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut head)
        .and_then(|_| f.seek(SeekFrom::Start(0)))
        .map_err(|_| RpcError::invalid_input("파일을 읽을 수 없습니다"))?;
    Ok(head)
}

/// 파일을 새 키로 암호화해 `space`에 등록·저장하고 고정합니다.
///
/// 등록하자마자 고정합니다. 조각을 다 쓴 뒤에 고정하면 큰 파일을 암호화하는 동안 저장 공간 정리(`cache`)가 이
/// 블롭을 받은 첨부로 보고 비울 수 있고, 그러면 내가 유일한 시더인 첨부가 조각 일부만 남은 채 나갑니다.
fn encrypt_into_store<R: Read + Seek>(
    node: &Node,
    space: &SpaceId,
    f: &mut R,
    len: u64,
    chunk_size: u32,
    security: SecurityLevel,
) -> Result<(BlobId, Secret32), RpcError> {
    let key = Secret32::random();
    // 1차: 조각 해시만 계산해 매니페스트를 정합니다.
    let manifest = {
        let (src, plen) = packed_source(BufReader::new(&mut *f), len).map_err(crypto_err)?;
        let mut enc = BlobEncryptor::new(&key, src, plen, chunk_size).map_err(crypto_err)?;
        while enc.next_chunk().map_err(crypto_err)?.is_some() {}
        enc.finish().map_err(crypto_err)?
    };
    let pb = PersistentBlob::new(manifest, security).map_err(store_err)?;
    let id = pb.id();
    let store = &node.store;
    store.blob_register(space, &pb).map_err(store_err)?;
    // 2차: 같은 키·조각 번호로 다시 암호화해 저장합니다. 파일이 바뀌었으면 해시가 달라 저장소가 거절합니다.
    let written = (|| -> Result<(), RpcError> {
        store.blob_set_pinned(&id, true).map_err(store_err)?;
        f.seek(SeekFrom::Start(0))
            .map_err(|_| RpcError::invalid_input("파일을 읽을 수 없습니다"))?;
        let (src, plen) = packed_source(BufReader::new(&mut *f), len).map_err(crypto_err)?;
        let mut enc = BlobEncryptor::new(&key, src, plen, chunk_size).map_err(crypto_err)?;
        while let Some((i, data)) = enc.next_chunk().map_err(crypto_err)? {
            match store.blob_write_chunk(&id, i, &data) {
                Ok(_) => {}
                Err(StoreError::InvalidInput(_)) => return Err(changed()),
                Err(e) => return Err(store_err(e)),
            }
            if i == 0 {
                super::testing::upload_checkpoint(node);
            }
        }
        enc.finish().map_err(crypto_err)?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = store.blob_delete(&id);
        return Err(e);
    }
    Ok((id, key))
}

fn check_file_name(name: &str) -> Result<(), RpcError> {
    if name.trim().is_empty()
        || name.chars().count() > MAX_FILE_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(RpcError::invalid_input(format!(
            "첨부 이름은 1~{MAX_FILE_NAME_CHARS}자이고 제어 문자가 없어야 합니다"
        )));
    }
    Ok(())
}

/// 첨부 원본: 파일 경로 또는 붙여넣기(`blob.paste`).
pub(crate) enum Origin {
    Path(PathBuf),
    Paste(Shared),
}

impl Origin {
    /// 첨부 입력의 원본과 이름. 붙여넣기는 경로가 없어 이름을 꼭 받습니다.
    pub(crate) fn of(node: &Node, inp: &AttachmentInput) -> Result<(Origin, String), RpcError> {
        if let Some(id) = &inp.paste {
            let name = inp
                .name
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            check_file_name(&name)?;
            return Ok((Origin::Paste(node.pastes.get(id)?), name));
        }
        let path = PathBuf::from(&inp.path);
        if !path.is_absolute() {
            return Err(RpcError::invalid_input("첨부 경로는 절대 경로여야 합니다"));
        }
        let name = match &inp.name {
            Some(n) => n.trim().to_owned(),
            None => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        check_file_name(&name)?;
        Ok((Origin::Path(path), name))
    }

    /// 열어 (원본, 길이, 형식 판별에 쓸 이름 경로). 한도를 넘으면 `too_big(max)`. `strip`이면 메타데이터를 뺀
    /// 원본과 그 길이를 돌려줍니다(`strip`).
    pub(crate) fn open(
        self,
        name: &str,
        max: u64,
        strip: bool,
        too_big: impl Fn(u64) -> RpcError,
    ) -> Result<(Source, u64, PathBuf), RpcError> {
        let (src, len, path) = self.open_raw(name, max, too_big)?;
        if !strip {
            return Ok((src, len, path));
        }
        let (src, len) = super::strip::clean(src, len)?;
        Ok((src, len, path))
    }

    fn open_raw(
        self,
        name: &str,
        max: u64,
        too_big: impl Fn(u64) -> RpcError,
    ) -> Result<(Source, u64, PathBuf), RpcError> {
        match self {
            Origin::Path(path) => {
                let f = File::open(&path)
                    .map_err(|_| RpcError::invalid_input("첨부 파일을 열 수 없습니다"))?;
                let meta = f
                    .metadata()
                    .map_err(|_| RpcError::invalid_input("첨부 파일을 열 수 없습니다"))?;
                if !meta.is_file() {
                    return Err(RpcError::invalid_input("첨부는 보통 파일이어야 합니다"));
                }
                if meta.len() > max {
                    return Err(too_big(max));
                }
                Ok((Source::File(f), meta.len(), path))
            }
            Origin::Paste(data) => {
                let len = data.as_ref().len() as u64;
                if len > max {
                    return Err(too_big(max));
                }
                Ok((Source::mem(data), len, PathBuf::from(name)))
            }
        }
    }

    /// 열지 않고 아는 길이(한도·RAM 예산 확인용).
    pub(crate) fn len(&self) -> Result<u64, RpcError> {
        match self {
            Origin::Path(path) => std::fs::metadata(path)
                .map(|m| m.len())
                .map_err(|_| RpcError::invalid_input("첨부 파일을 열 수 없습니다")),
            Origin::Paste(data) => Ok(data.as_ref().len() as u64),
        }
    }
}

fn too_big(max: u64) -> RpcError {
    RpcError::invalid_input(format!(
        "첨부 파일이 너무 큽니다(이 채널은 하나에 {}까지)",
        size_text(max)
    ))
}

/// 사람이 읽는 크기(오류 메시지용): 1MiB 이상이면 MiB·GiB로.
pub(crate) fn size_text(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    let (v, unit) = match n {
        n if n >= GIB => (n as f64 / GIB as f64, "GiB"),
        n if n >= MIB => (n as f64 / MIB as f64, "MiB"),
        n => return format!("{n}바이트"),
    };
    let s = format!("{v:.1}");
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// 보낼 첨부 하나의 한도: 이 기기 설정과 채널 한도 중 작은 쪽.
pub(crate) fn send_limit(base: u64, channel_limit: Option<u64>) -> u64 {
    channel_limit.map_or(base, |l| l.min(base))
}

/// 채널 한도가 0이면 첨부를 금지한 채널입니다(빈 파일도 막음).
pub(crate) fn check_attach_allowed(channel_limit: Option<u64>) -> Result<(), RpcError> {
    if channel_limit == Some(0) {
        return Err(RpcError::invalid_input("이 채널은 첨부를 받지 않습니다"));
    }
    Ok(())
}

struct Job {
    origin: Origin,
    name: String,
    hints: Hints,
    strip: bool,
}

fn attachment(
    node: &Node,
    space: &SpaceId,
    job: Job,
    max: u64,
    chunk_size: u32,
    security: SecurityLevel,
) -> Result<Attachment, RpcError> {
    let (mut f, len, path) = job.origin.open(&job.name, max, job.strip, too_big)?;
    let head = read_head(&mut f)?;
    let mime = sniff_mime(&head, &path);
    let (mut width, mut height) = if mime.starts_with("image/") {
        image_size(&head).map_or((None, None), |(w, h)| (Some(w), Some(h)))
    } else {
        (None, None)
    };
    let thumbable = THUMBNAIL_MIMES.contains(&mime.as_str());
    let mut thumbnail = if thumbable && len <= INLINE_THUMBNAIL_BYTES && head.len() as u64 == len {
        head
    } else {
        Vec::new()
    };
    // 썸네일·길이는 있으면 좋은 정보라, 만들지 못해도 첨부는 그대로 보냅니다.
    if thumbnail.is_empty() && thumbable && len <= THUMBNAIL_SOURCE_BYTES {
        if let Some(t) = make_thumbnail(
            BufReader::new(&mut f),
            &mime,
            MADE_THUMBNAIL_BYTES,
            LOCAL_DECODE,
        ) {
            thumbnail = t.jpeg;
            // 회전(EXIF)을 반영한 크기.
            (width, height) = (Some(t.width), Some(t.height));
        }
        rewind(&mut f)?;
    }
    let mut duration_ms = if has_mp4_duration(&mime) {
        let d = mp4_duration_ms(&mut f, len);
        rewind(&mut f)?;
        d
    } else {
        None
    };
    // 앱이 만든 동영상·음성 썸네일·크기·길이(검사한 뒤, 데몬이 읽은 길이가 우선).
    if let Some(h) = hints::apply(job.hints, &mime, duration_ms, MADE_THUMBNAIL_BYTES)? {
        if thumbnail.is_empty() {
            thumbnail = h.thumbnail;
        }
        if width.is_none() {
            (width, height) = (h.width, h.height);
        }
        duration_ms = h.duration_ms;
    }
    let (blob, key) = encrypt_into_store(node, space, &mut f, len, chunk_size, security)?;
    Ok(Attachment {
        blob,
        key,
        name: job.name,
        mime,
        size: len,
        width,
        height,
        duration_ms,
        thumbnail,
    })
}

fn rewind<R: Seek>(f: &mut R) -> Result<(), RpcError> {
    f.seek(SeekFrom::Start(0))
        .map(|_| ())
        .map_err(|_| RpcError::invalid_input("파일을 읽을 수 없습니다"))
}

/// `message.send`의 첨부 경로(절대 경로) → 암호화해 저장한 블롭과 `Attachment`. 하나라도 실패하면 이번에 만든
/// 블롭을 모두 지웁니다.
pub(crate) async fn prepare_attachments(
    node: &Arc<Node>,
    space: &SpaceId,
    channel: &ChannelId,
    inputs: &[AttachmentInput],
) -> Result<Vec<Attachment>, RpcError> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    if inputs.len() > MAX_ATTACHMENTS {
        return Err(RpcError::invalid_input(format!(
            "첨부는 {MAX_ATTACHMENTS}개까지입니다"
        )));
    }
    let state = node
        .space_state(space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    let ch = state
        .channels
        .get(channel)
        .ok_or_else(|| RpcError::not_found("없는 채널"))?;
    check_attach_allowed(state.attach_limit(channel))?;
    // 휘발성 채널의 첨부는 디스크에 두지 않습니다(B2의 휘발성 경로가 맡음).
    if ch.is_volatile() {
        return Err(RpcError::invalid_input(
            "휘발성 채널의 첨부는 이 경로로 보낼 수 없습니다",
        ));
    }
    let security = ch.security;
    let mut jobs = Vec::with_capacity(inputs.len());
    for inp in inputs {
        let (origin, name) = Origin::of(node, inp)?;
        let hints = hints::parse(inp)?;
        jobs.push(Job {
            origin,
            name,
            hints,
            strip: !inp.keep_metadata,
        });
    }
    let max = send_limit(
        node.cfg.sharing.max_attachment_bytes,
        state.attach_limit(channel),
    );
    let node = node.clone();
    let space = *space;
    let chunk_size = node.cfg.sharing.chunk_size;
    tokio::task::spawn_blocking(move || {
        let mut out: Vec<Attachment> = Vec::with_capacity(jobs.len());
        for job in jobs {
            match attachment(&node, &space, job, max, chunk_size, security) {
                Ok(a) => out.push(a),
                Err(e) => {
                    for a in &out {
                        let _ = node.store.blob_delete(&a.blob);
                    }
                    return Err(e);
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|_| RpcError::internal("첨부 준비 작업이 멈췄습니다"))?
}

fn mismatch() -> RpcError {
    RpcError::invalid_input("파일 내용이 첨부와 달라 다시 이을 수 없습니다(해시가 다름)")
}

/// `blob.relink`: 이 기기에 데이터가 빠진 첨부(일반 채널)를 같은 내용의 파일로 다시 채우고 보관(고정)합니다.
///
/// 첨부의 키와 같은 조각 크기로 파일을 다시 암호화해 매니페스트를 만들고, 그 ID(조각 해시들의 해시)가 첨부의
/// 블롭 ID와 같을 때만 조각을 저장합니다. 파일 경로는 기억하지 않습니다(데몬은 늘 암호화해 둔 사본으로 내줌).
pub(crate) async fn relink(
    node: &Arc<Node>,
    space: &SpaceId,
    channel: &ChannelId,
    message: &EventId,
    blob: &BlobId,
    path: &str,
) -> Result<(), RpcError> {
    let state = node
        .space_state(space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    let ch = state
        .channels
        .get(channel)
        .ok_or_else(|| RpcError::not_found("없는 채널"))?;
    // 휘발성 채널의 첨부는 디스크에 두지 않습니다.
    if ch.is_volatile() {
        return Err(RpcError::invalid_input(
            "휘발성 채널의 첨부는 다시 이을 수 없습니다",
        ));
    }
    let security = ch.security;
    let rec = node
        .store
        .get_message(message, None)
        .map_err(store_err)?
        .filter(|r| r.space == *space && r.channel == *channel);
    let att = rec
        .as_ref()
        .and_then(|r| r.content.as_ref())
        .and_then(|c| c.body.attachments.iter().find(|a| a.blob == *blob))
        .ok_or_else(|| RpcError::not_found("이 메시지에 그런 첨부가 없습니다"))?;
    let (key, size) = (att.key.clone(), att.size);
    let path = PathBuf::from(path);
    let max = node.cfg.sharing.max_attachment_bytes;
    // 매니페스트가 남아 있으면 그 조각 크기로, 없으면 지금 설정으로(블롭 ID가 조각 크기에 따라 달라짐).
    let chunk_size = node
        .store
        .blob_manifest(blob)
        .map_err(store_err)?
        .map_or(node.cfg.sharing.chunk_size, |m| m.chunk_size);
    // 내가 올린 첨부처럼 늘 남기던 블롭이면 그대로 두고, 아니면 다 쓴 뒤 사용자 고정(보관)으로 바꿉니다(풀 수 있게).
    let was_kept = node
        .store
        .blob_status(blob)
        .map_err(store_err)?
        .is_some_and(|s| s.pinned);
    let node2 = node.clone();
    let (space2, blob2) = (*space, *blob);
    let written = tokio::task::spawn_blocking(move || {
        let manifest_of = |f: &mut Source, len: u64| -> Result<_, RpcError> {
            let mut enc = {
                let (src, plen) =
                    packed_source(BufReader::new(&mut *f), len).map_err(crypto_err)?;
                BlobEncryptor::new(&key, src, plen, chunk_size).map_err(crypto_err)?
            };
            while enc.next_chunk().map_err(crypto_err)?.is_some() {}
            let m = enc.finish().map_err(crypto_err)?;
            rewind(f)?;
            Ok(m)
        };
        let (f, len) = open_file(&path, max, "첨부")?;
        let mut f = Source::File(f);
        let raw = if len == size {
            Some(manifest_of(&mut f, len)?).filter(|m| m.id() == blob2)
        } else {
            None
        };
        // 메타데이터를 지우고 보낸 첨부면 원본 파일에서 같은 방식으로 지운 내용이 맞습니다.
        let manifest = match raw {
            Some(m) => m,
            None => {
                let (clean, n) = super::strip::clean(f, len)?;
                f = clean;
                if n != size {
                    return Err(mismatch());
                }
                Some(manifest_of(&mut f, n)?)
                    .filter(|m| m.id() == blob2)
                    .ok_or_else(mismatch)?
            }
        };
        let pb = PersistentBlob::new(manifest, security).map_err(store_err)?;
        let store = &node2.store;
        store.blob_register(&space2, &pb).map_err(store_err)?;
        // 쓰는 동안 저장 공간 정리가 비우지 않게 잠시 고정합니다.
        store.blob_set_pinned(&blob2, true).map_err(store_err)?;
        let (src, plen) = packed_source(BufReader::new(&mut f), size).map_err(crypto_err)?;
        let mut enc = BlobEncryptor::new(&key, src, plen, chunk_size).map_err(crypto_err)?;
        // 이미 가진 조각은 저장소가 건너뜁니다. 중간에 파일이 바뀌면 거기서 멈추고, 그 전까지 쓴 조각은 맞는 것이라 둡니다.
        while let Some((i, data)) = enc.next_chunk().map_err(crypto_err)? {
            match store.blob_write_chunk(&blob2, i, &data) {
                Ok(_) => {}
                Err(StoreError::InvalidInput(_)) => return Err(changed()),
                Err(e) => return Err(store_err(e)),
            }
        }
        enc.finish().map_err(crypto_err)?;
        Ok(())
    })
    .await
    .map_err(|_| RpcError::internal("다시 잇기 작업이 멈췄습니다"))?;
    if !was_kept {
        if written.is_ok() {
            super::cache::pin(node, *space, *blob).map_err(RpcError::from)?;
        }
        let _ = node.store.blob_set_pinned(blob, false);
    }
    written?;
    super::on_relinked(node, space, blob);
    Ok(())
}

/// `profile.set`의 이미지 경로(절대 경로) → 블롭. png·jpeg·gif·webp, 8MiB 이하만. 키는 비밀값으로 두고
/// `profile_image_key`로 서명된 프로필에 실립니다.
pub(crate) async fn import_profile_image(node: &Arc<Node>, path: &str) -> Result<BlobId, RpcError> {
    let (id, key) = import_image(node, PROFILE_SPACE, path, "프로필 이미지").await?;
    if let Err(e) = node
        .store
        .put_secret(&format!("{IMAGE_SECRET_PREFIX}{id}"), &key.0)
    {
        let _ = node.store.blob_delete(&id);
        return Err(store_err(e));
    }
    Ok(id)
}

/// 크루 아이콘(`crew.update`의 `icon_path`) → 그 공간에 등록한 블롭과 키. 키는 호출자가 공간 설정
/// (`SpaceSettings::icon_key`, 상태 레인)에 실어 멤버만 열게 합니다. 형식·크기 제한은 프로필 이미지와 같습니다.
pub(crate) async fn import_space_image(
    node: &Arc<Node>,
    space: &SpaceId,
    path: &str,
) -> Result<(BlobId, Secret32), RpcError> {
    import_image(node, *space, path, "크루 아이콘").await
}

/// 이미지 파일을 `space`에 등록·암호화해 저장하고 고정합니다.
async fn import_image(
    node: &Arc<Node>,
    space: SpaceId,
    path: &str,
    what: &'static str,
) -> Result<(BlobId, Secret32), RpcError> {
    let path = PathBuf::from(path);
    let node = node.clone();
    let chunk_size = node.cfg.sharing.chunk_size;
    tokio::task::spawn_blocking(move || {
        let (mut f, len) = open_file(&path, MAX_PROFILE_IMAGE_BYTES, what)?;
        let head = read_head(&mut f)?;
        if !sniff_magic(&head).is_some_and(|m| THUMBNAIL_MIMES.contains(&m)) {
            return Err(RpcError::invalid_input(format!(
                "{what}는 png·jpeg·gif·webp만 됩니다"
            )));
        }
        // 친구·크루 모두가 받는 이미지라 첨부 설정과 상관없이 메타데이터를 지웁니다.
        let (mut src, len) = super::strip::clean_image(Source::File(f), len)?;
        encrypt_into_store(
            &node,
            &space,
            &mut src,
            len,
            chunk_size,
            SecurityLevel::Normal,
        )
    })
    .await
    .map_err(|_| RpcError::internal(format!("{what} 준비 작업이 멈췄습니다")))?
}

/// 메모리의 바이트를 새 키로 암호화해 `space`에 등록·저장하고 고정합니다(공간 이모지처럼 작은 것).
/// 돌려준 키는 그 블롭을 가리키는 이벤트 본문(예: `CustomEmoji::key`)에 실어 보냅니다. 등록하자마자 고정합니다
/// ([`encrypt_into_store`]와 같은 까닭).
pub(crate) fn import_bytes(
    node: &Node,
    space: &SpaceId,
    bytes: &[u8],
) -> crate::error::Result<(BlobId, Secret32)> {
    let key = Secret32::random();
    let (ct, manifest) = yangpa_crypto::blob::try_encrypt_blob(
        &key,
        &pack_blob(bytes),
        node.cfg.sharing.chunk_size,
    )?;
    let chunk_size = manifest.chunk_size as usize;
    let pb = PersistentBlob::new(manifest, SecurityLevel::Normal)?;
    let id = pb.id();
    node.store.blob_register(space, &pb)?;
    let written = node.store.blob_set_pinned(&id, true).and_then(|_| {
        ct.chunks(chunk_size)
            .enumerate()
            .try_for_each(|(i, c)| node.store.blob_write_chunk(&id, i as u32, c).map(|_| ()))
    });
    if let Err(e) = written {
        let _ = node.store.blob_delete(&id);
        return Err(e.into());
    }
    Ok((id, key))
}
