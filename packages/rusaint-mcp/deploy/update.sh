#!/bin/bash
# GitHub Release 중 가장 최근 mcp-v* 태그의 바이너리로 교체합니다.
# 토큰: /etc/rusaint-mcp/github-token (이 레포 Contents 읽기 전용 fine-grained token)
set -euo pipefail
REPO=AndyH0ng/rusaint-private
ASSET=rusaint-mcp-x86_64-linux
STATE=/var/lib/rusaint-mcp-update/current-tag
TOKEN=$(cat /etc/rusaint-mcp/github-token)

api() {
  curl -fsSL -H "Authorization: Bearer $TOKEN" -H "X-GitHub-Api-Version: 2022-11-28" "$@"
}

release=$(api -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$REPO/releases?per_page=50" |
  jq -c '[.[] | select(.draft | not) | select(.tag_name | startswith("mcp-v"))] | sort_by(.created_at) | last // empty')
[ -n "$release" ] || { echo "mcp-v* 릴리스 없음"; exit 0; }

tag=$(jq -r .tag_name <<<"$release")
if [ "$tag" = "$(cat "$STATE" 2>/dev/null || true)" ]; then
  exit 0
fi
bin_id=$(jq -r --arg n "$ASSET" '.assets[] | select(.name == $n) | .id' <<<"$release")
sum_id=$(jq -r --arg n "$ASSET.sha256" '.assets[] | select(.name == $n) | .id' <<<"$release")
[ -n "$bin_id" ] && [ -n "$sum_id" ] || { echo "$tag: 바이너리가 아직 없음"; exit 0; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
api -H "Accept: application/octet-stream" -o "$tmp/$ASSET" \
  "https://api.github.com/repos/$REPO/releases/assets/$bin_id"
api -H "Accept: application/octet-stream" -o "$tmp/$ASSET.sha256" \
  "https://api.github.com/repos/$REPO/releases/assets/$sum_id"
(cd "$tmp" && sha256sum -c "$ASSET.sha256")

install -m 755 "$tmp/$ASSET" /usr/local/bin/rusaint-mcp.new
mv -f /usr/local/bin/rusaint-mcp.new /usr/local/bin/rusaint-mcp
systemctl restart rusaint-mcp.service
echo "$tag" > "$STATE"
echo "$tag 로 업데이트했습니다 (커넥터를 다시 연결하세요)"
