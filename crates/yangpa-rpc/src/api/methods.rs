//! 메서드 표: 이름 · 필요한 범위 · 공간 제한 규칙 · 파괴적 여부 · 인자/결과 타입.
//!
//! 표에 없는 메서드는 서버가 핸들러에 넘기지 않고 `-32601`로 거절합니다(범위 검사를 우회하는 메서드가 없게).

use yangpa_core::device::Profile;
use yangpa_core::model::Role;

use super::params::*;
use super::types::*;
use super::{Empty, MethodSpec, Scope, SpaceRule};

macro_rules! access {
    (Public) => {
        None
    };
    ($s:ident) => {
        Some(Scope::$s)
    };
}

macro_rules! api_methods {
    ($(
        $(#[$doc:meta])*
        $ty:ident = $name:literal [$acc:ident, $space:ident $(, $flag:ident)*] ($params:ty) -> $result:ty;
    )*) => {
        /// 메서드 표지 타입. `client.request::<m::SpaceList>(Empty)`처럼 씁니다.
        pub mod m {
            #[allow(unused_imports)]
            use super::*;
            $(
                $(#[$doc])*
                #[doc = concat!("`", $name, "`")]
                #[derive(Clone, Copy, Debug, Default)]
                pub struct $ty;

                impl crate::api::Method for $ty {
                    const NAME: &'static str = $name;
                    type Params = $params;
                    type Result = $result;
                }
            )*
        }

        /// 모든 메서드.
        pub static METHODS: &[MethodSpec] = &[
            $( MethodSpec::new($name, access!($acc), SpaceRule::$space) $(.$flag())* ),*
        ];
    };
}

api_methods! {
    // 시스템·인증
    AuthHello = "auth.hello" [Public, Filtered] (HelloParams) -> HelloResult;
    SystemVersion = "system.version" [Public, Filtered] (Empty) -> VersionResult;
    SystemStatus = "system.status" [Read, Filtered] (Empty) -> StatusResult;
    SystemShutdown = "system.shutdown" [Owner, Global, human_only] (Empty) -> Empty;
    DiagMetricsGet = "diag.metrics" [Read, Global] (Empty) -> DiagMetrics;
    /// 휘발성 표식 문자열이 디스크·로그에 있는지 검사.
    DiagLeakcheck = "diag.leakcheck" [Owner, Global] (LeakcheckParams) -> LeakcheckResult;

    // 신원·기기
    IdentityCreate = "identity.create" [Owner, Global] (IdentityCreateParams) -> IdentityRef;
    IdentityGet = "identity.get" [Read, Filtered] (Empty) -> IdentityView;
    /// 이 기기의 계정과 로컬 데이터를 모두 지웁니다(`device.wiped`).
    IdentityDelete = "identity.delete" [Owner, Global, human_only] (Empty) -> Empty;
    DeviceList = "device.list" [Read, Global] (Empty) -> Vec<DeviceView>;
    /// 내 기기 이름을 바꿉니다(새 서명 목록으로 알림).
    DeviceRename = "device.rename" [Owner, Global, human_only] (DeviceRenameParams) -> Empty;
    /// 폐기 이벤트로 모든 키 그룹에서 해당 기기를 뺍니다(확정 기기의 키 갱신).
    DeviceRevoke = "device.revoke" [Owner, Global, human_only] (DeviceRevokeParams) -> Empty;
    /// 새 기기를 들일 QR·링크를 띄웁니다(앞서 띄운 것은 끝남).
    DeviceLinkCreate = "device.link.create" [Owner, Global, human_only] (Empty) -> DeviceLinkOffer;
    /// 띄운 링크로 들어온, 승인을 기다리는 요청.
    DeviceLinkPending = "device.link.pending" [Owner, Global, human_only] (Empty) -> Option<DeviceLinkRequestView>;
    /// 그 요청을 승인하거나 거절합니다.
    DeviceLinkDecide = "device.link.decide" [Owner, Global, human_only] (DeviceLinkDecideParams) -> Empty;
    /// 띄운 링크를 끝냅니다.
    DeviceLinkCancel = "device.link.cancel" [Owner, Global, human_only] (Empty) -> Empty;
    /// 신원이 없는 이 기기가 링크로 연결을 청합니다. 끝나면 `device.link_result`.
    DeviceLinkJoin = "device.link.join" [Owner, Global, human_only] (DeviceLinkJoinParams) -> DeviceLinkJoined;
    /// 이 기기의 연결 요청을 그만둡니다.
    DeviceLinkAbort = "device.link.abort" [Owner, Global, human_only] (Empty) -> Empty;

    // 후원자 확인(GitHub Sponsors·Patreon 정기 후원). 라이브러리 탭·콘텐츠 뷰어를 여는 데 앱이 씁니다.
    // 연결한 계정 이름(Patreon은 실명일 때가 많음)이 실려 AI·읽기 토큰에는 보이지 않게 owner로 둡니다.
    SupporterStatusGet = "supporter.status" [Owner, Global] (Empty) -> SupporterStatus;
    SupporterGithubBegin = "supporter.github_begin" [Owner, Global, human_only] (Empty) -> SupporterDeviceCode;
    SupporterPatreonBegin = "supporter.patreon_begin" [Owner, Global, human_only] (Empty) -> SupporterAuthUrl;
    SupporterRefresh = "supporter.refresh" [Owner, Global] (Empty) -> SupporterStatus;
    SupporterUnlink = "supporter.unlink" [Owner, Global, human_only] (SupporterUnlinkParams) -> Empty;

    // 앱 업데이트(공개 저장소의 GitHub 릴리스, Tor로 도는 기기는 Arti로만). 받은 파일은 앱이 서명을 다시 확인하고 설치합니다.
    UpdateCheck = "update.check" [Owner, Global] (Empty) -> UpdateInfo;
    /// 받기를 시작합니다. 진행과 끝은 `update.progress` 알림.
    UpdateDownload = "update.download" [Owner, Global, human_only] (UpdateDownloadParams) -> Empty;

    // 표시 상태(온라인·자리 비움·숨기·오프라인)와 마지막 동기화 시각 비공개. 오프라인으로 바꾸거나 오프라인에서
    // 나오면 데몬이 다시 뜹니다.
    PresenceGet = "presence.get" [Read, Global] (Empty) -> PresenceView;
    PresenceSet = "presence.set" [Settings, Global, human_only] (PresenceSetParams) -> PresenceView;

    // 후원자 표시(카드 배너·프로필 사진 후광). 받은 것과 내 것을 사용자마다 하나씩.
    FlairList = "flair.list" [Read, Global] (Empty) -> Vec<FlairView>;
    FlairSet = "flair.set" [Settings, Global, human_only] (FlairSetParams) -> FlairView;
    // 도전과제 뱃지. 보일 뱃지 고르기와 원년멤버 증서(개발자 계정만)는 사람만.
    BadgeGet = "badge.get" [Read, Global] (UserParams) -> BadgeView;
    BadgeSetShown = "badge.set_shown" [Settings, Global, human_only] (BadgeSetShownParams) -> BadgeView;
    BadgeGrantFounder = "badge.grant_founder" [Owner, Global, human_only] (UserParams) -> Empty;

    // 프로필·메모
    ProfileGet = "profile.get" [Read, Filtered] (ProfileGetParams) -> Profile;
    ProfileSet = "profile.set" [Write, Global] (ProfileSetParams) -> Profile;
    ProfileSetCrew = "profile.set_crew" [Write, Required] (ProfileSetCrewParams) -> Empty;
    NoteGet = "note.get" [Read, Global] (UserParams) -> NoteView;
    NoteSet = "note.set" [Write, Global] (NoteSetParams) -> Empty;

    // 친구
    FriendLinkCreate = "friend.link.create" [Write, Global] (FriendLinkCreateParams) -> FriendLinkCreateResult;
    FriendLinkList = "friend.link.list" [Read, Global] (Empty) -> Vec<FriendLinkView>;
    FriendLinkRevoke = "friend.link.revoke" [Write, Global] (TokenIdParams) -> Empty;
    FriendAdd = "friend.add" [Write, Global] (UrlParams) -> FriendAddResult;
    FriendRequests = "friend.requests" [Read, Global] (Empty) -> Vec<FriendRequestView>;
    FriendAccept = "friend.accept" [Write, Global] (RequestIdParams) -> SpaceRef;
    FriendReject = "friend.reject" [Write, Global] (RequestIdParams) -> Empty;
    FriendList = "friend.list" [Read, Global] (Empty) -> Vec<FriendView>;
    /// 임시 연락처를 친구로(상대가 수락하면 친구, 서로 보냈으면 바로).
    FriendPropose = "friend.propose" [Write, Global] (UserParams) -> FriendProposeResult;
    /// 그 연락처와 대화할 1:1 공간(크루로 바꿔 둘만의 공간이 없으면 새로 만들어 상대에게 보냄).
    FriendOpen = "friend.open" [Write, Global] (UserParams) -> SpaceRef;
    /// 1:1 공간에서 나가 되돌릴 수 없게 관계를 끊으므로 자동화 토큰이면 사람 확인.
    FriendRemove = "friend.remove" [Write, Global, destructive] (UserParams) -> Empty;
    FriendSafetyNumber = "friend.safety_number" [Read, Global] (UserParams) -> SafetyNumberView;
    /// 안전 번호 확인은 사람이 직접 해야 의미가 있습니다.
    FriendVerify = "friend.verify" [Write, Global, human_only] (FriendVerifyParams) -> Empty;

    // 신뢰 스위치(이 기기의 IP를 보여도 되는 사람·크루)
    /// 한 사람에 대한 신뢰와 지금 연결 방식(IP는 싣지 않음).
    TrustGet = "trust.get" [Read, Global] (UserParams) -> TrustView;
    /// 이 기기의 IP를 그 사람에게 드러내는 동의라 자동화 토큰이 대신 켜지 못하게 사람만.
    TrustSetUser = "trust.set_user" [Write, Global, human_only] (TrustSetUserParams) -> Empty;
    /// 크루 멤버에게 IP를 드러내는 동의라 사람만.
    TrustSetSpace = "trust.set_space" [Write, Required, human_only] (TrustSetSpaceParams) -> Empty;

    // 공간·크루
    SpaceList = "space.list" [Read, Filtered] (Empty) -> Vec<SpaceSummary>;
    SpaceGet = "space.get" [Read, Required] (SpaceParams) -> SpaceView;
    CrewCreate = "crew.create" [Write, Global] (CrewCreateParams) -> SpaceRef;
    CrewUpdate = "crew.update" [Admin, Required] (CrewUpdateParams) -> Empty;
    /// 크루 STUN 서버 목록(`manage_space`). 그 서버가 멤버 기기의 공인 IP를 보게 되므로 사람만.
    CrewSetServers = "crew.set_servers" [Admin, Required, human_only] (CrewServersParams) -> Empty;
    /// 이 기기가 그 크루의 서버를 쓸지. 크루 서버가 이 기기의 공인 IP를 보게 되므로 사람만.
    CrewUseServers = "crew.use_servers" [Settings, Required, human_only] (CrewUseServersParams) -> Empty;
    /// 1:1 대화를 앞으로 들어올 멤버에게 드러낼 수 있는 동의라 자동화 토큰이면 사람 확인.
    CrewConvertRequest = "crew.convert_request" [Write, Required, destructive] (CrewConvertRequestParams) -> Empty;
    CrewConvertAccept = "crew.convert_accept" [Write, Required, destructive] (SpaceParams) -> Empty;
    /// 이 기기의 크루 데이터를 되돌릴 수 없게 지움(자동화 토큰이면 사람 확인).
    CrewLeave = "crew.leave" [Write, Required, destructive] (SpaceParams) -> Empty;
    CrewTransferOwnership = "crew.transfer_ownership" [Admin, Required, destructive] (TransferOwnershipParams) -> Empty;
    /// 크루 해산(소유자만). 모든 멤버에게서 크루가 끝나고 이 기기의 크루 데이터를 지움(자동화 토큰이면 사람 확인).
    CrewDisband = "crew.disband" [Admin, Required, destructive] (SpaceParams) -> Empty;
    /// 커밋 확정 기기 지정(소유자 기기만, 대상은 소유자·관리자의 활성 기기).
    CrewSetCommitter = "crew.set_committer" [Admin, Required] (CommitterSetParams) -> Empty;
    /// 확정 기기가 될 수 있는 기기(소유자·관리자의 활성 기기)와 지금 확정 기기.
    CrewCommitterCandidates = "crew.committer_candidates" [Read, Required] (SpaceParams) -> Vec<CommitterCandidate>;
    MemberList = "member.list" [Read, Required] (SpaceParams) -> Vec<MemberView>;
    /// 큰 크루용 멤버 목록 쪽: 온라인·최근 거르기와 이름 검색.
    MemberSearch = "member.search" [Read, Required] (MemberSearchParams) -> MemberPage;
    MemberKick = "member.kick" [Admin, Required, destructive] (MemberKickParams) -> Empty;
    MemberSetRoles = "member.set_roles" [Admin, Required, destructive] (MemberSetRolesParams) -> Empty;

    // 모더레이션
    /// 추방하고 밴 목록에 올림(BAN_MEMBERS). 밴된 사용자는 옛 초대·새 초대로 다시 들어오지 못합니다.
    MemberBan = "member.ban" [Admin, Required, destructive] (MemberBanParams) -> Empty;
    /// 밴 해제. 다시 들어오려면 새 초대가 필요합니다.
    MemberUnban = "member.unban" [Admin, Required, destructive] (SpaceUserParams) -> Empty;
    MemberBans = "member.bans" [Admin, Required] (SpaceParams) -> Vec<BanView>;
    /// 이 기기에만 저장하는 차단. 상대에게 알리거나 퍼뜨리지 않습니다.
    UserBlock = "user.block" [Write, Global] (UserParams) -> Empty;
    UserUnblock = "user.unblock" [Write, Global] (UserParams) -> Empty;
    UserBlocked = "user.blocked" [Read, Global] (Empty) -> Vec<BlockedView>;
    /// 이 기기가 아는 그 사용자의 기기(폐기한 것 포함)와 차단 여부.
    UserDevices = "user.devices" [Read, Global] (UserParams) -> Vec<UserDeviceView>;
    /// 남의 계정에서 기기 하나만 이 기기에서 차단합니다(인증 거절, 그 기기의 글 접기). 알리거나 퍼뜨리지 않습니다.
    UserBlockDevice = "user.block_device" [Write, Global] (UserDeviceParams) -> Empty;
    UserUnblockDevice = "user.unblock_device" [Write, Global] (UserDeviceParams) -> Empty;
    UserBlockedDevices = "user.blocked_devices" [Read, Global] (Empty) -> Vec<BlockedDeviceView>;
    /// 운영진 기기에만 봉인해 보내는 신고.
    MessageReport = "message.report" [Write, Required] (MessageReportParams) -> ReportRef;
    ReportList = "report.list" [Admin, Required] (ReportListParams) -> Vec<ReportView>;
    /// `dismiss`가 아니면 자동화 토큰은 사람 확인을 거칩니다.
    ReportResolve = "report.resolve" [Admin, Required, destructive_others] (ReportResolveParams) -> Empty;

    // 채널
    ChannelList = "channel.list" [Read, Required] (SpaceParams) -> Vec<ChannelView>;
    ChannelCreate = "channel.create" [Admin, Required] (ChannelCreateParams) -> ChannelRef;
    ChannelUpdate = "channel.update" [Admin, Required] (ChannelUpdateParams) -> Empty;
    ChannelDelete = "channel.delete" [Admin, Required, destructive] (SpaceChannelParams) -> Empty;
    ChannelJoin = "channel.join" [Write, Required] (SpaceChannelParams) -> Empty;
    ChannelLeave = "channel.leave" [Write, Required] (SpaceChannelParams) -> Empty;
    /// 휘발성 채널의 API 읽기 허용(사람만, owner 토큰).
    ChannelAllowApiRead = "channel.allow_api_read" [Owner, Required, human_only] (AllowApiReadParams) -> Empty;
    /// 채널을 P2P 전용으로 바꾸거나 Tor로 되돌림(양쪽 다 언제든). 멤버들의 내용이 참여 전까지 잠기므로 사람만.
    ChannelSetP2p = "channel.set_p2p" [Admin, Required, human_only] (ChannelSetP2pParams) -> Empty;
    /// P2P 전용 채널에 이 기기로 참여(IP 노출 경고를 받아들임). 자동화 토큰이 사용자를 IP 노출에 동의시키지 못하게 사람만.
    ChannelP2pJoin = "channel.p2p_join" [Write, Required, human_only] (SpaceChannelParams) -> Empty;
    /// P2P 전용 채널 참여를 그만둠(이 기기에서 다시 잠김).
    ChannelP2pLeave = "channel.p2p_leave" [Write, Required, human_only] (SpaceChannelParams) -> Empty;
    ChannelMarkRead = "channel.mark_read" [Write, Required] (MarkReadParams) -> Empty;
    /// 글 쓰는 중이라고 알림(몇 초마다 불러도 됨, 데몬이 간격을 맞춤).
    ChannelTyping = "channel.typing" [Write, Required] (SpaceChannelParams) -> Empty;

    // 메시지
    MessageList = "message.list" [Read, Required] (MessageListParams) -> MessageListResult;
    MessageSend = "message.send" [Write, Required] (MessageSendParams) -> EventRef;
    MessageEdit = "message.edit" [Write, Required] (MessageEditParams) -> Empty;
    /// 남의 메시지는 MANAGE_MESSAGES(자동화 토큰이면 사람 확인).
    MessageDelete = "message.delete" [Write, Required, destructive_others] (MessageRefParams) -> Empty;
    MessageReact = "message.react" [Write, Required] (ReactParams) -> Empty;
    MessageUnreact = "message.unreact" [Write, Required] (ReactParams) -> Empty;
    MessageSearch = "message.search" [Read, OptionalFiltered] (MessageSearchParams) -> MessageSearchPage;
    ForumCreatePost = "forum.create_post" [Write, Required] (ForumCreatePostParams) -> EventRef;
    ForumListPosts = "forum.list_posts" [Read, Required] (ForumListPostsParams) -> Vec<ForumPostView>;
    ForumUpdatePost = "forum.update_post" [Write, Required] (ForumUpdatePostParams) -> Empty;
    ForumGetPost = "forum.get_post" [Read, Required] (ForumPostParams) -> ForumPostView;
    /// 포럼 대문 게시글 정하기·빼기(그 채널의 `manage_channels`).
    ForumSetFeatured = "forum.set_featured" [Write, Required] (ForumFeaturedParams) -> Empty;

    // 첨부·이모지·블롭
    BlobStatus = "blob.status" [Read, Required] (BlobParams) -> BlobStatusView;
    BlobFetch = "blob.fetch" [Read, Required] (BlobFetchParams) -> Empty;
    /// `binary: true`면 결과가 바이너리 프레임으로 옵니다.
    BlobRead = "blob.read" [Read, Required] (BlobReadParams) -> BlobReadResult;
    BlobSave = "blob.save" [Write, Required] (BlobSaveParams) -> Empty;
    /// 받은 첨부가 쓰는 저장 공간과 한도.
    BlobCacheStatus = "blob.cache_status" [Read, Global] (Empty) -> BlobCacheView;
    /// 쓸 자리를 줄이면(한도를 낮추거나 최소 여유 공간을 올림) 바로 정리하므로 자동화 토큰은 사람 확인.
    BlobCacheSetLimit = "blob.cache_set_limit" [Settings, Global, destructive_others] (BlobCacheLimitParams) -> BlobCacheView;
    /// 정리 대상(고정·내 첨부·이모지·프로필 이미지 빼고 받은 첨부)의 데이터를 모두 비움. 다시 받을 수 있습니다.
    BlobCacheClear = "blob.cache_clear" [Settings, Global, destructive] (Empty) -> BlobCacheClearResult;
    /// 저장 공간 정리에서 빼고, 아직 받지 않았으면 받기 시작.
    BlobPin = "blob.pin" [Write, Required] (BlobParams) -> Empty;
    BlobUnpin = "blob.unpin" [Write, Required] (BlobParams) -> Empty;
    BlobPins = "blob.pins" [Read, OptionalFiltered] (OptSpaceParams) -> Vec<BlobPinView>;
    /// 이 기기에 둔 첨부 데이터만 지움(메시지는 그대로, 다시 받을 수 있음). 내가 올린 첨부면 이 기기에서 더는 내주지 못합니다.
    BlobEvict = "blob.evict" [Write, Required, destructive] (BlobParams) -> Empty;
    /// 이 기기가 보내거나 받은 첨부를 실은 메시지(라이브러리 탭, 일반 등급 채널만).
    LibraryList = "library.list" [Read, OptionalFiltered] (LibraryListParams) -> LibraryPage;
    /// 이 기기에 데이터가 없는 첨부를 해시가 같은 파일로 다시 잇기(일반 채널만). 그 파일을 다시 암호화해 같은 블롭인지 봅니다.
    BlobRelink = "blob.relink" [Write, Required] (BlobRelinkParams) -> BlobStatusView;
    /// 클립보드에서 붙여넣은 첨부를 RAM에 미리 올립니다(경로 대신 `AttachmentInput.paste`로 보냄).
    BlobPaste = "blob.paste" [Write, Global] (BlobPasteParams) -> BlobPasteResult;
    /// 미리 올린 붙여넣기를 지웁니다(보냈거나 뺐을 때).
    BlobPasteDiscard = "blob.paste_discard" [Write, Global] (BlobPasteDiscardParams) -> Empty;
    /// 지금 전체 올리기·받기 속도와 내가 내주는 첨부를 받는 사람.
    TransferRatesGet = "transfer.rates" [Read, Global] (Empty) -> TransferRates;
    /// 크루·채널·사람별 속도 제한(이 기기만의 설정).
    RateLimits = "rate.limits" [Read, Global] (Empty) -> Vec<RateLimitView>;
    RateSetLimit = "rate.set_limit" [Settings, Global] (RateLimitParams) -> Empty;
    EmojiList = "emoji.list" [Read, Required] (SpaceParams) -> Vec<EmojiView>;
    EmojiCreate = "emoji.create" [Write, Required] (EmojiCreateParams) -> EmojiCreateResult;
    EmojiDelete = "emoji.delete" [Write, Required] (EmojiDeleteParams) -> Empty;

    // 역할·초대
    RoleList = "role.list" [Read, Required] (SpaceParams) -> Vec<Role>;
    RoleCreate = "role.create" [Admin, Required] (RoleCreateParams) -> RoleRef;
    RoleUpdate = "role.update" [Admin, Required, destructive] (RoleUpdateParams) -> Empty;
    RoleDelete = "role.delete" [Admin, Required, destructive] (RoleParams) -> Empty;
    InviteCreate = "invite.create" [Admin, Required, destructive] (InviteCreateParams) -> UrlResult;
    InviteList = "invite.list" [Read, Required] (SpaceParams) -> Vec<InviteView>;
    InviteRevoke = "invite.revoke" [Admin, Required] (InviteRevokeParams) -> Empty;
    InviteJoin = "invite.join" [Write, Global] (UrlParams) -> InviteJoinResult;

    // 동기화 설정
    SeedGet = "seed.get" [Read, OptionalFiltered] (OptSpaceParams) -> SeederPolicy;
    SeedSet = "seed.set" [Settings, OptionalGlobal] (SeedSetParams) -> Empty;
    SeedExcludeMessage = "seed.exclude_message" [Settings, Required] (SeedExcludeParams) -> Empty;
    PeerGet = "peer.get" [Read, OptionalFiltered] (OptSpaceParams) -> PeerPolicy;
    PeerSet = "peer.set" [Settings, OptionalGlobal] (PeerSetParams) -> Empty;
    SeedCoverage = "seed.coverage" [Read, Required] (SpaceParams) -> Vec<ChannelCoverage>;

    // 보관 전용 노드
    /// 보관 노드(`yangpad --archive`)에서: 관리자에게 줄 1회용 제안 링크.
    ArchiveOffer = "archive.offer" [Owner, Global] (ArchiveOfferParams) -> ArchiveOfferResult;
    /// 보관 노드에서: 받아 둔 공간(내용 없이 수치만).
    ArchiveHosted = "archive.hosted" [Read, Global] (Empty) -> Vec<ArchiveHostedView>;
    /// 보관 노드에서: 받아 둔 공간을 지움(그 크루에는 알리지 않음).
    ArchiveDrop = "archive.drop" [Owner, Global, destructive] (SpaceParams) -> Empty;
    /// 크루에서(MANAGE_SPACE): 제안 링크로 보관 노드를 지정. 보관 노드가 암호문과 멤버 기기 명단을 받게 되므로 사람 확인.
    ArchiveDesignate = "archive.designate" [Admin, Required, destructive] (ArchiveDesignateParams) -> ArchiveView;
    /// 지정 해제. 보관 노드는 받아 둔 것을 지웁니다.
    ArchiveRevoke = "archive.revoke" [Admin, Required, destructive] (ArchiveRevokeParams) -> Empty;
    ArchiveList = "archive.list" [Read, Required] (SpaceParams) -> Vec<ArchiveView>;

    // 네트워크·Tor
    NetStatusGet = "net.status" [Read, Global] (Empty) -> NetStatus;
    /// P2P 스웜이 공유기 밖 주소를 알아낼 때 쓰는 STUN 서버. Tor 연결에는 쓰지 않습니다.
    NetStunServers = "net.stun_servers" [Read, Global] (Empty) -> StunServers;
    /// STUN 서버 목록 바꾸기(`null`이면 기본 목록). 고른 서버가 이 기기의 공인 IP를 보게 되므로 사람만.
    NetSetStunServers = "net.set_stun_servers" [Settings, Global, human_only] (StunServersParams) -> Empty;
    /// 큰 첨부를 Tor로 주고받을 때 파일 전용 회로를 쓰는지(기본 꺼짐).
    NetFileCircuits = "net.file_circuits" [Read, Global] (Empty) -> FileCircuits;
    /// 파일 회로 켜기·끄기. 켜면 이 기기의 onion 서비스가 회로를 더 자주 만들어 익명성의 대가가 있으므로 사람만.
    NetSetFileCircuits = "net.set_file_circuits" [Settings, Global, human_only] (FileCircuitsParams) -> Empty;
    /// 채널 묶음·알림 토픽을 쓰는지(기본 꺼짐).
    NetChannelTopics = "net.channel_topics" [Read, Global] (Empty) -> ChannelTopics;
    /// 채널 토픽 켜기·끄기. 켜면 따라가지 않는 채널의 새 글은 실시간으로 오지 않고 알림·안 읽음 힌트로만 알게 되므로 사람만.
    NetSetChannelTopics = "net.set_channel_topics" [Settings, Global, human_only] (ChannelTopicsParams) -> Empty;
    /// Tor 브리지(검열 우회, 기본 꺼짐). 직접 넣은 브리지 줄은 공개되지 않은 브리지 주소라 사람만.
    NetBridges = "net.bridges" [Read, Global, human_only] (Empty) -> BridgesView;
    /// 브리지 켜기·끄기·바꾸기. 브리지(Snowflake면 프록시)가 이 기기의 IP를 보게 되므로 사람만.
    NetSetBridges = "net.set_bridges" [Settings, Global, human_only] (BridgesParams) -> Empty;
    NetPeers = "net.peers" [Read, Global] (Empty) -> Vec<PeerView>;

    // 음성·화면공유
    VoiceJoin = "voice.join" [Write, Required] (SpaceChannelParams) -> VoiceJoinResult;
    VoiceLeave = "voice.leave" [Write, Required] (SpaceChannelParams) -> Empty;
    VoiceSignal = "voice.signal" [Write, Required] (VoiceSignalParams) -> Empty;
    VoiceMedia = "voice.media" [Write, Required] (VoiceMediaParams) -> Empty;
    VoiceState = "voice.state" [Write, Required] (VoiceStateParams) -> Empty;
    VoiceWatch = "voice.watch" [Write, Required] (VoiceWatchParams) -> Empty;

    // 토큰·감사·사람 확인
    TokenCreate = "token.create" [Owner, Global, destructive] (TokenCreateParams) -> TokenCreateResult;
    TokenList = "token.list" [Settings, Global] (Empty) -> Vec<TokenView>;
    TokenRevoke = "token.revoke" [Settings, Global] (IdParams) -> Empty;
    AuditList = "audit.list" [Settings, Global] (AuditListParams) -> Vec<AuditEntry>;
    ConfirmList = "confirm.list" [Read, Filtered] (Empty) -> Vec<PendingConfirmation>;
    /// 사람 토큰만. 데몬은 승인하는 토큰이 원래 메서드의 범위와 그 공간(공간 제한 토큰이면)도 가졌는지 확인해야 합니다.
    ConfirmApprove = "confirm.approve" [Admin, Filtered, human_only] (IdParams) -> ConfirmApproveResult;
    ConfirmDeny = "confirm.deny" [Write, Filtered, human_only] (IdParams) -> Empty;

    // 이벤트 구독(서버가 직접 처리)
    EventsSubscribe = "events.subscribe" [Read, Filtered] (SubscribeParams) -> SubscribeResult;
    EventsUnsubscribe = "events.unsubscribe" [Read, Filtered] (UnsubscribeParams) -> Empty;
}
