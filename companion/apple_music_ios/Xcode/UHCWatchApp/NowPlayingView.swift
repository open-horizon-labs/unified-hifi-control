import SwiftUI
import WatchKit
import UHCKit

/// Artwork, three lines of text, transport, and the Digital Crown bound to
/// volume.
struct NowPlayingView: View {
    @Environment(WatchController.self) private var controller

    /// Crown state. Kept local and pushed to the server on change rather than
    /// bound straight through: the crown emits a continuous stream and one
    /// request per tick would flood a zone.
    @State private var crownVolume: Double = 0
    @State private var crownPrimed = false
    @State private var volumeCommit: Task<Void, Never>?
    @State private var wasAtLimit = false

    var body: some View {
        ScrollView {
            VStack(spacing: 6) {
                artwork
                titles
                transportControls
                volumeReadout
            }
            .padding(.horizontal, 4)
        }
        .navigationTitle(controller.selectedZone?.name ?? "Now Playing")
        .navigationBarTitleDisplayMode(.inline)
        .focusable()
        .digitalCrownRotation(
            $crownVolume,
            from: controller.volumeControl?.min ?? 0,
            through: controller.volumeControl?.max ?? 100,
            by: controller.volumeControl?.step ?? 1,
            sensitivity: .medium,
            isContinuous: false,
            isHapticFeedbackEnabled: true
        )
        .onChange(of: crownVolume) { _, newValue in
            // Ignore the first value: SwiftUI seeds the binding before the
            // zone's real volume is known, and acting on it would yank the
            // volume to zero the moment the screen appears.
            guard crownPrimed else { return }
            reportLimit(newValue)
            scheduleVolumeCommit(newValue)
        }
        .onChange(of: controller.nowPlaying?.volume) { _, newValue in
            guard !crownPrimed, let newValue else { return }
            crownVolume = newValue
            crownPrimed = true
        }
        .task {
            await controller.refreshNowPlaying()
            if let volume = controller.nowPlaying?.volume {
                crownVolume = volume
                crownPrimed = true
            }
        }
    }

    // MARK: - Pieces

    @ViewBuilder
    private var artwork: some View {
        Group {
            if let image = controller.artwork {
                Image(uiImage: image)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
            } else {
                RoundedRectangle(cornerRadius: 6)
                    .fill(.quaternary)
                    .overlay {
                        Image(systemName: "music.note")
                            .font(.title3)
                            .foregroundStyle(.secondary)
                    }
                    .aspectRatio(1, contentMode: .fit)
            }
        }
        .frame(maxWidth: 90)
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    @ViewBuilder
    private var titles: some View {
        VStack(spacing: 1) {
            Text(controller.nowPlaying?.title ?? "—")
                .font(.headline)
                .lineLimit(2)
                .multilineTextAlignment(.center)
            if let artist = controller.nowPlaying?.artist, !artist.isEmpty {
                Text(artist)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            if let album = controller.nowPlaying?.album, !album.isEmpty {
                Text(album)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
        }
    }

    @ViewBuilder
    private var transportControls: some View {
        let np = controller.nowPlaying
        HStack(spacing: 10) {
            Button {
                Task { await controller.previous() }
            } label: {
                Image(systemName: "backward.fill")
            }
            .disabled(!(np?.isPreviousAllowed ?? false))

            Button {
                Task { await controller.playPause() }
            } label: {
                Image(systemName: (np?.isPlaying ?? false) ? "pause.fill" : "play.fill")
                    .font(.title3)
            }
            // The server publishes which commands the zone will accept; honour
            // it rather than offering a button that is guaranteed to fail.
            .disabled(!((np?.isPlayAllowed ?? false) || (np?.isPauseAllowed ?? false)))

            Button {
                Task { await controller.next() }
            } label: {
                Image(systemName: "forward.fill")
            }
            .disabled(!(np?.isNextAllowed ?? false))
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
    }

    @ViewBuilder
    private var volumeReadout: some View {
        if let control = controller.volumeControl {
            // While the crown is moving, show where it points -- not what the
            // server last said. The server's answer lags the turn, and
            // rendering it mid-gesture is what made the number walk backwards
            // under the user's finger.
            let pending = controller.pendingVolume
            let shown = pending ?? control.value
            HStack(spacing: 4) {
                Image(systemName: "speaker.wave.2.fill")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                Text(formatted(shown))
                    .font(.caption2)
                    .monospacedDigit()
                    // Dimmed until the zone confirms it: the number is a
                    // request, and it says so, rather than claiming a change
                    // that has not happened.
                    .foregroundStyle(pending == nil ? .primary : .secondary)
                    .animation(.easeOut(duration: 0.18), value: pending == nil)
            }
            .accessibilityLabel(
                pending == nil
                    ? "Volume \(formatted(shown))"
                    : "Volume \(formatted(shown)), setting"
            )
        }
        if let message = controller.errorMessage {
            Text(message)
                .font(.caption2)
                .foregroundStyle(.orange)
                .multilineTextAlignment(.center)
        }
    }

    private func formatted(_ value: Double) -> String {
        value == value.rounded()
            ? String(Int(value))
            : String(format: "%.1f", value)
    }

    /// Coalesce crown motion into one request per quiet period. Without this a
    /// single flick of the crown becomes dozens of `vol_abs` commands.
    /// Commit once the crown has been still for a beat. 250ms fired partway
    /// through an ordinary turn, so a single adjustment became several writes
    /// racing each other; a hand comes to rest well inside 600ms.
    private static let crownQuietPeriod = Duration.milliseconds(600)

    private func scheduleVolumeCommit(_ value: Double) {
        volumeCommit?.cancel()
        volumeCommit = Task {
            try? await Task.sleep(for: Self.crownQuietPeriod)
            guard !Task.isCancelled else { return }
            let landed = await controller.commitVolume(value)
            guard !Task.isCancelled else { return }
            // Confirm on the wrist. The crown already ticks while turning;
            // this is the distinct beat that says the zone actually took it,
            // which is the whole difference between guessing and knowing.
            WKInterfaceDevice.current().play(landed ? .click : .failure)
        }
    }

    /// The ends of the range deserve to be felt, not discovered by watching a
    /// number stop moving.
    private func reportLimit(_ value: Double) {
        guard let control = controller.volumeControl else { return }
        let atLimit = value <= control.min || value >= control.max
        if atLimit, !wasAtLimit { WKInterfaceDevice.current().play(.retry) }
        wasAtLimit = atLimit
    }
}
