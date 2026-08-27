import SwiftUI
import UHCKit

/// Every zone UHC knows about. Playing zones sort to the top: on a watch the
/// list is scrolled with a crown and the thing making noise should not be
/// below the fold.
struct ZoneListView: View {
    @Environment(WatchController.self) private var controller

    private var orderedZones: [Zone] {
        controller.zones.sorted { lhs, rhs in
            if lhs.state.isPlaying != rhs.state.isPlaying { return lhs.state.isPlaying }
            return lhs.name.localizedCaseInsensitiveCompare(rhs.name) == .orderedAscending
        }
    }

    var body: some View {
        List {
            if let message = controller.errorMessage {
                Text(message)
                    .font(.footnote)
                    .foregroundStyle(.orange)
            }

            ForEach(orderedZones) { zone in
                NavigationLink {
                    NowPlayingView()
                        .onAppear { controller.selectedZoneID = zone.id }
                } label: {
                    ZoneRow(zone: zone)
                }
            }

            NavigationLink("Server") { ServerSetupView() }
                .font(.footnote)
        }
        .listStyle(.carousel)
        .refreshable { await controller.refreshZones() }
    }
}

struct ZoneRow: View {
    let zone: Zone

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: icon)
                .foregroundStyle(zone.state.isPlaying ? Color.green : Color.secondary)
                .font(.caption)
            VStack(alignment: .leading, spacing: 1) {
                Text(zone.name)
                    .font(.body)
                    .lineLimit(1)
                Text(zone.state.rawValue.capitalized)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }

    private var icon: String {
        switch zone.state {
        case .playing: return "speaker.wave.2.fill"
        case .paused: return "pause.fill"
        case .loading, .buffering: return "clock"
        default: return "speaker.slash"
        }
    }
}
