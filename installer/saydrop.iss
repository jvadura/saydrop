; Inno Setup 6 script for saydrop.
;
; Build:   cargo build --release
;          iscc installer\saydrop.iss
; Output:  target\installer\saydrop-<version>-setup.exe
;
; Installs for the current user only, so it never asks for administrator rights.
;
; On a first install the wizard asks for the speech server, the hotkey and the
; optional cleanup model. For unattended installs the same answers can be given
; on the command line:
;
;   saydrop-x.y.z-setup.exe /VERYSILENT /ASR=http://server:8020 /HOTKEY=Insert
;       /LLM=http://server:8000 /LLMMODEL=my-model /TASKS="autostart"
;
; The answers are written to setup.ini in the config folder; saydrop merges
; them into config.ini the next time it starts.

#define AppName "saydrop"
#define AppExe "..\target\release\saydrop.exe"
#define AppVersion GetStringFileInfo(AppExe, "ProductVersion")
#define AppUrl "https://github.com/jvadura/saydrop"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

#if AppVersion == ""
  #error The release executable is missing or has no version info. Run "cargo build --release" first.
#endif

[Setup]
; Never change AppId: it is how upgrades find the previous installation.
AppId={{4C6569EC-2481-4E92-AB19-D5E2D31281F6}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=jvadura
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
DisableDirPage=auto
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\target\installer
OutputBaseFilename={#AppName}-{#AppVersion}-setup
SetupIconFile=..\assets\saydrop.ico
UninstallDisplayIcon={app}\saydrop.exe
UninstallDisplayName={#AppName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; The running copy is closed by the [Code] section below, which knows how to
; ask a tray-only program to exit; Restart Manager is not needed.
CloseApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Start {#AppName} when I sign in to Windows"
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\saydrop.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\saydrop.exe"; Tasks: desktopicon

[Registry]
; The same value the tray menu's "Start with Windows" switch reads and writes,
; so the installer option and the in-app switch always agree.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\saydrop.exe"""; Tasks: autostart
Root: HKCU; Subkey: "{#RunKey}"; ValueType: none; ValueName: "{#AppName}"; Flags: deletevalue; Tasks: not autostart

[Run]
Filename: "{app}\saydrop.exe"; Description: "Start {#AppName} now"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; The error log. The config in %APPDATA%\saydrop is the user's and is left alone.
Type: filesandordirs; Name: "{localappdata}\{#AppName}"

[Code]
const
  CloseMessage = $0010; { WM_CLOSE }
  RunningMutex = 'Local\saydrop.single-instance';
  DefaultAsr = 'http://127.0.0.1:8020';
  DefaultAsrModel = 'whisper-1';
  DefaultHotkey = 'Win+Shift+D';

var
  SpeechPage: TInputQueryWizardPage;
  CleanupPage: TInputQueryWizardPage;

function ConfigDir: String;
begin
  Result := ExpandConstant('{userappdata}\{#AppName}');
end;

{ An existing config means this is an upgrade: the user's settings stand and
  the setup pages are not shown. }
function ConfigExists: Boolean;
begin
  Result := FileExists(ConfigDir + '\config.ini');
end;

function Param(Name: String): String;
begin
  Result := Trim(ExpandConstant('{param:' + Name + '}'));
end;

function ParamOr(Name, Default: String): String;
begin
  Result := Param(Name);
  if Result = '' then
    Result := Default;
end;

procedure InitializeWizard;
begin
  SpeechPage := CreateInputQueryPage(wpSelectTasks,
    'Speech recognition',
    'Which server turns your speech into text?',
    'saydrop records your voice and sends it to a speech-to-text server that you run or ' +
    'have access to: llama.cpp, whisper.cpp, speaches, vLLM, or the OpenAI API. ' +
    'Everything here can be changed later from the tray icon (Edit config).');
  SpeechPage.Add('Server address:', False);
  SpeechPage.Add('Model name (any value works for servers with a single model):', False);
  SpeechPage.Add('Hotkey to start and stop dictating:', False);
  SpeechPage.Values[0] := ParamOr('ASR', DefaultAsr);
  SpeechPage.Values[1] := ParamOr('ASRMODEL', DefaultAsrModel);
  SpeechPage.Values[2] := ParamOr('HOTKEY', DefaultHotkey);

  CleanupPage := CreateInputQueryPage(SpeechPage.ID,
    'Text cleanup (optional)',
    'Should a language model tidy up the transcript?',
    'A chat model can fix punctuation and remove filler words before the text is pasted. ' +
    'Leave the address empty to paste the transcript as it is. ' +
    'Extra settings are JSON merged into each request, for example {"temperature": 0.2}.');
  CleanupPage.Add('Chat server address (empty = no cleanup):', False);
  CleanupPage.Add('Model name:', False);
  CleanupPage.Add('Extra settings (optional):', False);
  CleanupPage.Values[0] := Param('LLM');
  CleanupPage.Values[1] := Param('LLMMODEL');
  CleanupPage.Values[2] := Param('LLMEXTRA');
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := ((PageID = SpeechPage.ID) or (PageID = CleanupPage.ID)) and ConfigExists;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = SpeechPage.ID then
  begin
    if Trim(SpeechPage.Values[0]) = '' then
    begin
      MsgBox('Please enter the address of your speech-to-text server.', mbError, MB_OK);
      Result := False;
    end
    else if Trim(SpeechPage.Values[2]) = '' then
    begin
      MsgBox('Please enter a hotkey, for example Win+Shift+D or Insert.', mbError, MB_OK);
      Result := False;
    end;
  end;
end;

procedure AddLine(var Lines: TArrayOfString; Key, Value: String);
begin
  Value := Trim(Value);
  if Value = '' then
    Exit;
  SetArrayLength(Lines, GetArrayLength(Lines) + 1);
  Lines[GetArrayLength(Lines) - 1] := Key + ' = ' + Value;
end;

{ Hand the answers to saydrop. On a first install they come from the wizard
  pages (which start out with any command-line values); on an upgrade only
  values given explicitly on the command line are applied. }
procedure WriteSetupFile;
var
  Lines: TArrayOfString;
begin
  SetArrayLength(Lines, 0);
  if ConfigExists then
  begin
    AddLine(Lines, 'asr_url', Param('ASR'));
    AddLine(Lines, 'asr_model', Param('ASRMODEL'));
    AddLine(Lines, 'hotkey', Param('HOTKEY'));
    AddLine(Lines, 'llm_url', Param('LLM'));
    AddLine(Lines, 'llm_model', Param('LLMMODEL'));
    AddLine(Lines, 'llm_extra', Param('LLMEXTRA'));
  end
  else
  begin
    AddLine(Lines, 'asr_url', SpeechPage.Values[0]);
    AddLine(Lines, 'asr_model', SpeechPage.Values[1]);
    AddLine(Lines, 'hotkey', SpeechPage.Values[2]);
    AddLine(Lines, 'llm_url', CleanupPage.Values[0]);
    AddLine(Lines, 'llm_model', CleanupPage.Values[1]);
    AddLine(Lines, 'llm_extra', CleanupPage.Values[2]);
  end;
  if GetArrayLength(Lines) = 0 then
    Exit;
  ForceDirectories(ConfigDir);
  SaveStringsToUTF8File(ConfigDir + '\setup.ini', Lines, False);
end;

{ Ask a running saydrop to exit and wait until it has. It has no visible
  window, so it is found through its hidden main window's class. }
procedure CloseRunningApp;
var
  Window: HWND;
  Attempt: Integer;
begin
  Window := FindWindowByClassName('saydrop.main');
  if Window <> 0 then
    PostMessage(Window, CloseMessage, 0, 0);
  for Attempt := 1 to 50 do
  begin
    if not CheckForMutexes(RunningMutex) then
      Exit;
    Sleep(100);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  CloseRunningApp;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    WriteSetupFile;
end;

function InitializeUninstall: Boolean;
begin
  CloseRunningApp;
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  { Autostart may have been switched on from the tray menu after installing,
    so it is removed here rather than relying on what Setup itself wrote. }
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
end;
