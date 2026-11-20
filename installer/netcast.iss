; netcast - Inno Setup script
;
; Built by the release workflow with
;   ISCC.exe /DAppVersion=1.0.0 installer\netcast.iss
; Run `cargo build --release` first so ..\target\release\netcast.exe exists.
;
; Output: installer\output\netcast-<version>-setup-x64.exe
;
; netcast is a command-line program (plus a web page it serves itself), so this
; puts it on PATH rather than on the desktop, and doesn't offer to launch it.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

; Folder holding the built exe, relative to this script. CI passes
; /DBinDir=..\target\x86_64-pc-windows-msvc\release when it builds with --target.
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

; VersionInfoVersion must be plain numeric (x.x.x[.x]); strip any prerelease
; suffix like "-rc1" so prerelease tags still compile.
#if Pos("-", AppVersion) > 0
  #define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
#else
  #define NumericVersion AppVersion
#endif

#define AppName      "netcast"
#define AppPublisher "Londopy"
#define AppURL       "https://github.com/Londopy/netcast"
#define AppExeName   "netcast.exe"

[Setup]
; Unique GUID - do NOT reuse this for a different app.
; The doubled opening brace is Inno's escape for a literal "{".
AppId={{37C85771-A6FD-4B62-B541-A7DCE13E6EE0}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases

; Per-user install, no UAC prompt. The dialog still lets the user pick an
; all-users install if they want one.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; With lowest privileges {autopf} resolves to %LOCALAPPDATA%\Programs.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
AllowNoIcons=yes

LicenseFile=..\LICENSE

OutputDir=output
OutputBaseFilename=netcast-{#AppVersion}-setup-x64
SetupIconFile=..\assets\netcast.ico

; Compression
Compression=lzma2/ultra64
SolidCompression=yes

; Windows 10+ only (64-bit)
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

WizardStyle=modern

; Restart Manager offers to close a running copy (a listener, the web page)
; before its files are replaced. The wizard asks first; install.ps1 runs setup
; silently, so it refuses to start while netcast is running instead, since
; closing it could cut a delivery off halfway.
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no

; PATH changes are broadcast so new terminals see netcast straight away
ChangesEnvironment=yes

UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName} {#AppVersion}
VersionInfoVersion={#NumericVersion}
VersionInfoDescription={#AppName} {#AppVersion} setup
VersionInfoProductName={#AppName}
VersionInfoCompany={#AppPublisher}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; on by default, unlike other integrations: a command-line program nobody can
; type isn't installed in any useful sense
Name: "addtopath"; \
  Description: "Add {#AppName} to the PATH"; \
  GroupDescription: "Environment:"

[Files]
; Main binary - built before running iscc
Source: "{#BinDir}\{#AppExeName}"; \
  DestDir: "{app}"; \
  Flags: ignoreversion

[Icons]
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"

; Uninstall leaves %LOCALAPPDATA%\netcast alone on purpose: it holds the
; station's signing key and its delivery history, which a reinstall should keep.

[Code]
// PATH handling (from nexium): append or remove {app} in the user's or the
// machine's PATH, depending on the install mode chosen in the wizard.
function EnvKey(): string;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

function EnvRoot(): Integer;
begin
  if IsAdminInstallMode then
    Result := HKEY_LOCAL_MACHINE
  else
    Result := HKEY_CURRENT_USER;
end;

procedure EnvAddPath(Dir: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(EnvRoot(), EnvKey(), 'Path', Paths) then
    Paths := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;
  if (Paths <> '') and (Paths[Length(Paths)] <> ';') then
    Paths := Paths + ';';
  Paths := Paths + Dir;
  RegWriteExpandStringValue(EnvRoot(), EnvKey(), 'Path', Paths);
end;

procedure EnvRemovePath(Dir: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot(), EnvKey(), 'Path', Paths) then
    exit;
  // P is where ";Dir;" starts in ";Paths;", which is where Dir starts in Paths
  P := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then
    exit;
  // take the separator before Dir with it, or the one after when Dir is first
  if P > 1 then
    Delete(Paths, P - 1, Length(Dir) + 1)
  else
    Delete(Paths, 1, Length(Dir) + 1);
  RegWriteExpandStringValue(EnvRoot(), EnvKey(), 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    EnvAddPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;
