#!/usr/bin/env bash
#
# 从矢量源生成 macOS / Windows 图标：AppIcon.icns、AppIcon.ico
#
# 依赖：Google Chrome（无头渲染 SVG）、sips、iconutil —— 全是 macOS 自带的，
# 只有 Chrome 是额外要求。之所以用 Chrome 而不是 rsvg-convert / ImageMagick：
# 本机没有装那两个，而 Chrome 的无头截图对 SVG 的滤镜、渐变支持最完整。
#
# 用法：
#   scripts/make-icon.sh            # 重新生成 AppIcon.icns
#   scripts/make-icon.sh --check    # 只渲染一张 1024 预览图到 target/icon-preview.png
#
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

SVG="${ROOT}/assets/icon/macscp-icon.svg"
ICONSET="${ROOT}/assets/icon/AppIcon.iconset"
ICNS="${ROOT}/assets/icon/AppIcon.icns"
ICO="${ROOT}/assets/icon/AppIcon.ico"
MASTER="${ROOT}/target/icon-master-1024.png"
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

[ -f "${SVG}" ] || { echo "找不到图标源：${SVG}" >&2; exit 1; }
[ -x "${CHROME}" ] || { echo "找不到 Chrome：${CHROME}" >&2; exit 1; }

mkdir -p "$(dirname "${MASTER}")"

# 1024×1024 母版。窗口按 512 CSS px 开、device scale factor 2 —— 这样输出
# 正好 1024 物理像素，同时避开无头 Chrome 在大窗口下软件渲染过慢的问题。
# --default-background-color=00000000 保留圆角外的透明区域（icns 需要 alpha）。
echo "→ 渲染 ${SVG##*/} → 1024×1024 PNG"
"${CHROME}" \
  --headless=new \
  --no-sandbox \
  --disable-dev-shm-usage \
  --disable-gpu \
  --enable-unsafe-swiftshader \
  --no-proxy-server \
  --default-background-color=00000000 \
  --force-device-scale-factor=2 \
  --window-size=512,512 \
  --virtual-time-budget=2000 \
  --screenshot="${MASTER}" \
  "file://${SVG}" 2>/dev/null

if [ ! -s "${MASTER}" ]; then
  echo "渲染失败：${MASTER} 没有生成" >&2
  exit 1
fi

if [ "${1:-}" = "--check" ]; then
  echo "预览图：${MASTER}"
  exit 0
fi

# iconutil 要求 iconset 目录里是这 10 个固定文件名（1x / 2x 各五档）。
# 缺任何一档，`iconutil -c icns` 会直接报错而不是警告。
echo "→ 生成 iconset"
rm -rf "${ICONSET}"
mkdir -p "${ICONSET}"
for spec in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x \
            128:128x128 256:128x128@2x 256:256x256 512:256x256@2x \
            512:512x512 1024:512x512@2x; do
  px="${spec%%:*}"
  name="${spec##*:}"
  sips -z "${px}" "${px}" "${MASTER}" --out "${ICONSET}/icon_${name}.png" >/dev/null
done

echo "→ 打包 icns"
iconutil -c icns "${ICONSET}" -o "${ICNS}"

echo "→ 打包 ico"
sips -z 256 256 "${MASTER}" --setProperty format ico --out "${ICO}" >/dev/null

# iconset 只是中间产物（10 个 PNG，约 1.4 MB），留着的唯一价值是排查某档尺寸糊掉。
# 需要时重跑一次本脚本即可，默认清掉，别让仓库里躺着两套图标。
rm -rf "${ICONSET}"

echo "完成：${ICNS}、${ICO}"
