// ios/Sources/ZClawBridge.mm — C bridge to the Rust zclaw FFI (static link).
// Links against libzclaw.a / ZClawKit.xcframework built from
// gitee.com/ushaw/ulnclaw (cargo lipo / cargo-xcframework for iOS targets).

#import "ZClawBridge.h"
#include "zclaw.h"  // include/zclaw.h — canonical FFI contract
#include <string>

@implementation ZClawBridge

+ (BOOL)isAvailable {
    // Static linking: symbols resolved at load time. Probe via version().
    return zclaw_version() != NULL;
}

+ (NSString *)version {
    const char* v = zclaw_version();
    NSString* out = v ? [NSString stringWithUTF8String:v] : @"unknown";
    if (v) zclaw_free(v);
    return out;
}

+ (BOOL)initWithApiUrl:(NSString *)apiUrl
                apiKey:(NSString *)apiKey
                 model:(NSString *)model
          workspaceDir:(NSString *)workspaceDir {
    std::string config =
        std::string("{\"api_url\":\"") + apiUrl.UTF8String +
        "\",\"api_key\":\"" + apiKey.UTF8String +
        "\",\"default_model\":\"" + model.UTF8String +
        "\",\"temperature\":0.7,\"workspace_dir\":\"" + workspaceDir.UTF8String +
        "\",\"security\":{\"autonomy\":\"full\"},"
        "\"memory\":{\"backend\":\"sqlite\"},"
        "\"agent\":{\"max_iterations\":10,"
        "\"system_prompt\":\"You are ZClaw, a helpful pocket AI assistant.\"}}";
    return zclaw_init(config.c_str()) == 0;
}

+ (int)chat:(NSString *)message {
    return zclaw_chat(message.UTF8String);
}

+ (NSString *)pollChunks {
    const char* json = zclaw_poll_chunks();
    NSString* out = json ? [NSString stringWithUTF8String:json] : @"[]";
    if (json) zclaw_free(json);
    return out;
}

+ (BOOL)isRunning {
    return zclaw_is_running() == 1;
}

+ (BOOL)cancel {
    return zclaw_cancel() == 0;
}

+ (NSString *)getSessions {
    const char* json = zclaw_get_sessions();
    NSString* out = json ? [NSString stringWithUTF8String:json] : @"[]";
    if (json) zclaw_free(json);
    return out;
}

+ (NSString *)getMessages:(NSString *)sessionId {
    const char* json = zclaw_get_messages(sessionId.UTF8String);
    NSString* out = json ? [NSString stringWithUTF8String:json] : @"[]";
    if (json) zclaw_free(json);
    return out;
}

@end
