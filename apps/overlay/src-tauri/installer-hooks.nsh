; A running cuw-daemon holds its own exe open, so NSIS cannot replace the copy
; the bundle carries: the overlay updates, the daemon does not, and the skew only
; surfaces at runtime as a 404 on a route the old daemon never had. The template
; checks the main binary only, and it does that *after* this hook — so the
; overlay goes first, because while it lives it restarts the daemon as soon as
; the port goes quiet.
!macro KillWidgetProcesses
  nsis_tauri_utils::KillProcessCurrentUser "cuw-overlay.exe"
  Pop $R0
  nsis_tauri_utils::KillProcessCurrentUser "cuw-daemon.exe"
  Pop $R0
  ; the handle outlives the process by a moment
  Sleep 500
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro KillWidgetProcesses
!macroend

; Same lock on the way out: a live daemon leaves its exe behind.
!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro KillWidgetProcesses
!macroend
