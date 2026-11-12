import SwiftUI
import Foundation
import Charts

// This app is a viewer. It reads through `tokeman list --json` (everything
// tokeman has stored, read without a request or a write), `tokeman history
// --json` and `tokeman settings show`, and never opens tokens.toml itself.
// The background service keeps those stores fresh; only when no service is
// installed does the app probe on its own interval, as `tokeman` would.
// It changes things only through `tokeman settings set` and, for a Launch
// button, `tokeman rotate use`.

// MARK: - JSON models (match `tokeman list --json`)

struct Window: Codable, Equatable {
    let utilization: Double
    let reset: Int64

    var remaining: Double { max(0, min(1, 1.0 - utilization)) }
}

/// One limit of one account, as tokeman's `quota::Limit` serializes: a
/// window of some length (`span`) over the whole account, one model or one
/// named group (`scope`).
struct Limit: Decodable {
    /// `5h`, `weekly`, `monthly`, or another length in words.
    let span: String
    /// The group the limit is scoped to (Antigravity's model families).
    let group: String?
    /// The model the limit is scoped to.
    let model: String?
    /// The server's name for the limit, or the span's when it gave none.
    let label: String
    let window: Window
    let amount: Amount?

    /// Whether the limit covers the whole account.
    var isShared: Bool { group == nil && model == nil }

    struct Amount: Decodable {
        let used: Double
        let limit: Double
        let unit: String
    }

    private enum Keys: String, CodingKey { case span, scope, label, window, amount }
    private enum ScopeKeys: String, CodingKey { case model, group }
    private enum SpanKeys: String, CodingKey { case other }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        if let named = try? c.decode(String.self, forKey: .span) {
            switch named {
            case "five_hour": span = "5h"
            case "unknown": span = "window"
            default: span = named
            }
        } else {
            let other = try c.nestedContainer(keyedBy: SpanKeys.self, forKey: .span)
            span = Limit.spanText(try other.decode(Int64.self, forKey: .other))
        }
        if let scope = try? c.nestedContainer(keyedBy: ScopeKeys.self, forKey: .scope) {
            group = try scope.decodeIfPresent(String.self, forKey: .group)
            model = try scope.decodeIfPresent(String.self, forKey: .model)
        } else {
            group = nil
            model = nil
        }
        let given = try c.decodeIfPresent(String.self, forKey: .label) ?? ""
        label = given.isEmpty ? span : given
        window = try c.decode(Window.self, forKey: .window)
        amount = try c.decodeIfPresent(Amount.self, forKey: .amount)
    }

    /// `daily`, `2d`, `90m`: a length in its largest whole unit.
    private static func spanText(_ seconds: Int64) -> String {
        if seconds == 86_400 { return "daily" }
        for (size, suffix) in [(Int64(86_400), "d"), (3_600, "h"), (60, "m")] where seconds % size == 0 {
            return "\(seconds / size)\(suffix)"
        }
        return "\(seconds)s"
    }
}

/// Why a read produced no quota, as tokeman's `quota::ReadError` serializes,
/// in words.
struct ReadFailure: Decodable {
    let text: String

    private enum Keys: String, CodingKey { case kind, message, status, body, attempts }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let kind = try c.decode(String.self, forKey: .kind)
        let message = try c.decodeIfPresent(String.self, forKey: .message)
        let status = try c.decodeIfPresent(Int.self, forKey: .status)
        switch kind {
        case "no_credential": text = "no usable credential"
        case "network": text = "network: \(message ?? "")"
        case "http":
            text = "HTTP \(status ?? 0): \(try c.decodeIfPresent(String.self, forKey: .body) ?? "")"
        case "edge": text = "HTTP \(status ?? 0): Cloudflare challenge, not an auth failure"
        default: text = message ?? kind
        }
    }
}

struct Overview: Decodable {
    let at: Int64
    /// `running`, `paused` or `not-installed`.
    let monitor: String
    let default_account: String?
    let target_model: String?
    let claude: [ClaudeAccount]
    let resets_on_offer: [ResetOffer]
    let codex: [CodexAccount]
    let services: [ServiceAccount]
}

struct ClaudeAccount: Codable, Identifiable {
    var id: String { name }
    let name: String
    let is_default: Bool
    let credential: String
    let login: String
    let login_note: String?
    let login_warning: Bool
    let login_ends_at: Int64?
    let reading: Reading?

    struct Reading: Codable {
        let at: Int64
        let status: String?
        let error: String?
        let session: Window?
        /// `present`, `absent` (the account has no 5h limit) or `unknown`.
        let session_limit: String
        let weekly: Window?
        let overage: Window?
        let buckets: [Bucket]
        let notes: [String]
    }

    struct Bucket: Codable, Identifiable {
        var id: String { label }
        let label: String
        let short: String
        let window: Window
        let source: String
        /// The limit's size as a fraction of the weekly limit, when it is one.
        let share: Double?
        /// Room for the model as a fraction of the weekly limit.
        let room: Double
    }
}

struct ResetOffer: Codable, Identifiable {
    var id: String { "\(account)-\(offer)" }
    let account: String
    let offer: String
    let read_at: Int64
}

struct NamedWindow: Codable, Identifiable {
    var id: String { label }
    let label: String
    let window: Window
}

struct CodexAccount: Codable, Identifiable {
    var id: String { name }
    let name: String
    let label: String?
    let parked: Bool
    let reading: Reading?

    struct Reading: Codable {
        let at: Int64
        let email: String?
        let plan: String?
        let state: String
        let viable: Bool
        let windows: [NamedWindow]
        let extra: [Extra]
        let reset_credits: ResetCredits?
    }

    struct Extra: Codable, Identifiable {
        var id: String { name }
        let name: String
        let limited: Bool
        let windows: [NamedWindow]
    }

    struct ResetCredits: Codable {
        let available: Int
        let applicable: Int?
    }
}

struct ServiceAccount: Decodable, Identifiable {
    var id: String { "\(service)-\(name)" }
    let name: String
    let service: String
    let service_label: String
    let summary: String?
    /// The stored reading, as tokeman's `quota::Reading` serializes.
    let usage: Usage?

    struct Usage: Decodable {
        let at: Int64
        let plan: String?
        let limits: [Limit]
        let balances: [Balance]?
        let error: ReadFailure?
    }

    struct Balance: Decodable {
        let label: String
        let left_minor: Int64
        let total_minor: Int64?
        let currency: String
    }
}

// MARK: - History model (matches `tokeman history --json`)

/// One stored Claude reading (`quota::Reading`), read for its windows.
struct HistorySnapshot: Decodable, Identifiable {
    var id: String { "\(account)-\(at)" }
    let account: String
    /// Epoch seconds.
    let at: Int64
    let limits: [Limit]
    let detail: Detail?

    struct Detail: Decodable {
        let claude: Claude?
    }

    struct Claude: Decodable {
        let overage: Overage?
    }

    struct Overage: Decodable {
        let window: Window?
    }

    var date: Date { Date(timeIntervalSince1970: TimeInterval(at)) }

    private func shared(_ span: String) -> Window? {
        limits.first(where: { $0.isShared && $0.span == span })?.window
    }

    var session: Window? { shared("5h") }
    var weekly: Window? { shared("weekly") }
    var overage: Window? { detail?.claude?.overage?.window }
    /// The per-model limits.
    var buckets: [Limit] { limits.filter { $0.model != nil } }

    /// Percent left in the named series: `7d`, `5h`, `$$` or a bucket label.
    func remaining(for series: String) -> Double? {
        let window: Window?
        switch series {
        case "7d": window = weekly
        case "5h": window = session
        case "$$": window = overage
        default: window = buckets.first(where: { $0.label == series })?.window
        }
        return window.map { $0.remaining * 100 }
    }
}

// MARK: - Launch settings (from `tokeman settings show`)

struct LaunchSettings {
    var launchArgs: [String] = []
    var dangerousMode = false
    var terminal: String? = nil
    var claudeBin: String? = nil
    var probeIntervalSecs = 30

    /// Parse the `key = value` lines `tokeman settings show` prints.
    static func parse(_ output: String) -> LaunchSettings {
        var settings = LaunchSettings()
        for line in output.split(separator: "\n") {
            guard let eq = line.range(of: " = ") ?? line.range(of: " =") else { continue }
            let key = String(line[..<eq.lowerBound])
            let value = String(line[eq.upperBound...]).trimmingCharacters(in: .whitespaces)
            switch key {
            case "launch_args":
                settings.launchArgs = value.split(separator: " ").map(String.init)
            case "dangerous_mode": settings.dangerousMode = value == "true"
            case "terminal": settings.terminal = value.isEmpty ? nil : value
            case "claude_bin": settings.claudeBin = value.isEmpty ? nil : value
            case "probe_interval_secs": settings.probeIntervalSecs = Int(value) ?? 30
            default: break
            }
        }
        return settings
    }
}

// MARK: - Text helpers

func levelColor(_ remaining: Double) -> Color {
    if remaining > 0.5 { return .green }
    if remaining > 0.2 { return .orange }
    return .red
}

/// `3h20m`, `Tue 4:00PM`, `now`, or `nil` for a window with no reset pending.
func resetText(_ reset: Int64) -> String? {
    if reset == 0 { return nil }
    let diff = Double(reset) - Date().timeIntervalSince1970
    if diff <= 0 { return "now" }
    let h = Int(diff) / 3600
    let m = (Int(diff) % 3600) / 60
    if h > 24 {
        let f = DateFormatter()
        f.dateFormat = "EEE h:mma"
        return f.string(from: Date(timeIntervalSince1970: Double(reset)))
    }
    return h > 0 ? "\(h)h\(m)m" : "\(m)m"
}

func ageText(_ at: Int64) -> String {
    let secs = max(0, Int(Date().timeIntervalSince1970) - Int(at))
    if secs < 60 { return "just now" }
    if secs < 3600 { return "\(secs / 60)m ago" }
    if secs < 86_400 { return "\(secs / 3600)h ago" }
    return "\(secs / 86_400)d ago"
}

func moneyText(_ minor: Int64, _ currency: String) -> String {
    let amount = String(format: "%lld.%02lld", abs(minor) / 100, abs(minor) % 100)
    let sign = minor < 0 ? "-" : ""
    return currency == "USD" || currency.isEmpty ? "\(sign)$\(amount)" : "\(sign)\(amount) \(currency)"
}

// MARK: - View model

@MainActor
class TokemanViewModel: ObservableObject {
    @Published var overview: Overview?
    @Published var isLoading = false
    @Published var isProbing = false
    @Published var lastLoad: Date?
    @Published var loadError: String?
    @Published var settings = LaunchSettings()
    @Published var showSettings = false
    @Published var history: [String: [HistorySnapshot]] = [:]

    private var timer: Timer?
    private var lastProbe: Date?
    private var lastHistoryLoad: Date?
    /// A day of history is megabytes of JSON; the charts move slowly.
    private let historyInterval: TimeInterval = 300
    /// The stored view is local and cheap to read.
    private let reloadInterval: TimeInterval = 15

    /// `polls: false` is for drawing one picture of the stored view.
    let polls: Bool

    init(polls: Bool = true) {
        self.polls = polls
        // The menu-bar icon shows the fleet's state before the first click.
        if polls { Task { @MainActor in self.start() } }
    }

    /// Whether anything else keeps the stored reads fresh.
    var serviceInstalled: Bool { overview?.monitor != "not-installed" }

    /// Weekly room of the best Claude account whose last reading allows work.
    var bestWeeklyRemaining: Double? {
        overview?.claude.compactMap { account -> Double? in
            guard let reading = account.reading, reading.error == nil,
                  reading.status != "rejected" else { return nil }
            return reading.weekly?.remaining
        }.max()
    }

    var statusIcon: String {
        guard overview != nil else { return loadError == nil ? "bolt.fill" : "bolt.slash.fill" }
        guard let best = bestWeeklyRemaining else { return "bolt.slash.fill" }
        return best > 0.2 ? "bolt.fill" : "bolt.trianglebadge.exclamationmark.fill"
    }

    var statusColor: Color {
        guard let best = bestWeeklyRemaining else { return .gray }
        return levelColor(best)
    }

    func start() {
        guard polls, timer == nil else { return }
        reload()
        timer = Timer.scheduledTimer(withTimeInterval: reloadInterval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.reload() }
        }
    }

    /// Read the stored view, the history and the settings. No request is
    /// made and nothing is written.
    func reload() {
        guard !isLoading else { return }
        isLoading = true
        Task {
            await load()
            isLoading = false
            probeIfNobodyElseDoes()
        }
    }

    func load() async {
        let bin = findTokeman()
        let output = await shell(bin, args: ["list", "--json"])
        if output.isEmpty {
            loadError = "Could not run tokeman (looked at \(bin)). Install it with: cargo install --path ."
        } else if let data = output.data(using: .utf8) {
            do {
                overview = try JSONDecoder().decode(Overview.self, from: data)
                lastLoad = Date()
                loadError = nil
            } catch {
                loadError = "This tokeman does not print the stored view (`tokeman list --json`): \(error)"
            }
        }
        settings = LaunchSettings.parse(await shell(bin, args: ["settings", "show"]))
        await loadHistory(bin)
    }

    private func loadHistory(_ bin: String) async {
        if let last = lastHistoryLoad, Date().timeIntervalSince(last) < historyInterval { return }
        lastHistoryLoad = Date()
        let output = await shell(bin, args: ["history", "--json", "--since", "24"])
        guard let data = output.data(using: .utf8),
              let snapshots = try? JSONDecoder().decode([HistorySnapshot].self, from: data) else {
            return
        }
        var grouped: [String: [HistorySnapshot]] = [:]
        for snapshot in snapshots {
            grouped[snapshot.account, default: []].append(snapshot)
        }
        for key in grouped.keys {
            grouped[key]?.sort { $0.date < $1.date }
        }
        history = grouped
    }

    /// With no background service nothing refreshes the stores, so the app
    /// probes on the configured interval, exactly as running `tokeman` does.
    private func probeIfNobodyElseDoes() {
        guard overview != nil, !serviceInstalled, !isProbing else { return }
        let interval = TimeInterval(max(15, settings.probeIntervalSecs))
        if let last = lastProbe, Date().timeIntervalSince(last) < interval { return }
        probe()
    }

    /// Probe every Claude account now (`tokeman --json`), then show the result.
    func probe() {
        guard !isProbing else { return }
        isProbing = true
        lastProbe = Date()
        Task {
            _ = await shell(findTokeman(), args: ["--json"])
            isProbing = false
            reload()
        }
    }

    func toggleDangerMode() {
        let enable = !settings.dangerousMode
        // Move the switch at once; the reload below confirms what was saved.
        settings.dangerousMode = enable
        Task {
            await setSetting("dangerous_mode", enable ? "true" : "false")
            reload()
        }
    }

    func saveSettings(launchArgs: String, terminal: String, claudeBin: String, probeInterval: String) {
        Task {
            await setSetting("launch_args", launchArgs)
            await setSetting("terminal", terminal)
            await setSetting("claude_bin", claudeBin)
            await setSetting("probe_interval_secs", probeInterval)
            reload()
        }
    }

    /// Every write to tokens.toml goes through tokeman itself: its writer
    /// takes the config lock and keeps comments and every field.
    private func setSetting(_ key: String, _ value: String) async {
        let output = await shell(findTokeman(), args: ["settings", "set", key, value])
        if !output.hasPrefix("set ") {
            fputs("tokeman tray: could not set \(key): \(output)\n", stderr)
        }
    }

    private var claudeCommand: String {
        let bin = settings.claudeBin ?? "claude"
        var args = settings.launchArgs
        if settings.dangerousMode && !args.contains("--dangerously-skip-permissions") {
            args.append("--dangerously-skip-permissions")
        }
        return ([bin] + args).map(shellQuote).joined(separator: " ")
    }

    /// Make `name` the default, then open Claude in a terminal.
    func launchAccount(_ name: String) {
        guard overview?.claude.contains(where: { $0.name == name }) == true else { return }
        let cmd = claudeCommand
        Task {
            let result = await shell(findTokeman(), args: ["rotate", "use", name])
            guard !result.isEmpty else {
                loadError = "Could not activate \(name) through tokeman"
                return
            }
            launchTerminal(cmd: cmd, terminal: settings.terminal)
        }
    }

    /// Open Claude on whichever account the rotation policy picks.
    func launchBest() {
        launchTerminal(cmd: "\(shellQuote(findTokeman())) launch", terminal: settings.terminal)
    }

    // --- helpers ---

    private func findTokeman() -> String {
        let candidates = [
            ProcessInfo.processInfo.environment["TOKEMAN_BIN"],
            "\(NSHomeDirectory())/.cargo/bin/tokeman",
            "/usr/local/bin/tokeman",
            "/opt/homebrew/bin/tokeman",
        ]
        for p in candidates.compactMap({ $0 }) {
            if FileManager.default.isExecutableFile(atPath: p) { return p }
        }
        if let resolved = shellWhich("tokeman") { return resolved }
        return "tokeman"
    }

    private func shellWhich(_ cmd: String) -> String? {
        let proc = Process()
        let pipe = Pipe()
        proc.executableURL = URL(fileURLWithPath: "/bin/zsh")
        proc.arguments = ["-lc", "which \(cmd)"]
        proc.standardOutput = pipe
        proc.standardError = FileHandle.nullDevice
        do {
            try proc.run()
            proc.waitUntilExit()
        } catch { return nil }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        guard let path = String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines),
              !path.isEmpty,
              FileManager.default.isExecutableFile(atPath: path) else { return nil }
        return path
    }

    private func shell(_ path: String, args: [String]) async -> String {
        await runTokeman(path, args: args)
    }

    private func shellQuote(_ value: String) -> String {
        "'\(value.replacingOccurrences(of: "'", with: "'\\''"))'"
    }

    private func appleScriptQuote(_ value: String) -> String {
        value.replacingOccurrences(of: "\\", with: "\\\\")
             .replacingOccurrences(of: "\"", with: "\\\"")
    }

    private func launchTerminal(cmd: String, terminal: String?) {
        let app = terminal ?? "Terminal"

        let script: String
        switch app.lowercased() {
        case "iterm2", "iterm":
            let badge = "printf '\\033]1337;SetBadgeFormat=dG9rZW1hbiDCtyBtYW5hZ2Vk\\007'; "
            let escaped_cmd = appleScriptQuote(badge + cmd)
            script = """
            tell application "iTerm2"
                activate
                create window with default profile
                tell current session of current window
                    set name to "Claude · tokeman"
                    write text "\(escaped_cmd)"
                end tell
            end tell
            """
        default:
            let escaped_cmd = appleScriptQuote(cmd)
            script = """
            tell application "Terminal"
                activate
                do script "\(escaped_cmd)"
            end tell
            """
        }
        Process.launchedProcess(launchPath: "/usr/bin/osascript", arguments: ["-e", script])
    }
}

/// Run tokeman and return its stdout; empty when it could not run. The
/// output is drained before the wait, so a large answer cannot fill the pipe
/// and stall the child.
func runTokeman(_ path: String, args: [String]) async -> String {
    await withCheckedContinuation { cont in
        DispatchQueue.global().async {
            let proc = Process()
            let pipe = Pipe()
            proc.executableURL = URL(fileURLWithPath: path)
            proc.arguments = args
            proc.standardOutput = pipe
            proc.standardError = FileHandle.nullDevice
            do {
                try proc.run()
            } catch {
                cont.resume(returning: "")
                return
            }
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            proc.waitUntilExit()
            cont.resume(returning: String(data: data, encoding: .utf8) ?? "")
        }
    }
}

// MARK: - Views

struct GaugeRow: View {
    let label: String
    var labelWidth: CGFloat = 34
    let window: Window
    var note: String? = nil

    private var caption: String {
        let reset = resetText(window.reset).map { "resets \($0)" } ?? "no reset pending"
        return note.map { "\(reset) · \($0)" } ?? reset
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(label)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .frame(width: labelWidth, alignment: .trailing)

                GeometryReader { geo in
                    ZStack(alignment: .leading) {
                        RoundedRectangle(cornerRadius: 3)
                            .fill(.quaternary)
                        RoundedRectangle(cornerRadius: 3)
                            .fill(levelColor(window.remaining))
                            .frame(width: geo.size.width * window.remaining)
                    }
                }
                .frame(height: 12)

                Text("\(Int((window.remaining * 100).rounded()))%")
                    .font(.system(size: 11, design: .monospaced))
                    .frame(width: 36, alignment: .trailing)
            }

            Text(caption)
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .padding(.leading, labelWidth + 6)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

/// A dim line under a card's gauges.
struct Caption: View {
    let text: String
    var color: Color = .secondary
    var lines: Int? = nil

    init(_ text: String, color: Color = .secondary, lines: Int? = nil) {
        self.text = text
        self.color = color
        self.lines = lines
    }

    var body: some View {
        Text(text)
            .font(.system(size: 10))
            .foregroundStyle(color)
            .lineLimit(lines)
            .fixedSize(horizontal: false, vertical: true)
    }
}

struct Card<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) { content }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.ultraThinMaterial)
            .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}

struct SectionTitle: View {
    let title: String

    var body: some View {
        Text(title)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
    }
}

struct MiniUtilizationChart: View {
    let snapshots: [HistorySnapshot]

    private var chartColor: Color {
        guard let rem = snapshots.last?.remaining(for: "7d") else { return .green }
        return levelColor(rem / 100)
    }

    var body: some View {
        Chart {
            ForEach(snapshots) { snap in
                if let rem = snap.remaining(for: "7d") {
                    AreaMark(x: .value("Time", snap.date), y: .value("Remaining", rem))
                        .foregroundStyle(chartColor.opacity(0.15))
                    LineMark(x: .value("Time", snap.date), y: .value("Remaining", rem))
                        .foregroundStyle(chartColor)
                        .lineStyle(StrokeStyle(lineWidth: 1.5))
                }
            }
        }
        .chartYScale(domain: 0...100)
        .chartXAxis(.hidden)
        .chartYAxis(.hidden)
        .frame(height: 30)
    }
}

struct AccountDetailChart: View {
    let snapshots: [HistorySnapshot]
    @State private var series = "7d"

    /// The weekly and 5h windows, each per-model limit seen, and extra usage.
    private var seriesNames: [String] {
        var names = ["7d", "5h"]
        for snapshot in snapshots {
            for bucket in snapshot.buckets where !names.contains(bucket.label) {
                names.append(bucket.label)
            }
        }
        return names + ["$$"]
    }

    var body: some View {
        VStack(spacing: 6) {
            Picker("Window", selection: $series) {
                ForEach(seriesNames, id: \.self) { name in
                    Text(name).tag(name)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            Chart {
                RuleMark(y: .value("Warning", 20))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [4, 3]))
                    .foregroundStyle(.orange.opacity(0.5))
                RuleMark(y: .value("Critical", 10))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [4, 3]))
                    .foregroundStyle(.red.opacity(0.5))

                ForEach(snapshots) { snap in
                    if let rem = snap.remaining(for: series) {
                        LineMark(x: .value("Time", snap.date), y: .value("Remaining %", rem))
                            .foregroundStyle(.blue)
                            .lineStyle(StrokeStyle(lineWidth: 2))
                            .interpolationMethod(.monotone)
                    }
                }
            }
            .chartYScale(domain: 0...100)
            .chartYAxis {
                AxisMarks(values: [0, 25, 50, 75, 100]) { val in
                    AxisGridLine()
                    AxisValueLabel {
                        if let v = val.as(Int.self) { Text("\(v)%").font(.system(size: 9)) }
                    }
                }
            }
            .chartXAxis {
                AxisMarks { _ in
                    AxisGridLine()
                    AxisValueLabel(format: .dateTime.hour().minute())
                }
            }
            .frame(height: 150)
        }
    }
}

struct ComparisonChart: View {
    let history: [String: [HistorySnapshot]]

    var body: some View {
        Chart {
            ForEach(Array(history.keys.sorted()), id: \.self) { name in
                if let snaps = history[name] {
                    ForEach(snaps) { snap in
                        if let rem = snap.remaining(for: "7d") {
                            LineMark(
                                x: .value("Time", snap.date),
                                y: .value("Remaining %", rem)
                            )
                            .foregroundStyle(by: .value("Account", name))
                            .lineStyle(StrokeStyle(lineWidth: 1.5))
                            .interpolationMethod(.monotone)
                        }
                    }
                }
            }
        }
        .chartYScale(domain: 0...100)
        .chartYAxis {
            AxisMarks(values: [0, 50, 100]) { val in
                AxisGridLine()
                AxisValueLabel {
                    if let v = val.as(Int.self) { Text("\(v)%").font(.system(size: 9)) }
                }
            }
        }
        .chartXAxis {
            AxisMarks { _ in
                AxisGridLine()
                AxisValueLabel(format: .dateTime.hour())
            }
        }
        .chartLegend(.hidden)
        .frame(height: 80)
    }
}

struct ClaudeCard: View {
    let account: ClaudeAccount
    let history: [HistorySnapshot]?
    let onLaunch: () -> Void
    @State private var showDetail = false

    private func statusColor(_ status: String) -> Color {
        switch status {
        case "allowed": return .green
        case "allowed_warning": return .orange
        case "rejected": return .red
        default: return .secondary
        }
    }

    /// How many stored readings have a weekly window to chart.
    private var weeklyPoints: Int {
        (history ?? []).filter { $0.weekly != nil }.count
    }

    /// `50% of the weekly, 25% of the weekly left for it`, for a limit that
    /// is a share of the weekly one.
    private func shareNote(_ bucket: ClaudeAccount.Bucket) -> String? {
        guard let share = bucket.share else { return nil }
        let size = Int((share * 100).rounded())
        let room = Int((max(0, bucket.room) * 100).rounded())
        return "\(size)% of the weekly, \(room)% of the weekly left for it"
    }

    var body: some View {
        Card {
            HStack {
                Text(account.name).font(.headline).lineLimit(1)
                if account.is_default {
                    Text("default")
                        .font(.system(size: 9, weight: .semibold))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Color.accentColor.opacity(0.25))
                        .clipShape(Capsule())
                }
                if let status = account.reading?.status {
                    Text(status).font(.caption).foregroundStyle(statusColor(status))
                }
                Spacer()
                Button("Launch") { onLaunch() }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            }

            if let reading = account.reading {
                if let session = reading.session {
                    GaugeRow(label: "5h", window: session)
                } else if reading.session_limit == "absent" {
                    Caption("5h  no 5h limit on this account")
                } else if reading.weekly != nil {
                    Caption("5h  not reported", color: .secondary.opacity(0.6))
                }
                if let weekly = reading.weekly { GaugeRow(label: "7d", window: weekly) }
                ForEach(reading.buckets) { bucket in
                    GaugeRow(
                        label: bucket.source == "observed-rejection" ? "\(bucket.short)!" : bucket.short,
                        window: bucket.window,
                        note: shareNote(bucket)
                    )
                }
                if let overage = reading.overage { GaugeRow(label: "$$", window: overage) }
                if let error = reading.error {
                    Caption(error, color: .red, lines: 3)
                }
                ForEach(reading.notes, id: \.self) { note in Caption(note) }

                if let snaps = history, weeklyPoints >= 2 {
                    MiniUtilizationChart(snapshots: snaps).padding(.top, 2)
                }
            } else {
                Caption("not read yet")
            }

            if let note = account.login_note {
                Caption(note, color: account.login_warning ? .orange : .secondary)
            }
            if let reading = account.reading {
                Caption("read \(ageText(reading.at))", color: .secondary.opacity(0.6))
            }

            if showDetail, let snaps = history, snaps.count >= 2 {
                AccountDetailChart(snapshots: snaps).padding(.top, 4)
            }
        }
        .contentShape(Rectangle())
        .onTapGesture {
            if (history?.count ?? 0) >= 2 {
                withAnimation(.easeInOut(duration: 0.2)) { showDetail.toggle() }
            }
        }
    }
}

struct CodexCard: View {
    let account: CodexAccount

    private func creditsText(_ credits: CodexAccount.ResetCredits) -> String {
        let noun = credits.available == 1 ? "reset credit" : "reset credits"
        if let now = credits.applicable {
            return "\(credits.available) \(noun) (\(now) usable now)"
        }
        return "\(credits.available) \(noun)"
    }

    var body: some View {
        Card {
            HStack {
                Text(account.name).font(.headline).lineLimit(1)
                if let plan = account.reading?.plan { Caption(plan) }
                if account.parked { Caption("parked") }
                Spacer()
                if let reading = account.reading {
                    Text(reading.state)
                        .font(.caption)
                        .foregroundStyle(reading.viable ? .green : .orange)
                        .lineLimit(2)
                }
            }
            if let reading = account.reading {
                ForEach(reading.windows) { named in
                    GaugeRow(label: named.label, labelWidth: 50, window: named.window)
                }
                ForEach(reading.extra) { extra in
                    ForEach(extra.windows) { named in
                        GaugeRow(
                            label: named.label,
                            labelWidth: 50,
                            window: named.window,
                            note: extra.limited ? "\(extra.name), limited" : extra.name
                        )
                    }
                }
                if let credits = reading.reset_credits, credits.available > 0 {
                    Caption(creditsText(credits), color: .green)
                }
                Caption("read \(ageText(reading.at))", color: .secondary.opacity(0.6))
            } else {
                Caption("not read yet (`tokeman codex list`)")
            }
        }
    }
}

struct ServiceCard: View {
    let account: ServiceAccount

    /// The meters in the service's order, split where the group changes.
    private var groups: [(name: String?, meters: [Limit])] {
        var result: [(name: String?, meters: [Limit])] = []
        for meter in account.usage?.limits ?? [] {
            if let last = result.indices.last, result[last].name == meter.group {
                result[last].meters.append(meter)
            } else {
                result.append((meter.group, [meter]))
            }
        }
        return result
    }

    private func amountNote(_ meter: Limit) -> String? {
        guard let amount = meter.amount else { return nil }
        return "\(Int(amount.used.rounded()))/\(Int(amount.limit.rounded())) \(amount.unit)"
    }

    private func balanceText(_ balance: ServiceAccount.Balance) -> String {
        let left = moneyText(balance.left_minor, balance.currency)
        if let total = balance.total_minor {
            return "\(balance.label) \(left) of \(moneyText(total, balance.currency)) left"
        }
        return "\(balance.label) \(left) left"
    }

    var body: some View {
        Card {
            HStack {
                Text(account.name).font(.headline).lineLimit(1)
                Caption(account.service_label)
                if let plan = account.usage?.plan { Caption("[\(plan)]") }
                Spacer()
            }
            if let usage = account.usage {
                if let error = usage.error { Caption(error.text, color: .red, lines: 3) }
                ForEach(Array(groups.enumerated()), id: \.offset) { _, group in
                    if let name = group.name {
                        Text(name).font(.system(size: 10, weight: .medium))
                    }
                    ForEach(Array(group.meters.enumerated()), id: \.offset) { _, meter in
                        GaugeRow(
                            label: meter.label,
                            labelWidth: 50,
                            window: meter.window,
                            note: amountNote(meter)
                        )
                    }
                }
                ForEach(Array((usage.balances ?? []).enumerated()), id: \.offset) { _, balance in
                    Caption(balanceText(balance), color: .primary)
                }
                Caption("read \(ageText(usage.at))", color: .secondary.opacity(0.6))
            } else {
                Caption("not read yet (`tokeman services`)")
            }
        }
    }
}

struct SettingsSection: View {
    @ObservedObject var vm: TokemanViewModel
    @State private var launchArgs: String = ""
    @State private var terminal: String = ""
    @State private var claudeBin: String = ""
    @State private var probeInterval: String = "30"

    var body: some View {
        DisclosureGroup(isExpanded: $vm.showSettings) {
            VStack(alignment: .leading, spacing: 10) {
                LabeledContent("Launch args") {
                    TextField("e.g. --model opus", text: $launchArgs)
                        .textFieldStyle(.roundedBorder)
                        .frame(maxWidth: 200)
                }

                LabeledContent("Terminal") {
                    Picker("", selection: $terminal) {
                        Text("Terminal.app").tag("")
                        Text("iTerm2").tag("iTerm2")
                    }
                    .pickerStyle(.menu)
                    .frame(maxWidth: 200)
                }

                LabeledContent("Claude binary") {
                    TextField("claude", text: $claudeBin)
                        .textFieldStyle(.roundedBorder)
                        .frame(maxWidth: 200)
                }

                LabeledContent("Probe interval") {
                    HStack {
                        TextField("30", text: $probeInterval)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 60)
                        Text("sec").foregroundStyle(.secondary)
                    }
                }

                HStack {
                    Spacer()
                    Button("Save") {
                        vm.saveSettings(
                            launchArgs: launchArgs,
                            terminal: terminal,
                            claudeBin: claudeBin,
                            probeInterval: probeInterval
                        )
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                }
            }
            .padding(.top, 6)
        } label: {
            Label("Settings", systemImage: "gear")
                .font(.caption)
        }
        .onAppear { loadFromSettings() }
        .onChange(of: vm.showSettings) { _ in loadFromSettings() }
    }

    private func loadFromSettings() {
        launchArgs = vm.settings.launchArgs.joined(separator: " ")
        terminal = vm.settings.terminal ?? ""
        claudeBin = vm.settings.claudeBin ?? ""
        probeInterval = "\(vm.settings.probeIntervalSecs)"
    }
}

/// Everything the stored view holds, as cards.
struct OverviewCards: View {
    let overview: Overview
    let history: [String: [HistorySnapshot]]
    let onLaunch: (String) -> Void

    var body: some View {
        VStack(spacing: 6) {
            ForEach(overview.claude) { account in
                ClaudeCard(account: account, history: history[account.name]) {
                    onLaunch(account.name)
                }
            }

            if !overview.resets_on_offer.isEmpty {
                SectionTitle(title: "Resets on offer")
                Card {
                    ForEach(overview.resets_on_offer) { offer in
                        Caption("\(offer.account): \(offer.offer)", color: .green)
                        Caption("read \(ageText(offer.read_at)); use with `tokeman resets use`",
                                color: .secondary.opacity(0.6))
                    }
                }
            }

            if !overview.codex.isEmpty {
                SectionTitle(title: "Codex")
                ForEach(overview.codex) { account in CodexCard(account: account) }
            }

            if !overview.services.isEmpty {
                SectionTitle(title: "Services")
                ForEach(overview.services) { account in ServiceCard(account: account) }
            }
        }
        .padding(10)
    }
}

struct PopoverContent: View {
    @ObservedObject var vm: TokemanViewModel
    /// Room for the cards before they scroll; `nil` lays every card out
    /// at full height, for a picture.
    var maxCardsHeight: CGFloat? = 560

    private var monitorText: String {
        switch vm.overview?.monitor {
        case "running": return "stored reads · service running"
        case "paused": return "stored reads · rotation paused"
        case "not-installed": return "no service · probing here"
        default: return ""
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Tokeman").font(.headline)
                Text(monitorText).font(.caption2).foregroundStyle(.secondary)
                Spacer()
                if vm.isProbing {
                    Text("probing…").font(.caption2).foregroundStyle(.secondary)
                } else if let last = vm.lastLoad {
                    Text("\(Int(-last.timeIntervalSinceNow))s ago")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                // With a service the button re-reads the stores; without one it probes.
                Button(action: { vm.serviceInstalled ? vm.reload() : vm.probe() }) {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .disabled(vm.isLoading || vm.isProbing)
                .help(vm.serviceInstalled ? "Read the stored view again" : "Probe every account now")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            Divider()

            if let overview = vm.overview {
                if vm.history.values.contains(where: { $0.count >= 2 }) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("24h overview (7d remaining)")
                            .font(.system(size: 10))
                            .foregroundStyle(.secondary)
                        ComparisonChart(history: vm.history)
                    }
                    .padding(.horizontal, 14)
                    .padding(.top, 10)
                }

                if let maxCardsHeight {
                    ScrollView {
                        OverviewCards(overview: overview, history: vm.history) { name in
                            vm.launchAccount(name)
                        }
                    }
                    .frame(maxHeight: maxCardsHeight)
                } else {
                    OverviewCards(overview: overview, history: vm.history) { name in
                        vm.launchAccount(name)
                    }
                }

                Divider()

                VStack(spacing: 8) {
                    HStack {
                        Button(action: { vm.launchBest() }) {
                            Label("Launch Best", systemImage: "paperplane.fill")
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .disabled(overview.claude.isEmpty)
                        .help("Runs `tokeman launch`: Claude on the account the policy picks")

                        Text("default: \(overview.default_account ?? "/login")"
                             + (overview.target_model.map { " · \($0)" } ?? ""))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)

                        Spacer()
                    }

                    HStack {
                        Toggle(isOn: Binding(
                            get: { vm.settings.dangerousMode },
                            set: { _ in vm.toggleDangerMode() }
                        )) {
                            HStack(spacing: 4) {
                                Image(systemName: "exclamationmark.shield.fill")
                                Text("--dangerously-skip-permissions")
                                    .font(.system(size: 11, design: .monospaced))
                            }
                            .foregroundStyle(vm.settings.dangerousMode ? .red : .secondary)
                        }
                        .tint(.red)
                        .toggleStyle(.switch)
                        .controlSize(.small)
                    }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
            } else {
                VStack(spacing: 8) {
                    if let err = vm.loadError {
                        Image(systemName: "exclamationmark.triangle")
                            .font(.title2)
                            .foregroundStyle(.orange)
                        Text(err)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 20)
                        Button("Retry") { vm.reload() }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                    } else {
                        ProgressView()
                        Text("Reading the stored view...")
                            .foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, minHeight: 120)
            }

            Divider()

            SettingsSection(vm: vm)
                .padding(.horizontal, 14)
                .padding(.vertical, 8)

            Divider()

            Button("Quit Tokeman") { NSApplication.shared.terminate(nil) }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .font(.caption)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
        }
        .frame(width: 400)
        .onAppear { vm.start() }
    }
}

// MARK: - App

/// `TokemanTray --render <file.png>` draws the popover once from the stored
/// view, at full height, and exits: a way to look at the layout without a
/// menu bar. It reads what the app reads and probes nothing.
@MainActor
func renderPopover(to path: String) async -> Bool {
    let vm = TokemanViewModel(polls: false)
    await vm.load()
    let view = PopoverContent(vm: vm, maxCardsHeight: nil)
        .background(Color(nsColor: .windowBackgroundColor))
        .environment(\.colorScheme, .dark)
    let renderer = ImageRenderer(content: view)
    renderer.scale = 2
    guard let image = renderer.nsImage,
          let tiff = image.tiffRepresentation,
          let bitmap = NSBitmapImageRep(data: tiff),
          let png = bitmap.representation(using: .png, properties: [:]) else {
        return false
    }
    return (try? png.write(to: URL(fileURLWithPath: path))) != nil
}

@main
enum Main {
    static func main() {
        let arguments = CommandLine.arguments
        guard let flag = arguments.firstIndex(of: "--render"), flag + 1 < arguments.count else {
            TokemanTrayApp.main()
            return
        }
        let path = arguments[flag + 1]
        let app = NSApplication.shared
        app.setActivationPolicy(.prohibited)
        Task { @MainActor in
            let drawn = await renderPopover(to: path)
            if !drawn { fputs("tokeman tray: could not draw \(path)\n", stderr) }
            exit(drawn ? 0 : 1)
        }
        app.run()
    }
}

struct TokemanTrayApp: App {
    @StateObject private var vm = TokemanViewModel()

    var body: some Scene {
        MenuBarExtra {
            PopoverContent(vm: vm)
        } label: {
            Image(systemName: vm.statusIcon)
                .foregroundStyle(vm.statusColor)
        }
        .menuBarExtraStyle(.window)
    }
}
