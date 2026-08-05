; NSIS installer for Boundless (Windows).
;
; Built in CI (release.yml) with:
;   makensis /DVERSION=<x.y.z> /DTAG=<vX.Y.Z> /DEXEPATH=<path\to\boundless.exe> installer/installer.nsi
;
; Design decisions:
;   - Per-user install ($LOCALAPPDATA\Programs\Boundless), no UAC prompt:
;     matches VS Code's model and lets the in-app auto-updater replace the
;     exe in place without admin rights (see src/updater.rs).
;   - The exe already carries the embedded app icon (build.rs + icon.ico), so
;     shortcuts and the Add/Remove-Programs entry pick it up automatically.

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef TAG
  !define TAG "vdev"
!endif
; Path to the pre-built exe; resolved relative to this .nsi file by default.
!ifndef EXEPATH
  !define EXEPATH "boundless.exe"
!endif

!define APPNAME "Boundless"
!define EXE     "boundless.exe"
!define UNINSTKEY "Boundless"
!define UNINSTREG "Software\Microsoft\Windows\CurrentVersion\Uninstall\${UNINSTKEY}"

Name "${APPNAME} ${VERSION}"
; OutFile may be passed as an absolute Windows path via /DOUTFILE=... so the
; build always lands it at the repo root (where the upload glob looks). Defaults
; to a relative name written next to the script.
!ifndef OUTFILE
  !define OUTFILE "boundless-${TAG}-win-x64-setup.exe"
!endif
OutFile "${OUTFILE}"

; Per-user install: no admin elevation, writable by the user (auto-updater
; needs write access to the install directory).
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\${APPNAME}"
InstallDirRegKey HKCU "${UNINSTREG}" "InstallLocation"

; Simple two-page flow: pick folder, then install.
Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File "/oname=${EXE}" "${EXEPATH}"

  ; Start-menu folder + shortcuts.
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${EXE}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"
  ; Desktop shortcut for quick access.
  CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${EXE}"

  ; Uninstaller + Add/Remove Programs entry (per-user = HKCU).
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINSTREG}" "DisplayName"     "${APPNAME}"
  WriteRegStr HKCU "${UNINSTREG}" "DisplayVersion"  "${VERSION}"
  WriteRegStr HKCU "${UNINSTREG}" "DisplayIcon"     "$INSTDIR\${EXE}"
  WriteRegStr HKCU "${UNINSTREG}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTREG}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTREG}" "Publisher"       "boundless"
  ; Estimated size in KB (exe ~30MB; rough is fine for Add/Remove Programs).
  WriteRegDWORD HKCU "${UNINSTREG}" "EstimatedSize" 0x7800
SectionEnd

Section "Uninstall"
  ; Kill a running instance so the exe isn't locked (best-effort).
  nsExec::ExecToLog 'taskkill /IM "${EXE}" /F'
  Sleep 500

  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\${EXE}.old"     ; leftover from an in-place auto-update
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"

  DeleteRegKey HKCU "${UNINSTREG}"
SectionEnd
