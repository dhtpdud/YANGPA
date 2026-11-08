// 릴리스 빌드에서 Windows 콘솔 창을 띄우지 않습니다.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    yangpa_desktop_lib::run();
}
