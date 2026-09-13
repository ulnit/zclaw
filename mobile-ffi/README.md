# ulnclaw-mobile-ffi

把 [ulnclaw](https://gitee.com/ushaw/ulnclaw)（hermes-agent 的 Rust 移植，26 万行）
包成 **zclaw v0.2 C ABI** 的 `libzclaw.so`，供爻荚（Ulnvault）三端移动 App 加载。

## 为什么这层存在

- App 端 JNI/NAPI 桥与 Kotlin/ArkTS 解析器**零改动**：符号名（`zclaw_*`）、
  chunk 协议（chunkType 0-5 + name/args/result）、config JSON、sessions/messages
  JSON 全部与 zclaw 0.5.2 逐字段对齐。
- ulnclaw 上游保持 pristine（path 依赖，可继续 `git pull` 同步）。本 crate 只加
  移动端外壳，不改上游一行代码。

## 架构

```
App (Kotlin/ArkTS/Swift)
  └─ JNI/NAPI 桥（dlopen "libzclaw.so"，解析 zclaw_* 符号）— 不变
       └─ mobile-ffi（本 crate）
            ├─ lib.rs      C ABI + State + chat 主流程 + catch_unwind 边界
            ├─ chunk.rs    zclaw v0.2 chunk 协议（单元测试锁字段名）
            ├─ provider.rs ThinkingCapture：拦截 SSE 流抽取 delta_reasoning
            │              → thinking chunk（ulnclaw on_thinking 是无参信号）
            ├─ tools.rs    移动端安全工具子集 + Bing 覆盖 web_search
            └─ prompt.rs   工具使用策略 prompt + 收口提示（移植 zclaw v0.5.2）
       └─ ulnclaw (path 依赖 rlib)
            agent / provider / tools / session(SQLite) / context …
```

### 与 zclaw 0.5.2 的行为对齐（防回归清单）

| zclaw 0.5.2 修复 | 本 crate 等价实现 |
|---|---|
| system prompt 工具策略 | `prompt.rs::mobile_system_prompt()`（同源文案） |
| no_progress 提前收口 | `no_progress()` 判据相同（FNV-1a、阈值3、参数≥2种）；命中即 abort 内层任务 → 裸问重试 |
| 裸问重试（道歉兜底已禁用） | `run_chat()` need_bare_retry 分支：无工具重问；仍空→error chunk |
| UTF-8 安全截断 | `truncate_chars()`（按字符）+ tools.rs `truncate_utf8()`（按边界） |
| panic 不跨 FFI | 所有 `extern "C"` 入口 `catch_unwind` |
| Bing 境内可达 | tools.rs 覆盖 web_search（cn.bing.com，tilk 真链接，含摘要） |
| 传输错误分类（Kotlin isTransportError 回退） | `classify_error()` 保留 timeout/dns/tls/connection 关键字 |

### 移动端工具面（安全子集）

保留：files(read/write/patch/search)、web_search(Bing 覆盖)/web_extract、memory、
todo、session_search、clarify、skills。
排除：terminal、execute_code、process、desktop、browser、computer_use、platform、
project、delegate、cronjob、media、video、x_search（无 shell/无桌面/危险或无意义）。

### 原生接口适配（Phase 3 规划）

手机原生能力（剪贴板/TTS/相册/相机/分享）经 **App 注册的回调桥**暴露给引擎：
新增 FFI `zclaw_register_native_handler(kind, fn_ptr)`，Rust 侧工具 handler 通过
函数指针调回 App 层（Kotlin JNI upcall / ArkTS NAPI threadsafe function）。
MVP 先不含原生工具——现有 App UI 功能面（聊天/搜索/会话）已全覆盖。

## 构建

```bash
# Android（双 ABI）— 需 cargo-ndk + ANDROID_NDK_HOME
ANDROID_NDK_HOME=E:/Android/Sdk/ndk/27.1.12297006 bash scripts/build-android.sh
# 产物 dist/android/<abi>/libzclaw.so，脚本自带 needle 校验
# （0.6.0-ulnclaw-mobile / cn.bing.com / 联网搜索只是 / 无旧版串 0.5.2-mobile）

# OHOS — .cmd 包装 DevEco clang（见 scripts/build-harmony-win.sh）
# 单元测试
cd mobile-ffi && cargo test    # 6 tests：chunk 契约/no_progress/prompt 锁/截断/错误分类
```

体积：release `opt-level="z"` + fat LTO + strip（LTO 自动裁掉未引用的 gateway/
26 平台/browser 死代码——已实证整库 cdylib 链接成功且仅 4.3MB）。

## 版本

- `zclaw_version()` → `0.6.0-ulnclaw-mobile`（App 引擎行可见，区分旧 zclaw 0.5.x）
- ulnclaw 上游基线：v0.6.3（2026-09-12, commit 9247670）
