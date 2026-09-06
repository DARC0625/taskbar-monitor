# 보안 테스트와 대응

이 프로젝트는 의존성 공지 검사, Rust 정적 분석, 잘못된 입력에 대한 회귀 테스트를 자동화합니다. 이 검사는 알려진 문제를 조기에 찾기 위한 것이며, 침투 테스트 완료나 모든 취약점 부재를 의미하지 않습니다. 운영 PC나 외부 시스템을 공격하는 자동화는 포함하지 않습니다.

## 자동 검사

| 검사 | 범위 | 실행 시점과 결과 |
| --- | --- | --- |
| `Dependency audit` | 커밋된 `Cargo.lock` 전체를 최신 RustSec 공지와 대조 | PR, main 변경, 월요일 02:23 UTC, 수동 실행, 릴리스에서 재사용. 취약점과 경고가 있으면 실패 |
| `CodeQL` | Rust 2024 소스의 `security-extended` 정적 분석 | 같은 시점에 실행. 분석 결과는 GitHub Security의 Code scanning에 저장 |
| Cargo 회귀 테스트 | 설정 범위·파손·원자적 저장, 누락·비정상 센서 값, 작업표시줄 좌표 경계 | CI의 Windows 테스트에서 실행. 실패하면 수정 후 다시 검사 |
| Dependabot | Cargo의 직접·간접 의존성과 GitHub Actions 버전 | 매주 업데이트 PR 생성. 자동 승인·병합은 하지 않음 |

`cargo-audit 0.22.2`는 `--locked --no-default-features`로 설치합니다. 실행 파일 추정 스캔 기능을 제외하고 소스의 잠금 파일을 검사하며, `--deny warnings`를 사용합니다. 공지 무시 목록이나 오류를 성공으로 바꾸는 설정은 두지 않습니다. 네트워크 또는 공지 데이터베이스 오류도 검사 실패로 다룹니다. 공식 동작은 [RustSec cargo-audit](https://github.com/rustsec/rustsec/tree/cargo-audit/v0.22.2/cargo-audit)를 참고하세요.

CodeQL은 Windows 2022에서 `build-mode: none`으로 실행합니다. 이 모드에서도 Rust 분석기는 `build.rs`와 매크로를 실행할 수 있으므로 PR은 일회성 GitHub 호스팅 실행기에서만 처리합니다. 생성 코드·Windows FFI·런타임 창 수명 문제는 이 분석만으로 검증되지 않습니다. [GitHub의 Rust 분석 범위](https://docs.github.com/en/code-security/reference/code-scanning/codeql/build-options-for-compiled-languages#building-rust)와 [지원 언어](https://codeql.github.com/docs/codeql-overview/supported-languages-and-frameworks/)를 기준으로 구성합니다.

분석 작업의 성공은 분석이 끝났다는 뜻입니다. CodeQL의 발견 사항은 별도 Code scanning 알림과 저장소의 코드 스캔 병합 규칙에서 확인해야 합니다. `CodeQL` 작업 성공만을 취약점이 없다는 증거로 사용하지 않습니다.

## 신뢰 경계와 검증 항목

| 입력 또는 경계 | 확인할 동작 | 검증 방법 |
| --- | --- | --- |
| `widget.json` | 잘린 JSON, 잘못된 타입·범위, 64 KiB 초과 파일에서 안전한 기본값 사용 | 단위 테스트와 격리된 임시 파일 테스트 |
| 설정 교체와 구버전 이관 | 저장 실패 시 이전 설정 보존, 임시 파일 정리, 기존 설정 덮어쓰기 방지 | 원자적 저장·이관 회귀 테스트 |
| OS 성능 카운터 | 카운터 초기화·역행, NaN·무한대, 누락 값과 실제 0 구분 | 순수 계산 함수의 결정적 테스트 |
| Explorer HWND와 DPI | 잘못된 크기 거부, 좌표 오버플로 방지, 호스트 변경 후 복구 | 계산 테스트 및 Windows 11 실제 UI 확인 |
| Windows COM·렌더러 자원 | 실패 처리, 참조 수명, 반복 재연결 시 누수·충돌 확인 | 코드 검토, 로컬 진단, 장시간 실행 확인 |
| 설치·업데이트·제거 | 현재 사용자 권한 유지, 설정 보존, 설치 범위 밖 파일 보호 | 격리된 설치 경로에서 생명주기 검사 |
| 공개 PR과 릴리스 | PR에 배포 권한·개인 토큰 제공 금지, 검증한 커밋에서 배포 파일 생성 | 워크플로 검토와 필수 검사 |

이 표는 검증 책임의 구분입니다. 실제 실행 여부는 해당 커밋의 Actions 기록과 호환성 기록을 확인해야 합니다. 수동 항목을 자동화 통과로 대체하지 않습니다.

## CI의 권한과 도구 업데이트

워크플로의 기본 권한은 `contents: read`이며 checkout은 자격 증명을 남기지 않습니다. CodeQL 작업만 분석 결과 업로드에 필요한 `security-events: write`를 추가합니다. `pull_request_target`, 개인 PC의 self-hosted runner, 저장소 비밀값은 사용하지 않습니다. 재사용 워크플로를 호출하는 릴리스 작업에도 CodeQL에 필요한 권한을 명시해야 합니다. [GitHub Actions 보안 지침](https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions)에 따른 구성입니다.

Actions는 발행 저장소에서 확인한 전체 커밋 SHA로 고정합니다. Dependabot PR을 검토할 때 변경 로그와 새 SHA의 출처를 확인하고, 기존 필수 검사가 통과한 뒤 병합합니다. Rust 도구 체인과 `cargo-audit`의 명시적 버전은 주기적으로 확인하고 함께 갱신합니다. [Dependabot 설정 참고](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference).

## 실패가 발견되면

1. 실패한 커밋과 실행 URL, 의존성 이름·버전 또는 소스 위치를 기록합니다. 비공개 정보는 공개 이슈에 붙이지 않습니다.
2. RustSec 발견 사항은 공지의 영향 범위와 사용 API를 확인하고 수정 버전으로 업데이트합니다. 공지 무시나 검사의 비활성화로 통과시키지 않습니다.
3. 소스 문제는 수정 전에 해당 실패를 검출하는 최소 회귀 테스트를 추가하고, 수정 후 전체 필수 검사와 관련 호환성 검사를 실행합니다.
4. 실제 취약점은 [SECURITY.md](../SECURITY.md)의 비공개 절차로 관리하고, 수정 릴리스와 영향받는 버전을 함께 안내합니다.

로컬에서 의존성 검사만 실행하려면 다음 명령을 사용합니다. Rust 1.88 이상이 필요하며 CI는 1.98.1로 고정합니다.

```powershell
cargo install cargo-audit --version 0.22.2 --locked --no-default-features
cargo audit --file Cargo.lock --deny warnings
```
