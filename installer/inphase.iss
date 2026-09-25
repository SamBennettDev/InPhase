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
  #define AppVersion "0.3.1"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\InPhase"
#endif
#ifndef RedistDir
  #define RedistDir "..\dist\redist"
#endif
#define ViGEmSetup "ViGEmBus_1.22.0_x64_x86_arm64.exe"
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
UninstallDisplayIcon={app}\inphase.ico
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
; The host is a windowless tray application and does not reliably participate in
; Restart Manager shutdown. PrepareToInstall below stops it explicitly before
; Setup attempts to replace the executable.
CloseApplications=no
RestartApplications=no
#ifdef SignTool
SignTool=byname
SignedUninstaller=yes
#endif

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startup"; Description: "Start InPhase Host automatically when I sign in"; GroupDescription: "Startup:"
; Game controllers need a virtual-controller driver on the PC. Ticked by
; default; skipped when ViGEmBus is already installed (Sunshine, Parsec, ...).
Name: "controllers"; Description: "Controller support (installs the free ViGEmBus driver)"; GroupDescription: "Controllers:"; Check: not ViGEmBusInstalled

[Files]
; Everything package.ps1 emitted: InPhaseHost.exe, runtime\, MANIFEST.csv,
; OPEN-SOURCE-COMPONENTS.txt, licenses\ ...
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion
; The host EXE carries no icon resource; shortcuts and Apps & features use this.
Source: "inphase.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "licenses\ViGEmBus-LICENSE.txt"; DestDir: "{app}\licenses\ViGEmBus"; Flags: ignoreversion
Source: "{#RedistDir}\{#ViGEmSetup}"; DestDir: "{tmp}"; Flags: deleteafterinstall; Tasks: controllers

[Icons]
Name: "{group}\InPhase Host";        Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; IconFilename: "{app}\inphase.ico"; Comment: "Start InPhase Host"
Name: "{group}\InPhase Diagnostics"; Filename: "{app}\{#AppExe}"; Parameters: "--doctor"; WorkingDir: "{app}"; IconFilename: "{app}\inphase.ico"; Comment: "Check this PC's InPhase environment"
Name: "{group}\Open InPhase (this PC)"; Filename: "http://127.0.0.1:47800/?dashboard"; IconFilename: "{app}\inphase.ico"
Name: "{group}\Uninstall InPhase Host"; Filename: "{uninstallexe}"

[InstallDelete]
; Retire the old shared certificate directory. Each user now gets a
; DPAPI-protected CA under their own profile. Other devices must trust it again.
Type: filesandordirs; Name: "{commonappdata}\InPhase\tls"

[Run]
Filename: "{sys}\certutil.exe"; Parameters: "-delstore -f Root ""InPhase Local CA"""; Flags: runhidden waituntilterminated
Filename: "{tmp}\{#ViGEmSetup}"; Parameters: "/exenoui /qn /norestart"; StatusMsg: "Installing controller support (ViGEmBus driver)..."; Flags: waituntilterminated; Tasks: controllers
Filename: "{app}\{#AppExe}"; Parameters: "--setup-firewall"; StatusMsg: "Configuring Windows Firewall..."; Flags: runhidden waituntilterminated
Filename: "{app}\{#AppExe}"; Parameters: "--trust-ca"; StatusMsg: "Setting up your InPhase certificate..."; Flags: runhidden waituntilterminated runasoriginaluser; Check: ShouldConfigureCertificate
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
// ViGEmBus registers a kernel service; its key is the reliable "already there".
function ViGEmBusInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\ViGEmBus');
end;

// The package regression test isolates upgrade behavior from certificate-store
// behavior on GitHub's headless Windows runner. Normal installs never set this.
function ShouldConfigureCertificate: Boolean;
begin
  Result := Lowercase(ExpandConstant('{param:SkipCertificateSetup|no}')) <> 'yes';
end;

// Stop the running tray host deterministically before [Files] is processed.
// Setup is elevated, so this also handles a host started by the original user.
// taskkill returns 128 when no matching process exists.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  if not Exec(ExpandConstant('{sys}\\taskkill.exe'),
              '/F /T /IM "{#AppExe}"', '',
              SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    Result := 'Setup could not start Windows taskkill to close InPhase Host. ' +
              'Close InPhase Host from the system tray and run Setup again.';
    exit;
  end;

  if (ResultCode <> 0) and (ResultCode <> 128) then
  begin
    Result := 'Setup could not close InPhase Host (taskkill exit code ' +
              IntToStr(ResultCode) + '). Close it from the system tray or Task Manager, then try again.';
    exit;
  end;

  // Process termination can complete just before Windows releases the image file.
  Sleep(750);
end;

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
