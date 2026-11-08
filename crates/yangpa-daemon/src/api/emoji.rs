//! `emoji.*`: 크루 커스텀 이모지.
//!
//! 이미지는 이모지마다 새 키로 암호화한 블롭으로 이 노드에 저장하고 공간에 묶습니다(닫힌 스웜). 키는 `EmojiAdd`
//! 상태 이벤트(상태 키로 암호화) 안에만 있으므로 멤버만 풀 수 있습니다. 다른 노드로 조각을 옮기는 일은 블롭 영역이
//! 맡습니다. ID는 이미지 해시 앞 64비트입니다(`EmojiId::from_image`).

use std::sync::Arc;

use yangpa_core::blob::DEFAULT_CHUNK_SIZE;
use yangpa_core::body::{CustomEmoji, EventBody, Secret32};
use yangpa_core::emoji::valid_name;
use yangpa_core::ids::{EmojiId, SpaceId};
use yangpa_core::model::{Permissions, SecurityLevel, SpaceState};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{EmojiCreateResult, EmojiView, m};
use yangpa_store::PersistentBlob;

use super::{CallCtx, P, R, empty, rpc};
use crate::author::{self, KeyChoice};
use crate::crews;
use crate::error::{Error, Result};
use crate::node::Node;

/// 파일 앞부분으로 형식을 알아냅니다. 반환: (MIME, 애니메이션인지). 애니메이션 여부는 표시용 추정입니다
/// (APNG의 `acTL`, GIF의 반복 확장·여러 프레임 제어, WebP `VP8X`의 애니메이션 비트).
fn sniff(b: &[u8]) -> Option<(&'static str, bool)> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("image/png", png_is_animated(b)));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        let frames = b.windows(2).filter(|w| w == b"\x21\xf9").count();
        let looped = b.windows(11).any(|w| w == b"NETSCAPE2.0");
        return Some(("image/gif", looped || frames > 1));
    }
    if b.len() >= 21 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let animated = &b[12..16] == b"VP8X" && b[20] & 0x02 != 0;
        return Some(("image/webp", animated));
    }
    None
}

/// PNG 청크를 따라가며 `IDAT` 전에 `acTL`(APNG)이 있는지 봅니다.
fn png_is_animated(b: &[u8]) -> bool {
    let mut i = 8usize;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        match &b[i + 4..i + 8] {
            b"acTL" => return true,
            b"IDAT" => return false,
            _ => {}
        }
        i = i.saturating_add(12).saturating_add(len);
    }
    false
}

/// 이미지를 읽어 확인합니다. 크기는 크루 한도(`max_bytes`)가 있을 때만 봅니다(먼저 파일 크기로, 읽은 뒤 다시).
async fn read_image(path: &str, max_bytes: Option<u64>) -> Result<(Vec<u8>, &'static str, bool)> {
    let too_big = |n: u64| {
        max_bytes
            .filter(|&max| n > max)
            .map(|max| Error::invalid(format!("이 크루의 이모지는 {max}바이트 이하여야 합니다")))
    };
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|_| Error::invalid("이미지 파일을 열 수 없습니다"))?;
    if !meta.is_file() {
        return Err(Error::invalid("이미지 파일을 열 수 없습니다"));
    }
    if let Some(e) = too_big(meta.len()) {
        return Err(e);
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| Error::invalid("이미지 파일을 읽을 수 없습니다"))?;
    if let Some(e) = too_big(bytes.len() as u64) {
        return Err(e);
    }
    let (mime, animated) =
        sniff(&bytes).ok_or_else(|| Error::invalid("PNG·GIF·WebP 이미지만 쓸 수 있습니다"))?;
    Ok((bytes, mime, animated))
}

/// 새 키로 암호화해 블롭으로 저장하고 공간에 묶습니다. 이모지 이벤트가 상태에 들어가기 전에도 저장 공간
/// 정리(`blobs::cache`)가 비우지 않게 등록하자마자 고정합니다(내가 유일한 시더).
fn store_blob(node: &Node, space: &SpaceId, bytes: &[u8]) -> Result<(PersistentBlob, Secret32)> {
    let key = Secret32::random();
    let (ct, manifest) = yangpa_crypto::blob::try_encrypt_blob(
        &key,
        &yangpa_crypto::pack_blob(bytes),
        DEFAULT_CHUNK_SIZE,
    )?;
    let blob = PersistentBlob::new(manifest, SecurityLevel::Normal)?;
    node.store.blob_register(space, &blob)?;
    let cs = blob.manifest().chunk_size as usize;
    let written = node.store.blob_set_pinned(&blob.id(), true).and_then(|_| {
        ct.chunks(cs).enumerate().try_for_each(|(i, chunk)| {
            node.store
                .blob_write_chunk(&blob.id(), i as u32, chunk)
                .map(|_| ())
        })
    });
    if let Err(e) = written {
        let _ = node.store.blob_delete(&blob.id());
        return Err(e.into());
    }
    Ok((blob, key))
}

/// 상태의 이모지 보기(올린 사람 포함).
pub(crate) fn views(state: &SpaceState) -> Vec<EmojiView> {
    state
        .emojis
        .values()
        .map(|e| EmojiView {
            id: e.id,
            name: e.name.clone(),
            animated: e.animated,
            blob: e.blob,
            mime: e.mime.clone(),
            created_by: state.emoji_by.get(&e.id).copied(),
        })
        .collect()
}

pub(crate) async fn list(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::EmojiList>) -> R<m::EmojiList> {
    let (state, _) = rpc(crews::member_state(node, &p.space))?;
    Ok(views(&state))
}

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::EmojiCreate>,
) -> R<m::EmojiCreate> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_EMOJI))?;
    if !valid_name(&p.name) {
        return Err(RpcError::invalid_input(
            "이모지 이름은 2~32자의 글자·숫자·밑줄이어야 합니다",
        ));
    }
    if state.emojis.values().any(|e| e.name == p.name) {
        return Err(RpcError::conflict("같은 이름의 이모지가 있습니다"));
    }
    let max = state.emoji_count_limit();
    if state.emojis.len() >= max {
        return Err(RpcError::invalid_input(format!(
            "이모지는 {max}개까지입니다"
        )));
    }
    let (bytes, mime, animated) = rpc(read_image(&p.path, state.emoji_limits.max_bytes).await)?;
    // 크루 모두가 받는 이미지라 첨부 설정과 상관없이 메타데이터를 지웁니다.
    let bytes = crate::blobs::strip::clean_image_bytes(bytes)?;
    let id = EmojiId::from_image(&bytes);
    if state.emojis.contains_key(&id) {
        return Err(RpcError::conflict("같은 이미지의 이모지가 있습니다"));
    }
    let (blob, key) = rpc(store_blob(node, &p.space, &bytes))?;
    let emoji = CustomEmoji {
        id,
        name: p.name,
        animated,
        blob: blob.id(),
        key,
        mime: mime.into(),
    };
    let r = author::publish(
        node,
        p.space,
        None,
        &EventBody::EmojiAdd { emoji },
        KeyChoice::State,
    )
    .await;
    if let Err(e) = r {
        let _ = node.store.blob_delete(&blob.id());
        return Err(e.into());
    }
    Ok(EmojiCreateResult { id, animated })
}

pub(crate) async fn delete(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::EmojiDelete>,
) -> R<m::EmojiDelete> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_EMOJI))?;
    if !state.emojis.contains_key(&p.id) {
        return Err(RpcError::not_found("없는 이모지"));
    }
    rpc(author::publish(
        node,
        p.space,
        None,
        &EventBody::EmojiRemove { id: p.id },
        KeyChoice::State,
    )
    .await)?;
    Ok(empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_formats_and_animation() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR0123456789abc\0\0\0\0IDAT";
        assert_eq!(sniff(png), Some(("image/png", false)));
        let apng = b"\x89PNG\r\n\x1a\n\0\0\0\x08acTL01234567";
        assert_eq!(sniff(apng), Some(("image/png", true)));
        assert_eq!(sniff(b"GIF89a....."), Some(("image/gif", false)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0".to_vec();
        webp.push(0x02);
        assert_eq!(sniff(&webp), Some(("image/webp", true)));
        assert_eq!(sniff(b"<svg/>"), None);
    }
}
