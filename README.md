# zclaw — Ulnvault 移动端本地 Agent（三端共用）

把鸿蒙版 Ulnvault 用到的**移动端可用的 ZClaw** 抽成独立项目，供
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

## 上游跟踪（zeroclaw）

zclaw 参考 [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)（GitLab 同步镜像
`gitlab.com/zeroclaw-labs/zeroclaw`）开源项目实现。

**当前版本 0.5.0**，已移植上游最新稳定版 **v0.8.4**（tag 2026-08-03）中适用于移动端嵌入式的改进：

| zeroclaw 编号 | 改进 | zclaw 落地 |
|---|---|---|
| #9824 | 工具输出封顶 + 搜索防限流 | 单结果 8KB / 搜索每结果 1.2KB / 总量 6KB；真实浏览器头 + 节流 + 限流时给模型准确提示 |
| #9105 | 进程控制超时 | shell 工具 60s 超时，防移动端聊天回合卡死 |
| #9490 | 按完整轮次裁剪历史 | 会话历史不留半轮对话 |
| #8899 | 记忆内容扫描 | 写入/召回边界拒绝凭据类内容、8KB 上限 |
| #8897 | 类型化记忆分类 | fact / preference / note（mem_type 列） |
| #8893 | 记忆审计 | memory_audit 表记录 store/recall/forget |
| **#8890** | **搜索失败分类** | **`search_status=blocked/unavailable/client_error` 标签 + 中文决策提示，全源失败时逐源列出**（0.5.0 新增） |
| **#8838** | **SSE 完成/空闲超时加固** | **已等效实现**：流式客户端不设总 `.timeout()`，只用 `connect_timeout(30s)` + `read_timeout(300s)`；我们的 300s 比上游 90s 更宽松（见 f125140） |

桌面/通道/发布类改进（A2A、SOP 引擎、Channels、Dashboard、gateway、robot-kit、k8s 等）不适用于移动端单进程库，故不移植。

### 上游同步纪律（2026-09-12 复核）

上游 v0.8.4 已演进为 **20+ crate 的 workspace**（edition 2024、rust-version 1.96.1），
且**不含任何移动端支持**：`crates/` 下无 ffi / android / ios / ohos / jni / mobile 模块。
zclaw 是 fork 自 v0.4.0 时代的**单 crate**（`rust/zclaw/src` 仅 9 个文件，91KB），
带三端 FFI 桥接。**直接 checkout 上游 master 会摧毁整个移动端集成层**，禁止这么做。

正确做法：只 backport 具体提交（按上表逐项对照），并保持本仓单 crate 结构。
复核时上游 master 领先 v0.8.4 的 3 个相关提交已逐一判定：
#8838（已等效）、#2a00bee7 loop_detector 性能优化（本仓无 loop_detector 模块，不适用）、
a6a050a7 OAuth refresh 重构（本仓无 OAuth provider，不适用）。

### zclaw 自有修复（上游无对应，因上游不支持移动端/未覆盖）

| 提交 | 问题 | 修复 |
|---|---|---|
| d9201d4 | OpenRouter 渠道发 `reasoning`/`reasoning_details` 而非 `reasoning_content`，推理模型思考期客户端收不到任何数据（实测最长 381s） | 三字段全读兼容 + 三端 UI 展示思考流 |
| 22bc258 | Android JNI 用 `NewStringUTF`（Modified UTF-8），emoji 导致对话无响应 | 改 `NewString` + `GetStringChars` |
| f125140 | 长对话被 120s 总时长超时掐断（`error decoding response body`） | 见上表 #8838 |
| **0.5.0** | **🔴 6 处 UTF-8 字节切片 panic**（`&s[..8000]` / `s.truncate(8000)`）落在中文字符中间。因 `panic="abort"` 且 FFI 层无 `catch_unwind`，**panic 直接杀死整个 App 进程**；抓任意中文网页必现 | 统一 `truncate_utf8()` helper（`is_char_boundary` 回退）；上游 `web_fetch.rs` 用 `.chars().take(N)` 本就安全，是我们 fork 时的回退缺陷 |
| **0.5.0** | **🔴 `web_search` 在中国大陆 100% 不可用**：只有 Brave（需 key）+ DDG 两条路，实测两者均 HTTP=000 超时，且三端都没设 `BRAVE_API_KEY` → 每次等满 20s 才返回失败 | 新增 **Bing 源**（`cn.bing.com` 免 key、实测 HTTP=200）并列为首选，DDG 降为境外兜底 |
| **0.5.0** | Bing 结果里的真实链接在 `<a class="tilk">` 上而非 `<h2>`；`<cite>` 只是显示格式（`域名 › 路径`），当 URL 用会让后续 `web_fetch` 全失败 | 取 `tilk` 真 href，`h2` 内嵌 a 次之，`cite` 仅兜底 |

上游的 `bocha`（博查，Chinese-friendly）搜索 provider 未移植：它需要付费 API key，
而 Bing 免 key 更适合移动端（无需配置、无需在 App 内分发密钥）。若后续需要更稳的国内搜索，
可考虑按上游方式接 Bocha，并把 key 放在后端而非客户端。
