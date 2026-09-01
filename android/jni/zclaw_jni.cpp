// jni/zclaw_jni.cpp — JNI bridge for libzclaw.so on Android
// Mirrors harmony/cpp/zclaw_napi.cpp: dlopen the prebuilt Rust library and
// expose the same nine operations to Kotlin.
//
// Pair with Kotlin class com.ulnit.omnimind.zclaw.ZClawNative (see
// ../android/kotlin). The Kotlin side loads this wrapper with
// System.loadLibrary("zclaw_jni").
//
// Build: NDK r25+, ABI arm64-v8a (armeabi-v7a optional).
// Prebuilt libzclaw.so must be built for Android NDK bionic libc
// (the OHOS build links libtime_service_ndk.so and will NOT load here).

#include <jni.h>
#include <string>
#include <cstring>
#include <dlfcn.h>

#include "zclaw.h"  // FFI contract (include/zclaw.h)

// ── Loaded function pointers (populated by load_zclaw) ──
typedef int         (*zclaw_init_fn)(const char*);
typedef int         (*zclaw_chat_fn)(const char*);
typedef const char* (*zclaw_poll_chunks_fn)();
typedef int         (*zclaw_is_running_fn)();
typedef int         (*zclaw_cancel_fn)();
typedef const char* (*zclaw_get_sessions_fn)();
typedef const char* (*zclaw_get_messages_fn)(const char*);
typedef void        (*zclaw_free_fn)(const char*);
typedef const char* (*zclaw_version_fn)();

static zclaw_init_fn           g_init     = nullptr;
static zclaw_chat_fn           g_chat     = nullptr;
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
    if (!handle) return false;
    g_init     = (zclaw_init_fn)         dlsym(handle, "zclaw_init");
    g_chat     = (zclaw_chat_fn)         dlsym(handle, "zclaw_chat");
    g_poll     = (zclaw_poll_chunks_fn)  dlsym(handle, "zclaw_poll_chunks");
    g_running  = (zclaw_is_running_fn)   dlsym(handle, "zclaw_is_running");
    g_cancel   = (zclaw_cancel_fn)       dlsym(handle, "zclaw_cancel");
    g_sessions = (zclaw_get_sessions_fn) dlsym(handle, "zclaw_get_sessions");
    g_messages = (zclaw_get_messages_fn) dlsym(handle, "zclaw_get_messages");
    g_free     = (zclaw_free_fn)         dlsym(handle, "zclaw_free");
    g_version  = (zclaw_version_fn)      dlsym(handle, "zclaw_version");
    g_loaded = g_init && g_chat && g_poll && g_running && g_cancel
             && g_sessions && g_messages && g_free && g_version;
    return g_loaded;
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
    jstring out = env->NewStringUTF(v ? v : "unknown");
    if (g_free && v) g_free(v);
    return out;
}

JNIEXPORT jboolean JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_init(JNIEnv* env, jclass,
        jstring apiUrl, jstring apiKey, jstring model, jstring workspaceDir) {
    if (!load_zclaw() || !g_init) return JNI_FALSE;

    const char* url = env->GetStringUTFChars(apiUrl, nullptr);
    const char* key = env->GetStringUTFChars(apiKey, nullptr);
    const char* mdl = env->GetStringUTFChars(model, nullptr);
    const char* dir = env->GetStringUTFChars(workspaceDir, nullptr);

    char config[2048];
    snprintf(config, sizeof(config),
        "{\"api_url\":\"%s\",\"api_key\":\"%s\",\"default_model\":\"%s\","
        "\"temperature\":0.7,\"workspace_dir\":\"%s\","
        "\"security\":{\"autonomy\":\"full\"},"
        "\"memory\":{\"backend\":\"sqlite\"},"
        "\"agent\":{\"max_iterations\":10,"
        "\"system_prompt\":\"You are ZClaw, a helpful pocket AI assistant.\"}}",
        url, key, mdl, dir);

    env->ReleaseStringUTFChars(apiUrl, url);
    env->ReleaseStringUTFChars(apiKey, key);
    env->ReleaseStringUTFChars(model, mdl);
    env->ReleaseStringUTFChars(workspaceDir, dir);

    return g_init(config) == 0 ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jint JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_chat(JNIEnv* env, jclass, jstring message) {
    if (!load_zclaw() || !g_chat) return -1;
    const char* msg = env->GetStringUTFChars(message, nullptr);
    int rc = g_chat(msg);
    env->ReleaseStringUTFChars(message, msg);
    return rc;
}

JNIEXPORT jstring JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_pollChunks(JNIEnv* env, jclass) {
    if (!load_zclaw() || !g_poll) return env->NewStringUTF("[]");
    const char* json = g_poll();
    jstring out = env->NewStringUTF(json ? json : "[]");
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
    jstring out = env->NewStringUTF(json ? json : "[]");
    if (g_free && json) g_free(json);
    return out;
}

JNIEXPORT jstring JNICALL
Java_com_ulnit_omnimind_zclaw_ZClawNative_getMessages(JNIEnv* env, jclass, jstring sessionId) {
    if (!load_zclaw() || !g_messages) return env->NewStringUTF("[]");
    const char* sid = env->GetStringUTFChars(sessionId, nullptr);
    const char* json = g_messages(sid);
    env->ReleaseStringUTFChars(sessionId, sid);
    jstring out = env->NewStringUTF(json ? json : "[]");
    if (g_free && json) g_free(json);
    return out;
}

} // extern "C"
