@echo off
cd /d "%~dp0"
if not exist "release\" mkdir "release"
explorer "release"
