# zclaw — OmniMind 移动端本地 Agent（三端共用）

把鸿蒙版 OmniMind 用到的**移动端可用的 ZClaw** 抽成独立项目，供
HarmonyOS / Android / iOS 三端集成。底层是 Rust 实现的本地 agent：
工具调用循环 + 流式输出 + SQLite 长期记忆，走 OpenAI 兼容接口
（默认 `https://ai.ulnit.com/v1`）。

## ⚠️ 这份源码是怎么来的（重要）

**原始 `libzclaw.so` 只以预编译二进制提交在 `omnimind-harmony` 仓库，
Rust 源码不在任何可达的仓库里**（ulnclaw 桌面版 / zeroclaw / 65 个公开
仓库 / crates.io / 全代码搜索均已排查）。因此本仓库的 `rust/zclaw` 是
**对那个二进制的忠实重建**，依据是：

1. 二进制里嵌入的完整符号和字符串（12 个工具名+描述、12 个模块路径、
   `zclaw_memory.db`、`struct Config with 8 elements`、版本号
   `0.2.0-mobile`、`## Tools` 系统提示等）；
2. 鸿蒙端 `zclaw_napi.cpp`（FFI v0.2 契约，逐字节）；
3. 鸿蒙端 `ZClawApi.ets`（chunk 协议：`chunkType` 0–5 + `name/args/result`）。

> 若你能提供原始 `zclaw` crate 源码，应以其为准替换 `rust/zclaw`。

## 目录结构

```
include/zclaw.h          C FFI 契约（9 个导出 + zclaw_set_session 扩展）
rust/zclaw/              Rust crate（cdylib + staticlib + rlib）
  src/ffi.rs             C ABI 入口
  src/agent/dispatcher.rs  工具调用循环
  src/providers/compatible.rs  OpenAI 兼容流式
  src/tools/mod.rs       12 个工具
  src/memory.rs          SQLite + FTS5/BM25 长期记忆
harmony/                 鸿蒙端桥接（NAPI wrapper + 预编译 .so + ArkTS）
android/jni/             Android JNI 桥（zclaw_jni.cpp）
ios/Sources/             iOS C 桥（ZClawBridge + Swift client）
scripts/
  build-android.sh       cargo-ndk 交叉编译（arm64 + armv7）
  build-ios.sh           Mac 上编静态库（device + sim）
```

## FFI 契约（v0.2，轮询式流式）

```
zclaw_init(config_json) -> i32        0=ok -1=err
zclaw_chat(message) -> i32            0=accepted -1=err -2=busy
zclaw_set_session(id) -> i32          v0.2+ 扩展：多会话（原版无此符号）
zclaw_poll_chunks() -> *const c_char  JSON 数组 chunk
zclaw_is_running() -> i32             1=running 0=idle
zclaw_cancel() -> i32                 0=ok -1=无在跑任务
zclaw_get_sessions() -> *const c_char
zclaw_get_messages(session_id) -> *const c_char
zclaw_free(ptr)
zclaw_version() -> *const c_char      "0.2.0-mobile"
```

chunk JSON 字段：`chunkType`(0=text 1=tool_call 2=tool_result 3=done
4=error 5=thinking)、`name`、`args`、`result`。

## 编译

### Android（本机 / 服务器，需 NDK）
```bash
export ANDROID_NDK_HOME=/opt/android-ndk   # 或你的路径
rustup target add aarch64-linux-android armv7-linux-androideabi
cargo install cargo-ndk
./scripts/build-android.sh                 # 产物在 dist/android/<abi>/libzclaw.so
```

### iOS（必须在 Mac）
```bash
./scripts/build-ios.sh                     # 产物在 dist/ios/libzclaw-{device,sim}.a
```
iOS 不允许运行时 dlopen 外部 .so，因此静态链接（.a），由
`ios/Sources/ZClawBridge.mm` 包装。

## 集成到各端

- **Android**：`libzclaw.so` 放进 `jniLibs/<abi>/`；JNI 桥
  `libzclaw_jni.so` 由 app 的 CMake（`app/src/main/cpp`）随 APK 一起编。
  Kotlin 入口 `ZClawNative` + 封装 `ZClawNativeApi`（含轮询→协程、降级）。
- **iOS**：静态库 + `ZClawBridge`，Swift 入口 `ZClawClient`。
- **HarmonyOS**：继续用随包的预编译 `.so` + 现有 NAPI wrapper
  （也可换成本仓库重建版）。

## 说明

- 原版 `.so` 依赖 `libtime_service_ndk.so`（HarmonyOS NDK 专有），
  所以那份二进制只能用于鸿蒙；Android/iOS 需用本仓库重建版重新编译。
- 工具集、chunk 协议、SQLite 库名、版本号都与鸿蒙端实际使用的行为对齐，
  保证三端体验一致。
