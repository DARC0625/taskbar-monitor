#requires -Version 7.2
<#
Run explicitly on an interactive Windows 11 PC, with no existing widget.
Only the copied candidate and its identity-targeted public control CLI are used.
Raw report.json may contain hardware descriptions; publish summary.json only.
This short GUI smoke test does not prove a 24-hour run, physical scanout latency,
pixel visibility, or the telemetry shutdown bound covered by unit tests.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Executable,
    [string]$OutputDir = 'target/widget-runtime-tests',
    [string]$PreferencesFile,
    [ValidateRange(30, 180)][int]$DurationSeconds = 45,
    [ValidateRange(0, 5)][int]$ReattachCount = 3
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or [Environment]::OSVersion.Version.Build -lt 22000) {
    throw 'This GUI smoke test requires an interactive Windows 11 client.'
}
$os = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
if ($os.InstallationType -cne 'Client' -or -not [Environment]::UserInteractive) {
    throw 'Windows Server and noninteractive sessions are not GUI compatibility targets.'
}
if (@(Get-Process -Name 'taskbar-monitor' -ErrorAction SilentlyContinue).Count -ne 0) {
    throw 'An existing taskbar-monitor process is present. No process was controlled or stopped.'
}
$source = (Resolve-Path -LiteralPath $Executable).ProviderPath
if (-not (Test-Path -LiteralPath $source -PathType Leaf) -or [IO.Path]::GetExtension($source) -ine '.exe') {
    throw 'Executable must name an existing candidate EXE.'
}
$sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
$preferences = $null
$preferencesHash = $null
if ($PreferencesFile) {
    $preferences = (Resolve-Path -LiteralPath $PreferencesFile).ProviderPath
    if ((Get-Item -LiteralPath $preferences).Length -gt 65536) { throw 'Preferences exceed 64 KiB.' }
    $config = Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json -NoEnumerate
    if ($config -isnot [pscustomobject]) { throw 'Preferences must be a JSON object.' }
    $preferencesHash = (Get-FileHash -LiteralPath $preferences -Algorithm SHA256).Hash
}
$outputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDir)
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$runDirectory = Join-Path $outputRoot ('run-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runDirectory | Out-Null
$candidate = Join-Path $runDirectory 'taskbar-monitor.exe'
$reportPath = Join-Path $runDirectory 'report.json'
$summaryPath = Join-Path $runDirectory 'summary.json'
Copy-Item -LiteralPath $source -Destination $candidate
Set-Content -LiteralPath (Join-Path $runDirectory 'portable.flag') -Value 'Isolated runtime smoke test.' -Encoding ascii
if ($preferences) { Copy-Item -LiteralPath $preferences -Destination (Join-Path $runDirectory 'widget.json') }
if ((Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash -ne $sourceHash) { throw 'Candidate copy hash mismatch.' }
[ordered]@{ schema_version = 1; passed = $false; status = 'not_completed' } |
    ConvertTo-Json | Set-Content -LiteralPath $summaryPath -Encoding utf8

$owned = $null
$ownedId = 0
$ownedStart = [long]0
$clock = [Diagnostics.Stopwatch]::StartNew()
$deadlineSeconds = $DurationSeconds + 20
$forcedCleanup = $false

function Assert-That([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Number($Value, [string]$Name, [switch]$Integer) {
    $numeric = $Value -is [int] -or $Value -is [long] -or $Value -is [double] -or $Value -is [decimal]
    Assert-That ($numeric -and [double]::IsFinite([double]$Value) -and $Value -ge 0) "$Name must be finite and nonnegative."
    if ($Integer) { Assert-That ($Value -is [int] -or $Value -is [long]) "$Name must be an integer." }
}
function Assert-Boolean($Value, [bool]$Expected, [string]$Name) {
    Assert-That ($Value -is [bool] -and $Value -eq $Expected) "Unexpected boolean: $Name."
}
function Assert-OwnedRunning {
    Assert-That ($null -ne $owned -and -not $owned.HasExited) 'The owned candidate exited before the requested action.'
    $current = Get-Process -Id $ownedId -ErrorAction Stop
    try {
        Assert-That ($current.StartTime.ToUniversalTime().ToFileTimeUtc() -eq $ownedStart) 'Candidate PID was reused; refusing control.'
        Assert-That ($current.Path -ieq $candidate) 'Candidate executable identity changed; refusing control.'
    } finally { $current.Dispose() }
}
function Assert-Exclusive([Diagnostics.Process]$Helper = $null) {
    foreach ($process in @(Get-Process -Name 'taskbar-monitor' -ErrorAction SilentlyContinue)) {
        try {
            if ($process.Id -eq $ownedId -and $process.StartTime.ToUniversalTime().ToFileTimeUtc() -eq $ownedStart) { continue }
            if ($null -ne $Helper -and -not $Helper.HasExited -and $process.Id -eq $Helper.Id -and
                $process.StartTime.ToUniversalTime().ToFileTimeUtc() -eq $Helper.StartTime.ToUniversalTime().ToFileTimeUtc()) { continue }
            throw 'A competing widget process appeared. No further control command will be sent.'
        } finally { $process.Dispose() }
    }
}
function Invoke-OwnedControl([ValidateSet('--reattach', '--report-now')][string]$Command) {
    Assert-OwnedRunning
    Assert-Exclusive
    Assert-That ($clock.Elapsed.TotalSeconds -lt $DurationSeconds - 8) 'Insufficient natural runtime remains for a control command.'
    # The app checks both fields before posting and at receipt, protecting against
    # another instance acquiring its singleton after an unexpected candidate exit.
    $arguments = @($Command, '--target-pid', [string]$ownedId, '--target-start-time', [string]$ownedStart)
    $helper = Start-Process -FilePath $candidate -ArgumentList $arguments -WorkingDirectory $runDirectory -WindowStyle Hidden -PassThru
    try {
        $null = $helper.SafeHandle
        $helperClock = [Diagnostics.Stopwatch]::StartNew()
        while (-not $helper.WaitForExit(100)) {
            Assert-OwnedRunning
            Assert-Exclusive -Helper $helper
            if ($helperClock.Elapsed.TotalSeconds -gt 5) {
                # Only this freshly started helper's retained process handle.
                $helper.Kill()
                throw 'The owned control helper exceeded five seconds.'
            }
        }
        Assert-That ($helper.ExitCode -eq 0) 'The identity-targeted control command was rejected.'
        Assert-OwnedRunning
        Assert-Exclusive
    } finally { $helper.Dispose() }
}
function Read-Report {
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { return $null }
    if ((Get-Item -LiteralPath $reportPath).Length -gt 4MB) { throw 'Unexpectedly large bounded diagnostic report.' }
    # --report-now writes synchronously, but this reader may see an incomplete write.
    try {
        $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json -NoEnumerate
    } catch { return $null }
    if ($report -isnot [pscustomobject]) { throw 'Diagnostic report must be a JSON object.' }
    return $report
}
function Wait-Report([double]$AfterElapsed, [long]$ExpectedAttachments) {
    $wait = [Diagnostics.Stopwatch]::StartNew()
    while ($wait.Elapsed.TotalSeconds -lt 6 -and $clock.Elapsed.TotalSeconds -lt $deadlineSeconds) {
        Assert-OwnedRunning
        Assert-Exclusive
        $report = Read-Report
        if ($null -ne $report -and $report.elapsed_seconds -gt $AfterElapsed -and
            $report.attach_count -ge $ExpectedAttachments -and $report.report_phase -ceq 'on_demand') {
            return $report
        }
        Start-Sleep -Milliseconds 100
    }
    throw 'No fresh report acknowledged the expected attachment count.'
}
function Assert-Report($Report, [long]$ExpectedAttachments, [string]$Phase) {
    Assert-That ($Report.schema_version -eq 2 -and $Report.mode -ceq 'live_widget_diagnostics') 'Unexpected diagnostic schema or mode.'
    Assert-That ($Report.report_phase -ceq $Phase -and $Report.hosting -ceq 'taskbar-child') 'Unexpected report phase or hosting mode.'
    Assert-Number $Report.elapsed_seconds 'elapsed_seconds'
    Assert-That ($Report.draw_errors -eq 0 -and $Report.attach_error -ceq '') 'Drawing or attachment reported an error.'
    Assert-That ($Report.attach_count -eq $ExpectedAttachments -and $Report.successful_reattachments -eq $ExpectedAttachments - 1) 'Unexpected successful attachment count.'
    $hostReport = $Report.last_verified_host
    foreach ($key in @('parent_is_taskbar', 'root_is_taskbar', 'ws_child', 'dpi_contexts_equal')) {
        Assert-Boolean $hostReport.$key $true "host.$key"
    }
    foreach ($key in @('ws_popup', 'ws_ex_topmost')) { Assert-Boolean $hostReport.$key $false "host.$key" }
    Assert-That ($hostReport.child_process_id -eq $ownedId) 'Report belongs to a different widget process.'
    Assert-That ($hostReport.parent_hwnd -gt 0 -and $hostReport.parent_hwnd -eq $hostReport.taskbar_hwnd) 'Parent HWND is not the reported taskbar.'
    Assert-That ($hostReport.host_process_id -gt 0 -and $hostReport.host_process_id -ne $ownedId) 'Invalid taskbar host process.'
    Assert-That ($hostReport.child_dpi -gt 0 -and $hostReport.child_dpi -eq $hostReport.host_dpi) 'Child/taskbar DPI mismatch.'
    Assert-Number $hostReport.observed_at_uptime_seconds 'host observation time'
    Assert-That ($hostReport.observed_at_uptime_seconds -le $Report.elapsed_seconds) 'Host observation is in the future.'
    $childRect = $hostReport.child_screen_rect
    $hostRect = $hostReport.host_screen_rect
    Assert-That ($childRect -is [array] -and $childRect.Count -eq 4 -and $hostRect -is [array] -and $hostRect.Count -eq 4) 'Missing host/child rectangles.'
    Assert-That ($childRect[0] -lt $childRect[2] -and $childRect[1] -lt $childRect[3] -and
        $childRect[0] -ge $hostRect[0] -and $childRect[1] -ge $hostRect[1] -and
        $childRect[2] -le $hostRect[2] -and $childRect[3] -le $hostRect[3]) 'Child rectangle lies outside its taskbar parent.'
    foreach ($key in @('widget_exists', 'host_is_current', 'parent_is_expected_taskbar', 'renderer_available')) {
        Assert-Boolean $Report.widget_state.$key $true "widget_state.$key"
    }
    Assert-Boolean $Report.widget_state.shutting_down $false 'widget_state.shutting_down'
    # Auto-hide and occlusion are not manipulated or inferred from IsWindowVisible.
    Assert-That ($Report.widget_state.win32_visible -is [bool]) 'Missing Win32 visibility state.'
    $lifecycle = $Report.lifecycle
    foreach ($key in @('attachment_create_errors', 'externally_destroyed_widgets', 'renderer_init_errors', 'resize_errors', 'paint_errors')) {
        Assert-Number $lifecycle.$key "lifecycle.$key" -Integer
        Assert-That ($lifecycle.$key -eq 0) "Unexpected lifecycle event: $key."
    }
    Assert-That ($lifecycle.attachment_create_attempts -eq $ExpectedAttachments -and $lifecycle.renderer_instances_created -eq $ExpectedAttachments) 'Renderer/attachment instance counters disagree.'
    Assert-Number $Report.paints 'paints' -Integer
    $renderer = $Report.renderer_stats
    foreach ($key in @('frames_presented', 'text_layouts_created', 'text_layouts_reused', 'path_geometries_created', 'path_geometries_reused', 'tick_sets_created', 'tick_sets_reused')) {
        Assert-Number $renderer.$key "renderer.$key" -Integer
    }
    Assert-That ($renderer.frames_presented -gt 0 -and $renderer.frames_presented -eq $Report.paints) 'Successful paints and cumulative presentations disagree.'
    $latency = $Report.cpu_sample_to_present_return_ms
    foreach ($key in @('count', 'capacity', 'total_samples', 'evicted_samples')) { Assert-Number $latency.$key "latency.$key" -Integer }
    Assert-That ($latency.capacity -eq 1200 -and $latency.count -gt 0 -and $latency.count -le $latency.capacity) 'Invalid latency buffer bounds.'
    Assert-That ($latency.total_samples -eq $latency.count + $latency.evicted_samples -and $latency.total_samples -le $Report.paints) 'Invalid latency accounting.'
    foreach ($key in @('p50', 'p95', 'max', 'recent_start_uptime_seconds', 'recent_end_uptime_seconds')) { Assert-Number $latency.$key "latency.$key" }
    Assert-That ($latency.p50 -le $latency.p95 -and $latency.p95 -le $latency.max) 'Invalid latency percentile order.'
    Assert-That ($latency.recent_start_uptime_seconds -le $latency.recent_end_uptime_seconds -and $latency.recent_end_uptime_seconds -le $Report.elapsed_seconds) 'Invalid recent latency coverage.'
    Assert-That ($latency.all_observed_samples.valid_samples -eq $latency.total_samples -and $latency.all_observed_samples.max -ge $latency.max) 'Lifetime latency range lost previous observations.'
    Assert-Boolean $latency.does_not_measure_unpainted_samples_or_physical_scanout $true 'latency caveat'
    $resources = $Report.process_resources
    foreach ($key in @('samples_total', 'recent_capacity', 'samples_evicted', 'samples_with_errors', 'logical_processors_for_cpu_normalization')) { Assert-Number $resources.$key "resources.$key" -Integer }
    Assert-That ($resources.recent_samples -is [array]) 'Resource samples must be an array.'
    $samples = @($resources.recent_samples)
    Assert-That ($resources.recent_capacity -eq 720 -and $samples.Count -gt 0 -and $samples.Count -le 720) 'Invalid resource buffer bounds.'
    Assert-That ($resources.samples_total -eq $samples.Count + $resources.samples_evicted -and $resources.samples_with_errors -eq 0) 'Invalid resource accounting or API error.'
    Assert-That ($resources.nominal_sample_interval_seconds -eq 5 -and $resources.logical_processors_for_cpu_normalization -gt 0) 'Invalid process sampling configuration.'
    foreach ($key in @('start_uptime_seconds', 'end_uptime_seconds', 'wall_seconds', 'cpu_seconds', 'average_percent_all_logical_processors')) {
        Assert-Number $resources.cpu_measured_interval.$key "CPU coverage.$key"
    }
    Assert-That ($resources.cpu_measured_interval.wall_seconds -gt 0 -and
        $resources.cpu_measured_interval.start_uptime_seconds -le $resources.cpu_measured_interval.end_uptime_seconds -and
        $resources.cpu_measured_interval.end_uptime_seconds -le $Report.elapsed_seconds) 'Invalid CPU coverage interval.'
    $previousAt = -1.0
    foreach ($sample in $samples) {
        Assert-Number $sample.uptime_seconds 'resource sample time'
        Assert-That ($sample.uptime_seconds -ge $previousAt -and $sample.uptime_seconds -le $Report.elapsed_seconds) 'Resource times are not ordered within uptime.'
        $previousAt = $sample.uptime_seconds
        foreach ($key in @('private_bytes', 'working_set_bytes', 'handles', 'gdi_objects', 'user_objects')) { Assert-Number $sample.$key "sample.$key" -Integer }
        Assert-Number $sample.process_cpu_seconds_since_process_start 'process CPU time'
        if ($null -ne $sample.cpu_percent_all_logical_processors) {
            Assert-Number $sample.cpu_percent_all_logical_processors 'interval CPU percent'
            Assert-Number $sample.cpu_interval_seconds 'CPU interval'
            Assert-That ($sample.cpu_interval_seconds -gt 0) 'A CPU sample lacks a positive interval.'
        }
        Assert-That ($sample.errors -is [array] -and $sample.errors.Count -eq 0) 'A resource API returned an error.'
    }
    Assert-That ($resources.recent_start_uptime_seconds -eq $samples[0].uptime_seconds -and $resources.recent_end_uptime_seconds -eq $samples[-1].uptime_seconds) 'Resource coverage does not match retained samples.'
    foreach ($key in @('private_bytes', 'working_set_bytes', 'handles', 'gdi_objects', 'user_objects')) {
        $range = $resources.all_observed_sample_ranges.$key
        Assert-That ($range.valid_samples -eq $resources.samples_total -and $range.missing_samples -eq 0) "Missing lifetime resource samples: $key."
        Assert-That ($range.min -le $range.max -and $range.first -ge $range.min -and $range.first -le $range.max -and $range.last -ge $range.min -and $range.last -le $range.max) "Invalid lifetime resource range: $key."
        foreach ($sample in $samples) { Assert-That ($sample.$key -ge $range.min -and $sample.$key -le $range.max) "Recent $key is outside its lifetime range." }
    }
}

try {
    # Repeat the no-instance check immediately before launch; later commands use
    # the retained process identity and are never sent with legacy/global targeting.
    Assert-Exclusive
    $owned = Start-Process -FilePath $candidate -ArgumentList @('--seconds', [string]$DurationSeconds, '--report', ('"' + $reportPath + '"')) -WorkingDirectory $runDirectory -WindowStyle Hidden -PassThru
    $null = $owned.SafeHandle
    $ownedId = $owned.Id
    $ownedStart = $owned.StartTime.ToUniversalTime().ToFileTimeUtc()
    Assert-OwnedRunning
    # Allow initial telemetry and HWND creation; do not drive the desktop UI.
    $warmup = [Diagnostics.Stopwatch]::StartNew()
    while ($warmup.Elapsed.TotalSeconds -lt 3) {
        Assert-OwnedRunning
        Assert-Exclusive
        Start-Sleep -Milliseconds 100
    }
    Invoke-OwnedControl '--report-now'
    $latest = Wait-Report -AfterElapsed -1 -ExpectedAttachments 1
    Assert-Report $latest 1 'on_demand'
    $baselineElapsed = [double]$latest.elapsed_seconds
    for ($iteration = 1; $iteration -le $ReattachCount; $iteration++) {
        Invoke-OwnedControl '--reattach'
        # Let the normal 500 ms maintenance timer verify the new parent/DPI.
        $settle = [Diagnostics.Stopwatch]::StartNew()
        while ($settle.Elapsed.TotalSeconds -lt 1) {
            Assert-OwnedRunning
            Assert-Exclusive
            Start-Sleep -Milliseconds 100
        }
        Invoke-OwnedControl '--report-now'
        $latest = Wait-Report -AfterElapsed $latest.elapsed_seconds -ExpectedAttachments ($iteration + 1)
        Assert-Report $latest ($iteration + 1) 'on_demand'
    }
    # The candidate closes itself through --seconds. No --quit is ever invoked.
    while (-not $owned.WaitForExit(200)) {
        Assert-Exclusive
        if ($clock.Elapsed.TotalSeconds -ge $deadlineSeconds) { throw 'Candidate did not exit within the smoke-test deadline.' }
    }
    Assert-That ($owned.ExitCode -eq 0) 'Candidate exited unsuccessfully.'
    Assert-Exclusive
    $final = Read-Report
    Assert-That ($null -ne $final) 'Candidate did not write its final report.'
    Assert-Report $final ($ReattachCount + 1) 'before_shutdown'
    Assert-That ($final.elapsed_seconds -ge $DurationSeconds -and $final.elapsed_seconds -le $deadlineSeconds) 'Final report does not cover the requested short runtime.'
    Assert-That ($final.elapsed_seconds -gt $baselineElapsed -and $final.process_resources.samples_total -ge 3) 'Runtime/resource coverage did not advance.'
    if ($preferences) {
        Assert-That ((Get-FileHash -LiteralPath $preferences -Algorithm SHA256).Hash -eq $preferencesHash) 'Source preferences changed during the test.'
        Assert-That ((Get-FileHash -LiteralPath (Join-Path $runDirectory 'widget.json') -Algorithm SHA256).Hash -eq $preferencesHash) 'Copied preferences changed during the test.'
    }
    [ordered]@{
        schema_version = 1; passed = $true; status = 'completed'; version = $final.version
        executable_sha256 = $sourceHash; requested_duration_seconds = $DurationSeconds
        observed_uptime_seconds = $final.elapsed_seconds; requested_reattachments = $ReattachCount
        attach_count = $final.attach_count; paints = $final.paints; draw_errors = $final.draw_errors
        latency_samples_total = $final.cpu_sample_to_present_return_ms.total_samples
        resource_samples_total = $final.process_resources.samples_total
        natural_exit = $true; scope = 'short live-widget smoke test; not a soak, scanout, or telemetry shutdown-bound test'
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $summaryPath -Encoding utf8
    Write-Output "Widget runtime smoke test passed. Summary: $summaryPath"
} finally {
    if ($null -ne $owned) {
        # On failure, allow the already requested natural exit until the same
        # deadline. Never find/stop an unrelated process or invoke global --quit.
        while (-not $owned.HasExited -and $clock.Elapsed.TotalSeconds -lt $deadlineSeconds) {
            [void]$owned.WaitForExit(200)
        }
        if (-not $owned.HasExited) {
            Assert-OwnedRunning
            $owned.Kill() # Retained handle from our own Start-Process, not a PID/name kill.
            [void]$owned.WaitForExit(5000)
            $forcedCleanup = $true
        }
        $owned.Dispose()
        if ($forcedCleanup) { throw 'Runtime smoke test failed; only its owned candidate required timeout cleanup.' }
    }
}
