#!/usr/bin/env bash
# ============================================================
# lanzou_webdav 一键发布脚本
# 用法：
#   export GH_TOKEN=ghp_xxxxxxxxxxxx
#   ./scripts/release.sh
# ============================================================
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="${VERSION:-v0.1.0}"
IMAGE="ghcr.io/tinggalleaf/lanzou-webdav"

# 颜色输出
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}===== 1. 推送 git =====${NC}"
git push -u origin main

echo ""
echo -e "${GREEN}===== 2. 给 release 打 tag =====${NC}"
git tag -a "${VERSION}" -m "release ${VERSION}" 2>/dev/null || echo "(tag ${VERSION} 已存在)"
git push origin "${VERSION}" 2>/dev/null || true

echo ""
echo -e "${GREEN}===== 3. 构建 Docker 镜像 =====${NC}"
docker build \
  -t "${IMAGE}:latest" \
  -t "${IMAGE}:${VERSION}" \
  .

echo ""
echo -e "${GREEN}===== 4. 登录 GitHub Container Registry =====${NC}"
if [ -z "${GH_TOKEN:-}" ]; then
  echo -e "${YELLOW}⚠️  GH_TOKEN 未设置，尝试使用 GITHUB_TOKEN${NC}"
  GH_TOKEN="${GITHUB_TOKEN:-}"
fi
if [ -z "${GH_TOKEN}" ]; then
  echo "❌ 请先执行：export GH_TOKEN=ghp_xxxxxxxxxxxx"
  exit 1
fi
echo "$GH_TOKEN" | docker login ghcr.io -u tinggalleaf --password-stdin

echo ""
echo -e "${GREEN}===== 5. 推送镜像（latest + ${VERSION}） =====${NC}"
docker push "${IMAGE}:latest"
docker push "${IMAGE}:${VERSION}"

echo ""
echo -e "${GREEN}✅ 完成！镜像已发布到：${NC}"
echo "   ${IMAGE}:latest"
echo "   ${IMAGE}:${VERSION}"