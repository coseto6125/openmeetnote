@echo off
REM Windows build. Three things this script exists for, each one cost an hour:
REM  1. MSVC env must be loaded first, or the linker cannot find kernel32.lib.
REM  2. cargo may live outside the default location; CARGO_HOME is honoured.
REM  3. cmd refuses to run from a UNC path, so cd to a real drive first.
setlocal

if not defined VSINSTALLDIR (
  for %%p in (
    "C:\Program Files\Microsoft Visual Studio\2022\Community"
    "C:\Program Files\Microsoft Visual Studio\2022\Professional"
    "C:\Program Files\Microsoft Visual Studio\2022\BuildTools"
  ) do if exist "%%~p\VC\Auxiliary\Build\vcvars64.bat" (
    call "%%~p\VC\Auxiliary\Build\vcvars64.bat" >nul
    goto :vcdone
  )
  echo [build-win] Visual Studio 2022 C++ build tools not found.
  exit /b 1
)
:vcdone

if defined CARGO_HOME set "PATH=%CARGO_HOME%\bin;%PATH%"
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

REM NUL is the null device in cmd. `>/dev/null` makes cmd try to create the
REM file \dev\null, which fails when \dev does not exist -- so the check below
REM reported "cargo not on PATH" on every machine, cargo installed or not.
where cargo >nul 2>&1 || (
  echo [build-win] cargo not on PATH. Set CARGO_HOME or install rustup.
  exit /b 1
)

cd /d "%~dp0"
call pnpm.cmd install --frozen-lockfile || exit /b 1
REM Neither flag is optional. bundle-dlls.windows.json declares the
REM sherpa/onnxruntime DLLs as bundle resources with the glob
REM `target/*/release/*.dll`, and that only matches when the build has a target
REM triple. Without it the installers come out missing the DLLs, and the app
REM they install dies in the loader before main -- which is exactly what 0.1.0
REM shipped. The file is not named tauri.windows.conf.json because that name is
REM auto-loaded by every cargo invocation, including a debug `cargo clippy`,
REM where target/*/release/ holds no DLLs and the build script aborts.
call pnpm.cmd tauri build --target x86_64-pc-windows-msvc --config src-tauri/bundle-dlls.windows.json || exit /b 1
echo [build-win] bundles are under src-tauri\target\x86_64-pc-windows-msvc\release\bundle
