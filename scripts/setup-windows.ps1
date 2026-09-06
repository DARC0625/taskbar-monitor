param(
    [ValidateSet('x86_64-pc-windows-gnu','x86_64-pc-windows-msvc')][string]$Target='x86_64-pc-windows-gnu',
    [switch]$Installer
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$root=Split-Path $PSScriptRoot -Parent
$toolsPath=if($env:RUNNER_TEMP){Join-Path $env:RUNNER_TEMP 'taskbar-monitor-tools'}else{Join-Path $root 'target/ci-tools'}
New-Item -ItemType Directory -Path $toolsPath -Force | Out-Null
function Get-VerifiedFile([string]$Url,[string]$Name,[string]$Sha256) {
    $file=Join-Path $toolsPath $Name
    if(-not (Test-Path -LiteralPath $file)) { Invoke-WebRequest -Uri $Url -OutFile $file }
    if((Get-FileHash -LiteralPath $file).Hash -ne $Sha256){throw "Tool checksum mismatch: $Name"}
    return $file
}
rustup toolchain install 1.98.1 --profile minimal --component rustfmt --component clippy --target $Target
if($LASTEXITCODE -ne 0){throw 'Rust toolchain installation failed'}
rustup override set 1.98.1 --path $root
if($LASTEXITCODE -ne 0){throw 'Rust override failed'}
$env:CARGO_BUILD_TARGET=$Target
$flags=@()
if($Target -eq 'x86_64-pc-windows-gnu') {
    $archive=Get-VerifiedFile 'https://github.com/mstorsjo/llvm-mingw/releases/download/20260826/llvm-mingw-20260826-ucrt-x86_64.zip' 'llvm-mingw.zip' 'AE601F4E0F72BBDF441AD2DF8BB16F037E2E9251559EA6B37B4057AEF39C06C3'
    $bin=Join-Path $toolsPath 'llvm-mingw-20260826-ucrt-x86_64/bin'
    if(-not (Test-Path -LiteralPath (Join-Path $bin 'llvm-rc.exe'))){Expand-Archive -LiteralPath $archive -DestinationPath $toolsPath}
    $env:PATH="$bin;$env:PATH"
    $sysroot=(& rustup run 1.98.1 rustc --print sysroot).Trim()
    $hostTriple=((& rustup run 1.98.1 rustc -vV | Select-String '^host:').ToString() -split ':',2)[1].Trim()
    $linker=Join-Path $sysroot "lib/rustlib/$hostTriple/bin/rust-lld.exe"
    if(-not (Test-Path -LiteralPath $linker)){throw 'Rust LLD is unavailable'}
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$linker
    $flags=@('-C','linker-flavor=ld.lld','-C','link-self-contained=yes')
    if($env:GITHUB_PATH){$bin | Out-File -LiteralPath $env:GITHUB_PATH -Append -Encoding utf8}
    if($env:GITHUB_ENV){"CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$linker" | Out-File -LiteralPath $env:GITHUB_ENV -Append -Encoding utf8}
}
# The most specific remapping is last. No private build directory is distributed.
$flags+=@("--remap-path-prefix=$env:USERPROFILE=/user","--remap-path-prefix=$root=/workspace")
$env:CARGO_ENCODED_RUSTFLAGS=$flags -join [char]31
if($env:GITHUB_ENV){
    "CARGO_BUILD_TARGET=$Target" | Out-File -LiteralPath $env:GITHUB_ENV -Append -Encoding utf8
    "CARGO_ENCODED_RUSTFLAGS=$env:CARGO_ENCODED_RUSTFLAGS" | Out-File -LiteralPath $env:GITHUB_ENV -Append -Encoding utf8
}
if($Installer){
    $installer=Get-VerifiedFile 'https://github.com/jrsoftware/issrc/releases/download/is-7_1_0/innosetup-7.1.0-x64.exe' 'innosetup.exe' '0362A383ED217D4C4239B5933866DD96D3EB2102737DA92F80F6057A4B40DF2F'
    $signature=Get-AuthenticodeSignature -LiteralPath $installer
    if($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Pyrsys B.V.'){throw 'Inno Setup publisher verification failed'}
    $destination=Join-Path $toolsPath 'inno-7'
    if(-not (Test-Path -LiteralPath (Join-Path $destination 'ISCC.exe'))){
        $process=Start-Process -FilePath $installer -ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-','/CURRENTUSER','/PORTABLE=1','/NOICONS',('/DIR="'+$destination+'"')) -WindowStyle Hidden -Wait -PassThru
        if($process.ExitCode -ne 0){throw 'Portable Inno Setup preparation failed'}
    }
    $env:INNO_COMPILER=Join-Path $destination 'ISCC.exe'
    if($env:GITHUB_ENV){"INNO_COMPILER=$env:INNO_COMPILER" | Out-File -LiteralPath $env:GITHUB_ENV -Append -Encoding utf8}
}
