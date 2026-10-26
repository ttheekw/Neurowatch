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
#ifndef AppIcon
  #error AppIcon must be supplied by package_windows.ps1
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
SetupIconFile={#AppIcon}
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
Name: "{group}\NeuroWatch"; Filename: "{app}\app\neurowatch-ui\neurowatch-ui.exe"; WorkingDir: "{app}"; AppUserModelID: "NeuroWatch.NeuroWatch"
Name: "{autodesktop}\NeuroWatch"; Filename: "{app}\app\neurowatch-ui\neurowatch-ui.exe"; WorkingDir: "{app}"; AppUserModelID: "NeuroWatch.NeuroWatch"; Tasks: desktopicon

[Run]
Filename: "{app}\app\neurowatch-ui\neurowatch-ui.exe"; Description: "Open NeuroWatch"; Flags: nowait postinstall skipifsilent

[Code]
// An update says so, and leaves nothing of the old version behind: after the
// new files are copied, anything under the program folders that this build's
// file list (.neurowatch-files, package_windows.ps1) does not name was left by
// an earlier version. A stale neurowatch-<old>.dist-info made the window show
// the old version, and old DLLs or GStreamer plugins could be loaded.
const
  // Where Setup records this installation: the AppId plus "_is1".
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{CDAA8C42-A8C6-4F7F-8A44-53B962D38052}_is1';
  ManifestName = '.neurowatch-files';
  FileAttributeReparsePoint = $400;

var
  PreviousVersion: String;
  RemovedFiles: Integer;

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

function InitializeSetup(): Boolean;
begin
  if not RegQueryStringValue(HKA, UninstallKey, 'DisplayVersion', PreviousVersion) then
    PreviousVersion := '';
  if PreviousVersion <> '' then
    Log('Updating NeuroWatch ' + PreviousVersion + ' to {#AppVersion}');
  Result := True;
end;

procedure InitializeWizard();
begin
  if PreviousVersion <> '' then
    CreateOutputMsgPage(wpWelcome, 'Update NeuroWatch',
      'NeuroWatch ' + PreviousVersion + ' is installed on this computer.',
      'Setup updates it to version {#AppVersion}. The old version''s program files are ' +
      'replaced and any it leaves behind are removed, so nothing of it stays mixed with ' +
      'the new version.' + #13#10#13#10 +
      'Your settings, cameras and recordings are kept. Recording pauses briefly while ' +
      'NeuroWatch updates.');
end;

function IsOwnTopLevel(const Name: String): Boolean;
var
  Lower: String;
begin
  Lower := Lowercase(Name);
  Result := (Lower = 'app') or (Lower = 'bin') or (Lower = 'gstreamer') or
    (Lower = 'license') or (Lower = 'notice') or (Lower = 'neurowatch.ps1') or
    (Lower = 'neurowatch.cmd') or (Lower = ManifestName) or (Pos('unins', Lower) = 1);
end;

// Whether {app} holds NeuroWatch alone, so its bin and gstreamer folders are
// NeuroWatch's too: it may have been installed into a shared folder.
function AppFolderIsOwn(const Root: String): Boolean;
var
  FindRec: TFindRec;
begin
  Result := True;
  if FindFirst(Root + '\*', FindRec) then
  try
    repeat
      if (FindRec.Name <> '.') and (FindRec.Name <> '..') and not IsOwnTopLevel(FindRec.Name) then
      begin
        Log('Not only NeuroWatch in ' + Root + ' (' + FindRec.Name + '): checking app\neurowatch-ui only');
        Result := False;
      end;
    until (not Result) or (not FindNext(FindRec));
  finally
    FindClose(FindRec);
  end;
end;

// Removes the files under Root\Relative (no trailing backslash) that Listed
// does not name, and the folders that leaves empty.
procedure RemoveUnlisted(const Root, Relative: String; Listed: TStringList);
var
  FindRec: TFindRec;
  Entry: String;
begin
  if not FindFirst(Root + '\' + Relative + '\*', FindRec) then
    Exit;
  try
    repeat
      if (FindRec.Name <> '.') and (FindRec.Name <> '..') then
      begin
        Entry := Relative + '\' + FindRec.Name;
        if (FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then
        begin
          if (FindRec.Attributes and FileAttributeReparsePoint) = 0 then
            RemoveUnlisted(Root, Entry, Listed);
          if Listed.IndexOf(Entry + '\') < 0 then
            RemoveDir(Root + '\' + Entry);
        end
        else if Listed.IndexOf(Entry) < 0 then
        begin
          if DeleteFile(Root + '\' + Entry) then
          begin
            RemovedFiles := RemovedFiles + 1;
            Log('Removed what an earlier version left: ' + Entry);
          end
          else
            Log('Could not remove ' + Entry);
        end;
      end;
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

procedure RemoveLeftovers();
var
  Root: String;
  Lines: TArrayOfString;
  Listed: TStringList;
  I: Integer;
begin
  Root := ExpandConstant('{app}');
  if not LoadStringsFromFile(Root + '\' + ManifestName, Lines) then
  begin
    Log('No ' + ManifestName + ' in ' + Root + ': nothing checked');
    Exit;
  end;
  // Sorted: IndexOf searches it quickly, ignoring case like Windows does.
  Listed := TStringList.Create;
  try
    Listed.Sorted := True;
    for I := 0 to GetArrayLength(Lines) - 1 do
      if Lines[I] <> '' then
        Listed.Add(Lines[I]);
    if AppFolderIsOwn(Root) then
    begin
      RemoveUnlisted(Root, 'app', Listed);
      RemoveUnlisted(Root, 'bin', Listed);
      RemoveUnlisted(Root, 'gstreamer', Listed);
    end
    else
      RemoveUnlisted(Root, 'app\neurowatch-ui', Listed);
  finally
    Listed.Free;
  end;
  Log('Removed ' + IntToStr(RemovedFiles) + ' file(s) left over from an earlier version');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    WizardForm.StatusLabel.Caption := 'Removing files the previous version left behind...';
    RemoveLeftovers();
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
var
  Note: String;
  Grown: Integer;
begin
  if (CurPageID = wpFinished) and (PreviousVersion <> '') then
  begin
    Note := 'NeuroWatch was updated from ' + PreviousVersion + ' to {#AppVersion}.';
    if RemovedFiles > 0 then
      Note := Note + ' ' + IntToStr(RemovedFiles) + ' file(s) left over from the old version were removed.';
    WizardForm.FinishedLabel.Caption := Note + #13#10#13#10 + WizardForm.FinishedLabel.Caption;
    Grown := WizardForm.AdjustLabelHeight(WizardForm.FinishedLabel);
    WizardForm.RunList.Top := WizardForm.RunList.Top + Grown;
  end;
end;

// User configuration and recordings live outside {app} and are preserved.
