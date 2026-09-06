# 소스 빌드

Windows 11 x64용 Rust 네이티브 앱입니다. WebView나 별도 .NET 런타임을 사용하지 않습니다. 공식 배포는 0.4.0이며 현재 소스 0.4.1은 개선 후보입니다.

## 준비

Windows x64, PowerShell 7, Git, `rustup`이 필요합니다. MSVC target은 Visual Studio Build Tools의 C++ 도구와 Windows SDK도 필요합니다. 명령은 저장소 루트의 같은 PowerShell 세션에서 실행합니다.

[`setup-windows.ps1`](scripts/setup-windows.ps1)은 Rust **1.98.1**, rustfmt, clippy와 target을 준비합니다. GNU 경로에서는 SHA-256을 확인한 **LLVM-MinGW 20260826 UCRT x86_64**를 사용하고 Rust LLD와 self-contained 링크를 설정합니다. `-Installer`는 **Inno Setup 7.1.0** 컴파일러도 준비하며, 다운로드 해시와 Authenticode의 Valid 상태·Pyrsys B.V. 발행자를 확인합니다. 빌드 도구는 앱 배포 파일에 포함하지 않습니다.

`Cargo.lock`을 커밋하고 `--locked`로 빌드합니다. `Cargo.toml`의 `rust-version`은 선언이며 현재 CI가 그 최소 버전까지 검사한다는 뜻은 아닙니다. 도구 버전·다운로드 검증의 실제 정의는 스크립트가 기준입니다.

## 앱 빌드와 회귀 검사

GNU 예시입니다. MSVC로 검사하려면 첫 줄의 target을 `x86_64-pc-windows-msvc`로 바꿉니다.

```powershell
./scripts/setup-windows.ps1 -Target x86_64-pc-windows-gnu
cargo fmt --check
if ($LASTEXITCODE -ne 0) { throw 'Formatting failed' }
cargo clippy --locked --all-targets -- -D clippy::correctness -D clippy::suspicious
if ($LASTEXITCODE -ne 0) { throw 'Clippy failed' }
cargo test --locked
if ($LASTEXITCODE -ne 0) { throw 'Tests failed' }
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
$exe = "target/$env:CARGO_BUILD_TARGET/release/taskbar-monitor.exe"
./scripts/test-probe.ps1 -Executable $exe
```

probe는 위젯 창을 만들지 않고 별도 자식 프로세스에서 시스템 상태를 읽습니다. 보고서는 기본적으로 `target/probe-tests`에 저장합니다. 원본에는 장치 설명이 들어갈 수 있으므로 공개하지 말고 허용된 `summary.json`만 공유합니다. 검사 범위와 현재 테스트 구성은 [자동 검사 안내](docs/testing.md)를 참고하세요.

`build.rs`는 `assets/app.rc`의 아이콘·버전·manifest를 EXE에 넣습니다. 아이콘을 다시 생성할 때만 Python/Pillow와 `packaging/make-icon.py`가 필요합니다. 일반 빌드에는 필요하지 않습니다.

## 설치 EXE와 포터블 ZIP 만들기

이 단계는 배포 파일을 **생성·검사**합니다. 생성한 설치 프로그램이나 위젯을 자동으로 실행하지 않습니다. 이전 출력이 섞이지 않도록 매번 새로운 빈 출력 폴더를 사용합니다.

```powershell
./scripts/setup-windows.ps1 -Target x86_64-pc-windows-gnu -Installer
$bundle = Join-Path 'target' ('bundle-' + [guid]::NewGuid().ToString('N'))
$payload = Join-Path $bundle 'payload'
$release = Join-Path $bundle 'dist'
./packaging/build-release.ps1 -InnoCompiler $env:INNO_COMPILER -PayloadDir $payload -ReleaseDir $release
./scripts/test-package.ps1 -ReleaseDir $release -PayloadDir $payload
Get-ChildItem -LiteralPath $release
```

빌드 스크립트는 테스트와 release 빌드 후 설치 EXE, Portable ZIP, `SHA256SUMS.txt`, `build-info.json`을 만듭니다. 이미 같은 소스에서 검사한 EXE를 사용할 때만 `-SkipBuild -ExecutablePath <EXE>`를 지정합니다.

설치 payload에는 EXE·사용 안내·앱 LICENSE·외부 라이선스만 넣습니다. `portable.flag`와 일반 초기 설정 `widget.json`은 포터블 ZIP에만 추가합니다. 개인 설정·진단자료·소스·도구체인을 payload에 넣지 않습니다. 패키지 검사는 버전·manifest 권한·라이선스·허용된 ZIP 항목·EXE 동일성·배포 해시를 확인합니다.

공개 빌드는 `--remap-path-prefix`로 사용자 경로를 `/user`, 저장소 경로를 `/workspace`로 치환합니다. 가장 구체적인 저장소 규칙을 마지막에 적용하며 `CARGO_ENCODED_RUSTFLAGS`로 공백이 있는 경로를 전달합니다. 심볼 제거만으로 소스 경로가 없어지는 것은 아닙니다.

**`scripts/test-installer.ps1`은 GitHub-hosted 실행기 전용입니다. 개인 PC에서 실행하거나 실행기 환경 변수를 흉내 내 우회하지 마세요.** 실제 Windows 11 GUI 검사는 별도 깨끗한 VM에서 [호환성 절차](docs/compatibility.md)에 따라 수동으로 진행합니다.

## 공개 전 확인

앱과 설치 프로그램에는 **코드 서명이 적용되지 않았습니다**. 다운로드한 Inno 컴파일러의 서명 검증이나 SHA-256 파일이 앱의 서명을 대신하지 않습니다.

로컬 빌드 성공은 원격 CI·보안 검사·Windows 11 GUI 검증·공개 완료를 뜻하지 않습니다. [자동 검사](docs/testing.md), [보안 검사](docs/security-testing.md), [릴리스 절차](docs/release-process.md)를 따르세요. 작업표시줄 연결은 공식 Windows 확장 API가 아니므로 OS 업데이트별 확인이 필요합니다.
