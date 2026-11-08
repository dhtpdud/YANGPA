//! `blob.*`: 첨부 상태·내려받기·읽기·저장.
//!
//! 담당: B3(블롭·정책). 블롭은 그 공간에서 쓰이는 것(메시지 첨부, 공간 이모지·아이콘, 멤버 프로필 이미지)만
//! 다룹니다. 복호화 키는 `key_from` 메시지의 첨부에서 찾고, 이모지·프로필 이미지는 `key_from`과 무관하게
//! 공간 상태·서명된 프로필에서 찾습니다. 휘발성 채널 첨부(RAM 전용)는 `volatile::attach`가 맡습니다.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use yangpa_core::blob::BlobManifest;
use yangpa_core::body::Secret32;
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_core::model::SpaceState;
use yangpa_crypto::blob::plaintext_chunk_len;
use yangpa_crypto::{BlobReader, ReadError, decrypt_blob_chunk};
use yangpa_rpc::api::{BLOB_READ_MAX_LEN, BlobReadResult, BlobStatusView, m};
use yangpa_rpc::{RpcError, RpcOutput};
use zeroize::Zeroizing;

use super::{CallCtx, P, R, me};
use crate::blobs;
use crate::error::Error;
use crate::node::Node;
use crate::volatile::attach as volatile_blob;

/// 기본 직접 내려받기 우선순위(0~255, 클수록 먼저).
const DEFAULT_FETCH_PRIORITY: u8 = 128;

/// 내가 멤버인 공간의 상태.
fn member_state(node: &Node, space: &SpaceId) -> Result<Arc<SpaceState>, RpcError> {
    let me = me(node)?;
    let state = node
        .space_state(space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    Ok(state)
}

/// 이 공간에서 쓰이는 블롭인지(아니면 있는지도 알리지 않음).
fn known_blob(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    blob: &BlobId,
) -> Result<(), RpcError> {
    if blobs::uses_in_space(node, space, state, blob).is_empty() {
        return Err(RpcError::not_found("이 공간에서 쓰이지 않는 블롭"));
    }
    Ok(())
}

pub(crate) async fn status(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::BlobStatus>,
) -> R<m::BlobStatus> {
    if let Some(f) = volatile_blob::find(node, &p.space, &p.blob, None) {
        return volatile_blob::status(node, _ctx, f).await;
    }
    let state = member_state(node, &p.space)?;
    known_blob(node, &p.space, &state, &p.blob)?;
    let st = node.store.blob_status(&p.blob).map_err(Error::from)?;
    let (have_chunks, total_chunks) = st.map_or((0, 0), |s| (s.have_count, s.chunk_count));
    let seeders_online = blobs::seeders_online(node, &p.space, &p.blob).await;
    Ok(BlobStatusView {
        have_chunks,
        total_chunks,
        seeders_online,
        size: blobs::image_content_size(node, &state, &p.blob)
            .unwrap_or_else(|| blobs::plaintext_size(node, &p.space, &p.blob)),
        downloading: blobs::is_fetching(node, &p.blob),
        pinned: blobs::cache::is_pinned(node, &p.blob),
        kept: blobs::cache::is_kept(node, &p.blob),
        volatile: false,
    })
}

/// 직접 내려받기. 자동 다운로드 한도·피어 설정의 종류와 무관하게 받고, 기다리던 재시도를 지웁니다.
pub(crate) async fn fetch(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::BlobFetch>) -> R<m::BlobFetch> {
    if let Some(f) = volatile_blob::find(node, &p.space, &p.blob, None) {
        if downloads_off(node, &p.space, None) {
            return Err(RpcError::forbidden(DOWNLOADS_OFF));
        }
        return volatile_blob::fetch(node, _ctx, f);
    }
    let state = member_state(node, &p.space)?;
    let priority = p.priority.unwrap_or(DEFAULT_FETCH_PRIORITY);
    let mut want = blobs::manual_want(node, &p.space, &state, &p.blob, priority)
        .ok_or_else(|| RpcError::not_found("이 공간에서 쓰이지 않는 블롭"))?;
    // 화면에 그릴 이미지(프로필·아이콘·이모지, 채널 없음)는 자동 받기 줄에 넣습니다. 직접 받기는 자동 줄보다 늘 먼저
    // 나가고 포기하지 않아, 아무도 갖지 않은 사진 여럿이 받기 자리(Tor로 돌면 하나)를 계속 차지할 수 있습니다.
    if want.channel.is_none() {
        want.manual = false;
    }
    // 받기 제한 0(전체·크루·채널) = 받지 않음. 화면용 이미지는 조용히 넘깁니다.
    if downloads_off(node, &p.space, want.channel.as_ref()) {
        if want.channel.is_none() {
            return Ok(super::empty());
        }
        return Err(RpcError::forbidden(DOWNLOADS_OFF));
    }
    blobs::add_want(node, p.blob, want);
    Ok(super::empty())
}

const DOWNLOADS_OFF: &str =
    "받기 속도 제한이 0이라 받지 않습니다(설정 → 동기화나 크루·채널 속도 제한)";

/// 전체(그 공간의 피어 설정)나 크루·채널 받기 제한이 0인지.
pub(crate) fn downloads_off(node: &Node, space: &SpaceId, channel: Option<&ChannelId>) -> bool {
    use crate::ratelimit::{self, Dir};
    crate::policy::peer_policy(node, Some(space)).downloads_off()
        || ratelimit::blocked(node, Dir::Down, space, channel, None)
}

/// 이 기기에 데이터가 빠진 첨부를 같은 내용(같은 해시)의 로컬 파일로 다시 채웁니다(일반 채널만).
pub(crate) async fn relink(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::BlobRelink>,
) -> R<m::BlobRelink> {
    member_state(node, &p.space)?;
    blobs::relink(node, &p.space, &p.channel, &p.message, &p.blob, &p.path).await?;
    status(
        node,
        ctx,
        yangpa_rpc::api::BlobParams {
            space: p.space,
            blob: p.blob,
        },
    )
    .await
}

/// `blob.paste`: 붙여넣은 첨부 바이트를 RAM에 올립니다(조각씩).
pub(crate) async fn paste(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::BlobPaste>) -> R<m::BlobPaste> {
    me(node)?;
    let (paste, received) = node
        .pastes
        .put(p.paste.as_deref(), p.size, &p.data_base64)?;
    Ok(yangpa_rpc::api::BlobPasteResult { paste, received })
}

/// `blob.paste_discard`: 붙여넣기를 지웁니다(없어도 성공).
pub(crate) async fn paste_discard(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::BlobPasteDiscard>,
) -> R<m::BlobPasteDiscard> {
    node.pastes.discard(&p.paste);
    Ok(super::empty())
}

/// 키와 매니페스트(다 받지 않았어도 됨).
fn open_blob(
    node: &Node,
    space: &SpaceId,
    blob: &BlobId,
    key_from: &EventId,
) -> Result<(Secret32, Arc<yangpa_core::blob::BlobManifest>), RpcError> {
    let state = member_state(node, space)?;
    let key = blobs::key_for(node, space, &state, blob, key_from)
        .ok_or_else(|| RpcError::not_found("이 블롭의 키를 찾을 수 없습니다"))?;
    let manifest = node
        .store
        .blob_manifest(blob)
        .map_err(Error::from)?
        .ok_or_else(|| RpcError::not_found("아직 받지 않은 블롭"))?;
    Ok((key, manifest))
}

fn decrypt_chunk(
    node: &Node,
    blob: &BlobId,
    key: &Secret32,
    manifest: &yangpa_core::blob::BlobManifest,
    index: u32,
) -> Result<Zeroizing<Vec<u8>>, RpcError> {
    // 읽는 중(재생·저장)인 블롭은 잠시 저장 공간 정리에서 뺍니다.
    blobs::cache::touch(node, blob);
    let chunk = node
        .store
        .blob_read_chunk(blob, index)
        .map_err(Error::from)?
        .ok_or_else(|| RpcError::conflict("그 조각을 아직 받지 않았습니다"))?;
    decrypt_blob_chunk(key, manifest, index, &chunk)
        .map(Zeroizing::new)
        .map_err(|_| RpcError::internal("블롭을 복호화할 수 없습니다"))
}

/// 블롭 내용 읽기 실패를 RPC 오류로. 조각을 가져오다 난 오류는 그대로, 머리글·압축이 깨졌으면 내부 오류.
pub(crate) fn read_err(e: ReadError<RpcError>) -> RpcError {
    match e {
        ReadError::Fetch(e) => e,
        ReadError::Crypto(_) => RpcError::internal("블롭 내용을 풀 수 없습니다"),
    }
}

/// `blob.read`의 범위를 읽습니다(일반·휘발성 첨부 공통). `fetch(i)`는 조각 `i`의 평문.
///
/// `index`를 주면 내용을 조각 평문 크기로 나눈 `index`번째 부분, 아니면 내용 `offset`부터 `len`바이트(없으면 끝까지,
/// 최대 8MiB). 내용은 압축 머리글을 벗긴 원래 파일입니다(`yangpa_crypto::blobpack`).
pub(crate) fn read_content<F>(
    manifest: &BlobManifest,
    fetch: F,
    index: Option<u32>,
    offset: Option<u64>,
    len: Option<u64>,
) -> Result<Zeroizing<Vec<u8>>, RpcError>
where
    F: FnMut(u32) -> Result<Zeroizing<Vec<u8>>, RpcError>,
{
    let (offset, len) = match index {
        Some(i) => {
            if offset.is_some() || len.is_some() {
                return Err(RpcError::invalid_input(
                    "index와 offset·len은 함께 쓸 수 없습니다",
                ));
            }
            if i as usize >= manifest.chunks.len() {
                return Err(RpcError::invalid_input("조각 번호 범위 밖"));
            }
            let per = plaintext_chunk_len(manifest);
            (i as u64 * per, Some(per))
        }
        None => {
            if let Some(l) = len
                && l > BLOB_READ_MAX_LEN
            {
                return Err(RpcError::invalid_input(format!(
                    "한 번에 최대 {BLOB_READ_MAX_LEN}바이트까지 읽을 수 있습니다"
                )));
            }
            (offset.unwrap_or(0), len)
        }
    };
    let mut reader = BlobReader::open(manifest, fetch).map_err(read_err)?;
    let total = reader.len();
    if index.is_none() && offset > total {
        return Err(RpcError::invalid_input("offset이 블롭 크기를 넘습니다"));
    }
    let len = len
        .unwrap_or(BLOB_READ_MAX_LEN)
        .min(total.saturating_sub(offset));
    reader.read(offset, len).map_err(read_err)
}

/// 결과가 바이너리 프레임일 수 있어(`binary: true`) [`RpcOutput`]을 직접 돌려줍니다.
///
/// `index`를 주면 그 조각의 평문, 아니면 평문 `offset`부터 `len`바이트(없으면 끝까지, 최대 8MiB).
pub(crate) async fn read(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::BlobRead>,
) -> Result<RpcOutput, RpcError> {
    if let Some(f) = volatile_blob::find(node, &p.space, &p.blob, Some(&p.key_from)) {
        return volatile_blob::read(node, _ctx, f, p);
    }
    let (key, manifest) = open_blob(node, &p.space, &p.blob, &p.key_from)?;
    let data = read_content(
        &manifest,
        |i| decrypt_chunk(node, &p.blob, &key, &manifest, i),
        p.index,
        p.offset,
        p.len,
    )?
    .to_vec();
    if p.binary == Some(true) {
        Ok(RpcOutput::Binary(bytes::Bytes::from(data)))
    } else {
        RpcOutput::json(&BlobReadResult {
            data_base64: Some(base64::engine::general_purpose::STANDARD.encode(&data)),
        })
    }
}

/// 다 받은 블롭을 평문으로 파일에 씁니다. 경로는 절대 경로이고, 이미 있는 파일은 덮어쓰지 않습니다.
pub(crate) async fn save(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::BlobSave>) -> R<m::BlobSave> {
    if let Some(f) = volatile_blob::find(node, &p.space, &p.blob, Some(&p.key_from)) {
        return volatile_blob::save(node, _ctx, f, p).await;
    }
    let path = PathBuf::from(&p.path);
    if !path.is_absolute() {
        return Err(RpcError::invalid_input("저장 경로는 절대 경로여야 합니다"));
    }
    let (key, manifest) = open_blob(node, &p.space, &p.blob, &p.key_from)?;
    if !blobs::is_downloaded(node, &p.blob) {
        return Err(RpcError::conflict("아직 다 받지 않은 블롭입니다"));
    }
    let node2 = node.clone();
    let blob = p.blob;
    tokio::task::spawn_blocking(move || {
        let mut f: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    RpcError::conflict("이미 있는 파일은 덮어쓰지 않습니다")
                }
                _ => RpcError::invalid_input("저장할 파일을 만들 수 없습니다"),
            })?;
        let written = (|| -> Result<(), RpcError> {
            let mut reader = BlobReader::open(&manifest, |i| {
                decrypt_chunk(&node2, &blob, &key, &manifest, i)
            })
            .map_err(read_err)?;
            reader
                .for_each(|piece| {
                    f.write_all(piece)
                        .map_err(|_| RpcError::internal("파일에 쓸 수 없습니다"))
                })
                .map_err(read_err)?;
            f.sync_all()
                .map_err(|_| RpcError::internal("파일에 쓸 수 없습니다"))?;
            mark_from_internet(&path);
            Ok(())
        })();
        if written.is_err() {
            drop(f);
            let _ = std::fs::remove_file(&path);
        }
        written
    })
    .await
    .map_err(|_| RpcError::internal("저장 작업이 멈췄습니다"))??;
    Ok(super::empty())
}

/// 남에게서 받아 저장한 파일에 Windows의 "인터넷에서 받음" 표시(Mark of the Web)를 붙입니다. 이 표시가 있어야 Office의
/// 매크로 차단과 SmartScreen이 상대가 보낸 문서·실행 파일을 막습니다. 어디서 받았는지(주소)는 적지 않습니다. 대체 데이터
/// 스트림이 없는 드라이브(FAT·exFAT 등)에서는 붙지 않아도 저장은 그대로 둡니다.
pub(crate) fn mark_from_internet(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let mut ads = path.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let _ = std::fs::write(ads, "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    #[cfg(not(windows))]
    let _ = path;
}
