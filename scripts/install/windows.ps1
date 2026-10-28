<#
.SYNOPSIS
  scripts/install/windows.ps1 — install veles on Windows 10/11, natively.

.DESCRIPTION
  The Windows counterpart of scripts/install/install.sh (PLAN-windows.md
  §4), step for step: dependencies through winget, Tcl/Tk 8.6.16 built
  from source, veles and its modules built in release, installed under
  the prefix, the gateway registered as a logon task — and then PROVEN by
  running it: `veles version`, `veles doctor` and `veles doctor --sandbox`,
  the last of which executes a script through the fence and must print
  42. A script that says "installed" without running the thing it
  installed is the defect common.sh exists to prevent.

  Every install is a source build: veles links libtcl through bindgen,
  so the binary is bound to the Tcl this script builds beside it.

.PARAMETER Prefix
  Where to install. Default: $env:LOCALAPPDATA\Programs\veles
  (bin\ and tcl\ under it).
.PARAMETER SkipDeps
  Do not touch winget; build and verify with what is on the machine.
.PARAMETER WithWireGuard
  Also install the WireGuard app (for a pool node).
.PARAMETER DefenderExclusion
  Add the repo's target\ directory to Defender's exclusions (a cargo
  build without it is markedly slower). Needs an elevated shell.
.PARAMETER TclVersion
  The Tcl/Tk version to build. Default 8.6.16 (the flake's).
.PARAMETER TcllibVersion
  The tcllib to install beside it (the desktop needs its json package).
  Default 2.0 (the flake's).
.PARAMETER SqliteZip
  The sqlite.org DLL bundle to use when the download page cannot be
  read for the current one (a relative URL under https://sqlite.org/).
.PARAMETER PortableZip
  Build and write the portable release zip to this path instead of
  installing (the GitHub release lane and the release proof use it).
  Nothing under the prefix except tcl\ and sqlite\ is touched.
.PARAMETER Revision
  The short source revision in the zip name when the tree has no .git.

.EXAMPLE
  pwsh -ExecutionPolicy Bypass -File scripts\install\windows.ps1
.EXAMPLE
  pwsh -File scripts\install\windows.ps1 -SkipDeps -PortableZip out\veles.zip
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\veles'),
    [switch]$SkipDeps,
    [switch]$WithWireGuard,
    [switch]$DefenderExclusion,
    [string]$TclVersion = '8.6.16',
    [string]$TcllibVersion = '2.0',
    [string]$SqliteZip = '2026/sqlite-dll-win-x64-3540000.zip',
    [string]$PortableZip,
    [string]$Revision
)
$ErrorActionPreference = 'Stop'
$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Say($m)  { Write-Host ""; Write-Host "▸ $m" -ForegroundColor White }
function Note($m) { Write-Host "  $m" }
function Die($m)  { Write-Host "✗ $m" -ForegroundColor Red; exit 1 }
function Have($name) { $null -ne (Get-Command $name -ErrorAction SilentlyContinue) }

# ── dependencies ─────────────────────────────────────────────────────
#
# Each REQUIRED one is fatal with the winget id to install; an OPTIONAL
# one is named with the capability it costs. The same rule as
# deps/<os>.sh: a dependency that quietly did not install becomes a link
# error twenty minutes into a cargo build, or a feature that silently is
# not there.
function Winget-Install($id) {
    Note "winget install $id"
    & winget install --id $id -e --silent --accept-package-agreements --accept-source-agreements --disable-interactivity | Out-Null
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne -1978335189) {   # already installed
        Die "winget install $id failed (exit $LASTEXITCODE)"
    }
}

function Refresh-Path {
    $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
                [Environment]::GetEnvironmentVariable('Path', 'User')
    if (Test-Path "$env:USERPROFILE\.cargo\bin") { $env:Path += ";$env:USERPROFILE\.cargo\bin" }
}

# Fetch a URL to a file. curl.exe (in Windows since 1803) first: it
# follows SourceForge's mirror redirects to the bytes, where
# Invoke-WebRequest was handed an HTML "choose a mirror" page that tar
# then refused ("Unrecognized archive format", 2026-10-13). The result
# is checked to be a gzip/zip, not a page.
function Fetch($url, $out) {
    if (Have curl.exe) { & curl.exe -sSL -o $out $url } else { Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing }
    if (-not (Test-Path $out)) { Die "download failed: $url" }
    $b = [IO.File]::ReadAllBytes($out)[0..1]
    $gzip = ($b[0] -eq 0x1f -and $b[1] -eq 0x8b); $zip = ($b[0] -eq 0x50 -and $b[1] -eq 0x4b)
    if (-not ($gzip -or $zip)) { Remove-Item $out -Force; Die "download is not an archive (an HTML page?): $url" }
}

function Find-VsDevCmd {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $vs) { return $null }
    $bat = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
    if (Test-Path $bat) { return $bat } else { return $null }
}

Say "veles installer — Windows (native)"
Note "prefix: $Prefix   source: $Repo"

if (-not $SkipDeps) {
    Say "dependencies (winget)"
    if (-not (Have winget)) { Die "winget is not available; install 'App Installer' from the Microsoft Store, or run with -SkipDeps" }
    if (-not (Have git))    { Winget-Install 'Git.Git' }
    if (-not (Have cmake))  { Winget-Install 'Kitware.CMake' }
    if (-not (Have rustup)) { Winget-Install 'Rustlang.Rustup' }
    if (-not (Test-Path "$env:ProgramFiles\LLVM\bin\libclang.dll")) { Winget-Install 'LLVM.LLVM' }
    if (-not (Have nasm))   { try { Winget-Install 'NASM.NASM' } catch { Note "NASM not installed (optional: aws-lc-sys builds its C path without it)" } }
    if (-not (Have rg))     { try { Winget-Install 'BurntSushi.ripgrep.MSVC' } catch { Note "ripgrep not installed (optional: the search_files tool falls back)" } }
    if ($WithWireGuard -and -not (Test-Path "$env:ProgramFiles\WireGuard\wireguard.exe")) { Winget-Install 'WireGuard.WireGuard' }
    if (-not (Find-VsDevCmd)) {
        Note "no MSVC found: installing the Visual Studio 2022 Build Tools with the C++ workload (several GB)"
        & winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-package-agreements --accept-source-agreements `
            --override '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended' | Out-Null
    }
    Refresh-Path
    & rustup toolchain install stable --profile minimal | Out-Null
    & rustup default stable | Out-Null
} else {
    Note "-SkipDeps: not touching winget"
    Refresh-Path
}

$vcvars = Find-VsDevCmd
if (-not $vcvars) { Die "no MSVC C++ toolchain (vcvars64.bat not found). Install the Visual Studio 2022 Build Tools with the 'Desktop development with C++' workload: winget install Microsoft.VisualStudio.2022.BuildTools" }
foreach ($t in 'git','cargo','rustc','cmake') { if (-not (Have $t)) { Die "$t is not on PATH after the dependency step" } }
$libclang = Join-Path $env:ProgramFiles 'LLVM\bin'
if (-not (Test-Path (Join-Path $libclang 'libclang.dll'))) { Die "no libclang.dll under $libclang. veles links libtcl through bindgen, which needs it: winget install LLVM.LLVM" }
Note "msvc:     $vcvars"
Note "rust:     $(& rustc --version)"
Note "libclang: $libclang"

# Runs a command line inside the MSVC environment (vcvars64 sets PATH,
# INCLUDE and LIB for cl, link and nmake). Always through a log file:
# the one call that passed the line to `cmd /c` without the redirect
# had its inner quotes mangled on the way (PowerShell 7's native
# argument passing) and `lib.exe` never ran (2026-10-13).
function In-Msvc([string]$cmdline, [string]$log) {
    if (-not $log) { $log = Join-Path $env:TEMP 'veles-msvc.log' }
    $full = "call `"$vcvars`" >nul && $cmdline"
    & cmd /c "$full > `"$log`" 2>&1"
    return $LASTEXITCODE
}

# ── Tcl/Tk from source ───────────────────────────────────────────────
$TclDir = Join-Path $Prefix 'tcl'
$tclsh = Join-Path $TclDir 'bin\tclsh.exe'
if (Test-Path $tclsh) {
    Say "Tcl/Tk: already at $TclDir"
} else {
    Say "Tcl/Tk $TclVersion from source → $TclDir"
    $build = Join-Path $env:TEMP "veles-tcl-build"
    New-Item -ItemType Directory -Force $build | Out-Null
    foreach ($pkg in 'tcl','tk') {
        $tgz = Join-Path $build "$pkg$TclVersion-src.tar.gz"
        if (-not (Test-Path $tgz)) {
            $url = "https://downloads.sourceforge.net/project/tcl/Tcl/$TclVersion/$pkg$TclVersion-src.tar.gz"
            Note "download $url"
            Fetch $url $tgz
        }
        if (-not (Test-Path (Join-Path $build "$pkg$TclVersion"))) { & tar -xzf $tgz -C $build }
    }
    $tclSrc = Join-Path $build "tcl$TclVersion"
    $tkSrc  = Join-Path $build "tk$TclVersion"
    foreach ($step in @(
        @("cd /d `"$tclSrc\win`" && nmake -nologo -f makefile.vc release INSTALLDIR=`"$TclDir`"", "tcl build"),
        @("cd /d `"$tclSrc\win`" && nmake -nologo -f makefile.vc install INSTALLDIR=`"$TclDir`"", "tcl install"),
        @("cd /d `"$tkSrc\win`" && nmake -nologo -f makefile.vc release INSTALLDIR=`"$TclDir`" TCLDIR=`"$tclSrc`"", "tk build"),
        @("cd /d `"$tkSrc\win`" && nmake -nologo -f makefile.vc install INSTALLDIR=`"$TclDir`" TCLDIR=`"$tclSrc`"", "tk install")
    )) {
        $log = Join-Path $build ($step[1].Replace(' ','-') + '.log')
        Note "$($step[1]) (log: $log)"
        if ((In-Msvc $step[0] $log) -ne 0) { Die "$($step[1]) failed — see $log" }
    }
    # The engine looks for a bare `tclsh`/`wish`; the source build names
    # them tclsh86t.exe / wish86t.exe. clib's probe (the tcl crate) also
    # finds the headers and tcl86t.lib from `where tclsh`.
    Copy-Item (Join-Path $TclDir 'bin\tclsh86t.exe') $tclsh
    Copy-Item (Join-Path $TclDir 'bin\wish86t.exe') (Join-Path $TclDir 'bin\wish.exe')
}
# tcllib: the desktop's `package require json` (and more) — a source
# Tcl build has none of it, and the app dies at startup without it
# ("can't find package json", seen 2026-10-13). 2.0 is the flake's.
$tcllibDir = Join-Path $TclDir "lib\tcllib$TcllibVersion"
if (-not (Test-Path (Join-Path $tcllibDir 'pkgIndex.tcl'))) {
    Say "tcllib $TcllibVersion → $tcllibDir"
    $build = Join-Path $env:TEMP "veles-tcl-build"
    New-Item -ItemType Directory -Force $build | Out-Null
    $tgz = Join-Path $build "tcllib-$TcllibVersion.tar.gz"
    if (-not (Test-Path $tgz)) {
        Fetch "https://downloads.sourceforge.net/project/tcllib/tcllib/$TcllibVersion/tcllib-$TcllibVersion.tar.gz" $tgz
    }
    $src = Join-Path $build "tcllib-$TcllibVersion"
    if (-not (Test-Path $src)) { & tar -xzf $tgz -C $build }
    Push-Location $src
    & $tclsh installer.tcl -no-gui -no-wait -no-apps -no-html -no-nroff -no-examples -pkg-path $tcllibDir | Out-Null
    $rc = $LASTEXITCODE
    Pop-Location
    if ($rc -ne 0 -or -not (Test-Path (Join-Path $tcllibDir 'pkgIndex.tcl'))) { Die "tcllib did not install into $tcllibDir" }
}
# clib (the tcl crate's probe) asks pkg-config first, then `where tclsh86`,
# then `where tclsh`, and derives the headers from the first match. GitHub's
# Windows image also has Git's ucrt64 tclsh86.exe on PATH (vendor/clib/WHY.md,
# the third patch). So: no pkg-config for Tcl, and this Tcl answers first to
# every name the probe tries.
foreach ($n in 'TCL86', 'TCL', 'TCL8_6') { Set-Item "Env:${n}_NO_PKG_CONFIG" 1 }
if (-not (Test-Path (Join-Path $TclDir 'bin\tclsh86.exe'))) { Copy-Item $tclsh (Join-Path $TclDir 'bin\tclsh86.exe') }
$env:Path = (Join-Path $TclDir 'bin') + ';' + $libclang + ';' + $env:Path
$env:LIBCLANG_PATH = $libclang
$env:TCL_LIBRARY = Join-Path $TclDir "lib\tcl8.6"
$env:TK_LIBRARY  = Join-Path $TclDir "lib\tk8.6"
Note "tclsh: $tclsh"

# ── SQLite as a shared DLL ───────────────────────────────────────────
# rusqlite links the SHARED sqlite on every target (crates/veles-core/
# Cargo.toml says why: core lives in veles_runtime.dll and the exe
# inlines rusqlite code, so both must resolve the same sqlite3_*
# symbols — a bundled copy in the DLL exports none of them). Windows
# has no system sqlite: this is sqlite.org's own DLL (FTS5 compiled
# in), with an import library made from the .def it ships.
$SqliteDir = Join-Path $Prefix 'sqlite'
if (-not (Test-Path (Join-Path $SqliteDir 'sqlite3.lib'))) {
    Say "SQLite (sqlite.org's sqlite3.dll) → $SqliteDir"
    New-Item -ItemType Directory -Force $SqliteDir | Out-Null
    $rel = $null
    try {
        $page = (Invoke-WebRequest -Uri 'https://sqlite.org/download.html' -UseBasicParsing).Content
        if ($page -match '(20\d\d/sqlite-dll-win-x64-\d+\.zip)') { $rel = $Matches[1] }
    } catch { }
    if (-not $rel) { $rel = $SqliteZip; Note "sqlite.org's download page was not readable; using the pinned $rel" }
    $zip = Join-Path $SqliteDir 'sqlite-dll.zip'
    Note "download https://sqlite.org/$rel"
    Fetch "https://sqlite.org/$rel" $zip
    & tar -xf $zip -C $SqliteDir
    if ($LASTEXITCODE -ne 0) { Die "could not unpack $zip" }
    $liblog = Join-Path $SqliteDir 'lib.log'
    if ((In-Msvc "cd /d `"$SqliteDir`" && lib /nologo /DEF:sqlite3.def /OUT:sqlite3.lib /MACHINE:X64" $liblog) -ne 0) { Die "lib.exe could not make sqlite3.lib from sqlite3.def — see $liblog" }
}
$env:SQLITE3_LIB_DIR = $SqliteDir
$env:Path = "$SqliteDir;$env:Path"
Note "sqlite: $SqliteDir\sqlite3.dll"

# ── build ────────────────────────────────────────────────────────────
if ($DefenderExclusion) {
    try { Add-MpPreference -ExclusionPath (Join-Path $Repo 'target'); Note "Defender exclusion added for $Repo\target" }
    catch { Note "could not add the Defender exclusion (needs an elevated shell): $_" }
}
Say "building veles from $Repo (winrelease: release without LTO — Cargo.toml says why)"
Push-Location $Repo
# No --bin: with several -p it restricts every package to that one target.
$crates = '-p veles-cli -p veles-launcher -p veles-module-echo -p veles-module-irc -p veles-module-telegram -p veles-module-slopdrop -p veles-module-lang-js'
$buildLog = Join-Path $env:TEMP 'veles-build.log'
$attempt = 1
while ($true) {
    # clib's build script records where it found Tcl (`where tclsh` → <prefix>\lib
    # as a link-search path) and cargo cannot see that input change: a Tcl
    # prefix that moved or was removed since the last build leaves a cached
    # `rustc-link-search` pointing at nothing and the link fails with
    # LNK1181 tcl86t.lib (the Windows build host, 2026-10-13). Rebuilding the two small
    # Tcl crates every run is cheap; a wrong cached path is not.
    $null = In-Msvc "cargo clean --profile winrelease -p clib -p tcl" $buildLog
    $rc = In-Msvc "cargo build --profile winrelease $crates" $buildLog
    Get-Content $buildLog -Tail 3 | ForEach-Object { Note $_ }
    if ($rc -eq 0) { if ($attempt -gt 1) { Note "the build succeeded on attempt $attempt" }; break }
    $log = Get-Content $buildLog -Raw
    if ($log.Contains('available field is: `_address`')) {
        Pop-Location
        Die "bindgen made the Tcl structs opaque (the libclang at $libclang is newer than the bindgen clib pins). veles ships a patched clib for this — check Cargo.toml's [patch.crates-io] clib = { path = ""vendor/clib"" } and vendor/clib/WHY.md."
    }
    if (-not $log.Contains('could not compile `tcl`')) { Pop-Location; Die "cargo build failed — see $buildLog" }
    switch ($attempt) {
        1 { Note "the tcl crate failed to build: clib/inwelling lose a race with the build dir now and then — retrying" }
        2 { Note "still failing; dropping clib's artifacts so its build script reruns"; & cargo clean -p clib 2>$null }
        default { Pop-Location; Die "cargo build failed 3 times on the tcl crate — see $buildLog and vendor/clib/WHY.md" }
    }
    $attempt++
}
Pop-Location

# ── portable release zip (-PortableZip) ──────────────────────────────
#
# The prebuilt Windows artifact (PLAN-windows.md §10, "Prebuilt Windows
# bundle"): the same build, laid out so nothing has to be installed.
# Tcl's own install layout is the trick: tcl86t.dll in bin\ finds its
# library at bin\..\lib\tcl8.6, so the embedded interpreter, tclsh.exe
# and wish.exe all work from wherever the zip is unpacked, with no
# TCL_LIBRARY/VELES_TCLSH/.env. Rust resolves a bare `tclsh`/`wish`
# against the running exe's directory first, so the engine finds them
# beside veles.exe; modules are found at <exe>\..\lib\veles\modules.
function New-PortableZip([string]$zipPath) {
    $rel = Join-Path $Repo 'target\winrelease'
    $version = $null
    foreach ($line in Get-Content (Join-Path $Repo 'Cargo.toml')) {
        if ($line -match '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') { $version = $Matches[1]; break }
    }
    if (-not $version) { Die "no workspace version in Cargo.toml" }
    $revision = $Revision
    if (-not $revision) {
        $revision = (& git -C $Repo rev-parse --short=7 HEAD 2>$null)
        if ($LASTEXITCODE -ne 0 -or -not $revision) { Die "no git revision for $Repo; pass -Revision" }
        $revision = $revision.Trim()
    }
    $name = "veles-$version-$revision-windows-x86_64"
    $work = Join-Path $env:TEMP 'veles-portable'
    $stage = Join-Path $work $name
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    $bin = Join-Path $stage 'bin'
    $lib = Join-Path $stage 'lib'
    New-Item -ItemType Directory -Force $bin, $lib | Out-Null
    Say "portable zip $name"
    foreach ($f in 'veles.exe','veles-gw.exe','veles_runtime.dll') {
        if (-not (Test-Path (Join-Path $rel $f))) { Die "$f was not built" }
        Copy-Item (Join-Path $rel $f) $bin
    }
    $sysroot = (& rustc --print sysroot).Trim()
    $std = Get-ChildItem (Join-Path $sysroot 'lib\rustlib\x86_64-pc-windows-msvc\lib') -Filter 'std-*.dll' | Select-Object -First 1
    if (-not $std) { Die "no std-*.dll in the toolchain's rustlib dir" }
    Copy-Item $std.FullName $bin
    Get-ChildItem (Join-Path $TclDir 'bin') | Where-Object { $_.Extension -in '.exe','.dll' } |
        ForEach-Object { Copy-Item $_.FullName $bin }
    Copy-Item (Join-Path $SqliteDir 'sqlite3.dll') $bin
    # The MSVC C++ runtime the Rust and Tcl DLLs import (vcruntime140*.dll,
    # msvcp140.dll). A fresh Windows has the UCRT but not necessarily the
    # VC++ redistributable; app-local copies from the Build Tools' own
    # redist directory are the deployment Microsoft documents for this.
    $vsRoot = Split-Path (Split-Path (Split-Path (Split-Path $vcvars)))
    $crt = Get-ChildItem (Join-Path $vsRoot 'VC\Redist\MSVC') -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | ForEach-Object { Get-ChildItem (Join-Path $_.FullName 'x64') -Directory -Filter 'Microsoft.VC14*.CRT' -ErrorAction SilentlyContinue } |
        Select-Object -First 1
    if (-not $crt) { Die "no VC\Redist\MSVC\<ver>\x64\Microsoft.VC14*.CRT under $vsRoot (install the C++ workload's redistributables)" }
    Get-ChildItem $crt.FullName -Filter '*.dll' | ForEach-Object { Copy-Item $_.FullName $bin }
    Note "C++ runtime: $($crt.FullName)"
    # Tcl's script library, Tk's, tcllib and the bundled extensions. Import
    # libraries, tclConfig.sh and the nmake rules are build-time only.
    Get-ChildItem (Join-Path $TclDir 'lib') | Where-Object { $_.Name -ne 'nmake' -and $_.Extension -notin '.lib','.sh' } |
        ForEach-Object { Copy-Item $_.FullName (Join-Path $lib $_.Name) -Recurse }
    if (-not (Test-Path (Join-Path $lib 'tcl8.6\init.tcl'))) { Die "the Tcl library did not stage" }
    foreach ($m in @{ 'echo-module' = 'echo'; 'irc' = 'irc'; 'telegram' = 'telegram'; 'slopdrop' = 'slopdrop'; 'lang-js' = 'lang-js' }.GetEnumerator()) {
        $dll = Join-Path $rel ("veles_module_" + $m.Value.Replace('-','_') + ".dll")
        if (-not (Test-Path $dll)) { Die "$dll was not built" }
        $dir = Join-Path $lib "veles\modules\$($m.Key)"
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item $dll (Join-Path $dir 'module.dll')
        Set-Content -Encoding ascii (Join-Path $dir 'plugin.toml') @(
            "name = `"$($m.Key)`"", 'kind = "module"', 'entry = "module.dll"', "description = `"In-process module $($m.Key)`"")
    }
    Copy-Item (Join-Path $Repo 'veles-desktop') (Join-Path $stage 'veles-desktop') -Recurse
    Copy-Item (Join-Path $Repo 'skills') (Join-Path $stage 'skills') -Recurse
    Copy-Item (Join-Path $Repo 'packaging\veles.svg') $stage -ErrorAction SilentlyContinue
    foreach ($f in 'LICENSE', 'THIRD_PARTY_LICENSES.md') {
        if (Test-Path (Join-Path $Repo $f)) { Copy-Item (Join-Path $Repo $f) $stage }
    }
    $info = [ordered]@{ name = 'veles'; version = $version; revision = $revision; architecture = 'x86_64'; os = 'windows';
        runtime = 'portable: Tcl/Tk 8.6, tcllib, sqlite3 and the Rust std beside veles.exe' }
    [IO.File]::WriteAllText((Join-Path $stage 'BUNDLE-INFO.json'), ($info | ConvertTo-Json -Compress))
    [IO.File]::WriteAllText((Join-Path $stage 'README.txt'), @"
veles $version ($revision) for Windows x86_64 - portable.

Run bin\veles.exe from anywhere; nothing else needs installing.
  bin\veles.exe setup     pick a provider and model
  bin\veles.exe gui       the desktop app
  bin\veles.exe doctor    check the install

scripts/install/install.ps1 installs this under %LOCALAPPDATA%\Programs\veles
and puts veles on PATH. Configuration lives in %APPDATA%\veles.
"@)
    $zipPath = [IO.Path]::GetFullPath($zipPath)
    New-Item -ItemType Directory -Force (Split-Path $zipPath) | Out-Null
    if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
    # bsdtar (tar.exe, Windows 10 1803+) writes forward-slash zip entries;
    # Windows PowerShell 5.1's Compress-Archive writes backslashes that
    # other unzip tools turn into file names.
    Push-Location $work
    & tar.exe -a -c -f $zipPath $name
    $rc = $LASTEXITCODE
    Pop-Location
    if ($rc -ne 0 -or -not (Test-Path $zipPath)) { Die "could not write $zipPath" }
    Note "wrote $zipPath ($([math]::Round((Get-Item $zipPath).Length / 1MB, 1)) MB)"
    Remove-Item -Recurse -Force $work
    return $zipPath
}

# The zip proves itself the way an installer does: unpacked into a fresh
# directory, with a fresh APPDATA/LOCALAPPDATA and a PATH holding no Tcl,
# sqlite or Rust toolchain, `veles --version` and `veles doctor` run.
function Test-PortableZip([string]$zipPath) {
    Say "proving $zipPath from a fresh directory"
    $probe = Join-Path $env:TEMP ("veles-zip-probe-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Force $probe, "$probe\appdata", "$probe\localappdata", "$probe\unpacked" | Out-Null
    & tar.exe -x -f $zipPath -C "$probe\unpacked"
    if ($LASTEXITCODE -ne 0) { Die "could not unpack $zipPath" }
    $exe = Get-ChildItem "$probe\unpacked\*\bin\veles.exe" | Select-Object -First 1
    if (-not $exe) { Die "the zip has no <root>\bin\veles.exe" }
    $saved = @{ Path = $env:Path; APPDATA = $env:APPDATA; LOCALAPPDATA = $env:LOCALAPPDATA;
        TCL_LIBRARY = $env:TCL_LIBRARY; TK_LIBRARY = $env:TK_LIBRARY; VELES_TCLSH = $env:VELES_TCLSH; WISH = $env:WISH }
    try {
        $env:Path = "$env:SystemRoot\system32;$env:SystemRoot;$env:SystemRoot\System32\WindowsPowerShell\v1.0"
        $env:APPDATA = "$probe\appdata"; $env:LOCALAPPDATA = "$probe\localappdata"
        foreach ($k in 'TCL_LIBRARY','TK_LIBRARY','VELES_TCLSH','WISH','VELES_HOME') { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
        $out = & $exe.FullName --version 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { Die "veles --version failed from the unpacked zip: $out" }
        Note "veles --version: $($out.Trim())"
        $doctor = & $exe.FullName doctor 2>&1 | Out-String
        Write-Host $doctor
        if ($doctor -notmatch '(?i)tcl') { Die "veles doctor did not report on Tcl from the unpacked zip" }
    } finally {
        foreach ($k in $saved.Keys) { Set-Item "Env:$k" $saved[$k] -ErrorAction SilentlyContinue }
        if (-not $saved.TCL_LIBRARY) { Remove-Item Env:TCL_LIBRARY -ErrorAction SilentlyContinue }
        if (-not $saved.TK_LIBRARY) { Remove-Item Env:TK_LIBRARY -ErrorAction SilentlyContinue }
        if (-not $saved.VELES_TCLSH) { Remove-Item Env:VELES_TCLSH -ErrorAction SilentlyContinue }
        if (-not $saved.WISH) { Remove-Item Env:WISH -ErrorAction SilentlyContinue }
        Remove-Item -Recurse -Force $probe -ErrorAction SilentlyContinue
    }
    Note "the unpacked zip runs with no Tcl, sqlite or toolchain on PATH"
}

if ($PortableZip) {
    $zip = New-PortableZip $PortableZip
    Test-PortableZip $zip
    Say "done"
    Note "portable zip: $zip"
    exit 0
}

# ── install ──────────────────────────────────────────────────────────
Say "installing to $Prefix\bin"
$Bin = Join-Path $Prefix 'bin'
New-Item -ItemType Directory -Force $Bin | Out-Null
$rel = Join-Path $Repo 'target\winrelease'
# A re-install: Windows locks a running exe, so a gateway from the
# previous install of this prefix must stop before veles.exe is replaced
# (Linux overwrites a running binary's path; Windows says "being used by
# another process" on the Windows build host, 2026-10-13). `gateway stop` is the
# graceful one (over the pipe, then the task); `gateway install` at the
# end starts the new one.
$oldExe = Join-Path $Bin 'veles.exe'
if (Test-Path $oldExe) {
    # Every veles.exe from THIS prefix locks the file — the gateway, but
    # also a `veles gui` window or a `veles chat` someone left open. Only
    # the gateway is ours to stop; the rest is named and the install
    # stops BEFORE touching the gateway, so a refused install leaves the
    # node as it was. (2026-10-14: a `veles gui` counted as "a gateway",
    # the real gateway was stopped, the wait for the window's pid timed
    # out, and the installer died with the node down and nothing copied.)
    $mine = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -eq $oldExe })
    $gateways = @($mine | Where-Object { $_.CommandLine -match '(^|\s)gateway(\s|$)' })
    $others = @($mine | Where-Object { $_.CommandLine -notmatch '(^|\s)gateway(\s|$)' })
    if ($others.Count -gt 0) {
        $what = ($others | ForEach-Object { "pid $($_.ProcessId): $($_.CommandLine)" }) -join '; '
        Die "veles is in use here and would keep veles.exe locked - $what - close it, then run this again (the gateway was left running)"
    }
    if ($gateways.Count -gt 0) {
        $ids = $gateways | ForEach-Object { $_.ProcessId }
        Note "a gateway from the previous install is running (pid $($ids -join ' ')): stopping it first"
        & $oldExe gateway stop 2>&1 | Where-Object { $_ -notmatch 'rust plugin' } | ForEach-Object { Note "  $_" }
        for ($i = 0; $i -lt 20; $i++) {
            if (-not (Get-Process -Id $ids -ErrorAction SilentlyContinue)) { break }
            Start-Sleep 1
        }
        if (Get-Process -Id $ids -ErrorAction SilentlyContinue) { Die "the old gateway (pid $($ids -join ' ')) did not stop; stop it, then run this again" }
    }
}
foreach ($f in 'veles.exe','veles-gw.exe','veles_runtime.dll') {
    if (-not (Test-Path (Join-Path $rel $f))) { Die "$f was not built" }
    Copy-Item (Join-Path $rel $f) (Join-Path $Bin $f) -Force
    Note "installed $Bin\$f"
}
# Rust's std as a DLL: the runtime dylib links it dynamically, and Windows
# has no rpath — it must sit beside the exe.
$sysroot = (& rustc --print sysroot).Trim()
$std = Get-ChildItem (Join-Path $sysroot 'lib\rustlib\x86_64-pc-windows-msvc\lib') -Filter 'std-*.dll' | Select-Object -First 1
if (-not $std) { Die "no std-*.dll in the toolchain's rustlib dir" }
Copy-Item $std.FullName (Join-Path $Bin $std.Name) -Force
Note "installed $Bin\$($std.Name)"
# The C libraries the exe and the runtime import: Tcl/Tk (+ its zlib)
# and sqlite3. Beside the exe is the one place Windows always looks.
foreach ($d in @(Get-ChildItem (Join-Path $TclDir 'bin') -Filter '*.dll') + @(Get-Item (Join-Path $SqliteDir 'sqlite3.dll'))) {
    Copy-Item $d.FullName (Join-Path $Bin $d.Name) -Force
    Note "installed $Bin\$($d.Name)"
}
# A tree is replaced whole. `Copy-Item -Recurse` onto a directory that
# EXISTS nests the source inside it (…\veles-desktop\veles-desktop) and
# leaves every old file in place: the Windows build host ran the 2026-10-13 desktop
# and skills through every re-install until 2026-10-14, with each run's
# fresh copy one level down. Removing first also drops files the tree
# no longer has, as `git archive` would.
function Install-Tree($src, $dst) {
    if (Test-Path $dst) { Remove-Item -Recurse -Force $dst }
    Copy-Item $src $dst -Recurse -Force
}
# The desktop app, beside bin\ where `find_desktop_dir` looks (one up
# from the binary).
Install-Tree (Join-Path $Repo 'veles-desktop') (Join-Path $Prefix 'veles-desktop')
Note "installed $Prefix\veles-desktop"
# The bundled skills (skills/windows-node and whatever joins it), found
# the same way.
Install-Tree (Join-Path $Repo 'skills') (Join-Path $Prefix 'skills')
Note "installed $Prefix\skills"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $Bin) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$Bin", 'User')
    Note "added $Bin to the user PATH (new shells see it)"
}
$env:Path = "$Bin;$env:Path"
$Veles = Join-Path $Bin 'veles.exe'

# What the engine needs at run time, recorded where the gateway reads it
# (the config dir's .env), as ensure_tclsh does on unix.
$cfg = Join-Path $env:APPDATA 'veles'
New-Item -ItemType Directory -Force $cfg | Out-Null
$envf = Join-Path $cfg '.env'
$lines = @()
if (Test-Path $envf) { $lines = Get-Content $envf }
foreach ($kv in @("VELES_TCLSH=$tclsh", "TCL_LIBRARY=$($env:TCL_LIBRARY)", "TK_LIBRARY=$($env:TK_LIBRARY)", "WISH=$(Join-Path $TclDir 'bin\wish.exe')")) {
    $k = $kv.Split('=')[0]
    if (-not ($lines | Where-Object { $_ -like "$k=*" })) { Add-Content $envf $kv; Note "recorded $k in $envf" }
}

Say "modules"
# <the name the module declares> = <its crate's short name>. The host
# refuses a manifest whose name is not the module's own.
foreach ($m in @{ 'echo-module' = 'echo'; 'irc' = 'irc'; 'telegram' = 'telegram'; 'slopdrop' = 'slopdrop'; 'lang-js' = 'lang-js' }.GetEnumerator()) {
    $dll = Join-Path $rel ("veles_module_" + $m.Value.Replace('-','_') + ".dll")
    if (-not (Test-Path $dll)) { Die "$dll was not built" }
    & $Veles plugins install-module $m.Key $dll
    if ($LASTEXITCODE -ne 0) { Die "installing module $($m.Key) failed" }
}

# ── the service ──────────────────────────────────────────────────────
Say "the gateway as a logon task"
& $Veles gateway install
if ($LASTEXITCODE -ne 0) { Die "veles gateway install failed" }

# ── the verdict ──────────────────────────────────────────────────────
Say "verifying the install by running it"
& $Veles version
if ($LASTEXITCODE -ne 0) { Die "the installed binary does not run" }
& $Veles doctor
if ($LASTEXITCODE -eq 0) { Note "doctor: OK" } else { Note "doctor reported problems (expected on a host with no provider configured yet)" }
Say "sandbox fence"
# The verdict is the fence's own line: `doctor --sandbox` also probes
# the podman user boxes, which no Windows host has (its report says so
# and the exit code is 1), and that is not the failure this guards.
$fence = & $Veles doctor --sandbox 2>&1 | Out-String
Write-Host $fence
if ($fence -notmatch 'result: OK \(42\)') { Die "the sandbox fence does not work on this host — see the report above. This is a real failure: it means untrusted code has nowhere safe to run." }
Note "the fence runs (user boxes: none on Windows — PLAN-windows.md §10)."

Say "done"
Note "binary:  $Veles"
Note "config:  $cfg\config.toml"
Note "state:   $env:LOCALAPPDATA\veles"
Note ""
Note "next:    veles setup     # pick a provider and model"
Note "         veles chat      # or: veles gui"
