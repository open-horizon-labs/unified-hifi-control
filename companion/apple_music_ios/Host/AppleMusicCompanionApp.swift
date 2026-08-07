import SwiftUI

/// Example host entry point. A production target should replace the URL and
/// companion ID placeholders with setup/Keychain values and use QR scanning
/// for the bridge ID and short-lived claim code.
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
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                TextField("Short-lived pairing code", text: $model.pairingCodeInput)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                Button("Claim this companion") {
                    model.claim(bridgeID: model.bridgeID, pairingCode: model.pairingCodeInput)
                }
                .disabled(model.bridgeID.isEmpty || model.pairingCodeInput.isEmpty)
                if model.isPaired {
                    Button("Revoke pairing", role: .destructive, action: model.revoke)
                }
            }
            .padding()
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { model.startPolling() }
            else { model.stopPolling() }
        }
    }
}
