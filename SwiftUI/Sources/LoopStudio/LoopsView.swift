import AppKit
import SwiftUI

// MARK: - Loops & Kanban Canvas (AGENTS.md Autonomous Workflows)

public struct LoopsView: View {
    @ObservedObject var store: LoopsStore
    @Binding var sidebarVisibility: NavigationSplitViewVisibility

    @State private var isNewTaskPresented: Bool = false
    @State private var isDiffSheetPresented: Bool = false
    @State private var activeGitDiff: String = ""
    @State private var isDiffLoading: Bool = false
    /// Set when `git diff` could not be run or failed. A non-nil value is the
    /// opposite of a clean tree, and the sheet says so instead of drawing the
    /// green checkmark that sits above "Approve & Complete".
    @State private var gitDiffError: String? = nil
    @State private var isLogDrawerExpanded: Bool = true
    @State private var newTaskDescription: String = ""
    @State private var newTaskPriority: TaskPriority = .high
    @State private var newTaskLoop: String = "daily-coding.yaml"
    @State private var newTaskModule: String = ""
    @FocusState private var isTaskDescriptionFocused: Bool
    @FocusState private var isModuleFocused: Bool

    public init(
        store: LoopsStore,
        sidebarVisibility: Binding<NavigationSplitViewVisibility> = .constant(.all)
    ) {
        self.store = store
        self._sidebarVisibility = sidebarVisibility
    }

    public var body: some View {
        VStack(spacing: 0) {
            headerBar
            Divider().opacity(0.3)

            // Loop Pipeline Timeline
            if let loop = store.selectedLoop {
                loopPipelineBar(loop)
                Divider().opacity(0.25)
            }

            // Main Kanban Board
            kanbanBoardView
                .frame(maxWidth: .infinity, maxHeight: .infinity)

            // Execution & Human Gate Drawer
            if store.isRunningLoop || store.isGateAwaitingApproval || !store.runnerLogs.isEmpty {
                Divider().opacity(0.3)
                runnerDrawer
            }
        }
        .background(VisualEffectView().ignoresSafeArea())
        .sheet(isPresented: $isNewTaskPresented) {
            newTaskSheet
        }
        .sheet(isPresented: $isDiffSheetPresented) {
            gitDiffSheet
        }
    }

    // MARK: - Header Bar

    private var headerBar: some View {
        HStack(spacing: 12) {
            Button {
                withAnimation(LiquidGlass.spring) {
                    sidebarVisibility = (sidebarVisibility == .detailOnly) ? .all : .detailOnly
                }
                LiquidGlass.haptic(.alignment)
            } label: {
                Image(systemName: "sidebar.leading")
                    .font(.system(size: 13, weight: .medium))
                    .foregroundColor(.secondary)
                    .frame(width: 28, height: 28)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(Color.white.opacity(0.06))
                    )
            }
            .buttonStyle(.plain)
            .help("Toggle Sidebar (⌘B)")
            .keyboardShortcut("b", modifiers: .command)

            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text("Autonomous Loops")
                        .font(.system(size: 13, weight: .bold))
                    Text("KANBAN")
                        .font(.system(size: 9, weight: .bold, design: .monospaced))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Color.accentColor.opacity(0.2)))
                        .foregroundColor(.accentColor)
                }
                Text("Autonomous task queue & reliability gates (~/todo/loop-studio-tasks.yaml)")
                    .font(.system(size: 10))
                    .foregroundColor(.secondary)
            }

            Spacer()

            // Loop Selector Pill
            Menu {
                ForEach(store.loops) { loop in
                    Button(loop.name) {
                        store.selectedLoop = loop
                        LiquidGlass.haptic(.alignment)
                    }
                }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "arrow.triangle.2.circlepath")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundColor(.accentColor)
                    Text(store.selectedLoop?.name ?? "daily-coding.yaml")
                        .font(.system(size: 11, weight: .medium))
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9))
                        .foregroundColor(.secondary)
                }
                .claudeModelPill()
            }
            .menuStyle(.borderlessButton)
            .fixedSize()

            // New Task Button
            Button {
                isNewTaskPresented = true
                LiquidGlass.haptic(.alignment)
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: "plus")
                        .font(.system(size: 10, weight: .bold))
                    Text("New Task")
                        .font(.system(size: 11, weight: .medium))
                }
            }
            .controlSize(.small)
            .loopStudioGlass()

            // Run Top Pending Task
            Button {
                if let next = store.tasks(for: .pending).first {
                    store.executeLoop(task: next)
                }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "play.fill")
                        .font(.system(size: 9))
                    Text("Execute Loop")
                        .font(.system(size: 11, weight: .semibold))
                }
            }
            .controlSize(.small)
            .loopStudioGlassProminent()
            .disabled(store.isRunningLoop || store.tasks(for: .pending).isEmpty)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.ultraThinMaterial)
    }

    // MARK: - Loop Pipeline Timeline

    private func loopPipelineBar(_ loop: LoopDefinition) -> some View {
        HStack(spacing: 12) {
            Text(loop.description)
                .font(.system(size: 11))
                .foregroundColor(.secondary)
                .lineLimit(1)

            Spacer()

            HStack(spacing: 4) {
                ForEach(Array(loop.phases.enumerated()), id: \.offset) { idx, phase in
                    HStack(spacing: 3) {
                        if phase.isHumanGate {
                            Image(systemName: "hand.raised.fill")
                                .font(.system(size: 8))
                                .foregroundColor(.yellow)
                        }
                        Text(phase.name)
                            .font(.system(size: 9, weight: .semibold, design: .monospaced))
                            .foregroundColor(phaseColor(phase.name))
                    }
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        Capsule()
                            .fill(phaseBackground(phase.name))
                            .overlay(Capsule().strokeBorder(phaseBorder(phase.name), lineWidth: 0.5))
                    )

                    if idx < loop.phases.count - 1 {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 6, weight: .bold))
                            .foregroundColor(.secondary.opacity(0.35))
                    }
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 6)
        .background(Color.black.opacity(0.18))
    }

    private func phaseColor(_ name: String) -> Color {
        let active = store.currentRunnerPhase.rawValue.lowercased()
        if active.contains(name.lowercased()) {
            return .accentColor
        }
        return .primary.opacity(0.8)
    }

    private func phaseBackground(_ name: String) -> Color {
        let active = store.currentRunnerPhase.rawValue.lowercased()
        if active.contains(name.lowercased()) {
            return Color.accentColor.opacity(0.20)
        }
        return Color.white.opacity(0.04)
    }

    private func phaseBorder(_ name: String) -> Color {
        let active = store.currentRunnerPhase.rawValue.lowercased()
        if active.contains(name.lowercased()) {
            return Color.accentColor.opacity(0.50)
        }
        return Color.white.opacity(0.08)
    }

    // MARK: - Kanban Board

    private var kanbanBoardView: some View {
        HStack(alignment: .top, spacing: 12) {
            kanbanColumn(status: .pending, title: "Task Queue", color: .secondary)
            kanbanColumn(status: .inProgress, title: "In Progress", color: .blue)
            kanbanColumn(status: .reviewGate, title: "Review & Gate", color: .orange)
            kanbanColumn(status: .completed, title: "Completed", color: .green)
            // `TaskStatus.failed` existed with no column, so a task moved there
            // left the board entirely — the one status an operator most needs
            // to see is the one that disappeared.
            kanbanColumn(status: .failed, title: "Failed", color: .red)
        }
        .padding(14)
    }

    private func kanbanColumn(status: TaskStatus, title: String, color: Color) -> some View {
        let list = store.tasks(for: status)

        return VStack(alignment: .leading, spacing: 10) {
            // Column Header
            HStack(spacing: 6) {
                Image(systemName: status.icon)
                    .font(.system(size: 11))
                    .foregroundColor(color)

                Text(title)
                    .font(.system(size: 12, weight: .bold))
                    .foregroundColor(.primary)

                Spacer()

                Text("\(list.count)")
                    .font(.system(size: 10, weight: .bold, design: .monospaced))
                    .foregroundColor(color)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(Capsule().fill(color.opacity(0.15)))
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.white.opacity(0.04))
            )

            // Cards Scroll
            ScrollView(.vertical, showsIndicators: false) {
                LazyVStack(spacing: 8) {
                    if list.isEmpty {
                        VStack(spacing: 4) {
                            Spacer().frame(height: 30)
                            Text("No tasks")
                                .font(.system(size: 11))
                                .foregroundColor(.secondary.opacity(0.5))
                            Spacer().frame(height: 30)
                        }
                        .frame(maxWidth: .infinity)
                    } else {
                        ForEach(list) { task in
                            taskCard(task)
                        }
                    }
                }
            }
        }
        .padding(8)
        .frame(minWidth: 200, maxWidth: .infinity)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(Color.black.opacity(0.18))
                .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Color.white.opacity(0.06), lineWidth: 1))
        )
    }

    private func taskCard(_ task: TaskItem) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            // Card Header: ID & Priority
            HStack(spacing: 6) {
                Text(task.id)
                    .font(.system(size: 10.5, weight: .bold, design: .monospaced))
                    .foregroundColor(.secondary)

                if let mod = task.module {
                    Text(mod)
                        .font(.system(size: 9, design: .monospaced))
                        .foregroundColor(.cyan)
                        .padding(.horizontal, 4)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Color.cyan.opacity(0.12)))
                }

                Spacer()

                Text(task.priority.label)
                    .font(.system(size: 9, weight: .bold))
                    .foregroundColor(task.priority.color)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1.5)
                    .background(Capsule().fill(task.priority.color.opacity(0.15)))
            }

            // Task Content
            Text(task.task)
                .font(.system(size: 12, weight: .medium))
                .foregroundColor(.primary.opacity(0.95))
                .lineLimit(3)
                .fixedSize(horizontal: false, vertical: true)

            // Footer: Loop Pill & Action Buttons
            HStack {
                HStack(spacing: 3) {
                    Image(systemName: "arrow.triangle.2.circlepath")
                        .font(.system(size: 8))
                    Text(task.loopName.replacingOccurrences(of: ".yaml", with: ""))
                        .font(.system(size: 9, design: .monospaced))
                }
                .foregroundColor(.secondary)

                Spacer()

                // Move Task Menu
                Menu {
                    ForEach(TaskStatus.allCases) { s in
                        if s != task.status {
                            Button("Move to \(s.title)") {
                                Task { await store.moveTask(id: task.id, to: s) }
                            }
                        }
                    }
                    Divider()
                    Button("Delete Task", role: .destructive) {
                        Task { await store.deleteTask(id: task.id) }
                    }
                } label: {
                    Image(systemName: "ellipsis")
                        .font(.system(size: 9, weight: .bold))
                        .foregroundColor(.secondary)
                        .frame(width: 20, height: 20)
                        .background(Circle().fill(Color.white.opacity(0.06)))
                }
                .menuStyle(.borderlessButton)
                .fixedSize()

                // Execute Loop for this card
                if task.status != .completed {
                    Button {
                        store.executeLoop(task: task)
                    } label: {
                        Image(systemName: "play.circle.fill")
                            .font(.system(size: 13))
                            .foregroundColor(.accentColor)
                    }
                    .buttonStyle(.plain)
                    .help("Execute autonomous loop on this task")
                }
            }
        }
        .padding(10)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color.black.opacity(0.35))
                .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.10), lineWidth: 0.8))
        )
    }

    // MARK: - Runner & Human Gate Drawer

    private var runnerDrawer: some View {
        VStack(spacing: 0) {
            // Drawer Header Bar
            HStack(spacing: 8) {
                Image(systemName: store.currentRunnerPhase.icon)
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundColor(store.isGateAwaitingApproval ? .yellow : .accentColor)

                Text("Loop Runner: \(store.currentRunnerPhase.rawValue)")
                    .font(.system(size: 12, weight: .bold))

                if store.isRunningLoop {
                    ProgressView().scaleEffect(0.6)
                }

                Spacer()

                if store.isGateAwaitingApproval {
                    HStack(spacing: 8) {
                        Text("Human Gate Gatekeeper: git diff review required")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundColor(.yellow)

                        Button {
                            loadGitDiff()
                        } label: {
                            HStack(spacing: 4) {
                                if isDiffLoading {
                                    ProgressView().controlSize(.mini)
                                } else {
                                    Image(systemName: "doc.text.magnifyingglass")
                                        .font(.system(size: 10))
                                }
                                Text("Inspect Git Diff")
                                    .font(.system(size: 11, weight: .semibold))
                            }
                        }
                        .controlSize(.small)
                        .loopStudioGlass()
                        .disabled(isDiffLoading)

                        Button {
                            store.cancelLoop()
                        } label: {
                            Text("Reject")
                                .font(.system(size: 11))
                                .foregroundColor(.red)
                        }
                        .controlSize(.small)
                        .loopStudioGlass()

                        Button {
                            Task { await store.approveHumanGate() }
                        } label: {
                            HStack(spacing: 4) {
                                Image(systemName: "checkmark.seal.fill")
                                    .font(.system(size: 10))
                                Text("Approve & Complete")
                                    .font(.system(size: 11, weight: .bold))
                            }
                        }
                        .controlSize(.small)
                        .loopStudioGlassProminent()
                    }
                } else {
                    Button {
                        withAnimation(LiquidGlass.spring) {
                            isLogDrawerExpanded.toggle()
                        }
                    } label: {
                        Image(systemName: isLogDrawerExpanded ? "chevron.down" : "chevron.up")
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundColor(.secondary)
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(Color.black.opacity(0.30))

            // Log output area
            if isLogDrawerExpanded {
                ScrollView(.vertical) {
                    Text(store.runnerLogs)
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundColor(.primary.opacity(0.85))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(12)
                        .textSelection(.enabled)
                }
                .frame(height: 160)
                .background(Color.black.opacity(0.50))
            }
        }
    }

    // MARK: - New Task Sheet

    private var newTaskSheet: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("Schedule Autonomous Task")
                    .font(.system(size: 14, weight: .bold))
                Spacer()
                Button("Cancel") {
                    isNewTaskPresented = false
                }
                .buttonStyle(.plain)
                .foregroundColor(.secondary)
            }

            Divider().opacity(0.3)

            VStack(alignment: .leading, spacing: 5) {
                Text("Task Description (Scope Tightly per AGENTS.md)")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundColor(.secondary)
                TextEditor(text: $newTaskDescription)
                    .font(.system(size: 12, design: .monospaced))
                    .frame(height: 72)
                    .padding(8)
                    .background(
                        RoundedRectangle(cornerRadius: 8, style: .continuous)
                            .fill(Color.black.opacity(0.20))
                            .overlay(
                                RoundedRectangle(cornerRadius: 8, style: .continuous)
                                    .strokeBorder(
                                        isTaskDescriptionFocused
                                            ? LinearGradient(colors: [Color.accentColor, Color.purple], startPoint: .topLeading, endPoint: .bottomTrailing)
                                            : LinearGradient(colors: [Color.white.opacity(0.18), Color.white.opacity(0.06)], startPoint: .topLeading, endPoint: .bottomTrailing),
                                        lineWidth: isTaskDescriptionFocused ? 1.5 : 1
                                    )
                            )
                    )
                    .focused($isTaskDescriptionFocused)
            }

            HStack(spacing: 16) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Priority")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundColor(.secondary)
                    Picker("", selection: $newTaskPriority) {
                        ForEach(TaskPriority.allCases) { p in
                            Text(p.label).tag(p)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(width: 220)
                }

                VStack(alignment: .leading, spacing: 4) {
                    Text("Target Loop")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundColor(.secondary)
                    Picker("", selection: $newTaskLoop) {
                        ForEach(store.loops) { l in
                            Text(l.name).tag(l.name)
                        }
                    }
                    .frame(width: 170)
                }
            }

            VStack(alignment: .leading, spacing: 5) {
                Text("Target Module / Scope (Optional)")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundColor(.secondary)
                TextField("e.g. rust-src/gateway or SwiftUI/Chat", text: $newTaskModule)
                    .textFieldStyle(.plain)
                    .font(.system(size: 12))
                    .focused($isModuleFocused)
                    .liquidGlassTextField(cornerRadius: 8, isFocused: isModuleFocused)
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    isNewTaskPresented = false
                }
                .keyboardShortcut(.cancelAction)
                .loopStudioGlass()

                Button("Schedule Task") {
                    // The typed text and the sheet survive a failed write. The
                    // queue write is off the main actor now, so clearing first
                    // would lose the description on a lock the worker happens to
                    // be holding, leaving only a banner.
                    let description = newTaskDescription
                    let module = newTaskModule
                    let priority = newTaskPriority
                    let loop = newTaskLoop
                    Task {
                        if await store.addTask(
                            taskDescription: description,
                            priority: priority,
                            loopName: loop,
                            module: module
                        ) {
                            newTaskDescription = ""
                            newTaskModule = ""
                            isNewTaskPresented = false
                        }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .controlSize(.regular)
                .loopStudioGlassProminent()
                .disabled(newTaskDescription.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(22)
        .frame(width: 480)
        .liquidGlassModalCard(cornerRadius: 16)
        .onAppear {
            isTaskDescriptionFocused = true
        }
    }

    // MARK: - Git Diff Audit Sheet

    private var gitDiffSheet: some View {
        VStack(spacing: 0) {
            // Header
            HStack(spacing: 10) {
                Image(systemName: "arrow.triangle.branch")
                    .font(.system(size: 13, weight: .bold))
                    .foregroundColor(.accentColor)

                VStack(alignment: .leading, spacing: 1) {
                    Text("Phase 5 Human Gate: Working Tree Diff Review")
                        .font(.system(size: 13, weight: .bold))
                    Text("Verify changes uncommitted in working tree before approving completion (AGENTS.md mandatory gate)")
                        .font(.system(size: 10))
                        .foregroundColor(.secondary)
                }

                Spacer()

                Button {
                    loadGitDiff()
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                }
                .buttonStyle(.plain)
                .help("Refresh Diff")

                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(activeGitDiff, forType: .string)
                    LiquidGlass.haptic(.alignment)
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "doc.on.doc")
                            .font(.system(size: 10))
                        Text("Copy Diff")
                            .font(.system(size: 11))
                    }
                }
                .controlSize(.small)
                .loopStudioGlass()

                Button {
                    isDiffSheetPresented = false
                    Task { await store.approveHumanGate() }
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "checkmark.seal.fill")
                            .font(.system(size: 10))
                        Text("Approve Gate")
                            .font(.system(size: 11, weight: .bold))
                    }
                }
                .keyboardShortcut(.defaultAction)
                .controlSize(.small)
                .loopStudioGlassProminent()

                Button {
                    isDiffSheetPresented = false
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 14))
                        .foregroundColor(.secondary)
                }
                .keyboardShortcut(.cancelAction)
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .background(.ultraThinMaterial)
            .overlay(Divider().opacity(0.3), alignment: .bottom)

            // Diff Body
            ScrollView(.vertical, showsIndicators: true) {
                if let failure = gitDiffError {
                    VStack(spacing: 8) {
                        Spacer().frame(height: 60)
                        Image(systemName: "exclamationmark.triangle.fill")
                            .font(.system(size: 28))
                            .foregroundColor(.orange)
                        Text("Could not read the working tree.")
                            .font(.system(size: 13, weight: .semibold))
                        Text(failure)
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundColor(.secondary)
                            .multilineTextAlignment(.center)
                            .textSelection(.enabled)
                        Text("Nothing has been verified. Approving now would mark the task complete on no evidence.")
                            .font(.system(size: 11))
                            .foregroundColor(.secondary)
                            .multilineTextAlignment(.center)
                        Spacer().frame(height: 60)
                    }
                    .frame(maxWidth: .infinity)
                } else if activeGitDiff.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    VStack(spacing: 8) {
                        Spacer().frame(height: 60)
                        Image(systemName: "checkmark.circle.fill")
                            .font(.system(size: 28))
                            .foregroundColor(.green)
                        Text("Working tree is completely clean.")
                            .font(.system(size: 13, weight: .semibold))
                        Text("No unstaged or uncommitted changes detected in repository.")
                            .font(.system(size: 11))
                            .foregroundColor(.secondary)
                        Spacer().frame(height: 60)
                    }
                    .frame(maxWidth: .infinity)
                } else {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(Array(activeGitDiff.components(separatedBy: "\n").enumerated()), id: \.offset) { _, line in
                            let isAdd = line.hasPrefix("+") && !line.hasPrefix("+++")
                            let isDel = line.hasPrefix("-") && !line.hasPrefix("---")
                            let isHunk = line.hasPrefix("@@")
                            let isHeader = line.hasPrefix("diff --git") || line.hasPrefix("index ")

                            HStack(spacing: 8) {
                                Text(line)
                                    .font(.system(size: 11, design: .monospaced))
                                    .foregroundColor(
                                        isAdd ? .green :
                                        isDel ? .red :
                                        isHunk ? .cyan :
                                        isHeader ? .yellow : .primary.opacity(0.9)
                                    )
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .padding(.horizontal, 8)
                            .padding(.vertical, 1)
                            .background(
                                isAdd ? Color.green.opacity(0.12) :
                                isDel ? Color.red.opacity(0.12) :
                                isHunk ? Color.cyan.opacity(0.08) :
                                isHeader ? Color.yellow.opacity(0.08) : Color.clear
                            )
                        }
                    }
                    .padding(10)
                }
            }
            .background(Color.black.opacity(0.40))
        }
        .frame(width: 740, height: 520)
        .liquidGlassModalCard(cornerRadius: 16)
    }

    /// The gate's evidence, and the only thing standing between a loop task and
    /// a `completed` status.
    ///
    /// Three things were wrong with the old version, and all three showed up as
    /// a green checkmark:
    ///
    /// 1. `waitUntilExit()` ran *before* the pipe was drained. A `git diff`
    ///    passes 64 KiB — about 1,500 lines, which any refactor reaches — and
    ///    the child then blocks in `write()` while the parent blocks in
    ///    `wait()`. Nothing times out, `isDiffLoading` stays true, and the
    ///    button that opens this sheet is disabled, so the gate could never be
    ///    inspected again. The drain comes first now: reading to EOF ends when
    ///    the child exits, and the wait that follows returns immediately.
    /// 2. The exit status was never read. Launched from Finder, Dock or
    ///    Spotlight, `currentDirectoryPath` is `/`, `git diff` fails with
    ///    `fatal: not a git repository` on stderr (which was piped and never
    ///    read), stdout is empty, and the empty diff was rendered as "Working
    ///    tree is completely clean." — above a one-click "Approve Gate" that
    ///    marks the task complete and logs that the changes were verified.
    /// 3. The working directory was the process's, not the workspace's.
    private func loadGitDiff() {
        isDiffLoading = true
        let root = workspaceRoot()

        Task.detached(priority: .userInitiated) {
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: "/usr/bin/git")
            proc.arguments = ["diff"]
            proc.currentDirectoryURL = root

            let out = Pipe()
            let err = Pipe()
            proc.standardOutput = out
            proc.standardError = err

            do {
                try proc.run()
            } catch {
                await MainActor.run {
                    self.activeGitDiff = ""
                    self.gitDiffError = "Could not run git: \(error.localizedDescription)"
                    self.isDiffLoading = false
                    self.isDiffSheetPresented = true
                }
                return
            }
            // Drain first (see 1), then collect the child.
            let outData = out.fileHandleForReading.readDataToEndOfFile()
            let errData = err.fileHandleForReading.readDataToEndOfFile()
            proc.waitUntilExit()

            let stderr = String(data: errData, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            await MainActor.run {
                if proc.terminationStatus != 0 {
                    // A failed audit is not a clean tree, and must never be
                    // presented as one next to an Approve button.
                    self.activeGitDiff = ""
                    self.gitDiffError = stderr.isEmpty
                        ? "git diff exited \(proc.terminationStatus)"
                        : stderr
                } else {
                    self.gitDiffError = nil
                    self.activeGitDiff = String(data: outData, encoding: .utf8) ?? ""
                }
                self.isDiffLoading = false
                self.isDiffSheetPresented = true
                LiquidGlass.haptic(.alignment)
            }
        }
    }

    /// The repository the loops actually run in. `currentDirectoryPath` is the
    /// process's working directory, which is `/` for a Dock launch and the
    /// installer's directory otherwise; neither is the checkout. The repo
    /// root is found once by walking up from here, which is right for an app
    /// opened in a checkout and degrades to the process directory otherwise —
    /// and, either way, the exit status now decides what the sheet claims.
    private func workspaceRoot() -> URL {
        let fm = FileManager.default
        var candidate = URL(fileURLWithPath: fm.currentDirectoryPath)
        for _ in 0..<8 {
            var isDirectory: ObjCBool = false
            if fm.fileExists(atPath: candidate.appendingPathComponent(".git").path, isDirectory: &isDirectory) {
                return candidate
            }
            let parent = candidate.deletingLastPathComponent().path
            if parent.isEmpty || parent == candidate.path {
                break
            }
            candidate = URL(fileURLWithPath: parent)
        }
        return URL(fileURLWithPath: fm.currentDirectoryPath)
    }
}
