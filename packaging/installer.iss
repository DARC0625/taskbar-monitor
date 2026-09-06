; Taskbar Monitor, per-user Windows 11 x64 installer.
; Build with Inno Setup 7.1+ and absolute /DPayloadDir=... /DReleaseDir=... paths.
; Payload is explicitly allowlisted below. User settings are not installer files.
;
; Official syntax references:
; https://jrsoftware.org/ishelp/topic_setup_wizardstyle.htm
; https://jrsoftware.org/ishelp/topic_scriptevents.htm
; https://jrsoftware.org/ishelp/topic_isxfunc_extracttemporaryfile.htm
; https://jrsoftware.org/ishelp/topic_isxfunc_checkformutexes.htm
; https://jrsoftware.org/ishelp/topic_setup_useprevioustasks.htm
; https://jrsoftware.org/ishelp/topic_setup_setupmutex.htm
; https://jrsoftware.org/ishelp/topic_scriptdll.htm
; https://jrsoftware.org/ishelp/topic_isxfunc_findwindowbyclassname.htm

#ifndef PayloadDir
  #error "Pass /DPayloadDir=<absolute payload directory>"
#endif
#ifndef ReleaseDir
  #error "Pass /DReleaseDir=<absolute installer output directory>"
#endif
#ifndef AppVersion
  #error "Pass /DAppVersion=<Cargo.toml version> (or use build-release.ps1)"
#endif

#define AppName "Taskbar Monitor"
#define AppExeName "taskbar-monitor.exe"
#define QuitHelperName "taskbar-monitor-quit.exe"

[Setup]
AppId={{A8B2DC4A-9673-4EC0-8E88-C744D3A79E42}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
VersionInfoVersion={#AppVersion}
VersionInfoDescription={#AppName} Setup
DefaultDirName={localappdata}\Programs\TaskbarMonitor
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
SetupArchitecture=x64
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.22000
WizardStyle=modern dynamic
SetupIconFile=..\assets\taskbar-monitor.ico
OutputDir={#ReleaseDir}
OutputBaseFilename=TaskbarMonitor-Setup-{#AppVersion}-x64
Compression=lzma2
SolidCompression=yes
DisableProgramGroupPage=yes
DisableDirPage=auto
UsePreviousAppDir=yes
UsePreviousGroup=yes
UsePreviousLanguage=yes
UsePreviousTasks=yes
CloseApplications=no
RestartApplications=no
SetupMutex=Local\TaskbarMonitor.Setup.1
Uninstallable=yes
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExeName}
UninstallFilesDir={app}
ChangesAssociations=no
ChangesEnvironment=no

; AppMutex is deliberately absent: its early check would prevent our
; PrepareToInstall handler from asking the running widget to exit gracefully.

[Languages]
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
korean.AdditionalTasks=추가 설정:
korean.DesktopTask=바탕 화면에 바로 가기 만들기
korean.AutostartTask=Windows 로그인 시 Taskbar Monitor 실행
korean.RunApplication=Taskbar Monitor 실행
korean.QuitUnavailable=실행 중인 Taskbar Monitor에 종료를 요청할 프로그램을 찾지 못했습니다. 위젯의 우클릭 메뉴에서 종료한 뒤 다시 시도해 주세요.
korean.QuitLaunchFailed=Taskbar Monitor에 종료 요청을 보내지 못했습니다. 위젯의 우클릭 메뉴에서 종료한 뒤 다시 시도해 주세요.
korean.QuitTimedOut=Taskbar Monitor가 10초 안에 종료되지 않았습니다. 위젯의 우클릭 메뉴에서 종료한 뒤 다시 시도해 주세요. 설치된 앱 파일은 변경하지 않았습니다.
korean.QuitExtractFailed=실행 중인 Taskbar Monitor를 종료할 프로그램을 준비하지 못했습니다.
korean.QuitMonitorFailed=실행 중인 Taskbar Monitor의 프로세스 종료를 확인할 수 없습니다. 위젯의 우클릭 메뉴에서 종료한 뒤 다시 시도해 주세요.
english.AdditionalTasks=Additional options:
english.DesktopTask=Create a desktop shortcut
english.AutostartTask=Run Taskbar Monitor when I sign in to Windows
english.RunApplication=Launch Taskbar Monitor
english.QuitUnavailable=The program needed to request Taskbar Monitor to exit could not be found. Exit the widget using its right-click menu, then try again.
english.QuitLaunchFailed=Taskbar Monitor could not be asked to exit. Exit the widget using its right-click menu, then try again.
english.QuitTimedOut=Taskbar Monitor did not exit within 10 seconds. Exit the widget using its right-click menu, then try again. No installed files have been changed.
english.QuitExtractFailed=The program needed to request the running Taskbar Monitor to exit could not be prepared.
english.QuitMonitorFailed=The running Taskbar Monitor process could not be monitored for exit. Exit the widget using its right-click menu, then try again.

[Tasks]
Name: "desktopicon"; Description: "{cm:DesktopTask}"; GroupDescription: "{cm:AdditionalTasks}"; Flags: unchecked
Name: "autostart"; Description: "{cm:AutostartTask}"; GroupDescription: "{cm:AdditionalTasks}"; Flags: unchecked

[Files]
; Only a temporary copy of our own bundled payload is used
; for --quit. It is listed first so solid compression can extract it promptly.
Source: "{#PayloadDir}\{#AppExeName}"; DestName: "{#QuitHelperName}"; Flags: dontcopy noencryption
Source: "{#PayloadDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\README.ko.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "TaskbarMonitor"; ValueData: """{app}\{#AppExeName}"""; Flags: uninsdeletevalue; Tasks: autostart
; Unchecking a previously selected task during an upgrade disables only our value.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "TaskbarMonitor"; Flags: deletevalue dontcreatekey; Tasks: not autostart

[Run]
Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; Description: "{cm:RunApplication}"; Flags: nowait postinstall skipifsilent

; No [InstallDelete] or [UninstallDelete] section. In particular, nothing deletes
; {localappdata}\TaskbarMonitor or an old executable-adjacent widget.json.

[Code]
const
  WidgetMutex = 'Local\RustTaskbarMonitor.Widget.1';
  QuitWaitMilliseconds = 10000;
  ProcessSynchronize = $00100000;
  WaitObjectSignaled = 0;
  WaitTimedOut = $00000102;

function GetTickCount64: Int64;
  external 'GetTickCount64@kernel32.dll stdcall';

// Inno 7 HWND and THandle are NativeUInt, preserving all 64 pointer bits.
// DWORD and BOOL remain 32 bits in the Windows x64 API.
function GetWidgetWindowProcessId(Window: HWND; var ProcessId: DWORD): DWORD;
  external 'GetWindowThreadProcessId@user32.dll stdcall';

function OpenWidgetProcess(Access: DWORD; InheritHandle: BOOL; ProcessId: DWORD): THandle;
  external 'OpenProcess@kernel32.dll stdcall';

function WaitForWidgetProcess(Process: THandle; Milliseconds: DWORD): DWORD;
  external 'WaitForSingleObject@kernel32.dll stdcall';

function CloseWidgetProcess(Process: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall';

function FindWidgetWindow: HWND;
begin
  Result := FindWindowByClassName('RustTaskbarMonitor.Controller.1');
  if Result = 0 then
    Result := FindWindowByClassName('RustTaskbarMonitor.Widget.1');
end;

function RequestWidgetExit(const Executable: String): String;
var
  ResultCode: Integer;
  Deadline: Int64;
  Remaining: Int64;
  Window: HWND;
  ProcessId: DWORD;
  Process: THandle;
  WaitResult: DWORD;
begin
  Result := '';
  Window := FindWidgetWindow;
  if (Window = 0) and not CheckForMutexes(WidgetMutex) then
    Exit;

  // A live mutex without a discoverable window can be startup or shutdown in
  // progress. Abort conservatively; never equate mutex disappearance with exit.
  if Window = 0 then
  begin
    Result := CustomMessage('QuitMonitorFailed');
    Log(Result);
    Exit;
  end;

  if not FileExists(Executable) then
  begin
    Result := CustomMessage('QuitUnavailable');
    Exit;
  end;

  Deadline := GetTickCount64 + QuitWaitMilliseconds;
  ProcessId := 0;
  if GetWidgetWindowProcessId(Window, ProcessId) = 0 then
  begin
    Result := CustomMessage('QuitMonitorFailed') + #13#10 + SysErrorMessage(DLLGetLastError);
    Log(Result);
    Exit;
  end;
  if ProcessId = 0 then
  begin
    Result := CustomMessage('QuitMonitorFailed');
    Log(Result);
    Exit;
  end;

  // Acquire the exact running process before asking it to close. This handle
  // remains tied to that process if Windows later reuses its numeric PID.
  // SYNCHRONIZE is the only requested right: no termination or debug access.
  Process := OpenWidgetProcess(ProcessSynchronize, False, ProcessId);
  if Process = 0 then
  begin
    Result := CustomMessage('QuitMonitorFailed') + #13#10 + SysErrorMessage(DLLGetLastError);
    Log(Result);
    Exit;
  end;

  try
    Log('Requesting graceful Taskbar Monitor shutdown with --quit.');
    if not Exec(Executable, '--quit', '', SW_HIDE, ewNoWait, ResultCode) then
    begin
      Result := CustomMessage('QuitLaunchFailed') + #13#10 + SysErrorMessage(ResultCode);
      Log(Result);
      Exit;
    end;

    while CheckForMutexes(WidgetMutex) do
    begin
      Remaining := Deadline - GetTickCount64;
      if Remaining <= 0 then
      begin
        Result := CustomMessage('QuitTimedOut');
        Log(Result);
        Exit;
      end;
      if Remaining > 100 then
        Sleep(100)
      else
        Sleep(Integer(Remaining));
    end;

    // Releasing the app mutex happens shortly before process teardown. Wait for
    // the process object too, using only what remains of the same ten seconds.
    Remaining := Deadline - GetTickCount64;
    if Remaining < 0 then
      Remaining := 0;
    WaitResult := WaitForWidgetProcess(Process, DWORD(Remaining));
    if WaitResult = WaitTimedOut then
    begin
      Result := CustomMessage('QuitTimedOut');
      Log(Result);
      Exit;
    end;
    if WaitResult <> WaitObjectSignaled then
    begin
      Result := CustomMessage('QuitMonitorFailed') + #13#10 + SysErrorMessage(DLLGetLastError);
      Log(Result);
      Exit;
    end;
    Log('Taskbar Monitor released its singleton mutex and its process exited.');
  finally
    CloseWidgetProcess(Process);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  QuitHelper: String;
begin
  Result := '';
  NeedsRestart := False;
  if not CheckForMutexes(WidgetMutex) and (FindWidgetWindow = 0) then
    Exit;

  try
    ExtractTemporaryFile('{#QuitHelperName}');
    QuitHelper := ExpandConstant('{tmp}\{#QuitHelperName}');
    Result := RequestWidgetExit(QuitHelper);
  except
    Result := CustomMessage('QuitExtractFailed') + #13#10 + GetExceptionMessage;
    Log(Result);
  end;
end;

function InitializeUninstall: Boolean;
var
  Failure: String;
begin
  Failure := RequestWidgetExit(ExpandConstant('{app}\{#AppExeName}'));
  Result := Failure = '';
  if not Result then
  begin
    Log(Failure);
    if not UninstallSilent then
      SuppressibleMsgBox(Failure, mbError, MB_OK, IDOK);
  end;
end;
