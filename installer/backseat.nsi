; Per-user installer for Backseat (no administrator rights needed, like most chat apps).
; Built by tools\installer.ps1 from the folder tools\bundle.ps1 makes:
;   makensis /DVERSION=0.1.0 /DSRC=dist\backseat /DOUT=dist\backseat-setup.exe installer\backseat.nsi
Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include "MUI2.nsh"
!include "LogicLib.nsh"

!define NAME "Backseat"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${NAME}"

Name "${NAME}"
OutFile "${OUT}"
InstallDir "$LOCALAPPDATA\Programs\${NAME}"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "LegalCopyright" ""

!define MUI_ICON "..\crates\app\icons\icon.ico"
!define MUI_UNICON "..\crates\app\icons\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\backseat.exe"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
; The installer follows the system language: English, Ukrainian or Russian.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Ukrainian"
!insertmacro MUI_LANGUAGE "Russian"

; Closes a running Backseat from this install folder: its files cannot be replaced or
; removed while it runs ("Error opening file for writing: ...\MSVCP140.dll").
!macro CloseRunning
  DetailPrint "Closing Backseat if it is running..."
  ; Politely first (the window closes and the app exits), then for sure. One app runs
  ; per user, so the name is enough.
  nsExec::Exec 'taskkill /IM backseat.exe'
  Pop $0
  ${If} $0 == 0
    Sleep 2500
  ${EndIf}
  nsExec::Exec 'taskkill /F /IM backseat.exe'
  Pop $0
  Sleep 500
!macroend

Section "Backseat" Main
  SectionIn RO
  ; The app was called "streamer" before: remove that install (its files, shortcuts and
  ; entry), quietly and in place.
  ReadRegStr $3 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\streamer" "InstallLocation"
  ${If} $3 != ""
  ${AndIf} ${FileExists} "$3\uninstall.exe"
    DetailPrint "Removing the old streamer install..."
    ExecWait '"$3\uninstall.exe" /S _?=$3'
    Delete "$3\uninstall.exe"
    RMDir "$3"
  ${EndIf}
  !insertmacro CloseRunning
  SetOutPath "$INSTDIR"
  ; A previous version's plugins must not linger next to the new ones.
  RMDir /r "$INSTDIR\gstreamer-1.0"
  File /r "${SRC}\*"

  ; The plugin registry is a cache rebuilt on start; drop the old one.
  Delete "$LOCALAPPDATA\${NAME}\gst-registry-*.bin"

  CreateShortcut "$SMPROGRAMS\${NAME}.lnk" "$INSTDIR\backseat.exe"
  CreateShortcut "$DESKTOP\${NAME}.lnk" "$INSTDIR\backseat.exe"

  ; Invite links: backseat://join/CODE opens the app (the page at /j/CODE links here).
  WriteRegStr HKCU "Software\Classes\backseat" "" "URL:Backseat"
  WriteRegStr HKCU "Software\Classes\backseat" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\backseat\DefaultIcon" "" "$INSTDIR\backseat.exe,0"
  WriteRegStr HKCU "Software\Classes\backseat\shell\open\command" "" '"$INSTDIR\backseat.exe" "%1"'

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\backseat.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ; Size in KB for "Apps & features".
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" ${SIZE_KB}
SectionEnd

Section "Uninstall"
  !insertmacro CloseRunning
  Delete "$SMPROGRAMS\${NAME}.lnk"
  Delete "$DESKTOP\${NAME}.lnk"
  RMDir /r "$INSTDIR\gstreamer-1.0"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\backseat.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ; Logs, the plugin cache and the saved places go; the rooms and settings (state.json)
  ; stay, as they did in the WebView2 profile, for a later install.
  RMDir /r "$LOCALAPPDATA\${NAME}\logs"
  Delete "$LOCALAPPDATA\${NAME}\gst-registry-*.bin"
  Delete "$LOCALAPPDATA\${NAME}\places.json"
  RMDir "$LOCALAPPDATA\${NAME}"
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\Classes\backseat"
SectionEnd
