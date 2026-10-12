; Isolated preview installer: installs ShellRS for the current user, with no
; administrator rights, under %LOCALAPPDATA%\Programs\ShellRS Preview.
;
;   iscc /DAppVersion=0.2.0 /DBuildDir=..\..\target\release /O..\..\dist shellrs.iss
;
; This AppId is reserved for the preview package and must never match the
; official installer. Only registered preview files are removed on uninstall;
; user data and credentials are retained. No PATH or protocol registration.

#ifndef AppVersion
  #error Pass /DAppVersion=<version>
#endif
#ifndef BuildDir
  #define BuildDir "..\..\target\release"
#endif
#define IconFile AddBackslash(SourcePath) + "..\..\assets\logo\shellrs.ico"
; Languages\ChineseSimplified.isl is UTF-8 without a BOM, and has the
; messages of 6.5 onwards.
#if Ver < EncodeVer(6, 5, 0)
  #error Inno Setup 6.5 or later is needed
#endif

[Setup]
AppId={{8D847C27-FE52-49DD-ACE6-E539F0E064CB}
AppName=ShellRS Preview
AppVersion={#AppVersion}
AppVerName=ShellRS Preview {#AppVersion}
AppPublisher=Nex-Z preview build
AppPublisherURL=https://github.com/Nex-Z/shell-rs
AppSupportURL=https://github.com/Nex-Z/shell-rs
DefaultDirName={localappdata}\Programs\ShellRS Preview
DisableDirPage=yes
UsePreviousAppDir=no
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=ShellRS-{#AppVersion}-windows-x86_64-preview-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\shellrs.exe
UninstallDisplayName=ShellRS Preview
; A running ShellRS is closed through the Restart Manager rather than
; making the setup fail on files in use.
CloseApplications=yes
; The setup speaks the language Windows shows, without asking: Simplified
; Chinese on a Chinese Windows, English on any other, as ShellRS itself.
ShowLanguageDialog=no
; Not the language of the copy it replaces: up to 0.1.9-beta.1 every setup
; was English, which would otherwise stay with each installed copy.
UsePreviousLanguage=no
#if FileExists(IconFile)
SetupIconFile={#IconFile}
#endif

[Languages]
; The first is the one for a language not listed.
Name: "english"; MessagesFile: "compiler:Default.isl"
; Not among the translations Inno Setup installs, so kept here: issrc's
; Files/Languages/Unofficial/ChineseSimplified.isl at is-6_7_1, the version
; on the windows-2025 runner. Update it with Inno Setup.
Name: "chinesesimplified"; MessagesFile: "..\windows\Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BuildDir}\shellrs.exe"; DestDir: "{app}"; Flags: ignoreversion
; Kept beside the app for direct invocation; preview disables PATH installation.
Source: "{#BuildDir}\shellrs-cli.exe"; DestDir: "{app}"; Flags: ignoreversion

Source: "README.zh-CN.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The AppUserModelID ShellRS gives itself (`set_app_identity`, for its
; notifications): the same here keeps a pinned shortcut and the running
; window one taskbar button.
Name: "{autoprograms}\ShellRS Preview"; Filename: "{app}\shellrs.exe"; AppUserModelID: "com.shellrs.ShellRS.Preview"
Name: "{autodesktop}\ShellRS Preview"; Filename: "{app}\shellrs.exe"; AppUserModelID: "com.shellrs.ShellRS.Preview"; Tasks: desktopicon

[Run]
; Interactive installs only: an update is restarted by ShellRS's own helper,
; which waits for this setup to finish.
Filename: "{app}\shellrs.exe"; Description: "{cm:LaunchProgram,ShellRS}"; Flags: nowait postinstall skipifsilent

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  RequiredDir: String;
begin
  RequiredDir := ExpandConstant('{localappdata}\Programs\ShellRS Preview');
  if CompareText(AddBackslash(ExpandFileName(WizardDirValue)),
                 AddBackslash(ExpandFileName(RequiredDir))) <> 0 then
    Result := 'This preview can only be installed to ' + RequiredDir
  else
    Result := '';
end;
