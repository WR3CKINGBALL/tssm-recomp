@echo off
rem Builds the Windows port from your disc image: asks for the image, then makes dist\TSSM.exe.
rem An image can also be given:  build.bat "path\to\game.iso"   (see tools\build.ps1)
if not exist "%~dp0tools\build.ps1" (
    echo This build.bat has to stay in the folder it came in, next to the tools, crates and
    echo config folders. Unzip the whole package and run build.bat from there.
    pause
    exit /b 1
)
if "%~1"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\build.ps1"
) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\build.ps1" -Iso "%~1"
)
pause
