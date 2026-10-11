; Per-user install: the backend, frame mappings and tray belong to this session.
#ifndef AppVersion
  #error AppVersion must be supplied by package_windows.ps1
#endif
#ifndef BundleDir
  #error BundleDir must be supplied by package_windows.ps1
#endif
#ifndef OutputDir
  #error OutputDir must be supplied by package_windows.ps1
#endif

[Setup]
AppId={{CDAA8C42-A8C6-4F7F-8A44-53B962D38052}
AppName=NeuroWatch
AppVersion={#AppVersion}
AppPublisher=NeuroWatch
DefaultDirName={localappdata}\Programs\NeuroWatch
DefaultGroupName=NeuroWatch
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=neurowatch-{#AppVersion}-windows-x86_64-setup
LicenseFile={#BundleDir}\LICENSE
UninstallDisplayIcon={app}\app\neurowatch-ui\neurowatch-ui.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#BundleDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\NeuroWatch"; Filename: "{app}\app\neurowatch-ui\neurowatch-ui.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\NeuroWatch"; Filename: "{app}\app\neurowatch-ui\neurowatch-ui.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\app\neurowatch-ui\neurowatch-ui.exe"; Description: "Open NeuroWatch"; Flags: nowait postinstall skipifsilent

[Code]
function RunServiceCommand(const Command: String): Boolean;
var
  Exe: String;
  ExitCode: Integer;
begin
  Exe := ExpandConstant('{app}\app\neurowatch-ui\neurowatch-ui.exe');
  Result := True;
  if FileExists(Exe) then
    Result := Exec(Exe, 'service ' + Command, ExpandConstant('{app}'),
      SW_HIDE, ewWaitUntilTerminated, ExitCode) and (ExitCode = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not RunServiceCommand('stop') then
    Result := 'NeuroWatch could not stop recording. Stop it from the tray, then retry the update.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := RunServiceCommand('stop');
  if Result then
    Result := RunServiceCommand('uninstall');
  if not Result then
    MsgBox('NeuroWatch could not stop or remove its startup entry. Stop it from the tray and retry.', mbError, MB_OK);
end;

; User configuration and recordings live outside {app} and are preserved.
