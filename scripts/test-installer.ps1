param([Parameter(Mandatory)][string]$ReleaseDir)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
if(-not ($env:GITHUB_ACTIONS -ceq 'true' -and $env:RUNNER_ENVIRONMENT -ceq 'github-hosted' -and $env:RUNNER_OS -ceq 'Windows')){
    throw 'Installer lifecycle tests run only on disposable GitHub-hosted Windows runners.'
}
$release=(Resolve-Path -LiteralPath $ReleaseDir).Path
$setup=@(Get-ChildItem -LiteralPath $release -Filter 'TaskbarMonitor-Setup-*-x64.exe' -File)
if($setup.Count -ne 1){throw 'Expected exactly one installer'}
$registry='HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{A8B2DC4A-9673-4EC0-8E88-C744D3A79E42}_is1'
$run='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
function Autorun-Property {
    if(Test-Path -LiteralPath $run){
        $key=Get-ItemProperty -LiteralPath $run
        return $key.PSObject.Properties['TaskbarMonitor']
    }
    return $null
}
function Autorun {
    $property=Autorun-Property
    if($null -ne $property){return $property.Value}
    return $null
}
$startMenu=[Environment]::GetFolderPath('StartMenu')
$desktop=[Environment]::GetFolderPath('DesktopDirectory')
if([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA) -or [string]::IsNullOrWhiteSpace($startMenu) -or [string]::IsNullOrWhiteSpace($desktop)){
    throw 'Expected initialized per-user Windows folders'
}
$settingsDirectory=Join-Path $env:LOCALAPPDATA 'TaskbarMonitor'
$settings=Join-Path $settingsDirectory 'widget.json'
$shortcutDirectory=Join-Path $startMenu 'Programs/Taskbar Monitor'
$shortcut=Join-Path $shortcutDirectory 'Taskbar Monitor.lnk'
$desktopShortcut=Join-Path $desktop 'Taskbar Monitor.lnk'
# Finish every user-state check before creating even the test's own directories.
if(Test-Path -LiteralPath $registry){throw 'Refusing to replace an existing user installation'}
if(Get-Process taskbar-monitor -ErrorAction SilentlyContinue){throw 'Refusing to interfere with an existing widget'}
if(Test-Path -LiteralPath $settingsDirectory){throw 'User data exists; use a clean hosted runner'}
if($null -ne (Autorun-Property)){throw 'An existing TaskbarMonitor Run value is present'}
if((Test-Path -LiteralPath $shortcutDirectory) -or (Test-Path -LiteralPath $desktopShortcut)){throw 'An existing Taskbar Monitor shortcut location is present'}
$scratch=Join-Path ([IO.Path]::GetTempPath()) ('taskbar-installer-test-'+[guid]::NewGuid().ToString('N'))
$install=Join-Path $scratch '앱 설치 경로 with spaces'
New-Item -ItemType Directory -Path $scratch | Out-Null
New-Item -ItemType Directory -Path $settingsDirectory | Out-Null
Set-Content -LiteralPath $settings -Value '{"style":"minimal","offset_dip":28,"column_dip":104}' -Encoding utf8NoBOM
$hash=(Get-FileHash -LiteralPath $settings).Hash
function Install([string[]]$TaskArguments=@()){
    $arguments=@('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-','/LANG=korean',('/DIR="'+$install+'"')) + $TaskArguments
    $process=Start-Process -FilePath $setup[0].FullName -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
    if($process.ExitCode -ne 0){throw "Installer returned $($process.ExitCode)"}
}
function Check-Settings {if((Get-FileHash -LiteralPath $settings).Hash -ne $hash){throw 'Installer changed settings'}}
$startupCommand='"'+(Join-Path $install 'taskbar-monitor.exe')+'"'
function Simulate-AppStartup([bool]$Enabled){
    # Exercise an app-side change independently of Inno's previous task list.
    # This is the same named REG_SZ and quoted executable used by startup.rs.
    if($Enabled){
        if(-not (Test-Path -LiteralPath $run)){New-Item -Path $run | Out-Null}
        New-ItemProperty -LiteralPath $run -Name TaskbarMonitor -PropertyType String -Value $startupCommand -Force | Out-Null
    } elseif($null -ne (Autorun-Property)){
        Remove-ItemProperty -LiteralPath $run -Name TaskbarMonitor
    }
}
Install
if(-not (Test-Path -LiteralPath $registry)){throw 'Missing Installed Apps registration'}
if(-not (Test-Path -LiteralPath $shortcut)){throw 'Missing Start Menu shortcut'}
if($null -ne (Autorun-Property)){throw 'Autostart was enabled by default'}
if(Test-Path -LiteralPath (Join-Path $install 'portable.flag')){throw 'Installed mode contains portable.flag'}
if(-not (Test-Path -LiteralPath (Join-Path $install 'LICENSE'))){throw 'Application license was not installed'}
Check-Settings
& (Join-Path $PSScriptRoot 'test-probe.ps1') -Executable (Join-Path $install 'taskbar-monitor.exe') -OutputDir (Join-Path $scratch 'probe')
Simulate-AppStartup $true
Install
if((Autorun) -ne $startupCommand){throw 'Default reinstall lost app-enabled autostart'}
Check-Settings
Simulate-AppStartup $false
Install
if($null -ne (Autorun-Property)){throw 'Default reinstall restored stale app-disabled autostart'}
Check-Settings
Install @('/TASKS="autostart"')
if((Autorun) -ne $startupCommand){throw 'Autostart opt-in is incorrect'}
Check-Settings
Simulate-AppStartup $false
Install @('/MERGETASKS="autostart"')
if((Autorun) -ne $startupCommand){throw 'Explicit merged autostart opt-in was ignored'}
Install @('/MERGETASKS="!autostart"')
if($null -ne (Autorun-Property)){throw 'Explicit merged autostart opt-out was ignored'}
Simulate-AppStartup $true
Install @('/MERGETASKS="desktopicon"')
if((Autorun) -ne $startupCommand){throw 'An unrelated merged task replaced current app autostart'}
if(-not (Test-Path -LiteralPath $desktopShortcut)){throw 'Explicit merged desktop task was ignored'}
Check-Settings
Install @('/TASKS=""')
if($null -ne (Autorun-Property)){throw 'Autostart opt-out failed'}
Simulate-AppStartup $true
Install @('/TASKS=""')
if($null -ne (Autorun-Property)){throw 'Explicit empty task selection did not override app opt-in'}
Check-Settings
$foreignStartup='"'+(Join-Path $scratch 'portable copy\taskbar-monitor.exe')+'"'
New-ItemProperty -LiteralPath $run -Name TaskbarMonitor -PropertyType String -Value $foreignStartup -Force | Out-Null
Install
if((Autorun) -ne $foreignStartup){throw 'Default reinstall changed another copy''s startup registration'}
Install @('/TASKS="autostart"')
if((Autorun) -ne $startupCommand){throw 'Explicit installer opt-in did not register the installed path'}
Install @('/TASKS=""')
if($null -ne (Autorun-Property)){throw 'Explicit opt-out after registration transfer failed'}
Check-Settings
# No autostart task is selected in the last installation. Uninstall must still
# remove the value when the app enables startup afterwards.
Simulate-AppStartup $true
$uninstaller=Start-Process -FilePath (Join-Path $install 'unins000.exe') -ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART') -WindowStyle Hidden -Wait -PassThru
if($uninstaller.ExitCode -ne 0){throw 'Uninstall failed'}
if((Test-Path -LiteralPath $registry) -or (Test-Path -LiteralPath $shortcut) -or (Test-Path -LiteralPath $desktopShortcut) -or (Test-Path -LiteralPath (Join-Path $install 'taskbar-monitor.exe'))){throw 'Installed application remnants remain'}
Check-Settings
if($null -ne (Autorun-Property)){throw 'Uninstall left autostart enabled'}
Write-Output 'Installer lifecycle passed: default opt-out, app startup changes preserved on reinstall, explicit task overrides, startup ownership, uninstall after app opt-in and settings preservation.'
