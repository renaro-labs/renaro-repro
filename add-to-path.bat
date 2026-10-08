@echo off
rem Adds the current directory (or the directory given as the first argument) to your
rem user PATH so `repro` works from any new terminal. Double-click it next to repro.exe.
rem   add-to-path.bat                 add the current directory
rem   add-to-path.bat C:\tools\repro  add a specific directory
rem   add-to-path.bat -Remove         remove the current directory again
setlocal
set "SCRIPT=%~dp0add-to-path.ps1"
if not exist "%SCRIPT%" (
    echo add-to-path.ps1 must be next to this .bat file: "%SCRIPT%"
    set "RC=1"
    goto :done
)

rem A trailing backslash (e.g. C:\) would escape the closing quote, so append a dot.
set "DIR=%CD%"
if "%DIR:~-1%"=="\" set "DIR=%DIR%."

if "%~1"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT%" -Path "%DIR%"
) else if /i "%~1"=="-Remove" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT%" -Path "%DIR%" -Remove
) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT%" -Path "%~f1."
)
set "RC=%ERRORLEVEL%"

:done
rem Keep the window open when started by double-click from Explorer.
echo %CMDCMDLINE% | findstr /i /c:"%~nx0" >nul && pause
exit /b %RC%
