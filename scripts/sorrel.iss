; One-click Windows installer, like Claude Desktop's: no wizard pages, no admin
; prompt. Installs to %LOCALAPPDATA%\Programs\Sorrel, adds Start menu and
; desktop shortcuts, registers an uninstaller and launches the app.
; Built by scripts/package.sh:  ISCC -DVersion=0.1.0 scripts/sorrel.iss

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Exe
  #define Exe "..\target\release\sorrel.exe"
#endif

[Setup]
AppId={{05B26FCA-42BE-4EF1-A5DF-291CF7EFFE1E}
AppName=Sorrel
AppVersion={#Version}
AppPublisher=Sorrel
DefaultDirName={localappdata}\Programs\Sorrel
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ShowLanguageDialog=no
DisableWelcomePage=yes
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
DisableFinishedPage=yes
CloseApplications=force
SetupIconFile=..\crates\app\icon\sorrel.ico
UninstallDisplayIcon={app}\sorrel.exe
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\dist
OutputBaseFilename=sorrel-{#Version}-windows-x64-setup

[Files]
Source: "{#Exe}"; DestDir: "{app}"; DestName: "sorrel.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Sorrel"; Filename: "{app}\sorrel.exe"
Name: "{autodesktop}\Sorrel"; Filename: "{app}\sorrel.exe"

[Run]
Filename: "{app}\sorrel.exe"; Flags: nowait skipifsilent
