#!/usr/bin/env bash
#
# 一键构建：图标 → 编译 → 打包 → 产物汇总到 dist/。
#
# 产物：
#   dist/macscp.app   双击启动的 macOS 应用包（Finder / Dock / ⌘Tab 都带图标）
#   dist/macscp       裸二进制，给命令行用（./dist/macscp）
#
# 用法：
#   scripts/build.sh                # release 构建，先清空 dist/（默认）
#   scripts/build.sh debug          # debug 构建，快很多，但产物大 3~4 倍
#   scripts/build.sh --no-clean     # 增量构建，不清空 dist/
#   scripts/build.sh --no-binary    # 只出 .app，不额外复制裸二进制
#   scripts/build.sh --no-icon      # 跳过图标检查/重新生成
#
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

PROFILE="release"
CLEAN=1
WITH_BINARY=1
WITH_ICON=1

while [ $# -gt 0 ]; do
  case "$1" in
    debug|release) PROFILE="$1" ;;
    --clean)       CLEAN=1 ;;
    --no-clean)    CLEAN=0 ;;
    --no-binary)   WITH_BINARY=0 ;;
    --no-icon)     WITH_ICON=0 ;;
    -h|--help)
      sed -n '9,14p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "未知参数：$1" >&2
      echo "用法：scripts/build.sh [debug|release] [--no-clean] [--no-binary] [--no-icon]" >&2
      exit 1
      ;;
  esac
  shift
done

case "${PROFILE}" in
  debug)   CARGO_FLAGS="" ;;
  release) CARGO_FLAGS="--release" ;;
esac

DIST="${ROOT}/dist"
SVG="${ROOT}/assets/icon/macscp-icon.svg"
ICNS="${ROOT}/assets/icon/AppIcon.icns"
START=$(date +%s)

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
"${ROOT}/scripts/bundle.sh" "${PROFILE}" --out "${DIST}"

# ---- 4. 裸二进制 ----------------------------------------------------------
BIN="${ROOT}/target/${PROFILE}/macscp"
if [ "${WITH_BINARY}" = "1" ]; then
  cp "${BIN}" "${DIST}/macscp"
  chmod +x "${DIST}/macscp"
fi

# ---- 5. 汇总 --------------------------------------------------------------
ELAPSED=$(( $(date +%s) - START ))

echo
echo "产物（${PROFILE}，${ELAPSED}s）："
du -sh "${DIST}/macscp.app" | awk -v d="${DIST}" '{printf "  %-6s %s/macscp.app\n", $1, d}'
if [ "${WITH_BINARY}" = "1" ]; then
  du -sh "${DIST}/macscp" | awk -v d="${DIST}" '{printf "  %-6s %s/macscp\n", $1, d}'
fi
echo
echo "双击启动：open \"${DIST}/macscp.app\""
# 注意别写成 `[ ... ] && echo ...`：条件是假时整行返回 1，脚本最后一条命令
# 的状态码就是脚本的退出码，--no-binary 会「成功构建但退出码 1」。
if [ "${WITH_BINARY}" = "1" ]; then
  echo "命令行：  \"${DIST}/macscp\""
fi
