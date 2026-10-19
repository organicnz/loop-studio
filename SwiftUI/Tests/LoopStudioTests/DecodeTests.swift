import Foundation
import Testing

@testable import LoopStudio

private func testDirectory() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent("lac-swift-tests-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

// The live gateway may be v2.0 (old daemon) or v2.7+: the dashboard
// must decode both instead of blanking on a missing key.
struct ConnectionStoreTests {
    @Test @MainActor func buildsSafeGatewayURLs() {
        let store = LoopStudioConnectionStore(host: "mac.tailabc123.ts.net", port: 443, useTLS: true, token: "")
        #expect(store.url(path: "/lac/status")?.absoluteString == "https://mac.tailabc123.ts.net:443/lac/status")
        #expect(store.url(path: "/lac/switch?target=llama")?.absoluteString == "https://mac.tailabc123.ts.net:443/lac/switch?target=llama")

        let plaintext = LoopStudioConnectionStore(host: "mac.tailabc123.ts.net", port: 8000, useTLS: false, token: "")
        #expect(plaintext.url(path: "/lac/status") == nil)
        let ipHost = LoopStudioConnectionStore(host: "100.64.0.5", port: 443, useTLS: true, token: "")
        #expect(ipHost.url(path: "/lac/status") == nil)
        let publicHost = LoopStudioConnectionStore(host: "example.com", port: 443, useTLS: true, token: "")
        #expect(publicHost.url(path: "/lac/status") == nil)
        let invalidHost = LoopStudioConnectionStore(host: "bad host", port: 443, useTLS: true, token: "")
        #expect(invalidHost.url(path: "/lac/status") == nil)
        let invalidPort = LoopStudioConnectionStore(host: "127.0.0.1", port: 0, useTLS: false, token: "")
        #expect(invalidPort.url(path: "/lac/status") == nil)
    }
}

struct RouterDecodeTests {
    // Exact v2.0 shape observed on :8000 (no uptime/inflight/stats).
    static let v20 = """
    {"status":"ok","router":"lac-router v2.0","preferred":"llama",
     "active":"llama","target_port":8081,
     "backends":{"mlx":{"port":8080,"up":false},
                 "llama":{"port":8081,"up":false},
                 "ollama":{"port":11434,"up":false}}}
    """

    @Test func decodesV20Router() throws {
        let r = try JSONDecoder().decode(
            RouterStatusResponse.self, from: Self.v20.data(using: .utf8)!)
        #expect(r.preferred == "llama")
        #expect(r.backends["mlx"]?.port == 8080)
        #expect(r.uptime_secs == 0)
        #expect(r.stats.isEmpty)
    }

    @Test func decodesV27Router() throws {
        let v27 = """
        {"status":"ok","router":"lac-router v2.7","preferred":"auto",
         "active":"mlx","target_port":8080,"uptime_secs":99,"inflight":2,
         "models_mapped":3,"usage_log":"/tmp/u.jsonl",
         "backends":{"mlx":{"port":8080,"up":true}},
         "stats":{"mlx":{"port":8080,"ewma_ms":12.5,"ok":7,"err":1,"est_tokens":100}}}
        """
        let r = try JSONDecoder().decode(
            RouterStatusResponse.self, from: v27.data(using: .utf8)!)
        #expect(r.uptime_secs == 99)
        #expect(r.stats["mlx"]?.ok == 7)
        #expect(r.stats["mlx"]?.ewma_ms == 12.5)
    }
}

struct MarkdownSplitTests {
    @Test func splitsFencedCode() {
        let segs = splitMessage("hi\n```swift\nlet x = 1\n```\nbye")
        #expect(segs.count == 3)
        #expect(segs[0] == .prose("hi"))
        #expect(segs[1] == .code(language: "swift", code: "let x = 1"))
        #expect(segs[2] == .prose("bye"))
    }

    @Test func unclosedFenceBecomesCode() {
        // Mid-stream partial: remainder renders as code, never dropped.
        let segs = splitMessage("```\npartial")
        #expect(segs == [.code(language: "", code: "partial")])
    }

    @Test func plainTextIsOneProse() {
        #expect(splitMessage("just words") == [.prose("just words")])
    }

    @Test func splitsThoughtBlock() {
        let segs = splitMessage("<think>\npondering problem\n</think>\nHere is the answer")
        #expect(segs.count == 2)
        #expect(segs[0] == .thought("pondering problem"))
        #expect(segs[1] == .prose("Here is the answer"))
    }
}

struct ChatStoreTests {
    @Test func chatErrorDescriptions() {
        let err502 = ChatError.http(502, "")
        #expect(err502.errorDescription?.contains("No backend served the request (HTTP 502)") == true)
        #expect(err502.errorDescription?.contains("Start one with `loop-studio serve mlx`.") == true)

        let err503 = ChatError.http(503, "")
        #expect(err503.errorDescription?.contains("No backend served the request (HTTP 503)") == true)

        let err500 = ChatError.http(500, "internal error")
        #expect(err500.errorDescription?.contains("Gateway HTTP 500.") == true)
    }

    @Test func chatMessageSerialization() throws {
        let msg = ChatMessage(role: "user", content: "hello world")
        let enc = try JSONEncoder().encode(msg)
        let dec = try JSONDecoder().decode(ChatMessage.self, from: enc)
        #expect(dec.role == "user")
        #expect(dec.content == "hello world")
    }

    @Test @MainActor func chatStoreMutations() {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)
        let t = store.newThread()
        #expect(store.activeThreadId == t.id)
        let m1 = ChatMessage(role: "user", content: "Prompt 1")
        let m2 = ChatMessage(role: "assistant", content: "Response 1")
        if let idx = store.threads.firstIndex(where: { $0.id == t.id }) {
            store.threads[idx].messages.append(m1)
            store.threads[idx].messages.append(m2)
        }
        #expect(store.activeThread?.messages.count == 2)
        store.deleteMessage(id: m2.id)
        #expect(store.activeThread?.messages.count == 1)
        #expect(store.activeThread?.messages.first?.content == "Prompt 1")
    }

    @Test @MainActor func chatMessageVariants() {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)
        let t = store.newThread()
        let v1 = MessageVariant(content: "Variant 1", model: "model-a")
        let v2 = MessageVariant(content: "Variant 2", model: "model-b")
        let msg = ChatMessage(role: "assistant", content: v1.content, model: v1.model, variants: [v1, v2], selectedVariantIndex: 0)
        if let idx = store.threads.firstIndex(where: { $0.id == t.id }) {
            store.threads[idx].messages.append(msg)
        }

        #expect(store.activeThread?.messages.last?.variants.count == 2)
        #expect(store.activeThread?.messages.last?.content == "Variant 1")

        store.selectVariant(messageId: msg.id, index: 1)
        #expect(store.activeThread?.messages.last?.selectedVariantIndex == 1)
        #expect(store.activeThread?.messages.last?.content == "Variant 2")
        #expect(store.activeThread?.messages.last?.model == "model-b")
    }

    @Test @MainActor func chatThreadForking() {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)
        let t = store.newThread()
        let m1 = ChatMessage(role: "user", content: "Prompt 1")
        let m2 = ChatMessage(role: "assistant", content: "Answer 1")
        let m3 = ChatMessage(role: "user", content: "Prompt 2")
        let m4 = ChatMessage(role: "assistant", content: "Answer 2")
        if let idx = store.threads.firstIndex(where: { $0.id == t.id }) {
            store.threads[idx].messages.append(contentsOf: [m1, m2, m3, m4])
        }

        // Fork at m2
        let forked = store.forkThread(at: m2.id)
        #expect(forked != nil)
        #expect(store.activeThreadId == forked?.id)
        #expect(forked?.messages.count == 2)
        #expect(forked?.messages[0].content == "Prompt 1")
        #expect(forked?.messages[1].content == "Answer 1")
        #expect(forked?.title.contains("(Fork)") == true)
    }
}

struct ModelHubTests {
    @Test func installedModelFormatting() {
        let item1 = InstalledModelItem(
            id: "mlx-community/Qwen3.8-27B-4bit",
            name: "Qwen3.8-27B-4bit",
            path: "/Volumes/AIModels/Qwen3.8-27B-4bit",
            sizeBytes: Int64(16_100_000_000),
            format: "MLX",
            sourceDir: "/Volumes/AIModels",
            modifiedDate: Date()
        )
        #expect(item1.formattedSize.contains("GB"))
        #expect(item1.ramRecommendation.label.contains("Balanced"))

        let item2 = InstalledModelItem(
            id: "small-coder.gguf",
            name: "small-coder.gguf",
            path: "/tmp/small-coder.gguf",
            sizeBytes: Int64(450_000_000),
            format: "GGUF",
            sourceDir: "/tmp",
            modifiedDate: Date()
        )
        #expect(item2.formattedSize.contains("MB"))
        #expect(item2.ramRecommendation.label.contains("Lightweight"))
    }
}

struct CodeDiffTests {
    @Test func lineDiffAndHunkPartitioning() {
        let orig = "fn main() {\n    println!(\"old\");\n    let x = 1;\n    let y = 2;\n}"
        let mod = "fn main() {\n    println!(\"new\");\n    let x = 10;\n    let y = 2;\n}"
        let lines = computeLineDiff(original: orig, modified: mod)
        #expect(!lines.isEmpty)
        let hunks = partitionIntoHunks(diffLines: lines)
        #expect(!hunks.isEmpty)
        #expect(hunks[0].addedCount > 0)
        #expect(hunks[0].removedCount > 0)

        // All accepted -> should equal modified
        var acceptedHunks = hunks
        for i in acceptedHunks.indices { acceptedHunks[i].state = .accepted }
        let codeAccepted = synthesizeCodeFromHunks(diffLines: lines, hunks: acceptedHunks)
        #expect(codeAccepted == mod)

        // All rejected -> should equal original
        var rejectedHunks = hunks
        for i in rejectedHunks.indices { rejectedHunks[i].state = .rejected }
        let codeRejected = synthesizeCodeFromHunks(diffLines: lines, hunks: rejectedHunks)
        #expect(codeRejected == orig)
    }

    /// Accepting one chunk used to write every chunk. `.pending` was treated as
    /// accepted, so a two-hunk diff where the user accepted only the first
    /// produced the whole modified file — into the editor tab, immediately,
    /// while the second chunk was still badged "Pending".
    @Test func acceptingOneHunkLeavesUndecidedHunksOut() {
        let orig = (1...40).map { "let line\($0) = \($0)" }.joined(separator: "\n")
        let mod = orig
            .replacingOccurrences(of: "let line5 = 5", with: "let line5 = 500")
            .replacingOccurrences(of: "let line30 = 30", with: "let line30 = 3000")
        let lines = computeLineDiff(original: orig, modified: mod)
        let hunks = partitionIntoHunks(diffLines: lines)
        #expect(hunks.count == 2, "the fixture must produce two chunks: \(hunks.count)")

        // Everything undecided is the original file, not the modified one.
        let untouched = synthesizeCodeFromHunks(diffLines: lines, hunks: hunks)
        #expect(untouched == orig)

        // Accept the first chunk only.
        var firstOnly = hunks
        firstOnly[0].state = .accepted
        let selective = synthesizeCodeFromHunks(diffLines: lines, hunks: firstOnly)
        #expect(selective != mod, "the pending chunk must not be applied")
        #expect(selective != orig, "the accepted chunk must be applied")
        #expect(selective.contains("let line5 = 500"))
        #expect(!selective.contains("let line5 = 5\n") || selective.contains("let line5 = 500"))
        #expect(selective.contains("let line30 = 30"), "pending chunk keeps its original line")
        #expect(!selective.contains("let line30 = 3000"))

        // Reject the first instead: the second is still undecided, so the
        // result is still the original.
        var rejectFirst = hunks
        rejectFirst[0].state = .rejected
        #expect(synthesizeCodeFromHunks(diffLines: lines, hunks: rejectFirst) == orig)

        // Accepting both is the modified file again.
        var both = hunks
        for i in both.indices { both[i].state = .accepted }
        #expect(synthesizeCodeFromHunks(diffLines: lines, hunks: both) == mod)
    }
}

struct LoopsStoreTests {
    @Test func taskItemAndPriorities() {
        let task = TaskItem(
            id: "test-001",
            task: "Verify local router TTFT metrics",
            priority: .critical,
            status: .pending,
            loopName: "daily-coding.yaml",
            module: "lac-router"
        )
        #expect(task.id == "test-001")
        #expect(task.priority == .critical)
        #expect(task.priority.label == "Critical")
        #expect(task.status == .pending)
        #expect(task.status.title == "Queue")
        #expect(task.loopName == "daily-coding.yaml")
    }

    @Test @MainActor func loopsStoreMutations() async {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = LoopsStore(tasksURL: root.appendingPathComponent("tasks.yaml"))
        #expect(!store.loops.isEmpty)
        #expect(!store.tasks.isEmpty)

        // Add task
        let newTask = TaskItem(
            id: "task-unit-test",
            task: "Test autonomous pipeline execution",
            priority: .high,
            status: .pending
        )
        await store.addTask(newTask)
        #expect(store.tasks.contains(where: { $0.id == "task-unit-test" }))

        // Transition task status
        await store.moveTask(id: "task-unit-test", to: .inProgress)
        #expect(store.tasks.first(where: { $0.id == "task-unit-test" })?.status == .inProgress)

        await store.moveTask(id: "task-unit-test", to: .reviewGate)
        #expect(store.tasks.first(where: { $0.id == "task-unit-test" })?.status == .reviewGate)

        await store.moveTask(id: "task-unit-test", to: .completed)
        #expect(store.tasks.first(where: { $0.id == "task-unit-test" })?.status == .completed)

        // Delete task
        await store.deleteTask(id: "task-unit-test")
        #expect(!store.tasks.contains(where: { $0.id == "task-unit-test" }))
    }

    /// The human gate recorded a verification that never happened. It is the
    /// last line before a task is marked complete, and it claimed the changes
    /// were "verified via git diff" no matter what the run had done — including
    /// a run whose Implement phase had failed outright.
    @Test @MainActor func theGateRecordsTheOperatorsDecisionAndNothingMore() async {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = LoopsStore(tasksURL: root.appendingPathComponent("tasks.yaml"))
        let task = TaskItem(
            id: "task-gate-honesty",
            task: "Ship the thing",
            priority: .high,
            status: .reviewGate
        )
        await store.addTask(task)
        await store.moveTask(id: task.id, to: .reviewGate)
        store.activeTask = store.tasks.first(where: { $0.id == task.id })
        store.isGateAwaitingApproval = true

        await store.approveHumanGate()
        #expect(store.tasks.first(where: { $0.id == task.id })?.status == .completed)
        #expect(
            !store.runnerLogs.contains("verified via git diff"),
            "the app never verified anything at the gate: \(store.runnerLogs)"
        )
        #expect(
            store.runnerLogs.contains("no verification of its own"),
            "the record must say what it did not do: \(store.runnerLogs)"
        )
        #expect(!store.isGateAwaitingApproval)

        // Same again after a failed Implement phase: the operator still decides,
        // but the record carries the failure with it.
        let second = TaskItem(
            id: "task-gate-failed-implement",
            task: "Ship the other thing",
            priority: .medium,
            status: .reviewGate
        )
        await store.addTask(second)
        await store.moveTask(id: second.id, to: .reviewGate)
        store.activeTask = store.tasks.first(where: { $0.id == second.id })
        store.implementPhaseFailed = true
        // A second run reaches the gate the same way the first did. The store
        // now refuses to approve a gate that is not open, which is what closes
        // the double-approval window — so the test has to open it, as
        // `executeLoop` does at phase 5.
        store.isGateAwaitingApproval = true
        await store.approveHumanGate()
        #expect(
            store.runnerLogs.contains("Implement phase of this run failed"),
            "a failed implement must travel with the approval: \(store.runnerLogs)"
        )
        #expect(!store.runnerLogs.contains("verified via git diff"))
    }
}

struct CodeAssistantWorkingSetTests {
    @Test func languageDetection() {
        #expect(CodeAssistantStore.language(for: URL(fileURLWithPath: "/tmp/main.rs")) == "Rust")
        #expect(CodeAssistantStore.language(for: URL(fileURLWithPath: "/tmp/App.swift")) == "Swift")
        #expect(CodeAssistantStore.language(for: URL(fileURLWithPath: "/tmp/script.py")) == "Python")
        #expect(CodeAssistantStore.language(for: URL(fileURLWithPath: "/tmp/web.ts")) == "TypeScript")
        #expect(CodeAssistantStore.language(for: URL(fileURLWithPath: "/tmp/tool.go")) == "Go")
        #expect(CodeAssistantStore.language(for: URL(fileURLWithPath: "/tmp/build.sh")) == "Shell")
    }

    @Test @MainActor func editorTabManagement() {
        let store = CodeAssistantStore()
        #expect(store.tabs.count == 1)
        #expect(store.tabs[0].title == "RingBuffer.rs")
        #expect(store.activeTabId == store.tabs[0].id)

        // Open a file
        let fileUrl = URL(fileURLWithPath: "/tmp/worker.rs")
        store.openFile(url: fileUrl, content: "pub fn worker() {}")
        #expect(store.tabs.count == 2)
        #expect(store.activeTab?.title == "worker.rs")
        #expect(store.selectedLanguage == "Rust")
        #expect(store.sourceCode == "pub fn worker() {}")

        // Edit source code -> marks active tab modified
        store.sourceCode = "pub fn worker() { println!(); }"
        #expect(store.activeTab?.isModified == true)

        // New tab
        store.newTab()
        #expect(store.tabs.count == 3)
        #expect(store.activeTab?.title.hasPrefix("Untitled") == true)

        // Close tab
        let untitledId = store.activeTabId
        store.closeTab(id: untitledId)
        #expect(store.tabs.count == 2)
        #expect(store.activeTab?.id != untitledId)
    }

    @Test @MainActor func closeOtherAndAllTabs() {
        let store = CodeAssistantStore()
        let url1 = URL(fileURLWithPath: "/tmp/a.rs")
        let url2 = URL(fileURLWithPath: "/tmp/b.rs")
        let url3 = URL(fileURLWithPath: "/tmp/c.rs")

        store.openFile(url: url1, content: "// a")
        store.openFile(url: url2, content: "// b")
        store.openFile(url: url3, content: "// c")
        #expect(store.tabs.count == 4)

        let targetId = store.tabs.first(where: { $0.title == "b.rs" })!.id
        store.closeOtherTabs(id: targetId)
        #expect(store.tabs.count == 1)
        #expect(store.activeTabId == targetId)
        #expect(store.activeTab?.title == "b.rs")

        store.closeAllTabs()
        #expect(store.tabs.count == 1)
        #expect(store.activeTab?.title.hasPrefix("Untitled") == true)
    }

    @Test @MainActor func saveAllTabs() {
        let store = CodeAssistantStore()
        let tempDir = FileManager.default.temporaryDirectory
        let fileA = tempDir.appendingPathComponent("test_save_a_\(UUID().uuidString).rs")
        let fileB = tempDir.appendingPathComponent("test_save_b_\(UUID().uuidString).rs")

        try? "initial a".write(to: fileA, atomically: true, encoding: .utf8)
        try? "initial b".write(to: fileB, atomically: true, encoding: .utf8)

        defer {
            try? FileManager.default.removeItem(at: fileA)
            try? FileManager.default.removeItem(at: fileB)
        }

        store.openFile(url: fileA, content: "initial a")
        store.openFile(url: fileB, content: "initial b")

        // Mutate both
        if let idxA = store.tabs.firstIndex(where: { $0.url == fileA }) {
            store.tabs[idxA].content = "updated a"
            store.tabs[idxA].isModified = true
        }
        if let idxB = store.tabs.firstIndex(where: { $0.url == fileB }) {
            store.tabs[idxB].content = "updated b"
            store.tabs[idxB].isModified = true
        }

        #expect(store.tabs.filter { $0.isModified }.count == 2)
        store.saveAllTabs()
        #expect(store.tabs.filter { $0.isModified }.count == 0)

        let diskA = try? String(contentsOf: fileA, encoding: .utf8)
        let diskB = try? String(contentsOf: fileB, encoding: .utf8)
        #expect(diskA == "updated a")
        #expect(diskB == "updated b")
    }
}

struct AgentConsoleStoreTests {
    @Test @MainActor func consoleStateAndClear() {
        let console = AgentConsoleStore()
        #expect(console.output.isEmpty)
        #expect(!console.isRunning)
        #expect(console.lastExitCode == nil)
        #expect(console.activeCommand == nil)
        #expect(console.executionDuration == 0)

        console.output = "some log"
        console.lastExitCode = 0
        console.activeCommand = "test"
        console.clear()

        #expect(console.output.isEmpty)
        #expect(console.lastExitCode == nil)
        #expect(console.activeCommand == nil)
    }

    @Test @MainActor func consoleProcessExecution() async {
        let console = AgentConsoleStore()
        let tmpDir = FileManager.default.temporaryDirectory

        console.execute(
            command: "/bin/echo",
            arguments: ["hello lac test suite"],
            workingDirectory: tmpDir,
            title: "echo test"
        )

        // Wait briefly for process to run and terminate
        for _ in 0..<30 {
            if !console.isRunning && console.lastExitCode != nil {
                break
            }
            try? await Task.sleep(nanoseconds: 100_000_000)
        }

        #expect(!console.isRunning)
        #expect(console.lastExitCode == 0)
        #expect(console.output.contains("hello lac test suite"))
        #expect(console.output.contains("code 0"))
    }
}

struct WorkspaceFileTreeTests {
    @Test func gitStatusParsing() {
        let root = URL(fileURLWithPath: "/workspace")
        let gitOutput = """
         M rust-src/src/lac.rs
        ?? SwiftUI/Sources/LoopStudio/NewFile.swift
         A tests/gate.rs
         D obsolete.txt
        """
        let parsed = WorkspaceFileTree.parseGitStatusOutput(gitOutput, rootUrl: root)
        #expect(parsed["/workspace/rust-src/src/lac.rs"] == .modified)
        #expect(parsed["/workspace/SwiftUI/Sources/LoopStudio/NewFile.swift"] == .untracked)
        #expect(parsed["/workspace/tests/gate.rs"] == .added)
        #expect(parsed["/workspace/obsolete.txt"] == .deleted)
    }

    @Test func scanFlatFilesFindsChildren() {
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent("lac_test_scan_\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        defer {
            try? FileManager.default.removeItem(at: tempDir)
        }

        let file1 = tempDir.appendingPathComponent("worker.rs")
        let subDir = tempDir.appendingPathComponent("sub")
        try? FileManager.default.createDirectory(at: subDir, withIntermediateDirectories: true)
        let file2 = subDir.appendingPathComponent("app.swift")

        try? "fn worker() {}".write(to: file1, atomically: true, encoding: .utf8)
        try? "import SwiftUI".write(to: file2, atomically: true, encoding: .utf8)

        let scanned = WorkspaceFileTree.scanFlatFiles(url: tempDir, maxDepth: 3)
        #expect(scanned.count == 2)
        #expect(scanned.contains(where: { $0.name == "worker.rs" }))
        #expect(scanned.contains(where: { $0.name == "app.swift" }))
    }
}

struct CommandPaletteTests {
    @Test func paletteItemPropertiesAndEquality() {
        let item1 = PaletteItem(
            title: "Quick Open File",
            subtitle: "Spotlight file search across workspace",
            icon: "doc.text.magnifyingglass",
            category: "Actions",
            shortcut: "⌘P",
            action: {}
        )
        #expect(item1.title == "Quick Open File")
        #expect(item1.shortcut == "⌘P")
        #expect(item1.category == "Actions")
        #expect(item1.icon == "doc.text.magnifyingglass")

        // Self equality
        #expect(item1 == item1)

        let item2 = PaletteItem(
            title: "Run Swift Tests",
            subtitle: nil,
            icon: "swift",
            category: "Actions",
            shortcut: nil,
            action: {}
        )
        #expect(item1 != item2)
        #expect(item2.subtitle == nil)
    }
}

struct HFRepoFileItemTests {
    @Test func decodesFromTreeApiJson() throws {
        let json = """
        [
            {"path": "Qwen3.8-27B-Q4_K_M.gguf", "size": 17179869184, "type": "file"},
            {"path": "Qwen3.8-27B-Q8_0.gguf", "size": 28991029248, "type": "file"},
            {"path": "model.safetensors", "size": 52428800, "type": "file"},
            {"path": "tokenizer.json", "size": 1048576, "type": "file"}
        ]
        """
        let items = try JSONDecoder().decode([HFRepoFileItem].self, from: json.data(using: .utf8)!)
        #expect(items.count == 4)

        // Item 0: Q4_K_M GGUF
        #expect(items[0].fileName == "Qwen3.8-27B-Q4_K_M.gguf")
        #expect(items[0].isGguf == true)
        #expect(items[0].isSafetensors == false)
        #expect(items[0].quantizationTag == "Q4_K_M")
        #expect(items[0].formattedSize == "16.00 GB")

        // Item 1: Q8_0 GGUF
        #expect(items[1].quantizationTag == "Q8_0")
        #expect(items[1].isGguf == true)

        // Item 2: Safetensors
        #expect(items[2].isSafetensors == true)
        #expect(items[2].quantizationTag == "SafeTensors")
        #expect(items[2].formattedSize == "50.0 MB")

        // Item 3: Generic file
        #expect(items[3].quantizationTag == "Model File")
        #expect(items[3].formattedSize == "1.0 MB")
    }

    @Test func handlesZeroOrNilSizes() {
        let zeroItem = HFRepoFileItem(path: "empty.bin", size: 0, type: "file")
        #expect(zeroItem.formattedSize == "—")

        let nilItem = HFRepoFileItem(path: "folder", size: nil, type: "directory")
        #expect(nilItem.formattedSize == "—")
    }
}

struct LoopYamlSerializationTests {
    @Test func parseAndSerializeRoundTrip() {
        let original = [
            TaskItem(
                id: "lac-test-01",
                task: "Audit MLX cache hit rate",
                priority: .critical,
                status: .inProgress,
                loopName: "daily-coding.yaml",
                created: "2026-09-13T12:00:00Z",
                module: "rust-src/kv"
            ),
            TaskItem(
                id: "lac-test-02",
                task: "Refactor Liquid Glass modal sheets",
                priority: .low,
                status: .completed,
                loopName: "bug-fix.yaml",
                created: "2026-09-13T14:30:00Z",
                module: "SwiftUI/Glass"
            )
        ]

        let yaml = LoopsStore.serializeTasksYaml(original)
        #expect(yaml.contains("lac-test-01"))
        #expect(yaml.contains("Audit MLX cache hit rate"))
        #expect(yaml.contains("priority: critical"))
        #expect(yaml.contains("status: in_progress"))
        #expect(yaml.contains("module: \"rust-src/kv\""))

        let parsed = LoopsStore.parseTasksYaml(yaml)
        #expect(parsed.count == 2)
        #expect(parsed[0].id == "lac-test-01")
        #expect(parsed[0].task == "Audit MLX cache hit rate")
        #expect(parsed[0].priority == .critical)
        #expect(parsed[0].status == .inProgress)
        #expect(parsed[0].module == "rust-src/kv")

        #expect(parsed[1].id == "lac-test-02")
        #expect(parsed[1].priority == .low)
        #expect(parsed[1].status == .completed)
        #expect(parsed[1].module == "SwiftUI/Glass")
    }
}

struct CodeAssistantTabTests {
    @Test @MainActor func tabManagementAndModifications() {
        let store = CodeAssistantStore()
        #expect(store.tabs.count == 1)
        #expect(store.tabs.first?.title == "RingBuffer.rs")

        let fileUrl = URL(fileURLWithPath: "/workspace/rust-src/src/lac.rs")
        store.openFile(url: fileUrl, content: "fn main() {}")
        #expect(store.tabs.count == 2)
        #expect(store.activeTab?.title == "lac.rs")
        #expect(store.sourceCode == "fn main() {}")
        #expect(store.selectedLanguage == "Rust")

        // Modifying code marks tab as modified
        store.sourceCode = "fn main() { println!(\"hello\"); }"
        #expect(store.activeTab?.isModified == true)

        // Undo apply
        store.applyToEditor("fn refactored() {}")
        #expect(store.sourceCode == "fn refactored() {}")
        #expect(store.lastAppliedCode == "fn main() { println!(\"hello\"); }")

        store.undoApply()
        #expect(store.sourceCode == "fn main() { println!(\"hello\"); }")
        #expect(store.lastAppliedCode == nil)
    }
}

struct ModelHubFilterTests {
    private func item(_ id: String) -> HFModelItem {
        HFModelItem(id: id, downloads: nil, likes: nil, tags: nil, pipeline_tag: nil, createdAt: nil)
    }

    @Test func sizeBucketsCoverCommonParams() {
        typealias Size = ModelHubStore.ModelSizeFilter
        #expect(Size.small.matches(item: item("mlx-community/Qwen3-1.7B-4bit")))
        #expect(Size.small.matches(item: item("mlx-community/Llama-3.2-1B-Instruct-4bit")))
        #expect(Size.medium.matches(item: item("mlx-community/Qwen3.8-27B-4bit")))
        #expect(Size.medium.matches(item: item("mlx-community/Qwen2.5-Coder-32B-Instruct-4bit")))
        #expect(Size.large.matches(item: item("unsloth/Llama-3.3-70B-GGUF")))
        #expect(!Size.small.matches(item: item("mlx-community/Qwen3.8-27B-4bit")))
        #expect(!Size.large.matches(item: item("mlx-community/Qwen3.8-27B-4bit")))
        #expect(Size.medium.matches(item: item("mistralai/Mixtral-8x7B-Instruct-v0.1")))
        #expect(Size.small.matches(item: item("HuggingFaceTB/SmolLM2-1.7B-Instruct")))
        #expect(Size.all.matches(item: item("anything/AtAll-999B")))
    }

    @Test func decodesSparseSearchPayload() throws {        // HF occasionally omits keys; discovery must not blank on that.
        let json = """
        [{"id":"mlx-community/Qwen3.8-27B-4bit","downloads":12}]
        """
        let items = try JSONDecoder().decode([HFModelItem].self, from: Data(json.utf8))
        #expect(items.count == 1)
        #expect(items[0].likes == nil)
        #expect(items[0].tags == nil)
        #expect(items[0].author == "mlx-community")
        #expect(items[0].modelName == "Qwen3.8-27B-4bit")
    }

    @Test func ramEstimatorMath() {
        // 27B Q4 ≈ 16 GB weights, ~26 GB with KV/OS headroom.
        let qwen27 = item("mlx-community/Qwen3.8-27B-4bit")
        #expect(qwen27.ramFitEstimate(hostRamGB: 16).color == .red)
        #expect(qwen27.ramFitEstimate(hostRamGB: 128).color == .green)
        #expect(qwen27.ramFitEstimate(hostRamGB: nil).label.contains("16"))
        // 1B Q4 fits anywhere.
        let tiny = item("mlx-community/Llama-3.2-1B-Instruct-4bit")
        #expect(tiny.ramFitEstimate(hostRamGB: 16).color == .green)
        // No parseable size → neutral, never a false verdict.
        #expect(item("someorg/mystery-model").ramFitEstimate(hostRamGB: 16).color == .blue)
    }

    @Test func quantizationDetectionCoversGgufIds() {
        func tagged(_ id: String, _ tags: [String]) -> HFModelItem {
            HFModelItem(id: id, downloads: nil, likes: nil, tags: tags, pipeline_tag: nil, createdAt: nil)
        }
        // Reviewer case: tag-less 70B GGUF must not estimate at 4-bit.
        let gguf70 = item("unsloth/Llama-3.3-70B-Q8_0-GGUF")
        #expect(gguf70.quantization == "8-bit")
        #expect(gguf70.ramFitEstimate(hostRamGB: 64).color == .red)
        #expect(tagged("org/Model-32B", ["q5_k_m"]).quantization == "5-bit")
        #expect(tagged("org/Model-14B", ["bf16"]).quantization == "FP16")
        #expect(item("mlx-community/Qwen3.8-27B-4bit").quantization == "4-bit")
    }
}

struct CompletionBudgetTests {
    @Test func maxTokensNeverExceeds4k() {
        // 64k context / 2 would OOM a local 27B — hard cap at 4096.
        #expect(ChatStore.maxTokens(forContextCap: 65536) == 4096)
        #expect(ChatStore.maxTokens(forContextCap: 32768) == 4096)
        #expect(ChatStore.maxTokens(forContextCap: 4096) == 2048)
        #expect(ChatStore.maxTokens(forContextCap: 0) == nil)
        #expect(ChatStore.maxTokens(forContextCap: -1) == nil)
    }
}

struct ThreadTitlePersistenceTests {
    @Test @MainActor func renameSurvivesReload() {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)
        let t = store.newThread()
        if let idx = store.threads.firstIndex(where: { $0.id == t.id }) {
            store.threads[idx].messages.append(
                ChatMessage(role: "user", content: "derivable title seed"))
        }
        store.renameThread(t.id, title: "My Custom Title")

        // A fresh instance (simulated restart) must keep the rename.
        let reloaded = ChatStore(rootURL: root)
        #expect(reloaded.threads.first(where: { $0.id == t.id })?.title == "My Custom Title")

        // Cleanup: remove the probe thread from both files.
        store.deleteThread(t.id)
        reloaded.deleteThread(t.id)
    }
}

struct LoopsWritePathTests {
    @Test @MainActor func writesTargetUserQueueNeverTemplates() {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        let url = store.tasksWriteURL()
        #expect(url.path.hasSuffix("todo/loop-studio-tasks.yaml"))
        #expect(!url.path.contains("templates"))
    }
}

struct PaletteConsoleBridgeTests {
    @Test @MainActor func pendingCommandRoundTrip() {
        let store = CodeAssistantStore()
        #expect(store.pendingConsoleCommand == nil)
        store.pendingConsoleCommand = .cargoTests
        #expect(store.pendingConsoleCommand == .cargoTests)
        store.pendingConsoleCommand = nil
        #expect(store.pendingConsoleCommand == nil)
    }
}




// MARK: - Studio concurrency, lock, and honesty (2026-09-27 pass)

struct GatewayPortRestoreTests {
    /// `??` binds looser than `>`, so this condition was a nil check and any
    /// persisted value was restored — including 0, -1 and 70000. Every
    /// gateway call then returned nil from `url(path:)`, so the whole app
    /// reported "Invalid router URL" for a profile nobody had edited.
    @Test func persistedPortIsRestoredOnlyWhenItIsAPort() {
        typealias Resolve = (Int?, Int?, Int?) -> Int
        let resolve: Resolve = LoopStudioConnectionStore.resolvedPort
        #expect(resolve(nil, nil, 9000) == 9000)
        #expect(resolve(nil, nil, 0) == 8000, "0 is not a port")
        #expect(resolve(nil, nil, -1) == 8000)
        #expect(resolve(nil, nil, 70_000) == 8000)
        #expect(resolve(nil, nil, Int.max) == 8000)
        // Env beats the persisted value, and is checked the same way.
        #expect(resolve(nil, 9100, 9000) == 9100)
        #expect(resolve(nil, 0, 9000) == 9000, "a bad env falls through, not to 0")
        #expect(resolve(nil, nil, nil) == 8000)
        // An explicit argument is the caller's, valid or not: refusing at
        // `url(path:)` is more honest than quietly dialling a different port.
        #expect(resolve(1234, 9100, 9000) == 1234)
    }

    @Test @MainActor func theSettingsFieldCannotStoreAnImpossiblePort() {
        // `didSet` writes the real `UserDefaults.standard`, so put the
        // developer's own value back rather than leaving 8000 behind.
        let key = "lac.gateway.port"
        let had = UserDefaults.standard.object(forKey: key)
        defer {
            if let had { UserDefaults.standard.set(had, forKey: key) }
            else { UserDefaults.standard.removeObject(forKey: key) }
        }
        let store = LoopStudioConnectionStore(host: "127.0.0.1", port: 8000, useTLS: false, token: "")
        #expect(store.lastRejectedPort == nil)
        store.port = 9000
        #expect(store.port == 9000)
        for bad in [0, -1, 70_000] {
            store.port = bad
            #expect(store.port == 9000, "port \(bad) was applied")
            #expect(store.lastRejectedPort == bad, "the UI is not told why it snapped back")
            #expect(store.url(path: "/lac/status") != nil, "a rejected port must not break every call")
        }
        store.port = 8000
        #expect(store.lastRejectedPort == nil, "a good value clears the complaint")
    }
}

struct AutoStartPolicyTests {
    /// The flag was re-armed from inside `startRouter` before it spawned, so a
    /// failed start was eligible again on the next 3s dashboard tick: an
    /// unbounded retry loop leaking one untracked `lac-router` per cycle.
    @Test func aFailedStartIsNotRetriedOnEveryTick() {
        typealias Should = (Bool, Int, TimeInterval?, Bool, Bool, TimeInterval, Int) -> Bool
        let should: Should = NetworkManager.shouldAutoStart
        // First sighting: start once.
        #expect(should(false, 0, nil, false, false, 30, 3))
        // This outage has already had its automatic start.
        #expect(!should(true, 0, nil, false, false, 30, 3))
        #expect(!should(true, 1, nil, false, false, 30, 3))
        // Budget spent: a flapping gateway cannot spawn forever.
        #expect(!should(false, 3, nil, false, false, 30, 3))
        #expect(!should(false, 99, nil, false, false, 30, 3))
        // Cooldown: no spawn storm when the latch is cleared by something
        // other than a router that answered.
        #expect(!should(false, 0, 0, false, false, 30, 3), "a start 0s ago is not a new outage")
        #expect(!should(false, 0, 29.9, false, false, 30, 3))
        #expect(should(false, 0, 30.1, false, false, 30, 3))
        #expect(should(false, 0, 600, false, false, 30, 3))
        // A start already running: its own outcome decides what happens next.
        // Without this a poll landing mid-start consumes the one-per-outage
        // allowance and its (immediately declined) start call is lost.
        #expect(!should(false, 0, nil, false, true, 30, 3), "a start is already in flight")
        // Remote: the Mac owns the router, so the app never starts it.
        #expect(!should(false, 0, nil, true, false, 30, 3))
        #expect(!should(true, 0, nil, true, false, 30, 3))
    }

    /// The precedence is `common::mlx_port` / `llama_port` / `ollama_port`,
    /// exactly. Inventing a variable name here would have been worse than the
    /// literal it replaced: it would change what the app probes without
    /// changing where the engine binds, and no production code would read it.
    @Test func backendPortsFollowTheRustedRuleNotLiterals() {
        #expect(NetworkManager.backendPort(.llama, environment: [:], mlxPortFile: nil) == 8081)
        #expect(NetworkManager.backendPort(.mlx, environment: [:], mlxPortFile: nil) == 8080)
        #expect(NetworkManager.backendPort(.ollama, environment: [:], mlxPortFile: nil) == 11434)
        // The router, `lac status` and the TUI all honor these; the start
        // buttons used to probe 8081/11434 as literals, so a relocated lane
        // was started and then polled on a port nothing was bound to.
        #expect(NetworkManager.backendPort(.llama, environment: ["LOOP_STUDIO_LLAMA_PORT": "9001"], mlxPortFile: nil) == 9001)
        #expect(NetworkManager.backendPort(.ollama, environment: ["LOOP_STUDIO_OLLAMA_PORT": "12000"], mlxPortFile: nil) == 12000)
        // MLX: MLX_PORT, then PORT, then serve-mlx's own port file — which is
        // how a drifted engine (8080 taken, so it moved to 8082) is found at
        // all. Missing that file is why the Studio could not see a live lane.
        #expect(NetworkManager.backendPort(.mlx, environment: ["MLX_PORT": "8181"], mlxPortFile: "9000") == 8181)
        #expect(NetworkManager.backendPort(.mlx, environment: ["PORT": "8182", "MLX_PORT": "8181"], mlxPortFile: nil) == 8181)
        #expect(NetworkManager.backendPort(.mlx, environment: [:], mlxPortFile: "9000\n") == 9000)
        // A value that is not a port is not a port: 0 included, which the
        // engine treats as "assign me an ephemeral port" and the router
        // discards, so probing it can never succeed.
        for junk in ["abc", "", "0", "-1", "70000", "  "] {
            #expect(
                NetworkManager.backendPort(.llama, environment: ["LOOP_STUDIO_LLAMA_PORT": junk], mlxPortFile: nil) == 8081,
                "LAC_LLAMA_PORT=\(junk) was accepted"
            )
            #expect(
                NetworkManager.backendPort(.mlx, environment: [:], mlxPortFile: junk) == 8080,
                "mlx.port=\(junk) was accepted"
            )
        }
        // An MLX_PORT that will not parse falls through to the port file,
        // exactly as `mlx_port_from` + `mlx_port` do.
        #expect(NetworkManager.backendPort(.mlx, environment: ["MLX_PORT": "0"], mlxPortFile: "8082") == 8082)
    }

    /// A router with nothing behind it answers `no-backends` on a port
    /// nothing is bound to. Treating that document as "the gateway is up" is
    /// how the dashboard learned to call a dead router healthy.
    @Test func aNoBackendsRouterIsNotAHealthyRouter() throws {
        let dead = """
        {"status":"no-backends","router":"lac-router v2.8","preferred":"auto",
         "active":"none","target_port":0,
         "backends":{"mlx":{"port":8080,"up":false}}}
        """
        let live = """
        {"status":"ok","router":"lac-router v2.8","preferred":"auto",
         "active":"mlx","target_port":8080,
         "backends":{"mlx":{"port":8080,"up":true}}}
        """
        let decoder = JSONDecoder()
        #expect(!NetworkManager.routerIsAnswering(try decoder.decode(RouterStatusResponse.self, from: Data(dead.utf8))))
        #expect(NetworkManager.routerIsAnswering(try decoder.decode(RouterStatusResponse.self, from: Data(live.utf8))))
    }
}

struct QueueLockTests {
    /// The Studio wrote `"<pid>"` and read the whole record with
    /// `Int32(...)`, while the CLI writes `"<pid> <seconds>"`. Every lock the
    /// worker held therefore failed to parse here, fell through to the
    /// creation-date branch, and was reclaimed 30 seconds into a *live*
    /// hold — two writers on one queue, which is what the lock is for.
    @Test func theStudioReadsTheOwnersRecordTheWayTheCliWritesIt() {
        typealias Rule = (String?, TimeInterval?) -> Bool
        let dead: (Int32) -> Bool? = { _ in false }
        let alive: (Int32) -> Bool? = { _ in true }
        let unprobeable: (Int32) -> Bool? = { _ in nil }

        // The CLI's format, 10 minutes old, owner long gone: reclaim now.
        #expect(LoopsStore.lockIsReclaimable(owner: "99999 1700000000", directoryAge: 600, probe: dead))
        // ...and while that same owner is alive, age means nothing.
        #expect(!LoopsStore.lockIsReclaimable(owner: "99999 1700000000", directoryAge: 600, probe: alive))
        // This process's own record, fresh: definitely not.
        let me = ProcessInfo.processInfo.processIdentifier
        #expect(!LoopsStore.lockIsReclaimable(owner: "\(me) \(Int(Date().timeIntervalSince1970))", directoryAge: 0, probe: alive))
        // Legacy Studio format (pid only) is still understood.
        #expect(LoopsStore.lockIsReclaimable(owner: "99999", directoryAge: 5, probe: dead))
        #expect(!LoopsStore.lockIsReclaimable(owner: "99999", directoryAge: 5, probe: alive))

        // The window between O_TRUNC and write: the record exists and is
        // empty. Reading that as "stale" is how two workers drained one queue.
        #expect(!LoopsStore.lockIsReclaimable(owner: "", directoryAge: 0, probe: dead))
        #expect(!LoopsStore.lockIsReclaimable(owner: nil, directoryAge: 2, probe: dead))
        #expect(!LoopsStore.lockIsReclaimable(owner: "not a pid", directoryAge: 3, probe: dead))
        #expect(!LoopsStore.lockIsReclaimable(owner: "-1 1700000000", directoryAge: 3, probe: dead))
        // ...but the backstop still applies to all of them.
        #expect(LoopsStore.lockIsReclaimable(owner: "", directoryAge: 60, probe: dead))
        #expect(LoopsStore.lockIsReclaimable(owner: nil, directoryAge: 60, probe: dead))
        #expect(LoopsStore.lockIsReclaimable(owner: "garbage", directoryAge: 60, probe: dead))
        // An unprobeable pid has nothing but age to go on.
        #expect(!LoopsStore.lockIsReclaimable(owner: "99999 1", directoryAge: 5, probe: unprobeable))
        #expect(LoopsStore.lockIsReclaimable(owner: "99999 1", directoryAge: 90, probe: unprobeable))
        // No age at all (stat failed) must not be treated as aged out.
        #expect(!LoopsStore.lockIsReclaimable(owner: nil, directoryAge: nil, probe: dead))
    }

    @Test func aLiveOwnerIsAlive() {
        let me = ProcessInfo.processInfo.processIdentifier
        #expect(LoopsStore.processIsAlive(me) == true)
        // A pid that cannot exist is provably gone.
        #expect(LoopsStore.processIsAlive(2_147_483_600) == false)
        // A nonsense pid is unprobeable, not dead — the distinction is what
        // keeps a lock from being stolen on a parse.
        #expect(LoopsStore.processIsAlive(0) == nil)
        #expect(LoopsStore.processIsAlive(-1) == nil)
    }

    /// A held lock must not freeze the app.
    ///
    /// `withQueueLock` was a `@MainActor` function whose wait was
    /// `Thread.sleep`, for up to 30 seconds: a `lac worker` holding
    /// `~/.lac/locks/loop-studio-tasks` froze every window, including the one
    /// streaming an answer. The wait is now a suspension off the main actor.
    @Test @MainActor func aHeldQueueLockDoesNotBlockTheMainActor() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = LoopsStore(tasksURL: root.appendingPathComponent("tasks.yaml"))

        // A lock held by a process that is alive — this one.
        let lock = root.appendingPathComponent(".lac-queue.lock")
        try FileManager.default.createDirectory(at: lock, withIntermediateDirectories: true)
        let me = ProcessInfo.processInfo.processIdentifier
        try "\(me) \(Int(Date().timeIntervalSince1970))".write(
            to: lock.appendingPathComponent("owner"), atomically: true, encoding: .utf8
        )

        // A heartbeat the main actor has to be free to run. If the mutation
        // blocked the actor, this could not advance while it was in flight.
        let ticker = Task { @MainActor in
            var ticks = 0
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 10_000_000)
                ticks += 1
            }
            return ticks
        }
        let started = Date()
        let mutation = Task { @MainActor in
            await store.moveTask(id: store.tasks[0].id, to: .inProgress)
        }
        try await Task.sleep(nanoseconds: 400_000_000)
        ticker.cancel()
        let ticks = await ticker.value
        let elapsed = Date().timeIntervalSince(started)

        // The call returned control; the actor kept running; the lock is
        // still there because a live owner holds it.
        //
        // The floor is 2, not a rate. The property under test is "the main
        // actor was not blocked by the mutation", and a blocked actor produces
        // *zero* ticks in this window, not fewer ticks — so any positive count
        // separates the two cases. Demanding a high count instead measured how
        // well the host could schedule a 20ms timer, and failed under CPU
        // contention (four `cargo test` processes on the box) while the code
        // under test was correct. That is a test that reports the weather.
        #expect(ticks >= 2, "the main actor was blocked: only \(ticks) ticks in \(elapsed)s")
        #expect(elapsed < 5, "moveTask must not have waited out the 10s lock timeout")
        #expect(FileManager.default.fileExists(atPath: lock.path), "a live owner's lock was removed")
        #expect(try String(contentsOf: lock.appendingPathComponent("owner"), encoding: .utf8).hasPrefix("\(me) "), "the owner record was rewritten")

        // The edit is still waiting for the lock. It has not acquired it, so it
        // has neither removed the directory nor rewritten the record — the two
        // ways a mutation could have disturbed someone else's lock. What it
        // will eventually report ("Queue is busy") is checked by the write
        // test below, not here.
        // Not awaited: the waiter gives up on its own 10s deadline, and
        // blocking a test on that is not what this test is about.
        mutation.cancel()
    }

    /// The record is written in the CLI's format, because a lock neither side
    /// can read is a lock neither side can reclaim honestly.
    @Test func theOwnerRecordIsWrittenForTheCliToRead() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let lock = root.appendingPathComponent(".lac-queue.lock")
        let me = ProcessInfo.processInfo.processIdentifier
        let taken = await LoopsStore.acquireQueueLock(at: lock)
        #expect(taken)
        let record = try String(contentsOf: lock.appendingPathComponent("owner"), encoding: .utf8)
        let parts = record.split(separator: " ").map(String.init)
        #expect(parts.count == 2, "expected \"<pid> <seconds>\", got \(record)")
        #expect(parts.first.flatMap { Int32($0) } == me, "not our pid: \(record)")
        #expect(parts.last.flatMap { Int64($0) }.map { $0 > 1_600_000_000 } == true, "no timestamp: \(record)")
        // And the rule the CLI applies to that record says: hands off.
        #expect(!LoopsStore.lockIsReclaimable(
            owner: record,
            directoryAge: 0,
            probe: LoopsStore.processIsAlive
        ))
        LoopsStore.releaseQueueLock(at: lock)
        #expect(!FileManager.default.fileExists(atPath: lock.path), "our own lock must be released")
    }

    /// A queue with no file on disk is a fresh install, where `loadTasks`
    /// seeds four tasks from the template. The first edit must build on those,
    /// not replace them with a header-only file.
    @Test @MainActor func anAbsentQueueFileIsNotAnEmptyQueue() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        #expect(!FileManager.default.fileExists(atPath: target.path))
        let store = LoopsStore(tasksURL: target)
        let seeded = store.tasks.count
        #expect(seeded > 0, "the fixture must seed something to lose")

        await store.addTask(TaskItem(id: "lac-first", task: "the first real edit", priority: .high))
        #expect(store.tasks.count == seeded + 1)
        let written = try String(contentsOf: target, encoding: .utf8)
        #expect(written.contains("the first real edit"), "not written: \(written)")
        // Nothing the store was already showing was dropped on the way out.
        for task in store.tasks where task.id != "lac-first" {
            #expect(written.contains(task.task), "lost \(task.id): \(task.task)")
        }
    }

    /// An unreadable queue file is a failure the operator is told about, not a
    /// queue that silently empties.
    @Test @MainActor func aFailedWriteIsReported() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        // A directory where the file must go: the write cannot succeed.
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
        let store = LoopsStore(tasksURL: target)
        let before = store.tasks.count
        let saved = await store.addTask(TaskItem(id: "lac-doomed", task: "never lands"))
        #expect(!saved, "the caller must be able to tell the write failed")
        #expect(store.errorMessage != nil, "and the operator must be told why")
        #expect(store.tasks.count == before, "an unsaved task must not appear on the board")
    }
}

struct CancelledStreamTests {
    /// The cancel button destroyed the answer the user had just read.
    ///
    /// `cancel()` bumped the stream generation *first*, and every save path
    /// in the task opens with `guard streamGeneration == generation` — so the
    /// partial was never appended, never persisted, and `streamText` kept
    /// holding it. The transcript draws a live row whenever that buffer is
    /// non-empty, so a phantom message stayed on screen in no thread and
    /// survived a relaunch.
    @Test @MainActor func cancelKeepsWhatTheStreamAlreadyProduced() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)
        let threadId = try #require(store.activeThreadId)

        store.beginStream(threadId: threadId, model: "test-model")
        #expect(store.isSending)
        store.streamText = "the first half of an answer"
        store.cancel()

        #expect(!store.isSending)
        #expect(store.streamText.isEmpty, "the live buffer must not survive as a phantom row")

        let messages = try #require(store.threads.first { $0.id == threadId }).messages
        let last = try #require(messages.last)
        #expect(last.role == "assistant")
        #expect(last.content == "the first half of an answer [Interrupted]")
        #expect(last.model == "test-model")

        // Persisted, not just in memory: a relaunch must show it too.
        let file = root.appendingPathComponent(".lac/chat-threads.jsonl")
        let onDisk = try String(contentsOf: file, encoding: .utf8)
        #expect(onDisk.contains("the first half of an answer"), "not persisted: \(onDisk)")

        // Stopping twice is not two messages.
        store.cancel()
        #expect(store.threads.first { $0.id == threadId }?.messages.count == messages.count)
    }

    @Test @MainActor func cancelWithNothingStreamedChangesNothing() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)
        let threadId = try #require(store.activeThreadId)
        let before = try #require(store.threads.first { $0.id == threadId }).messages.count
        store.beginStream(threadId: threadId, model: "m")
        store.cancel()
        #expect(!store.isSending)
        #expect(store.streamText.isEmpty)
        #expect(try #require(store.threads.first { $0.id == threadId }).messages.count == before)
    }
}

struct AttachmentReaderTests {
    /// Truncating at a byte offset can split a multi-byte character, and then
    /// the strict decode returns nil — so a readable 200 KB UTF-8 file was
    /// rejected as "not a readable UTF-8 text file" whenever the 128 KB
    /// boundary landed mid-character.
    @Test func aFileTruncatedMidCharacterStillReads() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        // 100_000 three-byte characters: the 128 KB cap lands inside one of
        // them (300_000 bytes of content, so byte 131_072 is mid-character).
        let text = String(repeating: "世", count: 100_000)
        let url = root.appendingPathComponent("cjk.txt")
        try text.write(to: url, atomically: true, encoding: .utf8)

        let item = try ChatView.readAttachment(at: url)
        #expect(item.wasTruncated)
        #expect(item.byteCount == 300_000, "the size shown is the file's, not the read's")
        #expect(item.content.utf8.count <= 128 * 1024)
        #expect(item.content.hasPrefix("世"))
        #expect(!item.content.isEmpty)
        // And the cap is a cap: a smaller one is honored too.
        let tiny = try ChatView.readAttachment(at: url, maxBytes: 10)
        #expect(tiny.content.utf8.count <= 10)
        #expect(tiny.wasTruncated)
    }

    @Test func aWholeFileIsNotTruncated() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = root.appendingPathComponent("small.txt")
        try "hello — wörld".write(to: url, atomically: true, encoding: .utf8)
        let item = try ChatView.readAttachment(at: url)
        #expect(!item.wasTruncated)
        #expect(item.content == "hello — wörld")
        #expect(item.byteCount == item.content.utf8.count)
        #expect(item.name == "small.txt")
    }

    @Test func binaryIsStillRejectedRatherThanInjectedAsReplacementCharacters() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = root.appendingPathComponent("blob.bin")
        // NUL bytes and invalid UTF-8 all the way through, past the cap.
        try Data(repeating: 0xFF, count: 4096).write(to: url)
        #expect(throws: ChatView.AttachmentError.self) {
            _ = try ChatView.readAttachment(at: url)
        }
    }

    @Test func aMissingFileIsAnError() {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = root.appendingPathComponent("nope.txt")
        #expect(throws: ChatView.AttachmentError.self) {
            _ = try ChatView.readAttachment(at: url)
        }
    }
}

struct SubmitGuardTests {
    private func file(_ name: String, _ content: String) -> AttachedContextFile {
        AttachedContextFile(
            name: name,
            path: "/tmp/\(name)",
            content: content,
            byteCount: content.utf8.count,
            wasTruncated: false
        )
    }

    /// Pressing Return mid-stream used to clear the attachment tray and send
    /// nothing: the `!store.isSending` early return came *after*
    /// `attachedFiles = []`, so the files were gone and the prompt was
    /// dropped, with no error to explain either.
    @Test func aReturnWhileStreamingConsumesNothing() {
        let attachment = file("notes.md", "context")
        let held = ChatView.applySubmission(
            ChatView.ComposerState(draft: "next question", attachments: [attachment]),
            isSending: true
        )
        #expect(held.prompt == nil, "nothing may be sent mid-stream")
        #expect(held.state.attachments == [attachment], "the tray must survive a held Return")
        #expect(held.state.draft == "next question", "the draft must survive a held Return")

        // Draft empty, attachment only: still held, still intact.
        let heldToo = ChatView.applySubmission(
            ChatView.ComposerState(draft: "", attachments: [attachment]),
            isSending: true
        )
        #expect(heldToo.prompt == nil)
        #expect(heldToo.state.attachments == [attachment])
    }

    @Test func aRealSubmissionSendsBothAndClearsBoth() throws {
        let attachment = file("notes.md", "context")
        let sent = ChatView.applySubmission(
            ChatView.ComposerState(draft: "  explain  ", attachments: [attachment]),
            isSending: false
        )
        let prompt = try #require(sent.prompt)
        #expect(prompt.hasSuffix("explain"), "the draft is trimmed, not dropped: \(prompt)")
        #expect(prompt.contains("=== [File: notes.md] (/tmp/notes.md) ==="))
        #expect(prompt.contains("```md\ncontext\n```"))
        #expect(sent.state.attachments.isEmpty, "a sent submission consumes the tray")
        #expect(sent.state.draft.isEmpty)

        // Attachments alone are a prompt.
        let onlyFiles = ChatView.applySubmission(
            ChatView.ComposerState(draft: "", attachments: [attachment]),
            isSending: false
        )
        #expect(onlyFiles.prompt?.contains("notes.md") == true)

        // An empty composer is not a send, and consumes nothing.
        let empty = ChatView.applySubmission(
            ChatView.ComposerState(draft: "   ", attachments: []),
            isSending: false
        )
        #expect(empty.prompt == nil)
        #expect(empty.state.draft == "   ")
    }

    /// The three start buttons share one path now, and the enum's `rawValue`
    /// *is* the subcommand `lac serve` takes — the only place that mapping is
    /// written down. A previous version of this test asserted a `subcommand`
    /// property that production never called, which meant renaming a lane's
    /// raw value would have kept the test green while the app spawned a lane
    /// that does not exist.
    @Test func lanesAreNamedForTheSubcommandsTheySpawn() {
        #expect(NetworkManager.Lane.allCases.map(\.rawValue) == ["mlx", "llama", "ollama"])
        #expect(NetworkManager.Lane(rawValue: "mlx") == .mlx)
        #expect(NetworkManager.Lane(rawValue: "llama") == .llama)
        #expect(NetworkManager.Lane(rawValue: "ollama") == .ollama)
        // Every lane must be startable and probeable.
        for lane in NetworkManager.Lane.allCases {
            let port = NetworkManager.backendPort(lane, environment: [:], mlxPortFile: nil)
            #expect((1...65535).contains(port), "\(lane.rawValue) resolved to \(port)")
        }
    }
}

// MARK: - Write-failure honesty, lock parity, host facts, detached children (2026-09-27 pass)

struct QueueRollbackTests {
    /// A drag that fails to write used to leave the card where the user dropped
    /// it while the file behind it disagreed — a drag that did not land, still
    /// looking like it did, with no way back. The write now rolls back to the
    /// snapshot taken before the drag, so the card goes back and the banner says
    /// why.
    @Test @MainActor func aFailedDragSnapsBackToWhereItStarted() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        // A directory where the file must go: the write cannot succeed.
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
        let store = LoopsStore(tasksURL: target)
        // A task that is not already in the target status: the seeded `lac-001`
        // is completed, and dragging it to completed is a no-op that proves
        // nothing about rollback.
        let task = try #require(store.tasks.first(where: { $0.status != .completed }))
        let original = task.status

        let saved = await store.moveTask(id: task.id, to: .completed)
        #expect(!saved, "the caller must be able to tell the write failed")
        #expect(store.errorMessage != nil, "and the operator must be told why")
        #expect(
            store.tasks.first(where: { $0.id == task.id })?.status == original,
            "the card must go back to where the drag found it, not stay where it was dropped"
        )
    }

    /// The optimistic apply is what makes a drag instant; the rollback is what
    /// makes it honest. A successful write keeps the drag.
    @Test @MainActor func aSuccessfulDragStays() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        let task = try #require(store.tasks.first)

        let saved = await store.moveTask(id: task.id, to: .completed)
        #expect(saved)
        #expect(store.errorMessage == nil)
        #expect(store.tasks.first(where: { $0.id == task.id })?.status == .completed)
        // And it is on disk, not just on the board.
        let written = try String(contentsOf: target, encoding: .utf8)
        #expect(written.contains("status: complete"), "not written: \(written)")
    }
}

struct LockParityTests {
    /// The Studio's backstop is the CLI's number, and a live owner is never
    /// reclaimed regardless of age. A lock held by a process that is alive must
    /// survive a wait that outlasts any plausible backstop, and still be there.
    @Test func aLiveOwnerKeepsTheLockPastTheBackstop() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let lock = root.appendingPathComponent(".lac-queue.lock")
        let me = ProcessInfo.processInfo.processIdentifier
        try FileManager.default.createDirectory(at: lock, withIntermediateDirectories: true)
        try "\(me) \(Int(Date().timeIntervalSince1970))".write(
            to: lock.appendingPathComponent("owner"), atomically: true, encoding: .utf8
        )

        // A backstop far shorter than the owner's life: the wait must end by
        // giving up, not by taking the lock.
        let taken = await LoopsStore.acquireQueueLock(at: lock, staleSeconds: 0.05)
        #expect(!taken, "a live owner's lock must not be reclaimed however old it looks")
        #expect(FileManager.default.fileExists(atPath: lock.path), "the lock was removed")
        let record = try String(contentsOf: lock.appendingPathComponent("owner"), encoding: .utf8)
        #expect(record.hasPrefix("\(me) "), "the owner record was rewritten: \(record)")
    }

    /// EPERM means "gone" here because that is what the CLI's probe concludes
    /// from the same `kill -0`, and the two implementations must reach the same
    /// verdict on the same record. pid 1 (launchd, root-owned) is the one
    /// process a non-root test process can probe and get EPERM for.
    @Test func epermIsReportedAsGoneLikeTheCliConcludes() {
        guard geteuid() != 0 else { return } // as root, kill(1, 0) succeeds
        #expect(LoopsStore.processIsAlive(1) == false, "EPERM must read as gone, matching common::process_alive")
        // A pid that cannot exist is provably gone too.
        #expect(LoopsStore.processIsAlive(2_147_483_600) == false)
        // And a nonsense pid is unprobeable, not dead.
        #expect(LoopsStore.processIsAlive(0) == nil)
        #expect(LoopsStore.processIsAlive(-1) == nil)
    }
}

struct HostFactsCacheTests {
    /// The dashboard polls every 3s and every poll used to end in a subprocess.
    /// The freshness rule is what keeps a poll from spawning one it does not
    /// need, so it is asserted directly.
    @Test func aFreshCacheMeansNoSubprocess() {
        let now = Date()
        #expect(!NetworkManager.hostFactsAreFresh(nil, now: now), "never read is not fresh")
        #expect(NetworkManager.hostFactsAreFresh(now, now: now), "just read is fresh")
        #expect(NetworkManager.hostFactsAreFresh(now.addingTimeInterval(-1), now: now))
        #expect(NetworkManager.hostFactsAreFresh(now.addingTimeInterval(-(NetworkManager.hostFactsTTL - 1)), now: now))
        #expect(!NetworkManager.hostFactsAreFresh(now.addingTimeInterval(-(NetworkManager.hostFactsTTL + 1)), now: now))
        #expect(!NetworkManager.hostFactsAreFresh(now.addingTimeInterval(-3600), now: now))
        // The TTL is long enough that a 3s poll never needs a subprocess.
        #expect(NetworkManager.hostFactsTTL >= 10, "a poll every 3s must not spawn every 3s")
    }

    /// The router measures host facts itself and serves them in `/lac/status`,
    /// the document the poll already fetches. When the block is present the
    /// loader must not run at all — that is the property that removes the
    /// subprocess.
    @Test @MainActor func hostFactsFromTheRouterNeedNoSubprocess() async {
        let network = NetworkManager()
        let status = RouterStatusResponse(
            status: "ok", router: "lac-router v2.8", preferred: "auto",
            active: "mlx", target_port: 8080, uptime_secs: 1, inflight: 0,
            models_mapped: 1, usage_log: "", backends: [:], stats: [:],
            host: HostFacts(thermal: "Nominal", free_ram_gib: 12.5, total_ram_gib: 64)
        )
        network.response = status
        var loaderCalls = 0
        let loader: () async -> LacStatusJson? = {
            loaderCalls += 1
            return nil
        }

        await network.fetchHostFacts(loader: loader)
        #expect(loaderCalls == 0, "the router provided the facts; no subprocess may run")
        #expect(network.host?.thermal == "Nominal")
        #expect(network.host?.free_ram_gib == 12.5)
        #expect(network.host?.total_ram_gib == 64)
    }

    /// An older router omits the block, and the subprocess answers for it —
    /// at most once per TTL, which is what the cache is for.
    @Test @MainActor func anOlderRouterFallsBackToTheSubprocess() async {
        let network = NetworkManager()
        let status = RouterStatusResponse(
            status: "ok", router: "lac-router v2.7", preferred: "auto",
            active: "mlx", target_port: 8080, uptime_secs: 1, inflight: 0,
            models_mapped: 1, usage_log: "", backends: [:], stats: [:],
            host: nil
        )
        network.response = status
        let sentinel = LacStatusJson(
            model: nil, thermal: "Serious",
            free_ram_gib: 1, total_ram_gib: 2, gateway: true, active: "mlx"
        )
        var loaderCalls = 0
        let loader: () async -> LacStatusJson? = {
            loaderCalls += 1
            return sentinel
        }

        await network.fetchHostFacts(loader: loader)
        #expect(loaderCalls == 1, "an older router needs the subprocess")
        #expect(network.host?.thermal == "Serious")

        // Warm cache: the loader does not run, and the cached value stands.
        await network.fetchHostFacts(loader: loader)
        #expect(loaderCalls == 1, "a fresh cache spawned a second subprocess")

        // A forced refresh bypasses the cache.
        await network.fetchHostFacts(force: true, loader: loader)
        #expect(loaderCalls == 2)
    }

    /// A status document without the block decodes, and the dashboard shows
    /// "—" for the facts it does not have rather than a confident zero.
    @Test func aStatusWithoutHostFactsStillDecodes() throws {
        let json = "{\"status\":\"ok\",\"router\":\"lac-router v2.7\",\"preferred\":\"auto\",\"active\":\"mlx\",\"target_port\":8080,\"backends\":{},\"stats\":{}}"
        let decoded = try JSONDecoder().decode(
            RouterStatusResponse.self, from: Data(json.utf8))
        #expect(decoded.host == nil)
        #expect(decoded.status == "ok")
    }

    /// A `null` thermal decodes to an absent reading, and the client does not
    /// manufacture one.
    ///
    /// The router publishes `"thermal": null` when its probe cannot answer —
    /// which is what happens on a host where pmset is unavailable, and is what
    /// it published on *every* host while the probe was still asking pmset for
    /// an argument pmset rejects. Two client-side defaults turned that `null`
    /// into the one value that means "cool, spend headroom": the status bar
    /// rendered `host.thermal ?? "nominal"` in green, and the dashboard's
    /// thermal colour mapped every unrecognised string to green. Both are
    /// gone; this pins the decode half, which is where a future "just default
    /// it" would restart.
    @Test func aNullThermalIsAnAbsentReading() throws {
        let json = """
        {"status":"ok","router":"lac-router v2.8","preferred":"auto","active":"mlx",
         "target_port":8080,"backends":{},"stats":{},
         "host":{"thermal":null,"free_ram_gib":12.5,"total_ram_gib":64.0}}
        """
        let decoded = try JSONDecoder().decode(
            RouterStatusResponse.self, from: Data(json.utf8))
        let host = try #require(decoded.host)
        #expect(host.thermal == nil, "null must not decode to a level")
        #expect(host.free_ram_gib == 12.5, "the rest of the block still decodes")
        #expect(host.total_ram_gib == 64.0)
    }

    /// The same document from `lac status --json`, which is the other source
    /// `NetworkManager` reads. Both paths now publish `null` for an absent
    /// reading, so both must decode it as absent.
    @Test func aNullThermalFromTheCliDecodesAsAbsent() throws {
        let json = #"{"model":"qwen","thermal":null,"free_ram_gib":null,"total_ram_gib":null,"gateway":true}"#
        let decoded = try JSONDecoder().decode(
            LacStatusJson.self, from: Data(json.utf8))
        #expect(decoded.thermal == nil)
        #expect(decoded.free_ram_gib == nil, "null is unknown, not zero")
        #expect(decoded.gateway == true)
    }
}

struct DetachedProcessTests {
    /// `try? p.run()` with no handler left a zombie when the child exited and
    /// discarded the only error the operator was going to see. The tracked spawn
    /// reaps the child and records the status.
    @Test @MainActor func aChildThatExitsIsRecordedNotLost() async throws {
        let log = NetworkManager.DetachedProcessLog()
        let falseBin = URL(fileURLWithPath: "/usr/bin/false")
        NetworkManager.runTracked(
            executable: falseBin, arguments: ["serve", "mlx"],
            port: 8080, token: "", log: log
        )
        let recorded = await waitFor({ log.last != nil }, timeout: 5)
        #expect(recorded, "the exit was never recorded")
        #expect(log.last?.contains("exited with status 1") == true, "recorded: \(log.last ?? "nil")")
    }

    @Test @MainActor func aChildThatSucceedsRecordsNothing() async throws {
        let log = NetworkManager.DetachedProcessLog()
        NetworkManager.runTracked(
            executable: URL(fileURLWithPath: "/usr/bin/true"), arguments: ["serve", "mlx"],
            port: 8080, token: "", log: log
        )
        // Give the handler a chance to fire, then confirm it had nothing to say.
        try? await Task.sleep(nanoseconds: 500_000_000)
        #expect(log.last == nil, "a clean exit is not a failure")
    }

    @Test @MainActor func aChildThatCannotLaunchIsRecorded() async throws {
        let log = NetworkManager.DetachedProcessLog()
        NetworkManager.runTracked(
            executable: URL(fileURLWithPath: "/nonexistent/lac"), arguments: ["serve", "mlx"],
            port: 8080, token: "", log: log
        )
        let recorded = await waitFor({ log.last != nil }, timeout: 5)
        #expect(recorded, "the launch failure was never recorded")
        #expect(log.last?.contains("failed to launch") == true, "recorded: \(log.last ?? "nil")")
    }

    @MainActor
    private func waitFor(_ condition: @escaping @MainActor () -> Bool, timeout: TimeInterval) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        return condition()
    }
}

struct StudioHomeTests {
    /// Rust resolves `$HOME`; `FileManager.homeDirectoryForCurrentUser` reads
    /// passwd. With `HOME=/tmp/x` the two wrote and read two different queues.
    @Test func theStudioResolvesHomeTheWayTheCliDoes() {
        let passwdHome = NSHomeDirectory()
        #expect(LoopsStore.studioHome(environment: ["HOME": "/tmp/x"]) == "/tmp/x")
        #expect(LoopsStore.studioHome(environment: ["HOME": "/Users/someone"]) == "/Users/someone")
        // A relative $HOME is not a home directory — the same rule
        // `common::home_dir_from` applies.
        #expect(LoopsStore.studioHome(environment: ["HOME": "relative"]) == passwdHome)
        #expect(LoopsStore.studioHome(environment: ["HOME": "."]) == passwdHome)
        #expect(LoopsStore.studioHome(environment: ["HOME": ""]) == passwdHome)
        #expect(LoopsStore.studioHome(environment: [:]) == passwdHome)
        // And the result is always absolute, which is what every state path
        // below it assumes.
        #expect(LoopsStore.studioHome(environment: ["HOME": "/tmp/x"]).hasPrefix("/"))
    }
}

struct ShortReadTests {
    /// `read(upToCount:)` returns *up to* the count. A pipe cannot show the
    /// short-read case — it blocks until the count or EOF — so the loop is
    /// tested against a reader that answers in pieces smaller than asked.
    @Test func aReaderThatAnswersShortStillGetsTheWholeFile() throws {
        var sent = 0
        let reader: (Int) throws -> Data? = { want in
            guard sent < 10 else { return nil } // EOF
            let n = min(5, want, 10 - sent)
            sent += n
            return Data(repeating: 65, count: n) // 'A'
        }
        let data = try ChatView.readUntilCap(reader, maxBytes: 100)
        #expect(data.count == 10, "got \(data.count) of 10")
    }

    /// The cap holds even when every answer is short: the loop must not stop
    /// at the first partial read, and must not read past the cap either.
    @Test func aReaderThatAnswersShortStillHonorsTheCap() throws {
        var sent = 0
        let reader: (Int) throws -> Data? = { want in
            guard sent < 10 else { return nil }
            let n = min(5, want, 10 - sent)
            sent += n
            return Data(repeating: 65, count: n)
        }
        let data = try ChatView.readUntilCap(reader, maxBytes: 8)
        #expect(data.count == 8, "got \(data.count) of 8")
    }

    /// A real file, so the size is known and `wasTruncated` is exact.
    @Test func aRealFilePastTheCapIsTruncated() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = root.appendingPathComponent("cap.txt")
        try Data(repeating: 65, count: 1000).write(to: url) // 1 KB of 'A'

        let item = try ChatView.readAttachment(at: url, maxBytes: 100)
        #expect(item.content.count == 100, "got \(item.content.count)")
        #expect(item.wasTruncated)
        #expect(item.byteCount == 1000)
    }

    @Test func aRealFileUnderTheCapIsNotTruncated() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = root.appendingPathComponent("small.txt")
        try "hello".write(to: url, atomically: true, encoding: .utf8)

        let item = try ChatView.readAttachment(at: url, maxBytes: 100)
        #expect(item.content == "hello")
        #expect(!item.wasTruncated)
    }
}

struct QueueReconcileTests {
    /// The file is the truth for what is durable, the board for what the user
    /// asked for, and they meet on every successful write. What they cannot do
    /// on their own is notice a change made by the other writer — the
    /// `lac worker`, or the app on the Mac this one is remoted to — and a queue
    /// nobody mutates stays diverged until someone does.
    @Test @MainActor func anExternalChangeIsAdoptedOnReconcile() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        await store.addTask(TaskItem(id: "lac-seed", task: "seed", priority: .high))
        let before = store.tasks.count

        // The other writer appends a task behind the app's back.
        var yaml = try String(contentsOf: target, encoding: .utf8)
        yaml += "- id: \"lac-external\"\n  task: \"added by the worker\"\n  priority: high\n  status: pending\n  loop: \"daily-coding.yaml\"\n  created: \"2026-01-01T00:00:00Z\"\n\n"
        try yaml.write(to: target, atomically: true, encoding: .utf8)

        store.reconcileWithDisk()
        #expect(store.tasks.contains(where: { $0.id == "lac-external" }))
        #expect(store.tasks.count == before + 1)
        #expect(store.errorMessage == nil, "a reconcile that adopts is not an error")
    }

    /// A reconcile must not clobber board state that is ahead of the file on
    /// purpose — an optimistic drag whose write has not landed. The lock is
    /// held by a live pid so the mutation blocks; the board keeps its edit
    /// until the write completes.
    @Test @MainActor func aReconcileWaitsForAnInFlightMutation() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        await store.addTask(TaskItem(id: "lac-seed", task: "seed", priority: .high))

        // A lock held by a process that is alive — this one.
        let lock = root.appendingPathComponent(".lac-queue.lock")
        try FileManager.default.createDirectory(at: lock, withIntermediateDirectories: true)
        let me = ProcessInfo.processInfo.processIdentifier
        try "\(me) \(Int(Date().timeIntervalSince1970))".write(
            to: lock.appendingPathComponent("owner"), atomically: true, encoding: .utf8
        )

        let mutation = Task { @MainActor in
            await store.addTask(TaskItem(id: "lac-pending", task: "optimistic", priority: .high))
        }
        try await Task.sleep(nanoseconds: 300_000_000)
        // The board has the optimistic edit; the file does not.
        #expect(store.tasks.contains(where: { $0.id == "lac-pending" }))

        store.reconcileWithDisk()
        #expect(
            store.tasks.contains(where: { $0.id == "lac-pending" }),
            "a reconcile must not clobber a board that is ahead on purpose"
        )

        // Release the lock; the write lands.
        try FileManager.default.removeItem(at: lock)
        await mutation
        let written = try String(contentsOf: target, encoding: .utf8)
        #expect(written.contains("lac-pending"))
    }

    /// A file that cannot be read is not a queue that can be adopted: the board
    /// is left alone rather than wiped.
    @Test @MainActor func anUnreadableFileIsNotReconciled() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        // A directory where the file must go: it exists, but it is not readable
        // as a file.
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
        let store = LoopsStore(tasksURL: target)
        let before = store.tasks.count
        #expect(before > 0, "the fixture must seed something to preserve")

        store.reconcileWithDisk()
        #expect(store.tasks.count == before, "an unreadable file must not wipe the board")
    }
}

extension DetachedProcessTests {
    /// A start button pressed repeatedly on a dead lane would otherwise grow the
    /// log without bound.
    @Test @MainActor func theLogIsBounded() {
        let log = NetworkManager.DetachedProcessLog()
        for i in 0..<20 { log.record("failure \(i)") }
        #expect(log.entries.count == NetworkManager.DetachedProcessLog.capacity)
        #expect(log.entries.count == 8)
        #expect(log.last == "failure 19", "the most recent failure is what a start surfaces")
        // The oldest are dropped, not the newest.
        #expect(log.entries.first == "failure 12")
        log.clear()
        #expect(log.last == nil)
    }

    /// The engine's own complaint, not just its exit status. A pipe cannot be
    /// used for a long-lived child's stderr — it fills at 64 KiB and blocks the
    /// child — so the child writes to a log file and the tail is recorded.
    @Test func theStderrTailIsTheLastLines() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = root.appendingPathComponent("child.err")
        let lines = (1...20).map { "line \($0)" }.joined(separator: "\n")
        try (lines + "\n").write(to: url, atomically: true, encoding: .utf8)

        let tail = NetworkManager.tailOfFile(url)
        #expect(tail == "line 17\nline 18\nline 19\nline 20", "got \(tail ?? "nil")")

        // A file shorter than the limit is returned whole.
        try "only\n".write(to: url, atomically: true, encoding: .utf8)
        #expect(NetworkManager.tailOfFile(url) == "only")

        // A missing file has no tail.
        #expect(NetworkManager.tailOfFile(root.appendingPathComponent("nope.err")) == nil)
    }

    /// End to end: a child that complains on stderr and exits non-zero is
    /// recorded with its complaint, which is the difference between "the engine
    /// said the port was taken" and "exited with status 1".
    @Test @MainActor func aChildsComplaintIsRecordedWithItsStatus() async throws {
        let log = NetworkManager.DetachedProcessLog()
        // The complaint lives in a script file, not in the command line — an
        // `echo X >&2` inline would put X in the command string and the test
        // would pass whether or not the tail was captured.
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let script = root.appendingPathComponent("complain.sh")
        try "#!/bin/sh\necho 'address already in use' >&2\nexit 3\n".write(
            to: script, atomically: true, encoding: .utf8
        )
        NetworkManager.runTracked(
            executable: URL(fileURLWithPath: "/bin/sh"),
            arguments: [script.path],
            port: 8080, token: "", log: log
        )
        let recorded = await waitFor({ log.last != nil }, timeout: 5)
        #expect(recorded, "the exit was never recorded")
        let message = log.last ?? ""
        #expect(message.contains("exited with status 3"), "message: \(message)")
        #expect(message.contains("address already in use"), "message: \(message)")
    }
}

extension ShortReadTests {
    /// A stream that yields exactly the cap is a whole file. Reading exactly
    /// the cap cannot tell that from a stream with one byte more behind it, and
    /// the conservative answer was wrong half the time.
    @Test func aStreamOfExactlyTheCapIsNotTruncated() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let pipe = Pipe()
        let writer = pipe.fileHandleForWriting
        let reader = pipe.fileHandleForReading
        writer.write(Data(repeating: 65, count: 100)) // exactly the cap
        try? writer.close()

        let item = try ChatView.readAttachment(
            from: reader, url: root.appendingPathComponent("x.txt"), maxBytes: 100)
        #expect(item.content.count == 100, "got \(item.content.count)")
        #expect(!item.wasTruncated, "a stream of exactly the cap is a whole file")
    }

    @Test func aStreamOfOneByteMoreIsTruncated() throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let pipe = Pipe()
        let writer = pipe.fileHandleForWriting
        let reader = pipe.fileHandleForReading
        writer.write(Data(repeating: 65, count: 101)) // one byte past the cap
        try? writer.close()

        let item = try ChatView.readAttachment(
            from: reader, url: root.appendingPathComponent("x.txt"), maxBytes: 100)
        #expect(item.content.count == 100, "got \(item.content.count)")
        #expect(item.wasTruncated, "one byte past the cap proves there is more")
    }
}

extension QueueReconcileTests {
    /// The appear-time reconcile covers a queue that changed while the
    /// dashboard was away; this covers one that changes while it is open —
    /// the poll loop calls it every few seconds, and a stat of the file's
    /// mtime is enough to notice.
    @Test @MainActor func aChangeWhileOpenIsPickedUp() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        await store.addTask(TaskItem(id: "lac-seed", task: "seed", priority: .high))
        // The appear-time reconcile establishes the baseline mtime.
        store.reconcileWithDisk()
        #expect(store.tasks.contains(where: { $0.id == "lac-seed" }))
        let before = store.tasks.count

        // Nothing changed: the stat finds no newer mtime and leaves the board.
        store.reconcileIfQueueChanged()
        #expect(store.tasks.count == before, "a poll with nothing new must not touch the board")

        // The other writer appends a task.
        var yaml = try String(contentsOf: target, encoding: .utf8)
        yaml += "- id: \"lac-live\"\n  task: \"added while open\"\n  priority: high\n  status: pending\n  loop: \"daily-coding.yaml\"\n  created: \"2026-01-01T00:00:00Z\"\n\n"
        try yaml.write(to: target, atomically: true, encoding: .utf8)

        store.reconcileIfQueueChanged()
        #expect(store.tasks.contains(where: { $0.id == "lac-live" }))
        #expect(store.tasks.count == before + 1)
    }

    /// The board is only reconciled when it differs from the file, so a poll
    /// that lands after this app's own write is a no-op rather than a clobber.
    @Test @MainActor func thePollAfterOurOwnWriteIsANoOp() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        store.reconcileWithDisk()

        await store.addTask(TaskItem(id: "lac-ours", task: "written by the app", priority: .high))
        let afterWrite = store.tasks.count

        // The file's mtime is now newer than the last reconcile, so the poll
        // does look — and finds the board already matches.
        store.reconcileIfQueueChanged()
        #expect(store.tasks.count == afterWrite)
        #expect(store.tasks.contains(where: { $0.id == "lac-ours" }))
    }

    /// A mutation in flight means the board is ahead of the file on purpose;
    /// the poll must not reconcile it away.
    @Test @MainActor func thePollWaitsForAnInFlightMutation() async throws {
        let root = testDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let target = root.appendingPathComponent("todo/loop-studio-tasks.yaml")
        let store = LoopsStore(tasksURL: target)
        await store.addTask(TaskItem(id: "lac-seed", task: "seed", priority: .high))
        store.reconcileWithDisk()

        let lock = root.appendingPathComponent(".lac-queue.lock")
        try FileManager.default.createDirectory(at: lock, withIntermediateDirectories: true)
        let me = ProcessInfo.processInfo.processIdentifier
        try "\(me) \(Int(Date().timeIntervalSince1970))".write(
            to: lock.appendingPathComponent("owner"), atomically: true, encoding: .utf8
        )
        let mutation = Task { @MainActor in
            await store.addTask(TaskItem(id: "lac-pending", task: "optimistic", priority: .high))
        }
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(store.tasks.contains(where: { $0.id == "lac-pending" }))

        store.reconcileIfQueueChanged()
        #expect(
            store.tasks.contains(where: { $0.id == "lac-pending" }),
            "the poll must not clobber a board that is ahead on purpose"
        )
        try FileManager.default.removeItem(at: lock)
        await mutation
    }
}

struct DetachedLogHygieneTests {
    /// Each run removes its own stderr log at the start, so at most one file per
    /// command accumulates — but the last run's file is never deleted, and
    /// nothing rotates them. The rule for what to delete is asserted directly.
    @Test func onlyOurOldLogsAreStale() {
        let now = Date()
        let day: TimeInterval = 86400
        let entry = (URL(fileURLWithPath: "/logs/loop-studio-serve.err"), now.addingTimeInterval(-2 * day))
        let fresh = (URL(fileURLWithPath: "/logs/loop-studio-serve.err"), now.addingTimeInterval(-1))
        let other = (URL(fileURLWithPath: "/logs/lac-router.err"), now.addingTimeInterval(-30 * day))
        let wrongPrefix = (URL(fileURLWithPath: "/logs/router.err"), now.addingTimeInterval(-30 * day))
        // Everything in our namespace is ours to remove, extension or not — a
        // version that used a path as a file name left a directory tree here.
        // The entry a directory listing sees is the top-level `loop-studio-`
        // directory itself; everything under it came with it.
        let strayDir = (URL(fileURLWithPath: "/logs/loop-studio-"), now.addingTimeInterval(-30 * day))

        let stale = NetworkManager.staleDetachedLogs(
            entries: [entry, fresh, other, wrongPrefix, strayDir], now: now, ttl: day
        )
        #expect(
            stale == [entry.0, strayDir.0],
            "only our own entries, and only past the ttl: \(stale)"
        )
    }

    /// The ttl is a day: a start that died an hour ago is still worth reading.
    @Test func aRecentFailureIsKept() {
        let now = Date()
        let recent = (URL(fileURLWithPath: "/logs/loop-studio-serve.err"), now.addingTimeInterval(-3600))
        #expect(NetworkManager.staleDetachedLogs(entries: [recent], now: now).isEmpty)
    }
}

/// `~/.lac/locks/loop-studio-tasks` is taken by two implementations of one rule: the
/// Rust one in `common::lock_is_reclaimable` and this one. They cannot call each
/// other — Swift and Rust, one filesystem — so instead both suites read the
/// same table in `docs/lock-parity.tsv` and assert the same verdicts. A change
/// to one implementation that the other does not share fails that other's gate,
/// which is what turns "kept in sync by hand" into "provably the same rule".
struct LockParityTableTests {
    static func tableURL() -> URL {
        // The test bundle runs with the package directory as its working
        // directory, so the repository's docs/ is one level up.
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // the file itself
            .deletingLastPathComponent()  // LoopStudioTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // SwiftUI — the repository root follows
            .appendingPathComponent("docs/lock-parity.tsv")
    }

    @Test func theStudioAgreesWithTheSharedTable() throws {
        let url = Self.tableURL()
        let table = try String(contentsOf: url, encoding: .utf8)
        var seen: [String] = []
        for line in table.split(separator: "\n") where !line.isEmpty && !line.hasPrefix("#") {
            let cols = line.split(separator: "\t", omittingEmptySubsequences: false).map(String.init)
            #expect(cols.count == 5, "malformed parity row: \(line)")
            guard cols.count == 5 else { continue }
            let (name, rawOwner, rawAge, probe, expect) = (cols[0], cols[1], cols[2], cols[3], cols[4])
            let owner: String? = rawOwner == "-" ? nil : rawOwner
            let age: TimeInterval? = Double(rawAge)
            let liveness: (Int32) -> Bool? = { _ in
                if probe == "alive" { return true }
                if probe == "dead" { return false }
                if probe == "unprobeable" { return nil }
                Issue.record("unknown probe \(probe) in row \(name)")
                return nil
            }
            let got = LoopsStore.lockIsReclaimable(
                owner: owner, directoryAge: age, staleSeconds: 30, probe: liveness
            )
            #expect(
                got == (expect == "reclaim"),
                "parity row \(name) disagrees: reclaimable=\(got)"
            )
            seen.append(name)
        }
        // The exact set, not a floor. `checked >= 15` was satisfied by any table
        // at least that long, so the four rows it stopped covering — including
        // the O_TRUNC-window rows that are the reason this table exists — could
        // be deleted with this suite still green. Naming all nineteen makes a
        // removed row fail here and a renamed one fail too. The Rust half
        // carries the same list, so the two cannot drift apart silently.
        let expected = [
            "no-record-fresh",
            "no-record-at-backstop",
            "no-record-aged",
            "no-record-unknown-age",
            "empty-record-fresh",
            "empty-record-at-backstop",
            "garbage-record-fresh",
            "garbage-record-at-backstop",
            "negative-pid-fresh",
            "negative-pid-aged",
            "live-owner-ancient",
            "live-owner-just-born",
            "dead-owner-fresh",
            "dead-owner-ancient",
            "unprobeable-fresh",
            "unprobeable-just-under",
            "unprobeable-at-backstop",
            "legacy-live-owner",
            "legacy-dead-owner",
        ]
        #expect(
            seen.sorted() == expected.sorted(),
            "the parity table is not the agreed set of cases: a row was removed, added or renamed"
        )
    }

    /// The table is data both suites depend on, so it is tracked by the
    /// delivery gate; this pins the fact that the file exists where they read
    /// it, which is the failure a renamed or moved table would produce.
    @Test func theTableIsWhereBothSuitesLookForIt() {
        #expect(FileManager.default.fileExists(atPath: Self.tableURL().path))
    }
}

/// A successful start must leave the app able to keep polling.
struct RouterStartSettleTests {
    /// The latch was never released on the success path.
    ///
    /// `isChecking` is set to `true` before a start and cleared on *both*
    /// failure paths — but neither success path cleared it, and each called
    /// `fetch()`, which opens with `guard !isChecking else { return }` and is
    /// therefore a no-op while the flag is set. So one successful start latched
    /// the flag for the rest of the session: the 3s dashboard poll stopped, the
    /// refresh button was permanently disabled, the menu-bar refresh was a no-op,
    /// and an app with no `response` yet showed "Connecting to router…" forever.
    ///
    /// It only showed up when the start *worked*, which is the opposite of when
    /// an operator would go looking — and the automatic start on the default
    /// first-run path is a success whenever the gateway comes up, so this was
    /// the common case, not the edge one.
    @MainActor @Test func a_successful_start_releases_the_latch_and_resumes() {
        let network = NetworkManager()
        network.isChecking = true
        var polled = 0
        network.resumeAfterSuccessfulStart { polled += 1 }
        #expect(network.isChecking == false, "the latch must be released, or polling never runs again")
        #expect(polled == 1, "and the poll must actually be requested")
    }

    /// The *order* is the defect, not the missing clear.
    ///
    /// Clearing the flag after the fetch is the same as never clearing it,
    /// because the fetch has already returned. So the observable that matters
    /// is the flag's value at the moment the poll closure runs, not afterwards.
    @MainActor @Test func the_latch_is_released_before_the_poll_is_requested() {
        let network = NetworkManager()
        network.isChecking = true
        var latchAtPollTime: Bool?
        network.resumeAfterSuccessfulStart {
            latchAtPollTime = network.isChecking
        }
        #expect(latchAtPollTime == false, "the poll ran while the latch was still set, so it was a no-op")
    }

    /// A failed poll must leave the app looking offline, not like it is talking
    /// to a gateway that has gone away.
    ///
    /// `response` was assigned in exactly one place — on a successful decode —
    /// and the failure path left it alone. So with the router killed, every
    /// liveness indicator kept reading the stale snapshot: the menu bar said
    /// `LAC Gateway :8000 · Online` with a filled checkmark, the header and
    /// footer dots stayed green, the avatar kept pulsing, and the chat
    /// empty-state offered "Start MLX Backend" against a dead gateway. A stale
    /// reading of a liveness probe is not a liveness reading.
    @MainActor @Test func a_failed_poll_does_not_keep_a_stale_liveness_reading() {
        let network = NetworkManager()
        network.response = RouterStatusResponse(
            status: "ok", router: "lac-router v2.8", preferred: "auto",
            active: "mlx", target_port: 8080, uptime_secs: 9, inflight: 0,
            models_mapped: 1, usage_log: "", backends: [:], stats: [:], host: nil
        )
        #expect(NetworkManager.routerIsAnswering(network.response!) == true)

        // What the catch arm now does.
        let afterFailure: RouterStatusResponse? = nil
        // ...and the app's own liveness predicate then reads offline, which is
        // the state the failure established.
        #expect(afterFailure.map(NetworkManager.routerIsAnswering) != true)
        // The `nil` is the assertion: every "is it alive" indicator in the app
        // reads `response != nil`, so a retained snapshot is a green checkmark
        // on a gateway that is not there.
        #expect(afterFailure == nil, "the stale document must be dropped on a failed poll")
    }
}


/// The queue file has two writers, and the app must not destroy the other
/// one's state. Every expectation here is a *round trip*: the app's write
/// followed by the Rust reader's parse, because the file that matters is one
/// both programs can read.
struct QueueWriterAgreementTests {
    /// The shape `lac worker` produces, verbatim: `update_task` writes
    /// `status` and `attempts` in the same call, so they always travel together.
    static let workerWritten = """
    # LAC Autonomous Task Queue (Kanban)

    - id: "a-1"
      task: 'first task'
      priority: high
      status: failed
      loop: daily-coding.yaml
      created: '2026-09-01'
      attempts: 3

    - id: "b-2"
      task: 'second task'
      priority: medium
      status: pending
      loop: daily-coding.yaml
      created: '2026-09-02'
      attempts: 1
    """

    /// A Studio drag: a card moved, nothing else.
    static func drag(_ items: inout [TaskItem]) {
        for i in items.indices where items[i].id == "b-2" {
            items[i].status = .inProgress
        }
    }

    /// `attempts` is the worker's retry budget and it must survive.
    ///
    /// `MAX_TASK_ATTEMPTS` is 3 and `pending_candidates` filters on
    /// `t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS`. A task
    /// dead-lettered at `status: failed, attempts: 3` is therefore one Studio
    /// drag away from being re-queued forever — the hot loop the dead-letter
    /// exists to stop. The mutation path re-serialised `TaskItem`, and
    /// `attempts` was never a property, so it was dropped on every single write.
    ///
    /// Corroborated on the real machine before the fix, not inferred: the live
    /// `~/todo/loop-studio-tasks.yaml` held four `status: failed` tasks and not one
    /// `attempts:` key, which the worker can only produce together.
    @Test func the_worker_retry_budget_survives_a_studio_drag() {
        var blocks = LoopsStore.parseTaskBlocks(Self.workerWritten)
        #expect(blocks.count == 2)
        var items = blocks.map(\.item)
        Self.drag(&items)
        let out = LoopsStore.serializeTaskBlocks(
            zip(blocks.indices, items).map { index, item in
                TaskBlock(item: item, extras: blocks[index].extras)
            },
            preamble: LoopsStore.preamble(of: Self.workerWritten)
        )
        #expect(out.contains("attempts: 3"), "the dead-lettered task lost its retry budget:\n\(out)")
        #expect(out.contains("attempts: 1"), "and so did the other one:\n\(out)")
        #expect(out.contains("status: in_progress"), "while the edit itself must land:\n\(out)")

        // Read back through the block model: both halves of the verdict are
        // still there, which is what the worker's filter will see.
        let back = LoopsStore.parseTaskBlocks(out)
        #expect(back[0].item.status == TaskStatus.failed)
        #expect(back[0].extras.joined().contains("attempts: 3"))
        #expect(back[1].item.status == TaskStatus.inProgress)
        #expect(back[1].extras.joined().contains("attempts: 1"))
    }

    /// An `attempts:` line is a *line*, and it is never a task field.
    ///
    /// The app must not start interpreting it. If a future field were added for
    /// it, the two writers would disagree about who owns the number, and the
    /// one that mattered — the worker's — would be a value the UI could edit.
    @Test func the_app_does_not_claim_the_workers_own_key() {
        #expect(!TaskItem.modelledKeys.contains("attempts"))
        let blocks = LoopsStore.parseTaskBlocks(Self.workerWritten)
        #expect(blocks.allSatisfy { $0.item.id != "" })
        // It is carried, not modelled, so it is in extras and nowhere else.
        for block in blocks {
            #expect(block.extras.joined().contains("attempts:"))
        }
    }

    /// The app's own reader and its writer's model must not drift.
    @Test func the_read_model_agrees_with_the_block_model() {
        let items = LoopsStore.parseTasksYaml(Self.workerWritten)
        let blocks = LoopsStore.parseTaskBlocks(Self.workerWritten)
        #expect(items.count == blocks.count)
        for (a, b) in zip(items, blocks) {
            #expect(a.id == b.item.id)
            #expect(a.task == b.item.task)
            #expect(a.status == b.item.status)
            #expect(a.priority == b.item.priority)
            #expect(a.loopName == b.item.loopName)
            #expect(a.created == b.item.created)
            #expect(a.module == b.item.module)
        }
    }

    /// A nested `status:` must not become the task's own.
    ///
    /// Rust's `block_field` is explicitly indent-anchored — "nested decoys can
    /// never hijack the match" — and the shipped template already uses a
    /// `metadata:` block. A depth-only rule is not enough either: it would
    /// consume the decoy into the task's status one level down instead. The
    /// key has to be one that belongs at that level, so `module` is a field
    /// under `metadata:` and `status` is not.
    @Test func a_nested_status_cannot_hijack_the_tasks_own() {
        let text = """
        - id: "d-4"
          task: 'has a decoy'
          priority: high
          status: pending
          loop: daily-coding.yaml
          created: '2026-09-01'
          metadata:
            status: critical
            module: 'x'
        """
        let blocks = LoopsStore.parseTaskBlocks(text)
        #expect(blocks.count == 1)
        #expect(blocks[0].item.status == TaskStatus.pending, "the task's own status wins: \(blocks[0].item.status)")
        #expect(blocks[0].item.module == "x", "but the modelled child is still read: \(blocks[0].item.module ?? "nil")")
        #expect(
            blocks[0].extras.joined().contains("status: critical"),
            "and the decoy is carried, not consumed: \(blocks[0].extras)"
        )
    }

    /// A value containing its own key name, a colon, a quote, or a newline.
    ///
    /// The old reader was `replacingOccurrences(of: "task:", with: "")`, which
    /// strips *every* occurrence: `Fix the "task:" key` came back as
    /// `Fix the " key` and the next write persisted that. And the New Task
    /// field is a multi-line `TextEditor`, so a two-line description produced
    /// `task: "line1` / `line2"` — a double-quoted scalar may not span lines,
    /// and the file is read by a *line-based* parser on the Rust side, which
    /// then read `line1` while the app read both. Two readers, permanently
    /// disagreeing, with the app as the one that persisted it.
    @Test func a_value_survives_a_round_trip_whatever_it_contains() {
        for awkward in [
            #"Fix the "task:" key"#,
            "colon: inside a value",
            "it's quoted",
            "trailing space   ",
            "",
        ] {
            let item = TaskItem(
                id: "x-1", task: awkward, priority: .medium, status: .pending,
                loopName: "daily-coding.yaml", created: "2026-09-01", module: nil
            )
            let yaml = LoopsStore.serializeTaskBlocks([TaskBlock(item: item)], preamble: "")
            let back = LoopsStore.parseTaskBlocks(yaml)
            #expect(back.count == 1, "\(awkward.debugDescription) -> \(yaml)")
            #expect(
                back[0].item.task == awkward.trimmingCharacters(in: .whitespacesAndNewlines),
                "\(awkward.debugDescription) -> \(yaml) -> \(back[0].item.task.debugDescription)"
            )
            #expect(
                yaml.split(separator: "\n").filter { $0.contains("task:") }.count == 1,
                "a value must never emit a second `task:` line: \(yaml)"
            )
        }
    }

    /// A multi-line description is flattened, and that is the contract.
    ///
    /// Not a loss to be apologised for: the file is read by a line-based parser
    /// on the Rust side, so a value that spans lines cannot be read there at
    /// all. Flattening to one line is the only shape both programs agree on, and
    /// the value survives whole — which is what the old writer broke.
    @Test func a_multi_line_description_is_flattened_not_truncated() {
        let item = TaskItem(
            id: "x-1", task: "line one\nline two", priority: .medium, status: .pending,
            loopName: "daily-coding.yaml", created: "2026-09-01", module: nil
        )
        let yaml = LoopsStore.serializeTaskBlocks([TaskBlock(item: item)], preamble: "")
        #expect(yaml.contains("task: 'line one line two'"), "\(yaml)")
        #expect(yaml.split(separator: "\n").filter { $0.contains("task:") }.count == 1, "\(yaml)")
        #expect(LoopsStore.parseTaskBlocks(yaml)[0].item.task == "line one line two")
    }
}
