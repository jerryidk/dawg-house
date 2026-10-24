; NSIS installer for Studio on Windows (x86_64).
; Built by scripts/release.sh:
;   makensis -DVERSION=0.1.0 -DEXE=path\to\studio.exe -DOUTFILE=dist\studio-0.1.0-windows-x86_64-setup.exe studio.nsi

Unicode true
!include "MUI2.nsh"
!include "x64.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef EXE
  !error "pass -DEXE=<path to studio.exe>"
!endif
!ifndef OUTFILE
  !define OUTFILE "studio-${VERSION}-windows-x86_64-setup.exe"
!endif

!define APP "Studio"
!define PUBLISHER "jerryidk"
!define URL "https://github.com/jerryidk/dawg-house"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Studio"

Name "${APP} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP}"
InstallDirRegKey HKLM "Software\${APP}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "MIT License"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\studio.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${APP} requires 64-bit Windows."
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

Section "${APP} (required)" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "/oname=studio.exe" "${EXE}"
  File "..\..\LICENSE"
  File "/oname=README.md" "..\..\README.md"
  File "/oname=WINDOWS.md" "..\..\WINDOWS.md"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortcut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\studio.exe"
  CreateShortcut "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\${APP}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINST_KEY}" "URLInfoAbout" "${URL}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\studio.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  SectionGetSize ${SecMain} $0
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" $0
SectionEnd

Section "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\studio.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "The ${APP} application."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Add a shortcut to the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$INSTDIR\studio.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\WINDOWS.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  Delete "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk"
  RMDir "$SMPROGRAMS\${APP}"
  Delete "$DESKTOP\${APP}.lnk"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "Software\${APP}"
SectionEnd
