#!/bin/sh
# RUSAINT_MCP_PUBLIC_URL 이 없으면 cloudflared quick tunnel 의 주소를 씁니다.
set -eu
if [ -z "${RUSAINT_MCP_PUBLIC_URL:-}" ]; then
  host=""
  for _ in $(seq 1 60); do
    host=$(curl -fsS http://127.0.0.1:20241/quicktunnel 2>/dev/null | jq -r '.hostname // empty' || true)
    [ -n "$host" ] && break
    sleep 1
  done
  [ -n "$host" ] || { echo "quick tunnel 주소를 받지 못했습니다" >&2; exit 1; }
  RUSAINT_MCP_PUBLIC_URL="https://$host"
  export RUSAINT_MCP_PUBLIC_URL
fi
echo "public url: $RUSAINT_MCP_PUBLIC_URL/mcp" >&2
exec /usr/local/bin/rusaint-mcp serve
