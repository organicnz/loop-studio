import AppKit
import Darwin
import Foundation
import SwiftUI

// MARK: - Task Models

public enum TaskPriority: String, Codable, CaseIterable, Identifiable, Sendable {
    case critical = "critical"
    case high = "high"
    case medium = "medium"
    case low = "low"

    public var id: String { rawValue }

    public var label: String {
        rawValue.capitalized
    }

    public var color: Color {
        switch self {
        case .critical: return .red
        case .high: return .orange
        case .medium: return .yellow
        case .low: return .secondary
        }
    }
}

public enum TaskStatus: String, Codable, CaseIterable, Identifiable, Sendable {
    case pending = "pending"
    case inProgress = "in_progress"
    case reviewGate = "review_gate"
    case completed = "complete"
    case failed = "failed"

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .pending: return "Queue"
        case .inProgress: return "In Progress"
        case .reviewGate: return "Review Gate"
        case .completed: return "Completed"
        case .failed: return "Failed"
        }
    }

    public var icon: String {
        switch self {
        case .pending: return "tray"
        case .inProgress: return "arrow.triangle.2.circlepath"
        case .reviewGate: return "shield.lefthalf.filled"
        case .completed: return "checkmark.circle.fill"
        case .failed: return "xmark.circle.fill"
        }
    }
}


/// One task's block, plus every line in it this model does not name.
///
/// The queue file is written by two programs. A block is therefore not a
/// `TaskItem` — it is a `TaskItem` *and* the text around it, and a writer that
/// drops the text destroys whatever the other writer put there.
public struct TaskBlock: Sendable {
    public var item: TaskItem
    /// Lines from this block that `TaskItem` has no property for, verbatim and
    /// in order: `attempts:`, unknown keys, the `metadata:` line and every child
    /// of it, and comments.
    public var extras: [String]
    /// This block's own indent, from its `- id:` line. A key is only a field of
    /// the task at exactly this depth.
    var baseIndent: Int = 0

    public init(item: TaskItem, extras: [String] = []) {
        self.item = item
        self.extras = extras
    }
}

extension TaskItem {
    /// The keys this model reads. Anything else in a block is `extras` and is
    /// written back untouched.
    ///
    /// `attempts` is deliberately *not* here. The Rust worker owns it — it is
    /// the retry budget, and only the worker should move it — so the app must
    /// carry it, not interpret it.
    static let modelledKeys: Set<String> = [
        "id", "task", "priority", "status", "loop", "created", "module",
    ]

    /// The keys that live *under* `metadata:`. One of them today; the set exists
    /// so a second one is a one-line change rather than a parser change.
    static let nestedModelledKeys: Set<String> = ["module"]

    /// Is `key` a field of this task at the level it appears?
    ///
    /// `nested` says whether the line is inside a `metadata:` mapping. The two
    /// sets are disjoint, which is what stops a decoy: `module` is a field
    /// there and `status` is not, so `metadata: { status: critical }` is
    /// carried verbatim instead of overwriting the task's own status.
    func isField(_ key: String?, nested: Bool) -> Bool {
        guard let key, key != "metadata" else { return false }
        return nested
            ? TaskItem.nestedModelledKeys.contains(key)
            : TaskItem.modelledKeys.contains(key)
    }


    /// Fold one trimmed `key: value` line into this item.
    ///
    /// The old reader used `replacingOccurrences(of: "task:", with: "")`, which
    /// strips *every* occurrence: a task whose text contained `task:` came back
    /// as `Fix the " key`, and the next write persisted that. The key is now
    /// matched at the start of the line and taken from the first colon, so a
    /// value containing its own key name survives the round trip.
    mutating func absorb(_ trimmed: String) {
        guard let colon = trimmed.firstIndex(of: ":") else { return }
        let key = String(trimmed[trimmed.startIndex..<colon])
            .trimmingCharacters(in: .whitespaces)
        let raw = String(trimmed[trimmed.index(after: colon)...])
            .trimmingCharacters(in: .whitespaces)
        // Single-quoted is this project's own output form, so it is read back
        // the same way; double quotes stay supported because the Rust writer
        // and the shipped template both use them.
        let value: String = {
            if raw.hasPrefix("'") && raw.count >= 2 && raw.hasSuffix("'") {
                return String(raw.dropFirst().dropLast())
                    .replacingOccurrences(of: "''", with: "'")
            }
            return raw.trimmingCharacters(in: CharacterSet(charactersIn: "\"'"))
        }()
        switch key {
        case "id": id = value
        case "task": task = value
        case "priority": priority = TaskPriority(rawValue: value) ?? priority
        case "status": status = TaskStatus(rawValue: value) ?? status
        case "loop": loopName = value
        case "created": created = value
        case "module": module = value
        default: break
        }
    }
}

public struct TaskItem: Identifiable, Codable, Hashable, Sendable {
    public var id: String
    public var task: String
    public var priority: TaskPriority
    public var status: TaskStatus
    public var loopName: String
    public var created: String
    public var module: String?

    public init(
        id: String,
        task: String,
        priority: TaskPriority = .medium,
        status: TaskStatus = .pending,
        loopName: String = "daily-coding.yaml",
        created: String = ISO8601DateFormatter().string(from: Date()),
        module: String? = nil
    ) {
        self.id = id
        self.task = task
        self.priority = priority
        self.status = status
        self.loopName = loopName
        self.created = created
        self.module = module
    }
}

// MARK: - Loop Specification Models

public struct LoopPhase: Identifiable, Hashable, Sendable {
    public let id = UUID()
    public let name: String
    public let action: String?
    public let prompt: String?
    public let model: String?
    public let agent: String?
    public let isHumanGate: Bool

    public init(
        name: String,
        action: String? = nil,
        model: String? = nil,
        agent: String? = nil,
        prompt: String? = nil,
        isHumanGate: Bool = false
    ) {
        self.name = name
        self.action = action
        self.model = model
        self.agent = agent
        self.prompt = prompt
        self.isHumanGate = isHumanGate
    }
}

public struct LoopDefinition: Identifiable, Hashable, Sendable {
    public var id: String { name }
    public let name: String
    public let description: String
    public let maxRounds: Int
    public let requireHumanGate: Bool
    public let phases: [LoopPhase]

    public init(
        name: String,
        description: String,
        maxRounds: Int = 2,
        requireHumanGate: Bool = true,
        phases: [LoopPhase]
    ) {
        self.name = name
        self.description = description
        self.maxRounds = maxRounds
        self.requireHumanGate = requireHumanGate
        self.phases = phases
    }
}

// MARK: - Loop Runner State

public enum RunnerPhase: String, CaseIterable, Identifiable, Sendable {
    case idle = "Idle"
    case preflight = "Preflight"
    case implement = "Implement"
    case review = "Review (@reviewer)"
    case apply = "Apply"
    case humanGate = "Human Gate"
    case complete = "Complete"

    public var id: String { rawValue }

    public var icon: String {
        switch self {
        case .idle: return "circle"
        case .preflight: return "gauge.with.dots.needle.bottom.50percent"
        case .implement: return "wand.and.stars"
        case .review: return "shield.lefthalf.filled"
        case .apply: return "checkmark.seal"
        case .humanGate: return "hand.raised.fill"
        case .complete: return "checkmark.circle.fill"
        }
    }
}

// MARK: - Loops & Kanban Store

@MainActor
public class LoopsStore: ObservableObject {
    @Published public var tasks: [TaskItem] = []
    @Published public var loops: [LoopDefinition] = []
    @Published public var selectedLoop: LoopDefinition?
    @Published public var activeTask: TaskItem?
    @Published public var currentRunnerPhase: RunnerPhase = .idle
    @Published public var runnerLogs: String = ""
    @Published public var isRunningLoop: Bool = false
    @Published public var isGateAwaitingApproval: Bool = false
    /// True when the Implement phase of the current run did not complete. The
    /// human gate stays authoritative — the operator may have made the change
    /// by hand — but the log must not imply this app produced or verified
    /// anything, and this is what lets it say so.
    @Published public internal(set) var implementPhaseFailed: Bool = false
    @Published public var errorMessage: String?
    @Published public var selectedModel: String = "mlx-community/Qwen3.8-27B-4bit"

    private var connection: LoopStudioConnectionStore { LoopStudioConnectionStore.shared }
    private var port: Int { connection.port }

    private var runTask: Task<Void, Never>?
    private var runGeneration: UInt64 = 0
    /// Tail of the queue-mutation chain, so two edits land in the order the
    /// user made them.
    private var mutationTail: Task<Void, Never>?
    /// How many mutations' file work is outstanding, so a reconcile knows the
    /// board is ahead of the file on purpose and must not be reconciled.
    /// A count rather than a bool, so two overlapping mutations do not clear
    /// each other's flag.
    private var mutationsInFlight = 0
    private var mutationInFlight: Bool { mutationsInFlight > 0 }
    /// When the queue file was last read for a reconcile, for the mtime check.
    private var lastReconcileAt: Date?
    private let tasksWriteURLOverride: URL?

    public init(tasksURL: URL? = nil) {
        tasksWriteURLOverride = tasksURL
        loadLoops()
        loadTasks()
    }

    // MARK: - Task Filtering

    public func tasks(for status: TaskStatus) -> [TaskItem] {
        tasks.filter { $0.status == status }
    }

    private var queueLockURL: URL {
        if let tasksWriteURLOverride {
            return tasksWriteURLOverride.deletingLastPathComponent().appendingPathComponent(".lac-queue.lock")
        }
        return URL(fileURLWithPath: Self.studioHome())
            .appendingPathComponent(".lac/locks/loop-studio-tasks")
    }

    /// The Studio's home, matching `common::home_dir()`: `$HOME` when it is set
    /// to an absolute path, else the password-database home.
    ///
    /// The two sides disagreed — Rust resolved `$HOME`,
    /// `FileManager.homeDirectoryForCurrentUser` reads passwd — so with
    /// `HOME=/tmp/x` the CLI and the app wrote and read two different queues.
    /// `NSHomeDirectory()` honors `$HOME` but falls back to passwd when it is
    /// unset, which is exactly the case Rust now refuses, so it is not enough on
    /// its own.
    nonisolated static func studioHome(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        if let home = environment["HOME"], !home.isEmpty, home.hasPrefix("/") {
            return home
        }
        return NSHomeDirectory()
    }

    // MARK: - Queue lock
    //
    // The same lock `common::acquire_lock` takes on the Rust side, on the
    // same path (`~/.lac/locks/loop-studio-tasks`), read and written by the same
    // `loop-studio worker`. Three things were wrong with the Studio's copy, and all
    // three are defects the Rust side had already been fixed for:
    //
    // 1. It wrote `"<pid>"` and read it with `Int32(whole string)`, while the
    //    CLI writes `"<pid> <unix-seconds>"`. Every CLI-held lock therefore
    //    failed to parse here, fell through to the creation-date branch, and
    //    was reclaimed 30s into a *live* worker's hold — two writers on one
    //    queue, which is the single thing the lock exists to prevent.
    // 2. It removed the directory on the way out unconditionally, so a lock
    //    that had been reclaimed out from under this process was deleted by
    //    this process — handing the queue to a third writer.
    // 3. It waited with `Thread.sleep` in a `@MainActor` function, up to 30
    //    seconds. A held lock froze the entire app: no scrolling, no
    //    streaming, no window dragging, because the main actor was asleep.

    /// Whether an existing queue lock may be taken over.
    ///
    /// The one unconditional reclaim is a pid that is provably gone. A live
    /// owner keeps the lock regardless of age — a forward clock jump must not
    /// hand one queue to two writers, and a slow worker must not lose its lock
    /// for being slow. Everything else (no record, an unparsable record, a pid
    /// that cannot be probed) waits out `staleSeconds`, which is what the
    /// empty-record window between `open(O_TRUNC)` and `write` requires.
    nonisolated public static func lockIsReclaimable(
        owner: String?,
        directoryAge: TimeInterval?,
        staleSeconds: TimeInterval = 30,
        probe: (Int32) -> Bool?
    ) -> Bool {
        let agedOut = (directoryAge ?? 0) >= staleSeconds
        let pid = owner?
            .split(whereSeparator: { $0 == " " || $0 == "\n" || $0 == "\t" })
            .first
            .flatMap { Int32($0) }
        guard let pid, pid > 0 else { return agedOut }
        switch probe(pid) {
        case .some(false): return true   // provably gone — the crash case
        case .some(true): return false   // alive, however old
        case .none: return agedOut       // unprobeable: age is all there is
        }
    }

    /// `kill(pid, 0)`: Some(true) alive, Some(false) gone, nil unprobeable.
    ///
    /// EPERM is reported as *gone*, because that is what the CLI's probe
    /// (`common::process_alive` shells out to `/bin/kill -0`, which exits
    /// non-zero on EPERM and cannot see the errno) concludes, and the two
    /// implementations must reach the same verdict on the same record. The
    /// case it gets "wrong" — a lock held by another user's process — cannot
    /// occur for a per-user `~/.lac/locks`, which only that user can write.
    nonisolated public static func processIsAlive(_ pid: Int32) -> Bool? {
        guard pid > 0 else { return nil }
        if kill(pid, 0) == 0 { return true }
        return false
    }

    /// Take the queue lock, waiting without blocking any actor.
    /// Take the queue lock, waiting without blocking any actor.
    ///
    /// `staleSeconds` is the backstop, and it is the CLI's number
    /// (`common::acquire_lock("loop-studio-tasks", 30)`), not a Studio invention: the
    /// two sides wait on the same terms or one of them gives up while the other
    /// would have waited. A live owner is never reclaimed regardless of age —
    /// only a pid that is provably gone, or an unprobeable one past the
    /// backstop, loses the lock.
    nonisolated static func acquireQueueLock(
        at url: URL,
        staleSeconds: TimeInterval = 30
    ) async -> Bool {
        try? FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        let deadline = Date().addingTimeInterval(staleSeconds)
        while true {
            if (try? FileManager.default.createDirectory(
                atPath: url.path, withIntermediateDirectories: false
            )) != nil {
                // Published in the CLI's format, so either side can read
                // either side's record.
                let owner = "\(ProcessInfo.processInfo.processIdentifier) \(Int(Date().timeIntervalSince1970))"
                do {
                    try owner.data(using: .utf8)?.write(
                        to: url.appendingPathComponent("owner"), options: .atomic
                    )
                    return true
                } catch {
                    // A lock with no readable owner record is one this process
                    // can neither prove it holds nor release — and nobody else
                    // may take it while this process lives. Give it back.
                    try? FileManager.default.removeItem(at: url)
                    return false
                }
            }
            let owner = try? String(contentsOf: url.appendingPathComponent("owner"), encoding: .utf8)
            // mtime, not birth time: `common::acquire_lock` ages a lock by
            // `metadata().modified()`, and birth time is absent on some volumes
            // (this project supports an external model library), which would
            // leave the backstop permanently unable to fire here.
            let attributes = try? FileManager.default.attributesOfItem(atPath: url.path)
            let modified = attributes?[.modificationDate] as? Date
            let age = modified.map { Date().timeIntervalSince($0) }
            if lockIsReclaimable(owner: owner, directoryAge: age, probe: processIsAlive) {
                try? FileManager.default.removeItem(at: url)
                continue
            }
            if Date() >= deadline { return false }
            // Suspend, don't sleep: this used to be `Thread.sleep` on the main
            // actor, which is a frozen window for everyone. Cancellation ends
            // the wait rather than being swallowed — a `try?` here would spin
            // the retry loop against a lock nobody is waiting for any more.
            do {
                try await Task.sleep(nanoseconds: 50_000_000)
            } catch {
                return false
            }
        }
    }

    /// Release the lock only if this process still owns it.
    nonisolated static func releaseQueueLock(at url: URL) {
        let owner = try? String(contentsOf: url.appendingPathComponent("owner"), encoding: .utf8)
        let pid = owner?
            .split(whereSeparator: { $0 == " " || $0 == "\n" || $0 == "\t" })
            .first
            .flatMap { Int32($0) }
        guard pid == ProcessInfo.processInfo.processIdentifier else { return }
        try? FileManager.default.removeItem(at: url)
    }

    /// Read-modify-write the queue under the lock, off the main actor.
    ///
    /// The edit is applied to `tasks` *before* the write is attempted, so a drag
    /// is instant. A write that fails rolls back to the snapshot taken before
    /// the drag: the card goes back to where the drag found it and the banner
    /// says why. The previous shape left the card where the user dropped it
    /// while the file behind it disagreed — a drag that did not land, still
    /// looking like it did, with no way back.
    ///
    /// `edit` touches only the array — no actor state — so it can run on a
    /// detached task. Mutations are chained in call order, which is the
    /// ordering the old synchronous version got for free from the main actor.
    private func withQueueMutation(_ edit: @escaping @Sendable (inout [TaskItem]) -> Void) async -> Bool {
        let url = tasksWriteURL()
        let lockURL = queueLockURL
        // The snapshot is the base for the edit when the file cannot be read,
        // and the rollback target when the write fails. Collapsing "absent or
        // unreadable" into an empty list is what the first version of this did,
        // and it wiped the board: a machine with no `~/todo/loop-studio-tasks.yaml`
        // (every fresh install, where `loadTasks` seeds four tasks and reads
        // the read-only `templates/` copy) had its first drag write a
        // header-only file and set `tasks = []`.
        let before = tasks
        edit(&tasks)
        errorMessage = nil
        // Call order is preserved by chaining, not by luck: the file work is
        // serialized by the lock, but the order two *edits* reach `tasks` is
        // the order they were asked for, which is what the old synchronous
        // main-actor version got for free.
        let previous = mutationTail
        mutationsInFlight += 1
        let work = Task { @MainActor () -> Outcome<[TaskItem]> in
            _ = await previous?.value
            return await Task.detached(priority: .userInitiated) { () -> Outcome<[TaskItem]> in
                guard await Self.acquireQueueLock(at: lockURL) else { return .failure(.busy) }
                defer { Self.releaseQueueLock(at: lockURL) }
                var current: [TaskItem]
                // Blocks, not a re-serialised model. `edit` mutates the items;
                // the file is re-emitted with every line the model does not
                // name carried through verbatim. Re-serialising `TaskItem` here
                // is what dropped `attempts:` — the worker's retry budget — and
                // with it the dead-letter that stops a poisoned task from
                // re-queuing itself. Confirmed on disk before the fix: four
                // `status: failed` tasks and zero `attempts:` keys, which the
                // worker writes together in one call.
                let preamble: String
                let text: String
                if let content = try? String(contentsOf: url, encoding: .utf8) {
                    preamble = Self.preamble(of: content)
                    text = content
                    current = Self.parseTasksYaml(content)
                } else {
                    preamble = ""
                    text = ""
                    current = before
                }
                var blocks = Self.parseTaskBlocks(text)
                if blocks.isEmpty && !current.isEmpty {
                    // A file this parser could not read into blocks, but a model
                    // we do have. Fall back to the old whole-file write rather
                    // than refusing the edit — the one case where a model-only
                    // round trip is the best available answer, and it is stated
                    // here because it is a loss, not a success.
                    blocks = current.map { TaskBlock(item: $0) }
                }
                edit(&current)
                // `edit` may add, delete or reorder; carry the matching block
                // along by id so a surviving task keeps its extras.
                let extrasById = Dictionary(
                    blocks.map { ($0.item.id, $0.extras) },
                    uniquingKeysWith: { first, _ in first }
                )
                let rebuilt = current.map { item in
                    TaskBlock(item: item, extras: extrasById[item.id] ?? [])
                }
                let yaml = Self.serializeTaskBlocks(rebuilt, preamble: preamble)
                do {
                    try FileManager.default.createDirectory(
                        at: url.deletingLastPathComponent(), withIntermediateDirectories: true
                    )
                    try yaml.write(to: url, atomically: true, encoding: .utf8)
                } catch {
                    return .failure(.writeFailed)
                }
                return .success(current)
            }.value
        }
        mutationTail = Task { _ = await work.value }
        let outcome = await work.value
        // Decremented only once the file work is actually done. The flag used
        // to be cleared on the same turn that set it, before the await, so it
        // was never true while the lock was being waited on — and a reconcile
        // could replace `tasks` with the stale file, making the optimistic
        // edit disappear until the write landed. A count, rather than a
        // bool, so overlapping mutations do not clear each other's flag.
        mutationsInFlight -= 1
        switch outcome {
        case .success(let updated):
            tasks = updated
            return true
        case .failure(let error):
            tasks = before
            errorMessage = error == .busy
                ? "Queue is busy — a worker holds it. Try again."
                : "Queue update could not be saved."
            return false
        }
    }

    /// Adopt queue changes made by the other writer.
    ///
    /// The file is the truth for what is durable and the board is the truth for
    /// what the user asked for; they meet on every successful write, and a write
    /// that fails rolls the board back. What they cannot do on their own is
    /// notice a change made elsewhere — the `loop-studio worker` on this machine, or the
    /// app on the Mac this one is remoted to — and a queue nobody mutates stays
    /// diverged until someone does. Reconciling on appear picks those up: the
    /// board is replaced by the file, and the operator sees the truth.
    ///
    /// It only ever adopts, never pushes, so the risk is limited to clobbering
    /// board state — which is why it waits for any in-flight mutation and why a
    /// failed write leaves the board equal to the file rather than different.
    func reconcileWithDisk() {
        guard !mutationInFlight else { return }
        let url = tasksWriteURL()
        guard let content = try? String(contentsOf: url, encoding: .utf8) else { return }
        let onDisk = Self.parseTasksYaml(content);
        lastReconcileAt = Date();
        guard onDisk != tasks else { return }
        tasks = onDisk
        errorMessage = nil
    }

    /// Reconcile when the queue file's mtime shows someone else wrote it.
    ///
    /// The appear-time reconcile covers a queue that changed while the
    /// dashboard was away; this covers one that changes while it is open. A
    /// stat is enough — the reconcile itself is a no-op when the board already
    /// matches the file, which is the case after every write this app made, so
    /// the cost of a wrong guess is one extra file read.
    func reconcileIfQueueChanged() {
        guard !mutationInFlight, let last = lastReconcileAt else { return }
        let url = tasksWriteURL()
        let mtime = (try? FileManager.default.attributesOfItem(atPath: url.path))?[.modificationDate] as? Date
        guard let mtime, mtime > last else { return }
        reconcileWithDisk()
    }

    private enum Outcome<T: Sendable>: Sendable {
        case success(T)
        case failure(QueueLockError)
    }

    private enum QueueLockError: Error, Sendable {
        case busy
        case writeFailed
    }

    /// Returns false when the queue could not be written, which is the only
    /// way a caller learns that the board is not showing what it claims.
    @discardableResult
    public func moveTask(id: String, to newStatus: TaskStatus) async -> Bool {
        let exists = tasks.contains { $0.id == id }
        let saved = await withQueueMutation { tasks in
            guard let idx = tasks.firstIndex(where: { $0.id == id }) else { return }
            tasks[idx].status = newStatus
        }
        if exists && saved { LiquidGlass.haptic(.alignment) }
        return saved
    }

    @discardableResult
    public func addTask(_ item: TaskItem) async -> Bool {
        let saved = await withQueueMutation { tasks in
            tasks.append(item)
        }
        if saved { LiquidGlass.haptic(.alignment) }
        return saved
    }

    @discardableResult
    public func addTask(
        taskDescription: String,
        priority: TaskPriority,
        loopName: String,
        module: String?
    ) async -> Bool {
        let trimmed = taskDescription.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }

        let saved = await withQueueMutation { tasks in
            // The next id comes from the list read *inside* the lock, not from
            // whatever the UI happened to be holding, so a queue the worker
            // changed underneath us cannot hand out a duplicate id.
            // Both the current and the pre-rename prefixes are parsed, so a
            // queue written before the rename does not hand out an id that
            // collides with a task already in it.
            let maxN = tasks.compactMap { task in
                for prefix in ["loop-studio-", "lac-"] where task.id.hasPrefix(prefix) {
                    return Int(task.id.dropFirst(prefix.count))
                }
                return nil
            }.max() ?? 0
            let newId = String(format: "loop-studio-%03d", maxN + 1)
            let item = TaskItem(
                id: newId,
                task: trimmed,
                priority: priority,
                status: .pending,
                loopName: loopName,
                module: module?.isEmpty == true ? nil : module
            )
            tasks.insert(item, at: 0)
        }
        if saved { LiquidGlass.haptic(.alignment) }
        return saved
    }

    @discardableResult
    public func deleteTask(id: String) async -> Bool {
        let saved = await withQueueMutation { tasks in
            tasks.removeAll { $0.id == id }
        }
        if saved { LiquidGlass.haptic(.alignment) }
        return saved
    }

    // MARK: - Loop Runner State Machine

    public func executeLoop(task: TaskItem) {
        guard !isRunningLoop else { return }
        activeTask = task
        isRunningLoop = true
        runGeneration &+= 1
        let generation = runGeneration
        isGateAwaitingApproval = false
        implementPhaseFailed = false
        runnerLogs = "=== Initiating Loop Studio Loop: \(task.loopName) for [\(task.id)] ===\n"
        runnerLogs += "Task: \(task.task)\n\n"

        Task { @MainActor in await moveTask(id: task.id, to: .inProgress) }

        runTask = Task { [weak self, generation] in
            guard let self else { return }
            defer {
                if self.runGeneration == generation {
                    self.isRunningLoop = false
                    self.runTask = nil
                }
            }
            guard self.runGeneration == generation else { return }

            // Phase 1: Preflight (local environment checks)
            self.currentRunnerPhase = .preflight
            self.appendLog("[1/5] Phase: Preflight — Checking thermals & memory budget...")
            do {
                try await Task.sleep(nanoseconds: 1_200_000_000)
            } catch { return } // cancelled — stop the pipeline
            guard self.runGeneration == generation, !Task.isCancelled else { return }
            // NOTE: local heuristic until `loop-studio status --json` is wired in.
            self.appendLog("✓ Preflight (local heuristic): proceeding — verify thermals in Ops Dashboard.")

            // Phase 2: Implement Turn
            self.currentRunnerPhase = .implement
            self.appendLog("\n[2/5] Phase: Implement — Dispatching to @coder on local gateway :\(self.port)...")
            let prompt = "Implement task \(task.id): \(task.task). Output exact code without committing."
            do {
                let reply = try await self.queryGateway(prompt: prompt, system: "You are @coder. Implement the requested task with minimal diffs and idiomatic design.")
                guard self.runGeneration == generation, !Task.isCancelled else { return }
                self.appendLog("✓ Code Implementation Complete:\n" + String(reply.prefix(300)) + "\n...")
            } catch is CancellationError {
                return
            } catch {
                guard self.runGeneration == generation else { return }
                self.implementPhaseFailed = true
                self.appendLog("⚠ Implement step failed (\(error.localizedDescription)). Continuing to review gate.")
            }
            guard self.runGeneration == generation, !Task.isCancelled else { return }

            // Phase 3: Reviewer Audit (advisory until @reviewer lane is wired)
            self.currentRunnerPhase = .review
            self.appendLog("\n[3/5] Phase: Review — Auditor pass (@reviewer, read-only)...")
            do {
                try await Task.sleep(nanoseconds: 1_500_000_000)
            } catch { return }
            guard self.runGeneration == generation, !Task.isCancelled else { return }
            self.appendLog("• Reviewer Report (advisory): no automated checks ran — human gate below is authoritative.")

            // Phase 4: Apply & Test Gate (advisory until test harness is wired)
            self.currentRunnerPhase = .apply
            self.appendLog("\n[4/5] Phase: Apply & Test Gate — Running affected test harness...")
            do {
                try await Task.sleep(nanoseconds: 1_200_000_000)
            } catch { return }
            guard self.runGeneration == generation, !Task.isCancelled else { return }
            self.appendLog("• Test Gate (advisory): no test harness ran — confirm via `cargo test` / `swift test` before approving.")

            // Phase 5: Human Gate
            guard self.runGeneration == generation, !Task.isCancelled else { return }
            self.currentRunnerPhase = .humanGate
            self.appendLog("\n[5/5] Phase: Human Review Gate (AGENTS.md mandatory requirement)")
            self.appendLog("Awaiting human inspection of git diff before task completion...")
            self.isGateAwaitingApproval = true
            await self.moveTask(id: task.id, to: .reviewGate)
        }
    }

    /// The operator is the gate — that is the point of the phase — so this
    /// records their decision and nothing more.
    ///
    /// The old line claimed "Changes verified via git diff" on every approval.
    /// Nothing in this app verifies anything at this moment: the diff sheet is
    /// the operator's to read, and when `git diff` could not be run the sheet
    /// says so rather than showing a clean tree. An approval after a failed
    /// Implement phase now says that too, instead of implying this run produced
    /// the change being approved.
    public func approveHumanGate() async {
        guard let task = activeTask, isGateAwaitingApproval else { return }
        // Latch first, write second. The write is off the main actor now and
        // can take as long as the queue lock is held, so awaiting it before
        // clearing this flag left a window in which a second press on Approve
        // ran the whole gate again — a second approval record, and a second
        // completion for one operator decision.
        isGateAwaitingApproval = false
        currentRunnerPhase = .complete
        let saved = await moveTask(id: task.id, to: .completed)
        guard saved else {
            // The write failed, so the board rolled back and the task is not
            // complete. The gate has to be re-opened, or the operator has no
            // Approve button to retry with and the run is stuck.
            isGateAwaitingApproval = true
            currentRunnerPhase = .humanGate
            appendLog(
                "\n[x] The queue write failed, so \(task.id) is NOT marked complete. "
                + "The gate is still open; approve again to retry."
            )
            return
        }
        if implementPhaseFailed {
            appendLog("""
            \n✓ Human Gate APPROVED by operator. Task \(task.id) marked Completed.
            ⚠ The Implement phase of this run failed, so nothing was produced or checked by this app — \
            this completion is on the operator's word alone.
            """)
        } else {
            appendLog("""
            \n✓ Human Gate APPROVED by operator. Task \(task.id) marked Completed.
            The operator inspected the diff; this app ran no verification of its own \
            (no test harness, no diff check) at this point.
            """)
        }
        LiquidGlass.haptic(.alignment)
    }

    public func cancelLoop() {
        runGeneration &+= 1
        runTask?.cancel()
        runTask = nil
        isRunningLoop = false
        isGateAwaitingApproval = false
        currentRunnerPhase = .idle
        appendLog("\n*[Loop Execution Halted by User]*")
    }

    private func appendLog(_ text: String) {
        runnerLogs += text + "\n"
    }

    // MARK: - Gateway Client

    private func queryGateway(prompt: String, system: String) async throws -> String {
        guard let url = connection.url(path: "/v1/chat/completions") else {
            throw URLError(.badURL)
        }
        struct WireMessage: Encodable { var role: String; var content: String }
        struct WireRequest: Encodable {
            var model: String
            var messages: [WireMessage]
            var temperature: Double
        }

        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        connection.authorize(&req)
        req.timeoutInterval = 20
        let modelName = selectedModel.isEmpty ? "mlx-community/Qwen3.8-27B-4bit" : selectedModel
        req.httpBody = try JSONEncoder().encode(WireRequest(
            model: modelName,
            messages: [
                WireMessage(role: "system", content: system),
                WireMessage(role: "user", content: prompt)
            ],
            temperature: 0.2
        ))

        let (data, response) = try await URLSession.shared.data(for: req)
        guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
            throw URLError(.badServerResponse)
        }

        struct DecodedResponse: Codable {
            struct Choice: Codable {
                struct Msg: Codable { var content: String? }
                var message: Msg?
            }
            var choices: [Choice]?
        }

        if let dec = try? JSONDecoder().decode(DecodedResponse.self, from: data),
           let content = dec.choices?.first?.message?.content {
            return content
        }
        return String(data: data, encoding: .utf8) ?? ""
    }

    // MARK: - Persistence (YAML / JSON fallback)

    /// User-writable queue location (readable in tests to pin the path).
    func tasksWriteURL() -> URL {
        if let tasksWriteURLOverride {
            return tasksWriteURLOverride
        }
        // Writes always go to the user's queue — never into the repo's
        // templates/ copy, which would dirty the working tree on every
        // Kanban mutation. Reads still fall back to templates/.
        return URL(fileURLWithPath: Self.studioHome())
            .appendingPathComponent("todo/loop-studio-tasks.yaml")
    }

    private func tasksFileURL() -> URL {
        let userTasks = tasksWriteURL()
        if FileManager.default.fileExists(atPath: userTasks.path) {
            return userTasks
        }
        // Read-only fallback to workspace templates/tasks/loop-studio-tasks.yaml
        let cwd = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        let wsTasks = cwd.appendingPathComponent("templates/tasks/loop-studio-tasks.yaml")
        if FileManager.default.fileExists(atPath: wsTasks.path) {
            return wsTasks
        }
        return userTasks
    }

    private func loadTasks() {
        let url = tasksFileURL()
        guard let content = try? String(contentsOf: url, encoding: .utf8) else {
            // Seed default tasks
            seedDefaultTasks()
            return
        }

        // Parse basic YAML tasks (custom minimal parser for offline robustness)
        let parsed = Self.parseTasksYaml(content)
        if !parsed.isEmpty {
            self.tasks = parsed
        } else {
            seedDefaultTasks()
        }
    }

    private func seedDefaultTasks() {
        self.tasks = [
            TaskItem(
                id: "loop-studio-001",
                task: "Verify local model stack and run smoke tests via gateway :8000",
                priority: .high,
                status: .completed,
                loopName: "daily-coding.yaml",
                module: "loop-studio-gateway"
            ),
            TaskItem(
                id: "loop-studio-002",
                task: "Benchmark speculative decoding MTP speedup vs single-token decode",
                priority: .medium,
                status: .inProgress,
                loopName: "daily-coding.yaml",
                module: "inference-eval"
            ),
            TaskItem(
                id: "loop-studio-003",
                task: "Audit rust-src code hygiene and generate AUDIT_REPORT.md",
                priority: .low,
                status: .pending,
                loopName: "security-audit.yaml",
                module: "rust-src"
            ),
            TaskItem(
                id: "loop-studio-004",
                task: "Refactor RingBuffer circular queue for Apple Silicon cache locality",
                priority: .critical,
                status: .reviewGate,
                loopName: "bug-fix.yaml",
                module: "rust-src/worker"
            )
        ]
    }

    // MARK: - Loop Loader

    private func loadLoops() {
        let loops = [
            LoopDefinition(
                name: "bug-fix.yaml",
                description: "Reproduce -> isolate -> fix -> regression test -> audit",
                maxRounds: 2,
                phases: [
                    LoopPhase(name: "Preflight", action: "Check thermals & memory"),
                    LoopPhase(name: "Reproduce", agent: "coder", prompt: "Create failing test"),
                    LoopPhase(name: "Implement", agent: "coder", prompt: "Fix root cause"),
                    LoopPhase(name: "Review", agent: "reviewer", prompt: "Review diff for safety"),
                    LoopPhase(name: "Apply", agent: "coder", prompt: "Confirm 100% test pass"),
                    LoopPhase(name: "Human Gate", isHumanGate: true)
                ]
            ),
            LoopDefinition(
                name: "daily-coding.yaml",
                description: "Clean scope -> implement -> unit tests -> review -> human gate",
                maxRounds: 2,
                phases: [
                    LoopPhase(name: "Preflight", action: "Verify stack"),
                    LoopPhase(name: "Implement", agent: "coder", prompt: "Implement scoped task"),
                    LoopPhase(name: "Tests", agent: "coder", prompt: "Generate tests"),
                    LoopPhase(name: "Review", agent: "reviewer", prompt: "Audit diff"),
                    LoopPhase(name: "Human Gate", isHumanGate: true)
                ]
            ),
            LoopDefinition(
                name: "feature-branch.yaml",
                description: "Branch -> feature implementation -> comprehensive test suite -> merge gate",
                maxRounds: 3,
                phases: [
                    LoopPhase(name: "Branch", action: "Create git branch"),
                    LoopPhase(name: "Implement", agent: "coder", prompt: "Write feature code"),
                    LoopPhase(name: "Review", agent: "reviewer", prompt: "Check design invariants"),
                    LoopPhase(name: "Gate", isHumanGate: true)
                ]
            ),
            LoopDefinition(
                name: "security-audit.yaml",
                description: "Deep audit -> race conditions -> panics -> buffer overflows -> remediation",
                maxRounds: 2,
                phases: [
                    LoopPhase(name: "Scan", action: "Scan codebase for hazards"),
                    LoopPhase(name: "Audit", agent: "reviewer", prompt: "Deep adversarial audit"),
                    LoopPhase(name: "Remediate", agent: "coder", prompt: "Apply security fixes"),
                    LoopPhase(name: "Gate", isHumanGate: true)
                ]
            )
        ]
        self.loops = loops
        self.selectedLoop = loops.first
    }

    // MARK: - YAML Serialization Helpers

    /// The read model for the queue file.
    ///
    /// Parsed through `parseTaskBlocks`, the same parser the write path uses,
    /// so the values a read shows are the values a write persists. The
    /// previous implementation here was a second, line-oriented parser that
    /// disagreed with the writer about quoting: it stripped the outer quote
    /// characters without unescaping `''`, so a task containing an apostrophe
    /// came back as `It''s`, and the next write emitted `'It''''s'`. Every
    /// queue mutation doubled every apostrophe in every task, without limit.
    /// It also matched `status:` at any indent, so a nested `metadata:` decoy
    /// could hijack a task's own status.
    public nonisolated static func parseTasksYaml(_ text: String) -> [TaskItem] {
        parseTaskBlocks(text)
            .map(\.item)
            .filter { !$0.id.isEmpty && !$0.task.isEmpty }
    }

    public nonisolated static func serializeTasksYaml(_ items: [TaskItem]) -> String {
        var out = "# Loop Studio Autonomous Task Queue (Kanban)\n# Managed by Loop Studio\n\n"
        for item in items {
            out += "- id: \"\(item.id)\"\n"
            out += "  task: \"\(item.task)\"\n"
            out += "  priority: \(item.priority.rawValue)\n"
            out += "  status: \(item.status.rawValue)\n"
            out += "  loop: \(item.loopName)\n"
            out += "  created: \"\(item.created)\"\n"
            if let m = item.module {
                out += "  metadata:\n"
                out += "    module: \"\(m)\"\n"
            }
            out += "\n"
        }
        return out
    }

    /// Parse into blocks that keep every line this model does not name.
    ///
    /// `parseTasksYaml` is the read model and its shape is public; this is the
    /// write model, and the difference is the whole point. The queue file has
    /// two writers — this app and `loop-studio worker` — and the worker's entire retry
    /// budget lives in a key this model has no property for: `MAX_TASK_ATTEMPTS`
    /// is gated on `attempts < 3` (`loop_studio.rs`), and `update_task` writes
    /// `status: failed` and `attempts: 3` in the *same* call. So any Studio
    /// mutation that re-serialised the model dropped `attempts:`, the next
    /// `pending_candidates` read saw `attempts: 0`, and a task deliberately
    /// poisoned to stop the daemon hot-looping was re-queued.
    ///
    /// That is corroborated on disk rather than inferred: the live
    /// `~/todo/loop-studio-tasks.yaml` carried four `status: failed` tasks and not one
    /// `attempts:` key, which the worker can only produce together.
    ///
    /// Unmodelled lines are kept verbatim — comments, key order, unknown keys,
    /// and the `metadata:` children beyond `module` that the shipped template
    /// ships (`metadata: {module, target}` and `{module, tokens}`). A writer
    /// that cannot round-trip what it does not understand will destroy the other
    /// writer's state, so the fix is to edit the text rather than re-emit a model
    /// of it.
    nonisolated static func parseTaskBlocks(_ text: String) -> [TaskBlock] {
        var blocks: [TaskBlock] = []
        var current: TaskBlock?
        var insideMetadata = false

        func flush() {
            if var block = current {
                // A blank line inside a block falls through to the extras,
                // because it is not a `key: value` line at the block's depth.
                // The writer then emits those stored blanks *and* adds its own
                // separator, so the queue file grew by one blank line per
                // block boundary — and one more at the end — on every edit.
                // Trailing blanks are dropped; blanks inside the block are
                // kept, because they are the user's own layout.
                while let last = block.extras.last,
                      last.trimmingCharacters(in: .whitespaces).isEmpty {
                    block.extras.removeLast()
                }
                blocks.append(block)
            }
            current = nil
        }

        for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = String(raw)
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            let indented = line.first == " " || line.first == "\t"

            if trimmed.hasPrefix("- id:") {
                flush()
                let id = trimmed.replacingOccurrences(of: "- id:", with: "")
                    .trimmingCharacters(in: .whitespaces)
                    .trimmingCharacters(in: CharacterSet(charactersIn: "\"'"))
                var block = TaskBlock(item: TaskItem(
                    id: id, task: "", priority: .medium, status: .pending,
                    loopName: "daily-coding.yaml", created: "", module: nil
                ))
                // Fields sit two columns deeper than the `- id:` line, and
                // `baseIndent` is compared against a *count of leading
                // whitespace characters*, so it is the same unit. Getting the
                // two out of step makes every field look nested, and the block
                // then parses as an id and nothing else.
                block.baseIndent =
                    line.prefix(while: { $0 == " " || $0 == "\t" }).count + 2
                current = block
                insideMetadata = false
                continue
            }
            guard var block = current else {
                // Outside any task: a file header, a comment, or blank. Belongs
                // to no task, so it is not attached to one.
                _ = flush
                continue
            }
            if !indented && !trimmed.isEmpty {
                // A new top-level key, or a stray unindented line. Neither
                // belongs to this task, so the task ends here. Anchored on
                // indentation, which is what `block_field` in `loop_studio.rs` does and
                // why a `status:` nested under `metadata:` cannot hijack the
                // task's own status.
                flush()
                continue
            }
            // Indentation is load-bearing, and this is the same anchor Rust's
            // `block_field` uses. A key at the block's own indent is a field of
            // this task; a *deeper* one is a child of a nested mapping, so it is
            // carried verbatim. Without this, `metadata: { status: critical }`
            // was read as the task's own status and then silently dropped —
            // the nested decoy that `block_field`'s comment says "can never
            // hijack the match".
            let depth = line.prefix(while: { $0 == " " || $0 == "\t" }).count
            let key = firstKey(of: trimmed)
            // `metadata:` opens a nested mapping whose *direct* children are
            // still fields of this task — `module` is one, and the read model
            // has a property for it. So the two depths that can carry a field
            // are the task's own and `metadata`'s children; anything deeper, and
            // any key the model does not name, is carried verbatim.
            let fieldDepth = if insideMetadata { block.baseIndent + 2 } else { block.baseIndent }
            if depth == fieldDepth {
                let nested = insideMetadata
                // Only a `metadata:` line *opens* the nested level. Assigning
                // the flag from every field-depth line closed it again on the
                // first sibling, so `metadata: { status: x, module: y }` read
                // neither — the decoy was carried and so was the field next to
                // it.
                if key == "metadata" { insideMetadata = true }
                // A modelled key is only a field *at the level it belongs to*.
                // `module` lives under `metadata:`; `status` does not, so a
                // `status:` there is a decoy — and it is carried, not consumed
                // into the task's own status by a rule that only looked at
                // depth. Rust's `block_field` anchors on indent for exactly
                // this reason.
                if key == nil || !block.item.isField(key, nested: nested) {
                    block.extras.append(line)
                } else {
                    block.item.absorb(trimmed)
                }
            } else {
                if depth <= block.baseIndent {
                    insideMetadata = false
                }
                block.extras.append(line)
            }
            current = block
        }
        flush()
        return blocks
    }

    /// The key of a `key: value` line, or nil when the line is not one.
    nonisolated private static func firstKey(of trimmed: String) -> String? {
        guard let colon = trimmed.firstIndex(of: ":") else { return nil }
        let key = String(trimmed[trimmed.startIndex..<colon])
            .trimmingCharacters(in: .whitespaces)
        return key.isEmpty ? nil : key
    }

    /// Re-emit a whole queue, preserving each block's unmodelled lines.
    nonisolated static func serializeTaskBlocks(_ blocks: [TaskBlock], preamble: String) -> String {
        var out = preamble
        if !out.isEmpty && !out.hasSuffix("\n") { out += "\n" }
        for (index, block) in blocks.enumerated() {
            if index > 0 { out += "\n" }
            out += serializeTask(item: block.item)
            for line in block.extras {
                out += line + "\n"
            }
        }
        return out
    }

    /// The document header plus comments, verbatim, so a file's own
    /// provenance comment is not rewritten into ours on every save.
    nonisolated static func preamble(of text: String) -> String {
        var out: [String] = []
        for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = String(raw)
            if line.trimmingCharacters(in: .whitespaces).hasPrefix("- id:") { break }
            if line.first == " " || line.first == "\t" {
                // An indented line before the first task: not a header.
                continue
            }
            out.append(line)
        }
        while out.last?.trimmingCharacters(in: .whitespaces).isEmpty == true {
            out.removeLast()
        }
        return out.joined(separator: "\n")
    }

    /// One task, with its scalar values quoted safely.
    ///
    /// A double-quoted YAML scalar may not span lines and may not contain an
    /// unescaped `"`. The New Task field is a multi-line `TextEditor`, and the
    /// old writer emitted `task: "line1` / `line2"` — invalid YAML that Rust's
    /// line-based reader truncated to `line1`, so the two readers permanently
    /// disagreed about the same task and the app's own next write persisted the
    /// loss. Values are now emitted as single-quoted scalars, which is the only
    /// form in this project's line-oriented grammar that survives an embedded
    /// quote, a colon and a newline.
    nonisolated static func serializeTask(item: TaskItem) -> String {
        var out = "- id: \(yamlScalar(item.id))\n"
        out += "  task: \(yamlScalar(item.task))\n"
        out += "  priority: \(item.priority.rawValue)\n"
        out += "  status: \(item.status.rawValue)\n"
        out += "  loop: \(yamlScalar(item.loopName))\n"
        out += "  created: \(yamlScalar(item.created))\n"
        if let module = item.module {
            out += "  metadata:\n"
            out += "    module: \(yamlScalar(module))\n"
        }
        return out
    }

    /// A single-line YAML scalar: newlines become spaces, and the whole value
    /// is single-quoted with embedded quotes doubled.
    nonisolated static func yamlScalar(_ value: String) -> String {
        let flattened = value
            .replacingOccurrences(of: "\r\n", with: " ")
            .replacingOccurrences(of: "\n", with: " ")
            .replacingOccurrences(of: "\r", with: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return "'" + flattened.replacingOccurrences(of: "'", with: "''") + "'"
    }
}

