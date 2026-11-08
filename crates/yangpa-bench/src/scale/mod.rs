//! 규모 시뮬레이션(`yangpa-bench scale`, docs/performance.md "규모 시뮬레이션").
//!
//! 네트워크 없이 한 프로세스에서 큰 크루를 재고, 아직 만들지 않은 구조를 모형으로 미리 따져 봅니다.
//!
//! - [`state`]: 실제 키·서명으로 만든 가입 기록을 해소기에 넣어 이벤트 크기·해소 시간·메모리를 잽니다. 동시 가입
//!   (같은 부모), 늦게 온 갈래, 갈라졌다 합치기 시나리오도 있습니다.
//! - [`trace`]: 시드 하나로 정해지는 멤버 변동 기록(가입·탈퇴·기기·권한·확정 기기 사건·갈라짐·밴 물결).
//! - [`keys`]: 키 관리 방식 세 가지(바꾸기 전 MLS, 원안 하위 그룹, 수정판 키 트리)를 모형으로 기록에 돌려
//!   이벤트·바이트·봉인 수를 세고, 누가 어떤 키를 얻을 수 있는지 기호로 따라가 불변식(I1~I4)을 검사합니다.
//! - [`keyreal`]: 실제 키 트리([`yangpa_crypto::keytree`])를 큰 크루에서 돌려 [`keys`]의 키 트리 모형과 나란히 놓습니다.
//! - [`keydaemon`]: 데몬 노드(확정 기기·실제 멤버·실제 가입자, 로컬 TCP)를 띄워 데몬 쪽 키 그룹 경로를 그대로 잽니다.
//! - [`catchup`]: 오래 꺼져 있던 기기가 지문 비교([`yangpa_sync::reconcile`])로 따라잡는 데 드는 요청·바이트.
//! - [`gossip`]: 연결 수·구독자 밀도에 따라 gossipsub 메시가 생기는지와 전달 지연을 어림하는 그래프 모형.
//! - [`store`]: 큰 크루 상태(간추린 기기 기록)를 행으로 저장하고 바뀐 행만 쓰는 비용, 상태 색인, 처음 읽기·상주 메모리.
//! - [`inbox`]: 가입 키 묶음을 공간 스트림에서 가입자 받은편지함으로 옮겨 다른 멤버가 덜 받는 양.
//! - [`checkpoint`]: 상태 체크포인트로 새 멤버가 받는 양(스냅숏 + 꼬리), 차가운 시작 시간·메모리, 해소기 항목 수.

pub mod catchup;
pub mod checkpoint;
pub mod gossip;
pub mod inbox;
pub mod keydaemon;
pub mod keyreal;
pub mod keys;
pub mod state;
pub mod store;
pub mod trace;

pub use state::{
    MessageReport, ScenarioReport, StaleChainReport, StateReport, concurrent_branches,
    fanout_joins, message, partition_merge, stale_chain, state,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 작은 규모로 끝까지 돌고, 가입이 모두 적용되며 숫자가 말이 되는지.
    #[test]
    fn small_crew_and_group_run() {
        let s = state(12).unwrap();
        assert_eq!(s.applied, 12, "{s:?}");
        assert_eq!(s.rejected, 0, "{s:?}");
        // 혼합 서명 셋(이벤트·기기 목록·초대 증명, 각 1,659B)과 상자 공개키(1,216B)가 든 기록입니다(형식 3판에서
        // KeyPackage가 빠져 약 9.8KB).
        assert!(s.join_wire > 8_000, "가입 기록이 너무 작음: {s:?}");
        assert!(s.stream_wire > 12 * s.join_wire as u64);

        let msg = message().unwrap();
        eprintln!("짧은 글 {}B, 서명 {}B", msg.short_text, msg.sig);
        // 채널 이벤트는 짧은 서명(730B, 공개키 없음)이라 짧은 글 하나가 1.1KB 안쪽입니다(1,031B).
        assert_eq!(msg.sig, yangpa_core::sign::Sig::SLIM_LEN);
        assert!(msg.short_text > msg.sig);
        assert!(msg.short_text <= 1_100, "짧은 글이 너무 큼: {msg:?}");
    }
}
