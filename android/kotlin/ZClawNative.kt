package com.ulnit.omnimind.zclaw

/**
 * ZClaw native binding (Android) — loads libzclaw_jni.so, which dlopen()s the
 * prebuilt Rust libzclaw.so at runtime. Contract mirrors the HarmonyOS NAPI
 * wrapper (zclaw_napi.cpp) 1:1.
 *
 * Prebuilt libzclaw.so MUST be the Android NDK build (bionic libc). The .so
 * shipped in the harmony tree is OHOS-only (links libtime_service_ndk.so).
 * See scripts/build-android.sh in the zclaw repo.
 */
object ZClawNative {
    var loaded = false
        private set

    init {
        try {
            System.loadLibrary("zclaw_jni")
            loaded = true
        } catch (e: UnsatisfiedLinkError) {
            loaded = false
        }
    }

    external fun isAvailable(): Boolean
    external fun version(): String
    external fun init(apiUrl: String, apiKey: String, model: String, workspaceDir: String): Boolean
    external fun chat(message: String): Int   // 0=ok, -1=error, -2=busy
    external fun pollChunks(): String          // JSON array of chunks
    external fun isRunning(): Boolean
    external fun cancel(): Boolean
    external fun getSessions(): String         // JSON
    external fun getMessages(sessionId: String): String // JSON
}

/** chunkType values from the Rust FFI */
object ZClawChunkType {
    const val TEXT = 0
    const val TOOL_CALL = 1
    const val TOOL_RESULT = 2
    const val DONE = 3
    const val ERROR = 4
    const val THINKING = 5
}
