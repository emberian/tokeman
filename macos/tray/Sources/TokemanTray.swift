import SwiftUI
import Foundation

// MARK: - JSON models (matches `tokeman --json` output)

struct TokenResult: Codable, Identifiable {
    var id: String { token_name }
    let token_name: String
    let probed_at: String
    let quota: Quota?
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
}

// MARK: - Config reader/writer (~/.config/tokeman/tokens.toml)

struct TokemanConfig {
    struct Token {
        let name: String
        let key: String
    }
    struct Settings {
        var launchArgs: [String] = []
        var dangerousMode: Bool = false
        var terminal: String? = nil
        var claudeBin: String? = nil
        var probeIntervalSecs: Int = 30
    }

    var tokens: [Token] = []
    var settings: Settings = Settings()

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

        for line in content.components(separatedBy: "\n") {
            let t = line.trimmingCharacters(in: .whitespaces)
            if t.isEmpty || t.hasPrefix("#") { continue }

            if t == "[[tokens]]" {
                if let n = curName, let k = curKey {
                    config.tokens.append(Token(name: n, key: k))
                }
                curName = nil; curKey = nil
                section = "tokens"; continue
            }
            if t == "[settings]" {
                if let n = curName, let k = curKey {
                    config.tokens.append(Token(name: n, key: k))
                }
                curName = nil; curKey = nil
                section = "settings"; continue
            }

            guard let eq = t.firstIndex(of: "=") else { continue }
            let key = t[..<eq].trimmingCharacters(in: .whitespaces)
            let val = t[t.index(after: eq)...].trimmingCharacters(in: .whitespaces)

            if section == "tokens" {
                if key == "name" { curName = unquote(val) }
                if key == "key" { curKey = unquote(val) }
            } else if section == "settings" {
                switch key {
                case "dangerous_mode": config.settings.dangerousMode = val == "true"
                case "terminal": config.settings.terminal = unquote(val)
                case "claude_bin": config.settings.claudeBin = unquote(val)
                case "probe_interval_secs": config.settings.probeIntervalSecs = Int(val) ?? 30
                case "launch_args": config.settings.launchArgs = parseArray(val)
                default: break
                }
            }
        }
        if let n = curName, let k = curKey {
            config.tokens.append(Token(name: n, key: k))
        }
        return config
    }

    func save() {
        var lines: [String] = []
        for token in tokens {
            lines.append("[[tokens]]")
            lines.append("name = \"\(Self.escapeToml(token.name))\"")
            lines.append("key = \"\(Self.escapeToml(token.key))\"")
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

        let content = lines.joined(separator: "\n")
        try? content.write(toFile: Self.configPath, atomically: true, encoding: .utf8)
    }

    private static func escapeToml(_ s: String) -> String {
        s.replacingOccurrences(of: "\\", with: "\\\\")
         .replacingOccurrences(of: "\"", with: "\\\"")
    }

    private static func unquote(_ s: String) -> String {
        if s.hasPrefix("\"") && s.hasSuffix("\"") && s.count >= 2 {
            return String(s.dropFirst().dropLast())
        }
        return s
    }

    private static func parseArray(_ s: String) -> [String] {
        let inner = s.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        return inner.components(separatedBy: ",")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .map { unquote($0) }
            .filter { !$0.isEmpty }
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
        tokens
            .filter { $0.quota?.status == "allowed" || $0.quota?.status == "allowed_warning" }
            .min(by: { ($0.quota?.weekly?.utilization ?? 1.0) < ($1.quota?.weekly?.utilization ?? 1.0) })
    }

    func startPolling() {
        probe()
        timer?.invalidate()
        let interval = TimeInterval(config.settings.probeIntervalSecs)
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
            } else {
                probeError = "Failed to parse tokeman output"
            }
            isProbing = false
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
        guard let tok = config.tokens.first(where: { $0.name == name }) else { return }
        let bin = config.settings.claudeBin ?? "claude"
        var args = config.settings.launchArgs
        if config.settings.dangerousMode && !args.contains("--dangerously-skip-permissions") {
            args.append("--dangerously-skip-permissions")
        }
        let cmd = ([bin] + args).joined(separator: " ")
        launchTerminal(cmd: cmd, tokenKey: tok.key, terminal: config.settings.terminal)
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

    private func launchTerminal(cmd: String, tokenKey: String, terminal: String?) {
        let escaped_key = tokenKey.replacingOccurrences(of: "'", with: "'\\''")
        let escaped_cmd = cmd.replacingOccurrences(of: "'", with: "'\\''")
        let app = terminal ?? "Terminal"

        let script: String
        switch app.lowercased() {
        case "iterm2", "iterm":
            script = """
            tell application "iTerm2"
                create window with default profile
                tell current session of current window
                    write text "export CLAUDE_CODE_OAUTH_TOKEN='\(escaped_key)'; \(escaped_cmd)"
                end tell
            end tell
            """
        default:
            script = """
            tell application "Terminal"
                activate
                do script "export CLAUDE_CODE_OAUTH_TOKEN='\(escaped_key)'; \(escaped_cmd)"
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
                    .frame(width: 22, alignment: .trailing)

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
                Spacer().frame(width: 28)
                Text("resets \(resetText)")
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
            }
        }
    }
}

struct TokenCard: View {
    let token: TokenResult
    let onLaunch: () -> Void

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
                if let o = q.overage { GaugeRow(label: "$$", window: o) }
            } else if let err = token.error {
                Text(err)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .lineLimit(2)
            }
        }
        .padding(10)
        .background(.ultraThinMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 8))
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
                // Token cards — no maxHeight, auto-expand for up to ~8 accounts
                VStack(spacing: 6) {
                    ForEach(vm.tokens) { token in
                        TokenCard(token: token) {
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

            // Settings + Quit
            HStack(alignment: .top) {
                SettingsSection(vm: vm)
                Spacer()
                Button("Quit") { NSApplication.shared.terminate(nil) }
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
                    .font(.caption)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
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
