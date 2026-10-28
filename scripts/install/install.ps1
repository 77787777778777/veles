<#
.SYNOPSIS
  Install the prebuilt veles release on Windows 10/11 (x64).

.DESCRIPTION
  Downloads the portable Windows zip of a GitHub release, checks it
  against the release's SHA256SUMS (always), verifies SHA256SUMS' keyless
  Sigstore signature when cosign is on PATH (otherwise prints the exact
  command and continues, unless -RequireSignature), and installs it under
  %LOCALAPPDATA%\Programs\veles:

    releases\<version>-<revision>\   one directory per installed release
    current\                         a junction to the active release
    current\bin                      added to the user PATH

  Re-running upgrades: the new release is unpacked beside the old one and
  `current` is switched; the previous release is kept. %APPDATA%\veles
  (config, keys) is never touched. Nothing needs admin rights.

  From the web (parameters through the environment):
    irm https://raw.githubusercontent.com/77787777778777/veles/master/scripts/install/install.ps1 | iex
    $env:VELES_VERSION = 'v0.1.0'; irm .../install.ps1 | iex

  For a source build use scripts\install\windows.ps1 instead.

.PARAMETER Version
  The release to install, e.g. v0.1.0. Default: $env:VELES_VERSION, else the latest release.
.PARAMETER RequireSignature
  Refuse to install unless cosign verifies SHA256SUMS. Also $env:VELES_REQUIRE_SIGNATURE=1.
.PARAMETER InstallRoot
  Default: %LOCALAPPDATA%\Programs\veles (or $env:VELES_INSTALL_ROOT).
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = $env:VELES_VERSION,
    [switch]$RequireSignature,
    [string]$InstallRoot = $(if ($env:VELES_INSTALL_ROOT) { $env:VELES_INSTALL_ROOT } else { Join-Path $env:LOCALAPPDATA 'Programs\veles' })
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# The one place the repository slug lives on Windows (install.sh has its twin).
$DefaultRepository = '77787777778777/veles'

function Say($m)  { Write-Host ''; Write-Host "> $m" -ForegroundColor White }
function Note($m) { Write-Host "  $m" }
function Die($m)  { Write-Host "x $m" -ForegroundColor Red; throw "veles install: $m" }

if ($env:VELES_REQUIRE_SIGNATURE -eq '1') { $RequireSignature = $true }
$repo = if ($env:VELES_REPOSITORY) { $env:VELES_REPOSITORY } else { $DefaultRepository }
if ($repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or $repo -match '\.\.') { Die "invalid VELES_REPOSITORY '$repo'; expected OWNER/NAME" }
$base = if ($env:VELES_RELEASE_BASE_URL) { $env:VELES_RELEASE_BASE_URL.TrimEnd('/') } else { "https://github.com/$repo/releases" }
if ($base -notmatch '^https://' -and $base -notmatch '^http://(127\.0\.0\.1|localhost)[:/]') { Die 'VELES_RELEASE_BASE_URL must be https:// (or loopback http:// for tests)' }
if (-not [Environment]::Is64BitOperatingSystem) { Die 'the prebuilt release is for 64-bit Windows' }
$curl = Get-Command curl.exe -ErrorAction SilentlyContinue
if (-not $curl) { Die 'curl.exe is missing (it ships with Windows 10 1803 and later)' }
$proto = if ($base -match '^https://') { '=https' } else { '=http,https' }

function Fetch([string]$url, [string]$out) {
    & $curl.Source --fail --silent --show-error --location --proto $proto --proto-redir $proto --connect-timeout 30 --retry 2 --output $out $url
    return ($LASTEXITCODE -eq 0)
}

# The release.
if ($Version) {
    $tag = 'v' + $Version.TrimStart('v')
} else {
    $latest = & $curl.Source --fail --silent --show-error --location --proto $proto --proto-redir $proto --connect-timeout 30 --output NUL --write-out '%{url_effective}' "$base/latest"
    if ($LASTEXITCODE -ne 0 -or $latest -notmatch '/tag/(v[^/]+)$') { Die "could not find the latest release at $base/latest (pass -Version vX.Y.Z)" }
    $tag = $Matches[1]
}
if ($tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$') { Die "'$tag' is not a release version such as v0.1.0" }
$rel = "$base/download/$tag"
Say "veles $tag for windows-x86_64 from $repo"

$stage = Join-Path ([IO.Path]::GetTempPath()) ('veles-install-' + [guid]::NewGuid().ToString('N').Substring(0, 12))
New-Item -ItemType Directory -Force $stage | Out-Null
try {
    # 1. SHA256SUMS and, when it can be checked, its signature.
    if (-not (Fetch "$rel/SHA256SUMS" "$stage\SHA256SUMS")) { Die "could not download $rel/SHA256SUMS - is $tag a published release of $repo?" }
    $identity = "https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag"
    $issuer = 'https://token.actions.githubusercontent.com'
    $cosign = Get-Command cosign -ErrorAction SilentlyContinue
    if ($cosign) {
        if (-not (Fetch "$rel/SHA256SUMS.sigstore.json" "$stage\SHA256SUMS.sigstore.json")) { Die 'the release has no SHA256SUMS.sigstore.json, so cosign cannot verify it; refusing' }
        $out = & $cosign.Source verify-blob --bundle "$stage\SHA256SUMS.sigstore.json" --certificate-identity $identity --certificate-oidc-issuer $issuer "$stage\SHA256SUMS" 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { Write-Host $out; Die "cosign rejected SHA256SUMS for $repo $tag; nothing was installed" }
        Note "signature: SHA256SUMS is signed by $repo's release workflow at $tag (cosign)"
    } elseif ($RequireSignature) {
        Die '-RequireSignature: cosign is not on PATH (winget install sigstore.cosign); nothing was installed'
    } else {
        Note 'signature: not checked (cosign is not installed). To check it yourself:'
        Note "  cosign verify-blob --bundle SHA256SUMS.sigstore.json --certificate-identity $identity --certificate-oidc-issuer $issuer SHA256SUMS"
    }

    # 2. The one Windows zip SHA256SUMS names for this version.
    $pkgver = ($tag.TrimStart('v') -split '-')[0]
    $re = '^veles-' + [regex]::Escape($pkgver) + '-[0-9a-z]+-windows-x86_64\.zip$'
    $hits = @(Get-Content "$stage\SHA256SUMS" | ForEach-Object {
        $f = $_ -split '\s+', 2
        if ($f.Count -eq 2 -and $f[0] -match '^[0-9a-f]{64}$') {
            $n = $f[1].TrimStart('*').Trim()
            if ($n -match $re) { [pscustomobject]@{ Sum = $f[0]; Name = $n } }
        }
    })
    if ($hits.Count -ne 1) { Die "SHA256SUMS of $tag lists $($hits.Count) windows-x86_64 zips for $pkgver (expected exactly one)" }
    $name = $hits[0].Name

    # 3. Download and check.
    Note "downloading $name"
    $zip = Join-Path $stage $name
    if (-not (Fetch "$rel/$name" $zip)) { Die "could not download $rel/$name" }
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
    if ($actual -ne $hits[0].Sum) { Die "checksum mismatch for ${name}: SHA256SUMS says $($hits[0].Sum), the download is $actual; refusing to install it" }
    Note "checksum: $name matches SHA256SUMS"

    # 4. Unpack beside the installed releases and switch `current`.
    $releases = Join-Path $InstallRoot 'releases'
    New-Item -ItemType Directory -Force $releases | Out-Null
    $unpack = Join-Path $stage 'unpacked'
    New-Item -ItemType Directory -Force $unpack | Out-Null
    & tar.exe -x -f $zip -C $unpack
    if ($LASTEXITCODE -ne 0) { Die "could not unpack $name" }
    $roots = @(Get-ChildItem $unpack -Directory)
    if ($roots.Count -ne 1 -or -not (Test-Path (Join-Path $roots[0].FullName 'bin\veles.exe'))) { Die "$name does not hold one <root>\bin\veles.exe" }
    $info = Get-Content (Join-Path $roots[0].FullName 'BUNDLE-INFO.json') -Raw | ConvertFrom-Json
    if ($info.version -ne $pkgver) { Die "$name does not contain veles $pkgver" }
    $id = "$($info.version)-$($info.revision)"
    $dest = Join-Path $releases $id
    if (Test-Path $dest) {
        Note "release $id is already unpacked; replacing it"
        Remove-Item -Recurse -Force $dest -ErrorAction SilentlyContinue
        if (Test-Path $dest) { Die "$dest is in use (close veles: veles gateway stop, and any veles window), then run this again" }
    }
    Move-Item $roots[0].FullName $dest

    $current = Join-Path $InstallRoot 'current'
    $previous = $null
    if (Test-Path $current) {
        $item = Get-Item $current -Force
        if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Die "$current exists and is not an installer junction; remove it and run this again" }
        $previous = @($item.Target)[0]
        # Removing a junction never touches its target, even a running one.
        [IO.Directory]::Delete($current, $false)
    }
    New-Item -ItemType Junction -Path $current -Target $dest | Out-Null
    if ($previous -and ($previous -ne $dest)) { Note "upgraded: $(Split-Path -Leaf $previous) -> $id (the previous release is kept)" } else { Note "installed: $id" }

    $bin = Join-Path $current 'bin'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    if (($userPath -split ';') -notcontains $bin) {
        [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';') + ";$bin").TrimStart(';')), 'User')
        Note "added $bin to your user PATH (new terminals see it)"
    }
    $env:Path = "$bin;$env:Path"
    $source = Join-Path $InstallRoot 'bin\veles.exe'
    if (Test-Path $source) { Note "note: a source-built veles is also at $source; whichever PATH entry comes first wins" }

    # 5. Prove it runs.
    Say 'checking the installation'
    $veles = Join-Path $bin 'veles.exe'
    & $veles --version
    if ($LASTEXITCODE -ne 0) { Die 'the installed veles does not run' }
    $doctor = & $veles doctor 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) { Note 'veles doctor: OK' } else {
        Write-Host ($doctor -replace '(?m)^', '    ')
        Note "veles doctor reported problems above; on a new machine that is usually just"
        Note "the missing provider, which 'veles setup' configures."
    }
    if ($previous) { Note 'a gateway started before this upgrade keeps running the old release until: veles gateway stop' }

    Say 'done'
    Note 'next:  veles setup     # pick a provider and model (terminal)'
    Note '       veles gui       # or the desktop app, which walks you through it'
} finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
