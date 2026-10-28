#!/usr/bin/env bash
#
# macOS / Linux 一键构建，产物汇总到 dist/。
#
# 产物：
#   macOS: dist/Blip.app + dist/Blip-<version>-macos-<arch>.dmg
#   Linux: dist/Blip-<version>-linux-<arch>.tar.gz
#
# 用法：
#   scripts/build.sh                # release 构建，先清空 dist/（默认）
#   scripts/build.sh -v 0.2.0       # 指定 APP 与 DMG 的版本号
#   scripts/build.sh debug          # debug 构建，快很多，但产物大 3~4 倍
#   scripts/build.sh --no-clean     # 增量构建，不清空 dist/
#   scripts/build.sh --no-icon      # 跳过图标检查/重新生成
#
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

PROFILE="release"
CLEAN=1
WITH_ICON=1
VERSION=""
VERSION_FILE="${ROOT}/.macscp-build-version"

while [ $# -gt 0 ]; do
  case "$1" in
    debug|release) PROFILE="$1" ;;
    -v|--version)
      [ $# -ge 2 ] && [ -n "$2" ] \
        || { echo "$1 后缺少版本号" >&2; exit 1; }
      VERSION="$2"
      shift
      ;;
    --clean)       CLEAN=1 ;;
    --no-clean)    CLEAN=0 ;;
    --no-icon)     WITH_ICON=0 ;;
    -h|--help)
      sed -n '9,14p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "未知参数：$1" >&2
      echo "用法：scripts/build.sh [debug|release] [-v VERSION] [--no-clean] [--no-icon]" >&2
      exit 1
      ;;
  esac
  shift
done

if [ -z "${VERSION}" ] && [ -s "${VERSION_FILE}" ]; then
  VERSION=$(<"${VERSION_FILE}")
fi
if [ -z "${VERSION}" ]; then
  VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -1)
fi
VERSION=${VERSION#v}
[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$ ]] \
  || { echo "版本号无效：${VERSION}（应类似 0.2.0）" >&2; exit 1; }

case "${PROFILE}" in
  debug)   CARGO_FLAGS="" ;;
  release) CARGO_FLAGS="--release" ;;
esac

DIST="${ROOT}/dist"
SVG="${ROOT}/assets/icon/macscp-icon.svg"
ICNS="${ROOT}/assets/icon/AppIcon.icns"
START=$(date +%s)
PLATFORM=$(uname -s)

# Linux 直接打包可执行文件；Windows 使用同目录的 build.ps1。
if [ "${PLATFORM}" = "Linux" ]; then
  if [ "${CLEAN}" = "1" ] && [ -d "${DIST}" ]; then
    case "${DIST}" in
      */dist) rm -rf "${DIST}" ;;
      *) echo "拒绝清空：${DIST} 不像构建产物目录" >&2; exit 1 ;;
    esac
  fi
  mkdir -p "${DIST}"

  echo "→ cargo build ${PROFILE}"
  # shellcheck disable=SC2086
  cargo build --locked ${CARGO_FLAGS}

  ARCH=$(uname -m)
  ARCHIVE="${DIST}/Blip-${VERSION}-linux-${ARCH}.tar.gz"
  STAGE=$(mktemp -d "${TMPDIR:-/tmp}/blip-linux.XXXXXX")
  trap 'rm -rf "${STAGE}"' EXIT
  cp "${ROOT}/target/${PROFILE}/blip" "${STAGE}/blip"
  cp "${ROOT}/README.md" "${STAGE}/README.md"
  tar -czf "${ARCHIVE}" -C "${STAGE}" blip README.md
  printf '%s\n' "${VERSION}" > "${VERSION_FILE}"

  ELAPSED=$(( $(date +%s) - START ))
  echo
  echo "产物（${PROFILE}，${ELAPSED}s）："
  du -sh "${ARCHIVE}" | awk -v p="${ARCHIVE}" '{printf "  %-6s %s\n", $1, p}'
  exit 0
fi

[ "${PLATFORM}" = "Darwin" ] || {
  echo "不支持的构建主机：${PLATFORM}；Windows 请运行 scripts/build.ps1" >&2
  exit 1
}

# ---- 1. 图标 --------------------------------------------------------------
# 只在「没有 icns」或「svg 比 icns 新」时重生成：Chrome 渲染 + iconset
# 要 2 秒左右，每次都跑没必要，但改了 svg 忘记重跑会导致图标静默过期。
if [ "${WITH_ICON}" = "1" ]; then
  if [ ! -f "${ICNS}" ]; then
    echo "→ 图标缺失，先生成"
    "${ROOT}/scripts/make-icon.sh"
  elif [ "${SVG}" -nt "${ICNS}" ]; then
    echo "→ ${SVG##*/} 比 icns 新，重新生成图标"
    "${ROOT}/scripts/make-icon.sh"
  fi
fi

# ---- 2. 清理（默认开启） --------------------------------------------------
# 默认清空，是为了让 dist/ 里永远只有「这一次构建」的产物 —— 否则
# release 和 debug 交替构建时，残留的旧 .app 很容易被误当成新的。
# 只要增量重打包（比如只改了 Info.plist 想快一点）就加 --no-clean。
if [ "${CLEAN}" = "1" ]; then
  # 只允许删 <repo>/dist。万一以后有人改 DIST 或从别处 source 这个脚本，
  # 一句 rm -rf 就能把无关目录端掉 —— 这里必须挡一道。
  case "${DIST}" in
    */dist) ;;
    *) echo "拒绝清空：${DIST} 不像构建产物目录（应以 /dist 结尾）" >&2; exit 1 ;;
  esac
  if [ -d "${DIST}" ]; then
    echo "→ 清空 dist/"
    rm -rf "${DIST}"
  fi
fi
mkdir -p "${DIST}"

# ---- 3. 编译 + 打包 -------------------------------------------------------
# 编译交给 bundle.sh（它会跑 cargo build），这里不再单独 build 一次。
"${ROOT}/scripts/bundle.sh" "${PROFILE}" --out "${DIST}" --version "${VERSION}"

# ---- 4. DMG ---------------------------------------------------------------
DMG_STAGE=$(mktemp -d "${TMPDIR:-/tmp}/blip-dmg.XXXXXX")
trap 'rm -rf "${DMG_STAGE}"' EXIT
cp -R "${DIST}/Blip.app" "${DMG_STAGE}/Blip.app"
ln -s /Applications "${DMG_STAGE}/Applications"
MAC_ARCH=$(uname -m)
DMG="${DIST}/Blip-${VERSION}-macos-${MAC_ARCH}.dmg"
rm -f "${DMG}"
if /usr/sbin/diskutil image create from --help >/dev/null 2>&1; then
  /usr/sbin/diskutil image create from --format UDZO --volumeName "Blip" \
    "${DMG_STAGE}" "${DMG}" >/dev/null
else
  /usr/bin/hdiutil create -volname "Blip" -srcfolder "${DMG_STAGE}" \
    -format UDZO "${DMG}" >/dev/null
fi
rm -rf "${DMG_STAGE}"
trap - EXIT
printf '%s\n' "${VERSION}" > "${VERSION_FILE}"

# ---- 5. 汇总 --------------------------------------------------------------
ELAPSED=$(( $(date +%s) - START ))

echo
echo "产物（${PROFILE}，${ELAPSED}s）："
du -sh "${DIST}/Blip.app" | awk -v d="${DIST}" '{printf "  %-6s %s/Blip.app\n", $1, d}'
du -sh "${DMG}" | awk -v p="${DMG}" '{printf "  %-6s %s\n", $1, p}'
echo
echo "双击启动：open \"${DIST}/Blip.app\""
