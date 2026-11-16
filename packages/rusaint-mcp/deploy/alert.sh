#!/bin/sh
# rusaint-mcp.service 의 OnFailure 로 불립니다. 학번·토큰 같은 값은 보내지 않습니다.
set -u
host=$(hostname)
when=$(date -Is)
state=$(systemctl is-failed rusaint-mcp.service 2>/dev/null || true)
message="rusaint-mcp 중단: $host $when ($state)"
echo "$message" >&2
if [ -n "${RUSAINT_ALERT_WEBHOOK:-}" ]; then
  curl -fsS -m 10 -X POST -H "Content-Type: application/json" \
    --data "$(printf '{"text":"%s"}' "$message")" \
    "$RUSAINT_ALERT_WEBHOOK" >/dev/null 2>&1 || echo "알림 전송 실패" >&2
fi
