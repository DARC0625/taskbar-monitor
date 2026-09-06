param([string]$InnoCompiler='ISCC.exe', [Parameter(Mandatory)][string]$PayloadDir, [Parameter(Mandatory)][string]$ReleaseDir, [string]$ExecutablePath, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectDir = Split-Path $PSScriptRoot -Parent
function Normalize-Path([string]$Path) {
    return [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)))
}
function Same-Or-Descendant([string]$Candidate,[string]$Ancestor) {
    if($Candidate.Equals($Ancestor,[StringComparison]::OrdinalIgnoreCase)){return $true}
    $prefix=$Ancestor
    if(-not $prefix.EndsWith([IO.Path]::DirectorySeparatorChar.ToString())){$prefix += [IO.Path]::DirectorySeparatorChar}
    return $Candidate.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase)
}
$projectDir=Normalize-Path $projectDir
$PayloadDir=Normalize-Path $PayloadDir
$ReleaseDir=Normalize-Path $ReleaseDir
$executable=if($ExecutablePath){Normalize-Path $ExecutablePath}else{Join-Path $projectDir 'target/release/taskbar-monitor.exe'}
if((Same-Or-Descendant $PayloadDir $ReleaseDir) -or (Same-Or-Descendant $ReleaseDir $PayloadDir)){
    throw 'Payload and release directories must not equal or contain each other'
}
foreach($destination in @($PayloadDir,$ReleaseDir)){
    if(Same-Or-Descendant $projectDir $destination){throw 'An output directory must not equal or contain the source directory'}
    if(Test-Path -LiteralPath $destination){
        $item=Get-Item -LiteralPath $destination -Force
        if(-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)){
            throw 'Output paths must be ordinary directories'
        }
        if(@(Get-ChildItem -LiteralPath $destination -Force).Count -gt 0){throw 'Use fresh empty payload and release directories; stale files must not enter a release'}
    }
}
# Validate both destinations before creating either one.
foreach($destination in @($PayloadDir,$ReleaseDir)){
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
}
Push-Location $projectDir
try {
    $version=[regex]::Match((Get-Content Cargo.toml -Raw),'(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"').Groups[1].Value
    if(-not $version){throw 'Expected a stable numeric Cargo version'}
    if(-not $SkipBuild){
        cargo test --locked
        if ($LASTEXITCODE -ne 0) { throw 'Rust tests failed' }
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'Rust release build failed' }
        if(-not $ExecutablePath -and $env:CARGO_BUILD_TARGET){$executable=Join-Path $projectDir "target/$env:CARGO_BUILD_TARGET/release/taskbar-monitor.exe"}
    } elseif(-not $ExecutablePath){throw '-SkipBuild requires -ExecutablePath'}
    if((Get-Item -LiteralPath $executable).VersionInfo.ProductVersion -ne $version){throw 'Executable and source versions differ'}
    Copy-Item -LiteralPath $executable -Destination (Join-Path $PayloadDir 'taskbar-monitor.exe')
    Copy-Item -LiteralPath 'packaging/README.ko.txt' -Destination $PayloadDir
    Copy-Item -LiteralPath 'LICENSE','THIRD-PARTY-NOTICES.txt' -Destination $PayloadDir
    # Copy the contents to an explicit target so repeated builds cannot nest licenses/.
    $licenseDirectory = Join-Path $PayloadDir 'licenses'
    New-Item -ItemType Directory -Path $licenseDirectory -Force | Out-Null
    Get-ChildItem -LiteralPath 'licenses' | Copy-Item -Destination $licenseDirectory -Recurse -Force
    & $InnoCompiler ("/DPayloadDir=$PayloadDir") ("/DReleaseDir=$ReleaseDir") ("/DAppVersion=$version") (Join-Path $PSScriptRoot 'installer.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
    $portable=Join-Path (Split-Path $PayloadDir -Parent) ('portable-'+[guid]::NewGuid().ToString('N'))
    $portableApp=Join-Path $portable 'TaskbarMonitor'
    New-Item -ItemType Directory -Path $portableApp -Force | Out-Null
    Get-ChildItem -LiteralPath $PayloadDir | Copy-Item -Destination $portableApp -Recurse
    Set-Content -LiteralPath (Join-Path $portableApp 'portable.flag') -Value 'Portable mode: settings stay beside this executable.' -Encoding ascii
    @{theme='auto';offset_dip=12;column_dip=96;visible=@($true,$true,$true,$true,$true,$true);style='hud'} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $portableApp 'widget.json') -Encoding utf8NoBOM
    Compress-Archive -LiteralPath $portableApp -DestinationPath (Join-Path $ReleaseDir "TaskbarMonitor-Portable-$version-x64.zip") -CompressionLevel Optimal
    $assetNames=@("TaskbarMonitor-Setup-$version-x64.exe","TaskbarMonitor-Portable-$version-x64.zip")
    $lines=foreach($name in $assetNames){(Get-FileHash -LiteralPath (Join-Path $ReleaseDir $name)).Hash+'  '+$name}
    $lines | Set-Content -LiteralPath (Join-Path $ReleaseDir 'SHA256SUMS.txt') -Encoding ascii
    @{version=$version;source_commit=(& git rev-parse HEAD);target=$env:CARGO_BUILD_TARGET;rust=(& rustc --version);executable_sha256=(Get-FileHash -LiteralPath $executable).Hash} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $ReleaseDir 'build-info.json') -Encoding utf8
} finally { Pop-Location }
