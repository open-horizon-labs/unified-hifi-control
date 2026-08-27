import SwiftUI
import WebKit

/// UHC's own web UI, embedded. The phone deliberately does not reimplement
/// zone control natively: the web UI is already responsive, already the admin
/// surface, and already the thing that gets fixed when a control bug is
/// found. A second hand-written iOS surface would be a second place for those
/// fixes to be forgotten.
struct UHCWebView: UIViewRepresentable {
    let url: URL
    @Binding var reloadToken: Int

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        // The UI drives playback controls, not media playback itself.
        config.allowsInlineMediaPlayback = true
        let view = WKWebView(frame: .zero, configuration: config)
        view.accessibilityIdentifier = "uhcWebView"
        view.allowsBackForwardNavigationGestures = true
        // The server is on the LAN over plain HTTP; pull-to-refresh is the
        // cheapest recovery when the phone changes networks.
        let refresh = UIRefreshControl()
        refresh.addTarget(
            context.coordinator,
            action: #selector(Coordinator.refresh(_:)),
            for: .valueChanged
        )
        view.scrollView.refreshControl = refresh
        context.coordinator.webView = view
        view.load(URLRequest(url: url))
        context.coordinator.loadedURL = url
        return view
    }

    func updateUIView(_ view: WKWebView, context: Context) {
        // Reload when the server address changes, or when the token is bumped.
        if context.coordinator.loadedURL != url || context.coordinator.token != reloadToken {
            context.coordinator.loadedURL = url
            context.coordinator.token = reloadToken
            view.load(URLRequest(url: url))
        }
    }

    final class Coordinator: NSObject {
        weak var webView: WKWebView?
        var loadedURL: URL?
        var token = 0

        @objc func refresh(_ sender: UIRefreshControl) {
            webView?.reload()
            sender.endRefreshing()
        }
    }
}
