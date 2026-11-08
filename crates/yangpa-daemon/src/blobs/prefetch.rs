//! 미리 받기(헤드리스 시더, 시더 설정 `prefetch`).
//!
//! 새 메시지(작성 시각이 [`LIVE_MS`] 안)의 첨부는 `on_message_applied`가 열지 않은 채널이어도 미루지 않고 받습니다
//! ([`policy::prefetches`], 보통 자동 내려받기처럼 자리가 없으면 오래 안 본 첨부를 비움). 지난 메시지의 첨부(따라잡기로
//! 들어온 것 포함)는 미뤄 두고 정리 주기마다(그리고 `seed.set`으로 켤 때) 여기서 채웁니다: 한 번도 받지 않은(저장소에
//! 기록이 없는) 첨부를 최근 것부터, 저장 공간 한도의 남은 자리와 피어 설정 안에서만 넣고, 자리를 만들려고 다른
//! 첨부를 비우지 않습니다(`Want::backfill`). 정리로 비운 첨부는 기록이 남아 다시 넣지 않으므로 받고 비우기를
//! 되풀이하지 않습니다.

use std::collections::HashSet;

use yangpa_core::ids::{BlobId, SpaceId};

use super::{BlobRef, REF_PREFIX, Want, add_want, cache};
use crate::node::Node;
use crate::policy;

/// 한 번에 넣는 보충 수(대기열이 지난 첨부로 넘치지 않게).
const BACKFILL_PER_SWEEP: usize = 64;
/// 미리 받기가 "새 메시지"로 보고 바로 받는 작성 시각 범위(작성자 시계가 조금 틀려도 되게 넉넉히).
pub(crate) const LIVE_MS: u64 = 10 * 60 * 1000;

/// 지난 메시지의 첨부를 받을 블롭에 넣습니다. 넣은 수를 돌려줍니다. 기록을 모두 훑으므로 `spawn_blocking`에서
/// 부릅니다(정리 주기는 통째로, `seed.set`은 [`cache::run_blocking`]으로).
pub(crate) fn backfill(node: &Node) -> usize {
    let spaces: Vec<SpaceId> = node
        .space_ids()
        .into_iter()
        .filter(|s| {
            let p = policy::stored_seeder_policy(node, Some(s));
            p.enabled && p.prefetch && !policy::is_marked_dormant(node, s)
        })
        .collect();
    if spaces.is_empty() {
        return 0;
    }
    let mut room = cache::room_for_backfill(node);
    if room == 0 {
        return 0;
    }
    // 기록이 있는 블롭은 받았거나 받는 중이거나 정리로 비운 것입니다. 블롭마다 묻지 않고 한 번에 읽습니다.
    let Ok(known) = node.store.blob_ids() else {
        return 0;
    };
    let known: HashSet<BlobId> = known.into_iter().collect();
    let rows = node
        .store
        .settings_with_prefix(REF_PREFIX)
        .unwrap_or_default();
    let mut cands: Vec<(BlobId, BlobRef)> = Vec::new();
    for (key, value) in rows {
        let Ok(blob) = key[REF_PREFIX.len()..].parse::<BlobId>() else {
            continue;
        };
        if known.contains(&blob) {
            continue;
        }
        let Ok(refs) = serde_json::from_value::<Vec<BlobRef>>(value) else {
            continue;
        };
        if let Some(r) = refs
            .into_iter()
            .filter(|r| r.verified != Some(false) && spaces.contains(&r.space))
            .max_by_key(|r| r.lamport)
        {
            cands.push((blob, r));
        }
    }
    cands.sort_by(|a, b| b.1.lamport.cmp(&a.1.lamport).then(a.0.cmp(&b.0)));
    let mut added = 0;
    for (blob, r) in cands {
        if added >= BACKFILL_PER_SWEEP {
            break;
        }
        if r.size > room {
            continue;
        }
        if node
            .blobs
            .mgr
            .lock()
            .wants
            .get(&blob)
            .is_some_and(|w| !w.deferred)
        {
            continue;
        }
        let Some(state) = node.space_state(&r.space) else {
            continue;
        };
        if !policy::prefetches(node, &r.space, &state, &r.channel, &r.event, r.data_type)
            || state.attach_limit(&r.channel).is_some_and(|l| r.size > l)
        {
            continue;
        }
        // 지워진 메시지, 차단한 사용자·기기의 첨부는 받지 않습니다(`on_message_applied`와 같게).
        let Ok(Some(rec)) = node.store.get_message(&r.event, None) else {
            continue;
        };
        if rec.deleted
            || rec.space != r.space
            || crate::moderation::is_author_blocked(node, &rec.author, &rec.author_device)
            || node
                .me()
                .is_ok_and(|me| rec.author_device == me.device_id())
        {
            continue;
        }
        let pp = policy::peer_policy(node, Some(&r.space));
        if !pp.auto_download(r.data_type, r.size)
            || crate::ratelimit::blocked(
                node,
                crate::ratelimit::Dir::Down,
                &r.space,
                Some(&r.channel),
                None,
            )
        {
            continue;
        }
        let w = Want {
            space: r.space,
            channel: Some(r.channel),
            data_type: r.data_type,
            size: r.size,
            manual: false,
            priority: pp.priority(r.data_type),
            deferred: false,
            profile: false,
            added_ms: 0,
            backfill: true,
        };
        if add_want(node, blob, w) {
            added += 1;
            room = room.saturating_sub(r.size);
        }
    }
    added
}
