#!/usr/bin/env bash
#
# macOS / Linux / Windows 一键构建，产物汇总到 dist/。
#
# 产物：
#   macOS: dist/Blip.app + dist/Blip-<version>-macos-<arch>.dmg
#   Linux: dist/Blip-<version>-linux-<arch>.tar.gz
#   Windows: dist/Blip-<version>-windows-<arch>.zip
#
# 用法：
#   scripts/build.sh                # 当前系统 release 构建，先清空 dist/
#   scripts/build.sh -v 0.2.0       # 指定版本号
#   scripts/build.sh debug          # debug 构建
#   scripts/build.sh --no-clean     # 增量构建，不清空 dist/
#   scripts/build.sh --no-icon      # macOS 跳过图标检查
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
      sed -n '10,15p' "$0" | sed 's/^# \{0,1\}//'
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

# 三个平台共用清理与版本逻辑，以下只处理编译产物格式差异。
if [ "${CLEAN}" = "1" ]; then
  case "${DIST}" in
    */dist) ;;
    *) echo "拒绝清空：${DIST} 不像构建产物目录" >&2; exit 1 ;;
  esac
  if [ -d "${DIST}" ]; then
    echo "→ 清空 dist/"
    rm -rf "${DIST}"
  fi
fi
mkdir -p "${DIST}"

if [ "${PLATFORM}" = "Linux" ]; then
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

case "${PLATFORM}" in
  MINGW*|MSYS*|CYGWIN*)
    command -v cygpath >/dev/null 2>&1 \
      || { echo "Windows 打包请在 Git Bash 中运行" >&2; exit 1; }
    command -v powershell.exe >/dev/null 2>&1 \
      || { echo "找不到 Windows PowerShell" >&2; exit 1; }

    echo "→ cargo build ${PROFILE}"
    # shellcheck disable=SC2086
    cargo build --locked ${CARGO_FLAGS}

    ARCH=$(uname -m)
    [ "${ARCH}" = "x86_64" ] || { echo "当前只打包 Windows x64" >&2; exit 1; }
    ARCHIVE="${DIST}/Blip-${VERSION}-windows-x86_64.zip"
    STAGE=$(mktemp -d "${TMPDIR:-/tmp}/blip-windows.XXXXXX")
    trap 'rm -rf "${STAGE}"' EXIT
    cp "${ROOT}/target/${PROFILE}/blip.exe" "${STAGE}/Blip.exe"
    cp "${ROOT}/README.md" "${STAGE}/README.md"

    BLIP_STAGE_WIN=$(cygpath -w "${STAGE}")
    BLIP_ARCHIVE_WIN=$(cygpath -w "${ARCHIVE}")
    export BLIP_STAGE_WIN BLIP_ARCHIVE_WIN
    powershell.exe -NoProfile -NonInteractive -Command \
      '$files = @((Join-Path $env:BLIP_STAGE_WIN "Blip.exe"), (Join-Path $env:BLIP_STAGE_WIN "README.md")); Compress-Archive -LiteralPath $files -DestinationPath $env:BLIP_ARCHIVE_WIN -Force' \
      >/dev/null
    printf '%s\n' "${VERSION}" > "${VERSION_FILE}"

    ELAPSED=$(( $(date +%s) - START ))
    echo
    echo "产物（${PROFILE}，${ELAPSED}s）："
    du -sh "${ARCHIVE}" | awk -v p="${ARCHIVE}" '{printf "  %-6s %s\n", $1, p}'
    exit 0
    ;;
esac

[ "${PLATFORM}" = "Darwin" ] || {
  echo "不支持的构建主机：${PLATFORM}" >&2
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

# ---- 2. 编译 + 打包 -------------------------------------------------------
# 编译交给 bundle.sh（它会跑 cargo build），这里不再单独 build 一次。
"${ROOT}/scripts/bundle.sh" "${PROFILE}" --out "${DIST}" --version "${VERSION}"

# ---- 3. DMG ---------------------------------------------------------------
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

# ---- 4. 汇总 --------------------------------------------------------------
ELAPSED=$(( $(date +%s) - START ))

echo
echo "产物（${PROFILE}，${ELAPSED}s）："
du -sh "${DIST}/Blip.app" | awk -v d="${DIST}" '{printf "  %-6s %s/Blip.app\n", $1, d}'
du -sh "${DMG}" | awk -v p="${DMG}" '{printf "  %-6s %s\n", $1, p}'
echo
echo "双击启动：open \"${DIST}/Blip.app\""
