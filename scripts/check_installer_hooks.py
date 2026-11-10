#!/usr/bin/env python3
"""설치 파일 훅 연결 검사(표준 라이브러리만, 네트워크 없음).

tauri.conf.json이 가리키는 설치 훅이 실제로 있고 서로 맞는지 봅니다. 스키마(XSD) 검사는 릴리스 meta 작업이 xmllint로,
실제 설치·업데이트·제거는 릴리스 빌드 작업이 합니다. 이 검사는 PR마다 도는 CI에서 연결이 끊긴 것을 먼저 잡습니다.

- WiX 조각(`bundle.windows.wix.fragmentPaths`): 파일이 있고 XML로 읽히며, `componentGroupRefs`의 이름이 조각에 정의돼 있고,
  `Custom`이 가리키는 동작과 `Before`/`After` 대상이 조각이나 Windows Installer 표준 동작에 있음. Tauri가 조각을
  handlebars로 한 번 읽으므로 이중 중괄호가 없어야 하고, MSI Formatted 값(SetProperty·CustomAction)의 대괄호는
  속성 참조(대문자 이름·`...Folder`·`[\\x]` 이스케이프)만, 중괄호는 `[\\{]`·`[\\}]`로만 써야 합니다(PowerShell의
  `[IO.Path]` 같은 것이 빈 속성으로 바뀌는 실수 방지). QuietExec 명령은 끈 PID를 `YangpaStopDaemon stopped pid=`로 찍고
  `exit 0`으로 끝나야 하며, 같은 표시를 Windows 설치 시험(`scripts/installer-smoke.ps1`)이 찾습니다(데몬을 끈 것이 이
  동작인지 확인).
- NSIS 훅(`bundle.windows.nsis.installerHooks`): 파일이 있고 UTF-8 BOM으로 시작(한국어 문자열). 폴더를 통째로 지우면
  (`RMDir /r`) '앱 데이터 삭제' 체크와 업데이트 아님 조건을 함께 봄.
- deb 유지 스크립트(`bundle.linux.deb.*Script`): 파일이 있고 `#!/bin/sh`로 시작하며 `sh -n`을 통과(권한은 묶을 때
  Tauri가 0755로 씀).

사용: python3 -I scripts/check_installer_hooks.py [--config apps/desktop/src-tauri/tauri.conf.json]
      python3 -I scripts/check_installer_hooks.py selftest
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

WIX_NS = "{http://schemas.microsoft.com/wix/2006/wi}"

# MSI 조각의 PowerShell이 끈 프로세스마다 찍는 표시. installer-smoke.ps1이 MSI 로그에서 이 표시와 데몬 PID를 찾습니다.
STOP_MARKER = "YangpaStopDaemon stopped pid="
SMOKE_PS1 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "installer-smoke.ps1")

# InstallExecuteSequence·InstallUISequence의 표준 동작(Before/After 대상으로 쓸 만한 것).
STANDARD_ACTIONS = {
    "AppSearch", "LaunchConditions", "ValidateProductID", "CostInitialize", "FileCost", "CostFinalize",
    "InstallValidate", "InstallInitialize", "ProcessComponents", "UnpublishFeatures", "RemoveShortcuts",
    "RemoveFiles", "RemoveFolders", "CreateFolders", "MoveFiles", "InstallFiles", "PatchFiles", "DuplicateFiles",
    "WriteRegistryValues", "RemoveRegistryValues", "WriteIniValues", "RemoveIniValues", "CreateShortcuts",
    "RegisterUser", "RegisterProduct", "PublishFeatures", "PublishProduct", "InstallFinalize",
    "RemoveExistingProducts", "FindRelatedProducts", "MigrateFeatureStates", "InstallExecute",
    "InstallExecuteAgain", "ExecuteAction", "StopServices", "DeleteServices", "StartServices", "InstallServices",
    "ScheduleReboot", "ForceReboot", "SelfRegModules", "SelfUnregModules", "RegisterTypeLibraries",
    "UnregisterTypeLibraries", "WriteEnvironmentStrings", "RemoveEnvironmentStrings",
}

# MSI Formatted 문자열의 대괄호 하나: 이스케이프 [\x], 파일·구성 요소·환경 변수 참조, 또는 속성 이름.
BRACKET = re.compile(r"\[(\\.|[#!$%][A-Za-z_][A-Za-z0-9_.]*|[A-Za-z_][A-Za-z0-9_.]*)\]")


def formatted_errors(value):
    """MSI Formatted 값에서 의도하지 않은 대괄호·중괄호를 찾습니다."""
    errs = []
    for m in BRACKET.finditer(value):
        inner = m.group(1)
        if inner.startswith("\\") or inner[0] in "#!$%":
            continue
        # 공개 속성(대문자)이나 표준 폴더 속성만 속성 참조로 봅니다.
        if not (inner.isupper() or inner.endswith("Folder")):
            errs.append(f"속성 참조로 읽힐 대괄호: [{inner}] (리터럴이면 [\\[]·[\\]]로)")
    rest = BRACKET.sub("", value)
    for ch in "[]{}":
        if ch in rest:
            errs.append(f"이스케이프하지 않은 '{ch}' (MSI Formatted에서는 [\\{ch}]로)")
    return errs


def check_wix(base, wix, errors, smoke_ps1=SMOKE_PS1):
    frags = wix.get("fragmentPaths") or []
    if not frags:
        errors.append("bundle.windows.wix.fragmentPaths가 비어 있습니다(MSI가 데몬을 끄지 않음)")
        return
    defined_groups = set()
    for rel in frags:
        path = os.path.join(base, rel)
        if not os.path.isfile(path):
            errors.append(f"WiX 조각이 없습니다: {rel}")
            continue
        text = open(path, encoding="utf-8").read()
        if "{{" in text or "}}" in text:
            errors.append(f"{rel}: 이중 중괄호가 있습니다(Tauri가 handlebars로 읽음)")
        try:
            root = ET.fromstring(text.encode("utf-8"))
        except ET.ParseError as e:
            errors.append(f"{rel}: XML이 아닙니다: {e}")
            continue
        if root.tag != WIX_NS + "Wix":
            errors.append(f"{rel}: 최상위가 WiX v3 <Wix>가 아닙니다({root.tag})")
            continue
        actions = set()
        for el in root.iter(WIX_NS + "CustomAction"):
            actions.add(el.get("Id"))
        for el in root.iter(WIX_NS + "SetProperty"):
            actions.add(el.get("Action") or ("Set" + el.get("Id", "")))
        for el in root.iter(WIX_NS + "ComponentGroup"):
            defined_groups.add(el.get("Id"))
        for el in list(root.iter(WIX_NS + "SetProperty")) + list(root.iter(WIX_NS + "CustomAction")):
            for attr in ("Value", "ExeCommand"):
                v = el.get(attr)
                if v is None:
                    continue
                for e in formatted_errors(v):
                    errors.append(f"{rel}: {el.tag.replace(WIX_NS, '')} {el.get('Id')} {attr}: {e}")
        # QuietExec 명령줄은 SetProperty나 속성 설정 동작(<CustomAction Property=… Value=…>)으로 넣습니다.
        cmdlines = [el for el in root.iter(WIX_NS + "SetProperty") if (el.get("Id") or "").startswith("WixQuietExec")]
        cmdlines += [el for el in root.iter(WIX_NS + "CustomAction") if (el.get("Property") or "").startswith("WixQuietExec")]
        for el in cmdlines:
            v = el.get("Value") or ""
            if STOP_MARKER not in v:
                errors.append(f"{rel}: {el.get('Id')}가 끈 PID를 '{STOP_MARKER}'로 찍지 않습니다(설치 시험이 확인할 수 없음)")
            if not v.rstrip().rstrip('"').rstrip().endswith("exit 0"):
                errors.append(f"{rel}: {el.get('Id')}의 PowerShell이 'exit 0'으로 끝나지 않습니다(끌 것이 없을 때도 오류로 남음)")
        known = actions | STANDARD_ACTIONS
        for el in root.iter(WIX_NS + "Custom"):
            a = el.get("Action")
            if a not in actions:
                errors.append(f"{rel}: <Custom Action=\"{a}\">가 이 조각에 정의되지 않았습니다")
            for attr in ("Before", "After"):
                t = el.get(attr)
                if t is not None and t not in known:
                    errors.append(f"{rel}: <Custom Action=\"{a}\" {attr}=\"{t}\">의 대상을 모릅니다")
        for el in root.iter(WIX_NS + "SetProperty"):
            for attr in ("Before", "After"):
                t = el.get(attr)
                if t is not None and t not in known:
                    errors.append(f"{rel}: <SetProperty Id=\"{el.get('Id')}\" {attr}=\"{t}\">의 대상을 모릅니다")
    try:
        with open(smoke_ps1, encoding="utf-8") as f:
            if STOP_MARKER not in f.read():
                errors.append(f"{os.path.basename(smoke_ps1)}가 MSI 로그에서 '{STOP_MARKER}'를 찾지 않습니다")
    except OSError:
        errors.append(f"Windows 설치 시험이 없습니다: {smoke_ps1}")
    refs = wix.get("componentGroupRefs") or []
    if not refs:
        errors.append("bundle.windows.wix.componentGroupRefs가 비어 있어 조각이 MSI에 들어가지 않습니다")
    for r in refs:
        if r not in defined_groups:
            errors.append(f"componentGroupRefs의 {r}가 어느 조각에도 없습니다")


def check_nsis(base, nsis, errors):
    rel = nsis.get("installerHooks")
    if not rel:
        errors.append("bundle.windows.nsis.installerHooks가 없습니다")
        return
    path = os.path.join(base, rel)
    if not os.path.isfile(path):
        errors.append(f"NSIS 훅이 없습니다: {rel}")
        return
    with open(path, "rb") as f:
        raw = f.read()
    if not raw.startswith(b"\xef\xbb\xbf"):
        errors.append(f"{rel}: UTF-8 BOM으로 시작해야 합니다(없으면 makensis가 시스템 코드 페이지로 읽음)")
    errors.extend(f"{rel}: {e}" for e in nsis_delete_errors(raw.decode("utf-8", "replace")))


def nsis_delete_errors(text):
    """폴더를 통째로 지우는 훅은 '앱 데이터 삭제' 체크와 업데이트 아님(/UPDATE도 제거를 거침)을 함께 봐야 합니다."""
    if "rmdir /r" not in text.lower():
        return []
    return [
        f"RMDir /r이 있는데 '{c}' 조건이 없습니다(업데이트·체크 안 한 제거에서 데이터가 지워짐)"
        for c in ("$DeleteAppDataCheckboxState = 1", "$UpdateMode <> 1")
        if c not in text
    ]


def check_deb(base, deb, errors):
    rels = []
    for key in ("postInstallScript", "postRemoveScript"):
        rel = deb.get(key)
        if not rel:
            errors.append(f"bundle.linux.deb.{key}가 없습니다(업데이트·제거 뒤 옛 데몬이 남음)")
        elif rel not in rels:
            rels.append(rel)
    for rel in rels:
        path = os.path.join(base, rel)
        if not os.path.isfile(path):
            errors.append(f"deb 스크립트가 없습니다: {rel}")
            continue
        with open(path, "rb") as f:
            head = f.readline()
        if head.strip() != b"#!/bin/sh":
            errors.append(f"{rel}: 첫 줄이 #!/bin/sh가 아닙니다(dpkg는 POSIX sh로 실행)")
        r = subprocess.run(["sh", "-n", path], capture_output=True, text=True)
        if r.returncode != 0:
            errors.append(f"{rel}: sh -n 실패: {r.stderr.strip()}")


def run(config_path):
    with open(config_path, encoding="utf-8") as f:
        conf = json.load(f)
    base = os.path.dirname(os.path.abspath(config_path))
    bundle = conf.get("bundle") or {}
    errors = []
    win = bundle.get("windows") or {}
    check_wix(base, win.get("wix") or {}, errors)
    check_nsis(base, win.get("nsis") or {}, errors)
    check_deb(base, (bundle.get("linux") or {}).get("deb") or {}, errors)
    return errors


def selftest():
    good = '"[System64Folder]x.exe" -Command "$t = \'[INSTALLDIR]a.exe\'; Where-Object [\\{] $_.Path -eq $t [\\}]"'
    assert formatted_errors(good) == [], formatted_errors(good)
    assert formatted_errors("[#Path] [!Path] [%TEMP] [\\[]x[\\]]") == []
    assert any("IO.Path" in e for e in formatted_errors("[IO.Path]::Combine(1)"))
    assert any("'{'" in e for e in formatted_errors("ForEach-Object { 1 }"))
    assert any("']'" in e for e in formatted_errors("a ] b"))
    assert nsis_delete_errors('DeleteRegValue HKCU "x" "y"') == []
    guarded = '${If} $DeleteAppDataCheckboxState = 1\n${AndIf} $UpdateMode <> 1\nRMDir /r "$INSTDIR\\Data"'
    assert nsis_delete_errors(guarded) == []
    assert len(nsis_delete_errors('RMDir /r "$INSTDIR\\Data"')) == 2
    with tempfile.TemporaryDirectory() as d:
        wix = {"fragmentPaths": ["f.wxs"], "componentGroupRefs": ["G"]}
        smoke = os.path.join(d, "smoke.ps1")
        with open(smoke, "w", encoding="utf-8") as f:
            f.write(f"Select-String -Pattern '{STOP_MARKER}'")

        def errs_for(cmd):
            with open(os.path.join(d, "f.wxs"), "w", encoding="utf-8") as f:
                f.write(
                    '<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi"><Fragment><ComponentGroup Id="G" />'
                    f'<SetProperty Id="WixQuietExec64CmdLine" Before="InstallValidate" Value="{cmd}" />'
                    "</Fragment></Wix>"
                )
            out = []
            check_wix(d, wix, out, smoke)
            return out

        ok = f"x.exe -Command &quot;'{STOP_MARKER}' + 1; exit 0&quot;"
        assert errs_for(ok) == [], errs_for(ok)
        assert any("찍지 않습니다" in e for e in errs_for("x.exe -Command &quot;1; exit 0&quot;"))
        assert any("exit 0" in e for e in errs_for(f"x.exe -Command &quot;'{STOP_MARKER}' + 1&quot;"))
        os.remove(smoke)
        assert any("설치 시험이 없습니다" in e for e in errs_for(ok))
    print("selftest 통과")


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "selftest":
        selftest()
        return 0
    ap = argparse.ArgumentParser(description="설치 파일 훅 연결 검사")
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--config", default=os.path.join(here, "..", "apps", "desktop", "src-tauri", "tauri.conf.json"))
    args = ap.parse_args()
    errors = run(args.config)
    for e in errors:
        print(f"::error::{e}")
    if errors:
        return 1
    print("설치 파일 훅 연결 확인: WiX 조각, NSIS 훅, deb 유지 스크립트")
    return 0


if __name__ == "__main__":
    sys.exit(main())
