; Toru-style silent update hooks for Hato's NSIS installer.
;
; PREINSTALL: when running silently (/S), wait until Hato.exe is unlocked so
; the running app (which just quit to update) finishes releasing the file lock.
; POSTINSTALL: relaunch the app after a silent install so the user comes back
; on the new version automatically.

!include "LogicLib.nsh"

!macro NSIS_HOOK_PREINSTALL
  ${If} ${Silent}
    ; Wait up to ~10s for Hato.exe to unlock (rename probe, same as Toru).
    StrCpy $R9 0
    silent_wait_loop:
      IntOp $R9 $R9 + 1
      ${If} $R9 > 50
        Goto silent_wait_done
      ${EndIf}
      ${If} ${FileExists} "$INSTDIR\Hato.exe"
        Rename "$INSTDIR\Hato.exe" "$INSTDIR\Hato.exe.old"
        ${If} ${Errors}
          ClearErrors
          Sleep 200
          Goto silent_wait_loop
        ${Else}
          Rename "$INSTDIR\Hato.exe.old" "$INSTDIR\Hato.exe"
          Goto silent_wait_done
        ${EndIf}
      ${Else}
        Goto silent_wait_done
      ${EndIf}
    silent_wait_done:
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ${If} ${Silent}
    ; Relaunch after silent update (interactive installs use the Finish page).
    Exec '"$INSTDIR\Hato.exe"'
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
