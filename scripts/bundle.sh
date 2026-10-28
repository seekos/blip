#!/usr/bin/env bash
#
# 把 cargo 产出的裸二进制包成 macOS 应用包。
#
# 为什么需要这一步：Finder / Dock / 切换器只认 `.app` bundle 里的
# Contents/Resources/*.icns（由 Info.plist 的 CFBundleIconFile 指定）。
# 裸 Unix 可执行文件在 Finder 里永远是那张通用「终端窗口」图标 ——
# 无论二进制里嵌了什么资源。
#
# 这是**底层打包工具**，只负责「二进制 + icns + Info.plist → .app」。
# 日常构建请用 scripts/build.sh，它会把图标、编译、打包、产物汇总串起来。
#
# 用法：
#   scripts/bundle.sh                          # debug 构建，输出到 target/debug/
#   scripts/bundle.sh release                  # release 构建，输出到 target/release/
#   scripts/bundle.sh release --out dist       # 输出到 dist/Blip.app
#   scripts/bundle.sh release --out dist --no-build   # 不重新编译，只重新打包
#   scripts/bundle.sh release --version 0.2.0   # 覆盖应用包版本号
#
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

PROFILE="debug"
DO_BUILD=1
OUT_DIR=""
VERSION=""

while [ $# -gt 0 ]; do
  case "$1" in
    debug|release) PROFILE="$1" ;;
    --no-build)    DO_BUILD=0 ;;
    --out)         shift; OUT_DIR="${1:-}" ;;
    -v|--version)
      [ $# -ge 2 ] && [ -n "$2" ] \
        || { echo "$1 后缺少版本号" >&2; exit 1; }
      VERSION="$2"
      shift
      ;;
    -h|--help)
      sed -n '13,18p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "未知参数：$1" >&2
      echo "用法：scripts/bundle.sh [debug|release] [--out DIR] [--no-build] [--version VERSION]" >&2
      exit 1
      ;;
  esac
  shift
done

case "${PROFILE}" in
  debug)   CARGO_FLAGS="" ;;
  release) CARGO_FLAGS="--release" ;;
esac

BIN="${ROOT}/target/${PROFILE}/blip"
ICNS="${ROOT}/assets/icon/AppIcon.icns"

# --out 相对路径按仓库根解析，避免在 scripts/ 里跑脚本时产物落错地方。
if [ -n "${OUT_DIR}" ]; then
  case "${OUT_DIR}" in
    /*) ;;
    *) OUT_DIR="${ROOT}/${OUT_DIR}" ;;
  esac
else
  OUT_DIR="${ROOT}/target/${PROFILE}"
fi
APP="${OUT_DIR}/Blip.app"

# 没有显式覆盖时，版本号从 Cargo.toml 读取。
if [ -z "${VERSION}" ]; then
  VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -1)
fi
VERSION=${VERSION#v}
[ -n "${VERSION}" ] || { echo "读不到 Cargo.toml 里的 version" >&2; exit 1; }
[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$ ]] \
  || { echo "版本号无效：${VERSION}（应类似 0.2.0）" >&2; exit 1; }

[ -f "${ICNS}" ] || {
  echo "缺少图标：${ICNS}" >&2
  echo "先跑一次 scripts/make-icon.sh" >&2
  exit 1
}

if [ "${DO_BUILD}" = "1" ]; then
  echo "→ cargo build ${PROFILE}"
  # shellcheck disable=SC2086
  cargo build --locked ${CARGO_FLAGS}
fi

[ -x "${BIN}" ] || { echo "找不到二进制：${BIN}" >&2; exit 1; }

echo "→ 组装 ${APP}"
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

cp "${BIN}" "${APP}/Contents/MacOS/blip"
cp "${ICNS}" "${APP}/Contents/Resources/AppIcon.icns"

# APPL???? —— 老工具（部分 Finder 插件、installer）仍会读这个 8 字节文件。
printf 'APPL????' > "${APP}/Contents/PkgInfo"

cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleExecutable</key>
    <string>blip</string>
    <key>CFBundleIdentifier</key>
    <string>com.macscp.app</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Blip</string>
    <key>CFBundleDisplayName</key>
    <string>Blip</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST

# 没有签名的 arm64 bundle 也能跑（链接器已给二进制做了 ad-hoc 签名），
# 但补一次 bundle 级签名能让 LaunchServices 更愿意刷新图标缓存。
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "${APP}" >/dev/null 2>&1 \
    || echo "  （codesign 失败，忽略：不影响本地运行）"
fi

# 让 Finder 重新读一次 bundle，否则可能还显示缓存里的旧图标。
touch "${APP}"

echo "完成：${APP}"
