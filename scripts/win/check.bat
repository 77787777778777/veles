@echo off
rem scripts/win/check.bat - run on the Windows build host by check.sh.
rem %1 = check | clippy | build | test | all. Log: C:\veles\check.log
rem all = clippy, build, test (clippy covers check); its exit status is the WORST step's rc, not the last's.
rem Every rc is set QUOTED (set "RC=..."): "set RC=%ERRORLEVEL% & echo ..." keeps the space before the "&", so a green step stored "0 ", failed the =="0" test and reset FAIL to "0 " - a red clippy then ended as rc=0 behind a green test step (2026-10-20).
setlocal
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" >nul
set "PATH=C:\veles\tcl\bin;C:\veles\sqlite;C:\Program Files\LLVM\bin;%PATH%"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
set "TCL_LIBRARY=C:\veles\tcl\lib\tcl8.6"
set "TK_LIBRARY=C:\veles\tcl\lib\tk8.6"
rem The shared sqlite3.dll + its import lib (crates/veles-core/Cargo.toml, rusqlite).
set "SQLITE3_LIB_DIR=C:\veles\sqlite"
if not exist C:\veles\tcl\bin\tclsh.exe copy /y C:\veles\tcl\bin\tclsh86t.exe C:\veles\tcl\bin\tclsh.exe >nul
if not exist C:\veles\tcl\bin\wish.exe copy /y C:\veles\tcl\bin\wish86t.exe C:\veles\tcl\bin\wish.exe >nul
rem Racket CS and ECL have no Windows runtime yet (PLAN-windows.md section 10).
rem veles-userland is the phone's exec broker and its shell (PLAN-phone-userland.md T3): Unix sockets, SCM_RIGHTS, SO_PEERCRED, proot. Nothing on Windows links it and it has no Windows form, so the --workspace clippy/check leave it out.
rem build/test use --profile windev (Cargo.toml): the dev dylib has more exports than a PE import lib holds.
rem --no-fail-fast: one red test binary must not hide the others (cargo runs them in package order).
rem "< nul": with the ssh console on stdin, tests that ask whether a person is here (an owner prompt, a ticket paste) wait for a line that never comes. The Linux gate runs with </dev/null for the same reason.
rem No --bin here: with several -p, --bin restricts EVERY package to that target and the modules and the launcher are silently not built.
set "EXCL=--exclude veles-lang-racket --exclude veles-module-lang-racket --exclude veles-lang-lisp --exclude veles-module-lang-lisp --exclude veles-userland"
set "STEP=%~1"
if "%STEP%"=="" set "STEP=check"
cd /d C:\veles\src
echo START %DATE% %TIME% step=%STEP% > C:\veles\check.log
set RC=0
set FAIL=0
if "%STEP%"=="check" goto check
if "%STEP%"=="clippy" goto clippy
if "%STEP%"=="build" goto build
if "%STEP%"=="test" goto test
if "%STEP%"=="all" goto clippy
echo unknown step %STEP% & exit /b 2
:check
echo STEP=check & echo STEP=check >> C:\veles\check.log
cargo check --workspace --all-targets %EXCL% >> C:\veles\check.log 2>&1
set "RC=%ERRORLEVEL%" & echo RC=%ERRORLEVEL% check & echo RC=%ERRORLEVEL% check >> C:\veles\check.log
if not "%RC%"=="0" set "FAIL=%RC%"
if not "%STEP%"=="all" goto done
:clippy
echo STEP=clippy & echo STEP=clippy >> C:\veles\check.log
cargo clippy --workspace --all-targets %EXCL% -- -D warnings >> C:\veles\check.log 2>&1
set "RC=%ERRORLEVEL%" & echo RC=%ERRORLEVEL% clippy & echo RC=%ERRORLEVEL% clippy >> C:\veles\check.log
if not "%RC%"=="0" set "FAIL=%RC%"
if not "%STEP%"=="all" goto done
:build
echo STEP=build & echo STEP=build >> C:\veles\check.log
cargo build --profile windev -p veles-cli -p veles-launcher -p veles-module-echo -p veles-module-irc -p veles-module-telegram -p veles-module-slopdrop -p veles-module-lang-js -p veles-plugin-anthropic-so -p veles-plugin-sample-so -p veles-plugin-badabi-so >> C:\veles\check.log 2>&1
set "RC=%ERRORLEVEL%" & echo RC=%ERRORLEVEL% build & echo RC=%ERRORLEVEL% build >> C:\veles\check.log
if not "%RC%"=="0" set "FAIL=%RC%"
if not "%STEP%"=="all" goto done
:test
echo STEP=test & echo STEP=test >> C:\veles\check.log
set RUST_LOG=info
rem driver_parity.rs and plugin_so.rs load target\windev\veles_plugin_{anthropic,sample,badabi}_so.dll, sibling cdylibs with no dependency edge, so cargo test does not build them: build them here, as scripts/precommit.sh's "plugin .so" step does, so a bare "test" step finds them too. Same profile as the test, or it lands outside target\windev.
rem VELES_REQUIRE_PLUGIN_SO=1, as on Linux: plugin_so.rs takes a "not built" skip that reads as "ok" without it, and on Windows it did exactly that while the test looked for the Unix .so name. With it, a missing .dll FAILS.
cargo build --profile windev -p veles-plugin-anthropic-so -p veles-plugin-sample-so -p veles-plugin-badabi-so -p veles-plugin-echo >> C:\veles\check.log 2>&1
set "PRC=%ERRORLEVEL%"
echo RC=%PRC% test-plugin-build & echo RC=%PRC% test-plugin-build >> C:\veles\check.log
if not "%PRC%"=="0" set "FAIL=%PRC%"
set VELES_REQUIRE_PLUGIN_SO=1
cargo test --no-fail-fast --profile windev -p veles-core -p veles-frontends -p veles-install -p veles-cli -p veles-plugin-sdk -p veles-types < nul >> C:\veles\check.log 2>&1
set "RC=%ERRORLEVEL%" & echo RC=%ERRORLEVEL% test & echo RC=%ERRORLEVEL% test >> C:\veles\check.log
if not "%RC%"=="0" set "FAIL=%RC%"
:done
if not "%FAIL%"=="0" set "RC=%FAIL%"
echo END %DATE% %TIME% >> C:\veles\check.log
exit /b %RC%
