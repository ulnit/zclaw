// jni/zclaw_jni.cpp — JNI bridge for libzclaw.so on Android
// Mirrors harmony/cpp/zclaw_napi.cpp: dlopen the prebuilt Rust library and
// expose the same operations to Kotlin.
//
// Pair with Kotlin class com.ulnit.omnimind.zclaw.ZClawNative.
// The Kotlin side loads this wrapper with System.loadLibrary("zclaw_jni").
//
// Build: NDK r25+, ABI arm64-v8a (armeabi-v7a optional).
// Prebuilt libzclaw.so must be built for Android NDK bionic libc
// (the OHOS build links libtime_service_ndk.so and will NOT load here).
//
// ══════════════════════════════════════════════════════════════════════════
//  CRITICAL: do NOT use NewStringUTF / GetStringUTFChars for Rust text.
//
//  JNI's *StringUTF APIs use "Modified UTF-8" (CESU-8), where supplementary
//  characters (U+10000..U+10FFFF — i.e. every emoji) MUST be encoded as a
//  6-byte surrogate pair.  Rust's serde_json emits *standard* UTF-8, where
//  those characters are 4-byte sequences (e.g. U+1F60A = F0 9F 98 8A).
//
//  Feeding standard UTF-8 to NewStringUTF is invalid input: ART aborts the
//  call with "JNI ERROR: input is not valid Modified UTF-8" (debug builds) or
//  returns a truncated/garbled string (release builds).  Since zclaw_poll_chunks
//  returns the whole chunk batch as one JSON string, a single emoji anywhere in
//  the model's answer destroyed the entire batch — Kotlin's
//  Json.parseToJsonElement then failed, runCatching swallowed it, every chunk
//  was dropped, and the UI showed an empty bubble ("no response").
//
//  The reverse direction was broken too: GetStringUTFChars hands Rust a 6-byte
//  Modified-UTF-8 sequence for each emoji, and Rust's to_string_lossy() turns
//  that into U+FFFD garbage, so emoji sent by the user were corrupted before
//  reaching the model.
//
//  Fix: convert explicitly between standard UTF-8 and UTF-16 and use
//  NewString / GetStringChars.  HarmonyOS (napi_create_string_utf8) and iOS
//  (String(cString:)) both consume standard UTF-8 and were never affected.
// ══════════════════════════════════════════════════════════════════════════

#include <jni.h>
#include <android/log.h>
#include <string>
#include <vector>
#include <cstring>
#include <cstdint>
#include <dlfcn.h>

#include "zclaw.h"  // FFI contract (include/zclaw.h)

#define LOG_TAG "ZClawJNI"
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

// ── Loaded function pointers (populated by load_zclaw) ──
typedef int         (*zclaw_init_fn)(const char*);
typedef int         (*zclaw_chat_fn)(const char*);
typedef int         (*zclaw_set_session_fn)(const char*);
typedef const char* (*zclaw_poll_chunks_fn)();
typedef int         (*zclaw_is_running_fn)();
typedef int         (*zclaw_cancel_fn)();
typedef const char* (*zclaw_get_sessions_fn)();
typedef const char* (*zclaw_get_messages_fn)(const char*);
typedef void        (*zclaw_free_fn)(const char*);
typedef const char* (*zclaw_version_fn)();

static zclaw_init_fn           g_init     = nullptr;
static zclaw_chat_fn           g_chat     = nullptr;
static zclaw_set_session_fn    g_set_session = nullptr;
static zclaw_poll_chunks_fn    g_poll     = nullptr;
static zclaw_is_running_fn     g_running  = nullptr;
static zclaw_cancel_fn         g_cancel   = nullptr;
static zclaw_get_sessions_fn   g_sessions = nullptr;
static zclaw_get_messages_fn   g_messages = nullptr;
static zclaw_free_fn           g_free     = nullptr;
static zclaw_version_fn        g_version  = nullptr;
static bool                    g_loaded   = false;

static bool load_zclaw() {
    if (g_loaded) return true;
    // libzclaw.so is packaged in jniLibs/<abi>/ and loaded by name.
    void* handle = dlopen("libzclaw.so", RTLD_NOW);
    if (!handle) {
        LOGE("dlopen(libzclaw.so) failed: %s", dlerror());
        return false;
    }
    g_init     = (zclaw_init_fn)dlsym(handle, "zclaw_init");
    g_chat     = (zclaw_chat_fn)dlsym(handle, "zclaw_chat");
    g_set_session = (zclaw_set_session_fn)dlsym(handle, "zclaw_set_session"); // optional (v0.2+)
    g_poll     = (zclaw_poll_chunks_fn)dlsym(handle, "zclaw_poll_chunks");
    g_running  = (zclaw_is_running_fn)   dlsym(handle, "zclaw_is_running");
    g_cancel   = (zclaw_cancel_fn)       dlsym(handle, "zclaw_cancel");
    g_sessions = (zclaw_get_sessions_fn) dlsym(handle, "zclaw_get_sessions");
    g_messages = (zclaw_get_messages_fn) dlsym(handle, "zclaw_get_messages");
    g_free     = (zclaw_free_fn)         dlsym(handle, "zclaw_free");
    g_version  = (zclaw_version_fn)      dlsym(handle, "zclaw_version");
    g_loaded = g_init && g_chat && g_poll && g_running && g_cancel
             && g_sessions && g_messages && g_free && g_version;
    if (!g_loaded) LOGE("dlsym incomplete: missing required zclaw_* symbols");
    return g_loaded;
}

// ══════════════════════════════════════════════════════════════════════
//  Standard UTF-8 <-> UTF-16 conversion (emoji-safe)
// ══════════════════════════════════════════════════════════════════════

/// Decode standard UTF-8 into UTF-16 code units, emitting a surrogate pair for
/// supplementary characters.  Malformed bytes become U+FFFD instead of
/// truncating the rest of the string.
///
/// Uses vector<jchar> (not std::u16string) so the result feeds NewString
/// directly with no char_traits<char16_t> portability questions.
static std::vector<jchar> utf8_to_utf16(const char* s, size_t len) {
    std::vector<jchar> out;
    out.reserve(len);
    size_t i = 0;
    while (i < len) {
        uint8_t c = (uint8_t)s[i];
        uint32_t cp;
        size_t extra;
        if (c < 0x80)              { cp = c;          extra = 0; }
        else if ((c & 0xE0) == 0xC0) { cp = c & 0x1Fu; extra = 1; }
        else if ((c & 0xF0) == 0xE0) { cp = c & 0x0Fu; extra = 2; }
        else if ((c & 0xF8) == 0xF0) { cp = c & 0x07u; extra = 3; }
        else { out.push_back(0xFFFD); ++i; continue; }   // stray continuation byte

        if (extra > 0 && i + extra >= len) { out.push_back(0xFFFD); break; }  // truncated tail

        bool ok = true;
        for (size_t k = 1; k <= extra; ++k) {
            uint8_t cc = (uint8_t)s[i + k];
            if ((cc & 0xC0) != 0x80) { ok = false; break; }
            cp = (cp << 6) | (uint32_t)(cc & 0x3Fu);
        }
        if (!ok) { out.push_back(0xFFFD); ++i; continue; }

        // reject overlong encodings and surrogates smuggled in as UTF-8
        if ((extra == 1 && cp < 0x80) || (extra == 2 && cp < 0x800) ||
            (extra == 3 && cp < 0x10000) || (cp >= 0xD800 && cp <= 0xDFFF)) {
            out.push_back(0xFFFD);
        } else if (cp <= 0xFFFF) {
            out.push_back((jchar)cp);
        } else if (cp <= 0x10FFFF) {
            uint32_t v = cp - 0x10000;
            out.push_back((jchar)(0xD800 + (v >> 10)));
            out.push_back((jchar)(0xDC00 + (v & 0x3FF)));
        } else {
            out.push_back(0xFFFD);
        }
        i += extra + 1;
    }
    return out;
}

/// Rust C string (standard UTF-8) -> jstring.  Safe for emoji / any Unicode.
static jstring rust_to_jstring(JNIEnv* env, const char* s) {
    if (!s) return env->NewStringUTF("[]");            // ASCII literal: safe
    size_t len = strlen(s);
    std::vector<jchar> u = utf8_to_utf16(s, len);
    return env->NewString(u.empty() ? nullptr : u.data(), (jsize)u.size());
}

/// Encode one code point as standard UTF-8.
static void append_utf8(std::string& out, uint32_t cp) {
    if (cp < 0x80) {
        out.push_back((char)cp);
    } else if (cp < 0x800) {
        out.push_back((char)(0xC0 | (cp >> 6)));
        out.push_back((char)(0x80 | (cp & 0x3F)));
    } else if (cp < 0x10000) {
        out.push_back((char)(0xE0 | (cp >> 12)));
        out.push_back((char)(0x80 | ((cp >> 6) & 0x3F)));
        out.push_back((char)(0x80 | (cp & 0x3F)));
    } else {
        out.push_back((char)(0xF0 | (cp >> 18)));
        out.push_back((char)(0x80 | ((cp >> 12) & 0x3F)));
        out.push_back((char)(0x80 | ((cp >> 6) & 0x3F)));
        out.push_back((char)(0x80 | (cp & 0x3F)));
    }
}

/// jstring -> std::string in standard UTF-8 (surrogate pairs collapse to 4-byte
/// sequences, which is what Rust expects).
static std::string jstring_to_utf8(JNIEnv* env, jstring js) {
    std::string out;
    if (!js) return out;
    jsize n = env->GetStringLength(js);
    if (n <= 0) return out;
    const jchar* chars = env->GetStringChars(js, nullptr);
    if (!chars) return out;
    out.reserve((size_t)n);
    for (jsize i = 0; i < n; ) {
        uint32_t cp = (uint32_t)chars[i];
        if (cp >= 0xD800 && cp <= 0xDBFF && i + 1 < n) {
            uint32_t lo = (uint32_t)chars[i + 1];
            if (lo >= 0xDC00 && lo <= 0xDFFF) {
                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                i += 2;
            } else {
                cp = 0xFFFD; ++i;                        // lone high surrogate
            }
        } else if (cp >= 0xD800 && cp <= 0xDFFF) {
            cp = 0xFFFD; ++i;                            // lone surrogate
        } else {
            ++i;
        }
        append_utf8(out, cp);
    }
    env->ReleaseStringChars(js, chars);
    return out;
}

/// Escape a string for embedding in a JSON string literal.
static std::string json_escape(const std::string& in) {
    std::string out;
    out.reserve(in.size() + 8);
    for (char ch : in) {
        uint8_t c = (uint8_t)ch;
        switch (c) {
            case '"':  out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\b': out += "\\b";  break;
            case '\f': out += "\\f";  break;
            case '\n': out += "\\n";  break;
            case '\r': out += "\\r";  break;
            case '\t': out += "\\t";  break;
            default:
                if (c < 0x20) {
                    char buf[8];
                    snprintf(buf, sizeof(buf), "\\u%04x", c);
                    out += buf;
                } else {
                    out.push_back(ch);                   // UTF-8 bytes pass through
                }
        }
    }
    return out;
}

// ── JNI exports ──
extern "C" {

JNIEXPORT jboolean JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_isAvailable(JNIEnv*, jclass) {
    return load_zclaw() ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jstring JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_version(JNIEnv* env, jclass) {
    if (!load_zclaw() || !g_version)
        return env->NewStringUTF("unavailable");
    const char* v = g_version();
    jstring out = rust_to_jstring(env, v ? v : "unknown");
    if (g_free && v) g_free(v);
    return out;
}

JNIEXPORT jboolean JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_init(JNIEnv* env, jclass,
        jstring apiUrl, jstring apiKey, jstring model, jstring workspaceDir) {
    if (!load_zclaw() || !g_init) return JNI_FALSE;

    // Build the config as proper JSON (no fixed 2048 buffer — a long key or
    // workspace path used to truncate it into invalid JSON, failing init).
    std::string url = jstring_to_utf8(env, apiUrl);
    std::string key = jstring_to_utf8(env, apiKey);
    std::string mdl = jstring_to_utf8(env, model);
    std::string dir = jstring_to_utf8(env, workspaceDir);

    std::string config;
    config.reserve(512 + key.size() + dir.size());
    config += "{\"api_url\":\"";   config += json_escape(url);
    config += "\",\"api_key\":\""; config += json_escape(key);
    config += "\",\"default_model\":\""; config += json_escape(mdl);
    config += "\",\"temperature\":0.7,\"workspace_dir\":\""; config += json_escape(dir);
    config += "\",\"security\":{\"autonomy\":\"full\"}"
              ",\"memory\":{\"backend\":\"sqlite\"}"
              ",\"agent\":{\"max_iterations\":10,"
              "\"system_prompt\":\"You are ZClaw, a helpful pocket AI assistant.\"}}";

    int rc = g_init(config.c_str());
    if (rc != 0) {
        LOGE("zclaw_init failed rc=%d (api_url len=%zu model len=%zu ws len=%zu)",
             rc, url.size(), mdl.size(), dir.size());
    }
    return rc == 0 ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jint JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_chat(JNIEnv* env, jclass, jstring message) {
    if (!load_zclaw() || !g_chat) return -1;
    std::string msg = jstring_to_utf8(env, message);   // emoji-safe
    return (jint)g_chat(msg.c_str());
}

JNIEXPORT jint JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_setSession(JNIEnv* env, jclass, jstring sessionId) {
    if (!load_zclaw() || !g_set_session) return -1; // not in this build
    std::string sid = jstring_to_utf8(env, sessionId);
    return (jint)g_set_session(sid.c_str());
}

JNIEXPORT jstring JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_pollChunks(JNIEnv* env, jclass) {
    if (!load_zclaw() || !g_poll) return env->NewStringUTF("[]");
    const char* json = g_poll();
    jstring out = rust_to_jstring(env, json ? json : "[]");   // emoji-safe
    if (g_free && json) g_free(json);
    return out;
}

JNIEXPORT jboolean JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_isRunning(JNIEnv*, jclass) {
    if (!load_zclaw() || !g_running) return JNI_FALSE;
    return g_running() == 1 ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jboolean JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_cancel(JNIEnv*, jclass) {
    if (!load_zclaw() || !g_cancel) return JNI_FALSE;
    return g_cancel() == 0 ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jstring JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_getSessions(JNIEnv* env, jclass) {
    if (!load_zclaw() || !g_sessions) return env->NewStringUTF("[]");
    const char* json = g_sessions();
    jstring out = rust_to_jstring(env, json ? json : "[]");   // emoji-safe
    if (g_free && json) g_free(json);
    return out;
}

JNIEXPORT jstring JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_getMessages(JNIEnv* env, jclass, jstring sessionId) {
    if (!load_zclaw() || !g_messages) return env->NewStringUTF("[]");
    std::string sid = jstring_to_utf8(env, sessionId);
    const char* json = g_messages(sid.c_str());
    jstring out = rust_to_jstring(env, json ? json : "[]");   // emoji-safe
    if (g_free && json) g_free(json);
    return out;
}

} // extern "C"
