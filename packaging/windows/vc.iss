; Inno Setup script for thatsacut (vc CLI)
; Built in CI by .github/workflows/release.yml

#define AppName "thatsacut"
#define AppVersion "0.1.1"
#define AppPublisher "postmanpat"
#define AppURL "https://github.com/postmanpat-303/thatsacut"
#define AppExeName "vc.exe"

[Setup]
AppId={{3F8C2A71-6B4D-4E9A-A1C5-7D2E8F0B3C46}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
DefaultGroupName={#AppName}
OutputDir=..\..\installer
OutputBaseFilename=vc-setup-{#AppVersion}-windows-x86_64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "addtopath"; Description: "Add vc to your PATH (recommended)"; GroupDescription: "Extra options:"
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Extra options:"

[Files]
Source: "..\..\target\x86_64-pc-windows-msvc\release\vc.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName} command line"; Filename: "{cmd}"; Parameters: "/K ""{app}\vc.exe"" --help"; WorkingDir: "{app}"
Name: "{userdesktop}\{#AppName}"; Filename: "{cmd}"; Parameters: "/K ""{app}\vc.exe"" --help"; WorkingDir: "{app}"; Tasks: desktopicon
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}'))

[Code]
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    Exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;
