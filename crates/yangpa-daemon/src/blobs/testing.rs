//! 블롭·정책 영역의 통합 시험 도우미(`testkit::blobs`). 공개 API가 아니며 언제든 바뀝니다.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use yangpa_core::body::{Attachment, CustomEmoji, EventBody, MessageBody, Secret32};
use yangpa_core::event::EventKind;
use yangpa_core::ids::{BlobId, ChannelId, EmojiId, EventId, SpaceId};
use yangpa_core::model::{Member, Permissions, SpaceKind, SpaceSettings};

use super::{FetchStats, media};
use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::spaces::{self, NewSpace};
use crate::{identity, messages, policy, sync_service};

fn member(user: yangpa_core::ids::UserId) -> Member {
    Member {
        user,
        roles: Vec::new(),
        nickname: None,
        automation: false,
    }
}

/// `owner`가 크루 공간을 만들고 `others`를 처음 멤버로 넣습니다(초대 흐름 없이 만든 이벤트를 그대로 적용).
/// 다른 노드들도 크루 키 그룹에 들어갑니다. (공간, 첫 채널).
pub async fn create_group_space(
    owner: &Arc<Node>,
    others: &[&Arc<Node>],
) -> Result<(SpaceId, ChannelId)> {
    let me = owner.me()?;
    let mut members = vec![member(me.user())];
    let mut devices = vec![me.devices.clone()];
    for o in others {
        let om = o.me()?;
        members.push(member(om.user()));
        devices.push(om.devices.clone());
    }
    let spec = NewSpace {
        kind: SpaceKind::Crew,
        settings: SpaceSettings {
            name: "시험 크루".into(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        everyone: Permissions::DEFAULT_EVERYONE,
        members,
        devices,
        first_channel: Some("일반".into()),
    };
    let l = ingest::lock(owner).await;
    let (space, evs, report) = spaces::create_space_locked(owner, &l, spec)?;
    drop(l);
    ingest::after_unlock(owner, space, &report);
    let genesis = evs
        .iter()
        .find(|e| e.header.kind == EventKind::SpaceCreate)
        .map(|e| e.id())
        .ok_or_else(|| Error::internal("공간 생성 이벤트 없음"))?;
    for o in others {
        spaces::ensure_runtime(o, &space, Some(genesis))?;
        ingest::ingest(o, space, evs.clone(), Source::Bootstrap(owner.peer_id())).await?;
        sync_service::refresh_space_peers(o, &space);
    }
    sync_service::refresh_space_peers(owner, &space);
    let channel = owner
        .space_state(&space)
        .and_then(|s| s.channels.keys().next().copied())
        .ok_or_else(|| Error::internal("첫 채널 없음"))?;
    Ok((space, channel))
}

/// 이미지 바이트를 블롭으로 저장하고 공간 이모지로 추가·전파합니다(B1의 `emoji.create` 전 시험용).
/// (블롭, `EmojiAdd` 이벤트).
pub async fn add_emoji(
    node: &Arc<Node>,
    space: SpaceId,
    name: &str,
    bytes: &[u8],
) -> Result<(BlobId, EventId)> {
    let mime = media::sniff_magic(bytes)
        .filter(|m| yangpa_core::emoji::ALLOWED_MIME.contains(m))
        .ok_or_else(|| Error::invalid("이모지는 png·gif·webp"))?;
    let (blob, key) = super::import_bytes(node, &space, bytes)?;
    let emoji = CustomEmoji {
        id: EmojiId(rand::random::<u64>() >> 1),
        name: name.into(),
        animated: false,
        blob,
        key,
        mime: mime.into(),
    };
    let ev = author::publish(
        node,
        space,
        None,
        &EventBody::EmojiAdd { emoji },
        KeyChoice::State,
    )
    .await?;
    Ok((blob, ev.id()))
}

/// 이 노드가 보내는 블롭 조각의 한 바이트를 바꿉니다(받는 쪽 검증 시험).
pub fn corrupt_uploads(node: &Node, on: bool) {
    node.blobs.test.corrupt_uploads.store(on, Ordering::Relaxed);
}

/// 받은 조각 + 요청 중인 조각이 `at`에 닿으면 더 요청하지 않습니다(`None`이면 해제).
pub fn hold_downloads(node: &Node, at: Option<u32>) {
    *node.blobs.test.hold_at.lock() = at;
    node.blobs.wake.notify_one();
}

/// 이 피어들에게서 받을 때의 조각 요청 한도(피어마다, 블롭 하나 전체). `None`이면 직접 연결이라 고정 한도 없이 잰
/// 속도에 맞춥니다.
pub fn chunk_window(node: &Node, peers: &[yangpa_net::PeerId]) -> Option<(u32, u32)> {
    match super::chunk_window(node, peers) {
        super::ChunkWindow::Direct => None,
        super::ChunkWindow::Fixed(per_peer, total) => Some((per_peer, total)),
    }
}

/// 이 노드가 내주는 조각 답을 `ms`만큼 늦춥니다(왕복이 긴 연결 흉내, 0이면 끔).
pub fn delay_serving(node: &Node, ms: u64) {
    node.blobs.test.serve_delay_ms.store(ms, Ordering::Relaxed);
}

/// `delay_serving`으로 늦춘 조각 요청마다 그 요청이 왔을 때 함께 늦춰지던 요청 수(자기 포함, 온 차례대로). 받는 쪽이
/// 왕복 하나에 겹쳐 보낸 정도라, 걸린 시간과 달리 PC가 바빠도 거의 그대로입니다.
pub fn serving_overlaps(node: &Node) -> Vec<u32> {
    node.blobs.test.serving_overlaps.lock().clone()
}

/// 이 노드가 지금 실행에서 블롭을 받은 기록.
pub fn fetch_stats(node: &Node, blob: &BlobId) -> FetchStats {
    node.blobs
        .test
        .stats
        .lock()
        .get(blob)
        .cloned()
        .unwrap_or_default()
}

/// 받는 중인 블롭에 대해 조각을 가졌다고 답한 피어 수(지금 실행에서).
pub fn seeders_seen(node: &Node, blob: &BlobId) -> usize {
    node.blobs
        .test
        .seeders_seen
        .lock()
        .get(blob)
        .copied()
        .unwrap_or(0)
}

/// 저장소의 (가진 조각 수, 전체 조각 수).
pub fn chunk_progress(node: &Node, blob: &BlobId) -> Option<(u32, u32)> {
    node.store
        .blob_status(blob)
        .ok()
        .flatten()
        .map(|s| (s.have_count, s.chunk_count))
}

/// 휴면이라 주기 작업이 건너뛴 횟수.
pub fn dormant_skips(node: &Node) -> u64 {
    policy::dormant_skips(node)
}

/// 지금 휴면으로 표시돼 있는지.
pub fn is_dormant(node: &Node, space: &SpaceId) -> bool {
    policy::is_marked_dormant(node, space)
}

/// 정리 주기(이모지·친구 프로필 이미지 받기)를 지금 한 번 돌립니다.
pub fn sweep_now(node: &Arc<Node>) {
    super::sweep(node);
}

/// 피어 설정이 바뀐 뒤처럼 받을 블롭을 다시 거릅니다.
pub fn peer_policy_changed_now(node: &Arc<Node>) {
    super::on_peer_policy_changed(node);
}

/// 바이트를 이 공간의 블롭으로 저장합니다(메시지 없이). (블롭, 키).
pub fn import_blob(node: &Node, space: SpaceId, bytes: &[u8]) -> Result<(BlobId, Secret32)> {
    super::import_bytes(node, &space, bytes)
}

/// 첨부 하나를 그대로 실은 메시지를 만들어 적용·전파합니다. `message.send`와 달리 블롭을 만들지 않으므로
/// 남의 블롭 ID·틀린 키·거짓 크기를 실은 메시지(베낀 첨부)를 시험할 때 씁니다.
pub async fn send_attachment(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    attachment: Attachment,
) -> Result<EventId> {
    let body = EventBody::Message(MessageBody {
        text: "첨부".into(),
        attachments: vec![attachment],
        ..Default::default()
    });
    let l = ingest::lock(node).await;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let (hid, key) = messages::send_key_locked(node, &l, &space, &channel, &state)?;
    let (ev, report) = author::publish_locked(
        node,
        &l,
        space,
        Some(channel),
        &body,
        KeyChoice::HistoryWith(hid, key),
        &[],
    )?;
    drop(l);
    ingest::after_unlock(node, space, &report);
    let id = ev.id();
    sync_service::propagate(node, space, vec![ev]);
    Ok(id)
}

/// 이 노드가 가진 메시지에 실린 그 블롭의 키.
pub fn attachment_key(node: &Node, event: &EventId, blob: &BlobId) -> Option<Secret32> {
    super::attachment_key(node, event, blob)
}

/// `owner`의 프로필 이미지를 `avatar`(키 `key`)로 바꿔 서명하고, `on` 노드에 그 프로필을 받은 것처럼 저장합니다
/// (프로필 전파가 아직 없어 시험에서 직접 넣음). 남의 블롭 ID를 자기 아바타라고 주장하는 시험에 씁니다.
pub fn claim_avatar(on: &Node, owner: &Node, avatar: BlobId, key: Option<Secret32>) -> Result<()> {
    let me = owner.me()?;
    let mut p = me.profile.profile.clone();
    p.avatar = Some(avatar);
    p.avatar_key = key;
    p.version = p.version.saturating_add(1);
    let signed = p.sign(&me.identity.signing)?;
    identity::accept_profile(on, &signed)?;
    super::rebuild_profile_index(on);
    Ok(())
}

/// 정리(지워진 메시지의 첨부 지우기)를 지금 한 번 돌립니다.
pub fn cleanup_now(node: &Node) {
    super::cleanup(node);
}

/// 이 블롭을 지금 받는 작업이 돌고 있는지.
pub fn download_active(node: &Node, blob: &BlobId) -> bool {
    node.blobs.mgr.lock().active.contains(blob)
}

/// 이 블롭을 실은 메시지 기록 중 키가 확인된 것의 (공간, 이벤트).
pub fn verified_refs(node: &Node, blob: &BlobId) -> Vec<(SpaceId, EventId)> {
    super::verified_refs(node, blob)
        .into_iter()
        .map(|r| (r.space, r.event))
        .collect()
}

/// 이 노드가 아는 사용자의 프로필 이미지 키(나면 비밀값, 남이면 받은 서명 프로필에서).
pub fn known_image_key(
    node: &Node,
    user: &yangpa_core::ids::UserId,
    blob: &BlobId,
) -> Option<Secret32> {
    super::profile_key(node, user, blob)
}

/// 이 노드에서 메시지가 지워졌는지(삭제 이벤트를 적용함).
pub fn is_deleted(node: &Node, event: &EventId) -> bool {
    node.store.is_deleted(event).unwrap_or(false)
}

// ───────────── 저장 공간 정리(X4) ─────────────

/// 블롭의 마지막 접근 시각을 정합니다(정리 순서를 시험에서 정할 때, 밀리초).
pub fn set_last_access(node: &Node, blob: &BlobId, at_ms: u64) -> Result<bool> {
    Ok(node
        .store
        .blob_set_last_access(blob, yangpa_core::time::Timestamp(at_ms))?)
}

/// 디스크 여유 공간을 이 값으로 보게 합니다(`None`이면 실제 값).
pub fn set_disk_free(node: &Node, free: Option<u64>) {
    super::cache::set_free_override(node, free);
}

/// 한도·디스크 여유에 맞게 지금 한 번 정리합니다. (비운 수, 비운 바이트).
pub fn enforce_cache_now(node: &Node) -> (u32, u64) {
    super::cache::enforce(node)
}

/// 저장소에 있는 블롭의 조각 바이트(기록이 없으면 `None`).
pub fn stored_bytes(node: &Node, blob: &BlobId) -> Option<u64> {
    node.store
        .blob_status(blob)
        .ok()
        .flatten()
        .map(|s| s.stored_bytes)
}

/// 올리는 첨부를 멈춰 두는 고리([`pause_uploads`]).
#[derive(Default)]
pub struct UploadPause {
    /// (멈춘 곳에 닿음, 풀림).
    state: std::sync::Mutex<(bool, bool)>,
    cv: std::sync::Condvar,
}

impl UploadPause {
    /// 올리는 첨부가 멈춘 곳에 닿았는지.
    pub fn reached(&self) -> bool {
        self.state.lock().map(|s| s.0).unwrap_or(true)
    }

    /// 멈춘 업로드를 이어 가게 합니다.
    pub fn release(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.1 = true;
        }
        self.cv.notify_all();
    }
}

/// 이 노드가 다음에 암호화해 저장하는 블롭(첨부·아이콘·프로필 이미지)이 첫 조각을 쓴 뒤 멈추게 합니다(한 번).
/// 시험이 실패해도 멈춘 작업이 영영 남지 않게 1분 뒤에는 스스로 풀립니다.
pub fn pause_uploads(node: &Node) -> Arc<UploadPause> {
    let p = Arc::new(UploadPause::default());
    *node.blobs.test.upload_pause.lock() = Some(p.clone());
    p
}

/// `prepare::encrypt_into_store`가 첫 조각을 쓴 뒤 부릅니다. 고리가 없으면 바로 돌아갑니다.
pub(super) fn upload_checkpoint(node: &Node) {
    let Some(p) = node.blobs.test.upload_pause.lock().take() else {
        return;
    };
    let Ok(mut s) = p.state.lock() else { return };
    s.0 = true;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !s.1 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match p.cv.wait_timeout(s, left) {
            Ok((g, _)) => s = g,
            Err(_) => return,
        }
    }
}

/// 남에게 내주거나 로컬 API로 읽어 정리를 미루는 블롭인지(시각과 무관하게 기록이 있는지).
pub fn serving_recorded(node: &Node, blob: &BlobId) -> bool {
    super::cache::serving_recorded(node, blob)
}

/// 이 블롭을 시험이 풀 때까지 "내주는 중"으로 둡니다(`false`면 기록을 지움). 30초 보류 시간에 기대지 않고
/// 정리가 건너뛰는지 보게 합니다.
pub fn hold_serving(node: &Node, blob: &BlobId, on: bool) {
    super::cache::hold_serving(node, blob, on);
}

/// 정리가 목록을 읽은 뒤 `blob`을 고정하고(사용자가 정리와 동시에 고정한 경우) 모두 비웁니다. (비운 수, 바이트).
pub fn clear_pinning_midway(node: &Node, space: SpaceId, blob: BlobId) -> (u32, u64) {
    super::cache::clear_pinning_midway(node, space, blob)
}

/// 미리 받기가 바로 받는 "새 메시지" 범위(밀리초)를 바꿉니다. `Some(0)`이면 모든 메시지를 따라잡기로 들어온 지난
/// 메시지처럼 미룹니다(지난 시각으로 쓴 메시지를 만들지 않고 보게). `None`이면 기본값.
pub fn set_prefetch_live_ms(node: &Node, ms: Option<u64>) {
    *node.blobs.test.live_ms.lock() = ms;
}

/// 미리 받기 보충을 지금 한 번 돌립니다. 넣은 수.
pub fn backfill_now(node: &Node) -> usize {
    super::prefetch::backfill(node)
}

/// 이 블롭이 받을 블롭에 있으면 그 요청이 미뤄졌는지(`None`이면 받을 블롭이 아님).
pub fn want_deferred(node: &Node, blob: &BlobId) -> Option<bool> {
    node.blobs.mgr.lock().wants.get(blob).map(|w| w.deferred)
}
