#!/bin/sh
# CLOUDFLARE_TUNNEL_TOKEN 이 있으면 고정 주소(named tunnel), 없으면 임시 주소(quick tunnel).
# 고정 주소를 쓰면 터널이 재시작돼도 커넥터를 다시 연결하지 않아도 됩니다.
# 이때 /etc/rusaint-mcp/env 의 RUSAINT_MCP_PUBLIC_URL 도 그 도메인으로 맞춰야 합니다.
set -eu
if [ -n "${CLOUDFLARE_TUNNEL_TOKEN:-}" ]; then
  exec /usr/bin/cloudflared tunnel --no-autoupdate \
    --metrics "${RUSAINT_TUNNEL_METRICS}" run --token "${CLOUDFLARE_TUNNEL_TOKEN}"
fi
exec /usr/bin/cloudflared tunnel --no-autoupdate \
  --metrics "${RUSAINT_TUNNEL_METRICS}" --url "http://${RUSAINT_MCP_BIND}"
