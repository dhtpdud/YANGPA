//! 개인 메모: 다른 사용자에 대해 내가 적어 두는 글(그 사람에게는 보이지 않음).
//!
//! 공간 이벤트로 퍼뜨리지 않고 내 기기끼리만 직접 주고받습니다. 내용은 받는 기기의 상자 키로 봉인하고
//! (`seal_to`, 보낸·받는 기기를 AD로 묶음), 서명된 내 기기 목록에서 활성인 기기만 보내고 받습니다.
//! 같은 메모가 여러 기기에서 바뀌면 나중 시각이 이깁니다(시각이 같으면 글이 큰 쪽, 어느 기기에서나 같은 결과).
//!
//! - 쓰면: 저장하고 지금 인증된 내 다른 기기에게 그 메모 하나를 보냅니다.
//! - 내 다른 기기와 인증되면(`identity::on_authenticated`): 내 메모 전부를 묶음 상한(4MiB·4096개) 안의 여러 묶음으로
//!   나눠 차례로 보내고(마지막 묶음에 `full`), 상대는 받은 뒤 자기 메모 전부를 같은 방식으로 보내 줍니다. 잘라 버리는
//!   메모는 없습니다.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{DeviceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{SocialReply, SocialRequest, WireErrorCode, YpRequest, YpResponse};
use yangpa_crypto::{open_sealed, seal_to};
use yangpa_net::PeerId;
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::node::Node;

/// 메모마다 마지막으로 바뀐 시각(지운 것도 남겨 지움이 퍼지게 함).
const META_PREFIX: &str = "note.meta.";
const AD_TAG: &[u8] = b"yangpa/v1/notes";
/// 한 번에 보내는 메모 묶음(평문) 상한. 메모 하나는 저장소 한도(64KiB)를 넘지 않으므로 넘치면 다음 묶음으로 나눕니다.
const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
/// 한 묶음의 메모 수 상한(받는 쪽은 이보다 많으면 뒤를 버림).
const MAX_ITEMS: usize = 4096;
/// 묶음 크기 어림에 메모마다 더하는 몫(사용자 ID·시각·직렬화).
const ITEM_OVERHEAD: usize = 64;

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Meta {
    updated_at: u64,
}

/// 주고받는 메모 하나. 빈 글은 지움입니다.
#[derive(Clone, Serialize, Deserialize)]
struct NoteItem {
    user: UserId,
    text: String,
    updated_at: u64,
}

#[derive(Serialize, Deserialize)]
struct NotesPayload {
    /// 보낸 쪽 메모 전부인지(그렇다면 받는 쪽도 자기 것 전부를 답으로 보냄).
    full: bool,
    items: Vec<NoteItem>,
}

fn meta_key(user: &UserId) -> String {
    format!("{META_PREFIX}{user}")
}

fn meta_of(node: &Node, user: &UserId) -> Option<Meta> {
    node.store
        .get_setting::<Meta>(&meta_key(user))
        .ok()
        .flatten()
}

fn ad(from: &DeviceId, to: &DeviceId) -> Vec<u8> {
    let mut v = AD_TAG.to_vec();
    v.extend_from_slice(from.as_bytes());
    v.extend_from_slice(to.as_bytes());
    v
}

/// `note.get`: 없으면 빈 글.
pub(crate) fn get(node: &Node, user: &UserId) -> Result<String> {
    Ok(node.store.get_note(user)?.unwrap_or_default())
}

/// `note.set`: 저장하고 내 다른 기기에 보냅니다. 빈 글은 지움입니다.
pub(crate) fn set(node: &Arc<Node>, user: &UserId, text: &str) -> Result<()> {
    let me = node.me()?;
    if *user == me.user() {
        return Err(Error::invalid("자기 자신에게는 메모를 남길 수 없습니다"));
    }
    let now = Timestamp::now().0;
    // 시계가 뒤로 가도 내 쓰기가 앞선 값을 이기도록 늘 앞선 시각보다 크게 둡니다.
    let updated_at = meta_of(node, user).map_or(now, |m| now.max(m.updated_at + 1));
    node.store.set_note(user, text)?;
    node.store
        .put_setting(&meta_key(user), &Meta { updated_at })?;
    let item = NoteItem {
        user: *user,
        text: text.to_owned(),
        updated_at,
    };
    let targets: Vec<(PeerId, DeviceId)> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, d)| *u == me.user() && *d != me.device_id())
        .map(|(p, _, d)| (p, d))
        .collect();
    if !targets.is_empty() {
        node.spawn_detached(move |node| async move {
            for (peer, device) in targets {
                let payload = NotesPayload {
                    full: false,
                    items: vec![item.clone()],
                };
                if let Err(e) = send(&node, peer, device, &payload).await {
                    debug!(error = %e, "메모 전달 실패(다음 연결 때 맞춤)");
                }
            }
        });
    }
    Ok(())
}

/// 내 메모 전부(최근 것부터).
fn all_items(node: &Node) -> Vec<NoteItem> {
    let Ok(rows) = node.store.settings_with_prefix(META_PREFIX) else {
        return Vec::new();
    };
    let mut items: Vec<NoteItem> = rows
        .into_iter()
        .filter_map(|(key, value)| {
            let user: UserId = key.strip_prefix(META_PREFIX)?.parse().ok()?;
            let meta: Meta = serde_json::from_value(value).ok()?;
            let text = node.store.get_note(&user).ok()?.unwrap_or_default();
            Some(NoteItem {
                user,
                text,
                updated_at: meta.updated_at,
            })
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
    items
}

/// 메모들을 묶음 상한(크기·개수) 안의 묶음들로 나눕니다(순서 유지). 빈 목록이면 빈 묶음 하나.
fn chunk_items(items: Vec<NoteItem>) -> Vec<Vec<NoteItem>> {
    let mut out = Vec::new();
    let mut cur: Vec<NoteItem> = Vec::new();
    let mut size = 0usize;
    for i in items {
        let n = i.text.len().saturating_add(ITEM_OVERHEAD);
        if !cur.is_empty() && (size.saturating_add(n) > MAX_PAYLOAD || cur.len() >= MAX_ITEMS) {
            out.push(std::mem::take(&mut cur));
            size = 0;
        }
        size = size.saturating_add(n);
        cur.push(i);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// 내 메모 전부를 여러 묶음으로 차례로 보냅니다. `full`이면 마지막 묶음에 표시해 상대도 자기 것 전부를 보내게 합니다.
async fn send_all(node: &Arc<Node>, peer: PeerId, device: DeviceId, full: bool) -> Result<()> {
    let chunks = chunk_items(all_items(node));
    let last = chunks.len().saturating_sub(1);
    for (i, items) in chunks.into_iter().enumerate() {
        let payload = NotesPayload {
            full: full && i == last,
            items,
        };
        send(node, peer, device, &payload).await?;
    }
    Ok(())
}

/// 받은 메모를 합칩니다(나중 시각이 이김).
fn merge(node: &Node, items: Vec<NoteItem>) {
    let Ok(me) = node.me() else { return };
    // 같은 사용자가 여러 번 오면 이기는 하나만 봅니다.
    let mut best: BTreeMap<UserId, NoteItem> = BTreeMap::new();
    for i in items.into_iter().take(MAX_ITEMS) {
        if i.user == me.user() {
            continue;
        }
        match best.get(&i.user) {
            Some(b) if (b.updated_at, &b.text) >= (i.updated_at, &i.text) => {}
            _ => {
                best.insert(i.user, i);
            }
        }
    }
    for (user, i) in best {
        let local = meta_of(node, &user).map_or(0, |m| m.updated_at);
        let wins = match i.updated_at.cmp(&local) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => {
                let text = node
                    .store
                    .get_note(&user)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                i.text > text
            }
        };
        if !wins {
            continue;
        }
        if let Err(e) = node.store.set_note(&user, &i.text) {
            debug!(error = %e, "받은 메모 저장 실패");
            continue;
        }
        let _ = node.store.put_setting(
            &meta_key(&user),
            &Meta {
                updated_at: i.updated_at,
            },
        );
    }
}

/// 내 활성 기기의 상자 공개 키.
fn box_key_of(node: &Node, device: &DeviceId) -> Option<BoxPublicKey> {
    let me = node.me().ok()?;
    me.devices
        .list
        .active()
        .find(|e| e.device == *device)
        .map(|e| e.box_key.clone())
}

fn seal_payload(node: &Node, to: &DeviceId, payload: &NotesPayload) -> Result<Vec<u8>> {
    let me = node.me()?;
    let key = box_key_of(node, to).ok_or_else(|| Error::forbidden("내 활성 기기가 아닙니다"))?;
    let plain = Zeroizing::new(
        postcard::to_allocvec(payload).map_err(|_| Error::internal("메모 묶음 직렬화 실패"))?,
    );
    Ok(seal_to(&[(*to, key)], &ad(&me.device_id(), to), &plain))
}

fn open_payload(node: &Node, from: &DeviceId, sealed: &[u8]) -> Result<NotesPayload> {
    let me = node.me()?;
    let plain = open_sealed(
        &me.device_id(),
        &me.device.boxkey,
        &ad(from, &me.device_id()),
        sealed,
    )
    .map_err(|_| Error::forbidden("메모 묶음을 열 수 없음"))?;
    postcard::from_bytes(&plain).map_err(|_| Error::invalid("메모 묶음 형식"))
}

async fn send(
    node: &Arc<Node>,
    peer: PeerId,
    device: DeviceId,
    payload: &NotesPayload,
) -> Result<()> {
    let sealed = seal_payload(node, &device, payload)?;
    let req = YpRequest::Social(SocialRequest::Notes { sealed });
    match node.net.request(peer, req).await? {
        YpResponse::Ok => Ok(()),
        YpResponse::Social(SocialReply { notes, .. }) => {
            if !notes.is_empty() {
                let reply = open_payload(node, &device, &notes)?;
                merge(node, reply.items);
            }
            Ok(())
        }
        YpResponse::Error { code, .. } => Err(Error::forbidden(format!("메모 거절: {code:?}"))),
        _ => Err(Error::internal("메모 응답이 아님")),
    }
}

/// 내 다른 기기와 인증되었을 때: 서로의 메모 전부를 맞춥니다.
pub(crate) async fn sync_with(node: &Arc<Node>, peer: PeerId, device: DeviceId) {
    let Ok(me) = node.me() else { return };
    if device == me.device_id() || box_key_of(node, &device).is_none() {
        return;
    }
    if let Err(e) = send_all(node, peer, device, true).await {
        debug!(error = %e, "메모 맞추기 실패");
    }
}

/// 받은 `Social::Notes`(인증된 피어). 내 활성 기기에서 온 것만 받습니다. 상대 메모 전부의 마지막 묶음이면(`full`) 내
/// 메모 전부를 묶음으로 나눠 따로 보내 줍니다(답 하나에 다 담지 못할 수 있으므로).
pub(crate) fn handle_notes(
    node: &Arc<Node>,
    peer: PeerId,
    user: UserId,
    device: DeviceId,
    sealed: &[u8],
) -> YpResponse {
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "신원 없음");
    };
    if user != me.user() || device == me.device_id() || box_key_of(node, &device).is_none() {
        return YpResponse::error(WireErrorCode::Forbidden, "내 기기가 아님");
    }
    if sealed.len() > MAX_PAYLOAD * 2 {
        return YpResponse::error(WireErrorCode::BadRequest, "메모 묶음이 너무 큼");
    }
    let payload = match open_payload(node, &device, sealed) {
        Ok(p) => p,
        Err(_) => return YpResponse::error(WireErrorCode::BadRequest, "메모 묶음을 열 수 없음"),
    };
    let full = payload.full;
    merge(node, payload.items);
    if full {
        node.spawn_detached(move |node| async move {
            if let Err(e) = send_all(&node, peer, device, false).await {
                debug!(error = %e, "메모 답 보내기 실패");
            }
        });
    }
    YpResponse::Ok
}
