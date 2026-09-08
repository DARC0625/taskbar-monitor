# 자동 검사

이 문서는 0.4.1 후보에 구성한 검사와 통과 조건을 설명합니다. 원격 CI를 실행·통과했다는 기록을 대신하지 않습니다. 결과는 **해당 커밋의 Actions 실행**으로 확인하고, 건너뛴 검사는 미실행으로 남깁니다.

## Windows CI

[`ci.yml`](../.github/workflows/ci.yml)은 PR, main push, 매주 일요일 02:17 UTC, 수동 실행 및 Release 호출에서 동작하도록 구성합니다. 세 작업은 각각 최대 35분이며, 한 작업의 실패로 다른 환경 검사를 취소하지 않습니다.

| GitHub 호스팅 이미지 | Rust target | 검사 |
| --- | --- | --- |
| `windows-2022` | `x86_64-pc-windows-msvc` | 형식·Clippy·회귀 테스트·release 빌드·probe |
| `windows-2025` | `x86_64-pc-windows-msvc` | 위와 동일 |
| `windows-2025` | `x86_64-pc-windows-gnu` | 위 검사와 배포 패키지·설치 수명주기 |

이 이미지는 **Windows Server**입니다. 실제 Windows 11 데스크톱 지원 매트릭스와 구분합니다. `Required CI`는 세 Windows 작업과 Ubuntu의 `Release policy tests`가 모두 성공해야 통과합니다.

공개 저장소의 main은 PR과 최신 main 기준의 `Required CI`, `Dependency audit`, `CodeQL` 통과를 필수로 설정합니다. 관리자도 적용 대상이며 강제 push·삭제를 막습니다. 이 저장소 설정은 fork에 자동 복사되는 YAML 기능이 아닙니다. 개별 작업 하나의 성공을 전체 CI나 릴리스 검증 완료로 해석하지 않습니다.

각 환경은 Rust 1.98.1과 `Cargo.lock`을 사용합니다. 실행 명령은 다음과 같습니다.

```powershell
cargo fmt --check
cargo clippy --locked --all-targets -- -D clippy::correctness -D clippy::suspicious
cargo test --locked
cargo build --release --locked
```

현재 소스 기준으로 Rust 회귀 **60개**, Python 릴리스 정책 **21개**, probe **3종**, PowerShell 관찰 도구 회귀 **18개 assertion**을 검사합니다. 패키지 검사는 외부 라이선스 **110개**도 확인합니다. 실제 통과 여부는 해당 커밋의 실행 결과를 기준으로 판단하며, 로컬 결과를 원격 Server 매트릭스·CodeQL·의존성 감사·설치 수명주기의 성공 기록으로 대신하지 않습니다.

| 영역 | 회귀 기준 |
| --- | --- |
| 설정 | 잘린/잘못된 JSON, 타입·정수 범위, 64 KiB 경계, 정규화 전 geometry, 재정규화 안정성 |
| 저장·이관 | 읽기/교체 실패 시 기존 파일 보존, 임시 파일 정리, 기존 설정 위로 구버전을 덮어쓰지 않음 |
| 측정 | CPU 카운터의 역행·overflow, 메모리 범위, engine 단위·시간 경계, NaN/Inf/음수와 실제 0의 구분 |
| 리셋·상태 | 이전 세대 결과의 폐기, 잘못된 sequence 증가 억제, stale 경계와 마지막 값 보존 |
| 진단 | 역순/중복 표본 제외, 큰 유한 값의 평균, 비정상 CPU 시간 및 현재 값 JSON 처리, 최근 표본 버퍼 상한과 전체 기간 최대값 보존 |
| 수집기 수명주기 | 가짜 수집기 지연 중 다른 센서 진행, 중단 후 결과·알림 폐기, 고정 스레드 수와 전체 종료 대기 예산 |
| 장시간 관찰 도구 | PID와 시작 시각 검증, 기존 기록 보존, 읽기 오류·관찰 공백·출력 한계의 구분 |
| 배치·장치 이름 | DPI·작업표시줄 크기·열 폭·offset 경계와 장치 이름 처리 |
| 릴리스 정책 | 후보의 커밋·버전·출처와 보안 분석·해시 조건을 실패 시 차단 |

입력 회귀는 순수 계산과 자신이 생성한 임시 파일만 사용합니다. 시스템 ACL·드라이버·Explorer를 변경하거나 외부 시스템을 스캔하지 않습니다. 계산 테스트가 Windows FFI의 모든 실패나 모든 스레드 순서를 검증하는 것은 아닙니다.

Python 3가 있는 환경에서는 릴리스 정책 회귀만 다음처럼 재현합니다. 이 단위 테스트는 GitHub에 릴리스를 게시하지 않습니다.

```powershell
python -m unittest discover -s scripts -p test_release_gate.py -v
```

## 창 없는 probe 3종

[`test-probe.ps1`](../scripts/test-probe.ps1)은 release EXE를 자식 프로세스로 실행합니다. 각 실행은 최대 30초이며, 시간 초과 정리는 이 스크립트가 시작한 자식에만 적용합니다.

| 입력 | 기대 결과 |
| --- | --- |
| `--probe --seconds 6` | 종료 0, 유효한 JSON, 갱신된 sequence, CPU/RAM 표본과 메모리·백분율 조건 충족 |
| `--probe --seconds 0` | 종료 2, `invalid_arguments` 보고서 |
| `--probe --unknown` | 종료 2, `invalid_arguments` 보고서 |

`ready` 값은 유한하고 단위에 맞는 범위여야 하며, 준비되지 않은 항목의 현재 값은 `null`이어야 합니다. GPU·NPU·팬 등의 미지원 상태를 0으로 바꾸거나 가상 서버에 없는 센서의 `ready`를 강제하지 않습니다. 백분율 상한 100을 팬 RPM·디스크 전송률에 적용하지 않습니다.

로컬 재현은 [빌드 안내](../BUILD.ko.md)의 도구 준비 후 다음 명령으로 합니다.

```powershell
./scripts/test-probe.ps1 -Executable "target/$env:CARGO_BUILD_TARGET/release/taskbar-monitor.exe"
```

원본 JSON은 장치 설명을 포함할 수 있습니다. CI는 `target/probe-tests/summary.json`의 허용된 요약만 14일간 artifact로 보관합니다. 이 probe는 위젯 렌더러·화면 지연·센서의 물리적 정확도·24시간 안정성을 검증하지 않습니다.

## 패키지·설치 검사

GNU 작업은 [`build-release.ps1`](../packaging/build-release.ps1)로 후보를 만들고 [`test-package.ps1`](../scripts/test-package.ps1)로 다음을 확인합니다.

- Cargo·EXE·manifest 버전과 `asInvoker` 권한.
- 필수 라이선스와 설치 payload의 `portable.flag` 부재.
- 공개 EXE의 원래 빌드 경로 잔존 여부.
- ZIP의 허용된 항목과 경로, 내부 EXE와 설치 payload EXE의 SHA-256 동일성.
- `SHA256SUMS.txt`에 기록된 배포 파일의 해시.

[`test-installer.ps1`](../scripts/test-installer.ps1)은 **일회성 GitHub-hosted 실행기에서만** 실행합니다. 개인 PC·self-hosted runner에서 실행하지 않으며 실행기 확인을 우회하지 않습니다. 기존 설치·사용자 데이터·위젯이 있으면 중단합니다.

검사 범위는 한글·공백 경로의 새 설치, 설치된 앱·시작 메뉴 등록, 기본 자동 시작 꺼짐, 같은 버전 재설치, 자동 시작 선택/해제, 제거, 사용자 설정 보존입니다. 설치된 EXE에도 창 없는 probe를 실행합니다. 실제 이전 버전과의 업그레이드 또는 실행 중인 GUI 종료 시나리오 전체를 자동 통과로 해석하지 않습니다.

성공한 GNU 작업은 `candidate-<커밋 SHA>`에 설치 EXE·Portable ZIP·해시·빌드 정보를 14일간 보관합니다. 후보 artifact 생성은 GitHub Release 게시와 다릅니다.

## 보안·Windows 11 확인

[`security.yml`](../.github/workflows/security.yml)은 RustSec `cargo-audit`와 Rust CodeQL `security-extended` 분석을 수행하도록 구성합니다. Dependabot은 업데이트 PR을 만들며 자동 병합하지 않습니다. 범위와 실패 대응은 [보안 안내](security-testing.md)를 참고하세요.

실제 Windows 11 GUI는 깨끗한 대화형 VM에서 [호환성 매트릭스](compatibility.md)로 별도 확인합니다. 디자인·테마·실제 DPI·메뉴·드래그·자동 숨김·전체 화면·절전 복귀·Explorer 재연결·장시간 실행의 결과를 Server CI 성공으로 대신하지 않습니다. 검증한 EXE 해시와 실행한 범위를 기록한 뒤 [릴리스 절차](release-process.md)를 따릅니다.

실제 실행 프로세스의 보고서, 안전한 재연결 스모크 검사, 24시간 관찰 방법은 [위젯 안정성 검사](runtime-validation.md)를 참고하세요. 관찰 도구 자체의 회귀 테스트는 Server CI에서도 실행하지만, 작업표시줄 위젯을 표시하는 검사는 대화형 Windows 11에서 별도로 실행합니다.
