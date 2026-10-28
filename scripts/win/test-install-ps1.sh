#!/usr/bin/env bash
# scripts/win/test-install-ps1.sh — scripts/install/install.ps1 on the real
# Windows host against a fixture release of a real portable zip.
#
#   nix shell nixpkgs#socat nixpkgs#libarchive -c scripts/win/test-install-ps1.sh ZIP <host>
#
# The zip is published as release vX (SHA256SUMS) and, repacked with only
# BUNDLE-INFO's version bumped, as vX+0.0.1, by the loopback fixture server
# (scripts/install/fixture-release.sh), which the host reaches through an
# ssh reverse tunnel on ITS loopback. On the host, with a throwaway
# install root and APPDATA: install vX -> veles --version; write a config;
# install the latest -> `current` switched, previous kept, config intact;
# a tampered zip -> refused; -RequireSignature without cosign -> refused.
set -euo pipefail
zip=$(realpath "${1:?usage: test-install-ps1.sh ZIP <host>}")
host=${2:-${VELES_WIN_HOST:?usage: test-install-ps1.sh ZIP <ssh host> (or set VELES_WIN_HOST)}}
here=$(cd "$(dirname "$0")/../.." && pwd)
for tool in socat bsdtar sha256sum jq ssh scp; do command -v "$tool" >/dev/null || { echo "$tool required" >&2; exit 2; }; done
name=$(basename "$zip")
[[ $name =~ ^veles-([0-9]+)\.([0-9]+)\.([0-9]+)-([0-9a-z-]+)-windows-x86_64\.zip$ ]] || { echo "unexpected zip name $name" >&2; exit 2; }
v1=${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}
v2=${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))
rev=${BASH_REMATCH[4]}
t=$(mktemp -d "${TMPDIR:-/tmp}/veles-ps1-test.XXXXXX")
server=""
cleanup() { [[ -z $server ]] || kill "$server" 2>/dev/null || true; rm -rf "$t"; }
trap cleanup EXIT
mkdir -p "$t/srv/download/v$v1" "$t/srv/download/v$v2" "$t/x"
# A dev zip may carry a -dirty revision; release names never do.
name=veles-$v1-${rev%-dirty}-windows-x86_64.zip
cp "$zip" "$t/srv/download/v$v1/$name"
(cd "$t/srv/download/v$v1" && sha256sum "$name" > SHA256SUMS)
bsdtar -xf "$zip" -C "$t/x"
root=$(ls "$t/x")
jq -c --arg v "$v2" '.version = $v' "$t/x/$root/BUNDLE-INFO.json" > "$t/info" && cat "$t/info" > "$t/x/$root/BUNDLE-INFO.json"
name2=veles-$v2-${rev%-dirty}-windows-x86_64.zip
mv "$t/x/$root" "$t/x/veles-$v2-$rev-windows-x86_64"
(cd "$t/x" && bsdtar -a -cf "$t/srv/download/v$v2/$name2" "veles-$v2-$rev-windows-x86_64")
(cd "$t/srv/download/v$v2" && sha256sum "$name2" > SHA256SUMS)
mkdir -p "$t/srv/download/v9.0.0"
printf 'not a zip' > "$t/srv/download/v9.0.0/veles-9.0.0-${rev%-dirty}-windows-x86_64.zip"
echo "0000000000000000000000000000000000000000000000000000000000000000  veles-9.0.0-${rev%-dirty}-windows-x86_64.zip" > "$t/srv/download/v9.0.0/SHA256SUMS"
echo "v$v2" > "$t/srv/LATEST"
port=$((20000 + RANDOM % 20000))
bash "$here/scripts/install/fixture-release.sh" serve "$t/srv" "$port" 2>"$t/server.log" &
server=$!
cat > "$t/probe.ps1" <<PS
\$ErrorActionPreference = 'Stop'
\$p = 'C:\\veles\\ps1probe'
if (Test-Path \$p) { cmd /c rmdir /s /q \$p }
New-Item -ItemType Directory -Force "\$p\\appdata", "\$p\\localappdata" | Out-Null
\$env:APPDATA = "\$p\\appdata"; \$env:LOCALAPPDATA = "\$p\\localappdata"
\$env:VELES_INSTALL_ROOT = "\$p\\root"; \$env:VELES_RELEASE_BASE_URL = 'http://127.0.0.1:$port/releases'
\$savedPath = [Environment]::GetEnvironmentVariable('Path', 'User')
function Ok(\$m) { Write-Host "  ok  \$m" }
try {
  & C:\\veles\\install.ps1 -Version v$v1 *> "\$p\\one.log"
  \$v = & "\$p\\root\\current\\bin\\veles.exe" --version
  if (\$v -notmatch '$v1') { throw "v$v1 not installed: \$v" }
  Ok "install v$v1 from the fixture release: \$v"
  New-Item -ItemType Directory -Force "\$p\\appdata\\veles" | Out-Null
  Set-Content "\$p\\appdata\\veles\\config.toml" '[model]'
  \$before = (Get-FileHash "\$p\\appdata\\veles\\config.toml").Hash
  & C:\\veles\\install.ps1 *> "\$p\\two.log"
  \$target = @((Get-Item "\$p\\root\\current").Target)[0]
  if (\$target -notlike '*$v2-$rev') { throw "current is \$target" }
  if (-not (Test-Path "\$p\\root\\releases\\$v1-$rev\\bin\\veles.exe")) { throw 'previous release not kept' }
  if ((Get-FileHash "\$p\\appdata\\veles\\config.toml").Hash -ne \$before) { throw 'config changed' }
  & "\$p\\root\\current\\bin\\veles.exe" --version | Out-Null
  if (\$LASTEXITCODE -ne 0) { throw 'upgraded veles does not run' }
  if (-not (Select-String -Quiet -Path "\$p\\two.log" -Pattern 'upgraded: $v1-$rev -> $v2-$rev')) { throw 'no upgrade line' }
  Ok "upgrade to the latest ($v2): junction switched, previous kept, config intact"
  \$failed = \$false
  try { & C:\\veles\\install.ps1 -Version v9.0.0 *> "\$p\\bad.log" } catch { \$failed = \$true }
  if (-not \$failed -or -not (Select-String -Quiet -Path "\$p\\bad.log" -Pattern 'checksum mismatch')) { throw 'tampered zip accepted' }
  if (@((Get-Item "\$p\\root\\current").Target)[0] -notlike '*$v2-$rev') { throw 'current moved after a mismatch' }
  Ok 'checksum mismatch refused, current untouched'
  \$failed = \$false
  try { & C:\\veles\\install.ps1 -RequireSignature *> "\$p\\sig.log" } catch { \$failed = \$true }
  if (-not \$failed -or -not (Select-String -Quiet -Path "\$p\\sig.log" -Pattern 'RequireSignature: cosign is not on PATH')) { throw '-RequireSignature without cosign accepted' }
  Ok '-RequireSignature without cosign refused'
  Write-Host 'INSTALL_PS1_PASS'
} catch {
  Write-Host "FAIL: \$_"
  Get-ChildItem "\$p\\*.log" | ForEach-Object { Write-Host "--- \$(\$_.Name)"; Get-Content \$_.FullName | Select-Object -Last 20 }
} finally {
  [Environment]::SetEnvironmentVariable('Path', \$savedPath, 'User')
  cmd /c rmdir /s /q \$p
}
PS
scp -q -o BatchMode=yes "$here/scripts/install/install.ps1" "$host:C:/veles/install.ps1"
scp -q -o BatchMode=yes "$t/probe.ps1" "$host:C:/veles/ps1probe.ps1"
ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -R "127.0.0.1:$port:127.0.0.1:$port" "$host" \
    "pwsh -NoProfile -ExecutionPolicy Bypass -File C:\\veles\\ps1probe.ps1" 2>/dev/null | tr -d '\r' | tee "$t/out"
grep -q INSTALL_PS1_PASS "$t/out"
