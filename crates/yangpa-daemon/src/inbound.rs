//! 들어온 노드 간 요청(`YpRequest`)을 영역별 처리기로 나눕니다.
//!
//! 인증 전에도 받는 요청: `Auth`, `RedeemFriendLink`, `FriendAccepted`, `RedeemInvite`, `Ping`, `Archive`의 지정·해제
//! (요청에 기기 증명이 실림), `FileLink`·`CheckJoin`(요청에 기기 증명이 실림), `DeviceLink`·`DeviceLinkPoll`(링크
//! 토큰을 가진 쪽만, 답은 요청한 기기의 상자 키로 봉인).
//! 나머지는 각 처리기가 `node.peers.identity_of`로 인증·멤버 여부를 확인합니다. 위 목록은
//! `YpRequest::before_auth`와 같아야 합니다(그 요청은 압축하지 않고 오가므로, 빠지면 인증 전 상대가 보낸 압축본을 거절함).
//!
//! 압축된 요청은 인증된 노드(그 기기의 파일 회로 포함)에게서 온 것만 풉니다. 인증 전 입력이 OpenZL(C) 해제기에
//! 닿지 않게 합니다.

use std::sync::Arc;

use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_net::{Incoming, PeerId};

use crate::node::Node;
use crate::{
    blobs, crews, devlink, friends, identity, keygroup, moderation, peers, sync_service, voice,
    volatile,
};

/// 인증 전 노드가 보낸 압축 요청에 주는 답.
pub(crate) const PACKED_BEFORE_AUTH: &str = "인증 전에는 압축한 요청을 풀지 않음";

pub(crate) async fn handle(node: &Arc<Node>, peer: PeerId, req: Incoming) -> YpResponse {
    if req.is_packed()
        && node.peers.identity_of(&peer).is_none()
        && crate::filelink::main_peer_of(node, &peer).is_none()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, PACKED_BEFORE_AUTH);
    }
    // 푼 압축 요청의 예산 표식은 답을 만들 때까지 쥡니다.
    let (req, _opened) = match req.open() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    if matches!(req, YpRequest::Ping) {
        return YpResponse::Pong;
    }
    if node.me.read().is_none() {
        return YpResponse::error(WireErrorCode::Unauthenticated, "이 노드에 아직 신원이 없음");
    }
    // 상대 기기의 파일 회로(`filelink`): 블롭 요청만 그 기기의 주 연결인 것처럼 답합니다.
    if let Some(main) = crate::filelink::main_peer_of(node, &peer) {
        return crate::filelink::serve(node, main, req).await;
    }
    // 보관 전용 노드 고리: 받아 둔 공간의 요청·보관 관계로만 믿는 연결의 요청을 먼저 거릅니다.
    let req = match crate::archive::route(node, peer, req) {
        Ok(r) => return r,
        Err(req) => req,
    };
    match req {
        YpRequest::Ping => YpResponse::Pong,
        YpRequest::Auth(auth) => peers::handle_auth(node, peer, *auth),
        YpRequest::DeviceLink(r) => devlink::handle_request(node, peer, *r),
        YpRequest::DeviceLinkPoll { token } => devlink::handle_poll(node, peer, token),
        YpRequest::SupporterShare(s) => crate::supporter::handle_share(node, peer, s),
        YpRequest::Presence(s) => crate::presence::handle(node, peer, s),
        YpRequest::Badges(m) => crate::badges::handle(node, peer, *m),
        YpRequest::RedeemFriendLink(r) => friends::handle_redeem(node, peer, *r),
        YpRequest::FriendAccepted {
            token,
            space,
            events,
            profile,
            contact,
        } => {
            let accepted = friends::Accepted {
                token,
                space,
                events,
                profile: profile.map(|p| *p),
                contact,
            };
            friends::handle_accepted(node, peer, accepted).await
        }
        YpRequest::RedeemInvite(r) => crews::handle_redeem_invite(node, peer, r).await,
        YpRequest::PushEvents { space, events } => {
            sync_service::handle_push(node, peer, space, events).await
        }
        YpRequest::GetHeads { space, stream } => {
            sync_service::handle_get_heads(node, peer, space, stream)
        }
        YpRequest::GetFingerprints {
            space,
            stream,
            ranges,
        } => sync_service::handle_get_fingerprints(node, peer, space, stream, ranges),
        YpRequest::GetIds {
            space,
            stream,
            range,
            limit,
        } => sync_service::handle_get_ids(node, peer, space, stream, range, limit),
        YpRequest::GetEvents { space, ids } => {
            sync_service::handle_get_events(node, peer, space, ids)
        }
        req @ (YpRequest::BlobManifest { .. }
        | YpRequest::BlobHave { .. }
        | YpRequest::BlobChunk { .. }) => blobs::handle_request(node, peer, req).await,
        YpRequest::GetChannelRoots {
            space,
            channel,
            ids,
        } => keygroup::relay::handle_get_roots(node, &peer, space, channel, ids),
        YpRequest::GetGroupKeys { space, group } => {
            keygroup::relay::handle_get_group_keys(node, &peer, space, group).await
        }
        YpRequest::KeyTableHandover(h) => keygroup::receive_handover(node, peer, *h).await,
        YpRequest::GetStateKeys { space, ids } => {
            keygroup::relay::handle_get_state_keys(node, &peer, space, ids)
        }
        YpRequest::GetKeyShard { space, blob } => {
            keygroup::shards::handle_get_shard(node, &peer, space, blob)
        }
        YpRequest::Signal {
            space,
            channel,
            call,
            from,
            payload,
        } => voice::handle_signal(node, peer, space, channel, call, from, payload).await,
        YpRequest::VolatileSnapshot { space, channel } => {
            volatile::handle_snapshot(node, peer, space, channel).await
        }
        YpRequest::VolatilePresence {
            space,
            channel,
            present,
        } => volatile::handle_presence(node, peer, space, channel, present).await,
        YpRequest::VolatileKeyWrap {
            space,
            channel,
            generation,
            wrap,
            recipients,
        } => volatile::handle_key_wrap(node, peer, space, channel, generation, wrap, recipients),
        YpRequest::Social(r) => identity::handle_social(node, peer, r).await,
        YpRequest::Moderation(r) => moderation::handle_request(node, peer, r).await,
        YpRequest::Archive(r) => crate::archive::handle_request(node, peer, *r).await,
        YpRequest::P2pHello(h) => crate::p2p::handle_hello(node, peer, *h),
        YpRequest::P2pPunch(h) => crate::p2p::handle_punch(node, peer, *h).await,
        YpRequest::FileLink(auth) => crate::filelink::handle_link(node, peer, *auth),
        YpRequest::Typing { space, channel } => crate::typing::handle(node, peer, space, channel),
        YpRequest::GetChannelTips { space, channels } => {
            crate::chantopics::handle_get_tips(node, peer, space, channels)
        }
        YpRequest::FindDevice {
            space,
            user,
            device,
        } => crate::locator::handle_find_device(node, peer, space, user, device),
        YpRequest::GetMemberNodes { space } => {
            crate::locator::handle_get_member_nodes(node, peer, space)
        }
        YpRequest::CheckJoin { spaces, auth } => {
            peers::handle_check_join(node, peer, &spaces, &auth)
        }
        YpRequest::GetCheckpoint { space, from_seq } => {
            crate::checkpoint::handle_get_checkpoint(node, peer, space, from_seq)
        }
        YpRequest::GetDeviceKeys { space, devices } => {
            crate::checkpoint::handle_get_device_keys(node, peer, space, devices).await
        }
        YpRequest::GetMemberCards { space, users } => {
            crate::cards::handle_get(node, peer, space, users)
        }
        YpRequest::GetGroupMembers {
            space,
            group,
            seq,
            root_commit,
        } => {
            keygroup::relay::handle_get_group_members(node, &peer, space, group, seq, root_commit)
                .await
        }
    }
}
