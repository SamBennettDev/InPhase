; InPhase Host installer (Inno Setup 6).
;
; Build:  scripts\build-installer.ps1        (drives build -> package -> ISCC)
; Payload comes from  dist\InPhase\  produced by  scripts\package.ps1.
;
; One code-signed EXE. Installs the Host + its PRIVATE GStreamer runtime under
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
#define AppUrl "https://inphase.app"

[Setup]
AppId={{9C3D5E2A-4B7F-4E10-9A2B-InPhaseHost01}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
DefaultDirName={autopf}\InPhase
DefaultGroupName=InPhase
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir=..\dist
OutputBaseFilename=InPhaseSetup
Compression=lzma2/max
SolidCompression=yes
; Program Files + firewall + machine-wide install -> needs elevation. HKCU
; registry writes still target the invoking (non-elevated) user's hive.
PrivilegesRequired=admin
SetupIconFile=inphase.ico
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
MinVersion=10.0.19041
; The host is a windowless tray process. Restart Manager's graceful close
; (CloseApplications=yes) sends WM_CLOSE to the hidden tray window, the tray
; thread exits, and InPhaseHost.exe keeps the Program Files locks - which is
; exactly the "unable to automatically close all applications" dialog.
; `force` TerminateProcess-es leftovers; PrepareToInstall also taskkill's first
; so an elevated Setup can stop the unelevated user-session host (UAC split).
CloseApplications=force
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

[Registry]
; Per-user "run at sign-in" — HKCU under an Inno elevated install lands in the
; ORIGINAL user's hive, not the admin's.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "InPhaseHost"; \
  ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
; The Host is a windowless tray application — no launcher script or hidden
; console. It self-manages its Windows Firewall rules (program allow + a per-port
; rule for 47800 and 443, remote-scoped to the LAN by default and widened to
; global IPv6 only when [remote_access] is enabled) on every startup — so
; nothing to do here beyond the cert.
; Generate the local CA + leaf cert and add the CA to the machine trust store
; (runs elevated during install → silent). Makes HTTPS "just work" on this PC.
Filename: "{app}\{#AppExe}"; Parameters: "--trust-ca"; StatusMsg: "Setting up the InPhase certificate..."; \
  Flags: runhidden waituntilterminated
; The elevated step above owns the cert files. Let the per-user Host reissue the
; leaf when the machine's IPs change, instead of silently falling back to HTTP.
Filename: "{sys}\icacls.exe"; Parameters: """{commonappdata}\InPhase\tls"" /grant *S-1-5-11:(OI)(CI)M /T /C /Q"; \
  Flags: runhidden waituntilterminated
; Add the inbound firewall rules now (elevated), so a windowless sign-in launch
; doesn't need the user to approve a Windows Security prompt.
Filename: "{app}\{#AppExe}"; Parameters: "--setup-firewall"; StatusMsg: "Configuring Windows Firewall..."; \
  Flags: runhidden waituntilterminated
; Post-install environment check (opt-in on the Finished page).
Filename: "{app}\{#AppExe}"; Parameters: "--doctor"; Description: "Check this PC's InPhase environment"; \
  Flags: postinstall skipifsilent unchecked runasoriginaluser
; Start now so the user doesn't have to sign out / in.
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser skipifsilent; Tasks: startup

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/im {#AppExe} /f"; Flags: runhidden; RunOnceId: "killhost"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""InPhase App"""; Flags: runhidden; RunOnceId: "fwapp"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""InPhase LAN"""; Flags: runhidden; RunOnceId: "fwlan"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""InPhase 47800"""; Flags: runhidden; RunOnceId: "fw47800"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""InPhase 443"""; Flags: runhidden; RunOnceId: "fw443"
Filename: "{sys}\certutil.exe"; Parameters: "-delstore -f Root ""InPhase Local CA"""; Flags: runhidden; RunOnceId: "delca"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\InPhase\gst-registry.bin"
; The local CA + leaf: removed here so a reinstall regenerates one that matches
; the fresh Root-store entry `--trust-ca` adds.
Type: filesandordirs; Name: "{commonappdata}\InPhase\tls"

[Code]
procedure StopRunningHost;
var
  ResultCode: Integer;
begin
  { tools/ship.sh's interactive boot task must not relaunch mid-copy. }
  Exec(ExpandConstant('{sys}\schtasks.exe'),
    '/End /TN "InPhaseWTStart"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'),
    '/F /T /IM InPhaseHost.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'),
    '/F /T /IM inphase-host.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(800);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  NeedsRestart := False;
  StopRunningHost;
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopRunningHost;
  Result := True;
end;

// Offer to remove user data (config + host log) on uninstall.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  dir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    dir := ExpandConstant('{userappdata}\InPhase');
    if DirExists(dir) then
      if MsgBox('Also remove InPhase settings and logs (' + dir + ')?',
                mbConfirmation, MB_YESNO) = IDYES then
        DelTree(dir, True, True, True);
  end;
end;
