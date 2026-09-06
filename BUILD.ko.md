# Taskbar Monitor 0.4.0 소스 빌드

Windows 11 x64용 Rust 네이티브 앱입니다. WebView, 별도 .NET 런타임은 사용하지 않습니다.

검증한 도구: Rust 1.98.1 stable-x86_64-pc-windows-gnu, LLVM-MinGW 20260826 UCRT x86_64, Rust LLD, Inno Setup 7.1.0 x64. 패키지에 도구체인은 포함하지 않습니다. 의존 버전은 Cargo.lock으로 고정합니다.

GNU 빌드는 LLVM-MinGW의 bin을 PATH 앞에 두어 windres/dlltool을 사용할 수 있어야 합니다. 이 릴리스는 Rust sysroot의 lib/rustlib/x86_64-pc-windows-gnu/bin/rust-lld.exe를 CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER로 지정하고 RUSTFLAGS를 `-C linker-flavor=ld.lld -C link-self-contained=yes`로 설정했습니다. 오래된 GNU 링커로 대체한 빌드는 검증하지 않았습니다.

공개 배포 빌드에는 Rust의 `--remap-path-prefix`를 추가하여 작업 폴더 경로를 `/workspace`, 사용자 폴더 경로를 `/user`로 치환합니다. 공백이 있는 경로를 안전하게 전달하려면 `CARGO_ENCODED_RUSTFLAGS`에 인수를 ASCII 0x1f로 구분해 지정합니다. 설정 파일·진단 결과·도구체인 폴더는 Git에 올리지 않습니다.

1. `cargo fmt --check`, `cargo test --locked`, `cargo build --release --locked`를 실행합니다.
2. build.rs가 assets/app.rc의 아이콘·버전 정보·manifest를 EXE에 넣습니다. 아이콘 재생성만 Python/Pillow와 packaging/make-icon.py를 사용합니다.
3. payload 폴더에 taskbar-monitor.exe, packaging/README.ko.txt, 루트 LICENSE, THIRD-PARTY-NOTICES.txt, licenses/를 준비합니다. packaging/build-release.ps1은 빌드 후 이 파일들을 함께 복사합니다.
4. `ISCC.exe /DPayloadDir="절대 payload 경로" /DReleaseDir="절대 출력 경로" packaging/installer.iss`로 설치 프로그램을 만듭니다. 한국어 번역은 Inno Setup에 포함됩니다.
5. 휴대용 ZIP에만 portable.flag를 넣습니다. 설치 payload에는 설정·portable.flag·검증 보고서·소스를 넣지 않습니다.

앱과 설치 프로그램은 코드 서명되지 않았습니다. 공식 Inno Setup 컴파일러는 다운로드 후 Authenticode의 Valid 상태와 Pyrsys B.V. 발행자를 확인했습니다. 컴파일러 서명은 생성된 앱/설치 프로그램의 서명을 의미하지 않습니다.

작업표시줄 연결은 별도 프로세스에서 Explorer의 Shell_TrayWnd를 부모로 삼는 자식 HWND 방식입니다. DLL 주입이나 Explorer 바이너리 변경을 사용하지 않습니다. Windows 11의 공식 위젯 확장 계약은 아니므로 OS 업데이트 호환성은 별도 확인해야 합니다.

참고: [Inno Setup](https://jrsoftware.org/isdl.php), [embed-resource](https://docs.rs/embed-resource/3.0.11/embed_resource/), [SetParent](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setparent).
