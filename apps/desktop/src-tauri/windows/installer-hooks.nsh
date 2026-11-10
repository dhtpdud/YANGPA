; 양파 메신저(가칭) NSIS 설치 파일 훅. tauri.conf.json의 bundle.windows.nsis.installerHooks가 가리킵니다.
;
; 앱이 띄운 데몬(yangpad.exe, 사이드카)은 앱 창을 닫아도 시딩을 위해 계속 돕니다. Windows는 실행 중인 실행 파일을
; 덮어쓰거나 지울 수 없는데 Tauri는 주 실행 파일(yangpa-desktop.exe)만 확인하므로, 그대로 두면 업데이트는
; "Error opening file for writing"에서 멈추고(조용한 설치 /S는 실패) 제거는 데몬을 남깁니다.
; 그래서 파일을 바꾸거나 지우기 전에 설치 경로의 yangpad.exe를 쓰는 프로세스만 Restart Manager로 끝냅니다.
; 다른 경로에서 띄운 데몬(CLI 묶음)은 건드리지 않습니다. 창이 없는 프로세스라 강제 종료가 되며, 휘발성 채널
; 내용은 RAM에만 있어 함께 사라지고 DB는 SQLite 저널로 일관성을 지킵니다.
;
; 이 파일은 Tauri 템플릿이 utils.nsh·Win\RestartManager.nsh(LogicLib 포함) 뒤에 !include 합니다.
; 한국어 문자열 때문에 UTF-8 BOM으로 저장합니다(BOM이 없으면 makensis가 시스템 코드 페이지로 읽음).

!macro YANGPA_STOP_DAEMON
  ${If} ${FileExists} "$INSTDIR\yangpad.exe"
    Push $0
    DetailPrint "실행 중인 YANGPA 데몬(yangpad.exe)을 끕니다"
    !insertmacro RestartManager_ShutdownFile "$INSTDIR\yangpad.exe" $0
    ${If} $0 <> 0
      ; 끄지 못해도 계속합니다. 파일을 바꿀 수 없으면 NSIS가 다시 시도·무시를 묻습니다.
      DetailPrint "데몬을 끄지 못했습니다(Restart Manager 오류 $0). 작업 관리자에서 yangpad.exe를 끝낸 뒤 다시 시도하세요"
    ${EndIf}
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro YANGPA_STOP_DAEMON
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro YANGPA_STOP_DAEMON
!macroend

; 앱이 쓴 '윈도 시작 시 켜기' 항목(platform.rs set_auto_start)은 제거 프로그램이 모르므로 직접 지웁니다.
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "YANGPA"
  ; '앱 데이터 삭제'를 고르면 Tauri는 %APPDATA%·%LOCALAPPDATA%의 app.yangpa.desktop만 지우는데, 양파 데이터는 설치 폴더의
  ; Data(기본)나 %APPDATA%\yangpa(예전 판, 데이터 위치 설정)에 있습니다. 업데이트(/UPDATE)도 이 제거를 거치므로 두 조건을 꼭 봅니다.
  ; 설치 폴더 통째(바탕화면 같은 곳일 수 있음)와 사용자가 고른 데이터 폴더(다른 파일이 섞였을 수 있음)는 지우지 않습니다.
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    RMDir /r "$INSTDIR\Data"
    Delete "$INSTDIR\data-dir.json"
    RMDir "$INSTDIR"
    RMDir /r "$APPDATA\yangpa"
  ${EndIf}
!macroend
