import SwiftUI
import Foundation
import Charts

// MARK: - JSON models (matches `tokeman --json` output)

struct TokenResult: Codable, Identifiable {
    var id: String { token_name }
    let token_name: String
    let probed_at: String
    let quota: Quota?
    let model_usage: ModelUsage?
    let model_usage_error: String?
    let error: String?

    struct Quota: Codable {
        let status: String
        let representative_claim: String
        let session: Window?
        let weekly: Window?
        let overage: Window?
    }

    struct Window: Codable {
        let utilization: Double
        let reset: Int64
    }

    struct ModelUsage: Codable {
        let opus_weekly: Window?
        let sonnet_weekly: Window?
    }
}

// MARK: - History model (matches `tokeman history --json` output)

struct HistorySnapshot: Codable, Identifiable {
    var id: String { "\(token_name)-\(probed_at)" }
    let token_name: String
    let probed_at: String
    let unified_status: String?
    let utilization_5h: Double?
    let reset_5h: Int64?
    let utilization_7d: Double?
    let reset_7d: Int64?
    let utilization_opus_7d: Double?
    let reset_opus_7d: Int64?
    let utilization_sonnet_7d: Double?
    let reset_sonnet_7d: Int64?
    let representative_claim: String?
    let overage_status: String?
    let utilization_overage: Double?
    let reset_overage: Int64?
    let error: String?
    let model_usage_error: String?

    var probedDate: Date {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let d = formatter.date(from: probed_at) { return d }
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: probed_at) ?? Date()
    }

    func remaining(for window: ChartWindowType) -> Double? {
        switch window {
        case .fiveHour: return utilization_5h.map { max(0, (1.0 - $0) * 100) }
        case .sevenDay: return utilization_7d.map { max(0, (1.0 - $0) * 100) }
        case .opusWeekly: return utilization_opus_7d.map { max(0, (1.0 - $0) * 100) }
        case .sonnetWeekly: return utilization_sonnet_7d.map { max(0, (1.0 - $0) * 100) }
        case .overage: return utilization_overage.map { max(0, (1.0 - $0) * 100) }
        }
    }
}

enum ChartWindowType: String, CaseIterable {
    case fiveHour = "5h"
    case sevenDay = "7d"
    case opusWeekly = "Opus"
    case sonnetWeekly = "Sonnet"
    case overage = "$$"
}

// MARK: - Config reader/writer (~/.config/tokeman/tokens.toml)

struct TokemanConfig {
    struct Token {
        let name: String
        let key: String
        let usageKey: String?
    }
    struct Settings {
        var launchArgs: [String] = []
        var dangerousMode: Bool = false
        var terminal: String? = nil
        var claudeBin: String? = nil
        var probeIntervalSecs: Int = 30
    }
    struct Rotation {
        var normalMin5hRemaining = 0.10
        var normalMin7dRemaining = 0.05
        var sipMin5hRemaining = 0.02
        var sipMin7dRemaining = 0.03
        var normalProbeIntervalSecs = 120
        var sipProbeIntervalSecs = 20
    }

    var tokens: [Token] = []
    var settings: Settings = Settings()
    var rotation: Rotation = Rotation()

    static var configPath: String {
        let configDir = ProcessInfo.processInfo.environment["XDG_CONFIG_HOME"]
            ?? "\(NSHomeDirectory())/.config"
        return "\(configDir)/tokeman/tokens.toml"
    }

    static func load() -> TokemanConfig {
        guard let content = try? String(contentsOfFile: configPath, encoding: .utf8) else {
            return TokemanConfig()
        }

        var config = TokemanConfig()
        var section = ""
        var curName: String?
        var curKey: String?
        var curUsageKey: String?

        for line in content.components(separatedBy: "\n") {
            let t = line.trimmingCharacters(in: .whitespaces)
            if t.isEmpty || t.hasPrefix("#") { continue }

            if t == "[[tokens]]" {
                if let n = curName, let k = curKey {
                    config.tokens.append(Token(name: n, key: k, usageKey: curUsageKey))
                }
                curName = nil; curKey = nil; curUsageKey = nil
                section = "tokens"; continue
            }
            if t == "[settings]" {
                if let n = curName, let k = curKey {
                    config.tokens.append(Token(name: n, key: k, usageKey: curUsageKey))
                }
                curName = nil; curKey = nil; curUsageKey = nil
                section = "settings"; continue
            }
            if t == "[rotation]" {
                if let n = curName, let k = curKey {
                    config.tokens.append(Token(name: n, key: k, usageKey: curUsageKey))
                }
                curName = nil; curKey = nil; curUsageKey = nil
                section = "rotation"; continue
            }

            guard let eq = t.firstIndex(of: "=") else { continue }
            let key = t[..<eq].trimmingCharacters(in: .whitespaces)
            let val = t[t.index(after: eq)...].trimmingCharacters(in: .whitespaces)

            if section == "tokens" {
                if key == "name" { curName = unquote(val) }
                if key == "key" { curKey = unquote(val) }
                if key == "usage_key" { curUsageKey = unquote(val) }
            } else if section == "settings" {
                switch key {
                case "dangerous_mode": config.settings.dangerousMode = val == "true"
                case "terminal": config.settings.terminal = unquote(val)
                case "claude_bin": config.settings.claudeBin = unquote(val)
                case "probe_interval_secs": config.settings.probeIntervalSecs = Int(val) ?? 30
                case "launch_args": config.settings.launchArgs = parseArray(val)
                default: break
                }
            } else if section == "rotation" {
                switch key {
                case "normal_min_5h_remaining": config.rotation.normalMin5hRemaining = Double(val) ?? 0.10
                case "normal_min_7d_remaining": config.rotation.normalMin7dRemaining = Double(val) ?? 0.05
                case "sip_min_5h_remaining": config.rotation.sipMin5hRemaining = Double(val) ?? 0.02
                case "sip_min_7d_remaining": config.rotation.sipMin7dRemaining = Double(val) ?? 0.03
                case "normal_probe_interval_secs": config.rotation.normalProbeIntervalSecs = Int(val) ?? 120
                case "sip_probe_interval_secs": config.rotation.sipProbeIntervalSecs = Int(val) ?? 20
                default: break
                }
            }
        }
        if let n = curName, let k = curKey {
            config.tokens.append(Token(name: n, key: k, usageKey: curUsageKey))
        }
        return config
    }

    func save() {
        var lines: [String] = []
        for token in tokens {
            lines.append("[[tokens]]")
            lines.append("name = \"\(Self.escapeToml(token.name))\"")
            lines.append("key = \"\(Self.escapeToml(token.key))\"")
            if let usageKey = token.usageKey {
                lines.append("usage_key = \"\(Self.escapeToml(usageKey))\"")
            }
            lines.append("")
        }
        lines.append("[settings]")
        if !settings.launchArgs.isEmpty {
            let args = settings.launchArgs
                .map { "\"\(Self.escapeToml($0))\"" }
                .joined(separator: ", ")
            lines.append("launch_args = [\(args)]")
        }
        lines.append("dangerous_mode = \(settings.dangerousMode)")
        if let terminal = settings.terminal {
            lines.append("terminal = \"\(Self.escapeToml(terminal))\"")
        }
        if let claudeBin = settings.claudeBin {
            lines.append("claude_bin = \"\(Self.escapeToml(claudeBin))\"")
        }
        lines.append("probe_interval_secs = \(settings.probeIntervalSecs)")
        lines.append("")
        lines.append("[rotation]")
        lines.append("normal_min_5h_remaining = \(rotation.normalMin5hRemaining)")
        lines.append("normal_min_7d_remaining = \(rotation.normalMin7dRemaining)")
        lines.append("sip_min_5h_remaining = \(rotation.sipMin5hRemaining)")
        lines.append("sip_min_7d_remaining = \(rotation.sipMin7dRemaining)")
        lines.append("normal_probe_interval_secs = \(rotation.normalProbeIntervalSecs)")
        lines.append("sip_probe_interval_secs = \(rotation.sipProbeIntervalSecs)")
        lines.append("")

        let content = lines.joined(separator: "\n")
        let url = URL(fileURLWithPath: Self.configPath)
        let directory = url.deletingLastPathComponent()
        do {
            try FileManager.default.createDirectory(
                at: directory,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            try content.write(to: url, atomically: true, encoding: .utf8)
            try FileManager.default.setAttributes(
                [.posixPermissions: 0o600],
                ofItemAtPath: Self.configPath
            )
        } catch {
            fputs("tokeman tray: failed to save config: \(error)\n", stderr)
        }
    }

    private static func escapeToml(_ s: String) -> String {
        s.replacingOccurrences(of: "\\", with: "\\\\")
         .replacingOccurrences(of: "\"", with: "\\\"")
    }

    private static func unquote(_ s: String) -> String {
        if s.hasPrefix("\"") && s.hasSuffix("\"") && s.count >= 2 {
            if let data = s.data(using: .utf8),
               let value = try? JSONSerialization.jsonObject(with: data) as? String {
                return value
            }
            return String(s.dropFirst().dropLast())
        }
        return s
    }

    private static func parseArray(_ s: String) -> [String] {
        let inner = s.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        var values: [String] = []
        var current = ""
        var quoted = false
        var escaped = false
        for character in inner {
            if escaped {
                current.append(character)
                escaped = false
            } else if character == "\\" && quoted {
                current.append(character)
                escaped = true
            } else if character == "\"" {
                current.append(character)
                quoted.toggle()
            } else if character == "," && !quoted {
                let value = unquote(current.trimmingCharacters(in: .whitespaces))
                if !value.isEmpty { values.append(value) }
                current = ""
            } else {
                current.append(character)
            }
        }
        let value = unquote(current.trimmingCharacters(in: .whitespaces))
        if !value.isEmpty { values.append(value) }
        return values
    }
}

// MARK: - View model

@MainActor
class TokemanViewModel: ObservableObject {
    @Published var tokens: [TokenResult] = []
    @Published var isProbing = false
    @Published var lastProbe: Date?
    @Published var probeError: String?
    @Published var config = TokemanConfig.load()
    @Published var showSettings = false
    @Published var history: [String: [HistorySnapshot]] = [:]

    private var timer: Timer?

    var statusIcon: String {
        if tokens.isEmpty && probeError != nil { return "bolt.slash.fill" }
        guard let best = bestToken else {
            return tokens.isEmpty ? "bolt.fill" : "bolt.slash.fill"
        }
        let rem = 1.0 - (best.quota?.weekly?.utilization ?? 1.0)
        if rem > 0.2 { return "bolt.fill" }
        return "bolt.trianglebadge.exclamationmark.fill"
    }

    var statusColor: Color {
        guard let best = bestToken else { return .gray }
        let rem = 1.0 - (best.quota?.weekly?.utilization ?? 1.0)
        if rem > 0.5 { return .green }
        if rem > 0.2 { return .orange }
        return .red
    }

    var bestToken: TokenResult? {
        let allowed = tokens.filter {
            ($0.quota?.status == "allowed" || $0.quota?.status == "allowed_warning")
                && $0.error == nil
        }
        let normal = allowed.filter {
            ($0.quota?.session.map { 1.0 - $0.utilization } ?? 0) > config.rotation.normalMin5hRemaining
                && ($0.quota?.weekly.map { 1.0 - $0.utilization } ?? 0) > config.rotation.normalMin7dRemaining
        }
        if !normal.isEmpty {
            return normal.min {
                let lhs = ($0.quota?.weekly?.utilization ?? 1.0,
                           $0.quota?.session?.utilization ?? 1.0)
                let rhs = ($1.quota?.weekly?.utilization ?? 1.0,
                           $1.quota?.session?.utilization ?? 1.0)
                return lhs < rhs
            }
        }
        // Sip-and-drain: once normal capacity is gone, sample/rotate against the
        // 2% five-hour and 3% weekly floors and prefer immediate 5h headroom.
        return allowed.filter {
            ($0.quota?.session.map { 1.0 - $0.utilization } ?? 0) > config.rotation.sipMin5hRemaining
                && ($0.quota?.weekly.map { 1.0 - $0.utilization } ?? 0) > config.rotation.sipMin7dRemaining
        }.min {
            let lhs = ($0.quota?.session?.utilization ?? 1.0,
                       $0.quota?.weekly?.utilization ?? 1.0)
            let rhs = ($1.quota?.session?.utilization ?? 1.0,
                       $1.quota?.weekly?.utilization ?? 1.0)
            return lhs < rhs
        }
    }

    func startPolling() {
        probe()
        loadHistory()
        timer?.invalidate()
        let interval = TimeInterval(max(1, config.settings.probeIntervalSecs))
        timer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.probe() }
        }
    }

    func probe() {
        guard !isProbing else { return }
        isProbing = true
        probeError = nil
        Task {
            let bin = findTokenman()
            let output = await shell(bin, args: ["--json"])
            if output.isEmpty {
                probeError = "Could not run tokeman — install with: cargo install --path ."
            } else if let data = output.data(using: .utf8),
               let decoded = try? JSONDecoder().decode([TokenResult].self, from: data) {
                tokens = decoded
                lastProbe = Date()
                probeError = nil
                loadHistory()
            } else {
                probeError = "Failed to parse tokeman output"
            }
            isProbing = false
        }
    }

    func loadHistory() {
        Task {
            let bin = findTokenman()
            let output = await shell(bin, args: ["history", "--json", "--since", "24"])
            guard let data = output.data(using: .utf8),
                  let snapshots = try? JSONDecoder().decode([HistorySnapshot].self, from: data) else {
                return
            }
            var grouped: [String: [HistorySnapshot]] = [:]
            for s in snapshots {
                grouped[s.token_name, default: []].append(s)
            }
            // Sort each group by time
            for key in grouped.keys {
                grouped[key]?.sort { $0.probedDate < $1.probedDate }
            }
            history = grouped
        }
    }

    func reloadConfig() {
        config = TokemanConfig.load()
        timer?.invalidate()
        startPolling()
    }

    func toggleDangerMode() {
        config.settings.dangerousMode.toggle()
        config.save()
    }

    func saveSettings(launchArgs: String, terminal: String, claudeBin: String, probeInterval: String) {
        config.settings.launchArgs = launchArgs
            .components(separatedBy: .whitespaces)
            .filter { !$0.isEmpty }
        config.settings.terminal = terminal.isEmpty ? nil : terminal
        config.settings.claudeBin = claudeBin.isEmpty ? nil : claudeBin
        config.settings.probeIntervalSecs = Int(probeInterval) ?? 30
        config.save()
        reloadConfig()
    }

    func launchToken(_ name: String) {
        guard config.tokens.contains(where: { $0.name == name }) else { return }
        let bin = config.settings.claudeBin ?? "claude"
        var args = config.settings.launchArgs
        if config.settings.dangerousMode && !args.contains("--dangerously-skip-permissions") {
            args.append("--dangerously-skip-permissions")
        }
        let cmd = ([bin] + args).map(shellQuote).joined(separator: " ")
        Task {
            let tokeman = findTokenman()
            let result = await shell(tokeman, args: ["rotate", "use", name])
            guard !result.isEmpty else {
                probeError = "Could not activate \(name) through tokeman"
                return
            }
            launchTerminal(cmd: cmd, terminal: config.settings.terminal)
        }
    }

    func launchBest() {
        guard let best = bestToken else { return }
        launchToken(best.token_name)
    }

    // --- helpers ---

    private func findTokenman() -> String {
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
                    proc.waitUntilExit()
                } catch {
                    cont.resume(returning: "")
                    return
                }
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                cont.resume(returning: String(data: data, encoding: .utf8) ?? "")
            }
        }
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

// MARK: - Views

struct GaugeRow: View {
    let label: String
    let window: TokenResult.Window

    private var remaining: Double { max(0, min(1, 1.0 - window.utilization)) }

    private var barColor: Color {
        if remaining > 0.5 { return .green }
        if remaining > 0.2 { return .orange }
        return .red
    }

    private var resetText: String {
        let diff = Double(window.reset) - Date().timeIntervalSince1970
        if diff <= 0 { return "now" }
        let h = Int(diff) / 3600
        let m = (Int(diff) % 3600) / 60
        if h > 24 {
            let f = DateFormatter()
            f.dateFormat = "EEE h:mma"
            return f.string(from: Date(timeIntervalSince1970: Double(window.reset)))
        }
        return h > 0 ? "\(h)h\(m)m" : "\(m)m"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(label)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .frame(width: 34, alignment: .trailing)

                GeometryReader { geo in
                    ZStack(alignment: .leading) {
                        RoundedRectangle(cornerRadius: 3)
                            .fill(.quaternary)
                        RoundedRectangle(cornerRadius: 3)
                            .fill(barColor)
                            .frame(width: geo.size.width * remaining)
                    }
                }
                .frame(height: 12)

                Text("\(Int(remaining * 100))%")
                    .font(.system(size: 11, design: .monospaced))
                    .frame(width: 36, alignment: .trailing)
            }

            HStack {
                Spacer().frame(width: 40)
                Text("resets \(resetText)")
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
            }
        }
    }
}

struct MiniUtilizationChart: View {
    let snapshots: [HistorySnapshot]
    let window: ChartWindowType = .sevenDay

    private var chartColor: Color {
        guard let last = snapshots.last,
              let rem = last.remaining(for: window) else { return .green }
        if rem > 50 { return .green }
        if rem > 20 { return .orange }
        return .red
    }

    var body: some View {
        Chart {
            ForEach(snapshots) { snap in
                if let rem = snap.remaining(for: window) {
                    AreaMark(
                        x: .value("Time", snap.probedDate),
                        y: .value("Remaining", rem)
                    )
                    .foregroundStyle(chartColor.opacity(0.15))

                    LineMark(
                        x: .value("Time", snap.probedDate),
                        y: .value("Remaining", rem)
                    )
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

struct TokenDetailChart: View {
    let snapshots: [HistorySnapshot]
    @State private var selectedWindow: ChartWindowType = .sevenDay

    var body: some View {
        VStack(spacing: 6) {
            Picker("Window", selection: $selectedWindow) {
                ForEach(ChartWindowType.allCases, id: \.self) { w in
                    Text(w.rawValue).tag(w)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            Chart {
                // Threshold lines
                RuleMark(y: .value("Warning", 20))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [4, 3]))
                    .foregroundStyle(.orange.opacity(0.5))
                RuleMark(y: .value("Critical", 10))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [4, 3]))
                    .foregroundStyle(.red.opacity(0.5))

                ForEach(snapshots) { snap in
                    if let rem = snap.remaining(for: selectedWindow) {
                        LineMark(
                            x: .value("Time", snap.probedDate),
                            y: .value("Remaining %", rem)
                        )
                        .foregroundStyle(.blue)
                        .lineStyle(StrokeStyle(lineWidth: 2))
                        .interpolationMethod(.monotone)

                        PointMark(
                            x: .value("Time", snap.probedDate),
                            y: .value("Remaining %", rem)
                        )
                        .foregroundStyle(.blue)
                        .symbolSize(15)
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
                AxisMarks { val in
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
                        if let rem = snap.remaining(for: .sevenDay) {
                            LineMark(
                                x: .value("Time", snap.probedDate),
                                y: .value("Remaining %", rem)
                            )
                            .foregroundStyle(by: .value("Token", name))
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
        .chartLegend(.visible)
        .frame(height: 100)
    }
}

struct TokenCard: View {
    let token: TokenResult
    let history: [HistorySnapshot]?
    let onLaunch: () -> Void
    @State private var showDetail = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(token.token_name).font(.headline)

                if let status = token.quota?.status {
                    Text(status)
                        .font(.caption)
                        .foregroundStyle(statusColor(status))
                }

                Spacer()

                Button("Launch") { onLaunch() }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            }

            if let q = token.quota {
                if let s = q.session { GaugeRow(label: "5h", window: s) }
                if let w = q.weekly { GaugeRow(label: "7d", window: w) }
                if let w = token.model_usage?.opus_weekly {
                    GaugeRow(label: "Opus", window: w)
                } else {
                    UnavailableGaugeRow(label: "Opus")
                }
                if let w = token.model_usage?.sonnet_weekly {
                    GaugeRow(label: "Son", window: w)
                } else {
                    UnavailableGaugeRow(label: "Son")
                }
                if let o = q.overage { GaugeRow(label: "$$", window: o) }

                // Mini chart (7d trend)
                if let snaps = history, snaps.count >= 2 {
                    MiniUtilizationChart(snapshots: snaps)
                        .padding(.top, 2)
                }
            } else if let err = token.error {
                Text(err)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .lineLimit(2)
            }

            // Detail chart (tap to expand)
            if showDetail, let snaps = history, snaps.count >= 2 {
                TokenDetailChart(snapshots: snaps)
                    .padding(.top, 4)
            }
        }
        .padding(10)
        .background(.ultraThinMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .contentShape(Rectangle())
        .onTapGesture {
            if history != nil && (history?.count ?? 0) >= 2 {
                withAnimation(.easeInOut(duration: 0.2)) { showDetail.toggle() }
            }
        }
    }

    private func statusColor(_ s: String) -> Color {
        switch s {
        case "allowed": return .green
        case "allowed_warning": return .orange
        case "rejected": return .red
        default: return .secondary
        }
    }
}

struct UnavailableGaugeRow: View {
    let label: String

    var body: some View {
        HStack(spacing: 6) {
            Text(label)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.secondary)
                .frame(width: 34, alignment: .trailing)
            Text("profile usage unavailable")
                .font(.system(size: 10))
                .foregroundStyle(.tertiary)
            Spacer()
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
        .onAppear { loadFromConfig() }
        .onChange(of: vm.showSettings) { _ in loadFromConfig() }
    }

    private func loadFromConfig() {
        launchArgs = vm.config.settings.launchArgs.joined(separator: " ")
        terminal = vm.config.settings.terminal ?? ""
        claudeBin = vm.config.settings.claudeBin ?? ""
        probeInterval = "\(vm.config.settings.probeIntervalSecs)"
    }
}

struct PopoverContent: View {
    @ObservedObject var vm: TokemanViewModel

    var body: some View {
        VStack(spacing: 0) {
            // Header
            HStack {
                Text("Tokeman").font(.headline)
                Spacer()
                if let last = vm.lastProbe {
                    Text("\(Int(-last.timeIntervalSinceNow))s ago")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                Button(action: { vm.probe() }) {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .disabled(vm.isProbing)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            Divider()

            if vm.tokens.isEmpty {
                VStack(spacing: 8) {
                    if let err = vm.probeError {
                        Image(systemName: "exclamationmark.triangle")
                            .font(.title2)
                            .foregroundStyle(.orange)
                        Text(err)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 20)
                        Button("Retry") { vm.probe() }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                    } else {
                        ProgressView()
                        Text("Probing tokens...")
                            .foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, minHeight: 120)
            } else {
                // Comparison chart (all tokens overlaid)
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

                // Token cards — no maxHeight, auto-expand for up to ~8 accounts
                VStack(spacing: 6) {
                    ForEach(vm.tokens) { token in
                        TokenCard(token: token, history: vm.history[token.token_name]) {
                            vm.launchToken(token.token_name)
                        }
                    }
                }
                .padding(10)

                Divider()

                // Bottom actions
                VStack(spacing: 8) {
                    HStack {
                        Button(action: { vm.launchBest() }) {
                            Label("Launch Best", systemImage: "paperplane.fill")
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .disabled(vm.bestToken == nil)

                        if let best = vm.bestToken {
                            let pct = Int((1.0 - (best.quota?.weekly?.utilization ?? 1.0)) * 100)
                            Text("\(best.token_name) (\(pct)%)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }

                        Spacer()
                    }

                    // Danger mode toggle
                    HStack {
                        Toggle(isOn: Binding(
                            get: { vm.config.settings.dangerousMode },
                            set: { _ in vm.toggleDangerMode() }
                        )) {
                            HStack(spacing: 4) {
                                Image(systemName: "exclamationmark.shield.fill")
                                Text("--dangerously-skip-permissions")
                                    .font(.system(size: 11, design: .monospaced))
                            }
                            .foregroundStyle(vm.config.settings.dangerousMode ? .red : .secondary)
                        }
                        .tint(.red)
                        .toggleStyle(.switch)
                        .controlSize(.small)
                    }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
            }

            Divider()

            // Settings
            SettingsSection(vm: vm)
                .padding(.horizontal, 14)
                .padding(.vertical, 8)

            Divider()

            // Quit
            Button("Quit Tokeman") { NSApplication.shared.terminate(nil) }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .font(.caption)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
        }
        .frame(width: 400)
        .onAppear { vm.startPolling() }
    }
}

// MARK: - App

@main
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
