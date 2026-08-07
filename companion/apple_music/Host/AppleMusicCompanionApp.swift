import SwiftUI

/// Example macOS host entry point. A production app should replace the
/// placeholders with setup/Keychain values and provide a polished onboarding
/// flow for the UHC URL and short-lived pairing code.
@main
struct AppleMusicCompanionApp: App {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = CompanionModel(
        baseURL: URL(string: "https://uhc.example.invalid")!,
        companionID: "replace-from-keychain"
    )

    var body: some Scene {
        WindowGroup {
            VStack(spacing: 16) {
                Text("Apple Music Companion")
                Text(model.status).font(.footnote)
                Button("Authorize Apple Music", action: model.authorize)
                TextField("UHC bridge ID", text: $model.bridgeID)
                    .textFieldStyle(.roundedBorder)
                TextField("Short-lived pairing code", text: $model.pairingCodeInput)
                    .textFieldStyle(.roundedBorder)
                Button("Claim this companion", action: model.claim)
                    .disabled(model.bridgeID.isEmpty || model.pairingCodeInput.isEmpty)
                if model.isPaired {
                    Button("Revoke pairing", role: .destructive, action: model.revoke)
                }
            }
            .padding()
            .frame(minWidth: 360)
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { model.startPolling() }
            else { model.stopPolling() }
        }
    }
}
