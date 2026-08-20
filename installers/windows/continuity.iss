; Inno Setup script for the Continuity Windows installer.
;
; Compiled by CI (ISCC.exe, installed via choco) after continuityd.exe is
; built — see the "windows" leg of the desktop job in
; .github/workflows/release.yml, which copies the freshly-built exe next
; to this script before invoking iscc. Not meant to be run standalone
; against a bare checkout.
;
; Why an installer at all: a raw .exe with no install step means no
; Start Menu entry, no autostart registration, and no clean way to
; uninstall — a normal Rust binary just isn't discoverable as an app.
; This wraps continuityd.exe with the install/uninstall/autostart
; scaffolding users expect from a background sync tool.

#define MyAppName "Continuity"
#define MyAppVersion "0.1.5-beta.10"
#define MyAppPublisher "hardope"
#define MyAppURL "https://github.com/hardope/continuity"
#define MyAppExeName "continuityd.exe"

[Setup]
AppId={{DE73E4CD-2239-42D4-99F9-8596F72F63B6}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
; Per-user install (Program Files if run elevated, %LocalAppData%\Programs
; otherwise) so it can register autostart without needing a UAC prompt,
; and so running the installer doesn't require admin rights at all.
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=continuity-windows-setup
OutputDir=.
Compression=lzma
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}
; Unsigned — Explorer will still show the ISCC-generated default icon
; for the shortcut/uninstall entry rather than continuityd's tray icon,
; since that icon is generated at runtime (see build_icon() in
; core/continuityd/src/main.rs) rather than embedded as a PE resource.

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startup"; Description: "Launch {#MyAppName} automatically when you sign in"; GroupDescription: "Startup:"; Flags: checkedonce

[Files]
Source: "{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: """{app}\{#MyAppExeName}"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName} now"; Flags: nowait postinstall skipifsilent runasoriginaluser

; continuityd persists two things Inno Setup has no way to know about on
; its own, since neither is installed by this script — they're created by
; the app itself the first time it runs, well after setup finishes:
;   - the trust store + config, written via the `directories` crate to
;     %APPDATA%\continuity\continuity\config (see
;     continuity-crypto::TrustStore::default_path — "continuity" appears
;     twice because ProjectDirs::from("app", "continuity", "continuity")
;     uses the same string for both organization and application)
;   - the device's private key, in Windows Credential Manager via the
;     `keyring` crate's windows-native backend, under a generic credential
;     whose target name is "{user}.{service}" per that backend's default
;     naming (continuity-crypto::identity's KEYRING_USER/KEYRING_SERVICE
;     constants: "device-signing-key" and "app.continuity.identity.default"
;     for a real, non-test-profile install)
; Neither was cleaned up by uninstalling before, which is exactly the
; "leftover data needs manual removal" a user reported. Both are covered
; here now. `%APPDATA%\continuity` is unambiguous — nothing else on a
; user's machine should be writing there — so removing the whole tree
; (not just the `config` subfolder) is safe and complete.
[UninstallDelete]
Type: filesandordirs; Name: "{userappdata}\continuity"

[UninstallRun]
; cmdkey exits non-zero if the credential doesn't exist (e.g. the app was
; installed but never actually run, so no identity was ever generated) —
; that's fine, Inno Setup doesn't treat a non-zero exit here as a failed
; uninstall.
Filename: "{cmd}"; Parameters: "/C cmdkey /delete:device-signing-key.app.continuity.identity.default"; Flags: runhidden; RunOnceId: "RemoveContinuityCredential"
