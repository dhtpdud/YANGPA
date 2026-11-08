//! 통화 SFU(sans-IO).
//!
//! 참가자가 많은 통화에서 한 노드(호스트)가 참가자마다 PeerConnection 하나씩을 받아 오디오·카메라·화면공유
//! 프레임을 다른 참가자에게 그대로 넘깁니다. 프레임은 보내는 앱이 이미 E2EE로 암호화했고 키는 호스트에게 가지
//! 않으므로, 호스트는 메시 참가자가 아는 것(누가 어떤 트랙을 얼마나 보내는지, 상대 주소) 이상을 알 수 없습니다.
//! 소켓·시계·시그널 경로는 데몬(`yangpa-daemon`의 `sfu`)이 맡습니다.

mod engine;
pub mod layer;
pub mod stun;
pub mod testkit;

pub use engine::{Output, RoomId, SessionInfo, Sfu, SfuConfig, SfuError, SfuEvent};
pub use str0m::Candidate;
