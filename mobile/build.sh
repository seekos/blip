#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
UPSTREAM_REV="5fd54f4554263ae06753e90472c7f2e2ff5d7495"
HOST_DIR="$SCRIPT_DIR/.host"
UPSTREAM_DIR="$HOST_DIR/gpui-mobile-$UPSTREAM_REV"
IOS_DIR="$HOST_DIR/ios"
ANDROID_DIR="$HOST_DIR/android"

usage() {
    echo "用法：$0 ios [--simulator|--device] [--release] [--no-run] [--out DIR] [-v VERSION]"
    echo "      $0 android [--device|--emulator] [--release] [--no-run] [--out DIR] [-v VERSION]"
}

if [ "$#" -lt 1 ]; then
    usage
    exit 1
fi

PLATFORM="$1"
shift
PROFILE="debug"
TARGET_KIND="device"
NO_RUN=false
OUT_DIR=""
VERSION=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --simulator) TARGET_KIND="simulator" ;;
        --device) TARGET_KIND="device" ;;
        --emulator) TARGET_KIND="emulator" ;;
        --release) PROFILE="release" ;;
        --no-run) NO_RUN=true ;;
        --out)
            [ "$#" -ge 2 ] && [ -n "$2" ] \
                || { echo "$1 后缺少输出目录" >&2; exit 1; }
            OUT_DIR="$2"
            shift
            ;;
        -v|--version)
            [ "$#" -ge 2 ] && [ -n "$2" ] \
                || { echo "$1 后缺少版本号" >&2; exit 1; }
            VERSION="$2"
            shift
            ;;
        -h|--help) usage; exit 0 ;;
        *) echo "未知参数：$1" >&2; usage; exit 1 ;;
    esac
    shift
done

if [ -z "$VERSION" ]; then
    VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
fi
VERSION="${VERSION#v}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$ ]] \
    || { echo "版本号无效：$VERSION（应类似 0.2.0）" >&2; exit 1; }

VERSION_CORE="$(printf '%s\n' "$VERSION" | sed -E 's/^([0-9]+\.[0-9]+\.[0-9]+).*/\1/')"
IFS=. read -r VERSION_MAJOR VERSION_MINOR VERSION_PATCH <<< "$VERSION_CORE"
VERSION_CODE=$((10#$VERSION_MAJOR * 1000000 + 10#$VERSION_MINOR * 1000 + 10#$VERSION_PATCH))
[ "$VERSION_CODE" -le 2100000000 ] \
    || { echo "版本号过大，无法生成 Android versionCode：$VERSION" >&2; exit 1; }

if [ -n "$OUT_DIR" ]; then
    case "$OUT_DIR" in
        /*) ;;
        *) OUT_DIR="$PROJECT_DIR/$OUT_DIR" ;;
    esac
    mkdir -p "$OUT_DIR"
fi

prepare_upstream() {
    mkdir -p "$HOST_DIR"
    if [ ! -d "$UPSTREAM_DIR/.git" ]; then
        echo "==> 获取固定版本的 gpui-mobile 原生宿主模板"
        git clone --quiet https://github.com/itsbalamurali/gpui-mobile.git "$UPSTREAM_DIR"
        git -C "$UPSTREAM_DIR" checkout --quiet "$UPSTREAM_REV"
    fi
}

prepare_ios_host() {
    prepare_upstream
    mkdir -p "$IOS_DIR"
    cp -R "$UPSTREAM_DIR/example/ios/." "$IOS_DIR/"
    cp "$SCRIPT_DIR/platform/ios/project.yml" "$IOS_DIR/project.yml"
    cp "$SCRIPT_DIR/platform/ios/Info.plist" "$IOS_DIR/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" \
        "$IOS_DIR/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION_CODE" \
        "$IOS_DIR/Info.plist"
    cp "$PROJECT_DIR/docs/app-icon.png" \
        "$IOS_DIR/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png"
}

prepare_android_host() {
    prepare_upstream
    mkdir -p "$ANDROID_DIR"
    cp -R "$UPSTREAM_DIR/example/android/gradle/." "$ANDROID_DIR/"
    cp "$SCRIPT_DIR/platform/android/app.build.gradle.kts" \
        "$ANDROID_DIR/app/build.gradle.kts"
    sed -E \
        -e "s/versionCode = [0-9]+/versionCode = $VERSION_CODE/" \
        -e "s/versionName = \"[^\"]+\"/versionName = \"$VERSION\"/" \
        "$ANDROID_DIR/app/build.gradle.kts" > "$ANDROID_DIR/app/build.gradle.kts.tmp"
    mv "$ANDROID_DIR/app/build.gradle.kts.tmp" "$ANDROID_DIR/app/build.gradle.kts"
    cp "$SCRIPT_DIR/platform/android/settings.gradle.kts" \
        "$ANDROID_DIR/settings.gradle.kts"
    cp "$SCRIPT_DIR/platform/android/AndroidManifest.xml" \
        "$ANDROID_DIR/app/src/main/AndroidManifest.xml"
    mkdir -p "$ANDROID_DIR/app/src/main/java/com/amov/blip/mobile"
    cp "$SCRIPT_DIR"/platform/android/Blip*.java \
        "$ANDROID_DIR/app/src/main/java/com/amov/blip/mobile/"
    mkdir -p "$ANDROID_DIR/app/src/main/java/dev/gpui/mobile"
    cp "$SCRIPT_DIR/platform/android/GpuiPathPicker.java" \
        "$ANDROID_DIR/app/src/main/java/dev/gpui/mobile/GpuiPathPicker.java"
    for icon in "$ANDROID_DIR"/app/src/main/res/mipmap-*/ic_launcher*.png; do
        cp "$PROJECT_DIR/docs/app-icon.png" "$icon"
    done
    chmod +x "$ANDROID_DIR/gradlew"
}

profile_flags() {
    if [ "$PROFILE" = "release" ]; then
        echo "--release"
    fi
}

package_ios_product() {
    local product="$1"
    [ -n "$OUT_DIR" ] || return

    if [ "$TARGET_KIND" = "simulator" ]; then
        local output="$OUT_DIR/Blip-$VERSION-ios-simulator-arm64.zip"
        rm -f "$output"
        /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$product" "$output"
        echo "==> iOS 打包产物：$output"
        return
    fi

    local suffix=""
    /usr/bin/codesign --verify --deep --strict "$product" >/dev/null 2>&1 \
        || suffix="-unsigned"
    local output="$OUT_DIR/Blip-$VERSION-ios-arm64$suffix.ipa"
    local stage
    stage="$(mktemp -d "${TMPDIR:-/tmp}/blip-ios.XXXXXX")"
    mkdir -p "$stage/Payload"
    cp -R "$product" "$stage/Payload/Blip.app"
    rm -f "$output"
    (cd "$stage" && /usr/bin/zip -qry "$output" Payload)
    rm -rf "$stage"
    echo "==> iOS 打包产物：$output"
}

package_android_product() {
    local product="$1"
    [ -n "$OUT_DIR" ] || return

    local suffix=""
    [ "$PROFILE" = "debug" ] && suffix="-debug"
    [[ "$product" == *-unsigned.apk ]] && suffix="-unsigned"
    if [ "$PROFILE" = "release" ] && [ -z "${BLIP_ANDROID_KEYSTORE:-}" ]; then
        suffix="-debug-signed"
    fi
    local output="$OUT_DIR/Blip-$VERSION-android-arm64$suffix.apk"
    cp "$product" "$output"
    echo "==> Android 打包产物：$output"
}

build_ios() {
    if [ "$TARGET_KIND" = "emulator" ]; then
        echo "Android 模拟器请使用 android --emulator" >&2
        exit 1
    fi
    prepare_ios_host
    command -v xcodegen >/dev/null || {
        echo "缺少 XcodeGen：brew install xcodegen" >&2
        exit 1
    }
    if [ -n "${BLIP_DEVELOPMENT_TEAM:-}" ] \
        && [[ ! "$BLIP_DEVELOPMENT_TEAM" =~ ^[A-Z0-9]{10}$ ]]; then
        echo "BLIP_DEVELOPMENT_TEAM 无效：应为 10 位 Apple Team ID" >&2
        exit 1
    fi

    local rust_target="aarch64-apple-ios"
    local sdk="iphoneos"
    local destination="generic/platform=iOS"
    if [ "$TARGET_KIND" = "simulator" ]; then
        rust_target="aarch64-apple-ios-sim"
        sdk="iphonesimulator"
        destination="generic/platform=iOS Simulator"
    fi
    rustup target add "$rust_target"

    echo "==> 构建 Rust 静态库（${rust_target}, ${PROFILE}）"
    IPHONEOS_DEPLOYMENT_TARGET=16.0 \
    cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" --lib \
        --target "$rust_target" $(profile_flags)

    echo "==> 生成并构建 iOS 工程"
    (cd "$IOS_DIR" && xcodegen generate --spec project.yml)
    local configuration="Debug"
    [ "$PROFILE" = "release" ] && configuration="Release"
    local team_args=("COMPILER_INDEX_STORE_ENABLE=NO")
    local provisioning_flag=""
    if [ -n "${BLIP_DEVELOPMENT_TEAM:-}" ]; then
        team_args+=("DEVELOPMENT_TEAM=$BLIP_DEVELOPMENT_TEAM")
        provisioning_flag="-allowProvisioningUpdates"
    elif [ "$TARGET_KIND" = "device" ]; then
        team_args+=("CODE_SIGNING_ALLOWED=NO" "CODE_SIGNING_REQUIRED=NO")
    fi
    xcodebuild \
        -project "$IOS_DIR/BlipMobile.xcodeproj" \
        -scheme BlipMobile \
        -configuration "$configuration" \
        -sdk "$sdk" \
        -destination "$destination" \
        -derivedDataPath "$IOS_DIR/build" \
        ${provisioning_flag:+"$provisioning_flag"} \
        "${team_args[@]}" \
        build

    local product="$IOS_DIR/build/Build/Products/${configuration}-${sdk}/Blip.app"
    echo "==> iOS 产物：$product"
    package_ios_product "$product"
    if $NO_RUN; then
        return
    fi
    if [ "$TARGET_KIND" = "simulator" ]; then
        local simulator
        simulator="$(xcrun simctl list devices available | awk -F '[()]' '/iPhone/ {print $2; exit}')"
        [ -n "$simulator" ] || { echo "没有可用的 iOS 模拟器" >&2; exit 1; }
        xcrun simctl boot "$simulator" 2>/dev/null || true
        open -a Simulator
        xcrun simctl bootstatus "$simulator" -b
        xcrun simctl install "$simulator" "$product"
        xcrun simctl launch --terminate-running-process "$simulator" com.amov.Blip.mobile
    else
        echo "真机安装请在生成的 Xcode 工程中选择签名团队，或设置 BLIP_DEVELOPMENT_TEAM 后重试。"
    fi
}

find_android_ndk() {
    if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
        echo "$ANDROID_NDK_HOME"
        return
    fi
    local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
    if [ -z "$sdk" ] && [ -d "$HOME/Library/Android/sdk" ]; then
        sdk="$HOME/Library/Android/sdk"
    fi
    if [ -z "$sdk" ] && [ -d "$HOME/Android/Sdk" ]; then
        sdk="$HOME/Android/Sdk"
    fi
    [ -n "$sdk" ] || return 1
    find "$sdk/ndk" -mindepth 1 -maxdepth 1 -type d | sort | tail -1
}

build_android() {
    if [ "$TARGET_KIND" = "simulator" ]; then
        echo "Android 模拟器请使用 --emulator" >&2
        exit 1
    fi
    prepare_android_host
    local signing_values=(
        "${BLIP_ANDROID_KEYSTORE:-}"
        "${BLIP_ANDROID_KEY_ALIAS:-}"
        "${BLIP_ANDROID_STORE_PASSWORD:-}"
    )
    local signing_count=0
    local value
    for value in "${signing_values[@]}"; do
        [ -z "$value" ] || signing_count=$((signing_count + 1))
    done
    if [ "$signing_count" -ne 0 ] && [ "$signing_count" -ne 3 ]; then
        echo "Android 签名需同时设置 BLIP_ANDROID_KEYSTORE、BLIP_ANDROID_KEY_ALIAS 和 BLIP_ANDROID_STORE_PASSWORD" >&2
        exit 1
    fi
    if [ "$signing_count" -eq 3 ] && [ ! -f "$BLIP_ANDROID_KEYSTORE" ]; then
        echo "找不到 Android keystore：$BLIP_ANDROID_KEYSTORE" >&2
        exit 1
    fi
    if [ "$PROFILE" = "release" ] && [ "$signing_count" -eq 0 ]; then
        echo "==> 未配置发布 keystore，使用 Android 调试证书生成可安装测试包"
    fi
    local ndk
    ndk="$(find_android_ndk)" || { echo "找不到 Android NDK" >&2; exit 1; }
    if [ -z "${ANDROID_HOME:-}" ] && [ -z "${ANDROID_SDK_ROOT:-}" ]; then
        export ANDROID_HOME="$(dirname "$(dirname "$ndk")")"
    fi
    local host_tag="linux-x86_64"
    [ "$(uname -s)" = "Darwin" ] && host_tag="darwin-x86_64"
    local tools="$ndk/toolchains/llvm/prebuilt/$host_tag/bin"
    local clang="$tools/aarch64-linux-android26-clang"
    [ -x "$clang" ] || { echo "找不到 Android clang：$clang" >&2; exit 1; }

    rustup target add aarch64-linux-android
    echo "==> 构建 Rust 动态库（arm64-v8a, ${PROFILE}）"
    CC_aarch64_linux_android="$clang" \
    AR_aarch64_linux_android="$tools/llvm-ar" \
    CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$clang" \
        cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" --lib \
            --target aarch64-linux-android $(profile_flags)

    local jni_dir="$ANDROID_DIR/app/src/main/jniLibs/arm64-v8a"
    mkdir -p "$jni_dir"
    cp "$SCRIPT_DIR/target/aarch64-linux-android/$PROFILE/libblip_mobile.so" "$jni_dir/"
    if [ "$PROFILE" = "debug" ]; then
        # Cargo keeps full DWARF data in target/ for native debugging. Do not
        # copy hundreds of megabytes of those symbols into every installable APK.
        "$tools/llvm-strip" --strip-debug "$jni_dir/libblip_mobile.so"
    fi

    local task="assembleDebug"
    local variant="debug"
    if [ "$PROFILE" = "release" ]; then
        task="assembleRelease"
        variant="release"
    fi
    echo "==> 组装 Android APK"
    rm -rf "$ANDROID_DIR/app/build/outputs/apk/$variant"
    (cd "$ANDROID_DIR" && ./gradlew "$task")
    local apk="$ANDROID_DIR/app/build/outputs/apk/$variant/app-$variant.apk"
    if [ ! -f "$apk" ] && [ -f "$ANDROID_DIR/app/build/outputs/apk/$variant/app-$variant-unsigned.apk" ]; then
        apk="$ANDROID_DIR/app/build/outputs/apk/$variant/app-$variant-unsigned.apk"
    fi
    echo "==> Android 产物：$apk"
    package_android_product "$apk"
    if $NO_RUN; then
        return
    fi
    if [[ "$apk" == *-unsigned.apk ]]; then
        echo "Release APK 尚未签名；请签名后安装，或使用 debug 构建直接安装。" >&2
        exit 1
    fi
    local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
    [ -z "$sdk" ] && [ -d "$HOME/Library/Android/sdk" ] && sdk="$HOME/Library/Android/sdk"
    [ -z "$sdk" ] && [ -d "$HOME/Android/Sdk" ] && sdk="$HOME/Android/Sdk"
    local adb="$sdk/platform-tools/adb"
    [ -x "$adb" ] || { echo "找不到 adb；APK 已生成。" >&2; exit 1; }
    "$adb" install -r "$apk"
    "$adb" shell am start -n com.amov.blip.mobile/.BlipActivity
}

case "$PLATFORM" in
    ios) build_ios ;;
    android) build_android ;;
    -h|--help) usage ;;
    *) usage; exit 1 ;;
esac

