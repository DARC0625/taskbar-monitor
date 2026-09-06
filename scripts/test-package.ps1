param(
    [Parameter(Mandatory)][string]$ReleaseDir,
    [Parameter(Mandatory)][string]$PayloadDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Resolve-FileSystemDirectory {
    param([Parameter(Mandatory)][string]$Path)

    # Resolve against the PowerShell provider location, not the process CWD.
    $resolved = Resolve-Path -LiteralPath $Path
    if ($resolved.Provider.Name -ne 'FileSystem') {
        throw 'Package directories must use the FileSystem provider'
    }
    $directory = Get-Item -LiteralPath $resolved.ProviderPath -Force
    if (-not $directory.PSIsContainer) {
        throw "Expected a directory: $Path"
    }
    return $directory.FullName
}

function Get-SafeArchiveName {
    param([Parameter(Mandatory)][string]$Name)

    # Accept either ZIP separator, then detect aliases using the normalized name.
    $normalized = $Name.Replace('\', '/')
    if ([string]::IsNullOrWhiteSpace($normalized) -or
        $normalized.StartsWith('/', [StringComparison]::Ordinal) -or
        $normalized.Contains('//') -or
        $normalized -match '[\x00-\x1f\x7f<>:"|?*]') {
        throw "Unsafe archive path: $Name"
    }
    foreach ($segment in $normalized.TrimEnd('/').Split('/')) {
        if ([string]::IsNullOrEmpty($segment) -or $segment -in @('.', '..') -or
            $segment.EndsWith(' ', [StringComparison]::Ordinal) -or
            $segment.EndsWith('.', [StringComparison]::Ordinal) -or
            $segment -match '^(?i:CON|PRN|AUX|NUL|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3])(?:\..*)?$') {
            throw "Unsafe archive path segment: $Name"
        }
    }
    return $normalized
}

function Get-ArchiveEntryHash {
    param([Parameter(Mandatory)][IO.Compression.ZipArchiveEntry]$Entry)

    $algorithm = [Security.Cryptography.SHA256]::Create()
    try {
        $stream = $Entry.Open()
        try {
            return [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '')
        } finally {
            $stream.Dispose()
        }
    } finally {
        $algorithm.Dispose()
    }
}

$ReleaseDir = Resolve-FileSystemDirectory -Path $ReleaseDir
$PayloadDir = Resolve-FileSystemDirectory -Path $PayloadDir
$root = Split-Path $PSScriptRoot -Parent
$version = [regex]::Match(
    (Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw),
    '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"'
).Groups[1].Value
if (-not $version) { throw 'Unsupported Cargo version' }

$commonFiles = @('taskbar-monitor.exe', 'LICENSE', 'README.ko.txt', 'THIRD-PARTY-NOTICES.txt')
foreach ($required in $commonFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $PayloadDir $required) -PathType Leaf)) {
        throw "Missing payload file: $required"
    }
}
$licenseDirectory = Join-Path $PayloadDir 'licenses'
if (-not (Test-Path -LiteralPath $licenseDirectory -PathType Container)) {
    throw 'Missing payload licenses directory'
}
if (Test-Path -LiteralPath (Join-Path $PayloadDir 'portable.flag')) {
    throw 'Installed payload contains portable mode flag'
}
$exe = Join-Path $PayloadDir 'taskbar-monitor.exe'
if ((Get-Item -LiteralPath $exe).VersionInfo.ProductVersion -ne $version) {
    throw 'Executable version differs from Cargo'
}
[xml]$manifest = Get-Content -LiteralPath (Join-Path $root 'assets/app.manifest') -Raw
if ($manifest.assembly.assemblyIdentity.version -ne "$version.0") {
    throw 'Manifest version differs from Cargo'
}
if ($manifest.assembly.trustInfo.security.requestedPrivileges.requestedExecutionLevel.level -ne 'asInvoker') {
    throw 'Unexpected application privilege requirement'
}
$bytes = [IO.File]::ReadAllBytes($exe)
foreach ($encoding in @([Text.Encoding]::UTF8, [Text.Encoding]::Unicode)) {
    $text = $encoding.GetString($bytes)
    foreach ($prefix in @($root, $env:USERPROFILE)) {
        if ([string]::IsNullOrWhiteSpace($prefix)) { continue }
        foreach ($variant in @($prefix, $prefix.Replace('\', '/'))) {
            if ($text.IndexOf($variant, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                throw 'Executable contains a private build path'
            }
        }
    }
}

$expectedFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$expectedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$payloadFiles = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
foreach ($file in $commonFiles) {
    $name = "TaskbarMonitor/$file"
    [void]$expectedFiles.Add($name)
    $payloadFiles.Add($name, (Join-Path $PayloadDir $file))
}
foreach ($file in @('portable.flag', 'widget.json')) {
    [void]$expectedFiles.Add("TaskbarMonitor/$file")
}
[void]$expectedDirectories.Add('TaskbarMonitor/')
[void]$expectedDirectories.Add('TaskbarMonitor/licenses/')

$licenseRoot = Get-Item -LiteralPath $licenseDirectory -Force
if (($licenseRoot.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw 'Payload licenses must not be a reparse point'
}
$licenseItems = @(Get-ChildItem -LiteralPath $licenseDirectory -Force -Recurse)
$licenseFileCount = 0
foreach ($item in $licenseItems) {
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'Payload licenses must not contain reparse points'
    }
    $relative = [IO.Path]::GetRelativePath($licenseDirectory, $item.FullName).Replace('\', '/')
    $name = Get-SafeArchiveName -Name "TaskbarMonitor/licenses/$relative"
    if ($item.PSIsContainer) {
        [void]$expectedDirectories.Add("$name/")
    } else {
        if (-not $expectedFiles.Add($name)) { throw "Duplicate payload license: $name" }
        $payloadFiles.Add($name, $item.FullName)
        $licenseFileCount += 1
    }
}
if ($licenseFileCount -eq 0) { throw 'Payload contains no license files' }

$portableName = "TaskbarMonitor-Portable-$version-x64.zip"
$setupName = "TaskbarMonitor-Setup-$version-x64.exe"
$setupPath = Join-Path $ReleaseDir $setupName
if (-not (Test-Path -LiteralPath $setupPath -PathType Leaf)) {
    throw 'Missing Setup executable'
}
if ((Get-Item -LiteralPath $setupPath).VersionInfo.ProductVersion -ne $version) {
    throw 'Setup executable version differs from Cargo'
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead((Join-Path $ReleaseDir $portableName))
try {
    $seenNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $entries = [Collections.Generic.Dictionary[string, IO.Compression.ZipArchiveEntry]]::new([StringComparer]::Ordinal)
    foreach ($entry in $zip.Entries) {
        $name = Get-SafeArchiveName -Name $entry.FullName
        if (-not $seenNames.Add($name.TrimEnd('/'))) {
            throw "Duplicate portable entry or Windows path alias: $name"
        }
        $isDirectory = $name.EndsWith('/', [StringComparison]::Ordinal)
        $unixType = ($entry.ExternalAttributes -shr 16) -band 0xf000
        $directoryAttribute = ($entry.ExternalAttributes -band [int][IO.FileAttributes]::Directory) -ne 0
        $reparseAttribute = ($entry.ExternalAttributes -band [int][IO.FileAttributes]::ReparsePoint) -ne 0
        if ($unixType -notin @(0, 0x8000, 0x4000) -or $reparseAttribute) {
            throw "Unsupported portable entry type: $name"
        }
        if ($isDirectory) {
            if ($unixType -eq 0x8000 -or $entry.Length -ne 0 -or -not $expectedDirectories.Contains($name)) {
                throw "Unexpected portable directory: $name"
            }
        } else {
            if ($unixType -eq 0x4000 -or $directoryAttribute -or -not $expectedFiles.Contains($name)) {
                throw "Unexpected portable file: $name"
            }
            $entries.Add($name, $entry)
        }
    }
    foreach ($name in $expectedFiles) {
        if (-not $entries.ContainsKey($name)) { throw "Missing portable file: $name" }
    }
    # This also requires every license file exactly once, with no extra license files.
    foreach ($pair in $payloadFiles.GetEnumerator()) {
        $entry = $entries[$pair.Key]
        if ($entry.Length -ne (Get-Item -LiteralPath $pair.Value).Length) {
            throw "Portable file size differs from installer payload: $($pair.Key)"
        }
        $hash = Get-ArchiveEntryHash -Entry $entry
        if ($hash -ne (Get-FileHash -LiteralPath $pair.Value -Algorithm SHA256).Hash) {
            throw "Portable file differs from installer payload: $($pair.Key)"
        }
    }

    $settings = $entries['TaskbarMonitor/widget.json']
    if ($settings.Length -eq 0 -or $settings.Length -gt 64 * 1024) {
        throw 'Portable settings are empty or exceed the application size limit'
    }
    $stream = $settings.Open()
    try {
        $reader = [IO.StreamReader]::new($stream, [Text.UTF8Encoding]::new($false, $true), $false)
        try { $settingsText = $reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally {
        $stream.Dispose()
    }
    if ($settingsText -notmatch '\A[\x09-\x0d\x20]*\{') {
        throw 'Portable settings must be a UTF-8 JSON object without a byte-order mark'
    }
    $config = ConvertFrom-Json -InputObject $settingsText -AsHashtable -NoEnumerate -Depth 16
    if ($config -isnot [Collections.IDictionary]) { throw 'Portable settings must be a JSON object' }
    $configKeys = @('theme', 'style', 'offset_dip', 'column_dip', 'visible')
    if ($config.Count -ne $configKeys.Count) { throw 'Unexpected portable settings keys' }
    foreach ($key in $configKeys) {
        if ($config.Keys -cnotcontains $key) { throw "Missing portable setting: $key" }
    }
    if ($config['theme'] -isnot [string] -or @('auto', 'dark', 'light') -cnotcontains $config['theme']) {
        throw 'Portable theme must be auto, dark or light'
    }
    if ($config['style'] -isnot [string] -or @('hud', 'eva', 'minimal') -cnotcontains $config['style']) {
        throw 'Portable style must be hud, eva or minimal'
    }
    $bounds = @{ offset_dip = @(0, 16000); column_dip = @(88, 116) }
    foreach ($key in $bounds.Keys) {
        $value = $config[$key]
        if (($value -isnot [int] -and $value -isnot [long]) -or
            $value -lt $bounds[$key][0] -or $value -gt $bounds[$key][1]) {
            throw "Portable $key must be an integer within the application range"
        }
    }
    $visible = $config['visible']
    if ($visible -isnot [array] -or $visible.Count -ne 6) {
        throw 'Portable visible setting must contain six booleans'
    }
    foreach ($value in $visible) {
        if ($value -isnot [bool]) { throw 'Portable visible setting must contain only booleans' }
    }
    if ($visible -notcontains $true) { throw 'Portable settings must show at least one metric' }
} finally {
    $zip.Dispose()
}

$expectedAssets = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
[void]$expectedAssets.Add($setupName)
[void]$expectedAssets.Add($portableName)
$seenAssets = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$checksumLines = @(Get-Content -LiteralPath (Join-Path $ReleaseDir 'SHA256SUMS.txt'))
if ($checksumLines.Count -ne 2) { throw 'Checksum manifest must contain exactly the Setup and Portable assets' }
foreach ($line in $checksumLines) {
    $match = [regex]::Match($line, '\A([A-Fa-f0-9]{64})  ([A-Za-z0-9._-]+)\z')
    if (-not $match.Success) { throw 'Malformed or empty checksum manifest line' }
    $asset = $match.Groups[2].Value
    if (-not $expectedAssets.Contains($asset)) { throw "Unexpected checksum asset: $asset" }
    if (-not $seenAssets.Add($asset)) { throw "Duplicate checksum asset: $asset" }
    $assetPath = Join-Path $ReleaseDir $asset
    if (-not (Test-Path -LiteralPath $assetPath -PathType Leaf)) { throw "Missing release asset: $asset" }
    if ((Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash -ne $match.Groups[1].Value) {
        throw "Release asset checksum mismatch: $asset"
    }
}
foreach ($asset in $expectedAssets) {
    if (-not $seenAssets.Contains($asset)) { throw "Missing checksum asset: $asset" }
}
Write-Output "Package invariants passed for $version ($licenseFileCount license files)."
