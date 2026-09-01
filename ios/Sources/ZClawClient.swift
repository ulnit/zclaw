import Foundation

/// ZClaw Swift client — wraps the C bridge (ZClawBridge) backed by the Rust
/// libzclaw static library. Mirrors HarmonyOS ZClawApi.ets semantics:
/// init once per instance, chat() then poll chunks on a timer until done.
public final class ZClawClient {
    public static let shared = ZClawClient()

    public enum ChunkType: Int {
        case text = 0, toolCall = 1, toolResult = 2, done = 3, error = 4, thinking = 5
    }

    public struct Chunk {
        public let chunkType: Int
        public let name: String
        public let args: String
        public let result: String
    }

    public var isAvailable: Bool { ZClawBridge.isAvailable() }
    public var version: String { ZClawBridge.version() }

    @discardableResult
    public func initialize(apiUrl: String, apiKey: String,
                           model: String, workspaceDir: String) -> Bool {
        ZClawBridge.init(withApiUrl: apiUrl, apiKey: apiKey,
                         model: model, workspaceDir: workspaceDir)
    }

    /// 0 = accepted, -1 = error, -2 = busy
    @discardableResult
    public func chat(_ message: String) -> Int {
        ZClawBridge.chat(message)
    }

    public var isRunning: Bool { ZClawBridge.isRunning() }

    @discardableResult
    public func cancel() -> Bool { ZClawBridge.cancel() }

    /// Drain one batch of chunks.
    public func pollChunks() -> [Chunk] {
        let json = ZClawBridge.pollChunks()
        guard let data = json.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return [] }
        return arr.map {
            Chunk(
                chunkType: ($0["chunkType"] as? Int) ?? 0,
                name: ($0["name"] as? String) ?? "",
                args: ($0["args"] as? String) ?? "",
                result: ($0["result"] as? String) ?? ""
            )
        }
    }

    public func sessions() -> [[String: Any]] {
        parseArray(ZClawBridge.getSessions())
    }

    public func messages(sessionId: String) -> [[String: Any]] {
        parseArray(ZClawBridge.getMessages(sessionId))
    }

    private func parseArray(_ json: String) -> [[String: Any]] {
        guard let data = json.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return [] }
        return arr
    }
}
