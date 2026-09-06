# 릴리스 절차

공식 배포는 현재 **v0.4.0**이며 소스 **0.4.1은 개선 후보**입니다. 이 문서는 자동화의 조건과 운영 절차를 설명합니다. 새 CI나 0.4.1 릴리스의 실행·검증·게시 완료를 주장하지 않습니다.

[`release.yml`](../.github/workflows/release.yml)은 **main에서 수동 실행**합니다. `publish`의 기본값은 **false**이고 태그 push나 일반 PR로 공개하지 않습니다. 기존 태그를 덮어쓰지 않습니다.

## 1. 후보 만들기

PR의 필수 검사를 통과한 변경을 main에 병합하고 Cargo·EXE 리소스·manifest 버전과 [`release-notes.md`](release-notes.md), 라이선스를 함께 확인합니다. 배포 버전의 태그가 이미 있으면 새 버전을 사용합니다. GitHub CLI로 후보 실행을 요청할 수 있습니다.

```powershell
gh workflow run release.yml --ref main -f publish=false -f windows11_verified=false
gh run list --workflow release.yml --event workflow_dispatch --limit 5
```

Release는 Windows CI와 Security를 재사용합니다. Windows 3개 환경·릴리스 정책·RustSec·CodeQL 작업의 성공 여부를 해당 run에서 확인합니다. GNU 작업이 만든 `candidate-<커밋 SHA>` artifact는 14일간 보관하며, 다음 네 파일을 포함합니다.

| 파일 | 내용 |
| --- | --- |
| `TaskbarMonitor-Setup-<버전>-x64.exe` | 현재 사용자용 설치 프로그램 |
| `TaskbarMonitor-Portable-<버전>-x64.zip` | 실행 파일·초기 설정·문서·라이선스 |
| `SHA256SUMS.txt` | 설치 EXE와 Portable ZIP의 해시 |
| `build-info.json` | 버전·소스 커밋·target·Rust 버전·내부 EXE 해시 |

`publish=false`의 성공은 후보 준비 완료입니다. Windows 11 화면 검증이나 공개 완료는 별도입니다. 일반 PR/CI artifact를 게시용 후보로 대신하지 말고, 성공한 **Release workflow run**을 선택합니다.

## 2. 같은 후보를 Windows 11에서 검증하기

다음은 저장소 루트에서 후보를 내려받아 검증할 새 폴더를 만드는 예시입니다. 완료된 후보 run ID를 입력합니다. GitHub CLI 로그인과 PowerShell 7이 필요합니다.

```powershell
$candidateRun = Read-Host '성공한 Release 후보 run ID'
$run = gh run view $candidateRun --json conclusion,headSha | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or $run.conclusion -ne 'success') { throw 'Candidate run did not succeed' }
$candidateDir = Join-Path 'target' ('candidate-' + [guid]::NewGuid().ToString('N'))
gh run download $candidateRun --name "candidate-$($run.headSha)" --dir $candidateDir
if ($LASTEXITCODE -ne 0) { throw 'Candidate download failed' }
foreach ($line in Get-Content (Join-Path $candidateDir 'SHA256SUMS.txt')) {
    if ($line -notmatch '^([A-Fa-f0-9]{64})  ([A-Za-z0-9._-]+)$') { throw 'Invalid checksum manifest' }
    if ((Get-FileHash (Join-Path $candidateDir $Matches[2])).Hash -ne $Matches[1]) { throw 'Asset hash mismatch' }
}
$info = Get-Content (Join-Path $candidateDir 'build-info.json') -Raw | ConvertFrom-Json
if ($info.source_commit -ne $run.headSha) { throw 'Candidate commit mismatch' }
$portableDir = Join-Path $candidateDir 'portable'
Expand-Archive -LiteralPath (Join-Path $candidateDir "TaskbarMonitor-Portable-$($info.version)-x64.zip") -DestinationPath $portableDir
$candidateExe = Join-Path $portableDir 'TaskbarMonitor/taskbar-monitor.exe'
$verifiedHash = (Get-FileHash -LiteralPath $candidateExe).Hash
if ($verifiedHash -ne $info.executable_sha256) { throw 'Executable hash mismatch' }
Write-Output "Candidate run: $candidateRun"
Write-Output "Commit: $($run.headSha)"
Write-Output "Executable SHA-256: $verifiedHash"
```

이 파일을 **깨끗한 대화형 Windows 11 VM**에서 [호환성 매트릭스](compatibility.md)에 따라 검사합니다. 설치형과 포터블, 디자인·테마·DPI·메뉴·자동 숨김·절전 복귀 등의 실제 실행 범위를 기록합니다. Server CI의 계산·probe 성공으로 화면 검증을 대신하지 않습니다.

검증 기록에는 후보 run ID, 커밋, **검사한 `taskbar-monitor.exe`의 SHA-256**, OS build·배율, 통과/실패/미실행 항목을 남깁니다. 해시는 설치 프로그램 자체의 해시가 아닙니다. 개인 화면·로컬 경로·원본 진단 JSON은 공개 기록에 넣지 않습니다.

설치 수명주기 자동화 `scripts/test-installer.ps1`은 **GitHub-hosted 전용**입니다. 개인 PC에서 실행하거나 실행기 검사를 우회하지 않습니다. Windows 11에서는 준비한 VM 안에서 수동으로 검사합니다.

검사 중 main이 바뀌거나 후보 artifact가 만료되면 새 후보를 만들고 다시 검증합니다. 수정·재빌드한 로컬 EXE를 이전 후보의 검사 결과로 대신하지 않습니다.

## 3. 검증한 후보 그대로 게시하기

호환성 검증을 마친 뒤 Actions의 **Release → Run workflow**에서 main과 아래 값을 지정합니다. 같은 작업은 다음 명령으로 요청할 수 있습니다.

```powershell
$verifiedRunId = Read-Host 'Windows 11에서 검증한 후보 run ID'
$verifiedExeHash = Read-Host '검증한 taskbar-monitor.exe의 SHA-256'
gh workflow run release.yml --ref main -f publish=true -f windows11_verified=true -f "verified_candidate_run_id=$verifiedRunId" -f "verified_executable_sha256=$verifiedExeHash"
```

게시 실행도 CI와 Security를 다시 검사합니다. **이번 실행이 새로 빌드한 파일을 게시하지 않습니다.** 지정한 이전 Release run의 후보 artifact를 내려받아 다음 조건을 확인합니다.

- 성공한 동일 저장소·main·소스 SHA의 Release 수동 실행에서 나온 후보인가.
- 후보의 버전·커밋·내부 EXE 해시가 현재 소스와 검증자가 입력한 값에 일치하는가.
- main이 해당 커밋을 가리키고, 그 커밋의 Rust CodeQL 분석이 성공했으며 열린 high/critical 보안 발견이 없는가.
- 배포 파일의 해시가 manifest와 일치하고 버전 태그가 아직 사용되지 않았는가.
- `windows11_verified=true`로 수동 검증 완료를 명시했는가.

수동 검증 확인값 자체가 화면 검사의 증거는 아닙니다. 실행한 시나리오와 해시 기록이 함께 있어야 합니다. 조건 검사나 API 접근에 실패하면 게시를 중단합니다. 판정 구현은 [`release-gate.py`](../scripts/release-gate.py)가 기준입니다.

조건 통과 후 새 태그와 draft Release를 만들고, 태그·커밋을 다시 확인한 뒤 공개합니다. 공개 자산은 위의 네 파일로 제한합니다. 원본 진단자료·검증 ZIP·도구체인은 올리지 않습니다.

## 실패·서명 상태

CI·감사·CodeQL·호환성 회귀가 있으면 원인을 수정하고 해당 소스에서 새 후보를 검증합니다. 기존 태그가 있는 경우 파일을 덮어쓰지 않습니다. 태그나 draft 생성 뒤 실패했다면 유지보수자가 상태를 확인해야 하며, 자동으로 태그를 지워 재시도하지 않습니다.

앱과 설치 프로그램은 현재 **코드 서명되지 않았습니다**. `SHA256SUMS.txt`와 `build-info.json`은 동일성 확인 자료이며 코드 서명이나 악성 코드 부재의 증명이 아닙니다. Inno 컴파일러의 유효한 서명도 생성된 제품의 서명을 뜻하지 않습니다.

관련 문서: [자동 검사](testing.md) · [보안 검사와 대응](security-testing.md) · [호환성](compatibility.md) · [소스 빌드](../BUILD.ko.md).
