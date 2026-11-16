#!/bin/sh
# RUSAINT_MCP_PUBLIC_URL 이 없으면 cloudflared quick tunnel 의 주소를 씁니다.
set -eu
if [ -z "${RUSAINT_MCP_PUBLIC_URL:-}" ]; then
  host=""
  for _ in $(seq 1 60); do
    host=$(curl -fsS "http://${RUSAINT_TUNNEL_METRICS}/quicktunnel" 2>/dev/null | jq -r '.hostname // empty' || true)
    [ -n "$host" ] && break
    sleep 1
  done
  [ -n "$host" ] || { echo "quick tunnel 주소를 받지 못했습니다" >&2; exit 1; }
  RUSAINT_MCP_PUBLIC_URL="https://$host"
  export RUSAINT_MCP_PUBLIC_URL
fi
echo "public url: $RUSAINT_MCP_PUBLIC_URL/mcp" >&2

# 증명서 PDF 필터의 의존성을 미리 받아 둡니다. 첫 출력에서 받기 시작하면 제한 시간을 넘길 수 있습니다.
# 서비스 시작을 늦추지 않도록 백그라운드로 돌리고, 실패해도 서버는 그대로 뜹니다.
filter="${RUSAINT_CERTI_ASSETS:-/usr/local/lib/rusaint-mcp/certi}/certi_report.py"
if [ -f "$filter" ]; then
  (uv run --quiet "$filter" --help >/dev/null 2>&1 || true) &
fi

exec /usr/local/bin/rusaint-mcp serve
