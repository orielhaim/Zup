@echo off
rem The Demo App "program".
rem
rem It is a batch file rather than a compiled binary so that this project stays a
rem few readable files with nothing to build. Everything Zup does with it - placing
rem it, reporting it in a plan, repairing it, removing it - is the same work it
rem does with any other file, and a shortcut to it behaves the same way.

setlocal
if /I "%~1"=="--installed" goto installed
if /I "%~1"=="--version" goto version

echo Demo App 1.0.0
echo.
echo This program is part of the Demo App demo project.
echo Run it with --version for the version, or --installed to see that it was
echo started from the installed copy rather than from the project.
echo.
echo The install directory this copy is running from is:
echo   %~dp0
exit /b 0

:version
echo Demo App 1.0.0
exit /b 0

:installed
echo Demo App 1.0.0, running from:
echo   %~dp0
echo.
echo Remove this from PATH to prove that uninstall took it back out again.
exit /b 0
