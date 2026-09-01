// ios/Sources/ZClawBridge.h — C bridge exposing libzclaw to Swift.
//
// iOS does not allow dlopen of dynamic libraries outside the app bundle, so
// libzclaw is linked STATICALLY (libzclaw.a or an XCFramework). The bridge
// just forwards to the zclaw_* symbols declared in include/zclaw.h.

#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

@interface ZClawBridge : NSObject

/// Always YES on iOS when libzclaw.a is linked (symbols resolve at link time).
+ (BOOL)isAvailable;

/// Library version string.
+ (NSString *)version;

/// Initialize the agent. Config mirrors the Rust zclaw_init() contract:
/// api_url, api_key, default_model, temperature, workspace_dir,
/// security.autonomy, memory.backend, agent.max_iterations, agent.system_prompt.
+ (BOOL)initWithApiUrl:(NSString *)apiUrl
                apiKey:(NSString *)apiKey
                 model:(NSString *)model
          workspaceDir:(NSString *)workspaceDir;

/// Submit a user message. Returns 0=accepted, -1=error, -2=busy.
+ (int)chat:(NSString *)message;

/// Drain pending chunks as a JSON array string.
/// chunkType: 0=text 1=tool_call 2=tool_result 3=done 4=error 5=thinking
+ (NSString *)pollChunks;

/// YES while the agent is running.
+ (BOOL)isRunning;

/// Cancel the running chat.
+ (BOOL)cancel;

/// Session store accessors (JSON strings).
+ (NSString *)getSessions;
+ (NSString *)getMessages:(NSString *)sessionId;

@end

NS_ASSUME_NONNULL_END
