//! 짧은 연락처·초대 링크와 만든 링크 다시 복사(protocol.md "짧은 링크").
//!
//! 사람에게 주는 링크에는 만든 기기의 onion 공개키와 긴 링크의 확인값만 담고([`ShortLink`]), 긴 링크는 만든 기기의 암호화된
//! 저장소(설정 `link.made.<확인값>`)에만 둡니다. 링크를 여는 쪽은 그 onion에 `GetLink`로 물어 확인값과 맞는 긴 링크만
//! 씁니다. 이 기기에서 폐기하면 지우므로, 그 짧은 링크에서는 사용자·크루를 알 수 없습니다.
//!
//! 긴 링크에는 토큰 원문이 들어 있어 로그에 남기지 않고, 다른 기기·상태 이벤트로 보내지 않습니다.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use yangpa_core::ids::{LinkCode, LinkToken, SpaceId};
use yangpa_core::links::{ContactInfo, MAX_LINK_CHARS, SCHEME, ShortLink, link_code, token_hash};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_net::addr;
use yangpa_net::config::DEFAULT_ONION_PORT;

use crate::error::{Error, Result};
use crate::node::Node;

const PREFIX: &str = "link.made.";

/// 이 기기에서 만든 링크.
#[derive(Serialize, Deserialize)]
struct Made {
    /// 사람에게 준 주소.
    url: String,
    /// 긴 링크에서 주소를 뺀 나머지(`invite/…`).
    long: String,
    token_hash: [u8; 32],
    /// 초대 링크의 크루.
    space: Option<SpaceId>,
    expires_at: Option<Timestamp>,
}

impl Made {
    fn expired(&self) -> bool {
        self.expires_at.is_some_and(|t| t.is_past())
    }
}

fn key(code: &LinkCode) -> String {
    format!("{PREFIX}{code}")
}

fn all(node: &Node) -> Vec<(String, Made)> {
    let rows = node.store.settings_with_prefix(PREFIX).unwrap_or_default();
    rows.into_iter()
        .filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?)))
        .collect()
}

fn forget_if(node: &Node, f: impl Fn(&Made) -> bool) {
    for (k, m) in all(node) {
        if f(&m) {
            let _ = node.store.delete_setting(&k);
        }
    }
}

/// 만든 긴 링크를 남기고 사람에게 줄 주소를 돌려줍니다(onion이 없는 노드는 긴 링크 그대로).
pub(crate) fn save(
    node: &Node,
    long: &str,
    contact: &ContactInfo,
    token: &LinkToken,
    space: Option<SpaceId>,
    expires_at: Option<Timestamp>,
) -> Result<String> {
    forget_if(node, Made::expired);
    let url = contact
        .onion
        .as_deref()
        .and_then(|o| o.strip_suffix(&format!(":{DEFAULT_ONION_PORT}")))
        .and_then(addr::onion_key)
        .and_then(|onion| ShortLink::url_of(long, onion))
        .unwrap_or_else(|| long.to_owned());
    let made = Made {
        url: url.clone(),
        long: long
            .split_once('#')
            .map_or(long, |(_, frag)| frag)
            .to_owned(),
        token_hash: token_hash(token),
        space,
        expires_at,
    };
    node.store.put_setting(&key(&link_code(long)), &made)?;
    Ok(url)
}

/// 다시 복사할 수 있는 링크들의 토큰 해시.
pub(crate) fn made(node: &Node) -> HashSet<[u8; 32]> {
    all(node)
        .into_iter()
        .filter(|(_, m)| !m.expired())
        .map(|(_, m)| m.token_hash)
        .collect()
}

/// 이 기기에서 만든 링크의 주소.
pub(crate) fn url_of(node: &Node, hash: &[u8; 32]) -> Option<String> {
    all(node)
        .into_iter()
        .find(|(_, m)| m.token_hash == *hash && !m.expired())
        .map(|(_, m)| m.url)
}

/// 폐기한 링크를 지웁니다.
pub(crate) fn forget(node: &Node, hash: &[u8; 32]) {
    forget_if(node, |m| m.token_hash == *hash);
}

/// 나간 크루의 초대 링크를 지웁니다.
pub(crate) fn forget_space(node: &Node, space: &SpaceId) {
    forget_if(node, |m| m.space == Some(*space));
}

pub(crate) fn handle_get(node: &Node, code: &LinkCode) -> YpResponse {
    match node.store.get_setting::<Made>(&key(code)) {
        Ok(Some(m)) if m.expired() => YpResponse::error(WireErrorCode::Expired, "만료된 링크"),
        Ok(Some(m)) => YpResponse::Link(m.long),
        Ok(None) => YpResponse::error(WireErrorCode::InvalidToken, "쓸 수 없는 링크"),
        Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    }
}

/// 짧은 링크의 긴 링크를 받아 옵니다(`yangpa://invite/…`). 만든 기기에 닿지 못했으면 `Error::Network`(나중에 다시 하면 될
/// 수 있음), 그 밖은 다시 해도 같은 오류입니다.
pub(crate) async fn open(node: &Arc<Node>, short: &ShortLink) -> Result<String> {
    // 이 기기에서 만든 링크("내 링크입니다" 같은 답은 부르는 쪽이 냄).
    if let Some(m) = node.store.get_setting::<Made>(&key(&short.code))? {
        return Ok(format!("{SCHEME}{}", m.long));
    }
    let onion = addr::onion_from_key(short.onion, DEFAULT_ONION_PORT)
        .ok_or_else(|| Error::invalid("링크의 onion 주소가 잘못됨"))?;
    let peer = node.net.dial_addr(onion).await?;
    let req = YpRequest::GetLink { code: short.code };
    match node.net.request(peer, req).await? {
        YpResponse::Link(long) => check(short, &long),
        YpResponse::Error {
            code: WireErrorCode::InvalidToken | WireErrorCode::Expired,
            ..
        } => Err(Error::invalid("링크가 만료됐거나 폐기됐습니다")),
        // 바쁨·저장소 오류·아직 신원이 없는 노드는 지나갈 수 있습니다.
        _ => Err(Error::Network("링크를 만든 기기가 답하지 않음".into())),
    }
}

/// 받은 긴 링크가 짧은 링크가 가리키는 그것인지 봅니다.
fn check(short: &ShortLink, long: &str) -> Result<String> {
    if long.len() > MAX_LINK_CHARS || link_code(long) != short.code {
        return Err(Error::invalid(
            "링크를 만든 기기가 준 내용이 링크와 다릅니다",
        ));
    }
    Ok(format!("{SCHEME}{long}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_takes_only_the_pinned_link() {
        let short = ShortLink {
            onion: [1; 32],
            code: link_code("invite/3yZe7d"),
        };
        assert_eq!(
            check(&short, "invite/3yZe7d").unwrap(),
            "yangpa://invite/3yZe7d"
        );
        assert!(check(&short, "invite/3yZe7e").is_err());
        assert!(check(&short, "friend/3yZe7d").is_err());
    }
}
