<img src="assets/taskbar-monitor.png" width="80" height="80" alt="Taskbar Monitor 아이콘">

# Taskbar Monitor

**Windows 11 작업표시줄 안에서 PC 상태를 확인하는 Rust 위젯입니다.**

CPU·RAM·GPU·디스크·NPU·팬 상태와 장치 이름을 작은 게이지로 표시합니다. 기본 모니터의 가로 작업표시줄을 대상으로 합니다.

**공식 배포는 [v0.4.0](https://github.com/DARC0625/taskbar-monitor/releases/tag/v0.4.0)**입니다. 현재 소스의 **0.4.1은 안정성·자동 검사 개선 후보**이며, 소스 버전 변경이 새 릴리스의 검증·게시 완료를 뜻하지 않습니다.

[사용 안내](packaging/README.ko.txt) · [소스 빌드](BUILD.ko.md) · [자동 검사](docs/testing.md) · [실제 위젯 검사](docs/runtime-validation.md) · [호환성](docs/compatibility.md) · [보안](docs/security-testing.md) · [릴리스 절차](docs/release-process.md)

## 화면과 조작

- **HUD / EVA / Minimal**: 원형 계기, 분절 게이지, 얇은 원형 디자인을 선택합니다.
- **테마**: Windows 테마에 맞추거나 밝은 테마·어두운 테마를 고정합니다.
- **표시 항목과 폭**: 필요한 항목만 남기고 열의 폭을 조절합니다.
- **위치**: 왼쪽 버튼으로 끌어서 이동하거나 우클릭 메뉴에서 좌우로 미세 조정합니다.
- **자동 실행**: 우클릭 메뉴의 **Windows 로그인 시 자동 실행**으로 켜고 끕니다. 설치 프로그램에서 선택한 등록 상태도 메뉴에 반영합니다.

위젯은 Explorer 작업표시줄의 자식 창으로 연결됩니다. Direct2D·DirectWrite·DirectComposition으로 텍스트와 반투명 게이지를 그리며, 부모 작업표시줄 안에 함께 표시되도록 구성합니다. 설정과 종료는 위젯 또는 알림 영역 아이콘의 **우클릭 메뉴**에서 찾을 수 있습니다.

## 설치와 포터블 실행

설치 프로그램과 포터블 ZIP은 [GitHub Releases](https://github.com/DARC0625/taskbar-monitor/releases)에서 확인할 수 있습니다. 직접 빌드하려면 [소스 빌드 안내](BUILD.ko.md)를 참고하세요.

- **설치형**: 현재 사용자용으로 설치합니다. 설치할 때 바탕 화면 바로 가기와 Windows 로그인 시 자동 시작을 선택할 수 있으며, 처음에는 두 옵션 모두 꺼져 있습니다. 제거는 Windows의 **설정 → 앱 → 설치된 앱**에서 진행합니다.
- **포터블**: 쓰기 가능한 폴더에 파일을 풀고 `taskbar-monitor.exe`를 실행합니다. 실행 파일 옆에 `portable.flag`가 있어야 포터블 설정 경로를 사용합니다.

자동 실행은 현재 사용자에게 적용되며 관리자 권한이 필요하지 않습니다. 체크 표시는 현재 실행 파일의 로그인 자동 실행 등록 상태입니다. Windows에서 시작 앱을 별도로 사용 중지한 경우, 메뉴의 **Windows 시작 앱 설정…**에서 다시 켤 수 있습니다. 포터블 폴더를 이동·삭제하기 전에는 자동 실행을 꺼 주세요. 다른 위치의 실행 파일이 등록되어 있으면 해당 위치의 앱에서 해제한 뒤 새 위치에서 켭니다.

앱과 설치 프로그램에는 **코드 서명이 없습니다**.

## 측정값과 지원 상태

CPU·RAM은 250ms, GPU·디스크 등은 1초 간격으로 수집합니다. 이는 수집 주기이며 화면 표시 지연이나 모든 PC에서의 성능을 보장하는 수치는 아닙니다.

CPU/RAM·GPU·디스크·NPU 수집은 네 개의 고정 작업 스레드로 분리합니다. 한 센서가 응답을 늦게 보내도 다른 수집 경로는 계속 진행하며, 오래된 결과는 갱신 지연으로 구분합니다. 종료 대기와 실제 위젯의 성능 측정 범위는 [안정성 검사 안내](docs/runtime-validation.md)에 설명되어 있습니다.

DISK 게이지는 물리 디스크 중 가장 높은 활성 시간을 표시합니다. 각 항목의 하위 메뉴에서 장치 이름과 측정 내용을 확인할 수 있습니다.

하드웨어나 드라이버가 값을 제공하지 않으면 상태를 구분해서 표시합니다.

| 표시 | 의미 |
| --- | --- |
| 장치 없음 | NPU 등 해당 장치가 확인되지 않음 |
| 센서 미지원 | 장치나 드라이버가 필요한 센서 값을 제공하지 않음 |
| 준비 중 | 측정 기준을 준비하는 중 |
| 갱신 지연 / 조회 오류 | 최신 값을 받지 못했거나 조회에 실패함 |

NPU가 없는 PC나 팬 RPM을 읽을 수 없는 PC에 가짜 `0%`·`0RPM`을 표시하지 않습니다. **센서 다시 확인**으로 장치와 측정 기준을 새로 확인할 수 있습니다.

## 설정 저장

| 실행 방식 | 설정 파일 |
| --- | --- |
| 일반 설치 또는 `portable.flag` 없음 | `%LocalAppData%\TaskbarMonitor\widget.json` |
| 실행 파일 옆에 `portable.flag` 있음 | 실행 파일과 같은 폴더의 `widget.json` |

일반 모드에서 저장된 설정이 없으면 실행 파일 옆의 유효한 구버전 설정을 한 번 가져옵니다. 설치형 앱을 제거해도 사용자 설정은 재설치를 위해 남습니다. 설정 파일을 직접 수정할 때는 앱을 먼저 종료해 주세요.

## 개발과 호환성

Rust와 Windows 네이티브 API를 사용합니다. 빌드 도구, 테스트, 설치 프로그램 제작 절차는 [BUILD.ko.md](BUILD.ko.md)에 정리되어 있습니다.

0.4.1 후보의 자동 검사는 Windows Server 2022/MSVC, Server 2025/MSVC, Server 2025/GNU에서 회귀 테스트와 창 없는 probe를 실행하도록 구성합니다. GNU 작업은 설치·포터블 패키지와 GitHub 호스팅 실행기 안의 설치 수명주기도 검사합니다. CodeQL과 RustSec 감사는 별도 보안 워크플로에서 수행합니다. **구성된 검사와 통과한 검사는 다릅니다.** 해당 커밋의 [Actions 결과](https://github.com/DARC0625/taskbar-monitor/actions)를 확인하세요.

현재 Windows 11 x64용이며, 작업표시줄 내부 창 구조를 사용하는 방식은 공식 작업표시줄 확장 API가 아니므로 Windows 업데이트나 다른 작업표시줄 도구와의 호환성은 별도로 확인해야 합니다.

Server CI는 실제 Windows 11 작업표시줄의 모양·입력·자동 숨김·절전 복귀를 보증하지 않습니다. 공개 전에는 [Windows 11 수동 검증](docs/compatibility.md)을 수행하고, 검증한 후보의 실행 파일 해시를 [릴리스 절차](docs/release-process.md)에 따라 확인합니다. 설치 수명주기 자동화 스크립트는 개인 PC에서 실행하지 않습니다.

앱 소스는 [MIT License](LICENSE)로 제공합니다. 외부 구성요소의 고지는 [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt)와 [licenses](licenses/)에 포함되어 있습니다.
