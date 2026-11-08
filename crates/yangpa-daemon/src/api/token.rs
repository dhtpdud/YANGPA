//! `token.*`: API 토큰 발급·목록·폐기(인증은 `api::authenticate_token`이 `api_tokens`를 읽음).
//!
//! 저장소에는 토큰 해시(`api::hash_token`)만 두고, 원문은 만들 때 한 번만 돌려줍니다.

use std::sync::Arc;

use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::serde_util::SecretString;
use yangpa_rpc::api::{Scope, TokenCreateResult, TokenView, m};
use yangpa_store::ApiTokenRecord;

use super::{CallCtx, OWNER_TOKEN_ID, P, R, hash_token};
use crate::admin;
use crate::error::Error;
use crate::node::Node;

/// 이름 최대 길이.
const MAX_NAME_CHARS: usize = 64;
/// 토큰 하나의 허용 공간 수 상한.
const MAX_SPACES: usize = 64;
/// 쓸 수 있는(폐기 안 된) 토큰 수 상한.
const MAX_TOKENS: usize = 256;

fn store_err(e: yangpa_store::StoreError) -> RpcError {
    Error::from(e).into()
}

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::TokenCreate>,
) -> R<m::TokenCreate> {
    let name = p.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(RpcError::invalid_input(format!(
            "토큰 이름은 1~{MAX_NAME_CHARS}자여야 합니다"
        )));
    }
    let mut scopes = p.scopes.clone();
    scopes.sort_by_key(|s| s.name());
    scopes.dedup();
    if scopes.is_empty() {
        return Err(RpcError::invalid_input(
            "범위(scopes)가 하나 이상 필요합니다",
        ));
    }
    let automation = p.automation.unwrap_or(false);
    // 자동화 토큰은 최소 권한으로(owner는 토큰 발급·종료까지 포함).
    if automation && scopes.contains(&Scope::Owner) {
        return Err(RpcError::invalid_input(
            "자동화 토큰에는 owner 범위를 줄 수 없습니다",
        ));
    }
    let mut spaces = p.spaces.clone();
    spaces.sort();
    spaces.dedup();
    if spaces.len() > MAX_SPACES {
        return Err(RpcError::invalid_input(format!(
            "허용 공간은 {MAX_SPACES}개 이하여야 합니다"
        )));
    }
    if let Some(s) = spaces.iter().find(|s| node.space_state(s).is_none()) {
        return Err(RpcError::not_found(format!("모르는 공간: {s}")));
    }
    let active = node
        .store
        .list_api_tokens()
        .map_err(store_err)?
        .into_iter()
        .filter(|t| !t.revoked)
        .count();
    if active >= MAX_TOKENS {
        return Err(RpcError::conflict(
            "토큰이 너무 많습니다(쓰지 않는 토큰을 폐기하세요)",
        ));
    }
    let secret = zeroize::Zeroizing::new(bs58::encode(rand::random::<[u8; 32]>()).into_string());
    let id = bs58::encode(rand::random::<[u8; 12]>()).into_string();
    node.store
        .create_api_token(&ApiTokenRecord {
            id: id.clone(),
            name,
            token_hash: hash_token(&secret),
            scopes: scopes.iter().map(|s| s.name().to_owned()).collect(),
            spaces,
            automation,
            created_at: Timestamp::now(),
            revoked: false,
            last_used_at: None,
        })
        .map_err(store_err)?;
    Ok(TokenCreateResult {
        token: SecretString::new(secret.as_str()),
        id,
    })
}

pub(crate) async fn list(node: &Arc<Node>, _ctx: &CallCtx, _p: P<m::TokenList>) -> R<m::TokenList> {
    Ok(node
        .store
        .list_api_tokens()
        .map_err(store_err)?
        .into_iter()
        .filter(|t| !t.revoked)
        .map(|t| TokenView {
            id: t.id,
            name: t.name,
            scopes: t.scopes.iter().filter_map(|s| s.parse().ok()).collect(),
            spaces: t.spaces,
            automation: t.automation,
            created_at: t.created_at,
            last_used_at: t.last_used_at,
        })
        .collect())
}

pub(crate) async fn revoke(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::TokenRevoke>,
) -> R<m::TokenRevoke> {
    if p.id == OWNER_TOKEN_ID {
        return Err(RpcError::invalid_input(
            "소유자 토큰은 폐기할 수 없습니다(owner.token 파일을 지우고 데몬을 다시 시작하세요)",
        ));
    }
    let known = node
        .store
        .list_api_tokens()
        .map_err(store_err)?
        .into_iter()
        .any(|t| t.id == p.id && !t.revoked);
    if !known || !node.store.revoke_api_token(&p.id).map_err(store_err)? {
        return Err(RpcError::not_found("없는 토큰"));
    }
    node.admin.mark_revoked(&p.id);
    admin::drop_requests_of(node, &p.id);
    Ok(super::empty())
}
