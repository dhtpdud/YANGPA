//! 키 저장(데몬 쪽): 상태 키 행, 채널 뿌리 캐시(뿌리 저장소), 키 그룹 멤버 상태·확정 기기 표의 잠긴 저장.
//!
//! - **상태 키**: 크루 상태 키 S = state_key(크루 R)는 범위가 공간 ID인 행, 비공개 그룹 상태 키 S_g는 범위가
//!   (공간 ‖ 그룹)인 행입니다. 상태 이벤트·크루 `KeyUpdate`·공간 스트림 키 묶음은 크루 행만 엽니다([`crew_state_key`]):
//!   그룹 멤버가 S_g로 상태 이벤트를 쓰면 그룹 밖 멤버는 못 읽어 상태가 갈리므로, 모두가 같은 이유로 못 엽니다.
//! - **지금 상태 키**: 크루 키 그룹의 사슬 끝을 적용했으면 그 S([`crate::keygroup::current_crew_key`]), 아직이면(처음
//!   상태 키 k0, 자기 가입 갱신을 아직 적용하지 못한 가입자) 설정 `statekey.current.<공간>`에 둔 키입니다.
//! - **뿌리 저장소**: 채널 뿌리 ID → 뿌리. 뿌리는 그룹 키(R·T)에서 파생하므로 저장은 캐시입니다(연 기기가 씀). 모르는
//!   ID는 보관한 그룹 상태에서 다시 파생해 보고, 그래도 없으면 멤버에게 묻습니다(`keygroup::relay`). 보낼 뿌리는 정규
//!   사슬 끝에서만 고릅니다([`send_root`]).
//! - **멤버 상태·표**: 키 그룹 멤버 키(`MemberKeys`, 순번마다 몇 개)는 공간마다 비밀값 하나, 확정 기기 표
//!   (`KeyTree`·`CoverGroup`)는 그룹마다 칸 두 개(`kg.tg.<공간>.<그룹>.<0|1>`)와 그룹 목록 행(`kg.ti.<공간>`)으로 잠긴
//!   저장소에 둡니다(휘발성 키는 여기 오지 않음). 그룹마다 따로 두어야 4.5만 대 크루에서 행 하나가 비밀값 상한(16MiB)을
//!   넘지 않고, 바뀐 그룹만 씁니다.

use serde::{Deserialize, Serialize};
use yangpa_core::body::{Secret32, SharedKey, state_key_id};
use yangpa_core::ids::{ChannelId, ChannelRootId, KeyGroupId, SpaceId, StateKeyId};
use yangpa_core::time::Timestamp;
use yangpa_store::{KeyEntry, KeyKind};
use zeroize::Zeroizing;

use crate::error::Result;
use crate::node::Node;

/// 가입자에게 봉인해 주는 최근 상태 키 수(체크포인트가 있으면 그 크루 머리 뒤 것 가운데 최근 것). 그보다 오래된 상태
/// 이벤트의 키는 가입자가 열지 못한 이벤트를 보고 멤버에게 묻습니다(`GetStateKeys`, `keygroup::relay`). 그래서 가입 묶음
/// 크기가 크루 나이와 체크포인트 주기와 상관없이 묶이고(키 하나 약 50B, 64개 약 3KB), 크루 키가 아무리 많이 회전해도
/// 가입자가 옛 상태 이벤트를 영영 기다리지 않습니다.
pub(crate) const JOINER_STATE_KEYS: usize = 64;

// ───────────── 상태 키 ─────────────

fn group_scope(space: &SpaceId, group: &KeyGroupId) -> Vec<u8> {
    let mut v = space.as_bytes().to_vec();
    v.extend_from_slice(group.as_bytes());
    v
}

/// 이 공간의 상태 키(크루 S든 그룹 S_g든). 채널 스트림 키 묶음처럼 그룹 상태 키로도 쓰는 이벤트에.
pub(crate) fn state_key(node: &Node, space: &SpaceId, id: &StateKeyId) -> Result<Option<Secret32>> {
    Ok(node
        .store
        .get_key_entry(KeyKind::State, id.as_bytes())?
        .filter(|e| e.space == Some(*space))
        .map(|e| e.key))
}

/// 크루 상태 키만(상태 이벤트·크루 갱신·공간 스트림 키 묶음).
pub(crate) fn crew_state_key(
    node: &Node,
    space: &SpaceId,
    id: &StateKeyId,
) -> Result<Option<Secret32>> {
    Ok(node
        .store
        .get_key_entry(KeyKind::State, id.as_bytes())?
        .filter(|e| e.space == Some(*space) && e.scope == space.as_bytes())
        .map(|e| e.key))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct CurrentStateKey {
    id: StateKeyId,
    /// 확정 기기가 받은편지함으로 보낸 묶음에서 정한 키면 그 묶음의 순위(논리 시계). 더 나중 묶음의 키로만 바꿉니다
    /// (가입자는 지난 키를 오래된 것부터 여러 묶음으로 받음).
    rank: Option<u64>,
}

fn current_key_setting(space: &SpaceId) -> String {
    format!("statekey.current.{space}")
}

fn member_secret(space: &SpaceId) -> String {
    format!("kg.m.{space}")
}

/// 예전(그룹을 한 행에 모두 둔) 표 행. 읽지 않고 지우기만 합니다.
fn legacy_table_secret(space: &SpaceId) -> String {
    format!("kg.t.{space}")
}

fn table_index_secret(space: &SpaceId) -> String {
    format!("kg.ti.{space}")
}

fn group_row_secret(space: &SpaceId, group: &KeyGroupId, slot: u8) -> String {
    format!("kg.tg.{space}.{group}.{slot}")
}

/// 공간을 지울 때 지금 상태 키 설정과 키 그룹 멤버 상태·표도 지웁니다(키 행은 `Store::purge_space`가 지움).
pub(crate) fn forget_space(node: &Node, space: &SpaceId) -> Result<()> {
    node.store.delete_setting(&current_key_setting(space))?;
    node.store.delete_secret(&member_secret(space))?;
    if let Some(bytes) = load_member_row(node, space, None)?
        && let Ok(idx) = postcard::from_bytes::<MemberIndex>(&bytes)
    {
        for g in &idx.groups {
            delete_member_row(node, space, g)?;
        }
    }
    node.store.delete_secret(&member_index_secret(space))?;
    save_chain_bases(node, space, &[], &[])?;
    node.store.delete_secret(&chain_bases_secret(space))?;
    delete_table_rows(node, space)?;
    Ok(())
}

/// 크루 상태 키를 저장합니다. `promote`면 지금 키 후보입니다: 아직 지금 키가 없거나, `rank`가 있고 지금 키도 순위로 정한
/// 것인데 그보다 나중 묶음일 때 지금 키가 됩니다(키 그룹 사슬 끝을 적용한 뒤에는 그 키가 앞섬).
pub(crate) fn put_state_key(
    node: &Node,
    space: &SpaceId,
    id: &StateKeyId,
    key: &Secret32,
    promote: bool,
    rank: Option<u64>,
) -> Result<()> {
    node.store.put_key_entry(&KeyEntry {
        kind: KeyKind::State,
        id: id.as_bytes().to_vec(),
        scope: space.as_bytes().to_vec(),
        space: Some(*space),
        epoch: None,
        not_before: None,
        key: key.clone(),
        created_at: Timestamp::now(),
    })?;
    if !promote {
        return Ok(());
    }
    let setting = current_key_setting(space);
    let cur: Option<CurrentStateKey> = node.store.get_setting(&setting)?;
    if replaces_current(cur, id, rank) {
        node.store
            .put_setting(&setting, &CurrentStateKey { id: *id, rank })?;
    }
    Ok(())
}

/// 키 그룹 상태에서 파생한 상태 키를 저장합니다(크루면 크루 행, 아니면 그룹 행). 새로 넣었으면 그 ID.
pub(crate) fn put_derived_state_key(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    key: &Secret32,
    seq: u64,
) -> Result<(StateKeyId, bool)> {
    let id = state_key_id(space, key);
    let scope = if *group == KeyGroupId::crew(space) {
        space.as_bytes().to_vec()
    } else {
        group_scope(space, group)
    };
    let new = node.store.put_key_entry(&KeyEntry {
        kind: KeyKind::State,
        id: id.as_bytes().to_vec(),
        scope,
        space: Some(*space),
        // 그룹 순번을 적어 두어 가입자 몫(최근 상태 키)을 순번으로 고릅니다(늦게 물어 받은 옛 키가 최근 것을 밀어내지 않게).
        epoch: Some(seq),
        not_before: None,
        key: key.clone(),
        created_at: Timestamp::now(),
    })?;
    if !new {
        node.store
            .set_key_epoch(KeyKind::State, id.as_bytes(), seq)?;
    }
    Ok((id, new))
}

/// 새 상태 키가 지금 상태 키를 바꾸는지([`put_state_key`]).
fn replaces_current(cur: Option<CurrentStateKey>, id: &StateKeyId, rank: Option<u64>) -> bool {
    match (cur, rank) {
        (None, _) => true,
        (Some(c), Some(r)) => c.rank.is_some_and(|o| r > o) && c.id != *id,
        (Some(_), None) => false,
    }
}

/// 상태 이벤트를 쓸 때 쓰는 지금 상태 키: 크루 키 그룹 사슬 끝의 S, 아직 없으면 설정의 키.
pub(crate) fn current_state_key(
    node: &Node,
    space: &SpaceId,
) -> Result<Option<(StateKeyId, Secret32)>> {
    if let Some(k) = crate::keygroup::current_crew_key(node, space) {
        return Ok(Some(k));
    }
    let cur: Option<CurrentStateKey> = node.store.get_setting(&current_key_setting(space))?;
    let Some(cur) = cur else { return Ok(None) };
    Ok(crew_state_key(node, space, &cur.id)?.map(|k| (cur.id, k)))
}

/// 이 공간의 크루 상태 키 가운데 체크포인트가 은퇴시키지 않은 것(오래된 것부터, 지금 키를 마지막에). 시험 도구가
/// 씁니다. 공간 코어 잠금을 쥔 채 부르지 않습니다.
pub(crate) fn all_state_keys(node: &Node, space: &SpaceId) -> Result<Vec<SharedKey>> {
    let mut entries = node.store.keys_in_scope(KeyKind::State, space.as_bytes())?;
    // keys_in_scope는 새 것부터. 오래된 것부터 담고 지금 키를 마지막에 둡니다.
    entries.reverse();
    let current = current_state_key(node, space)?.map(|(id, _)| id);
    let core = node.spaces.read().get(space).map(|rt| rt.core.clone());
    let retired: std::collections::HashSet<StateKeyId> = match &core {
        Some(c) => {
            let c = c.lock();
            entries
                .iter()
                .filter_map(|e| StateKeyId::from_slice(&e.id).ok())
                .filter(|id| c.resolver.is_retired_state_key(id))
                .collect()
        }
        None => Default::default(),
    };
    let mut out = Vec::new();
    let mut last = None;
    for e in entries {
        let Ok(id) = StateKeyId::from_slice(&e.id) else {
            continue;
        };
        if retired.contains(&id) && Some(id) != current {
            continue;
        }
        let k = SharedKey::State { id, key: e.key };
        if Some(id) == current {
            last = Some(k);
        } else {
            out.push(k);
        }
    }
    out.extend(last);
    Ok(out)
}

/// 가입자에게 줄 상태 키(오래된 것부터, 지금 키를 마지막에). 바탕 체크포인트가 있으면 그 뒤 이벤트를 여는 키만:
/// 본문의 유효한 옛 키(`valid_state_keys`, 체크포인트 이벤트의 키 포함)와 체크포인트의 크루 머리 뒤 크루 갱신에서
/// 나온 키 가운데 최근 [`JOINER_STATE_KEYS`]개입니다. 가입자는 경계 아래 이벤트를 받지 않으므로 그 앞 키는 기록을 읽을
/// 때만, 체크포인트가 늦어 머리 뒤 키가 그보다 많으면 그 나머지도 물어 받습니다(`GetStateKeys`). 체크포인트가 없으면
/// 최근 [`JOINER_STATE_KEYS`]개입니다. 저장소에서 그만큼만 읽어(공간 나이와 상관없이) 적용 잠금 안에서도 쌉니다.
pub(crate) fn keys_for_joiner(
    node: &Node,
    space: &SpaceId,
    base: Option<&yangpa_sync::CheckpointBase>,
) -> Result<Vec<SharedKey>> {
    let current = current_state_key(node, space)?;
    let n = match node
        .keygroup
        .joiner_keys
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        0 => JOINER_STATE_KEYS,
        n => n,
    };
    let mut picked: Vec<(StateKeyId, Secret32)> = Vec::new();
    match base {
        Some(b) => {
            let crew = KeyGroupId::crew(space);
            let since = b
                .key_heads
                .iter()
                .find(|k| k.group == crew)
                .map_or(0, |k| k.seq);
            for id in b.valid_state_keys.iter().chain(b.key.iter()) {
                if let Some(k) = crew_state_key(node, space, id)?
                    && !picked.iter().any(|(x, _)| x == id)
                {
                    picked.push((*id, k));
                }
            }
            // 새 것부터 오므로 순번이 머리 이하인 것을 만나면 그만 읽습니다.
            let mut after: Vec<(StateKeyId, Secret32)> = node
                .store
                .recent_keys_in_scope(KeyKind::State, space.as_bytes(), n)?
                .into_iter()
                .take_while(|e| e.epoch.is_some_and(|ep| ep > since))
                .filter_map(|e| Some((StateKeyId::from_slice(&e.id).ok()?, e.key)))
                .collect();
            after.reverse();
            for (id, k) in after {
                if !picked.iter().any(|(x, _)| *x == id) {
                    picked.push((id, k));
                }
            }
        }
        None => {
            let mut entries =
                node.store
                    .recent_keys_in_scope(KeyKind::State, space.as_bytes(), n)?;
            // 새 것부터 왔으므로 오래된 것부터 담습니다. 지금 키 자리를 남깁니다.
            entries.reverse();
            picked = entries
                .into_iter()
                .filter_map(|e| Some((StateKeyId::from_slice(&e.id).ok()?, e.key)))
                .collect();
            if current.is_some() && picked.len() >= n {
                picked.drain(..picked.len() + 1 - n);
            }
        }
    }
    let mut out: Vec<SharedKey> = picked
        .into_iter()
        .filter(|(id, _)| current.as_ref().is_none_or(|(c, _)| c != id))
        .map(|(id, key)| SharedKey::State { id, key })
        .collect();
    if let Some((id, key)) = current {
        out.push(SharedKey::State { id, key });
    }
    Ok(out)
}

/// 키 묶음 하나에 싣는 키 수 상한(검사 상한 `MAX_KEYS_PER_SHARE`의 절반: 여유를 둠).
const KEYS_PER_SHARE: usize = yangpa_sync::state::limits::MAX_KEYS_PER_SHARE / 2;
/// 키 묶음 하나의 대략적 본문 크기 상한(본문 상한 256KiB에 봉인 상자 머리를 넉넉히 뺀 것).
const SHARE_BYTES: usize = 192 * 1024;

/// 키를 순서대로 여러 키 묶음으로 나눕니다(키 수·크기 상한). 가입자 묶음은 오래된 상태 키부터 담고 지금 키를 마지막에
/// 두므로, 나눈 묶음을 이 순서로 내면 마지막 묶음의 마지막 상태 키가 지금 키 후보가 됩니다(받는 쪽 순위 규칙).
pub(crate) fn chunk_shares(keys: Vec<SharedKey>) -> Vec<Vec<SharedKey>> {
    fn size(k: &SharedKey) -> usize {
        match k {
            SharedKey::Bundle { bundle, .. } => bundle.len() + 64,
            _ => 96,
        }
    }
    let mut out: Vec<Vec<SharedKey>> = Vec::new();
    let mut cur = Vec::new();
    let mut bytes = 0;
    for k in keys {
        let n = size(&k);
        if !cur.is_empty() && (cur.len() >= KEYS_PER_SHARE || bytes + n > SHARE_BYTES) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += n;
        cur.push(k);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ───────────── 뿌리 저장소 ─────────────

/// 범위(공간 ‖ 채널).
fn root_scope(space: &SpaceId, channel: &ChannelId) -> Vec<u8> {
    let mut v = space.as_bytes().to_vec();
    v.extend_from_slice(channel.as_bytes());
    v
}

/// 채널 뿌리(저장한 그대로). 본문은 [`yangpa_crypto::keysched::record_key`]로 파생한 기록 키로 엽니다.
pub(crate) fn root(node: &Node, id: &ChannelRootId) -> Result<Option<Secret32>> {
    Ok(node.store.get_key(KeyKind::History, id.as_bytes())?)
}

/// 이 공간·채널의 채널 뿌리(다른 채널의 것이면 `None`: 중계할 때 권한을 본 채널의 뿌리만 내주게).
pub(crate) fn root_in(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    id: &ChannelRootId,
) -> Result<Option<Secret32>> {
    Ok(node
        .store
        .get_key_entry(KeyKind::History, id.as_bytes())?
        .filter(|e| e.scope == root_scope(space, channel))
        .map(|e| e.key))
}

/// 이 채널의 뿌리 저장소 전부(기록 공개로 바꿀 때 다시 봉인).
pub(crate) fn roots_of_channel(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
) -> Result<Vec<(ChannelRootId, Secret32)>> {
    let mut out = Vec::new();
    for e in node
        .store
        .keys_in_scope(KeyKind::History, &root_scope(space, channel))?
    {
        if let Ok(id) = ChannelRootId::from_slice(&e.id) {
            out.push((id, e.key));
        }
    }
    Ok(out)
}

/// 채널 뿌리를 캐시합니다. ID가 뿌리에서 나온 것만(먼저 쓴 쪽이 이기므로 엉뚱한 뿌리를 진짜 ID 아래 두지 않게).
pub(crate) fn put_root(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    key: &Secret32,
) -> Result<ChannelRootId> {
    let id = yangpa_crypto::keysched::channel_root_id(space, channel, key);
    node.store.put_key_entry(&KeyEntry {
        kind: KeyKind::History,
        id: id.as_bytes().to_vec(),
        scope: root_scope(space, channel),
        space: Some(*space),
        epoch: None,
        not_before: None,
        key: key.clone(),
        created_at: Timestamp::now(),
    })?;
    Ok(id)
}

// ───────────── 보낼 뿌리 ─────────────

/// 보낼 채널 뿌리(ID, 뿌리): 채널이 쓰는 키 그룹의 정규 사슬 끝(크루는 해소기의 크루 사슬 끝, 비공개·덮개 그룹은 그룹
/// 사슬의 이긴 끝)에서 이 기기가 적용한 상태로 파생합니다(기록 공개면 R의 공개 뿌리, 아니면 T의 비공개 뿌리). 진 갈래의
/// 상태는 읽기에만 쓰므로 여기서 고르지 않습니다. 사슬 끝 상태가 아직 없으면 `None`.
///
/// 사슬 끝이 이 기기가 적용한 제거를 모두 덮는지는 [`crate::keygroup::removal_pending`]이 봅니다. 사슬을 시작한 회전이
/// 덮는 제거 집합을 따로 들지 않고, 해소기의 빚(크루에서 빠져야 하는데 아직 안 빠진 기기)과 그룹 사슬 끝 멤버 가운데
/// 이제 볼 수 없는 기기로 셉니다(같은 답이고, 4.5만 대 집합을 갱신마다 복사하지 않음).
pub(crate) fn send_root(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    state: &std::sync::Arc<yangpa_core::model::SpaceState>,
) -> Result<Option<(ChannelRootId, Secret32)>> {
    let Some(ch) = state.channels.get(channel) else {
        return Ok(None);
    };
    if !ch.uses_group_keys() {
        return Ok(None);
    }
    let group = crate::keygroup::channel_group(node, space, state, channel);
    let Some(core) = node.spaces.read().get(space).map(|rt| rt.core.clone()) else {
        return Ok(None);
    };
    let mut c = core.lock();
    c.keys.ensure_loaded(node, space);
    let k = match &group {
        None => c.keys.crew_current(space, &c.resolver),
        Some(g) => c.keys.group_current(g),
    };
    Ok(k.map(|k| {
        let root = crate::keygroup::member::root_for(k, ch.history, channel);
        (
            yangpa_crypto::keysched::channel_root_id(space, channel, &root),
            root,
        )
    }))
}

// ───────────── 키 그룹 멤버 상태·확정 기기 표 ─────────────

/// 예전(멤버 상태 전체를 한 행에 둔) 멤버 행. 읽어서 옮긴 뒤 지웁니다.
pub(crate) fn load_member_blob(node: &Node, space: &SpaceId) -> Result<Option<Zeroizing<Vec<u8>>>> {
    Ok(node.store.get_secret(&member_secret(space))?)
}

pub(crate) fn delete_member_blob(node: &Node, space: &SpaceId) -> Result<()> {
    node.store.delete_secret(&member_secret(space))?;
    Ok(())
}

fn member_index_secret(space: &SpaceId) -> String {
    format!("kg.mi.{space}")
}

fn member_group_secret(space: &SpaceId, group: &KeyGroupId) -> String {
    format!("kg.mg.{space}.{group}")
}

/// 멤버 상태 행 이름: 그룹 목록(`None`) 또는 그룹 하나의 상태들. 그룹마다 따로 두어 바뀐 그룹만 씁니다.
pub(crate) fn load_member_row(
    node: &Node,
    space: &SpaceId,
    group: Option<&KeyGroupId>,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let name = match group {
        Some(g) => member_group_secret(space, g),
        None => member_index_secret(space),
    };
    Ok(node.store.get_secret(&name)?)
}

pub(crate) fn save_member_row(
    node: &Node,
    space: &SpaceId,
    group: Option<&KeyGroupId>,
    bytes: &[u8],
) -> Result<()> {
    let name = match group {
        Some(g) => member_group_secret(space, g),
        None => member_index_secret(space),
    };
    node.store.put_secret(&name, bytes)?;
    Ok(())
}

fn chain_bases_secret(space: &SpaceId) -> String {
    format!("kg.cb.{space}")
}

fn chain_base_secret(space: &SpaceId, group: &KeyGroupId) -> String {
    format!("kg.cbg.{space}.{group}")
}

/// 키 그룹 사슬 바닥 행(그룹마다 체크포인트 머리까지 접은 바닥, `yangpa_sync::ChainBase`)과 그 그룹 목록 행. 비밀은
/// 없지만 비공개 그룹의 멤버 목록이라 멤버 상태와 같이 잠긴 저장소에 둡니다. 그룹마다 따로 두어 멤버 목록을 상한
/// 없이 그대로 남깁니다(목록을 빼고 저장하면 다시 시작한 노드가 멤버를 몰라 그룹 판정과 중계가 멈춤).
pub(crate) fn load_chain_bases(
    node: &Node,
    space: &SpaceId,
) -> Result<Vec<(KeyGroupId, yangpa_sync::ChainBase)>> {
    let Some(bytes) = node.store.get_secret(&chain_bases_secret(space))? else {
        return Ok(Vec::new());
    };
    let groups: Vec<KeyGroupId> = postcard::from_bytes(&bytes).unwrap_or_default();
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        if let Some(b) = node.store.get_secret(&chain_base_secret(space, &g))?
            && let Ok(base) = postcard::from_bytes(&b)
        {
            out.push((g, base));
        }
    }
    Ok(out)
}

/// 바뀐 그룹의 사슬 바닥 행과 그룹 목록 행을 씁니다(목록에서 빠진 그룹의 행은 지움).
pub(crate) fn save_chain_bases(
    node: &Node,
    space: &SpaceId,
    changed: &[(KeyGroupId, yangpa_sync::ChainBase)],
    groups: &[KeyGroupId],
) -> Result<()> {
    for (g, b) in changed {
        let bytes = postcard::to_stdvec(b)
            .map_err(|_| crate::error::Error::internal("사슬 바닥 직렬화 실패"))?;
        node.store
            .put_secret(&chain_base_secret(space, g), &bytes)?;
    }
    let old: Vec<KeyGroupId> = node
        .store
        .get_secret(&chain_bases_secret(space))?
        .and_then(|b| postcard::from_bytes(&b).ok())
        .unwrap_or_default();
    for g in old.iter().filter(|g| !groups.contains(g)) {
        node.store.delete_secret(&chain_base_secret(space, g))?;
    }
    if old.as_slice() != groups {
        let bytes = postcard::to_stdvec(groups)
            .map_err(|_| crate::error::Error::internal("사슬 바닥 목록 직렬화 실패"))?;
        node.store.put_secret(&chain_bases_secret(space), &bytes)?;
    }
    Ok(())
}

pub(crate) fn delete_member_row(node: &Node, space: &SpaceId, group: &KeyGroupId) -> Result<()> {
    node.store
        .delete_secret(&member_group_secret(space, group))?;
    Ok(())
}

/// 멤버 상태 목록 행: 상태를 둔 그룹과 받은편지함으로 받은 그룹 R.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct MemberIndex {
    pub(crate) v: u8,
    pub(crate) groups: Vec<KeyGroupId>,
    pub(crate) rs: Vec<(KeyGroupId, Secret32)>,
}

/// 표에 든 비공개·덮개 그룹 목록(크루는 늘 있음).
pub(crate) fn load_table_index(node: &Node, space: &SpaceId) -> Result<Option<Vec<KeyGroupId>>> {
    let Some(bytes) = node.store.get_secret(&table_index_secret(space))? else {
        return Ok(None);
    };
    Ok(postcard::from_bytes(&bytes).ok())
}

pub(crate) fn save_table_index(node: &Node, space: &SpaceId, groups: &[KeyGroupId]) -> Result<()> {
    let bytes = Zeroizing::new(postcard::to_stdvec(groups).unwrap_or_default());
    node.store.put_secret(&table_index_secret(space), &bytes)?;
    Ok(())
}

pub(crate) fn load_group_row(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    slot: u8,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    Ok(node
        .store
        .get_secret(&group_row_secret(space, group, slot))?)
}

pub(crate) fn save_group_row(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    slot: u8,
    bytes: &[u8],
) -> Result<()> {
    node.store
        .put_secret(&group_row_secret(space, group, slot), bytes)?;
    Ok(())
}

pub(crate) fn delete_group_row(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    slot: u8,
) -> Result<()> {
    node.store
        .delete_secret(&group_row_secret(space, group, slot))?;
    Ok(())
}

/// 표 행을 모두 지웁니다(목록에 든 그룹과 크루의 두 칸, 목록 행, 예전 행).
pub(crate) fn delete_table_rows(node: &Node, space: &SpaceId) -> Result<()> {
    let mut groups = load_table_index(node, space)?.unwrap_or_default();
    groups.push(KeyGroupId::crew(space));
    for g in &groups {
        for slot in 0..2 {
            delete_group_row(node, space, g, slot)?;
        }
    }
    node.store.delete_secret(&table_index_secret(space))?;
    node.store.delete_secret(&legacy_table_secret(space))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 받은편지함으로 받은 지난 키 묶음(오래된 것부터 여러 묶음)은 나중 묶음의 키가 지금 키가 되고, 순위 없이 정한 지금
    /// 키는 봉인 상자로 받은 키가 바꾸지 못합니다.
    #[test]
    fn ranked_inbox_keys_promote_the_latest_bundle_only() {
        let k = |n: u8| StateKeyId([n; 16]);
        let cur = |id: u8, rank: Option<u64>| Some(CurrentStateKey { id: k(id), rank });
        assert!(replaces_current(None, &k(1), Some(10)));
        assert!(replaces_current(None, &k(1), None));
        assert!(replaces_current(cur(1, Some(10)), &k(2), Some(11)));
        assert!(!replaces_current(cur(2, Some(11)), &k(1), Some(10)));
        assert!(!replaces_current(cur(1, Some(10)), &k(2), None));
        assert!(!replaces_current(cur(1, None), &k(2), Some(99)));
    }

    #[test]
    fn crew_and_group_state_key_scopes_differ() {
        let s = SpaceId([1; 16]);
        let g = KeyGroupId([2; 16]);
        assert_ne!(group_scope(&s, &g), s.as_bytes().to_vec());
        assert_ne!(root_scope(&s, &ChannelId([2; 16])), s.as_bytes().to_vec());
    }

    /// 키가 많은 묶음(지난 상태 키 2048개 + 몫 묶음)도 묶음 하나의 키 수·크기 상한을 넘지 않게 나뉘고, 순서가 그대로라
    /// 지금 키(마지막 상태 키)가 마지막 묶음에 남습니다.
    #[test]
    fn joiner_keys_split_under_share_limits_in_order() {
        let space = SpaceId([1; 16]);
        let mut keys: Vec<SharedKey> = (0..2048)
            .map(|_| {
                let key = Secret32::random();
                SharedKey::State {
                    id: state_key_id(&space, &key),
                    key,
                }
            })
            .collect();
        let last_state = keys.last().cloned();
        for _ in 0..20 {
            keys.push(SharedKey::Bundle {
                group: KeyGroupId([2; 16]),
                seq: 1,
                bundle: vec![0; 12 * 1024],
            });
        }
        let total = keys.len();
        let chunks = chunk_shares(keys);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), total);
        for c in &chunks {
            assert!(!c.is_empty() && c.len() <= yangpa_sync::state::limits::MAX_KEYS_PER_SHARE);
            let bytes: usize = c
                .iter()
                .map(|k| match k {
                    SharedKey::Bundle { bundle, .. } => bundle.len(),
                    _ => 64,
                })
                .sum();
            assert!(bytes <= yangpa_core::event::MAX_BODY_LEN - 32 * 1024);
        }
        let in_last_state_chunk = chunks
            .iter()
            .rev()
            .find_map(|c| {
                c.iter()
                    .rev()
                    .find(|k| matches!(k, SharedKey::State { .. }))
            })
            .cloned();
        assert_eq!(in_last_state_chunk, last_state);
        assert!(chunk_shares(Vec::new()).is_empty());
    }
}
