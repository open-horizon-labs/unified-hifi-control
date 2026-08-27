import SwiftUI
import UHCKit

/// UHC on the wrist (#619).
///
/// Deliberately small: pick a zone, see what is playing, drive transport and
/// volume. No library browsing — a watch is a remote, not a catalogue, and the
/// browse endpoints are the expensive half of the API.
@main
struct UHCWatchApp: App {
    @State private var controller = WatchController()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(controller)
        }
    }
}

struct RootView: View {
    @Environment(WatchController.self) private var controller

    var body: some View {
        NavigationStack {
            Group {
                switch controller.connection {
                case .needsServer:
                    ServerSetupView()
                case .connecting:
                    ProgressView("Finding UHC…")
                case .connected:
                    ZoneListView()
                }
            }
            .navigationTitle("UHC")
        }
        .task {
            await controller.startup()
        }
    }
}
