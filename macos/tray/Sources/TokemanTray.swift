import SwiftUI
import Foundation
import Charts

// This app is a viewer of tokeman's fleet view. It reads `tokeman list --json`
// (everything tokeman has stored, read without a request or a write),
// `tokeman history --json` and `tokeman settings show`, and never opens
// tokens.toml itself. The background service keeps the stores fresh; only
// when it is not running, or when the reload button asks, does the app have
// tokeman read the accounts (`tokeman --fresh --json`, which prints the same
// view). It changes things only through `tokeman settings set` and, for a
// Launch button, `tokeman claude use` (one account) or `tokeman claude launch`
// (the best one).

// MARK: - JSON models (match `tokeman list --json`)

struct Window: Decodable, Equatable {
    let utilization: Double
    let reset: Int64

    /// Fraction left, for a gauge.
    var remaining: Double { max(0, min(1, 1.0 - utilization)) }
}

/// One limit of one account, as tokeman's `quota::Limit` serializes: a
/// window of some length (`span`) over the whole account, one model or one
/// named group (`scope`). Every gauge in the app is drawn from one of these.
struct Limit: Decodable {
    /// `5h`, `weekly`, `monthly`, or another length in words; `nil` when the
    /// server did not say.
    let span: String?
    /// The group the limit is scoped to (Antigravity's model families).
    let group: String?
    /// The model the limit is scoped to.
    let model: String?
    /// The server's name for the limit; empty when it has none.
    let label: String
    let window: Window
    let amount: Amount?
    /// The limit's size as a fraction of the weekly limit, when it is one.
    let share: Double?
    let source: String
    /// The server's own status for this window is a rejection.
    let rejected: Bool

    /// Whether the limit covers the whole account.
    var isShared: Bool { group == nil && model == nil }

    struct Amount: Decodable {
        let used: Double
        let limit: Double
        let unit: String
    }

    private struct Status: Decodable { let verdict: String }
    private enum Keys: String, CodingKey { case span, scope, label, window, amount, share, source, status }
    private enum ScopeKeys: String, CodingKey { case model, group }
    private enum SpanKeys: String, CodingKey { case other }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        if let named = try? c.decode(String.self, forKey: .span) {
            switch named {
            case "five_hour": span = "5h"
            case "unknown": span = nil
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
        label = try c.decodeIfPresent(String.self, forKey: .label) ?? ""
        window = try c.decode(Window.self, forKey: .window)
        amount = try c.decodeIfPresent(Amount.self, forKey: .amount)
        share = try c.decodeIfPresent(Double.self, forKey: .share)
        source = try c.decodeIfPresent(String.self, forKey: .source) ?? ""
        rejected = (try c.decodeIfPresent(Status.self, forKey: .status))?.verdict == "rejected"
    }

    /// `daily`, `2d`, `90m`: a length in its largest whole unit.
    private static func spanText(_ seconds: Int64) -> String {
        if seconds == 86_400 { return "daily" }
        for (size, suffix) in [(Int64(86_400), "d"), (3_600, "h"), (60, "m")] where seconds % size == 0 {
            return "\(seconds / size)\(suffix)"
        }
        return "\(seconds)s"
    }

    /// What the limit covers, then its length: `5h`, `weekly`, `Fable
    /// weekly`, `Gemini Models 5h`, `codex_spark weekly`. The same rule as
    /// tokeman's `ui::text::limit_name`.
    var name: String {
        let length = span ?? "window"
        if isShared { return label.isEmpty ? length : label }
        let isLength = label == length || ["5h", "weekly", "monthly", "daily"].contains(label)
        let head: String
        if let group {
            if label.isEmpty {
                head = group
            } else if isLength {
                return "\(group) \(label)"
            } else {
                head = label
            }
        } else {
            head = label.isEmpty ? (model ?? "") : label
        }
        return span == nil || head.hasSuffix(length) ? head : "\(head) \(length)"
    }

    /// The order every surface shows limits in: account-wide, models, groups.
    var rank: Int { isShared ? 0 : (model != nil ? 1 : 2) }

    private var sourceLabel: String? {
        guard model != nil else { return nil }
        switch source {
        case "profile": return "profile"
        case "headers": return "rate-limit headers"
        case "observed-rejection": return "observed Claude rejection"
        case "carried": return "carried sample"
        default: return nil
        }
    }

    /// What follows the gauge: that the server refuses on the limit, the
    /// absolute amount, where a model's reading came from, its share of the
    /// weekly, why extra usage is off. The same rule as tokeman's
    /// `ui::text::limit_note`.
    func note(in reading: Reading) -> String? {
        var parts: [String] = []
        if rejected { parts.append("limited") }
        if let amount {
            parts.append("\(Int(amount.used.rounded()))/\(Int(amount.limit.rounded())) \(amount.unit)")
        }
        if let sourceLabel { parts.append(sourceLabel) }
        if let share {
            let own = (1.0 - window.utilization) * share
            let room = reading.weekly.map { min(own, 1.0 - $0.utilization) } ?? own
            parts.append("\(Int((share * 100).rounded()))% of the weekly, "
                + "\(Int((max(0, room) * 100).rounded()))% of the weekly left for it")
        }
        if group == "extra usage", let reason = reading.detail?.claude?.overage?.disabled_reason {
            parts.append(reason == "out_of_credits" ? "no credits" : reason)
        }
        return parts.isEmpty ? nil : parts.joined(separator: "; ")
    }
}

/// One read of one account, as tokeman's `quota::Reading` serializes. The
/// fleet view and the history hold the same thing.
struct Reading: Decodable, Identifiable {
    var id: String { "\(account)-\(at)" }
    let account: String
    /// Epoch seconds.
    let at: Int64
    let plan: String?
    let limits: [Limit]
    let detail: Detail?

    struct Detail: Decodable { let claude: Claude? }
    struct Claude: Decodable { let overage: Overage? }
    struct Overage: Decodable { let disabled_reason: String? }

    var date: Date { Date(timeIntervalSince1970: TimeInterval(at)) }

    /// The account-wide weekly window.
    var weekly: Window? {
        limits.first(where: { $0.isShared && $0.span == "weekly" })?.window
    }

    /// The limits in the order they are shown.
    var ordered: [Limit] {
        limits.enumerated()
            .sorted { ($0.element.rank, $0.offset) < ($1.element.rank, $1.offset) }
            .map(\.element)
    }

    /// Percent left on the limit of this name.
    func remaining(for name: String) -> Double? {
        limits.first(where: { $0.name == name }).map { $0.window.remaining * 100 }
    }
}

/// One account of one service: tokeman's `ui::overview::AccountView`.
struct Account: Decodable, Identifiable {
    var id: String { "\(service)-\(name)" }
    let service: String
    let service_label: String
    let name: String
    let label: String?
    /// The tool holds this account now: Claude Code's default, agy's login.
    let held: Bool
    let parked: Bool
    let login: Login?
    let state: State?
    let error: String?
    let notes: [String]
    let reading: Reading?

    struct Login: Decodable {
        let note: String?
        let warning: Bool
    }

    struct State: Decodable {
        let word: String
        /// `ok`, `low` or `critical`.
        let level: String?
    }

    var isClaude: Bool { service == "claude" }
}

struct ResetOffer: Decodable, Identifiable {
    var id: String { "\(account)-\(offer)" }
    let account: String
    let offer: String
    let read_at: Int64
}

/// The fleet view: tokeman's `ui::overview::Overview`.
struct Overview: Decodable {
    /// Epoch seconds when the view was put together.
    let at: Int64
    /// `stored` (the last reads, each as old as its `at`) or `fresh`.
    let origin: String
    /// Epoch seconds of the newest reading in the view.
    let read_at: Int64?
    /// `running`, `paused`, `stopped` or `not-installed`.
    let monitor: String
    let default_account: String?
    let target_model: String?
    let best_weekly_remaining: Double?
    let accounts: [Account]
    let resets_on_offer: [ResetOffer]

    /// The services that have an account, in the view's order.
    var services: [(key: String, label: String)] {
        var seen: [(key: String, label: String)] = []
        for account in accounts where !seen.contains(where: { $0.key == account.service }) {
            seen.append((account.service, account.service_label))
        }
        return seen
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

/// The colour of a state's level, as tokeman names it.
func levelColor(_ level: String?) -> Color {
    switch level {
    case "ok": return .green
    case "low": return .orange
    case "critical": return .red
    default: return .secondary
    }
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

// MARK: - View model

@MainActor
class TokemanViewModel: ObservableObject {
    @Published var overview: Overview?
    @Published var isLoading = false
    @Published var isReading = false
    @Published var lastLoad: Date?
    @Published var loadError: String?
    @Published var settings = LaunchSettings()
    @Published var showSettings = false
    /// Stored readings of the last 24 hours, by service and account.
    @Published var history: [String: [Reading]] = [:]

    private var timer: Timer?
    private var lastRead: Date?
    private var lastHistoryLoad: Date?
    /// A day of history is megabytes of JSON; the charts move slowly.
    private let historyInterval: TimeInterval = 300
    /// The stored view is local and cheap to read.
    private let reloadInterval: TimeInterval = 15
    /// A stored view older than this, with the service running, is stale.
    private let staleAfter: Int64 = 600

    /// `polls: false` is for drawing one picture of the stored view.
    let polls: Bool

    init(polls: Bool = true) {
        self.polls = polls
        // The menu-bar icon shows the fleet's state before the first click.
        if polls { Task { @MainActor in self.start() } }
    }

    /// Whether the background service reads the accounts by itself.
    var serviceReads: Bool { overview?.monitor == "running" }

    /// The stored view is older than the service would leave it.
    var isStale: Bool {
        guard let overview, overview.origin == "stored", serviceReads else { return false }
        guard let read = overview.read_at else { return true }
        return overview.at - read > staleAfter
    }

    /// Where the view's readings come from and how old they are.
    var caption: String {
        guard let overview else { return "" }
        if overview.origin == "fresh" { return "read \(ageText(overview.at))" }
        guard let read = overview.read_at else { return "stored view · nothing read yet" }
        let age = ageText(read)
        switch overview.monitor {
        case "running":
            return isStale ? "stored view · STALE, service last read \(age)"
                           : "stored view · read \(age) by the service"
        case "paused": return "stored view · read \(age) · rotation paused"
        case "stopped": return "stored view · read \(age) · service not running"
        default: return "stored view · read \(age) · no service"
        }
    }

    var statusIcon: String {
        guard let overview else { return loadError == nil ? "bolt.fill" : "bolt.slash.fill" }
        guard let best = overview.best_weekly_remaining else { return "bolt.slash.fill" }
        return best > 0.2 ? "bolt.fill" : "bolt.trianglebadge.exclamationmark.fill"
    }

    var statusColor: Color {
        guard let best = overview?.best_weekly_remaining else { return .gray }
        return levelColor(best)
    }

    func history(for account: Account) -> [Reading]? {
        history["\(account.service)-\(account.name)"]
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
        guard !isLoading, !isReading else { return }
        isLoading = true
        Task {
            await load()
            isLoading = false
            readIfNobodyElseDoes()
        }
    }

    private func decode(_ output: String, from command: String) {
        guard let data = output.data(using: .utf8), !output.isEmpty else {
            loadError = "Could not run tokeman (looked at \(findTokeman())). Install it with: cargo install --path ."
            return
        }
        do {
            overview = try JSONDecoder().decode(Overview.self, from: data)
            lastLoad = Date()
            loadError = nil
        } catch {
            loadError = "This tokeman does not print the fleet view (`tokeman \(command)`): \(error)"
        }
    }

    func load() async {
        let bin = findTokeman()
        decode(await shell(bin, args: ["list", "--json"]), from: "list --json")
        settings = LaunchSettings.parse(await shell(bin, args: ["settings", "show"]))
        await loadHistory(bin)
    }

    private func loadHistory(_ bin: String) async {
        if let last = lastHistoryLoad, Date().timeIntervalSince(last) < historyInterval { return }
        guard let overview else { return }
        lastHistoryLoad = Date()
        var grouped: [String: [Reading]] = [:]
        for service in overview.services {
            let output = await shell(bin, args: ["history", "--service", service.key, "--json", "--since", "24"])
            guard let data = output.data(using: .utf8),
                  let readings = try? JSONDecoder().decode([Reading].self, from: data) else {
                continue
            }
            for reading in readings {
                grouped["\(service.key)-\(reading.account)", default: []].append(reading)
            }
        }
        for key in grouped.keys {
            grouped[key]?.sort { $0.at < $1.at }
        }
        history = grouped
    }

    /// With no background service reading, nothing refreshes the stores, so
    /// the app has tokeman read on the configured interval.
    private func readIfNobodyElseDoes() {
        guard polls, overview != nil, !serviceReads, !isReading else { return }
        let interval = TimeInterval(max(15, settings.probeIntervalSecs))
        if let last = lastRead, Date().timeIntervalSince(last) < interval { return }
        readNow()
    }

    /// Have tokeman read every account now (`tokeman --fresh --json`) and
    /// show the view it prints.
    func readNow() {
        guard !isReading else { return }
        isReading = true
        lastRead = Date()
        Task {
            decode(await shell(findTokeman(), args: ["--fresh", "--json"]), from: "--fresh --json")
            isReading = false
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
        guard overview?.accounts.contains(where: { $0.isClaude && $0.name == name }) == true else { return }
        let cmd = claudeCommand
        Task {
            let result = await shell(findTokeman(), args: ["claude", "use", name])
            guard !result.isEmpty else {
                loadError = "Could not activate \(name) through tokeman"
                return
            }
            launchTerminal(cmd: cmd, terminal: settings.terminal)
        }
    }

    /// Open Claude on whichever account the rotation policy picks.
    func launchBest() {
        launchTerminal(cmd: "\(shellQuote(findTokeman())) claude launch", terminal: settings.terminal)
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

/// One limit of any account of any service: its name, a bar of what is
/// left, the percentage, and under it the reset and what else there is to
/// say. The only gauge in the app.
struct LimitRow: View {
    let limit: Limit
    let reading: Reading
    var nameWidth: CGFloat = 34

    private var caption: String {
        let reset = resetText(limit.window.reset).map { "resets \($0)" } ?? "no reset pending"
        return limit.note(in: reading).map { "\(reset) · \($0)" } ?? reset
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(limit.name)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(width: nameWidth, alignment: .trailing)

                GeometryReader { geo in
                    ZStack(alignment: .leading) {
                        RoundedRectangle(cornerRadius: 3)
                            .fill(.quaternary)
                        RoundedRectangle(cornerRadius: 3)
                            .fill(levelColor(limit.window.remaining))
                            .frame(width: geo.size.width * limit.window.remaining)
                    }
                }
                .frame(height: 12)

                Text("\(Int((limit.window.remaining * 100).rounded()))%")
                    .font(.system(size: 11, design: .monospaced))
                    .frame(width: 36, alignment: .trailing)
            }

            Text(caption)
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .padding(.leading, nameWidth + 6)
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

/// The limit a small chart follows for an account: its weekly window when
/// it has one, else its first limit.
func mainLimit(_ readings: [Reading]) -> String? {
    guard let latest = readings.last else { return nil }
    if latest.weekly != nil { return "weekly" }
    return latest.ordered.first?.name
}

struct MiniUtilizationChart: View {
    let snapshots: [Reading]
    let series: String

    private var chartColor: Color {
        guard let rem = snapshots.last?.remaining(for: series) else { return .green }
        return levelColor(rem / 100)
    }

    var body: some View {
        Chart {
            ForEach(snapshots) { snap in
                if let rem = snap.remaining(for: series) {
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
    let snapshots: [Reading]
    @State private var series: String

    init(snapshots: [Reading]) {
        self.snapshots = snapshots
        _series = State(initialValue: mainLimit(snapshots) ?? "weekly")
    }

    /// Every limit the history holds, by name.
    private var seriesNames: [String] {
        var names: [String] = []
        for snapshot in snapshots.reversed() {
            for limit in snapshot.ordered where !names.contains(limit.name) {
                names.append(limit.name)
            }
        }
        return names
    }

    var body: some View {
        VStack(spacing: 6) {
            Picker("Limit", selection: $series) {
                ForEach(seriesNames, id: \.self) { name in
                    Text(name).tag(name)
                }
            }
            .pickerStyle(.menu)
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
                            .interpolationMethod(.stepEnd)
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

/// The weekly window of every Claude account over the last day.
struct ComparisonChart: View {
    let history: [(name: String, readings: [Reading])]

    var body: some View {
        Chart {
            ForEach(history, id: \.name) { entry in
                ForEach(entry.readings) { snap in
                    if let rem = snap.remaining(for: "weekly") {
                        LineMark(
                            x: .value("Time", snap.date),
                            y: .value("Remaining %", rem)
                        )
                        .foregroundStyle(by: .value("Account", entry.name))
                        .lineStyle(StrokeStyle(lineWidth: 1.5))
                        .interpolationMethod(.stepEnd)
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

/// One account of any service: who it is and its state, a gauge per limit,
/// then what else its reading says. A Claude account can be launched.
struct AccountCard: View {
    let account: Account
    let history: [Reading]?
    /// Whether the view's readings are stored ones, each with its own age.
    let stored: Bool
    let onLaunch: () -> Void
    @State private var showDetail = false

    private var heldWord: String {
        switch account.service {
        case "claude": return "default"
        case "antigravity": return "agy's login"
        default: return "in use"
        }
    }

    /// The series the small chart follows, when the history can draw it.
    private var sparkSeries: String? {
        guard let history, let series = mainLimit(history),
              history.filter({ $0.remaining(for: series) != nil }).count >= 2 else { return nil }
        return series
    }

    /// Wide enough for the longest limit name on the card.
    private func nameWidth(_ limits: [Limit]) -> CGFloat {
        let longest = limits.map { $0.name.count }.max() ?? 3
        return min(190, CGFloat(max(3, longest)) * 6.8)
    }

    var body: some View {
        Card {
            HStack {
                Text(account.name).font(.headline).lineLimit(1)
                if let plan = account.reading?.plan { Caption("[\(plan)]") }
                if account.parked { Caption("parked") }
                if account.held {
                    Text(heldWord)
                        .font(.system(size: 9, weight: .semibold))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Color.accentColor.opacity(0.25))
                        .clipShape(Capsule())
                }
                if let state = account.state {
                    Text(state.word)
                        .font(.caption)
                        .foregroundStyle(levelColor(state.level))
                        .lineLimit(1)
                }
                Spacer()
                if account.isClaude {
                    Button("Launch") { onLaunch() }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                }
            }

            if let reading = account.reading {
                if let error = account.error { Caption(error, color: .red, lines: 3) }
                let limits = reading.ordered
                ForEach(Array(limits.enumerated()), id: \.offset) { _, limit in
                    LimitRow(limit: limit, reading: reading, nameWidth: nameWidth(limits))
                }
                ForEach(account.notes, id: \.self) { note in
                    Caption(note, color: note.hasPrefix("!") ? .orange : .secondary)
                }
                if let history, let series = sparkSeries {
                    MiniUtilizationChart(snapshots: history, series: series).padding(.top, 2)
                }
            } else {
                Caption("not read yet")
            }

            if let login = account.login, let note = login.note {
                Caption(note, color: login.warning ? .orange : .secondary)
            }
            if stored, let reading = account.reading {
                Caption("read \(ageText(reading.at))", color: .secondary.opacity(0.6))
            }

            if showDetail, let history, history.count >= 2 {
                AccountDetailChart(snapshots: history).padding(.top, 4)
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

/// Every account of the fleet view as a card, under its service's name,
/// then the reset offers.
struct OverviewCards: View {
    let overview: Overview
    @ObservedObject var vm: TokemanViewModel

    var body: some View {
        VStack(spacing: 6) {
            ForEach(overview.services, id: \.key) { service in
                SectionTitle(title: service.label)
                ForEach(overview.accounts.filter { $0.service == service.key }) { account in
                    AccountCard(
                        account: account,
                        history: vm.history(for: account),
                        stored: overview.origin == "stored"
                    ) {
                        vm.launchAccount(account.name)
                    }
                }
            }

            if !overview.resets_on_offer.isEmpty {
                SectionTitle(title: "Resets on offer")
                Card {
                    ForEach(overview.resets_on_offer) { offer in
                        Caption("\(offer.account): \(offer.offer)", color: .green)
                        Caption("read \(ageText(offer.read_at)); use with `tokeman claude reset \(offer.account)`",
                                color: .secondary.opacity(0.6))
                    }
                }
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

    /// The weekly history of every Claude account that has enough to draw.
    private var claudeHistory: [(name: String, readings: [Reading])] {
        (vm.overview?.accounts ?? [])
            .filter { $0.isClaude }
            .compactMap { account in
                guard let readings = vm.history(for: account), readings.count >= 2 else { return nil }
                return (account.name, readings)
            }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Tokeman").font(.headline)
                Text(vm.isReading ? "reading every account…" : vm.caption)
                    .font(.caption2)
                    .foregroundStyle(vm.isStale ? .orange : .secondary)
                    .lineLimit(1)
                Spacer()
                Button(action: { vm.readNow() }) {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .disabled(vm.isLoading || vm.isReading)
                .help("Read every account now (tokeman --fresh)")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            Divider()

            if let overview = vm.overview {
                if !claudeHistory.isEmpty {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Claude, last 24h (weekly remaining)")
                            .font(.system(size: 10))
                            .foregroundStyle(.secondary)
                        ComparisonChart(history: claudeHistory)
                    }
                    .padding(.horizontal, 14)
                    .padding(.top, 10)
                }

                if let maxCardsHeight {
                    ScrollView {
                        OverviewCards(overview: overview, vm: vm)
                    }
                    .frame(maxHeight: maxCardsHeight)
                } else {
                    OverviewCards(overview: overview, vm: vm)
                }

                Divider()

                VStack(spacing: 8) {
                    HStack {
                        Button(action: { vm.launchBest() }) {
                            Label("Launch Best", systemImage: "paperplane.fill")
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .disabled(!overview.accounts.contains(where: { $0.isClaude }))
                        .help("Runs `tokeman claude launch`: Claude on the account the policy picks")

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
                        Text("Reading the fleet view...")
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
/// menu bar. It reads what the app reads and has nothing read.
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
