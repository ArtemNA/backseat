; Per-user installer for streamer (no administrator rights needed, like most chat apps).
; Built by tools\installer.ps1 from the folder tools\bundle.ps1 makes:
;   makensis /DVERSION=0.1.0 /DSRC=dist\streamer /DWEBVIEW2=...\MicrosoftEdgeWebview2Setup.exe /DOUT=dist\streamer-setup.exe installer\streamer.nsi
Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include "MUI2.nsh"
!include "LogicLib.nsh"

!define NAME "streamer"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${NAME}"
; WebView2 Runtime client id (Microsoft documentation, "Detect if a WebView2 Runtime is installed").
!define WV2_ID "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

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
!define MUI_FINISHPAGE_RUN "$INSTDIR\streamer.exe"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
; The installer follows the system language: English, Ukrainian or Russian.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Ukrainian"
!insertmacro MUI_LANGUAGE "Russian"

LangString NoWebView2 ${LANG_ENGLISH} "streamer needs Microsoft Edge WebView2 Runtime, and it could not be installed (is the computer offline?).$\r$\n$\r$\nConnect to the internet and run this installer again, or install WebView2 from https://go.microsoft.com/fwlink/p/?LinkId=2124703"
LangString NoWebView2 ${LANG_UKRAINIAN} "Для streamer потрібен Microsoft Edge WebView2 Runtime, але встановити його не вдалося (немає інтернету?).$\r$\n$\r$\nПідключіться до інтернету й запустіть інсталятор ще раз або встановіть WebView2 з https://go.microsoft.com/fwlink/p/?LinkId=2124703"
LangString NoWebView2 ${LANG_RUSSIAN} "Для streamer нужен Microsoft Edge WebView2 Runtime, но установить его не удалось (нет интернета?).$\r$\n$\r$\nПодключитесь к интернету и запустите установщик ещё раз или установите WebView2 с https://go.microsoft.com/fwlink/p/?LinkId=2124703"

Function HasWebView2
  ; Leaves "1" on the stack when a WebView2 Runtime is installed for the machine or the user.
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WV2_ID}" "pv"
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKCU "Software\Microsoft\EdgeUpdate\Clients\${WV2_ID}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    Push "0"
  ${Else}
    Push "1"
  ${EndIf}
FunctionEnd

Section "streamer" Main
  SectionIn RO
  SetOutPath "$INSTDIR"
  ; A previous version's plugins must not linger next to the new ones.
  RMDir /r "$INSTDIR\gstreamer-1.0"
  File /r "${SRC}\*"

  Call HasWebView2
  Pop $1
  ${If} $1 == "0"
    DetailPrint "Installing Microsoft Edge WebView2 Runtime..."
    SetOutPath "$PLUGINSDIR"
    File "/oname=MicrosoftEdgeWebview2Setup.exe" "${WEBVIEW2}"
    ExecWait '"$PLUGINSDIR\MicrosoftEdgeWebview2Setup.exe" /silent /install' $2
    DetailPrint "WebView2 setup exit code: $2"
    SetOutPath "$INSTDIR"
    ; The bootstrapper downloads the runtime; without internet it cannot.
    Call HasWebView2
    Pop $1
    ${If} $1 == "0"
      DetailPrint "WebView2 Runtime is still missing"
      MessageBox MB_OK|MB_ICONEXCLAMATION "$(NoWebView2)" /SD IDOK
    ${EndIf}
  ${EndIf}

  ; The plugin registry is a cache rebuilt on start; drop the old one.
  Delete "$LOCALAPPDATA\${NAME}\gst-registry-*.bin"

  CreateShortcut "$SMPROGRAMS\${NAME}.lnk" "$INSTDIR\streamer.exe"
  CreateShortcut "$DESKTOP\${NAME}.lnk" "$INSTDIR\streamer.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\streamer.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ; Size in KB for "Apps & features".
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" ${SIZE_KB}
SectionEnd

Section "Uninstall"
  Delete "$SMPROGRAMS\${NAME}.lnk"
  Delete "$DESKTOP\${NAME}.lnk"
  RMDir /r "$INSTDIR\gstreamer-1.0"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\streamer.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ; Logs and the plugin cache; the user's settings in the WebView2 profile stay.
  RMDir /r "$LOCALAPPDATA\${NAME}"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
