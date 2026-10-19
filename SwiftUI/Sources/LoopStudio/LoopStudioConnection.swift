import Foundation
import Security
import SwiftUI

// MARK: - LoopStudioConnectionStore: local + Tailscale remote gateway profiles
//
// Single source of truth for every gateway URL in Studio. Local default is
// `http://127.0.0.1:8000` (no token). Remote is a Tailscale MagicDNS name
// (e.g. `mac.tailabc123.ts.net`) with TLS and a Bearer token.
//
// Remote profiles are restricted to Tailscale addresses and require TLS via
// `tailscale serve --https`; this prevents a typo or public DNS endpoint from
// receiving prompts or the gateway token. Local loopback HTTP remains open.
//
// Host/port/TLS persist in UserDefaults; the token lives in Keychain
// (service `org.loopstudio.studio`, account `gateway-token`), never on disk.

@MainActor
final class LoopStudioConnectionStore: ObservableObject {
    static let shared = LoopStudioConnectionStore()

    @Published var host: String {
        didSet {
            let next = Self.normalized(host)
            if host != next { host = next }
            if Self.normalized(oldValue) != next {
                if !token.isEmpty { token = "" }
                if Self.isMagicDNSHost(next) {
                    useTLS = true
                } else if Self.isLoopback(next) {
                    useTLS = false
                }
            }
            UserDefaults.standard.set(host, forKey: Self.hostKey)
            touch()
        }
    }
    @Published var port: Int {
        didSet {
            // A port is 1...65535, and the settings field is a free numeric
            // TextField bound straight to this property. An out-of-range
            // value used to be persisted and applied, and then every gateway
            // call returned nil from `url(path:)` — the whole app reporting
            // "Invalid router URL" against a profile the user never edited.
            // Reject it here, where the mistake is visible.
            guard (1...65535).contains(port) else {
                // Swift does not re-enter `didSet` for an assignment made from
                // inside it, so this restores the last good port in one step.
                // The rejected value is captured first — after the restore,
                // `port` is the old one, and reporting that as "rejected"
                // would point the UI at the port that is already in use.
                let rejected = port
                port = oldValue
                lastRejectedPort = rejected
                return
            }
            if oldValue != port { token = "" }
            UserDefaults.standard.set(port, forKey: Self.portKey)
            lastRejectedPort = nil
            touch()
        }
    }
    /// The last value the settings field tried to set, kept so the UI can say
    /// why the field snapped back. Cleared by the next valid entry.
    @Published private(set) var lastRejectedPort: Int?
    @Published var useTLS: Bool {
        didSet {
            if oldValue != useTLS { token = "" }
            UserDefaults.standard.set(useTLS, forKey: Self.tlsKey)
            touch()
        }
    }
    /// In-memory token; persisted to Keychain on set.
    @Published var token: String {
        didSet { Self.saveToken(token) }
    }
    /// Bump to force URLSession clients to re-resolve (host switch).
    @Published private(set) var revision: UInt64 = 0

    // The pre-rename keys are still read, so a profile saved before the
    // rename survives it; the new key wins when both are present.
    private static let hostKey = "loop-studio.gateway.host"
    private static let portKey = "loop-studio.gateway.port"
    private static let tlsKey = "loop-studio.gateway.tls"
    private static let legacyHostKey = "lac.gateway.host"
    private static let legacyPortKey = "lac.gateway.port"
    private static let legacyTLSKey = "lac.gateway.tls"

    /// Read a value under the current key, falling back to the pre-rename one.
    private static func stored(_ key: String, legacy: String) -> Any? {
        let d = UserDefaults.standard
        if let value = d.object(forKey: key) { return value }
        return d.object(forKey: legacy)
    }

    init(
        host: String? = nil,
        port: Int? = nil,
        useTLS: Bool? = nil,
        token: String? = nil
    ) {
        let env = ProcessInfo.processInfo.environment
        let defaultHost = env["LOOP_STUDIO_ROUTER_HOST"]?.trimmingCharacters(in: .whitespacesAndNewlines)
        let defaultPort = env["LOOP_STUDIO_ROUTER_PORT"].flatMap { Int($0) }
        self.host = host
            ?? (defaultHost?.isEmpty == false ? defaultHost! : (Self.stored(Self.hostKey, legacy: Self.legacyHostKey) as? String))
            ?? "127.0.0.1"
        let storedPort = Self.stored(Self.portKey, legacy: Self.legacyPortKey) as? Int
        self.port = Self.resolvedPort(explicit: port, env: defaultPort, stored: storedPort)
        self.useTLS = useTLS ?? (Self.stored(Self.tlsKey, legacy: Self.legacyTLSKey) as? Bool) ?? false
        self.token = token ?? Self.loadToken() ?? env["LOOP_STUDIO_API_TOKEN"] ?? ""
        // Normalize empty host once (fresh installs, cleared defaults).
        if self.host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            self.host = "127.0.0.1"
        }
        if isRemote && useTLS == nil {
            self.useTLS = true
        }
    }

    /// The port precedence: explicit argument, then `LOOP_STUDIO_ROUTER_PORT`, then a
    /// persisted port that is actually a port, then 8000.
    ///
    /// This was `port ?? defaultPort ?? (storedPort ?? 0 > 0 ? storedPort! : 8000)`,
    /// and `??` binds looser than `>`, so the condition reduced to
    /// `storedPort ?? 0` — a nil check. Any *persisted* value was restored,
    /// including `0`, `-1` and `70000`, and `url(path:)` then returned nil for
    /// every gateway call, so the app showed "Invalid router URL" against a
    /// profile the user had never touched. The same field is written by the
    /// settings pane, so one bad keystroke there was enough.
    ///
    /// An explicit argument is passed through even when it is out of range:
    /// a caller that asks for port 0 gets a store that refuses every URL,
    /// which is the honest answer to a bad argument. Only *recovered* values
    /// (env, then what is on disk) are required to be real ports.
    nonisolated static func resolvedPort(explicit: Int?, env: Int?, stored: Int?) -> Int {
        if let explicit { return explicit }
        for candidate in [env, stored] {
            if let candidate, (1...65535).contains(candidate) {
                return candidate
            }
        }
        return 8000
    }

    private static func normalized(_ value: String) -> String {
        var h = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if h.count >= 2, h.hasPrefix("["), h.hasSuffix("]") {
            h.removeFirst()
            h.removeLast()
        }
        return h
    }

    private static func isLoopback(_ value: String) -> Bool {
        let h = value.lowercased()
        if h == "localhost" || h == "::1" { return true }
        let parts = h.split(separator: ".")
        if parts.count == 4, let first = Int(parts[0]), first == 127,
           parts.dropFirst().allSatisfy({ Int($0).map { (0...255).contains($0) } == true }) {
            return true
        }
        return false
    }

    private static func isMagicDNSHost(_ value: String) -> Bool {
        value.lowercased().hasSuffix(".ts.net")
    }

    var normalizedHost: String { Self.normalized(host) }

    var isRemote: Bool { !Self.isLoopback(normalizedHost) }

    var scheme: String { useTLS ? "https" : "http" }

    var displayName: String { "\(scheme)://\(normalizedHost):\(port)" }

    func url(path: String) -> URL? {
        let h = normalizedHost
        guard !h.isEmpty,
              (1...65535).contains(port),
              !h.contains(where: { $0.isWhitespace || "/?#@".contains($0) }),
              !isRemote || (useTLS && Self.isMagicDNSHost(h)) else {
            return nil
        }
        let p = path.hasPrefix("/") ? path : "/\(path)"
        var components = URLComponents()
        components.scheme = scheme
        components.host = h
        components.port = port
        if let queryStart = p.firstIndex(of: "?") {
            components.path = String(p[..<queryStart])
            components.query = String(p[p.index(after: queryStart)...])
        } else {
            components.path = p
        }
        return components.url
    }

    /// Attach `Authorization: Bearer` whenever a token is configured. A
    /// router with a token requires it on loopback too, which prevents local
    /// reverse proxies from bypassing the remote gate.
    func authorize(_ req: inout URLRequest) {
        let t = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty, !t.contains(where: { $0.isNewline }) else { return }
        req.setValue("Bearer \(t)", forHTTPHeaderField: "Authorization")
    }

    func clearToken() { token = "" }

    private func touch() { revision &+= 1 }

    // MARK: Keychain (token only)

    private static let service = "org.loopstudio.studio"
    private static let account = "gateway-token"

    static func loadToken() -> String? {
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data,
              let s = String(data: data, encoding: .utf8),
              !s.isEmpty
        else { return nil }
        return s
    }

    @discardableResult
    static func saveToken(_ token: String) -> Bool {
        let data = Data(token.utf8)
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        if token.isEmpty {
            SecItemDelete(q as CFDictionary)
            return true
        }
        let attrs: [String: Any] = [kSecValueData as String: data]
        let st = SecItemUpdate(q as CFDictionary, attrs as CFDictionary)
        if st == errSecSuccess { return true }
        if st == errSecItemNotFound {
            var add = q
            add[kSecValueData as String] = data
            return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
        }
        return false
    }
}
