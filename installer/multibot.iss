#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\bundle"
#endif
#if Pos("-", AppVersion) > 0
  #define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
#else
  #define NumericVersion AppVersion
#endif

[Setup]
AppId={{ACDCC43A-52E9-4B41-A8B0-2347B9C5CA6F}
AppName=Multibot
AppVersion={#AppVersion}
VersionInfoVersion={#NumericVersion}
AppPublisher=Multibot
DefaultDirName={localappdata}\Multibot
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\multibot.exe
OutputDir=..\release
OutputBaseFilename=Multibot-Setup-x64
; Binaries are already built; avoid recompressing them on the release critical path.
Compression=none
SolidCompression=no
CloseApplications=yes
RestartApplications=no
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\multibot.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\multibotd-linux-x86_64"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Multibot"; Filename: "{app}\multibot.exe"; WorkingDir: "{app}"; AppUserModelID: "Multibot.Desktop"
Name: "{autodesktop}\Multibot"; Filename: "{app}\multibot.exe"; WorkingDir: "{app}"; AppUserModelID: "Multibot.Desktop"; Tasks: desktopicon

[Run]
Filename: "{app}\multibot.exe"; Description: "Launch Multibot"; Flags: nowait postinstall skipifsilent
