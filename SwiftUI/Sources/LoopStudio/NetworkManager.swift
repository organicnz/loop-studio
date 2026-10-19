import AppKit
import SwiftUI

// MARK: - Data Models (match `loop-studio-router /loop-studio/status` exactly)

struct BackendDetail: Codable {
    let port: Int
    let up: Bool
}

struct BackendStats: Codable {
    let port: Int?
    let ewma_ms: Double?
    let ok: UInt64
    let err: UInt64
    let est_tokens: UInt64?

    enum CodingKeys: String, CodingKey {
        case port, ewma_ms, ok, err, est_tokens
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        port = try c.decodeIfPresent(Int.self, forKey: .port)
        ewma_ms = try c.decodeIfPresent(Double.self, forKey: .ewma_ms)
        ok = (try? c.decodeIfPresent(UInt64.self, forKey: .ok)) ?? 0
        err = (try? c.decodeIfPresent(UInt64.self, forKey: .err)) ?? 0
        est_tokens = try c.decodeIfPresent(UInt64.self, forKey: .est_tokens)
    }
}

/// Decodes every router generation: v2.7+ fields (uptime, inflight,
/// models_mapped, usage_log, stats) default when absent, so an older
/// gateway (e.g. v2.0) still renders instead of blanking the dashboard.
struct RouterStatusResponse: Codable {
    let status: String
    let router: String
    let preferred: String
    let active: String
    let target_port: Int
    let uptime_secs: Int
    let inflight: Int
    let models_mapped: Int
    let usage_log: String
    let backends: [String: BackendDetail]
    let stats: [String: BackendStats]
    /// Present only when the router is new enough to measure host facts. An
    /// older router omits it and the dashboard falls back to `loop-studio status
    /// --json`, so the field is optional rather than defaulted.
    let host: HostFacts?

    /// An explicit initializer alongside `init(from:)`, which otherwise
    /// suppresses the memberwise one and leaves no way to build a status
    /// except by decoding one.
    init(
        status: String,
        router: String,
        preferred: String,
        active: String,
        target_port: Int,
        uptime_secs: Int,
        inflight: Int,
        models_mapped: Int,
        usage_log: String,
        backends: [String: BackendDetail],
        stats: [String: BackendStats],
        host: HostFacts?
    ) {
        self.status = status
        self.router = router
        self.preferred = preferred
        self.active = active
        self.target_port = target_port
        self.uptime_secs = uptime_secs
        self.inflight = inflight
        self.models_mapped = models_mapped
        self.usage_log = usage_log
        self.backends = backends
        self.stats = stats
        self.host = host
    }

    enum CodingKeys: String, CodingKey {
        case status, router, preferred, active, target_port
        case uptime_secs, inflight, models_mapped, usage_log, backends, stats, host
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        status = try c.decode(String.self, forKey: .status)
        router = try c.decode(String.self, forKey: .router)
        preferred = try c.decode(String.self, forKey: .preferred)
        active = (try? c.decodeIfPresent(String.self, forKey: .active)) ?? "—"
        target_port = (try? c.decodeIfPresent(Int.self, forKey: .target_port)) ?? 0
        uptime_secs = (try? c.decodeIfPresent(Int.self, forKey: .uptime_secs)) ?? 0
        inflight = (try? c.decodeIfPresent(Int.self, forKey: .inflight)) ?? 0
        models_mapped = (try? c.decodeIfPresent(Int.self, forKey: .models_mapped)) ?? 0
        usage_log = (try? c.decodeIfPresent(String.self, forKey: .usage_log)) ?? ""
        backends = (try? c.decodeIfPresent([String: BackendDetail].self, forKey: .backends)) ?? [:]
        stats = (try? c.decodeIfPresent([String: BackendStats].self, forKey: .stats)) ?? [:]
        host = try? c.decodeIfPresent(HostFacts.self, forKey: .host)
    }
}

/// Host facts the router measures and serves in `/loop-studio/status`.
///
/// The router runs on the host and `common` has the probes, so the answers
/// belong in the document the dashboard already polls — not in a subprocess
/// the client spawns on every poll. `null` means the probe could not answer,
/// which the dashboard renders as "—" rather than as a confident zero.
struct HostFacts: Codable {
    let thermal: String?
    let free_ram_gib: Double?
    let total_ram_gib: Double?
}

/// Best-effort host facts from `loop-studio status --json` (separate endpoint,
/// keys may be absent on older builds — everything optional).
struct LacStatusJson: Codable {
    let model: String?
    let thermal: String?
    let free_ram_gib: Double?
    let total_ram_gib: Double?
    let gateway: Bool?
    let active: String?
}

// MARK: - Network Manager

@MainActor
class NetworkManager: ObservableObject {
    @Published var response: RouterStatusResponse?
    @Published var host: LacStatusJson?
    @Published var isChecking = false
    @Published var lastError: String?
    @Published var autoRefreshPaused = false
    @Published var lastAction: String?
    @Published var daemonInstalled = false
    @Published var hasAutoStartedRouter = false
    @Published var pullingModelId: String?
    @Published var pullOutput: String?
    @Published var pullProgress: Double?
    /// How the last detached child ended, surfaced when a start never comes up.
    @MainActor private let detachedLog = DetachedProcessLog()
    /// Non-nil while a backend start is in flight (mlx / llama / ollama).
    /// The start buttons used to have no guard at all, so a double-click
    /// spawned two engines — and with the same port, the second one loaded a
    /// second copy of the weights.
    @Published private(set) var startingBackend: String?
    private var pullProcess: Process?
    private var routerStartInFlight = false
    /// When the cached host facts were read, for the TTL. Internal so a test can
    /// set the cache state and observe whether the loader runs.
    var hostFactsAt: Date?

    /// Whether a cached host facts reading is still worth showing.
    ///
    /// Separate from the fetch so the rule is assertable without spawning
    /// anything: the dashboard's 3s poll must not become a 3s subprocess.
    nonisolated static func hostFactsAreFresh(
        _ readAt: Date?,
        now: Date = Date(),
        ttl: TimeInterval = hostFactsTTL
    ) -> Bool {
        guard let readAt else { return false }
        return now.timeIntervalSince(readAt) < ttl
    }

    /// How often the app asks the host for its facts. Long enough that a poll
    /// never spawns a subprocess it does not need, short enough that a thermal
    /// excursion is visible within a few seconds of it happening.
    nonisolated static let hostFactsTTL: TimeInterval = 15
    /// Automatic starts since the router last answered. A start that fails is
    /// not a reason to try again on the next 3s tick, and an answer resets
    /// the budget.
    private var autoStartAttempts = 0
    /// When the last automatic start was fired, for the spawn-storm brake.
    private var lastAutoStartAt: Date?
    nonisolated static let maxAutoStartAttempts = 3
    nonisolated static let autoStartCooldown: TimeInterval = 30

    /// Shared tailnet-ready connection (local 127.0.0.1 default, remote
    /// Tailscale IP/MagicDNS + Keychain Bearer token). All URLs flow here.
    var connection: LoopStudioConnectionStore { LoopStudioConnectionStore.shared }

    var port: Int { connection.port }
    var displayHost: String { connection.displayName }
    var isRemote: Bool { connection.isRemote }
    /// True while `startRouter` is running, so the button can say so instead
    /// of accepting a second press that would be silently dropped.
    var isStartingRouter: Bool { routerStartInFlight }

    init() {
        checkDaemonStatus()
    }

    /// Resolve the `loop-studio` binary: explicit env > ~/.local/bin >
    /// /opt/homebrew > /usr/local > PATH. Returns the executable URL plus an
    /// argument prefix: the PATH fallback goes through /usr/bin/env because a
    /// bare `"loop-studio"` is not a valid file URL (`Process.run` would
    /// always throw).
    ///
    /// The pre-rename names are still accepted so a machine that has not
    /// re-run `make install` keeps working.
    nonisolated static func loopStudioExecutable() -> (url: URL, prefix: [String]) {
        let fm = FileManager.default
        for key in ["LOOP_STUDIO_BIN", "LAC_BIN"] {
            if let env = ProcessInfo.processInfo.environment[key], fm.isExecutableFile(atPath: env) {
                return (URL(fileURLWithPath: env), [])
            }
        }
        let home = NSHomeDirectory()
        for name in ["loop-studio", "lac"] {
            for cand in [
                "\(home)/.local/bin/\(name)",
                "/opt/homebrew/bin/\(name)",
                "/usr/local/bin/\(name)",
                "/usr/bin/\(name)"
            ] {
                if fm.isExecutableFile(atPath: cand) { return (URL(fileURLWithPath: cand), []) }
            }
        }
        return (URL(fileURLWithPath: "/usr/bin/env"), ["loop-studio"])
    }

    func checkDaemonStatus() {
        let home = NSHomeDirectory()
        let plist = "\(home)/Library/LaunchAgents/org.loopstudio.router.plist"
        daemonInstalled = FileManager.default.fileExists(atPath: plist)
    }

    /// Whether a failed status poll may start the gateway on its own.
    ///
    /// The flag used to be re-armed from inside `startRouter` *before* it
    /// spawned, so a start that failed was eligible again on the very next
    /// 3s dashboard tick: an unbounded retry loop that leaked one untracked
    /// `loop-studio-router` per cycle, and a router that cannot bind because a
    /// previous attempt is still holding the port.
    ///
    /// Three independent brakes, because they answer three different
    /// questions. *One per outage* (`alreadyAttempted`, cleared only by a
    /// router that answers) stops the retry-per-tick. *A budget* (`attempts`,
    /// reset only by an answer) bounds a flapping gateway across the session.
    /// *A cooldown* stops a spawn storm when the latch is cleared by something
    /// other than a success. A deliberate stop counts as "not a crash": the
    /// operator asked for it, so the app does not undo it on the next tick.
    nonisolated static func shouldAutoStart(
        alreadyAttempted: Bool,
        attempts: Int,
        secondsSinceLastStart: TimeInterval?,
        remote: Bool,
        startInFlight: Bool = false,
        cooldown: TimeInterval = autoStartCooldown,
        maxAttempts: Int = maxAutoStartAttempts
    ) -> Bool {
        if remote { return false }           // the Mac owns the router there
        // A start is already running: its own outcome decides what happens
        // next. Without this, a poll landing during a start consumes the
        // one-per-outage allowance and its (immediately declined) call is lost.
        if startInFlight { return false }
        if alreadyAttempted { return false } // one automatic start per outage
        if attempts >= maxAttempts { return false }
        if let since = secondsSinceLastStart, since < cooldown { return false }
        return true
    }

    func fetch(clearLastError: Bool = true, allowAutoStart: Bool = true) {
        // Coalesce overlapping polls (3s dashboard ticker + manual refresh).
        guard !isChecking else { return }
        isChecking = true
        Task { @MainActor in
            do {
                guard let url = connection.url(path: "/loop-studio/status") else {
                    await MainActor.run {
                        self.lastError = "Invalid router URL (\(connection.displayName))"
                        self.isChecking = false
                    }
                    return
                }
                var req = URLRequest(url: url)
                req.timeoutInterval = 5
                connection.authorize(&req)
                let (data, resp) = try await URLSession.shared.data(for: req)
                if let http = resp as? HTTPURLResponse, http.statusCode == 401 {
                    throw NSError(domain: "LoopStudio", code: 401, userInfo: [NSLocalizedDescriptionKey: "401 Unauthorized — set the gateway token (Tailscale remote)."])
                }
                let decoded = try JSONDecoder().decode(RouterStatusResponse.self, from: data)
                await MainActor.run {
                    self.response = decoded
                    if clearLastError { self.lastError = nil }
                    self.isChecking = false
                    self.autoRefreshPaused = false
                    // A router that answers is the evidence that earns a
                    // future automatic restart, and it resets the budget.
                    if Self.routerIsAnswering(decoded) {
                        self.hasAutoStartedRouter = false
                        self.autoStartAttempts = 0
                        self.lastAutoStartAt = nil
                    }
                }
            } catch {
                await MainActor.run {
                    self.lastError = error.localizedDescription
                    self.isChecking = false
                    // The last known good document is *dropped*, not kept.
                    // `response` was assigned in exactly one place — on a
                    // successful decode — and the catch left it alone, so every
                    // "is it alive" indicator in the app kept reading a stale
                    // snapshot after the gateway was gone: the menu bar said
                    // `Loop Studio Gateway :8000 · Online` with a filled checkmark, the
                    // header and footer dots stayed green, the avatar kept
                    // pulsing, and the chat empty-state offered "Start MLX
                    // Backend" against a dead gateway. A stale reading of a
                    // liveness probe is not a liveness reading, so the failure
                    // clears it and the UI falls back to its offline state,
                    // which is the state the failure established.
                    self.response = nil
                    if self.isRemote {
                        self.hasAutoStartedRouter = false
                    }
                    if allowAutoStart,
                       Self.shouldAutoStart(
                            alreadyAttempted: self.hasAutoStartedRouter,
                            attempts: self.autoStartAttempts,
                            secondsSinceLastStart: self.lastAutoStartAt.map {
                                Date().timeIntervalSince($0)
                            },
                            remote: self.isRemote,
                            startInFlight: self.routerStartInFlight
                       ) {
                        self.hasAutoStartedRouter = true
                        self.autoStartAttempts += 1
                        self.lastAutoStartAt = Date()
                        Task { await self.startRouter() }
                    }
                }
            }
            await fetchHostFacts()
            await MainActor.run {
                self.checkDaemonStatus()
            }
        }
    }

    /// Release the in-flight latch and resume polling, in that order.
    ///
    /// The order is the whole content of this function. `fetch` opens with
    /// `guard !isChecking else { return }`, so it is a no-op while the latch is
    /// set — which means clearing the flag *after* the call is the same as never
    /// resuming at all. Both start paths returned from here without clearing it,
    /// so one successful start — the button, the palette, or the automatic start
    /// that is the default first-run path — latched the flag for the rest of the
    /// session: the 3s poll stopped, the refresh button was permanently
    /// disabled, the menu-bar refresh was a no-op, and an app with no `response`
    /// yet showed "Connecting to router…" forever. The failure path always
    /// cleared it, which is why this only appeared when the start *worked*.
    ///
    /// `poll` is injectable so the ordering is assertable without a router: the
    /// same seam `fetchHostFacts(loader:)` already uses.
    /// `poll` is optional rather than defaulted-to-`self.fetch()` because Swift
    /// forbids capturing `self` in a default argument.
    func resumeAfterSuccessfulStart(poll: (() -> Void)? = nil) {
        isChecking = false
        if let poll {
            poll()
        } else {
            fetch()
        }
    }

    /// A `/loop-studio/status` document that describes a live gateway, as opposed to
    /// the `no-backends` answer a router with nothing behind it gives.
    nonisolated static func routerIsAnswering(_ status: RouterStatusResponse) -> Bool {
        status.status != "no-backends" && status.target_port > 0
    }

    /// Host facts via `loop-studio status --json` (never blocks the router card).
    ///
    /// The dashboard polls every 3s and every poll used to end here, so a
    /// subprocess was spawned three times a minute for as long as the window
    /// was open — for thermal and RAM gauges that change on a scale of seconds.
    /// A fresh reading is taken at most once per `hostFactsTTL`; the poll that
    /// finds a fresh cache costs nothing.
    ///
    /// `loader` is injectable so the cache decision is testable without a
    /// subprocess: a fresh cache must not call it at all.
    func fetchHostFacts(
        force: Bool = false,
        loader: (() async -> LacStatusJson?)? = nil
    ) async {
        guard !isRemote else {
            host = nil
            return
        }
        if let facts = response?.host {
            host = LacStatusJson(
                model: nil,
                thermal: facts.thermal,
                free_ram_gib: facts.free_ram_gib,
                total_ram_gib: facts.total_ram_gib,
                gateway: nil,
                active: nil
            )
            hostFactsAt = Date()
            return
        }
        if !force, Self.hostFactsAreFresh(hostFactsAt) { return }
        let decode = loader ?? { await self.loadHostStatusJson() }
        guard let decoded = await decode() else { return }
        await MainActor.run { self.host = decoded }
        hostFactsAt = Date()
    }

    /// `loop-studio status --json`, decoded. The subprocess is the default loader.
    private func loadHostStatusJson() async -> LacStatusJson? {
        let out = await runLac(["status", "--json"])
        guard let data = out.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(LacStatusJson.self, from: data)
    }

    @discardableResult
    func runLac(_ args: [String], timeoutSeconds: Double = 30) async -> String {
        let routerPort = port
        let apiToken = connection.token
        return await Task.detached(priority: .utility) { () -> String in
            let exe = Self.loopStudioExecutable()
            let p = Process()
            p.executableURL = exe.url
            p.arguments = exe.prefix + args
            var environment = ProcessInfo.processInfo.environment
            environment["LOOP_STUDIO_ROUTER_PORT"] = String(routerPort)
            if !apiToken.isEmpty {
                environment["LOOP_STUDIO_API_TOKEN"] = apiToken
            }
            p.environment = environment
            let pipe = Pipe()
            p.standardOutput = pipe
            p.standardError = pipe
            DispatchQueue.global().asyncAfter(deadline: .now() + timeoutSeconds) {
                guard p.isRunning else { return }
                p.terminate()
                DispatchQueue.global().asyncAfter(deadline: .now() + 2) {
                    guard p.isRunning else { return }
                    p.interrupt()
                    DispatchQueue.global().asyncAfter(deadline: .now() + 2) {
                        if p.isRunning { Darwin.kill(p.processIdentifier, SIGKILL) }
                    }
                }
            }
            do {
                try p.run()
                p.waitUntilExit()
                return String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            } catch {
                return ""
            }
        }.value
    }

    func spawnDetachedLac(_ args: [String]) {
        let routerPort = port
        let apiToken = connection.token
        let exe = Self.loopStudioExecutable()
        Self.runTracked(
            executable: exe.url,
            arguments: exe.prefix + args,
            port: routerPort,
            token: apiToken,
            log: detachedLog
        )
    }

    /// The port a backend actually listens on.
    ///
    /// These were literals — 8080 / 8081 / 11434 — in the very functions that
    /// probe them, while the router, `loop-studio status` and the TUI all read the
    /// lane's real port. With a relocated lane the button started an engine,
    /// then polled a port nothing was bound to for five seconds and reported
    /// nothing. Same class of defect as the literals fixed in `loop-studio status` and
    /// the TUI header a pass earlier, in a third implementation of the rule.
    ///
    /// The precedence is the Rust side's, exactly (`common::mlx_port`,
    /// `llama_port`, `ollama_port`) — including the MLX lane's port *file*,
    /// which is how a drifted engine is found at all. An invented variable
    /// name would have been worse than the literal: it would change what the
    /// app probes without changing where the engine binds. `port` values that
    /// are not ports are ignored rather than probed, and 0 is not a port.
    nonisolated static func backendPort(
        _ lane: Lane,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        mlxPortFile: String? = try? String(
            contentsOf: URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent(".lac/mlx.port"),
            encoding: .utf8
        )
    ) -> Int {
        func valid(_ raw: String?) -> Int? {
            guard let raw, let port = Int(raw.trimmingCharacters(in: .whitespacesAndNewlines)),
                  (1...65535).contains(port) else { return nil }
            return port
        }
        switch lane {
        case .mlx:
            return valid(environment["MLX_PORT"])
                ?? valid(environment["PORT"])
                ?? valid(mlxPortFile)
                ?? 8080
        case .llama:
            return valid(environment["LOOP_STUDIO_LLAMA_PORT"]) ?? 8081
        case .ollama:
            return valid(environment["LOOP_STUDIO_OLLAMA_PORT"]) ?? 11434
        }
    }

    /// A backend lane. `rawValue` is the subcommand `loop-studio serve` takes, so the
    /// enum is the only place that mapping is written down.
    enum Lane: String, CaseIterable {
        case mlx, llama, ollama
    }

    /// Start one backend, with a guard and against its real port.
    ///
    /// `startingBackend` is the guard: the start buttons had none, so a
    /// double-click (or Return held down) spawned a second engine, which found
    /// the port taken, drifted, and loaded a second copy of a 27B model.
    func startBackend(_ lane: Lane) async {
        guard !isRemote else {
            lastAction = "Remote tailnet: start backends on the Mac, not here."
            return
        }
        guard startingBackend == nil else {
            // Say why the button does nothing, rather than appearing broken.
            lastAction = "Already starting \(startingBackend ?? "") — wait for it to come up."
            return
        }
        let port = Self.backendPort(lane)
        startingBackend = lane.rawValue
        defer { startingBackend = nil }
        detachedLog.clear()
        lastAction = "Starting \(lane.rawValue) on :\(port)..."
        spawnDetachedLac(["serve", lane.rawValue])
        for _ in 0..<10 {
            try? await Task.sleep(nanoseconds: 500_000_000)
            if await checkPortHealth(port) {
                lastAction = "\(lane.rawValue) is up on :\(port)"
                break
            }
        }
        // "not up" is what the poll reports when the engine died on a port
        // conflict and what it reports when it is simply slow to load a 27B
        // model. The exit record is the only thing that tells them apart.
        if let failure = detachedLog.last {
            lastAction = failure
            lastError = failure
        }
        fetch()
    }

    func startMLX() async { await startBackend(.mlx) }

    func startLlama() async { await startBackend(.llama) }

    func startOllama() async { await startBackend(.ollama) }

    /// How a detached `loop-studio` child ended.
    ///
    /// `try? p.run()` with no `waitUntilExit` and no `terminationHandler` left
    /// two things behind when the child exited — a port it could not bind, a
    /// preflight that failed: a zombie child of the app until Studio itself
    /// quit, and the only error the operator was ever going to see, discarded.
    /// A `terminationHandler` reaps the child and hands us the status.
    @MainActor
    final class DetachedProcessLog: @unchecked Sendable {
        /// Bounded: a start button that is pressed repeatedly on a dead lane
        /// would otherwise grow this without limit.
        nonisolated static let capacity = 8

        private(set) var entries: [String] = []

        /// The most recent failure, which is what a start that never came up
        /// surfaces.
        var last: String? { entries.last }

        func record(_ message: String) {
            entries.append(message)
            if entries.count > Self.capacity {
                entries.removeFirst(entries.count - Self.capacity)
            }
        }

        func clear() { entries.removeAll() }
    }

    /// The tail of a file, for reading a dead child's complaint.
    ///
    /// A pipe cannot be used for a long-lived child's stderr — it fills at
    /// 64 KiB and blocks the child — so the child writes to a log file and this
    /// reads the end of it. Split out so the truncation is assertable without
    /// spawning anything.
    nonisolated static func tailOfFile(_ url: URL, limit: Int = 400) -> String? {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        guard let size = try? handle.seekToEnd(), size > 0 else { return nil }
        let back = min(Int64(limit), Int64(size))
        // `read(upToCount:)` reads from the current offset, which `seekToEnd`
        // just moved to the end — so the tail has to be sought to first.
        try? handle.seek(toOffset: UInt64(size) - UInt64(back))
        guard let data = try? handle.read(upToCount: Int(back)),
              let text = String(data: data, encoding: .utf8) else { return nil }
        return text
            .split(separator: "\n", omittingEmptySubsequences: true)
            .suffix(4)
            .joined(separator: "\n")
    }

    /// A filesystem-safe log name for a command's stderr.
    ///
    /// The first argument can be a path (a script, a port, a subcommand), and
    /// a path in a file name is a directory structure that does not exist —
    /// which silently sent the child's stderr to null and lost the complaint
    /// this log exists to capture.
    nonisolated static func detachedLogName(for arguments: [String]) -> String {
        let raw = arguments.first.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "lac"
        let safe = String(raw.unicodeScalars.map { scalar in
            CharacterSet.alphanumerics.contains(scalar) ? Character(scalar) : "-"
        })
        return "loop-studio-\(safe).err"
    }

    /// Spawn a detached child and record how it ends.
    ///
    /// Zero-dependency and synchronous-looking, but the child is reaped, its
    /// exit status reaches `log`, and its stderr goes to a per-command log file
    /// whose tail is recorded with it — so a start that dies on a port conflict
    /// says what the engine said, not merely "exited with status 1".
    nonisolated static func runTracked(
        executable: URL,
        arguments: [String],
        port: Int,
        token: String,
        log: DetachedProcessLog
    ) {
        DispatchQueue.global().async {
            let p = Process()
            p.executableURL = executable
            p.arguments = arguments
            var environment = ProcessInfo.processInfo.environment
            environment["LOOP_STUDIO_ROUTER_PORT"] = String(port)
            if !token.isEmpty {
                environment["LOOP_STUDIO_API_TOKEN"] = token
            }
            p.environment = environment
            let nullDev = FileHandle.nullDevice
            p.standardOutput = nullDev
            let command = arguments.joined(separator: " ")
            // Fresh per run: concurrent starts of one lane are guarded, and a
            // stale log would blame the previous attempt for this one.
            let stderrURL = URL(fileURLWithPath: NSHomeDirectory())
                .appendingPathComponent("Library/Logs/\(Self.detachedLogName(for: arguments))")
            try? FileManager.default.createDirectory(
                at: stderrURL.deletingLastPathComponent(), withIntermediateDirectories: true
            )
            try? FileManager.default.removeItem(at: stderrURL)
            // Created explicitly: `forWritingAtPath` opens an existing file and
            // returns nil otherwise, which silently sent the child's stderr to
            // null and lost the complaint this log exists to capture.
            FileManager.default.createFile(atPath: stderrURL.path, contents: nil)
            p.standardError = FileHandle(forWritingAtPath: stderrURL.path) ?? nullDev
            p.terminationHandler = { process in
                let status = process.terminationStatus
                guard status != 0 else { return }
                var message = "\(command) exited with status \(status)"
                if let tail = Self.tailOfFile(stderrURL), !tail.isEmpty {
                    message += " — \(tail)"
                }
                Task { @MainActor in log.record(message) }
            }
            do {
                try p.run()
            } catch {
                Task { @MainActor in
                    log.record("\(command) failed to launch: \(error.localizedDescription)")
                }
            }
        }
    }

    /// Which stderr logs are stale enough to delete.
    ///
    /// Split from the cleanup so the rule is assertable without touching the
    /// real logs directory: a start that died is worth keeping for a day, a
    /// file older than that is a log nobody will read, and a name that is not
    /// one of ours is never touched.
    nonisolated static func staleDetachedLogs(
        entries: [(url: URL, modifiedAt: Date)],
        now: Date = Date(),
        ttl: TimeInterval = 86400
    ) -> [URL] {
        entries.compactMap { entry in
            // Everything in this namespace is ours, including a directory: a
            // version that used the first argument as a file name turned a path
            // into a directory tree under Logs, and only the tree's own mtime
            // says when it is safe to remove.
            guard entry.url.lastPathComponent.hasPrefix("loop-studio-") else { return nil }
            return now.timeIntervalSince(entry.modifiedAt) > ttl ? entry.url : nil
        }
    }

    /// Remove detached-command stderr logs older than `ttl`.
    ///
    /// Each run removes its own log at the start, so at most one file per
    /// command accumulates — but the last run's file is never deleted, and
    /// nothing rotates them. Called when the dashboard appears.
    @discardableResult
    nonisolated static func cleanDetachedLogs(ttl: TimeInterval = 86400, now: Date = Date()) -> Int {
        let dir = URL(fileURLWithPath: NSHomeDirectory())
            .appendingPathComponent("Library/Logs")
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: dir.path) else { return 0 }
        let entries: [(url: URL, modifiedAt: Date)] = names.compactMap { name in
            let url = dir.appendingPathComponent(name)
            let mtime = (try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? now
            return (url, mtime)
        }
        var removed = 0
        for url in staleDetachedLogs(entries: entries, now: now, ttl: ttl) {
            if (try? FileManager.default.removeItem(at: url)) != nil { removed += 1 }
        }
        return removed
    }

    func checkPortHealth(_ targetPort: Int) async -> Bool {
        if isRemote {
            let url = connection.url(path: "/v1/models")
            guard let url else { return false }
            var req = URLRequest(url: url)
            req.timeoutInterval = 3
            connection.authorize(&req)
            do {
                let (_, resp) = try await URLSession.shared.data(for: req)
                guard let http = resp as? HTTPURLResponse, http.statusCode == 200 else { return false }
                return true
            } catch {
                return false
            }
        }
        let url = URL(string: "http://127.0.0.1:\(targetPort)/v1/models")
        guard let url else { return false }
        var req = URLRequest(url: url)
        req.timeoutInterval = 1
        do {
            let (_, resp) = try await URLSession.shared.data(for: req)
            guard let http = resp as? HTTPURLResponse, http.statusCode == 200 else { return false }
            return true
        } catch {
            return false
        }
    }

    /// Free bytes on the home volume (for pull disk preflight).
    static func freeDiskBytes(path: String = NSHomeDirectory()) -> Int64 {
        let attrs = try? FileManager.default.attributesOfFileSystem(forPath: path)
        return (attrs?[.systemFreeSize] as? NSNumber)?.int64Value ?? 0
    }

    static func formatGB(_ bytes: Int64) -> String {
        let gb = Double(bytes) / (1024.0 * 1024.0 * 1024.0)
        if gb >= 1.0 { return String(format: "%.1f GB", gb) }
        return String(format: "%.0f MB", Double(bytes) / (1024.0 * 1024.0))
    }

    func stopAll() async {
        guard !isRemote else {
            lastAction = "Remote tailnet: stop services on the Mac, not here."
            return
        }
        _ = await runLac(["stop"])
        // A deliberate stop is not an outage. Without this the next dashboard
        // tick saw a failing status, found the auto-start flag clear, and
        // started the gateway the operator had just stopped.
        hasAutoStartedRouter = true
        fetch(allowAutoStart: false)
    }

    func pullModel(_ modelId: String, expectedBytes: Int64? = nil) {
        guard !isRemote else {
            lastAction = "Remote tailnet: pull models on the Mac, not here."
            return
        }
        guard pullingModelId == nil else { return }
        // Disk preflight: refuse before spawning when the download
        // provably does not fit (15% headroom for temp files).
        if let expected = expectedBytes, expected > 0 {
            let free = Self.freeDiskBytes()
            let need = Int64(Double(expected) * 1.15)
            if free > 0, free < need {
                let msg = "Pull blocked: \(modelId) needs ~\(Self.formatGB(need)) but only \(Self.formatGB(free)) is free. Inspect Files for a smaller quant or free disk space."
                pullOutput = msg
                lastError = msg
                lastAction = "Pull blocked (disk full)"
                return
            }
        }
        pullingModelId = modelId
        pullOutput = "Starting background download for \(modelId)..."
        pullProgress = nil
        lastAction = "Pulling \(modelId)..."
        
        // The Process is created and published synchronously on MainActor
        // BEFORE the background run starts: the old shape assigned
        // `pullProcess` via a queued main.async, so a fast Cancel in the
        // gap read nil and the child leaked. Only run/wait goes to .global.
        let exe = Self.loopStudioExecutable()
        let p = Process()
        p.executableURL = exe.url
        p.arguments = exe.prefix + ["pull", modelId]
        var environment = ProcessInfo.processInfo.environment
        environment["LOOP_STUDIO_ROUTER_PORT"] = String(port)
        if !connection.token.isEmpty {
            environment["LOOP_STUDIO_API_TOKEN"] = connection.token
        }
        p.environment = environment
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe

        let fileHandle = pipe.fileHandleForReading
        fileHandle.readabilityHandler = { [weak self] handle in
            guard let self else { return }
            let data = handle.availableData
            if !data.isEmpty, let str = String(data: data, encoding: .utf8) {
                DispatchQueue.main.async { [weak self, str] in
                    guard let self else { return }
                    let combined = (self.pullOutput ?? "") + str
                    // Keep only the last 2000 characters to avoid memory bloat
                    self.pullOutput = combined.count > 2000
                        ? String(combined.suffix(2000))
                        : combined
                    self.pullProgress = Self.parseProgress(from: combined)
                }
            }
        }

        self.pullProcess = p

        DispatchQueue.global().async {
            do {
                try p.run()
                p.waitUntilExit()
                fileHandle.readabilityHandler = nil

                DispatchQueue.main.async {
                    if p.terminationStatus != 0 {
                        // Terminated by user cancel (SIGTERM) — not a failure.
                        if p.terminationStatus == 15 || p.terminationStatus == 130 {
                            self.lastAction = "Pull cancelled"
                        } else {
                            self.lastError = "Pull failed with status \(p.terminationStatus)"
                        }
                    } else {
                        self.pullProgress = 1.0
                    }
                    self.pullingModelId = nil
                    self.pullProcess = nil
                    self.lastAction = p.terminationStatus == 0 ? "Pulled \(modelId)" : (self.lastAction ?? "Pull ended")
                    self.fetch()
                }
            } catch {
                DispatchQueue.main.async {
                    self.lastError = error.localizedDescription
                    self.pullingModelId = nil
                    self.pullProcess = nil
                    self.pullProgress = nil
                    self.fetch()
                }
            }
        }
    }

    /// Cancel an in-flight `loop-studio pull`. Terminates the child process;
    /// the completion handler records "cancelled", not a failure.
    func cancelPull() {
        pullProcess?.terminate()
        lastAction = "Cancelling pull..."
    }

    /// Best-effort parse of `NN%` progress from pull tool output.
    /// Returns 0...1, or nil when no percentage is present yet.
    nonisolated static func parseProgress(from text: String) -> Double? {
        // Scan trailing lines first — progress usually lives at the tail.
        let lines = text.split(separator: "\n").suffix(8)
        var best: Double?
        for line in lines {
            var idx = line.startIndex
            while idx < line.endIndex {
                guard let pct = line[idx...].firstIndex(of: "%") else { break }
                // Walk backwards over digits + decimal point.
                var start = pct
                while start > line.startIndex {
                    let prev = line.index(before: start)
                    let c = line[prev]
                    if c.isNumber || c == "." { start = prev } else { break }
                }
                if let v = Double(line[start..<pct]) {
                    best = min(max(v / 100.0, 0.0), 1.0)
                }
                idx = line.index(after: pct)
            }
        }
        return best
    }

    /// Switch the router's preferred backend via the existing
    /// /loop-studio/switch endpoint (no new endpoints), then refresh.
    /// - Parameter clearLastError: if false, `fetch()` won't nil the
    ///   switch error so the user can see what went wrong.
    func switchBackend(_ target: String, clearLastError: Bool = true) async {
        guard let url = connection.url(path: "/loop-studio/switch?target=\(target)") else {
            lastError = "Invalid router URL (\(connection.displayName)) — cannot switch to \(target)"
            return
        }
        var req = URLRequest(url: url)
        req.timeoutInterval = 8
        connection.authorize(&req)
        do {
            let (_, resp) = try await URLSession.shared.data(for: req)
            guard let http = resp as? HTTPURLResponse, http.statusCode == 200 else {
                if clearLastError { lastError = "Router switch failed (HTTP \((resp as? HTTPURLResponse)?.statusCode ?? -1))" }
                return
            }
            lastAction = "Switched router → \(target)"
        } catch {
            if clearLastError { lastError = "Router switch failed: \(error.localizedDescription)" }
        }
        fetch(clearLastError: false)
    }

    /// Install/uninstall the launchd daemons, record the first output
    /// line, then refresh from real status (never assume success).
    func setDaemon(enabled: Bool) async {
        guard !isRemote else { lastAction = "Remote tailnet: manage daemons on the Mac."; return }
        let out = await runLac(["daemon", enabled ? "install" : "uninstall"])
        lastAction = out.split(separator: "\n").first.map(String.init)
        checkDaemonStatus()
        fetch()
    }

    /// Start the gateway in the background, poll until responsive, then refresh.
    /// No-op on remote tailnet (the Mac owns the router there).
    ///
    /// Note what is *not* here any more: `hasAutoStartedRouter = false`. It
    /// used to be set here, before the spawn, which is what turned a failed
    /// start into a 3s-interval retry loop — one untracked `loop-studio-router` per
    /// tick, each finding the port the last one took. A user who presses the
    /// button is not asking for a loop, and a poll that fails is not asking
    /// either; [`shouldAutoStart`](shouldAutoStart) is the only thing that
    /// grants an automatic one.
    func startRouter() async {
        guard !isRemote, !routerStartInFlight else { return }
        routerStartInFlight = true
        defer { routerStartInFlight = false }
        // Every start, manual or automatic, is an attempt: this is what the
        // cooldown is measured from, so a press of the button cannot become a
        // spawn loop either.
        lastAutoStartAt = Date()
        lastAction = "Starting loop-studio-router on :\(port)..."
        isChecking = true

        let home = NSHomeDirectory()
        let plist = "\(home)/Library/LaunchAgents/org.loopstudio.router.plist"
        if FileManager.default.fileExists(atPath: plist) && connection.token.isEmpty {
            // The daemon branch used to discard this output entirely, so a
            // failed `loop-studio daemon install` looked exactly like a slow one until
            // the generic "did not become ready" three seconds later.
            let out = await runLac(["daemon", "install"])
            if let first = out.split(separator: "\n").first {
                lastAction = String(first)
            }
        } else {
            let out = await runLac(["route", "--daemon"])
            if let first = out.split(separator: "\n").first {
                lastAction = String(first)
            }
        }

        for _ in 0..<6 {
            try? await Task.sleep(nanoseconds: 500_000_000)
            if await checkRouterHealth() {
                autoRefreshPaused = false
                lastError = nil
                autoStartAttempts = 0
                lastAutoStartAt = nil
                resumeAfterSuccessfulStart()
                return
            }
        }

        let routerCandidates = [
            "\(home)/.local/bin/loop-studio-router",
            "/opt/homebrew/bin/loop-studio-router",
            "/usr/local/bin/loop-studio-router",
            "\(home)/.local/bin/lac-router",
            "/opt/homebrew/bin/lac-router",
            "/usr/local/bin/lac-router"
        ]
        if let candidate = routerCandidates.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) {
            // The same tracked spawn as the backends: a router that cannot bind
            // its port exits at once, and untracked that was an invisible
            // failure that surfaced only as "did not become ready".
            Self.runTracked(
                executable: URL(fileURLWithPath: candidate),
                arguments: ["\(port)"],
                port: port,
                token: connection.token,
                log: detachedLog
            )
        }

        for _ in 0..<6 {
            try? await Task.sleep(nanoseconds: 500_000_000)
            if await checkRouterHealth() {
                autoRefreshPaused = false
                lastError = nil
                autoStartAttempts = 0
                lastAutoStartAt = nil
                resumeAfterSuccessfulStart()
                return
            }
        }

        // Say what was tried, and leave the decision about trying again to the
        // budget rather than to a latch that never opens again. Latching on
        // failure — which is what this did first — meant one failure disabled
        // automatic recovery for the rest of the session, including the
        // transient case a retry fixes (a port still held by the router that
        // was dying a second ago). The budget and the cooldown bound it: at
        // most `maxAutoStartAttempts` starts, at least `autoStartCooldown`
        // apart.
        lastError = "Router did not become ready on :\(port) (tried the launchd agent, then a local loop-studio-router)."
        lastAction = "Router start failed"
        isChecking = false
        hasAutoStartedRouter = false
        fetch(allowAutoStart: false)
    }

    /// Whether the gateway is up *and usable* — the same rule the auto-start
    /// policy uses. A 200 carrying `no-backends` on `target_port: 0` is a live
    /// router with nothing behind it, and counting it as "ready" ended the
    /// start verification early and then reported the outage as a success.
    func checkRouterHealth() async -> Bool {
        guard let url = connection.url(path: "/loop-studio/status") else { return false }
        var req = URLRequest(url: url)
        req.timeoutInterval = 2
        connection.authorize(&req)
        guard let (data, resp) = try? await URLSession.shared.data(for: req),
              let http = resp as? HTTPURLResponse, http.statusCode == 200,
              let status = try? JSONDecoder().decode(RouterStatusResponse.self, from: data) else {
            return false
        }
        return Self.routerIsAnswering(status)
    }
}
