// cpp/zclaw_napi.cpp — NAPI wrapper for libzclaw.so on HarmonyOS
// Bridges the Rust .so to ArkTS via Node-API (NAPI)
//
// FFI functions (v0.2 — streaming via polling):
// - zclaw_init(config_json) -> i32  (0=success, -1=error)
// - zclaw_chat(message) -> i32  (0=accepted, -1=error, -2=busy)
// - zclaw_poll_chunks() -> *const c_char  (JSON array of chunks)
// - zclaw_is_running() -> i32  (1=running, 0=idle)
// - zclaw_cancel() -> i32  (0=success, -1=no chat running)
// - zclaw_get_sessions() -> *const c_char
// - zclaw_get_messages(session_id) -> *const c_char
// - zclaw_free(ptr)
// - zclaw_version() -> *const c_char

#include "napi/native_api.h"
#include <cstring>
#include <string>
#include <dlfcn.h>

// ── FFI function types (matching src/ffi.rs v0.2) ──
typedef int (*zclaw_init_fn)(const char* config_json);
typedef int (*zclaw_chat_fn)(const char* message);  // now returns int, not string
typedef const char* (*zclaw_poll_chunks_fn)();       // returns JSON array string
typedef int (*zclaw_is_running_fn)();                // returns 1/0
typedef int (*zclaw_cancel_fn)();                   // returns 0/-1
typedef const char* (*zclaw_get_sessions_fn)();
typedef const char* (*zclaw_get_messages_fn)(const char* session_id);
typedef void (*zclaw_free_fn)(const char* ptr);
typedef const char* (*zclaw_version_fn)();

// ── Loaded function pointers ──
static zclaw_init_fn          g_init       = nullptr;
static zclaw_chat_fn          g_chat       = nullptr;
static zclaw_poll_chunks_fn   g_poll       = nullptr;
static zclaw_is_running_fn    g_running    = nullptr;
static zclaw_cancel_fn        g_cancel     = nullptr;
static zclaw_get_sessions_fn  g_sessions   = nullptr;
static zclaw_get_messages_fn g_messages   = nullptr;
static zclaw_free_fn          g_free       = nullptr;
static zclaw_version_fn       g_version    = nullptr;

static bool g_loaded = false;

// ── Load libzclaw.so ──
static bool load_zclaw() {
    if (g_loaded) return true;
    void* handle = dlopen("libzclaw.so", RTLD_NOW);
    if (!handle) {
        return false;
    }
    g_init     = (zclaw_init_fn)dlsym(handle, "zclaw_init");
    g_chat     = (zclaw_chat_fn)dlsym(handle, "zclaw_chat");
    g_poll     = (zclaw_poll_chunks_fn)dlsym(handle, "zclaw_poll_chunks");
    g_running  = (zclaw_is_running_fn)dlsym(handle, "zclaw_is_running");
    g_cancel   = (zclaw_cancel_fn)dlsym(handle, "zclaw_cancel");
    g_sessions = (zclaw_get_sessions_fn)dlsym(handle, "zclaw_get_sessions");
    g_messages = (zclaw_get_messages_fn)dlsym(handle, "zclaw_get_messages");
    g_free     = (zclaw_free_fn)dlsym(handle, "zclaw_free");
    g_version  = (zclaw_version_fn)dlsym(handle, "zclaw_version");
    g_loaded = (g_init && g_chat && g_poll && g_running && g_cancel && g_free && g_version);
    return g_loaded;
}

// ── NAPI: isAvailable() ──
static napi_value IsAvailable(napi_env env, napi_callback_info info) {
    napi_value result;
    napi_get_boolean(env, load_zclaw() && g_loaded, &result);
    return result;
}

// ── NAPI: version() ──
static napi_value Version(napi_env env, napi_callback_info info) {
    napi_value result;
    if (!load_zclaw() || !g_version) {
        napi_create_string_utf8(env, "unavailable", NAPI_AUTO_LENGTH, &result);
        return result;
    }
    const char* ver = g_version();
    napi_create_string_utf8(env, ver ? ver : "unknown", NAPI_AUTO_LENGTH, &result);
    if (g_free && ver) g_free(ver);
    return result;
}

// ── NAPI: init(apiUrl, apiKey, model, workspaceDir) -> boolean ──
static napi_value Init(napi_env env, napi_callback_info info) {
    size_t argc = 4;
    napi_value args[4];
    napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);

    if (argc < 4 || !load_zclaw() || !g_init) {
        napi_value result;
        napi_get_boolean(env, false, &result);
        return result;
    }

    char apiUrl[256], apiKey[512], model[128], workspaceDir[512];
    size_t len;
    napi_get_value_string_utf8(env, args[0], apiUrl, sizeof(apiUrl), &len);
    napi_get_value_string_utf8(env, args[1], apiKey, sizeof(apiKey), &len);
    napi_get_value_string_utf8(env, args[2], model, sizeof(model), &len);
    napi_get_value_string_utf8(env, args[3], workspaceDir, sizeof(workspaceDir), &len);

    // Build JSON config
    char config[2048];
    snprintf(config, sizeof(config),
        "{\"api_url\":\"%s\",\"api_key\":\"%s\",\"default_model\":\"%s\","
        "\"temperature\":0.7,\"workspace_dir\":\"%s\","
        "\"security\":{\"autonomy\":\"full\"},"
        "\"memory\":{\"backend\":\"sqlite\"},"
        "\"agent\":{\"max_iterations\":10,"
        "\"system_prompt\":\"You are ZClaw, a helpful pocket AI assistant.\"}}",
        apiUrl, apiKey, model, workspaceDir);

    int rc = g_init(config);

    napi_value result;
    napi_get_boolean(env, rc == 0, &result);
    return result;
}

// ── NAPI: chat(message) -> int (0=ok, -1=error, -2=busy) ──
static napi_value Chat(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);

    napi_value result;
    if (argc < 1 || !load_zclaw() || !g_chat) {
        napi_create_int32(env, -1, &result);
        return result;
    }

    char message[8192];
    size_t len;
    napi_get_value_string_utf8(env, args[0], message, sizeof(message), &len);

    int rc = g_chat(message);
    napi_create_int32(env, rc, &result);
    return result;
}

// ── NAPI: pollChunks() -> JSON string ──
static napi_value PollChunks(napi_env env, napi_callback_info info) {
    napi_value result;
    if (!load_zclaw() || !g_poll) {
        napi_create_string_utf8(env, "[]", 2, &result);
        return result;
    }
    const char* json = g_poll();
    napi_create_string_utf8(env, json ? json : "[]", NAPI_AUTO_LENGTH, &result);
    if (g_free && json) g_free(json);
    return result;
}

// ── NAPI: isRunning() -> boolean ──
static napi_value IsRunning(napi_env env, napi_callback_info info) {
    napi_value result;
    bool running = false;
    if (load_zclaw() && g_running) {
        running = g_running() == 1;
    }
    napi_get_boolean(env, running, &result);
    return result;
}

// ── NAPI: cancel() -> boolean ──
static napi_value Cancel(napi_env env, napi_callback_info info) {
    napi_value result;
    bool ok = false;
    if (load_zclaw() && g_cancel) {
        ok = g_cancel() == 0;
    }
    napi_get_boolean(env, ok, &result);
    return result;
}

// ── NAPI: getSessions() -> JSON string ──
static napi_value GetSessions(napi_env env, napi_callback_info info) {
    napi_value result;
    if (!load_zclaw() || !g_sessions) {
        napi_create_string_utf8(env, "[]", 2, &result);
        return result;
    }
    const char* json = g_sessions();
    napi_create_string_utf8(env, json ? json : "[]", NAPI_AUTO_LENGTH, &result);
    if (g_free && json) g_free(json);
    return result;
}

// ── NAPI: getMessages(sessionId) -> JSON string ──
static napi_value GetMessages(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);

    napi_value result;
    if (argc < 1 || !load_zclaw() || !g_messages) {
        napi_create_string_utf8(env, "[]", 2, &result);
        return result;
    }

    char sessionId[256];
    size_t len;
    napi_get_value_string_utf8(env, args[0], sessionId, sizeof(sessionId), &len);

    const char* json = g_messages(sessionId);
    napi_create_string_utf8(env, json ? json : "[]", NAPI_AUTO_LENGTH, &result);
    if (g_free && json) g_free(json);
    return result;
}

// ── Module init ──
EXTERN_C_START
static napi_value InitNapi(napi_env env, napi_value exports) {
    napi_property_descriptor desc[] = {
        {"isAvailable", nullptr, IsAvailable, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"version",     nullptr, Version,     nullptr, nullptr, nullptr, napi_default, nullptr},
        {"init",        nullptr, Init,        nullptr, nullptr, nullptr, napi_default, nullptr},
        {"chat",        nullptr, Chat,        nullptr, nullptr, nullptr, napi_default, nullptr},
        {"pollChunks",  nullptr, PollChunks,  nullptr, nullptr, nullptr, napi_default, nullptr},
        {"isRunning",   nullptr, IsRunning,   nullptr, nullptr, nullptr, napi_default, nullptr},
        {"cancel",      nullptr, Cancel,      nullptr, nullptr, nullptr, napi_default, nullptr},
        {"getSessions", nullptr, GetSessions, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"getMessages", nullptr, GetMessages, nullptr, nullptr, nullptr, napi_default, nullptr},
    };
    napi_define_properties(env, exports, sizeof(desc) / sizeof(desc[0]), desc);
    return exports;
}
EXTERN_C_END

static napi_module zclawModule = {
    .nm_version = 1,
    .nm_flags = 1,
    .nm_filename = "zclaw_napi",
    .nm_register_func = InitNapi,
    .nm_modname = "zclaw_napi",
    .nm_priv = nullptr,
    .reserved = {0},
};

extern "C" __attribute__((constructor)) void RegisterZClawModule(void) {
    napi_module_register(&zclawModule);
}
