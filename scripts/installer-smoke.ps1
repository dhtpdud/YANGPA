# 양파 메신저(가칭) Windows 설치 파일 연기 시험(릴리스 작업이 관리자 권한 러너에서 부름). 리눅스·macOS는 installer-smoke.sh.
#
# 설치 경로의 데몬(yangpad.exe, 사이드카)이 도는 채로 업데이트·제거해도 설치 파일이 데몬을 먼저 끄는지 봅니다
# (MSI: windows/stop-daemon.wxs, NSIS: windows/installer-hooks.nsh). 끄지 못하면 msiexec는 3010(재부팅 필요)으로
# 끝나거나 파일이 남으므로, 종료 코드 0·데몬 종료·파일 정리를 모두 확인합니다. MSI는 데몬을 끈 것이 우리 동작인지도
# 봅니다: 동작이 끈 PID를 `YangpaStopDaemon stopped pid=<PID>`로 찍어 MSI 로그에 남기므로 그 PID 줄이 있어야 하고,
# 동작이 오류로 끝나 Return="ignore"로 넘어간 흔적("returned actual error code")이 없어야 합니다. 그래야 동작이 깨져도
# Restart Manager 등 다른 길이 데몬을 끄는 바람에 시험이 통과하는 일이 없습니다.
#
#   MSI:  예전 묶음 설치 → 데몬 → 새 묶음 설치(같은 UpgradeCode, 새 ProductCode: 메이저 업그레이드) → 데몬 → 제거
#   NSIS: 설치 → 데몬 → 같은 설치 파일로 다시 설치 → 데몬 → 제거(uninstall.exe, 제자리 실행)
#
# 데몬은 임시 데이터 경로·전용 named pipe·127.0.0.1·mDNS 끔으로 띄우고 로컬 API가 답할 때까지 기다립니다.

param(
  [Parameter(Mandatory = $true)] [string] $Cli,
  [string] $UpgradeFromMsi = '',
  [string] $Msi = '',
  [string] $Nsis = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$work = Join-Path ([IO.Path]::GetTempPath()) ("yangpa-installer-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null
$script:daemon = $null
$script:daemonDir = ''

function Write-Step([string] $msg) { Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $msg) }

function Start-Daemon([string] $exe, [string] $name) {
  if (-not (Test-Path $exe)) { throw "${name}: $exe 이 없습니다" }
  $dir = Join-Path $work $name
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $pipe = "\\.\pipe\yangpa-installer-$name-$PID"
  $argv = @('--data-dir', "`"$dir`"", '--endpoint', $pipe, '--key-storage', 'file', '--no-mdns',
    '--direct', '--listen', '/ip4/127.0.0.1/tcp/0', '--log-file', "`"$dir\yangpad.log`"", '--log', 'info')
  $p = Start-Process -FilePath $exe -ArgumentList $argv -PassThru -WindowStyle Hidden
  $deadline = (Get-Date).AddSeconds(120)
  while ($true) {
    if ($p.HasExited) { throw "${name}: 데몬이 바로 끝났습니다(코드 $($p.ExitCode))" }
    $out = & $Cli --endpoint $pipe --json --timeout 5s version 2>$null
    if ($LASTEXITCODE -eq 0 -and (($out | Out-String) -match '"protocol"')) { break }
    if ((Get-Date) -gt $deadline) { throw "${name}: 데몬이 120초 안에 준비되지 않았습니다" }
    Start-Sleep -Milliseconds 500
  }
  $script:daemon = $p
  $script:daemonDir = $dir
  Write-Step "${name}: 데몬 준비(PID $($p.Id), $exe)"
}

# MSI 로그로 데몬을 끈 것이 stop-daemon.wxs의 동작인지 확인합니다($PID는 PowerShell 자동 변수라 이름을 달리 씀).
function Assert-StoppedByAction([string] $log, [int] $daemonPid, [string] $what) {
  $evidence = Select-String -Path $log -Pattern 'YangpaStopDaemon|WixQuietExec'
  $failed = @($evidence | Where-Object { $_.Line -match 'CustomAction YangpaStopDaemon\w* returned actual error code' })
  $stopped = @($evidence | Where-Object { $_.Line -match "YangpaStopDaemon stopped pid=$daemonPid(\D|$)" })
  if ($failed.Count -gt 0 -or $stopped.Count -eq 0) {
    $evidence | Select-Object -First 40 | ForEach-Object { Write-Host $_.Line }
  }
  if ($failed.Count -gt 0) {
    throw "${what}: 데몬 끄기 동작이 오류로 끝났습니다(Return=ignore로 넘어감, PowerShell 문법·실행 실패): $($failed[0].Line)"
  }
  if ($stopped.Count -eq 0) {
    throw "${what}: MSI 로그에 데몬 끄기 동작이 PID $daemonPid 를 끈 줄이 없습니다(다른 길로 꺼졌거나 동작이 아무것도 끄지 않음)"
  }
  Write-Step "${what}: 데몬 끄기 동작이 PID $daemonPid 를 끔"
}

function Assert-DaemonStopped([string] $what) {
  if (-not $script:daemon.WaitForExit(30000)) {
    throw "${what}: 설치 파일이 데몬(PID $($script:daemon.Id))을 끄지 않았습니다"
  }
  $script:daemon = $null
  Write-Step "${what}: 데몬이 끝남"
}

function Invoke-Msi([string[]] $argv, [string] $log) {
  $all = $argv + @('/qn', '/norestart', '/l*v', "`"$log`"")
  $p = Start-Process -FilePath msiexec.exe -ArgumentList $all -Wait -PassThru
  return $p.ExitCode
}

function Assert-Code([int] $code, [string] $what, [string] $log) {
  if ($code -ne 0) {
    if ($log -and (Test-Path $log)) { Get-Content $log -Tail 60 | Write-Host }
    # 3010: 성공했지만 재부팅 필요(사용 중인 파일을 바꾸지 못함)
    throw "${what}: 종료 코드 $code"
  }
}

try {
  if ($Msi) {
    $installed = Join-Path $env:ProgramFiles 'YANGPA\yangpad.exe'
    $first = if ($UpgradeFromMsi) { $UpgradeFromMsi } else { $Msi }

    Write-Step "MSI: 설치 $first"
    $log = Join-Path $work 'msi-install.log'
    Assert-Code (Invoke-Msi @('/i', "`"$first`"") $log) 'MSI 설치' $log

    Start-Daemon $installed 'msi-upgrade'
    $daemonPid = $script:daemon.Id
    Write-Step "MSI: 업데이트 $Msi"
    $log = Join-Path $work 'msi-upgrade.log'
    Assert-Code (Invoke-Msi @('/i', "`"$Msi`"") $log) 'MSI 업데이트' $log
    Assert-DaemonStopped 'MSI 업데이트'
    Assert-StoppedByAction $log $daemonPid 'MSI 업데이트'

    Start-Daemon $installed 'msi-remove'
    $daemonPid = $script:daemon.Id
    Write-Step 'MSI: 제거'
    $log = Join-Path $work 'msi-remove.log'
    Assert-Code (Invoke-Msi @('/x', "`"$Msi`"") $log) 'MSI 제거' $log
    Assert-DaemonStopped 'MSI 제거'
    Assert-StoppedByAction $log $daemonPid 'MSI 제거'
    if (Test-Path $installed) { throw "MSI 제거: $installed 이 남았습니다" }
  }

  if ($Nsis) {
    $dir = Join-Path $env:LOCALAPPDATA 'YANGPA'
    $installed = Join-Path $dir 'yangpad.exe'

    Write-Step "NSIS: 설치 $Nsis"
    $p = Start-Process -FilePath $Nsis -ArgumentList '/S' -Wait -PassThru
    Assert-Code $p.ExitCode 'NSIS 설치' ''

    Start-Daemon $installed 'nsis-upgrade'
    Write-Step 'NSIS: 같은 설치 파일로 다시 설치'
    $p = Start-Process -FilePath $Nsis -ArgumentList '/S' -Wait -PassThru
    Assert-Code $p.ExitCode 'NSIS 다시 설치' ''
    Assert-DaemonStopped 'NSIS 다시 설치'

    Start-Daemon $installed 'nsis-remove'
    Write-Step 'NSIS: 제거'
    # 제자리 실행(_?=)이어야 끝날 때까지 기다릴 수 있습니다(아니면 임시 폴더로 복사해 다시 띄우고 바로 돌아옴).
    $p = Start-Process -FilePath (Join-Path $dir 'uninstall.exe') -ArgumentList @('/S', "_?=$dir") -Wait -PassThru
    Assert-Code $p.ExitCode 'NSIS 제거' ''
    Assert-DaemonStopped 'NSIS 제거'
    if (Test-Path $installed) { throw "NSIS 제거: $installed 이 남았습니다" }
  }
  Write-Step '설치 파일 시험 통과'
}
finally {
  if ($script:daemon -and -not $script:daemon.HasExited) { Stop-Process -Id $script:daemon.Id -Force }
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
