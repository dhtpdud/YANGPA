//! 알림 내보내기. 로컬 API 구독자(`EventHub`, 프레임 단위 묶음은 rpc 서버가 담당)와
//! 프로세스 안 구독자(테스트·다른 모듈)에게 같은 알림을 보냅니다.

use tokio::sync::broadcast;
use yangpa_rpc::EventHub;
use yangpa_rpc::api::Notification;

/// 프로세스 안 구독자 큐 길이. 느린 구독자는 알림을 잃을 뿐 데몬을 막지 않습니다.
const LOCAL_CAPACITY: usize = 4096;

/// 내보내기 전에 거르고 고치는 함수(`false`면 버림). 모더레이션이 시작할 때 한 번 정합니다(차단한 사용자의 새 메시지를
/// 버리고, 메시지 보기의 `author_blocked`를 채움).
pub type NotifyFilter = Box<dyn Fn(&mut Notification) -> bool + Send + Sync>;

pub struct Notifier {
    hub: EventHub,
    local: broadcast::Sender<Notification>,
    filter: std::sync::OnceLock<NotifyFilter>,
}

impl Notifier {
    pub fn new(hub: EventHub) -> Self {
        let (local, _) = broadcast::channel(LOCAL_CAPACITY);
        Notifier {
            hub,
            local,
            filter: std::sync::OnceLock::new(),
        }
    }

    pub fn set_filter(&self, f: NotifyFilter) {
        let _ = self.filter.set(f);
    }

    pub fn hub(&self) -> &EventHub {
        &self.hub
    }

    /// 알림 하나를 내보냅니다. 내용 있는 알림이라도 로그에는 종류만 남깁니다.
    pub fn publish(&self, mut n: Notification) {
        if self.filter.get().is_some_and(|f| !f(&mut n)) {
            return;
        }
        // 통화 소리는 말할 때마다 나와, 종류만 적어도 말한 때가 로그에 남습니다.
        if !matches!(n, Notification::VoiceMedia { .. }) {
            tracing::trace!(kind = n.kind(), "알림");
        }
        let _ = self.local.send(n.clone());
        self.hub.publish(n);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.local.subscribe()
    }
}
