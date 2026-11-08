//! `yangpa://` 링크로 앱 열기(웹 초대 페이지의 '앱에서 열기'). OS가 넘긴 링크는 처음 실행이면 인자로, 이미
//! 떠 있으면 두 번째 실행이 single-instance 플러그인으로 넘겨 deep-link 플러그인 이벤트로 옵니다.
//!
//! 아무 웹사이트나 이 링크를 띄울 수 있으므로 여기서는 받아 두기만 합니다. 웹뷰가 친구 추가·크루 참가 창에 채워
//! 보여 주고, 사용자가 누른 뒤에야 데몬에 넘깁니다.

use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager, Runtime, State, Url};
use tracing::debug;

use crate::daemon::MAIN_WINDOW;

/// 받아 둔 링크가 있다고 웹뷰에 알리는 이벤트. 웹뷰는 `deep_link_take`로 가져갑니다(웹뷰가 뜨기 전에 온 링크도 같은 길).
const OPEN_LINK: &str = "yangpa-open-link";
/// 링크 본문 한도 4096바이트의 base58(약 5600자)에 넉넉히.
const MAX_LEN: usize = 8192;

/// 마지막으로 받은 링크. 창은 하나씩 띄우므로 하나만 둡니다.
#[derive(Default)]
pub struct PendingLink(Mutex<Option<String>>);

/// OS가 넘긴 링크를 받아 두고 창을 앞으로 가져옵니다.
pub fn open<R: Runtime>(app: &AppHandle<R>, urls: Vec<Url>) {
    let Some(url) = pick(urls) else {
        return;
    };
    *app.state::<PendingLink>().0.lock() = Some(url);
    crate::notify::show_main(app);
    if let Err(e) = app.emit_to(MAIN_WINDOW, OPEN_LINK, ()) {
        debug!("받은 링크를 앱에 알리지 못했습니다: {e}");
    }
}

fn pick(urls: Vec<Url>) -> Option<String> {
    urls.into_iter()
        .rev()
        .find(|u| u.scheme() == "yangpa" && u.as_str().len() <= MAX_LEN)
        .map(String::from)
}

/// 받아 둔 링크를 가져갑니다(한 번만).
#[tauri::command]
pub fn deep_link_take(pending: State<'_, PendingLink>) -> Option<String> {
    pending.0.lock().take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_short_yangpa_links() {
        let u = |s: &str| Url::parse(s).unwrap();
        let link = "yangpa://invite/3yZe7d";
        assert_eq!(
            pick(vec![u("https://example.com/"), u(link)]).as_deref(),
            Some(link)
        );
        assert_eq!(
            pick(vec![
                u("file:///C:/x"),
                u("https://example.com/#invite/3yZe7d")
            ]),
            None
        );
        let long = format!("yangpa://invite/{}", "1".repeat(MAX_LEN));
        assert_eq!(pick(vec![u(&long)]), None);
    }
}
