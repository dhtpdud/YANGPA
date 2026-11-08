# 데몬 사이드카

릴리스 파이프라인이 이 폴더에 `yangpad-<타깃 트리플>`(윈도는 `.exe`)을 넣습니다. 예: `yangpad-x86_64-unknown-linux-gnu`.
`tauri.conf.json`의 `bundle.externalBin`(`binaries/yangpad`)이 이 파일을 앱 묶음에 넣고, 설치본에서는 앱 실행 파일 옆에
`yangpad`로 놓입니다.

개발 중에는 비워 둡니다. 파일이 없으면 `build.rs`가 externalBin을 빼고 빌드하고, 앱은 실행 파일 옆 → 워크스페이스
`target/{debug,release}` → `PATH` 순서로 `yangpad`를 찾습니다(`YANGPA_DAEMON_BIN`으로 직접 지정 가능).
이 폴더의 다른 파일은 git에 올리지 않습니다(`../.gitignore`). 폴더를 지우지 마세요: `build.rs`가 이 폴더의 변화를
지켜봅니다.
