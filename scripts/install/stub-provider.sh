#!/usr/bin/env bash
# A canned OpenAI-compatible chat provider on loopback, for install smokes:
# every completion answers STUB_PROVIDER_REPLY (streamed or not).
# No Python: socat accepts, this script answers one request per process.
#
#   stub-provider.sh serve PORT [LOG]      # foreground
# Config: [[custom_providers]] base_url = "http://127.0.0.1:PORT/v1",
#         api_mode = "chat_completions", api_key = "none", models = ["stub-model"]
set -euo pipefail
mode=${1:?usage: stub-provider.sh serve PORT [LOG]}
if [[ $mode == serve ]]; then
    exec socat "TCP-LISTEN:${2:?port},bind=127.0.0.1,reuseaddr,fork" EXEC:"bash $0 handle ${3:-/dev/null}"
fi
log=${2:-/dev/null}
# Byte semantics: `read -N` counts characters, and a UTF-8 prompt would
# leave it waiting for more characters than the body has bytes.
export LC_ALL=C
IFS=' ' read -r method path _ || exit 0
path=${path%$'\r'}
length=0
while IFS= read -r line; do
    line=${line%$'\r'}; [[ -z $line ]] && break
    case ${line,,} in content-length:*) length=${line#*:}; length=${length// /} ;; esac
done
body=""
((length > 0)) && IFS= read -r -N "$length" body || true
printf '%s %s %s\n' "$method" "$path" "${#body}" >>"$log"
send() { printf 'HTTP/1.1 200 OK\r\nContent-Type: %s\r\nContent-Length: %s\r\nConnection: close\r\n\r\n%s' "$1" "${#2}" "$2"; }
text=STUB_PROVIDER_REPLY
case $path in
    */models) send application/json '{"object":"list","data":[{"id":"stub-model","object":"model"}]}' ;;
    */chat/completions)
        if [[ $body == *'"stream":true'* || $body == *'"stream": true'* ]]; then
            chunk() { printf 'data: {"id":"stub","object":"chat.completion.chunk","created":0,"model":"stub-model","choices":[{"index":0,"delta":%s,"finish_reason":%s}]%s}\n\n' "$1" "$2" "${3:-}"; }
            sse=$(chunk "{\"role\":\"assistant\",\"content\":\"$text\"}" null; chunk '{}' '"stop"' ',"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}'; printf 'data: [DONE]\n\n')
            send text/event-stream "$sse"$'\n\n'
        else
            send application/json "{\"id\":\"stub\",\"object\":\"chat.completion\",\"created\":0,\"model\":\"stub-model\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"$text\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}"
        fi ;;
    *) printf 'HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n' ;;
esac
