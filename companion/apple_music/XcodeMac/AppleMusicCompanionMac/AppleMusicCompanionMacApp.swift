import SwiftUI
import ServiceManagement
import AppleMusicCompanionMacFeature

@main
struct AppleMusicCompanionMacApp: App {
    @StateObject private var model = CompanionModel()
    @State private var launchAtLoginEnabled = SMAppService.mainApp.status == .enabled
    @State private var launchAtLoginError: String?

    var body: some Scene {
        MenuBarExtra("Apple Music Companion", systemImage: "music.note") {
            MenuBarContentView(model: model)
            Divider()
            Toggle("Launch at login", isOn: Binding(
                get: { launchAtLoginEnabled },
                set: { setLaunchAtLogin($0) }
            ))
            if let launchAtLoginError {
                Text(launchAtLoginError)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 12)
            }
        }

        Window("Apple Music Companion", id: "main") {
            ContentView(model: model)
        }
    }

    private func setLaunchAtLogin(_ enabled: Bool) {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            launchAtLoginEnabled = SMAppService.mainApp.status == .enabled
            launchAtLoginError = nil
        } catch {
            // Keep the control truthful if macOS rejects the registration request.
            launchAtLoginEnabled = SMAppService.mainApp.status == .enabled
            launchAtLoginError = "Launch at login could not be changed. Check macOS Login Items settings."
        }
    }
}
