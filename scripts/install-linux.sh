#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

[ -x blip ] || { echo "找不到可执行文件：$(pwd)/blip" >&2; exit 1; }
[ -f com.amov.Blip.desktop ] || { echo "找不到桌面入口模板：$(pwd)/com.amov.Blip.desktop" >&2; exit 1; }
[ -f blip-app.png ] || { echo "找不到应用图标：$(pwd)/blip-app.png" >&2; exit 1; }

if [ "$(id -u)" -eq 0 ]; then
  PREFIX="${BLIP_PREFIX:-/usr/local}"
else
  PREFIX="${BLIP_PREFIX:-${HOME}/.local}"
fi

BIN="${PREFIX}/bin/blip"
APPLICATIONS="${PREFIX}/share/applications"
ICON="${PREFIX}/share/pixmaps/blip-app.png"

install -d "$(dirname "${BIN}")" "$(dirname "${ICON}")" "${APPLICATIONS}"
install -m755 blip "${BIN}"
install -m644 blip-app.png "${ICON}"
rm -f "${PREFIX}/share/icons/hicolor/scalable/apps/blip.svg"
rm -f "${PREFIX}/share/icons/blip.png"
rm -f "${PREFIX}/share/pixmaps/blip.png"
rm -f "${APPLICATIONS}/blip.desktop"

escape_sed() {
  printf '%s' "$1" | sed 's/[&|\\]/\\&/g'
}

EXEC_VALUE=$(escape_sed "${BIN}")
sed "s|@BLIP_EXEC@|${EXEC_VALUE}|g" \
    com.amov.Blip.desktop > "${APPLICATIONS}/com.amov.Blip.desktop"
chmod 644 "${APPLICATIONS}/com.amov.Blip.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${APPLICATIONS}" >/dev/null 2>&1 || true
fi

echo "Blip 已安装：${BIN}"
echo "桌面入口：${APPLICATIONS}/com.amov.Blip.desktop"
