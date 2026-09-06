param([Parameter(Mandatory)][string]$Executable,[string]$OutputDir='target/probe-tests')
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$exe=(Resolve-Path -LiteralPath $Executable).Path
$directory=$ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDir)
New-Item -ItemType Directory -Path $directory -Force | Out-Null
$runDirectory=Join-Path $directory ('run-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runDirectory | Out-Null
$summaryPath=Join-Path $directory 'summary.json'
# A repeated invocation must not publish a previous run's successful summary.
[ordered]@{schema_version=1;tests=3;passed=$false;status='not_completed'} |
    ConvertTo-Json | Set-Content -LiteralPath $summaryPath -Encoding utf8

function Is-JsonNumber($Value) {
    return ($Value -is [long] -or $Value -is [int] -or $Value -is [double] -or $Value -is [decimal])
}
function Run-Probe([string]$Name,[string[]]$Arguments,[int]$ExpectedExit) {
    $out=Join-Path $runDirectory "$Name.json"
    if(Test-Path -LiteralPath $out){throw "Report path already exists: $Name"}
    $process=Start-Process -FilePath $exe -ArgumentList (@('--probe')+$Arguments+@('--out',('"'+$out+'"'))) -WindowStyle Hidden -PassThru
    try {
        if(-not $process.WaitForExit(30000)){
            # This handle belongs only to the child started above.
            if(-not $process.HasExited){$process.Kill();[void]$process.WaitForExit(5000)}
            throw "$Name exceeded its bounded execution time"
        }
        if($process.ExitCode -ne $ExpectedExit){throw "$Name returned $($process.ExitCode), expected $ExpectedExit"}
    } finally {$process.Dispose()}
    if(-not (Test-Path -LiteralPath $out -PathType Leaf)){throw "$Name did not create a report"}
    $report=Get-Content -LiteralPath $out -Raw | ConvertFrom-Json
    if($report -isnot [pscustomobject]){throw "$Name did not return a JSON object"}
    if(-not (Is-JsonNumber $report.schema_version) -or $report.schema_version -ne 1){throw "$Name schema mismatch"}
    return $report
}
$valid=Run-Probe 'valid' @('--seconds','6') 0
if($valid.status -cne 'completed' -or $valid.mode -cne 'passive_telemetry_probe'){throw 'Probe mode/status mismatch'}
if(-not (Is-JsonNumber $valid.requested_duration_seconds) -or $valid.requested_duration_seconds -ne 6){throw 'Probe did not record the requested duration'}
foreach($condition in @('sequence_advanced','cpu_has_valid_sample','ram_has_valid_sample','memory_used_not_above_total')){
    $result=$valid.validation.$condition
    if($result -isnot [bool] -or -not $result){throw "Probe invariant failed: $condition"}
}
# Range validation is an object; optional sensors may have no samples (null).
$ranges=$valid.validation.percentage_ranges_within_0_100
if($ranges -isnot [pscustomobject]){throw 'Missing per-metric percentage validation'}
foreach($metric in @('cpu','ram','gpu','disk','npu')){
    $result=$ranges.$metric
    if($null -eq $result){
        if($metric -in @('cpu','ram')){throw "Missing required percentage samples: $metric"}
    } elseif($result -isnot [bool] -or -not $result){throw "Invalid percentage range: $metric"}
}

# Virtual runners may legitimately lack GPU, disk counters, NPU or fan providers.
$knownStates=@('ready','warming_up','not_present','unsupported','stale','error')
$states=[ordered]@{}
foreach($metric in @('cpu','ram','gpu','disk','npu','fan')) {
    $sample=$valid.last_snapshot.$metric
    if($sample -isnot [pscustomobject] -or $knownStates -cnotcontains $sample.state){throw "Unknown metric state: $metric"}
    if($valid.support_status.$metric -cne $sample.state){throw "Inconsistent support state: $metric"}
    $states[$metric]=$sample.state
    if($sample.state -ceq 'ready'){
        if(-not (Is-JsonNumber $sample.value) -or -not [double]::IsFinite([double]$sample.value) -or $sample.value -lt 0){throw "Invalid ready metric: $metric"}
        if($metric -ne 'fan' -and $sample.value -gt 100){throw "Invalid percentage: $metric"}
    } elseif($null -ne $sample.value){throw "Unavailable metric presented a current value: $metric"}
}
$invalid=Run-Probe 'invalid-duration' @('--seconds','0') 2
if($invalid.status -cne 'invalid_arguments'){throw 'Invalid arguments were not rejected'}
$unknown=Run-Probe 'unknown-argument' @('--unknown') 2
if($unknown.status -cne 'invalid_arguments'){throw 'Unknown arguments were not rejected'}
# Only publish explicitly selected state codes. Raw reports stay in the unique run directory.
$summary=[ordered]@{schema_version=1;tests=3;passed=$true;status='completed';cpu_and_ram_ready=$true;optional_sensor_states=$states}
$summary | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $summaryPath -Encoding utf8
Write-Output 'Passive process smoke tests passed (3 cases).'
