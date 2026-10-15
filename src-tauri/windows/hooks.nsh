; Hooks into Tauri's NSIS installer and uninstaller (bundle.windows.nsis
; .installerHooks), for companion/distribution.md DST-R10 and DST-R14.
;
; Tauri includes this file near the top of its installer.nsi, before the
; template declares its variables, so the functions here can't use them. The
; NSIS_HOOK_* macros are inserted later, inside the Install and Uninstall
; sections, and can ($UpdateMode, $PassiveMode, ${MAINBINARYNAME}).

!include LogicLib.nsh

; The removal wait (DST Tunables): how long the uninstaller lets --remove run
; before it ends it (DST-R14 steps 2 and 3).
!define PAGLE_REMOVAL_WAIT_MS 30000
; How long --quit gets to hand over to a running Companion.
!define PAGLE_QUIT_WAIT_MS 10000
; How many times, 250 ms apart, to look whether a closing Companion has exited
; before Tauri's own running-app check takes over (10 s).
!define PAGLE_EXIT_POLLS 40

; The installer sets this in its own environment. An old uninstaller it runs
; ("Uninstall before installing" on its reinstall page) inherits it, and knows
; that it runs during an install over an existing one, not a real uninstall.
; Tauri runs it there without /UPDATE (in update mode it doesn't run it at
; all), so /UPDATE alone can't tell the two apart. Picking "Uninstall Pagle
; Companion" on the reinstall page of the same version counts as an install
; over too: Tauri's installer goes on to install afterwards.
;
; An installer sets the marker and an older version's uninstaller reads it, so
; its name never changes.
!define PAGLE_INSTALLER_ENV "PAGLE_COMPANION_INSTALLER"

; Tauri's finish page offers a desktop shortcut through MUI's "show readme"
; checkbox, ticked by default. The Companion has no desktop shortcut (DST-R10),
; so it starts unticked; a Player who ticks it gets one.
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED

Var PagleHadDesktopShortcut
Var PagleKeepState
Var PagleRunValue

; Runs when the installer's window opens, before its reinstall page. Silent
; installs have no window and never run the old uninstaller.
!define MUI_CUSTOMFUNCTION_GUIINIT PagleMarkInstaller
Function PagleMarkInstaller
  System::Call 'kernel32::SetEnvironmentVariable(t "${PAGLE_INSTALLER_ENV}", t "1")'
FunctionEnd

; Closes a running Companion the gentle way: `--quit` through the
; single-instance channel (CMP-R2), then waits for its process to exit, seen
; when its exe can be opened for writing again. Tauri's CheckIfAppIsRunning,
; which follows, is the fallback (DST-R10, DST-R14 step 1).
!macro PAGLE_CLOSE_COMPANION
  ${If} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    DetailPrint "Closing ${PRODUCTNAME}"
    nsExec::Exec /TIMEOUT=${PAGLE_QUIT_WAIT_MS} '"$INSTDIR\${MAINBINARYNAME}.exe" --quit'
    Pop $0
    StrCpy $1 0
    ${Do}
      ClearErrors
      FileOpen $0 "$INSTDIR\${MAINBINARYNAME}.exe" a
      ${IfNot} ${Errors}
        FileClose $0
        ${Break}
      ${EndIf}
      IntOp $1 $1 + 1
      ${If} $1 >= ${PAGLE_EXIT_POLLS}
        ${Break}
      ${EndIf}
      Sleep 250
    ${Loop}
  ${EndIf}
!macroend

; Install: close the old Companion before its files are replaced (DST-R10).
!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$DESKTOP\${PRODUCTNAME}.lnk"
    StrCpy $PagleHadDesktopShortcut 1
  ${Else}
    StrCpy $PagleHadDesktopShortcut 0
  ${EndIf}
  !insertmacro PAGLE_CLOSE_COMPANION
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Tauri gives silent and passive installs a desktop shortcut; the Companion
  ; has none (DST-R10). A shortcut that was there before the install stays.
  ${If} $PagleHadDesktopShortcut = 0
    Delete "$DESKTOP\${PRODUCTNAME}.lnk"
  ${EndIf}
  ; The Companion this installer may start inherits nothing of the marker.
  System::Call 'kernel32::SetEnvironmentVariable(t "${PAGLE_INSTALLER_ENV}", p 0)'
!macroend

; Uninstall (DST-R14): close the Companion (step 1), run `--remove` from the
; install folder for at most the removal wait and end it after that (steps 2
; and 3); Tauri then deletes the program files, the Start menu shortcut and
; the Installed apps entry whatever `--remove` did. During an update (/UPDATE)
; or an install over an existing one (the installer's marker), `--remove`
; doesn't run and local state is kept, including the autostart entry, which
; Tauri's uninstaller would otherwise delete. The Companion is still closed
; then: its files can't be replaced while it runs, and the installer would
; close it anyway (DST-R10).
!macro NSIS_HOOK_PREUNINSTALL
  ReadEnvStr $0 ${PAGLE_INSTALLER_ENV}
  ${If} $UpdateMode = 1
  ${OrIf} $0 == "1"
    StrCpy $PagleKeepState 1
    ReadRegStr $PagleRunValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
  ${Else}
    StrCpy $PagleKeepState 0
  ${EndIf}

  !insertmacro PAGLE_CLOSE_COMPANION
  ; Tauri's fallback, here too, so --remove never runs beside a Companion
  ; that didn't quit.
  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ${If} $PagleKeepState = 0
  ${AndIf} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    DetailPrint "Removing ${PRODUCTNAME}'s data and link"
    ; nsExec ends the process once it has written nothing for the timeout;
    ; --remove writes nothing to stdout, so that is the whole run.
    nsExec::Exec /TIMEOUT=${PAGLE_REMOVAL_WAIT_MS} '"$INSTDIR\${MAINBINARYNAME}.exe" --remove'
    Pop $0
    ${If} $0 == "timeout"
      DetailPrint "--remove still ran after the removal wait and was ended"
    ${Else}
      DetailPrint "--remove: $0"
    ${EndIf}
  ${EndIf}
!macroend

; After Tauri has deleted the program files, the Start menu shortcut, the
; Installed apps entry and the autostart entry.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $PagleKeepState = 1
    ; Tauri's uninstaller deleted the autostart entry; the install that goes
    ; on keeps it (DST-R10).
    ${If} $PagleRunValue != ""
      WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}" $PagleRunValue
    ${EndIf}
  ${Else}
    ; Tauri keeps the install location (and the installer's language) in
    ; Software\Pagle\Pagle Companion unless "Delete the application data" is
    ; ticked. An uninstall leaves nothing of the install behind, ticked or
    ; not (DST-R14).
    DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty SHCTX "${MANUKEY}"
  ${EndIf}
!macroend
