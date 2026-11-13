# Blip Mobile

Blip 的 iOS/Android 客户端，基于固定版本的 `gpui-mobile` 和 GPUI Kit。业务层直接复用项目现有的 SFTP、局域网发现、加密配置和内置 SFTP 服务；界面按移动端习惯改成单栏远端文件列表、系统文件选择器、底部导航和应用内模态框。

## 功能

- 密码/私钥 SFTP 连接、主机密钥确认、公钥部署、收藏置顶/命名/删除与最近连接
- 远端目录浏览、刷新、返回上级、新建、重命名、删除和文本编辑
- 文件/文件夹上传与下载、断点续传、冲突选择、进度、速度和取消
- UDP + mDNS 局域网发现、多网卡上传与自动重连
- 内置局域网 SFTP 与 HTTP 文件共享服务、访问二维码、共享目录、端口和密码设置
- age 主密码加密配置、自动解锁、深色模式、安全区适配和版本更新检查

下载内容保存到应用 Documents 目录。iOS 已启用“文件”App 共享；Android 通过应用文件目录访问。所有危险确认均使用应用内模态框，没有浏览器 `alert/confirm/prompt`。
Android 前台服务可维持局域网服务在后台运行；iOS 进入后台后可能暂停网络监听，应保持 Blip 在前台接收文件。

## 构建

首次构建会把固定提交 `5fd54f4554263ae06753e90472c7f2e2ff5d7495` 的原生宿主模板下载到忽略提交的 `.host/` 目录。

```bash
# 在 Blip 项目根目录执行

# iOS 模拟器（需要 Xcode、XcodeGen）
brew install xcodegen
./mobile/build.sh ios --simulator

# iOS 真机；Apple Team ID 也可在生成的 Xcode 工程中设置
BLIP_DEVELOPMENT_TEAM=XXXXXXXXXX ./mobile/build.sh ios --device --release

# Android arm64（需要 Android SDK/NDK、JDK 17+）
./mobile/build.sh android --device
./mobile/build.sh android --release --no-run

# 将版本化安装包汇总到项目 dist/（统一构建入口会自动传这些参数）
./mobile/build.sh ios --device --release --no-run --out dist --version 0.1.0
./mobile/build.sh android --release --no-run --out dist --version 0.1.0
```

只生成产物、不安装运行时添加 `--no-run`。iOS 工程位于 `.host/ios/BlipMobile.xcodeproj`，Android APK 位于 `.host/android/app/build/outputs/apk/`。
Debug APK 使用 Android 调试证书，可直接安装；Release 未配置发布 keystore 时也会回退到调试
证书并生成 `debug-signed.apk`，可安装测试，但分发前必须换成自己的发布证书。
设置 `--out` 后会额外生成版本化的 IPA/APK；未设置 `BLIP_DEVELOPMENT_TEAM` 时，iOS 真机 IPA
同样以未签名形式输出，并在文件名中标记 `unsigned`。

## 发布签名

iOS 使用 Xcode 自动签名。先在 Xcode 登录 Apple Developer 账号并安装签名证书，再传入 Team ID：

```bash
BLIP_DEVELOPMENT_TEAM=XXXXXXXXXX \
./mobile/build.sh ios --device --release --no-run --out dist
```

Android Release 正式分发时使用已有 keystore，密码只通过环境变量传给 Gradle：

```bash
BLIP_ANDROID_KEYSTORE=/absolute/path/release.jks \
BLIP_ANDROID_KEY_ALIAS=blip \
BLIP_ANDROID_STORE_PASSWORD='store-password' \
BLIP_ANDROID_KEY_PASSWORD='key-password' \
./mobile/build.sh android --release --no-run --out dist
```

若 key password 与 store password 相同，可省略 `BLIP_ANDROID_KEY_PASSWORD`。三个必填变量只设置
一部分时脚本会直接报错，避免误产出未签名的 Release 包。

## 开发检查

```bash
cargo test --manifest-path mobile/Cargo.toml
cargo check --manifest-path mobile/Cargo.toml --target aarch64-apple-ios-sim

# Android 检查需要把 NDK clang 配成目标 linker；build.sh 已自动处理。
./mobile/build.sh android --no-run
```

