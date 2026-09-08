#requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateRange(1,2147483647)][int]$TargetProcessId,
    [Nullable[DateTimeOffset]]$ExpectedStartTimeUtc,
    [ValidateRange(1,604800)][int]$StopAfterSeconds=86400,
    [ValidateRange(0.25,300)][double]$IntervalSeconds=5,
    [string]$OutputRoot='target/soak-tests',
    [ValidateRange(1024,1073741824)][long]$MaxLogBytes=33554432,
    [ValidateRange(1,200000)][int]$MaxRecords=100000,
    [ValidateRange(1,3600)][int]$SummaryIntervalSeconds=60,
    [ValidateRange(1.5,100)][double]$GapMultiplier=3,
    [ValidateRange(1,100)][int]$MaxConsecutiveSampleErrors=5
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
if(-not $IsWindows){throw 'This observer requires Windows and PowerShell 7.2 or later.'}

function Read-SoakProcess([int]$ProcessNumber,[Nullable[long]]$ExpectedStartTicks) {
    $observed=$null
    try {
        $observed=[Diagnostics.Process]::GetProcessById($ProcessNumber)
        $started=$observed.StartTime.ToUniversalTime().Ticks
        if($null -ne $ExpectedStartTicks -and $started -ne $ExpectedStartTicks){
            return @{state='pid_reused';start_ticks=$started}
        }
        $result=@{
            state='sample';start_ticks=$started
            total_cpu_seconds=$observed.TotalProcessorTime.TotalSeconds
            private_bytes=$observed.PrivateMemorySize64
            working_set_bytes=$observed.WorkingSet64
            handles=$observed.HandleCount
            threads=$observed.Threads.Count
        }
        if($observed.HasExited){return @{state='target_exited'}}
        return $result
    } catch [ArgumentException] {
        return @{state='target_exited'}
    } catch [InvalidOperationException] {
        return @{state='target_exited'}
    } catch {
        # Do not copy exception messages: Windows errors can contain process IDs
        # and local paths. A read failure is distinct from a confirmed exit.
        return @{state='sample_error';error_type=$_.Exception.GetType().Name}
    } finally {
        if($null -ne $observed){$observed.Dispose()}
    }
}

function Get-SoakCpuInterval($Previous,$Current,[double]$Interval,[double]$GapFactor,[int]$LogicalProcessors) {
    if($null -eq $Previous){return @{state='baseline';percent=$null;seconds=0;cpu_seconds=0}}
    $delta=$Current.elapsed_seconds-$Previous.elapsed_seconds
    $wallDelta=($Current.utc_ticks-$Previous.utc_ticks)/[double][TimeSpan]::TicksPerSecond
    if($delta -le 0 -or $wallDelta -lt 0){
        return @{state='clock_discontinuity';percent=$null;seconds=$delta;cpu_seconds=0}
    }
    if([Math]::Max($delta,$wallDelta) -gt ($Interval*$GapFactor)){
        return @{state='observation_gap';percent=$null;seconds=[Math]::Max($delta,$wallDelta);cpu_seconds=0}
    }
    if($LogicalProcessors -le 0){return @{state='denominator_unavailable';percent=$null;seconds=$delta;cpu_seconds=0}}
    $cpuDelta=$Current.total_cpu_seconds-$Previous.total_cpu_seconds
    $percent=100.0*$cpuDelta/$delta/$LogicalProcessors
    if(-not [double]::IsFinite($percent) -or $percent -lt 0 -or $percent -gt 100){
        return @{state='invalid_cpu_counter';percent=$null;seconds=$delta;cpu_seconds=0}
    }
    return @{state='valid';percent=$percent;seconds=$delta;cpu_seconds=$cpuDelta}
}

function New-SoakSeries {
    return @{count=0;first=$null;last=$null;min=$null;max=$null;first_seconds=0.0;last_seconds=0.0;mean_seconds=0.0;mean_value=0.0;sxx=0.0;sxy=0.0}
}

function Add-SoakSeries($Series,[double]$Seconds,[long]$Value) {
    $Series.count++
    if($Series.count -eq 1){$Series.first=$Value;$Series.first_seconds=$Seconds;$Series.min=$Value;$Series.max=$Value}
    $Series.last=$Value;$Series.last_seconds=$Seconds
    $Series.min=[Math]::Min([long]$Series.min,$Value);$Series.max=[Math]::Max([long]$Series.max,$Value)
    # Online covariance avoids retaining every memory sample for a long run.
    $dx=$Seconds-$Series.mean_seconds
    $dy=$Value-$Series.mean_value
    $Series.mean_seconds+=$dx/$Series.count
    $Series.mean_value+=$dy/$Series.count
    $Series.sxx+=$dx*($Seconds-$Series.mean_seconds)
    $Series.sxy+=$dx*($Value-$Series.mean_value)
}

function Get-SoakSeriesSummary($Series) {
    return [ordered]@{
        sample_count=$Series.count;first=$Series.first;last=$Series.last;minimum=$Series.min;maximum=$Series.max
        change=if($Series.count){$Series.last-$Series.first}else{$null}
        observed_span_seconds=if($Series.count){$Series.last_seconds-$Series.first_seconds}else{0}
        linear_slope_per_hour=if($Series.count -ge 3 -and $Series.sxx -gt 0){3600*$Series.sxy/$Series.sxx}else{$null}
    }
}

function Write-SoakSummary([string]$Destination,$Summary) {
    $temporary=$Destination+'.'+[guid]::NewGuid().ToString('N')+'.tmp'
    $content=[Text.UTF8Encoding]::new($false).GetBytes(($Summary | ConvertTo-Json -Depth 8))
    if($content.Length -gt 65536){throw 'Summary size limit exceeded'}
    try {
        $writer=[IO.FileStream]::new($temporary,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
        try {$writer.Write($content,0,$content.Length);$writer.Flush($true)}finally{$writer.Dispose()}
        # A reader can briefly hold the old file without sharing delete access.
        # Retry the same atomic rename for a bounded interval; never truncate it.
        for($attempt=0;$attempt -lt 5;$attempt++){
            try {[IO.File]::Move($temporary,$Destination,$true);break}
            catch [IO.IOException] {if($attempt -eq 4){throw};Start-Sleep -Milliseconds 50}
        }
    } finally {
        if([IO.File]::Exists($temporary)){[IO.File]::Delete($temporary)}
    }
}

function Add-SoakRecord($Session,$Record) {
    $bytes=$Session.encoding.GetBytes(($Record | ConvertTo-Json -Compress -Depth 5)+[Environment]::NewLine)
    if($Session.records -ge $Session.max_records -or $Session.bytes+$bytes.Length -gt $Session.max_bytes){return $false}
    $Session.stream.Write($bytes,0,$bytes.Length)
    $Session.stream.Flush()
    $Session.bytes+=$bytes.Length;$Session.records++
    return $true
}

function Get-SoakSummary($Session,$Statistics,[string]$State,[string]$Reason,[double]$Elapsed,[DateTime]$StartedUtc,[int]$LogicalProcessors) {
    $orderedCpu=$Statistics.cpu.ToArray()
    [Array]::Sort($orderedCpu)
    $p95=if($orderedCpu.Length){$orderedCpu[[int][Math]::Ceiling($orderedCpu.Length*0.95)-1]}else{$null}
    return [ordered]@{
        schema_version=1;mode='read_only_process_soak';status=$State;completion_reason=$Reason
        started_utc=$StartedUtc.ToString('o');updated_utc=[DateTime]::UtcNow.ToString('o')
        requested_duration_seconds=$StopAfterSeconds;elapsed_seconds=$Elapsed
        wall_elapsed_seconds=([DateTime]::UtcNow-$StartedUtc).TotalSeconds
        duration_clock='maximum_of_monotonic_and_utc_elapsed'
        interval_seconds=$IntervalSeconds;sample_count=$Statistics.samples;record_count=$Session.records;log_bytes=$Session.bytes
        limits=@{max_log_bytes=$MaxLogBytes;max_records=$MaxRecords;max_summary_bytes=65536}
        identity_checked_by='process_id_and_start_time';identity_prebound=($null -ne $ExpectedStartTimeUtc);observed_process_identity_disclosed=$false
        sample_error_count=$Statistics.errors;observation_gap_count=$Statistics.gaps;observation_gap_seconds=$Statistics.gap_seconds
        clock_discontinuity_count=$Statistics.clock_errors;invalid_cpu_interval_count=$Statistics.invalid_cpu
        cpu=[ordered]@{
            normalization='all_os_logical_processors';logical_processor_count=if($LogicalProcessors -gt 0){$LogicalProcessors}else{$null}
            sample_count=$orderedCpu.Length;observed_interval_seconds=$Statistics.cpu_seconds
            mean_percent=if($Statistics.cpu_seconds -gt 0){100*$Statistics.cpu_work/$Statistics.cpu_seconds/$LogicalProcessors}else{$null}
            first_percent=if($Statistics.cpu.Count){$Statistics.cpu[0]}else{$null}
            last_percent=if($Statistics.cpu.Count){$Statistics.cpu[$Statistics.cpu.Count-1]}else{$null}
            p95_percent=$p95;maximum_percent=if($orderedCpu.Length){$orderedCpu[-1]}else{$null}
            percentile_method='nearest_rank';mean_method='interval_duration_weighted';gaps_excluded=$true
        }
        private_bytes=Get-SoakSeriesSummary $Statistics.private
        working_set_bytes=Get-SoakSeriesSummary $Statistics.working
        handles=Get-SoakSeriesSummary $Statistics.handles
        threads=Get-SoakSeriesSummary $Statistics.threads
        interpretation=@{
            thresholds_applied=$false
            gap_cause='suspend_or_scheduler_delay_not_confirmed'
            trend='linear_regression_of_observed_samples_not_a_leak_diagnosis'
            completeness='gaps_and_sample_errors_are_not_continuous_observation'
            checkpoint='running_is_an_intermediate_checkpoint_not_a_completed_run'
        }
    }
}

$outputDirectory=$ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
[IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
$runDirectory=Join-Path $outputDirectory ('run-'+[DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')+'-'+[guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($runDirectory) | Out-Null
$summaryPath=Join-Path $runDirectory 'summary.json'
$logPath=Join-Path $runDirectory 'observations.jsonl'
$startedUtc=[DateTime]::UtcNow
$elapsedClock=[Diagnostics.Stopwatch]::StartNew()
$session=@{
    encoding=[Text.UTF8Encoding]::new($false)
    stream=[IO.FileStream]::new($logPath,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::Read)
    records=0;bytes=0L;max_records=$MaxRecords;max_bytes=$MaxLogBytes
}
$statistics=@{
    samples=0;errors=0;gaps=0;gap_seconds=0.0;clock_errors=0;invalid_cpu=0
    cpu=[Collections.Generic.List[double]]::new();cpu_seconds=0.0;cpu_work=0.0
    private=New-SoakSeries;working=New-SoakSeries;handles=New-SoakSeries;threads=New-SoakSeries
}
$logicalProcessors=0
$expectedStart=if($null -ne $ExpectedStartTimeUtc){$ExpectedStartTimeUtc.UtcDateTime.Ticks}else{$null}
$previous=$null;$consecutiveErrors=0;$lastSummary=0.0
$lastObservationElapsed=$null;$lastObservationUtcTicks=0L
$reason='observer_interrupted';$status='interrupted'
try {
    Write-SoakSummary $summaryPath (Get-SoakSummary $session $statistics 'running' 'starting' 0 $startedUtc 0)
    try {
        # Unlike Environment.ProcessorCount, this is the OS-wide count, not a
        # count limited by this observer's CPU affinity or its job object.
        $logicalProcessors=[int](Get-CimInstance -ClassName Win32_ComputerSystem -Property NumberOfLogicalProcessors -OperationTimeoutSec 5).NumberOfLogicalProcessors
    } catch {$logicalProcessors=0}
    if($logicalProcessors -le 0){
        [void](Add-SoakRecord $session @{kind='cpu_unavailable';reason='logical_processor_count_unavailable';elapsed_seconds=$elapsedClock.Elapsed.TotalSeconds})
    }
    while($true) {
        $elapsed=$elapsedClock.Elapsed.TotalSeconds
        $observationUtc=[DateTime]::UtcNow
        # Detect gaps independently of successful process reads, including a
        # resume after the duration cap or a gap followed by target termination.
        if($null -ne $lastObservationElapsed){
            $observationDelta=$elapsed-$lastObservationElapsed
            $wallDelta=($observationUtc.Ticks-$lastObservationUtcTicks)/[double][TimeSpan]::TicksPerSecond
            $gap=[Math]::Max($observationDelta,$wallDelta)
            if($gap -gt $IntervalSeconds*$GapMultiplier){
                $event=@{kind='observation_gap';utc=$observationUtc.ToString('o');elapsed_seconds=$elapsed;unobserved_interval_seconds=$gap;cause='suspend_or_scheduler_delay_not_confirmed'}
                if(-not (Add-SoakRecord $session $event)){$status='stopped';$reason='output_limit';break}
                $statistics.gaps++;$statistics.gap_seconds+=$gap
            } elseif($wallDelta -lt 0 -or $observationDelta -lt 0){
                if(-not (Add-SoakRecord $session @{kind='clock_discontinuity';utc=$observationUtc.ToString('o');elapsed_seconds=$elapsed})){$status='stopped';$reason='output_limit';break}
                $statistics.clock_errors++
            }
        }
        $lastObservationElapsed=$elapsed;$lastObservationUtcTicks=$observationUtc.Ticks
        if([Math]::Max($elapsed,($observationUtc-$startedUtc).TotalSeconds) -ge $StopAfterSeconds){$status='completed';$reason='time_limit';break}
        $snapshot=Read-SoakProcess $TargetProcessId $expectedStart
        $now=[DateTime]::UtcNow
        $elapsed=$elapsedClock.Elapsed.TotalSeconds
        $record=[ordered]@{kind=$snapshot.state;utc=$now.ToString('o');elapsed_seconds=$elapsed}
        if($snapshot.state -in @('target_exited','pid_reused')){
            [void](Add-SoakRecord $session $record)
            $status='stopped';$reason=$snapshot.state;break
        }
        if($snapshot.state -eq 'sample_error'){
            $statistics.errors++;$consecutiveErrors++;$previous=$null
            $record.error_type=$snapshot.error_type
            if(-not (Add-SoakRecord $session $record)){$status='stopped';$reason='output_limit';break}
            if($consecutiveErrors -ge $MaxConsecutiveSampleErrors){$status='stopped';$reason='sample_errors';break}
        } else {
            if($null -eq $expectedStart){$expectedStart=$snapshot.start_ticks}
            $consecutiveErrors=0
            $snapshot.elapsed_seconds=$elapsed;$snapshot.utc_ticks=$now.Ticks
            $cpu=Get-SoakCpuInterval $previous $snapshot $IntervalSeconds $GapMultiplier $logicalProcessors
            $record.cpu_interval_state=$cpu.state;$record.cpu_percent=$cpu.percent
            $record.private_bytes=$snapshot.private_bytes;$record.working_set_bytes=$snapshot.working_set_bytes
            $record.handles=$snapshot.handles;$record.threads=$snapshot.threads
            $record.interval_seconds=$cpu.seconds
            if(-not (Add-SoakRecord $session $record)){$status='stopped';$reason='output_limit';break}
            $statistics.samples++
            switch($cpu.state){
                'valid' {$statistics.cpu.Add($cpu.percent);$statistics.cpu_seconds+=$cpu.seconds;$statistics.cpu_work+=$cpu.cpu_seconds}
                'invalid_cpu_counter' {$statistics.invalid_cpu++}
            }
            Add-SoakSeries $statistics.private $elapsed $snapshot.private_bytes
            Add-SoakSeries $statistics.working $elapsed $snapshot.working_set_bytes
            Add-SoakSeries $statistics.handles $elapsed $snapshot.handles
            Add-SoakSeries $statistics.threads $elapsed $snapshot.threads
            $previous=$snapshot
        }
        if($elapsed-$lastSummary -ge $SummaryIntervalSeconds -or $statistics.samples -eq 1 -or $snapshot.state -eq 'sample_error'){
            Write-SoakSummary $summaryPath (Get-SoakSummary $session $statistics 'running' 'observing' $elapsed $startedUtc $logicalProcessors)
            $lastSummary=$elapsed
        }
        # The duration cap is rechecked after resume; no request changes the
        # target process, Explorer, machine sleep policy, display or user input.
        $remaining=$StopAfterSeconds-[Math]::Max($elapsedClock.Elapsed.TotalSeconds,([DateTime]::UtcNow-$startedUtc).TotalSeconds)
        if($remaining -gt 0){Start-Sleep -Milliseconds ([int][Math]::Ceiling(1000*[Math]::Min($IntervalSeconds,$remaining)))}
    }
} catch {
    $status='failed';$reason='observer_error'
    Write-Verbose $_.Exception.Message
    try {[void](Add-SoakRecord $session @{kind='observer_error';error_type=$_.Exception.GetType().Name;elapsed_seconds=$elapsedClock.Elapsed.TotalSeconds})}catch{}
} finally {
    $session.stream.Dispose()
    Write-SoakSummary $summaryPath (Get-SoakSummary $session $statistics $status $reason $elapsedClock.Elapsed.TotalSeconds $startedUtc $logicalProcessors)
}
[pscustomobject]@{RunDirectory=$runDirectory;SummaryPath=$summaryPath;LogPath=$logPath;Status=$status;Reason=$reason;Samples=$statistics.samples}
