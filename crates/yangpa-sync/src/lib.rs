//! 양파 메신저 동기화 계층(순수 로직, 입출력·비동기 없음).
//!
//! - [`dag`]: 스트림별 머리·빠진 부모('받지 않은 기록') 추적, 새 이벤트의 부모·논리 시계 선택.
//! - [`reconcile`]: 논리 시계 구간 지문(개수 + ID XOR) 비교로 빠진 이벤트만 찾는 상태 기계.
//! - [`checkpoint`]: 체크포인트 사슬 검증(초대 링크의 고정부터), `GetDeviceKeys` 답의 키 확인.
//! - [`state`]: 상태·키 갱신 이벤트를 결정론적 순서로 적용하며 권한을 검사하는 상태 해소(크루 키 사슬·체크포인트
//!   포함), 키 그룹 스트림의 키 갱신 사슬([`GroupChain`]), 메시지 권한 검사.
//! - [`policy`]: 시더·피어 설정, 시더 요청 처리 5단계 검사, 대역폭 토큰 버킷.
//! - [`swarm`]: 블롭 하나의 조각 스케줄러(희귀 조각 우선, 피어별 동시 요청 상한, 시간 초과·실패 피어 제외).
//! - [`priority`]: 요청 순서 큐(상태 → 보고 있는 채널 최근 → 우선순위별 첨부 → 과거 기록).
//! - [`score`]: 피어 점수(위반 시 감점, 시간에 따라 회복, 임계 이하면 연결 해제).
//! - [`archive`]: 보관 전용 노드의 보관 범위, 서명된 멤버 명단 만들기·받기, 양쪽의 요청 검사.
//!
//! 시간이 필요한 곳은 모두 호출자가 밀리초 시각을 넘깁니다(내부에서 시스템 시계를 읽지 않음).
//! 그래서 같은 입력이면 어느 노드에서나 같은 결과가 나오고 테스트가 결정론적입니다.

pub mod archive;
pub mod checkpoint;
pub mod dag;
pub mod policy;
pub mod priority;
pub mod reconcile;
pub mod score;
pub mod state;
pub mod swarm;

pub use state::{
    AnchorPlace, AnchorState, AuthError, ChainBase, ChainChange, CheckpointBase,
    CheckpointConflict, CheckpointDraft, CheckpointError, CrewKeys, DecodedEvent, Equivocation,
    GroupChain, GroupTip, KeyPoint, KeyPoints, KeyView, LinkEvent, ResolveUpdate, Resolved,
    Resolver, UpdateKind, authorize_message, authorize_revoked_device_message, authorize_scoped,
    resolve, resolve_with,
};
