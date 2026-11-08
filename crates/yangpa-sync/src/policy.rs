//! 시더·피어 설정, 시더 요청 처리 5단계 검사, 대역폭 토큰 버킷.
//!
//! 시더 기본값은 기획서 리스크 대응대로 '본인이 연 채널만'입니다. 남의 콘텐츠를 모르는 사이 보관·배포하는
//! 부담을 줄이기 위해, 사용자가 직접 열어 본 채널의 데이터만 제공하고 나머지는 명시적으로 켜야 합니다.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use yangpa_core::blob::MAX_CHUNK_SIZE;
use yangpa_core::body::Attachment;
use yangpa_core::event::{Lane, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{HistoryVisibility, Permissions, SpaceState};
use yangpa_core::wire::{WireErrorCode, YpResponse};

/// 시더·피어 설정의 데이터 종류. API 이름: `text`, `image`, `video`, `file`, `emoji`, `history_bundle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// 메시지·키 이벤트.
    Text,
    Image,
    Video,
    /// 그 밖의 첨부(음성 파일 포함).
    File,
    /// 커스텀 이모지 이미지.
    Emoji,
    /// 봉인된 과거 기록 묶음.
    HistoryBundle,
}

impl DataType {
    pub const ALL: [DataType; 6] = [
        DataType::Text,
        DataType::Image,
        DataType::Video,
        DataType::File,
        DataType::Emoji,
        DataType::HistoryBundle,
    ];

    /// 첨부 MIME 형식으로 종류를 정합니다.
    pub fn from_mime(mime: &str) -> DataType {
        let m = mime.trim().to_ascii_lowercase();
        if m.starts_with("image/") {
            DataType::Image
        } else if m.starts_with("video/") {
            DataType::Video
        } else {
            DataType::File
        }
    }

    pub fn of_attachment(a: &Attachment) -> DataType {
        Self::from_mime(&a.mime)
    }

    pub const fn name(self) -> &'static str {
        match self {
            DataType::Text => "text",
            DataType::Image => "image",
            DataType::Video => "video",
            DataType::File => "file",
            DataType::Emoji => "emoji",
            DataType::HistoryBundle => "history_bundle",
        }
    }

    /// 기본 우선순위(클수록 먼저). 텍스트가 가장 먼저, 과거 기록 묶음이 가장 나중.
    /// 공간 전체에 속해 채널 없이 요청할 수 있는 종류(공간 이모지, 받는 기기에게 봉인된 과거 기록 묶음).
    /// 나머지는 그 데이터를 실은 채널을 알아야 볼 권한을 검사할 수 있습니다.
    pub const fn channel_optional(self) -> bool {
        matches!(self, DataType::Emoji | DataType::HistoryBundle)
    }

    pub const fn default_priority(self) -> u8 {
        match self {
            DataType::Text => 250,
            DataType::Emoji => 200,
            DataType::Image => 150,
            DataType::Video => 100,
            DataType::File => 50,
            DataType::HistoryBundle => 20,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for DataType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DataType::ALL
            .into_iter()
            .find(|t| t.name() == s)
            .ok_or_else(|| format!("알 수 없는 데이터 종류: {s}"))
    }
}

/// 큰 조각 하나는 통과할 수 있도록 하는 최소 버스트.
pub const MIN_BURST_BYTES: u64 = MAX_CHUNK_SIZE as u64;
/// 동시 전송 수가 찼을 때 다시 요청하라고 알려 주는 시간.
pub const BUSY_RETRY_MS: u32 = 500;
/// 시더 송신·피어 수신 대역폭 기본 상한(직접 연결·Tor 같음, 사용자 요청 2026-10-21).
pub const DEFAULT_RATE_LIMIT_BPS: u64 = 10 * 1024 * 1024;

/// 시더 설정(내가 남에게 제공하는 범위).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeederPolicy {
    /// 꺼도 상태·키 합의·공간 키는 제공합니다(모든 멤버에게 필수).
    pub enabled: bool,
    pub types: BTreeSet<DataType>,
    /// `None`이면 기본 범위('본인이 연 채널만', `all_channels`면 볼 수 있는 모든 채널).
    /// `Some`이면 정확히 그 채널들만.
    pub include_channels: Option<BTreeSet<ChannelId>>,
    /// `include_channels`가 `None`일 때 열지 않은 채널까지 제공.
    pub all_channels: bool,
    pub exclude_channels: BTreeSet<ChannelId>,
    /// 개별 메시지 제외(첨부도 그 메시지 단위로 제외).
    pub excluded_events: BTreeSet<EventId>,
    /// 송신 대역폭 상한(바이트/초). `None`이면 무제한.
    pub upload_limit_bps: Option<u64>,
    /// 동시 전송 수.
    pub max_concurrent: u32,
    /// 제공 범위 채널(`include_channels`가 `None`이면 열지 않은 채널까지)의 첨부를 열지 않아도 미리 받아 둠
    /// (헤드리스 시더). 받아 두고 내주지 않으면 쓸모가 없으므로 켜면 열지 않은 채널도 제공합니다.
    pub prefetch: bool,
}

impl Default for SeederPolicy {
    fn default() -> Self {
        SeederPolicy {
            enabled: true,
            types: DataType::ALL.into_iter().collect(),
            include_channels: None,
            all_channels: false,
            exclude_channels: BTreeSet::new(),
            excluded_events: BTreeSet::new(),
            upload_limit_bps: Some(DEFAULT_RATE_LIMIT_BPS),
            max_concurrent: 4,
            prefetch: false,
        }
    }
}

impl SeederPolicy {
    /// Tor로 도는 기기의 기본값(P2P 전용 채널도): 동시 전송 수만 줄여 텍스트를 우선합니다(대역폭은 Tor 회로가 먼저
    /// 한계라 직접 연결과 같게 둠).
    pub fn tor_default() -> Self {
        SeederPolicy {
            max_concurrent: 2,
            ..Self::default()
        }
    }

    /// 채널이 제공 범위에 드는지. `opened`는 내가 그 채널을 연 적이 있는지.
    pub fn seeds_channel(&self, channel: &ChannelId, opened: bool) -> bool {
        if self.exclude_channels.contains(channel) {
            return false;
        }
        match &self.include_channels {
            Some(set) => set.contains(channel),
            None => self.all_channels || self.prefetch || opened,
        }
    }

    /// 데이터 종류 × 채널 × 개별 메시지 제외까지 본 제공 범위.
    pub fn seeds(
        &self,
        data_type: DataType,
        channel: Option<&ChannelId>,
        event: Option<&EventId>,
        opened: bool,
    ) -> bool {
        self.enabled
            && self.types.contains(&data_type)
            && channel.is_none_or(|c| self.seeds_channel(c, opened))
            && event.is_none_or(|e| !self.excluded_events.contains(e))
    }
}

/// 피어 설정(내가 받는 범위).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerPolicy {
    pub types: BTreeSet<DataType>,
    /// 종류별 우선순위(클수록 먼저). 없는 종류는 [`DataType::default_priority`].
    pub priorities: BTreeMap<DataType, u8>,
    /// 수신 대역폭 상한(바이트/초). `None`이면 무제한.
    pub download_limit_bps: Option<u64>,
    /// 이 크기 이하의 첨부만 자동으로 받습니다(그보다 크면 사용자가 눌러야 받음).
    pub auto_download_max_bytes: u64,
}

impl Default for PeerPolicy {
    fn default() -> Self {
        PeerPolicy {
            types: DataType::ALL.into_iter().collect(),
            priorities: BTreeMap::new(),
            download_limit_bps: Some(DEFAULT_RATE_LIMIT_BPS),
            auto_download_max_bytes: 20 * 1024 * 1024,
        }
    }
}

impl PeerPolicy {
    /// Tor로 도는 기기의 기본값(P2P 전용 채널도): 자동 다운로드 한도만 줄입니다(대역폭은 직접 연결과 같음).
    pub fn tor_default() -> Self {
        PeerPolicy {
            auto_download_max_bytes: 2 * 1024 * 1024,
            ..Self::default()
        }
    }

    pub fn wants(&self, t: DataType) -> bool {
        self.types.contains(&t)
    }

    pub fn priority(&self, t: DataType) -> u8 {
        self.priorities
            .get(&t)
            .copied()
            .unwrap_or(t.default_priority())
    }

    /// 자동으로 받을지. 텍스트는 크기와 무관하게 받습니다(받을 종류에 있으면). 받기 제한이 0이면 첨부는 받지 않습니다.
    pub fn auto_download(&self, t: DataType, size: u64) -> bool {
        self.wants(t)
            && (t == DataType::Text
                || (size <= self.auto_download_max_bytes && !self.downloads_off()))
    }

    /// 받기 제한 0 = 첨부를 받지 않음.
    pub fn downloads_off(&self) -> bool {
        self.download_limit_bps == Some(0)
    }
}

/// 시더가 받은 요청 하나의 문맥.
///
/// 블롭(첨부·이모지) 요청은 `lane = Lane::Message`, `data_type`은 MIME으로 정한 종류, `channel`·`event`는
/// 그 첨부를 실은 메시지의 것(공간 이모지는 채널 없음)으로 넘깁니다. 첨부가 어느 메시지에 실렸는지
/// 모르면 `channel`·`event_lamport`를 비워 넘기면 되고, 그러면 검사가 안전한 쪽(거절)으로 끝납니다.
#[derive(Clone, Copy, Debug)]
pub struct SeedRequest<'a> {
    pub state: &'a SpaceState,
    /// 연결 인증(`PeerAuth`)으로 확인한 요청자 사용자와 기기.
    pub requester: UserId,
    /// 폐기된 기기는 사용자가 아직 멤버여도 거절해야 하므로, 해소된 상태의 기기 목록으로 확인합니다
    /// (`PeerAuth`에 실린 기기 목록은 폐기 전 옛 판일 수 있음).
    pub requester_device: DeviceId,
    pub lane: Lane,
    pub channel: Option<ChannelId>,
    pub data_type: DataType,
    /// 요청한 이벤트(블롭이면 그 첨부를 실은 메시지).
    pub event: Option<EventId>,
    pub event_lamport: Option<u64>,
    /// 요청자가 가장 최근에 멤버가 된 논리 시계(`Resolved::member_since`). `None`이면 처음 멤버.
    pub requester_since: Option<u64>,
    /// 시더 본인이 이 채널을 연 적이 있는지('본인이 연 채널만' 기본값 판단).
    pub channel_opened: bool,
    /// 보낼 바이트 수(대역폭 계산).
    pub bytes: u64,
    /// 지금 함께 보내 주고 있는 다른 기기 수(요청자 기기의 겹친 요청은 세지 않음).
    pub in_flight: u32,
    pub now_ms: u64,
}

/// 시더 요청 거절.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedRefusal {
    Denied(WireErrorCode),
    /// 대역폭·동시 전송 여유가 없음. 이 시간 뒤 다시 요청.
    Busy {
        retry_after_ms: u32,
    },
}

impl SeedRefusal {
    /// 노드 간 응답으로 바꿉니다. 메시지에는 요청 내용을 넣지 않습니다.
    pub fn to_response(self) -> YpResponse {
        match self {
            SeedRefusal::Busy { retry_after_ms } => YpResponse::Busy { retry_after_ms },
            SeedRefusal::Denied(code) => {
                let msg = match code {
                    WireErrorCode::NotMember => "공간 멤버가 아님",
                    WireErrorCode::Forbidden => "채널을 볼 권한이 없음",
                    WireErrorCode::HistoryPrivate => "입장 이전 기록은 공개되지 않음",
                    WireErrorCode::NotSeeded => "이 시더가 제공하지 않는 데이터",
                    WireErrorCode::BadRequest => "잘못된 요청",
                    _ => "거절됨",
                };
                YpResponse::error(code, msg)
            }
        }
    }
}

/// 시더 요청 처리 검사. 순서대로 보고 처음 실패한 단계에서 멈춥니다.
///
/// 1. 요청자가 공간의 현재 멤버이고 요청 기기가 그 멤버의 활성 기기인가 (`NotMember`)
/// 2. 요청자가 그 채널을 볼 권한이 있는가 (`Forbidden`)
/// 3. 요청한 이벤트가 요청자 입장 이전이면 채널의 과거 기록 공개가 켜져 있는가 (`HistoryPrivate`).
///    요청자가 나중에 들어온 멤버인데 이벤트 시점을 모르면 입장 이전으로 봅니다.
/// 4. 시더의 제공 범위(데이터 종류 × 채널 × 개별 메시지 제외)에 포함되는가 (`NotSeeded`)
/// 5. 대역폭·동시 전송 여유가 있는가 (없으면 `Busy`)
///
/// 상태·키 합의 레인과 공간 수준 키(채널 없는 키 레인)는 모든 멤버에게 필수라 1단계만 봅니다.
/// 메시지 레인은 채널 없는 공간 데이터([`DataType::channel_optional`])가 아니면 채널이 있어야 합니다
/// (채널을 모르면 2·3단계를 검사할 수 없으므로 `BadRequest`).
/// 통과하면 대역폭 버킷에서 `bytes`만큼 씁니다.
pub fn check_seed_request(
    policy: &SeederPolicy,
    req: &SeedRequest<'_>,
    bucket: &mut TokenBucket,
) -> Result<(), SeedRefusal> {
    if !req.state.is_member(&req.requester)
        || !req
            .state
            .device_belongs(&req.requester, &req.requester_device)
    {
        return Err(SeedRefusal::Denied(WireErrorCode::NotMember));
    }
    let required = matches!(req.lane, Lane::State | Lane::KeyAgreement)
        || (req.lane == Lane::Key && req.channel.is_none());
    if required {
        return Ok(());
    }
    // 여기까지 온 채널 없는 요청은 메시지 레인뿐입니다(채널 없는 키 레인은 위에서 끝남).
    if req.channel.is_none() && !req.data_type.channel_optional() {
        return Err(SeedRefusal::Denied(WireErrorCode::BadRequest));
    }

    if let Some(c) = &req.channel {
        let Some(ch) = req.state.channels.get(c) else {
            return Err(SeedRefusal::Denied(WireErrorCode::Forbidden));
        };
        if !req
            .state
            .channel_permissions(&req.requester, c)
            .contains(Permissions::VIEW_CHANNEL)
        {
            return Err(SeedRefusal::Denied(WireErrorCode::Forbidden));
        }
        if let Some(since) = req.requester_since
            && ch.history == HistoryVisibility::Private
            && req.event_lamport.is_none_or(|at| at < since)
        {
            return Err(SeedRefusal::Denied(WireErrorCode::HistoryPrivate));
        }
    }

    if !policy.seeds(
        req.data_type,
        req.channel.as_ref(),
        req.event.as_ref(),
        req.channel_opened,
    ) {
        return Err(SeedRefusal::Denied(WireErrorCode::NotSeeded));
    }

    if req.in_flight >= policy.max_concurrent.max(1) {
        return Err(SeedRefusal::Busy {
            retry_after_ms: BUSY_RETRY_MS,
        });
    }
    bucket.configure(policy.upload_limit_bps, req.now_ms);
    bucket
        .try_consume(req.bytes, req.now_ms)
        .map_err(|retry_after_ms| SeedRefusal::Busy { retry_after_ms })
}

/// 부분 전달 스트림(받은편지함·키 그룹)을 이 요청 기기에게 내줘도 되는지. 공간·채널 스트림이면 `None`(보통 검사).
///
/// 받은편지함 u는 u의 활성 기기와 u의 전달자(`inbox_relay`: u의 기기를 더한 적용된 가입·기기 목록 갱신을 쓴 활성
/// 기기, 데몬이 해소 결과로 셈)에게만, 키 그룹 g는 g의 채널 하나라도 볼 수 있는 멤버의 활성 기기에게만 줍니다. 전달자는
/// 처음에 그 기기만 믿는 가입·기기 연결 기기에게 넘겨주려고 받습니다. 자격이 없으면 시더는 거절 코드 대신 빈 스트림과
/// 똑같이 답합니다: 남의 받은편지함이 있는지, 그 그룹이 쓰이는지도 드러내지 않게.
pub fn may_read_scoped(
    state: &SpaceState,
    space: &SpaceId,
    requester: &UserId,
    device: &DeviceId,
    stream: &StreamId,
    inbox_relay: bool,
) -> Option<bool> {
    let active = state.is_member(requester) && state.device_belongs(requester, device);
    match stream {
        StreamId::Space | StreamId::Channel(_) => None,
        StreamId::Inbox(u) => Some(active && (u == requester || inbox_relay)),
        StreamId::KeyGroup(g) => Some(active && state.can_view_key_group(space, g, requester)),
    }
}

/// 바이트 단위 토큰 버킷. 시간은 호출자가 밀리초로 넘깁니다(시스템 시계를 읽지 않음).
///
/// 버스트보다 큰 요청도 버킷이 가득 차 있으면 보내고 그만큼 빚을 집니다. 그래서 큰 조각도 결국 지나가고,
/// 장기 평균은 설정한 속도를 넘지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenBucket {
    rate_bps: Option<u64>,
    burst: u64,
    /// 바이트 × 1000. 음수면 빚.
    tokens_milli: i128,
    last_ms: u64,
}

impl TokenBucket {
    /// 속도(바이트/초)로 만듭니다. 버스트는 1초 분량과 [`MIN_BURST_BYTES`] 중 큰 값. 처음엔 가득 참.
    pub fn new(rate_bps: Option<u64>, now_ms: u64) -> Self {
        let burst = default_burst(rate_bps);
        TokenBucket {
            rate_bps,
            burst,
            tokens_milli: i128::from(burst) * 1000,
            last_ms: now_ms,
        }
    }

    pub fn with_burst(rate_bps: u64, burst: u64, now_ms: u64) -> Self {
        let burst = burst.max(1);
        TokenBucket {
            rate_bps: Some(rate_bps),
            burst,
            tokens_milli: i128::from(burst) * 1000,
            last_ms: now_ms,
        }
    }

    pub fn unlimited() -> Self {
        Self::new(None, 0)
    }

    pub fn rate(&self) -> Option<u64> {
        self.rate_bps
    }

    pub fn burst(&self) -> u64 {
        self.burst
    }

    /// 속도가 바뀌었으면 지금까지 채운 뒤 새 속도로 바꿉니다(같으면 아무것도 안 함).
    pub fn configure(&mut self, rate_bps: Option<u64>, now_ms: u64) {
        if self.rate_bps == rate_bps {
            return;
        }
        self.refill(now_ms);
        let was_unlimited = self.rate_bps.is_none();
        self.rate_bps = rate_bps;
        self.burst = default_burst(rate_bps);
        let cap = i128::from(self.burst) * 1000;
        self.tokens_milli = if was_unlimited {
            cap
        } else {
            self.tokens_milli.min(cap)
        };
    }

    fn refill(&mut self, now_ms: u64) {
        // 시계가 거꾸로 가면 채우지 않습니다.
        if now_ms <= self.last_ms {
            return;
        }
        let elapsed = now_ms - self.last_ms;
        self.last_ms = now_ms;
        let Some(rate) = self.rate_bps else { return };
        let cap = i128::from(self.burst) * 1000;
        // 바이트/초 × 밀리초 = 밀리바이트.
        let add = u128::from(rate).saturating_mul(u128::from(elapsed));
        let room = (cap - self.tokens_milli).max(0) as u128;
        self.tokens_milli += add.min(room) as i128;
    }

    /// 지금 쓸 수 있는 바이트.
    pub fn available(&mut self, now_ms: u64) -> u64 {
        if self.rate_bps.is_none() {
            return u64::MAX;
        }
        self.refill(now_ms);
        (self.tokens_milli.max(0) / 1000) as u64
    }

    /// 보낼 수 있으면 쓰고 `Ok`, 아니면 다시 시도할 때까지의 밀리초.
    pub fn try_consume(&mut self, bytes: u64, now_ms: u64) -> Result<(), u32> {
        let retry = self.retry_after_ms(bytes, now_ms);
        if retry > 0 {
            return Err(retry);
        }
        if self.rate_bps.is_some() {
            self.tokens_milli -= i128::from(bytes) * 1000;
        }
        Ok(())
    }

    /// 확인하지 않고 씁니다(모자라면 빚). 다른 버킷과 함께 미리 확인한 뒤 쓸 때.
    pub fn force_consume(&mut self, bytes: u64, now_ms: u64) {
        self.refill(now_ms);
        if self.rate_bps.is_some() {
            self.tokens_milli -= i128::from(bytes) * 1000;
        }
    }

    /// 쓰지 않고, 이 크기를 보내려면 기다려야 하는 밀리초(0이면 지금 가능).
    pub fn retry_after_ms(&mut self, bytes: u64, now_ms: u64) -> u32 {
        let Some(rate) = self.rate_bps else { return 0 };
        if rate == 0 {
            // 속도 0 = 제공하지 않음. 한참 뒤에 다시 물어보게 합니다.
            return 60_000;
        }
        self.refill(now_ms);
        let need = i128::from(bytes.min(self.burst)) * 1000;
        if self.tokens_milli >= need {
            return 0;
        }
        let deficit = (need - self.tokens_milli) as u128;
        let ms = deficit.div_ceil(u128::from(rate));
        ms.clamp(1, u128::from(u32::MAX)) as u32
    }
}

fn default_burst(rate_bps: Option<u64>) -> u64 {
    rate_bps.unwrap_or(0).max(MIN_BURST_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::device::{DeviceEntry, DeviceList, MemberDevices};
    use yangpa_core::ids::{DeviceId, KeyGroupId, RoleId};
    use yangpa_core::model::{
        ChannelSettings, Member, OverwriteTarget, PermissionOverwrite, Role, SpaceKind,
        SpaceSettings,
    };
    use yangpa_core::time::Timestamp;

    struct Fx {
        state: SpaceState,
        member: UserId,
        outsider: UserId,
        public_ch: ChannelId,
        secret_ch: ChannelId,
        history_ch: ChannelId,
    }

    fn fx() -> Fx {
        let owner = UserId([1; 32]);
        let member = UserId([2; 32]);
        let mut members = BTreeMap::new();
        for u in [owner, member] {
            members.insert(
                u,
                Member {
                    user: u,
                    roles: vec![],
                    nickname: None,
                    automation: false,
                },
            );
        }
        let mut roles = BTreeMap::new();
        roles.insert(
            RoleId::EVERYONE,
            Role::everyone(Permissions::DEFAULT_EVERYONE),
        );
        let public_ch = ChannelId([1; 16]);
        let secret_ch = ChannelId([2; 16]);
        let history_ch = ChannelId([3; 16]);
        let mut secret = ChannelSettings::text("비밀");
        secret.overwrites = vec![PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        }];
        let mut history = ChannelSettings::text("공개 기록");
        history.history = HistoryVisibility::Public;
        let mut channels = BTreeMap::new();
        channels.insert(public_ch, ChannelSettings::text("일반"));
        channels.insert(secret_ch, secret);
        channels.insert(history_ch, history);
        let entry = |d: DeviceId, revoked: bool| DeviceEntry {
            device: d,
            box_key: yangpa_core::device::BoxPublicKey::filled(0),
            sign_keys: yangpa_core::device::DeviceSignKeys::filled(0),
            name: "기기".into(),
            added_at: Timestamp(0),
            revoked_at: revoked.then_some(Timestamp(5)),
        };
        let mut devices = BTreeMap::new();
        devices.insert(
            owner,
            MemberDevices::from_list(&DeviceList {
                user: owner,
                version: 1,
                devices: vec![entry(DeviceId([1; 32]), false)],
                otp: None,
            }),
        );
        devices.insert(
            member,
            MemberDevices::from_list(&DeviceList {
                user: member,
                version: 2,
                devices: vec![entry(MEMBER_DEVICE, false), entry(REVOKED_DEVICE, true)],
                otp: None,
            }),
        );
        Fx {
            state: SpaceState {
                kind: SpaceKind::Crew,
                owner,
                committer: DeviceId([9; 32]),
                settings: SpaceSettings {
                    name: "크루".into(),
                    description: String::new(),
                    icon: None,
                    icon_key: None,
                    allow_external_emoji: false,
                    strict_rotation: false,
                },
                members: members.into(),
                roles,
                channels: channels.into(),
                devices: devices.into(),
                invites: Default::default(),
                emojis: BTreeMap::new(),
                pending_crew_conversion: None,
                bans: Default::default(),
                archives: BTreeMap::new(),
                channel_transport: BTreeMap::new(),
                role_display: Default::default(),
                anonymous_channels: Default::default(),
                crew_servers: Default::default(),
                forum_featured: Default::default(),
                attach_limits: Default::default(),
                volatile_caps: Default::default(),
                edit_rules: Default::default(),
                emoji_limits: Default::default(),
                emoji_by: Default::default(),
                disbanded: false,
            },
            member,
            outsider: UserId([3; 32]),
            public_ch,
            secret_ch,
            history_ch,
        }
    }

    const MEMBER_DEVICE: DeviceId = DeviceId([2; 32]);
    const REVOKED_DEVICE: DeviceId = DeviceId([22; 32]);

    fn req<'a>(f: &'a Fx, requester: UserId, channel: Option<ChannelId>) -> SeedRequest<'a> {
        SeedRequest {
            state: &f.state,
            requester,
            requester_device: MEMBER_DEVICE,
            lane: Lane::Message,
            channel,
            data_type: DataType::Text,
            event: Some(EventId([7; 32])),
            event_lamport: Some(50),
            requester_since: Some(10),
            channel_opened: true,
            bytes: 1000,
            in_flight: 0,
            now_ms: 0,
        }
    }

    #[test]
    fn scoped_streams_only_for_entitled_devices() {
        let f = fx();
        let space = SpaceId([7; 16]);
        let owner = f.state.owner;
        let owner_dev = DeviceId([1; 32]);
        let read = |u: &UserId, d: &DeviceId, s: StreamId| {
            may_read_scoped(&f.state, &space, u, d, &s, false)
        };
        let relay = |u: &UserId, d: &DeviceId, s: StreamId| {
            may_read_scoped(&f.state, &space, u, d, &s, true)
        };
        // 공간·채널 스트림은 이 검사 대상이 아닙니다.
        assert_eq!(read(&f.member, &MEMBER_DEVICE, StreamId::Space), None);
        assert_eq!(
            read(&f.member, &MEMBER_DEVICE, StreamId::Channel(f.public_ch)),
            None
        );
        // 받은편지함: 그 사용자의 활성 기기만.
        let inbox = StreamId::Inbox(f.member);
        assert_eq!(read(&f.member, &MEMBER_DEVICE, inbox), Some(true));
        assert_eq!(read(&f.member, &REVOKED_DEVICE, inbox), Some(false));
        assert_eq!(read(&owner, &owner_dev, inbox), Some(false));
        assert_eq!(read(&f.outsider, &DeviceId([3; 32]), inbox), Some(false));
        // 다른 사용자의 기기 ID를 대도 안 됩니다(기기는 그 사용자 목록에 있어야 함).
        assert_eq!(read(&owner, &MEMBER_DEVICE, inbox), Some(false));
        // 전달자(그 사용자의 기기를 더한 기기)는 받지만, 활성 멤버 기기여야 합니다. 키 그룹에는 상관없습니다.
        assert_eq!(relay(&owner, &owner_dev, inbox), Some(true));
        assert_eq!(relay(&owner, &MEMBER_DEVICE, inbox), Some(false));
        assert_eq!(relay(&f.outsider, &DeviceId([3; 32]), inbox), Some(false));
        // 키 그룹: 그 그룹 채널을 볼 수 있는 멤버만. 보기 덮어쓰기가 없는 공개 채널은 크루 그룹(공간 스트림)을 써서
        // 그 규칙의 ID는 아무 채널의 그룹도 아니라 모르는 그룹과 같습니다. 규칙이 있는데 모두가 보는 채널은 비공개 그룹.
        let mut f = f;
        let secret = f
            .state
            .channel_key_group(&space, &f.state.channels[&f.secret_ch])
            .unwrap();
        let unused = KeyGroupId::private(&space, &f.state.channels[&f.public_ch].view_rule());
        assert_eq!(
            f.state
                .channel_key_group(&space, &f.state.channels[&f.public_ch]),
            None
        );
        let mut open = ChannelSettings::text("모두 허용");
        open.overwrites.push(PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::NONE,
        });
        let public = f.state.channel_key_group(&space, &open).unwrap();
        f.state.channels.insert(ChannelId([4; 16]), open);
        let read = |u: &UserId, d: &DeviceId, s: StreamId| {
            may_read_scoped(&f.state, &space, u, d, &s, false)
        };
        for (u, d) in [(&owner, &owner_dev), (&f.member, &MEMBER_DEVICE)] {
            assert_eq!(read(u, d, StreamId::KeyGroup(unused)), Some(false));
        }
        assert_eq!(
            read(&owner, &owner_dev, StreamId::KeyGroup(secret)),
            Some(true)
        );
        assert_eq!(
            read(&f.member, &MEMBER_DEVICE, StreamId::KeyGroup(secret)),
            Some(false)
        );
        assert_eq!(
            read(&f.member, &MEMBER_DEVICE, StreamId::KeyGroup(public)),
            Some(true)
        );
        assert_eq!(
            read(&f.member, &REVOKED_DEVICE, StreamId::KeyGroup(public)),
            Some(false)
        );
        assert_eq!(
            read(&f.outsider, &DeviceId([3; 32]), StreamId::KeyGroup(public)),
            Some(false)
        );
        // 쓰이지 않는 그룹(다른 공간의 규칙)은 아무에게도.
        let other = f
            .state
            .channel_key_group(&SpaceId([8; 16]), &f.state.channels[&f.secret_ch])
            .unwrap();
        assert_eq!(
            read(&owner, &owner_dev, StreamId::KeyGroup(other)),
            Some(false)
        );
    }

    #[test]
    fn data_type_names_and_mime() {
        assert_eq!(DataType::from_mime("image/png"), DataType::Image);
        assert_eq!(DataType::from_mime("Video/MP4"), DataType::Video);
        assert_eq!(DataType::from_mime("audio/ogg"), DataType::File);
        assert_eq!(DataType::from_mime("application/pdf"), DataType::File);
        for t in DataType::ALL {
            assert_eq!(t.name().parse::<DataType>().unwrap(), t);
            assert_eq!(
                serde_json::to_string(&t).unwrap(),
                format!("\"{}\"", t.name())
            );
        }
        assert!("nope".parse::<DataType>().is_err());
    }

    #[test]
    fn policies_roundtrip_json() {
        let mut p = SeederPolicy::default();
        p.exclude_channels.insert(ChannelId([4; 16]));
        p.include_channels = Some([ChannelId([5; 16])].into_iter().collect());
        let back: SeederPolicy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let mut q = PeerPolicy::default();
        q.priorities.insert(DataType::Video, 255);
        let json = serde_json::to_string(&q).unwrap();
        assert!(json.contains("\"video\":255"));
        let back: PeerPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q);
        // 빠진 필드는 기본값.
        let partial: SeederPolicy = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(!partial.enabled);
        assert_eq!(partial.types.len(), 6);
    }

    #[test]
    fn default_seeds_only_opened_channels() {
        let p = SeederPolicy::default();
        let c = ChannelId([1; 16]);
        assert!(p.seeds_channel(&c, true));
        assert!(!p.seeds_channel(&c, false));
        let all = SeederPolicy {
            all_channels: true,
            ..SeederPolicy::default()
        };
        assert!(all.seeds_channel(&c, false));
        let only = SeederPolicy {
            include_channels: Some(BTreeSet::new()),
            ..all.clone()
        };
        assert!(!only.seeds_channel(&c, true));
        let mut excluded = all;
        excluded.exclude_channels.insert(c);
        assert!(!excluded.seeds_channel(&c, true));
        // 미리 받기는 받아 둔 것을 내주도록 열지 않은 채널도 제공 범위에 넣음(목록을 정했으면 그 목록만).
        let prefetch = SeederPolicy {
            prefetch: true,
            ..SeederPolicy::default()
        };
        assert!(prefetch.seeds_channel(&c, false));
        let listed = SeederPolicy {
            include_channels: Some(BTreeSet::new()),
            ..prefetch.clone()
        };
        assert!(!listed.seeds_channel(&c, false));
        // 예전에 저장한 설정(prefetch 없음)은 꺼진 것으로 읽힘.
        let mut v = serde_json::to_value(SeederPolicy::default()).unwrap();
        v.as_object_mut().unwrap().remove("prefetch");
        assert!(!serde_json::from_value::<SeederPolicy>(v).unwrap().prefetch);
    }

    #[test]
    fn peer_policy_priorities_and_auto_download() {
        let mut p = PeerPolicy::default();
        assert!(p.priority(DataType::Image) > p.priority(DataType::File));
        p.priorities.insert(DataType::File, 255);
        assert_eq!(p.priority(DataType::File), 255);
        assert!(p.auto_download(DataType::Image, 1024));
        assert!(!p.auto_download(DataType::Video, p.auto_download_max_bytes + 1));
        assert!(p.auto_download(DataType::Text, u64::MAX));
        p.types.remove(&DataType::Image);
        assert!(!p.auto_download(DataType::Image, 1));
        // 받기 제한 0: 첨부는 받지 않고 텍스트는 그대로.
        p.download_limit_bps = Some(0);
        assert!(p.downloads_off());
        assert!(!p.auto_download(DataType::File, 1));
        assert!(p.auto_download(DataType::Text, 1));
    }

    #[test]
    fn five_steps_in_order() {
        let f = fx();
        let policy = SeederPolicy::default();
        let mut bucket = TokenBucket::new(policy.upload_limit_bps, 0);
        let ok = req(&f, f.member, Some(f.public_ch));
        assert_eq!(check_seed_request(&policy, &ok, &mut bucket), Ok(()));

        // 1. 멤버가 아니면 다른 조건과 무관하게 NotMember.
        let r = SeedRequest {
            requester: f.outsider,
            channel: Some(f.secret_ch),
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotMember))
        );
        // 멤버라도 폐기된 기기·다른 사람 기기·모르는 기기의 요청은 거절.
        for dev in [REVOKED_DEVICE, DeviceId([1; 32]), DeviceId([77; 32])] {
            let r = SeedRequest {
                requester_device: dev,
                ..ok
            };
            assert_eq!(
                check_seed_request(&policy, &r, &mut bucket),
                Err(SeedRefusal::Denied(WireErrorCode::NotMember)),
                "{dev:?}"
            );
        }

        // 2. 볼 수 없는 채널. 기록 비공개(3)이기도 하지만 2가 먼저.
        let r = SeedRequest {
            channel: Some(f.secret_ch),
            event_lamport: Some(1),
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::Forbidden))
        );
        let r = SeedRequest {
            channel: Some(ChannelId([99; 16])),
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::Forbidden))
        );

        // 3. 입장 이전 기록 + 비공개 채널. 제공 범위 밖(4)이기도 하지만 3이 먼저.
        let r = SeedRequest {
            event_lamport: Some(5),
            channel_opened: false,
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::HistoryPrivate))
        );
        // 과거 기록 공개 채널이면 통과.
        let r = SeedRequest {
            channel: Some(f.history_ch),
            event_lamport: Some(5),
            ..ok
        };
        assert_eq!(check_seed_request(&policy, &r, &mut bucket), Ok(()));
        // 처음 멤버(입장 시점 없음)는 전부 볼 수 있음.
        let r = SeedRequest {
            event_lamport: Some(5),
            requester_since: None,
            ..ok
        };
        assert_eq!(check_seed_request(&policy, &r, &mut bucket), Ok(()));
        // 나중에 들어온 멤버인데 이벤트 시점을 모르면 입장 이전으로 보고 거절(비공개 채널만).
        let r = SeedRequest {
            event_lamport: None,
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::HistoryPrivate))
        );
        let r = SeedRequest {
            event_lamport: None,
            channel: Some(f.history_ch),
            ..ok
        };
        assert_eq!(check_seed_request(&policy, &r, &mut bucket), Ok(()));

        // 4. 제공 범위: 열지 않은 채널, 끈 종류, 제외 메시지, 시더 꺼짐.
        let r = SeedRequest {
            channel_opened: false,
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotSeeded))
        );
        let mut no_video = policy.clone();
        no_video.types.remove(&DataType::Video);
        let r = SeedRequest {
            data_type: DataType::Video,
            ..ok
        };
        assert_eq!(
            check_seed_request(&no_video, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotSeeded))
        );
        let mut excl = policy.clone();
        excl.excluded_events.insert(EventId([7; 32]));
        assert_eq!(
            check_seed_request(&excl, &ok, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotSeeded))
        );
        let off = SeederPolicy {
            enabled: false,
            ..policy.clone()
        };
        assert_eq!(
            check_seed_request(&off, &ok, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotSeeded))
        );

        // 5. 동시 전송 수, 대역폭.
        let r = SeedRequest {
            in_flight: policy.max_concurrent,
            ..ok
        };
        assert_eq!(
            check_seed_request(&policy, &r, &mut bucket),
            Err(SeedRefusal::Busy {
                retry_after_ms: BUSY_RETRY_MS
            })
        );
        let slow = SeederPolicy {
            upload_limit_bps: Some(1000),
            ..policy.clone()
        };
        let mut b = TokenBucket::new(slow.upload_limit_bps, 0);
        let big = SeedRequest {
            bytes: MIN_BURST_BYTES,
            ..ok
        };
        assert_eq!(check_seed_request(&slow, &big, &mut b), Ok(()));
        match check_seed_request(&slow, &ok, &mut b) {
            Err(SeedRefusal::Busy { retry_after_ms }) => assert_eq!(retry_after_ms, 1000),
            other => panic!("대역폭 초과여야 함: {other:?}"),
        }
    }

    #[test]
    fn required_lanes_only_check_membership() {
        let f = fx();
        let off = SeederPolicy {
            enabled: false,
            ..SeederPolicy::default()
        };
        let mut bucket = TokenBucket::new(Some(0), 0);
        let base = req(&f, f.member, None);
        for lane in [Lane::State, Lane::KeyAgreement] {
            let r = SeedRequest {
                lane,
                in_flight: 1000,
                ..base
            };
            assert_eq!(check_seed_request(&off, &r, &mut bucket), Ok(()));
        }
        let r = SeedRequest {
            lane: Lane::Key,
            ..base
        };
        assert_eq!(check_seed_request(&off, &r, &mut bucket), Ok(()));
        let r = SeedRequest {
            lane: Lane::State,
            requester: f.outsider,
            ..base
        };
        assert_eq!(
            check_seed_request(&off, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotMember))
        );
        // 채널 키는 메시지처럼 5단계 전부.
        let r = SeedRequest {
            lane: Lane::Key,
            channel: Some(f.public_ch),
            ..base
        };
        assert_eq!(
            check_seed_request(&off, &r, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::NotSeeded))
        );
        // 채널 없는 메시지 요청은 잘못된 요청.
        let policy = SeederPolicy::default();
        assert_eq!(
            check_seed_request(&policy, &base, &mut bucket),
            Err(SeedRefusal::Denied(WireErrorCode::BadRequest))
        );
        // 공간 이모지·봉인된 과거 기록 묶음(채널 없는 블롭)은 2·3단계를 건너뜀.
        let mut b = TokenBucket::unlimited();
        for t in [DataType::Emoji, DataType::HistoryBundle] {
            let r = SeedRequest {
                data_type: t,
                event: None,
                ..base
            };
            assert_eq!(check_seed_request(&policy, &r, &mut b), Ok(()), "{t}");
        }
        // 채널을 모르는 첨부는 볼 권한을 검사할 수 없어 거절(비공개 채널 첨부 유출 방지).
        for t in [DataType::Image, DataType::Video, DataType::File] {
            let r = SeedRequest {
                data_type: t,
                ..base
            };
            assert_eq!(
                check_seed_request(&policy, &r, &mut b),
                Err(SeedRefusal::Denied(WireErrorCode::BadRequest)),
                "{t}"
            );
        }
        // 필수 레인도 폐기된 기기에는 주지 않음.
        let r = SeedRequest {
            lane: Lane::State,
            requester_device: REVOKED_DEVICE,
            ..base
        };
        assert_eq!(
            check_seed_request(&off, &r, &mut b),
            Err(SeedRefusal::Denied(WireErrorCode::NotMember))
        );
    }

    #[test]
    fn refusal_to_response() {
        assert_eq!(
            SeedRefusal::Busy { retry_after_ms: 7 }.to_response(),
            YpResponse::Busy { retry_after_ms: 7 }
        );
        match SeedRefusal::Denied(WireErrorCode::NotSeeded).to_response() {
            YpResponse::Error { code, .. } => assert_eq!(code, WireErrorCode::NotSeeded),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn token_bucket_rate_and_debt() {
        let mut b = TokenBucket::with_burst(1000, 1000, 0);
        assert_eq!(b.available(0), 1000);
        assert_eq!(b.try_consume(600, 0), Ok(()));
        assert_eq!(b.try_consume(600, 0), Err(200));
        assert_eq!(b.try_consume(600, 200), Ok(()));
        assert_eq!(b.available(200), 0);
        // 버스트보다 큰 요청: 가득 찼을 때 보내고 빚을 짐.
        let mut b = TokenBucket::with_burst(1000, 1000, 0);
        assert_eq!(b.try_consume(5000, 0), Ok(()));
        // 빚 4000 + 다음 1000 → 5초 뒤.
        assert_eq!(b.try_consume(1000, 0), Err(5000));
        assert_eq!(b.try_consume(1000, 5000), Ok(()));
        // 시계가 거꾸로 가도 넘치지 않음.
        assert!(b.try_consume(1000, 10).is_err());
        // 오래 쉬어도 버스트까지만 참.
        assert_eq!(b.available(1_000_000_000), 1000);
    }

    #[test]
    fn token_bucket_configure_and_unlimited() {
        let mut b = TokenBucket::unlimited();
        assert_eq!(b.try_consume(u64::MAX, 0), Ok(()));
        assert_eq!(b.available(0), u64::MAX);
        b.configure(Some(100), 0);
        assert_eq!(b.rate(), Some(100));
        assert_eq!(b.burst(), MIN_BURST_BYTES);
        assert_eq!(b.try_consume(MIN_BURST_BYTES, 0), Ok(()));
        assert!(b.try_consume(100, 0).is_err());
        b.configure(Some(0), 0);
        assert_eq!(b.try_consume(1, 1_000_000), Err(60_000));
        b.configure(None, 5);
        assert_eq!(b.try_consume(1, 5), Ok(()));
    }

    #[test]
    fn default_rate_limits_are_10_mib() {
        const TEN: Option<u64> = Some(10 * 1024 * 1024);
        assert_eq!(SeederPolicy::default().upload_limit_bps, TEN);
        assert_eq!(SeederPolicy::tor_default().upload_limit_bps, TEN);
        assert_eq!(SeederPolicy::tor_default().max_concurrent, 2);
        assert_eq!(PeerPolicy::default().download_limit_bps, TEN);
        assert_eq!(PeerPolicy::tor_default().download_limit_bps, TEN);
        assert_eq!(
            PeerPolicy::tor_default().auto_download_max_bytes,
            2 * 1024 * 1024
        );
    }
}
