param([string]$InnoCompiler = 'ISCC.exe', [Parameter(Mandatory)][string]$PayloadDir, [Parameter(Mandatory)][string]$ReleaseDir)
$ErrorActionPreference = 'Stop'
$projectDir = Split-Path $PSScriptRoot -Parent
Push-Location $projectDir
try {
    cargo test --locked
    if ($LASTEXITCODE -ne 0) { throw 'Rust tests failed' }
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Rust release build failed' }
    New-Item -ItemType Directory -Path $PayloadDir -Force | Out-Null
    Copy-Item -LiteralPath 'target/release/taskbar-monitor.exe' -Destination $PayloadDir
    Copy-Item -LiteralPath 'packaging/README.ko.txt' -Destination $PayloadDir
    # THIRD-PARTY-NOTICES.txt and licenses/ must be prepared in PayloadDir.
    & $InnoCompiler ("/DPayloadDir=" + [IO.Path]::GetFullPath($PayloadDir)) ("/DReleaseDir=" + [IO.Path]::GetFullPath($ReleaseDir)) (Join-Path $PSScriptRoot 'installer.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
} finally { Pop-Location }
