' Launch the InPhase host windowless, with stdout/stderr appended to host.log.
'
' Windowless because closing the console window used to kill the host; a
' persistent log because every diagnosis this project has needed started with
' "what did the host say at the time".
'
' Rotates first: the log is append-only and had reached 58 MB, at which point
' reading it is slow enough that people stop doing it - and a log nobody opens
' is the same as no log. One previous generation is kept.

Option Explicit

Const LOG_DIR  = "C:\ProgramData\InPhase"
Const LOG_PATH = "C:\ProgramData\InPhase\host.log"
Const PREV_PATH = "C:\ProgramData\InPhase\host.log.1"
Const MAX_BYTES = 20971520   ' 20 MiB

Dim fso, f
Set fso = CreateObject("Scripting.FileSystemObject")

If Not fso.FolderExists(LOG_DIR) Then
  fso.CreateFolder LOG_DIR
End If

On Error Resume Next
If fso.FileExists(LOG_PATH) Then
  Set f = fso.GetFile(LOG_PATH)
  If f.Size > MAX_BYTES Then
    If fso.FileExists(PREV_PATH) Then fso.DeleteFile PREV_PATH, True
    fso.MoveFile LOG_PATH, PREV_PATH
  End If
End If
On Error GoTo 0

CreateObject("WScript.Shell").Run _
  "cmd /c """"C:\Program Files\InPhase\InPhaseHost.exe"" >> " & LOG_PATH & " 2>&1""", 0
