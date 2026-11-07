<#
.SYNOPSIS
  scripts/win/browser-bridge-e2e.ps1 - the browser tab bridge end to end in
  real headless Firefox and Edge (and Chrome, when installed) on Windows.

.DESCRIPTION
  The Windows counterpart of scripts/browser-bridge-e2e.sh (BX-9): the same
  driver, scripts/browser-bridge-e2e.mjs, with the same checks. Run it on
  the Windows host from a source tree in which veles is built (by
  scripts/win/check.sh <host> build, or scripts/win/browser-bridge-e2e.sh
  from Linux, which does both).

  The browsers find native hosts only in the real HKCU, so the run installs
  under a test name of its own (veles_browser_e2e_<random>), never the
  shipped veles_browser, and ends with `veles browser-extension uninstall`
  and checks that no key of that name and no process of the run is left.
  Everything else is throwaway: a scratch VELES_HOME and browser profiles
  under %TEMP%, a test page on a loopback port. Both browsers run headless:
  nothing opens a window, and the installed veles is not touched.

  node: $env:NODE_BIN, else node on PATH, else the copy Visual Studio ships
  (MSBuild\Microsoft\VisualStudio\NodeJs\node.exe, found with vswhere).

.PARAMETER Veles
  The veles.exe to test. Default: <tree>\target\windev\veles.exe, the
  Windows gate's build, run with the gate's DLL directories on PATH. Any
  other (bin\veles.exe of an unpacked release zip, say) runs with only
  what is beside it, and finds the extension it ships there.
.PARAMETER Browsers
  Any of firefox, edge, chrome. Default: every one installed.
.EXAMPLE
  pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\win\browser-bridge-e2e.ps1 firefox
#>
param(
    [string]$Veles,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Browsers = @()
)
$ErrorActionPreference = 'Stop'
$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$gateBuild = -not $Veles
if ($gateBuild) { $Veles = Join-Path $Repo 'target\windev\veles.exe' }
if (-not (Test-Path $Veles)) { Write-Host "browser bridge e2e: no veles at $Veles; build it (scripts/win/check.sh <host> build) or pass -Veles"; exit 2 }
$Veles = (Resolve-Path $Veles).Path

# What a gate build of veles.exe loads at start: its own directory (the
# runtime DLL), the Rust std DLL, Tcl/Tk and sqlite as scripts/win/check.bat
# puts them on PATH. A release (the portable zip, an install) has them all
# beside veles.exe, and gets nothing added: what it does not ship must fail.
if ($gateBuild) {
    $dirs = @(Split-Path $Veles)
    if (Get-Command rustc -ErrorAction SilentlyContinue) {
        $sysroot = (& rustc --print sysroot).Trim()
        $dirs += Join-Path $sysroot 'lib\rustlib\x86_64-pc-windows-msvc\lib'
        $dirs += Join-Path $sysroot 'bin'
    }
    foreach ($d in 'C:\veles\tcl\bin', 'C:\veles\sqlite') { if (Test-Path $d) { $dirs += $d } }
    $env:Path = (($dirs | Where-Object { Test-Path $_ }) + $env:Path) -join ';'
    if (Test-Path 'C:\veles\tcl\lib\tcl8.6') { $env:TCL_LIBRARY = 'C:\veles\tcl\lib\tcl8.6' }
}

$node = $env:NODE_BIN
if (-not $node) { $node = (Get-Command node -ErrorAction SilentlyContinue).Source }
if (-not $node) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        foreach ($vs in & $vswhere -all -property installationPath) {
            $candidate = Join-Path $vs 'MSBuild\Microsoft\VisualStudio\NodeJs\node.exe'
            if (Test-Path $candidate) { $node = $candidate; break }
        }
    }
}
if (-not $node) { Write-Host 'browser bridge e2e: SKIP (no node: set NODE_BIN)'; exit 0 }
Write-Host "browser bridge e2e: veles $Veles, node $node ($(& $node --version))"
& $node (Join-Path $Repo 'scripts\browser-bridge-e2e.mjs') $Veles (Join-Path $Repo 'browser-extension') @Browsers
exit $LASTEXITCODE
