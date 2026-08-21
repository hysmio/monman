#ifndef AppVersion
  #error AppVersion must be provided with /DAppVersion=x.y.z
#endif

#ifndef ReleaseTag
  #error ReleaseTag must be provided with /DReleaseTag=vx.y.z
#endif

#define AppName "MonMan"
#define AppExeName "MonMan.exe"

[Setup]
AppId={{AF5EE3D1-BB1B-45E7-8E0D-95678B274B77}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=hysmio
AppPublisherURL=https://github.com/hysmio/monman
AppSupportURL=https://github.com/hysmio/monman/issues
AppUpdatesURL=https://github.com/hysmio/monman/releases
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
OutputDir=..\dist
OutputBaseFilename=monman-{#ReleaseTag}-windows-x86_64-setup
UninstallDisplayIcon={app}\{#AppExeName}

[Files]
Source: "..\target\release\monman.exe"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion

[Tasks]
Name: "autostart"; Description: "Start {#AppName} when I sign in to Windows"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExeName}"" --startup"; Flags: uninsdeletevalue; Tasks: autostart

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\{#AppExeName}"; Flags: nowait skipifnotsilent
