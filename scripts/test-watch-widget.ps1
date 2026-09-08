#requires -Version 7.2
param([string]$OutputRoot='target/watch-widget-tests')
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$watchPath=Join-Path $PSScriptRoot 'watch-widget.ps1'
$testRoot=Join-Path $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot) ('tests-'+[guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$passed=0
function Assert-SoakTest([bool]$Condition,[string]$Description){
    if(-not $Condition){throw $Description}
    $script:passed++
}

# Import only helper definitions; parsing the observer must not start a monitor.
$parseTokens=$null;$parseErrors=$null
$tree=[Management.Automation.Language.Parser]::ParseFile($watchPath,[ref]$parseTokens,[ref]$parseErrors)
if($parseErrors.Count){throw 'Observer PowerShell syntax is invalid'}
$wanted=@('Read-SoakProcess','Get-SoakCpuInterval','New-SoakSeries','Add-SoakSeries','Get-SoakSeriesSummary','Add-SoakRecord')
foreach($definition in $tree.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst]},$false)){
    if($definition.Name -in $wanted){. ([scriptblock]::Create($definition.Extent.Text))}
}
$previous=@{elapsed_seconds=0.0;utc_ticks=0L;total_cpu_seconds=10.0}
$current=@{elapsed_seconds=2.0;utc_ticks=2*[TimeSpan]::TicksPerSecond;total_cpu_seconds=14.0}
$cpu=Get-SoakCpuInterval $previous $current 2 3 4
Assert-SoakTest ($cpu.state -eq 'valid' -and $cpu.percent -eq 50) 'CPU must normalize across all four logical processors'
$gap=Get-SoakCpuInterval $previous @{elapsed_seconds=20.0;utc_ticks=20*[TimeSpan]::TicksPerSecond;total_cpu_seconds=14.0} 2 3 4
Assert-SoakTest ($gap.state -eq 'observation_gap' -and $null -eq $gap.percent) 'Unobserved intervals must not dilute CPU statistics'
$reset=Get-SoakCpuInterval $previous @{elapsed_seconds=2.0;utc_ticks=2*[TimeSpan]::TicksPerSecond;total_cpu_seconds=9.0} 2 3 4
Assert-SoakTest ($reset.state -eq 'invalid_cpu_counter' -and $null -eq $reset.percent) 'Regressing CPU counters must not become real zero usage'
$clock=Get-SoakCpuInterval $previous @{elapsed_seconds=2.0;utc_ticks=-1L;total_cpu_seconds=14.0} 2 3 4
Assert-SoakTest ($clock.state -eq 'clock_discontinuity') 'Wall clock reversal must be identifiable'
$unknown=Get-SoakCpuInterval $previous $current 2 3 0
Assert-SoakTest ($unknown.state -eq 'denominator_unavailable' -and $null -eq $unknown.percent) 'Unknown OS processor count must not produce guessed CPU usage'
$series=New-SoakSeries
Add-SoakSeries $series 0 100
Add-SoakSeries $series 10 110
Add-SoakSeries $series 20 120
$trend=Get-SoakSeriesSummary $series
Assert-SoakTest ($trend.sample_count -eq 3 -and $trend.change -eq 20 -and $trend.linear_slope_per_hour -eq 3600) 'Memory trend must preserve sample count, first/last change and observed slope'
$memoryLog=@{encoding=[Text.UTF8Encoding]::new($false);stream=[IO.MemoryStream]::new();records=0;bytes=0L;max_records=1;max_bytes=1024}
try {
    $firstRecord=Add-SoakRecord $memoryLog @{kind='sample'}
    $secondRecord=Add-SoakRecord $memoryLog @{kind='sample'}
    Assert-SoakTest ($firstRecord -and -not $secondRecord -and $memoryLog.records -eq 1) 'The record-count cap must stop appending even before the byte cap is reached'
} finally {$memoryLog.stream.Dispose()}

$shellPath=(Get-Process -Id $PID).Path
# Only these owned, finite-lived sleeping processes are used. No widget lookup,
# window interaction, suspend request or termination call is made by this test.
# Four observer starts can each spend up to five seconds obtaining the OS CPU
# count. Keep this child alive through those calls and all bounded observations.
$child=Start-Process -FilePath $shellPath -ArgumentList @('-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 45') -WindowStyle Hidden -PassThru
try {
    $identity=Read-SoakProcess $child.Id 0
    Assert-SoakTest ($identity.state -eq 'pid_reused') 'The same PID with a different start time must be rejected'
    $wrongIdentity=& $watchPath -TargetProcessId $child.Id -ExpectedStartTimeUtc ([DateTimeOffset]::UnixEpoch) -StopAfterSeconds 7 -IntervalSeconds 0.25 -OutputRoot $testRoot
    Assert-SoakTest ($wrongIdentity.Reason -eq 'pid_reused' -and $wrongIdentity.Samples -eq 0) 'A caller-bound identity mismatch must stop before the first sample'
    $childStartUtc=$child.StartTime.ToUniversalTime().ToString('o')
    # The observer's time limit includes its initial, bounded CIM metadata read.
    $first=& $watchPath -TargetProcessId $child.Id -ExpectedStartTimeUtc $childStartUtc -StopAfterSeconds 8 -IntervalSeconds 0.25 -SummaryIntervalSeconds 1 -OutputRoot $testRoot
    $summary=Get-Content -LiteralPath $first.SummaryPath -Raw | ConvertFrom-Json
    Assert-SoakTest ($first.Reason -eq 'time_limit' -and $summary.sample_count -ge 2) 'A bounded observer must capture samples and stop at its time limit'
    $records=@(Get-Content -LiteralPath $first.LogPath | ForEach-Object {$_ | ConvertFrom-Json})
    Assert-SoakTest ($records.Count -eq $summary.record_count) 'JSONL and summary record counts must agree'
    $cpuRecords=@($records | Where-Object {$_.kind -eq 'sample' -and $_.cpu_interval_state -eq 'valid'})
    Assert-SoakTest ($cpuRecords.Count -eq $summary.cpu.sample_count) 'CPU statistics must report their actual usable sample count'
    if($cpuRecords.Count){
        $weight=0.0;$total=0.0
        foreach($record in $cpuRecords){$weight+=$record.interval_seconds;$total+=$record.cpu_percent*$record.interval_seconds}
        Assert-SoakTest ([Math]::Abs($summary.cpu.mean_percent-$total/$weight) -lt 0.000001) 'CPU mean must use elapsed-interval weights'
    }
    $publicText=Get-Content -LiteralPath $first.SummaryPath -Raw
    Assert-SoakTest ($publicText -notmatch '"(pid|target_process_id|process_name|process_path|start_ticks)"' -and -not $publicText.Contains($testRoot) -and -not $publicText.Contains($shellPath)) 'The shareable summary must not disclose process IDs, names or local paths'
    $originalHash=(Get-FileHash -LiteralPath $first.LogPath).Hash
    $second=& $watchPath -TargetProcessId $child.Id -StopAfterSeconds 7 -IntervalSeconds 0.25 -OutputRoot $testRoot
    Assert-SoakTest ($second.RunDirectory -ne $first.RunDirectory -and (Get-FileHash -LiteralPath $first.LogPath).Hash -eq $originalHash) 'Repeated observations must use new directories without altering old records'
    $limited=& $watchPath -TargetProcessId $child.Id -StopAfterSeconds 8 -IntervalSeconds 0.25 -MaxLogBytes 1024 -OutputRoot $testRoot
    Assert-SoakTest ($limited.Reason -eq 'output_limit' -and (Get-Item -LiteralPath $limited.LogPath).Length -le 1024) 'Reaching the output limit must stop without overflowing the log'
    Assert-SoakTest ($null -eq (Get-ChildItem -LiteralPath $testRoot -Recurse -Filter '*.tmp' | Select-Object -First 1)) 'Completed atomic summary updates must not leave temporary files'
    if(-not $child.WaitForExit(50000)){throw 'The finite-lived test process did not exit in its allotted time'}
} finally {$child.Dispose()}

$ending=Start-Process -FilePath $shellPath -ArgumentList @('-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 2') -WindowStyle Hidden -PassThru
try {
    $ended=& $watchPath -TargetProcessId $ending.Id -StopAfterSeconds 7 -IntervalSeconds 0.25 -OutputRoot $testRoot
    Assert-SoakTest ($ended.Reason -eq 'target_exited' -and $ended.Status -eq 'stopped') 'A disappearing target must be distinguished from sample errors and the time limit'
    if(-not $ending.WaitForExit(5000)){throw 'The finite-lived exit-test process did not finish'}
} finally {$ending.Dispose()}

[ordered]@{schema_version=1;passed=$true;assertions=$passed;mode='finite_owned_processes_and_deterministic_helpers'} |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $testRoot 'test-summary.json') -Encoding utf8
Write-Output "Read-only soak observer tests passed ($passed assertions)."
