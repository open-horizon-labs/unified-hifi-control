import SwiftUI
import UHCKit

/// Server discovery, manual entry, and the diagnostics readout.
///
/// The diagnostics section is not decoration: on real Watch hardware it is the
/// instrument that answers #619's open question. If a direct LAN request is
/// blocked by the OS rather than by a missing server, the underlying `URLError`
/// shows up here, and that string is what the owner reports back.
struct ServerSetupView: View {
    @Environment(WatchController.self) private var controller
    @State private var manualHost: String = ""

    var body: some View {
        List {
            Section {
                Button {
                    Task { await controller.discover() }
                } label: {
                    Label("Find UHC", systemImage: "antenna.radiowaves.left.and.right")
                }

                if let server = controller.serverDescription {
                    LabeledContent("Server") {
                        Text(server).font(.caption2).lineLimit(2)
                    }
                }
            } header: {
                Text("Connection")
            } footer: {
                Text("Browses for _uhc._tcp on your network.")
                    .font(.caption2)
            }

            Section("Manual address") {
                // Dictation/scribble entry — a watch has no keyboard, so this
                // is the fallback when Bonjour is unavailable, which on watchOS
                // is a real possibility (see Transport.md).
                TextField("192.168.1.209:8088", text: $manualHost)
                    .textContentType(.URL)

                Button("Connect") {
                    guard let url = Self.normalizedURL(from: manualHost) else { return }
                    Task { await controller.connect(to: url) }
                }
                .disabled(Self.normalizedURL(from: manualHost) == nil)
            }

            if controller.errorMessage != nil || controller.lastTransportFailure != nil {
                Section("Diagnostics") {
                    if let message = controller.errorMessage {
                        Text(message)
                            .font(.caption2)
                            .foregroundStyle(.orange)
                    }
                    if let failure = controller.lastTransportFailure {
                        Text(failure)
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                }
            }

            Section {
                Button("Forget server", role: .destructive) {
                    controller.forgetServer()
                }
            }
        }
    }

    /// Accepts "host", "host:port", or a full URL, and always yields an
    /// `http://` URL. Typing a scheme on a watch is miserable, so it is
    /// optional.
    static func normalizedURL(from raw: String) -> URL? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        let candidate = trimmed.contains("://") ? trimmed : "http://\(trimmed)"
        guard let url = URL(string: candidate), let host = url.host, !host.isEmpty else {
            return nil
        }
        return url
    }
}
