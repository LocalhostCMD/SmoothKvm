; SmoothKvm Windows installer (Inno Setup 6).  Build:  ISCC /DAppVer=0.4.0 SmoothKvm.iss
#ifndef AppVer
  #define AppVer "0.0.0"
#endif

[Setup]
AppId={{B0E9A4D2-6C7A-4D8E-9D3E-5A1B2C3D4E5F}
AppName=SmoothKvm
AppVersion={#AppVer}
AppPublisher=SmoothKvm
DefaultDirName={localappdata}\Programs\SmoothKvm
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\..\dist
OutputBaseFilename=SmoothKvm-Setup-{#AppVer}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=smoothkvm.ico
UninstallDisplayIcon={app}\smoothkvm.ico
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=no

[Tasks]
Name: "autostart"; Description: "Start SmoothKvm when I sign in"; GroupDescription: "Options:"
Name: "addpath"; Description: "Let me type SmoothKvm in a terminal (adds it to PATH)"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop icon"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "..\..\target-rel\release\SmoothKvm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target-rel\release\SmoothKvmApp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "smoothkvm.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\SmoothKvm"; Filename: "{app}\SmoothKvmApp.exe"; IconFilename: "{app}\smoothkvm.ico"
Name: "{userprograms}\SmoothKvm Terminal"; Filename: "{app}\SmoothKvm.exe"; IconFilename: "{app}\smoothkvm.ico"
Name: "{userdesktop}\SmoothKvm"; Filename: "{app}\SmoothKvmApp.exe"; IconFilename: "{app}\smoothkvm.ico"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "SmoothKvm"; ValueData: """{app}\SmoothKvmApp.exe"" start"; Flags: uninsdeletevalue; Tasks: autostart
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}')); Tasks: addpath

[Run]
Filename: "{app}\SmoothKvmApp.exe"; Parameters: "open"; Description: "Open SmoothKvm now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\SmoothKvm.exe"; Parameters: "stop"; Flags: runhidden; RunOnceId: "StopSmoothKvm"

[Code]
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
  OldExe: string;
begin
  // Stop a running copy so its files can be replaced.
  OldExe := ExpandConstant('{app}\SmoothKvm.exe');
  if FileExists(OldExe) then
    Exec(OldExe, 'stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  // Older versions were installed with "SmoothKvm install" into %LOCALAPPDATA%\SmoothKvm.
  // Remove that copy (stops it, drops its autostart and PATH entries) so the two don't fight.
  OldExe := ExpandConstant('{localappdata}\SmoothKvm\SmoothKvm.exe');
  if FileExists(OldExe) then
    Exec(OldExe, 'uninstall', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  OrigPath, App: string;
  P: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    App := ExpandConstant('{app}');
    if RegQueryStringValue(HKCU, 'Environment', 'Path', OrigPath) then
    begin
      P := Pos(';' + Uppercase(App), Uppercase(OrigPath));
      if P > 0 then
      begin
        Delete(OrigPath, P, Length(App) + 1);
        RegWriteExpandStringValue(HKCU, 'Environment', 'Path', OrigPath);
      end;
    end;
  end;
end;
