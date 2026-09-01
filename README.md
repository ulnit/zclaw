# zclaw — OmniMind 移动端本地 Agent（ulnclaw 的移动端集成层）

把鸿蒙版 OmniMind 中用到的 **移动端可用的 ZClaw** 抽出来，作为三端
（HarmonyOS / Android / iOS）共用的独立项目。

## 它是什么

ZClaw 是 [ulnclaw](https://gitee.com/ushaw/ulnclaw)（Rust 实现的 Hermes-parity
agent：50+ 工具、MCP、浏览器）编译为移动端可用的原生库 `libzclaw`，通过
统一的 C FFI（`include/zclaw.h`）暴露给三端：

```
┌─────────────────────────────────────────────────────────┐
│  ArkTS / Kotlin / Swift  客户端层                        │
├──────────────┬──────────────┬───────────────────────────┤
│ harmony/     │ android/     │ ios/                      │
│ zclaw_napi   │ zclaw_jni    │ ZClawBridge(.h/.mm)       │
│ (C++ NAPI)   │ (C++ JNI)    │ (Obj-C 桥 + Swift 客户端)  │
│ dlopen       │ dlopen       │ 静态链接 libzclaw.a        │
├──────────────┴──────────────┴───────────────────────────┤
│  libzclaw（Rust，gitee.com/ushaw/ulnclaw 编译）           │
│  FFI v0.2：init / chat / poll_chunks / is_running /      │
│            cancel / get_sessions / get_messages / free    │
└─────────────────────────────────────────────────────────┘
```

## FFI 契约（v0.2，轮询式流式）

以 `include/zclaw.h` 为准（从鸿蒙 `zclaw_napi.cpp` 的实际调用反推并固化）：

| 函数 | 说明 |
|---|---|
| `zclaw_init(config_json)` | 初始化；config 含 api_url/api_key/default_model/workspace_dir/security/memory/agent |
| `zclaw_chat(message)` | 提交消息，异步运行；0=接受，-1=错误，-2=忙 |
| `zclaw_poll_chunks()` | 轮询输出块（JSON 数组）；调用方须 `zclaw_free()` 释放 |
| `zclaw_is_running()` | 1=运行中，0=空闲 |
| `zclaw_cancel()` | 取消；0=成功，-1=无任务 |
| `zclaw_get_sessions()` / `zclaw_get_messages(sid)` | SQLite 会话存储（Rust 内部管理） |
| `zclaw_free(ptr)` / `zclaw_version()` | 内存释放 / 版本 |

chunk 类型：`0=text 1=tool_call 2=tool_result 3=done 4=error 5=thinking`

## 目录结构

```
include/            zclaw.h — 三端共用的 C FFI 契约头
harmony/
  cpp/              zclaw_napi.cpp + CMakeLists.txt（NAPI 桥，dlopen）
  libs/<abi>/       libzclaw.so（OHOS 预编译）
  ets/              ZClawApi.ets / ZClawTools.ets（ArkTS 客户端，含降级逻辑）
android/
  jni/              zclaw_jni.cpp + CMakeLists.txt（JNI 桥，dlopen）
  kotlin/           ZClawNative.kt（Kotlin 绑定）
ios/
  Sources/          ZClawBridge.h/.mm（C 桥，静态链接）+ ZClawClient.swift
scripts/
  build.sh          从 ulnclaw Rust 源码交叉编译三端产物
rust/               （放 ulnclaw 源码克隆，不入库）
```

## ⚠️ 关键注意事项

**OHOS 的 .so 不能用于 Android。** `harmony/libs/` 下的 `libzclaw.so`
虽然放在 `arm64-v8a` 目录，但其动态依赖包含 `libtime_service_ndk.so`
（HarmonyOS NDK 专有），在 Android（bionic libc）上会加载失败。
Android 必须用 `scripts/build.sh`（cargo-ndk）从
[gitee.com/ushaw/ulnclaw](https://gitee.com/ushaw/ulnclaw) 重新编译。

**iOS 不允许 dlopen** 外部动态库，必须编译为静态库 `libzclaw.a`
（或 XCFramework）静态链接进 App。

## 三端集成方式

- **HarmonyOS**：已集成（omnimind-harmony），NAPI 加载，不可用时降级到
  ArkTS 轻量 agent loop（ZClawTools.ets 的 5 工具）。
- **Android**：JNI 桥 + `System.loadLibrary("zclaw_jni")`，把 Android 版
  `libzclaw.so` 放 `jniLibs/<abi>/`。
- **iOS**：静态链接 `libzclaw.a`，Obj-C 桥经桥接头暴露给 Swift。

## 构建产物（scripts/build.sh）

```bash
# 先克隆源码
git clone https://gitee.com/ushaw/ulnclaw.git rust/ulnclaw

PLATFORM=android ./scripts/build.sh   # -> dist/android/<abi>/libzclaw.so
PLATFORM=ios     ./scripts/build.sh   # -> dist/ios/ZClawKit.xcframework
PLATFORM=harmony ./scripts/build.sh   # -> dist/harmony/arm64-v8a/libzclaw.so
```

## 关联

- 上游 Rust 源码（主仓库）：https://gitee.com/ushaw/ulnclaw
- 宿主 APP：omnimind-app（Android/iOS）、omnimind-harmony（鸿蒙）
- 后端：new-api（ai.ulnit.com）
