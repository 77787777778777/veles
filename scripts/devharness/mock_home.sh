#!/usr/bin/env bash
# A private VELES_HOME wired to the mock provider.
set -euo pipefail
H=/dev/shm/veles-mockhome
rm -rf "$H"; mkdir -p "$H"
cat > "$H/config.toml" <<'TOML'
[model]
provider = "mock"
default  = "mock-model"

[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:18999/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]
TOML
echo "$H"
