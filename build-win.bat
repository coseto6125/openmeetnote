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

REM whisper-rs-sys and sherpa-rs-sys build their C++ through the cmake crate,
REM which defaults to the "Visual Studio 17 2022" generator. That generator
REM locates the toolchain through vswhere, which reads the installer's instance
REM records under ProgramData\Microsoft\VisualStudio\Packages\_Instances. Delete
REM that directory to reclaim space and every VS generator build dies with
REM "could not find any instance of Visual Studio" while cl.exe still works
REM fine. Ninja takes the compiler from the vcvars environment instead, needs no
REM instance records, and builds ggml faster than MSBuild does. Both ship inside
REM VS itself, so this adds no new dependency.
set "VSCMAKE=%VSINSTALLDIR%Common7\IDE\CommonExtensions\Microsoft\CMake"
if exist "%VSCMAKE%\Ninja\ninja.exe" (
  set "PATH=%VSCMAKE%\CMake\bin;%VSCMAKE%\Ninja;%PATH%"
  if not defined CMAKE_GENERATOR set "CMAKE_GENERATOR=Ninja"
)

REM whisper-rs-sys and sherpa-rs-sys both run bindgen, which loads libclang.dll
REM at build time. VS ships clang-format/clang-tidy without it, so a working
REM MSVC install is not enough. CI never hits this: the windows-latest runner
REM has LLVM preinstalled (choco `llvm`, pinned to major 20 by the toolset).
if not defined LIBCLANG_PATH (
  for %%p in (
    "C:\Program Files\LLVM\bin"
    "%VSINSTALLDIR%VC\Tools\Llvm\x64\bin"
  ) do if exist "%%~p\libclang.dll" (
    set "LIBCLANG_PATH=%%~p"
    goto :clangdone
  )
  echo [build-win] libclang.dll not found; bindgen cannot run.
  echo [build-win] Install it with: choco install llvm --version 20.1.8
  echo [build-win] Or set LIBCLANG_PATH to a directory containing libclang.dll.
  exit /b 1
)
:clangdone

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
