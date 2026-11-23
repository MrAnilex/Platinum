@echo off
rem Installs the release build of Platinum (command line, launcher, window and icon) into
rem %USERPROFILE%\Platinum, registers it to start with the Windows session, makes the "Platinum"
rem Start menu shortcut, then (re)starts the background service.
rem
rem Build first: cargo build --release --workspace (see README).
rem
rem Sound and microphone pass without Platinum for a few seconds while the background service is
rem stopped, the files copied and the service started again. An open Platinum window is closed.
rem Settings (%USERPROFILE%\Platinum\platinum.ini) are never replaced.
rem
rem Everything is logged to %USERPROFILE%\Platinum\install.log.
rem This file must keep Windows line endings (CRLF): cmd.exe misreads labels otherwise.

setlocal EnableExtensions
set "ROOT=%~dp0.."
set "SRC=%ROOT%\target\release"
set "ICO=%ROOT%\ui\platinum.ico"
set "DST=%USERPROFILE%\Platinum"
set "OUT=%DST%\install.log"

if not exist "%SRC%\platinum.exe" goto missing
if not exist "%SRC%\platinum-start.exe" goto missing
if not exist "%SRC%\platinum-ui.exe" goto missing
if not exist "%DST%" mkdir "%DST%"

echo [%date% %time%] installing Platinum from %SRC% to %DST% > "%OUT%"
echo Installing Platinum into %DST% ...

echo [%date% %time%] stopping the background service >> "%OUT%"
if exist "%DST%\platinum.exe" "%DST%\platinum.exe" daemon --stop >> "%OUT%" 2>&1
rem The service may run as platinum.old.exe when platinum.exe was replaced while it ran.
set /a n=0
:wait
tasklist /FI "IMAGENAME eq platinum.exe" /NH 2>nul | find /I "platinum.exe" >nul
if not errorlevel 1 goto still
tasklist /FI "IMAGENAME eq platinum.old.exe" /NH 2>nul | find /I "platinum.old.exe" >nul
if errorlevel 1 goto stopped
:still
if %n% geq 20 goto stuck
set /a n+=1
ping -n 2 127.0.0.1 >nul
goto wait
:stuck
echo [%date% %time%] WARNING: Platinum still runs after 20 s, copying may fail >> "%OUT%"
goto window
:stopped
echo [%date% %time%] background service stopped after %n% s >> "%OUT%"

:window
rem An open window keeps platinum-ui.exe from being replaced: ask it to close, force it after 5 s.
taskkill /IM platinum-ui.exe >> "%OUT%" 2>&1
set /a w=0
:closing
tasklist /FI "IMAGENAME eq platinum-ui.exe" /NH 2>nul | find /I "platinum-ui.exe" >nul
if errorlevel 1 goto closed
if %w% geq 5 goto force
set /a w+=1
ping -n 2 127.0.0.1 >nul
goto closing
:force
taskkill /F /IM platinum-ui.exe >> "%OUT%" 2>&1
ping -n 2 127.0.0.1 >nul
:closed

echo [%date% %time%] copying >> "%OUT%"
copy /Y "%SRC%\platinum.exe" "%DST%\" >> "%OUT%" 2>&1
copy /Y "%SRC%\platinum-start.exe" "%DST%\" >> "%OUT%" 2>&1
copy /Y "%SRC%\platinum-ui.exe" "%DST%\" >> "%OUT%" 2>&1
copy /Y "%ICO%" "%DST%\" >> "%OUT%" 2>&1
del /Q "%DST%\*.old.exe" >> "%OUT%" 2>&1

echo [%date% %time%] start with the session >> "%OUT%"
rem Writes platinum.ini if it is missing and the HKCU Run value "Platinum".
"%DST%\platinum.exe" autostart enable >> "%OUT%" 2>&1

echo [%date% %time%] Start menu shortcut >> "%OUT%"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s = (New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Programs') + '\Platinum.lnk'); $s.TargetPath = '%DST%\platinum-ui.exe'; $s.WorkingDirectory = '%DST%'; $s.IconLocation = '%DST%\platinum-ui.exe,0'; $s.Description = 'Audio mixer and effects'; $s.Save(); 'shortcut: ' + $s.FullName" >> "%OUT%" 2>&1
ie4uinit.exe -show >> "%OUT%" 2>&1

echo [%date% %time%] starting the background service >> "%OUT%"
start "" "%DST%\platinum-start.exe" daemon
ping -n 5 127.0.0.1 >nul
"%DST%\platinum.exe" daemon --status >> "%OUT%" 2>&1
"%DST%\platinum.exe" daemon --status
echo [%date% %time%] done >> "%OUT%"
echo Done. Open "Platinum" from the Start menu. Log: %OUT%
goto end

:missing
echo platinum.exe, platinum-start.exe or platinum-ui.exe is missing from %SRC%: build first with
echo   cargo build --release --workspace

:end
endlocal
