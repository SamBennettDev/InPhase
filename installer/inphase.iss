; InPhase Host installer (Inno Setup 6).
;
; Build:  scripts\build-installer.ps1        (drives build -> package -> ISCC)
; Payload comes from  dist\InPhase\  produced by  scripts\package.ps1.
;
; One installer EXE (unsigned unless a signing command is supplied). Installs the Host + its PRIVATE GStreamer runtime under
; Program Files. Registers a per-user "start at sign-in" entry. Adds the
; Private-profile firewall rule. Clean uninstall + in-place upgrade.
;
; Signing: pass  /DSignTool="<signtool command with $f>"  to ISCC, or set
; SIGNTOOL in the environment; unsigned builds compile with a warning.

#ifndef AppVersion
  #define AppVersion "0.1.1"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\InPhase"
#endif
#define AppName "InPhase Host"
#define AppPublisher "Sam Bennett"
#define AppExe "InPhaseHost.exe"
#define AppUrl "https://github.com/SamBennettDev/InPhase"

[Setup]
AppId={{9C3D5E2A-4B7F-4E10-9A2B-InPhaseHost01}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
LicenseFile=..\LICENSE
DefaultDirName={autopf}\InPhase
DefaultGroupName=InPhase
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir=..\dist
OutputBaseFilename=InPhaseSetup
Compression=lzma2/max
SolidCompression=yes
; Program Files and firewall setup require elevation. User identity and trust
; setup explicitly run as the original user, including over-the-shoulder elevation.
PrivilegesRequired=admin
SetupIconFile=inphase.ico
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
WizardStyle=modern
MinVersion=10.0.19041
CloseApplications=yes
CloseApplicationsFilter=InPhaseHost.exe
RestartApplications=no
#ifdef SignTool
SignTool=byname
SignedUninstaller=yes
#endif

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startup"; Description: "Start InPhase Host automatically when I sign in"; GroupDescription: "Startup:"

[Files]
; Everything package.ps1 emitted: InPhaseHost.exe, runtime\, MANIFEST.csv,
; OPEN-SOURCE-COMPONENTS.txt, licenses\ ...
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{group}\InPhase Host";        Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Comment: "Start InPhase Host"
Name: "{group}\InPhase Diagnostics"; Filename: "{app}\{#AppExe}"; Parameters: "--doctor"; WorkingDir: "{app}"; Comment: "Check this PC's InPhase environment"
Name: "{group}\Open InPhase (this PC)"; Filename: "http://127.0.0.1:47800/?dashboard"
Name: "{group}\Uninstall InPhase Host"; Filename: "{uninstallexe}"

[InstallDelete]
; Retire the old shared certificate directory. Each user now gets a
; DPAPI-protected CA under their own profile. Other devices must trust it again.
Type: filesandordirs; Name: "{commonappdata}\InPhase\tls"

[Run]
Filename: "{sys}\certutil.exe"; Parameters: "-delstore -f Root ""InPhase Local CA"""; Flags: runhidden waituntilterminated
Filename: "{app}\{#AppExe}"; Parameters: "--setup-firewall"; StatusMsg: "Configuring Windows Firewall..."; Flags: runhidden waituntilterminated
Filename: "{app}\{#AppExe}"; Parameters: "--trust-ca"; StatusMsg: "Setting up your InPhase certificate..."; Flags: runhidden waituntilterminated runasoriginaluser
Filename: "{app}\{#AppExe}"; Parameters: "--enable-startup"; Flags: runhidden waituntilterminated runasoriginaluser; Tasks: startup
Filename: "{app}\{#AppExe}"; Parameters: "--open-dashboard"; Description: "Open InPhase and pair a device"; Flags: postinstall nowait runasoriginaluser skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/im {#AppExe} /f"; Flags: runhidden; RunOnceId: "killhost"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""InPhase App"""; Flags: runhidden; RunOnceId: "fwapp"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""InPhase LAN"""; Flags: runhidden; RunOnceId: "fwlan"
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -NonInteractive -Command ""Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object {{ $_.DisplayName -match '^InPhase [0-9]+ (tcp|udp)$' } | Remove-NetFirewallRule"""; Flags: runhidden; RunOnceId: "fwports"

Filename: "{sys}\certutil.exe"; Parameters: "-delstore -f Root ""InPhase Local CA"""; Flags: runhidden; RunOnceId: "delca"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\InPhase\gst-registry.bin"
; The local CA + leaf: removed here so a reinstall regenerates one that matches
; the fresh Root-store entry `--trust-ca` adds.
Type: filesandordirs; Name: "{commonappdata}\InPhase\tls"

[Code]
// Offer to remove user data (config + host log) on uninstall.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  dir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'InPhaseHost');
    dir := ExpandConstant('{userappdata}\InPhase');
    if DirExists(dir) then
      if MsgBox('Also remove InPhase settings and logs (' + dir + ')?',
                mbConfirmation, MB_YESNO) = IDYES then
        DelTree(dir, True, True, True);
  end;
end;
