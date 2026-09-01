// zclaw.h — ZClaw mobile FFI contract (v0.2)
// The canonical C ABI exported by libzclaw (Rust, from ulnclaw).
// All three mobile bridges (HarmonyOS NAPI / Android JNI / iOS C) bind
// against these exact symbols via dlsym.
//
// Source of truth: https://gitee.com/ushaw/ulnclaw (Rust, src/ffi.rs)
// Streaming model (v0.2): submit message, then poll chunks.

#ifndef ZCLAW_FFI_H
#define ZCLAW_FFI_H

#ifdef __cplusplus
extern "C" {
#endif

// ── Lifecycle / control ──────────────────────────────────────────────
// config_json keys:
//   api_url, api_key, default_model, temperature, workspace_dir,
//   security.autonomy ("full"), memory.backend ("sqlite"),
//   agent.max_iterations, agent.system_prompt
// Returns 0 on success, -1 on error.
int zclaw_init(const char* config_json);

// Submit one user message; the agent runs asynchronously.
// Returns 0 = accepted, -1 = error, -2 = busy (a chat is already running).
int zclaw_chat(const char* message);

// Drain pending output chunks as a JSON array string.
// Each element: { "chunkType": int, "name": str, "args": str, "result": str }
//   chunkType: 0=text 1=tool_call 2=tool_result 3=done 4=error 5=thinking
// Returns a heap-allocated string the caller must release with zclaw_free().
const char* zclaw_poll_chunks(void);

// 1 if the agent is still running, 0 if idle.
int zclaw_is_running(void);

// Cancel the running chat. Returns 0 on success, -1 if nothing is running.
int zclaw_cancel(void);

// ── Session store (SQLite-backed inside the Rust lib) ────────────────
// Both return heap-allocated JSON strings; free with zclaw_free().
const char* zclaw_get_sessions(void);
const char* zclaw_get_messages(const char* session_id);

// ── Memory management ────────────────────────────────────────────────
// Free any string returned by this library.
void zclaw_free(const char* ptr);

// Library version string (free with zclaw_free()).
const char* zclaw_version(void);

#ifdef __cplusplus
}
#endif

#endif // ZCLAW_FFI_H
