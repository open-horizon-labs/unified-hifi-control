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
    /// The colour behind the status bar. Sampled from the page rather than
    /// hardcoded, because UHC ships several themes (including a true-black
    /// OLED one) and a fixed colour would be wrong in most of them.
    @Binding var chromeColor: Color

    func makeCoordinator() -> Coordinator { Coordinator(chromeColor: $chromeColor) }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        // The UI drives playback controls, not media playback itself.
        config.allowsInlineMediaPlayback = true
        let view = WKWebView(frame: .zero, configuration: config)
        view.navigationDelegate = context.coordinator
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

    final class Coordinator: NSObject, WKNavigationDelegate {
        weak var webView: WKWebView?
        var loadedURL: URL?
        var token = 0
        private let chromeColor: Binding<Color>

        init(chromeColor: Binding<Color>) {
            self.chromeColor = chromeColor
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            // The page server-renders and then hydrates, so the themed top bar
            // is not painted yet at didFinish. Sample now for the common case
            // and again once hydration has had time to land.
            sampleChrome(webView)
            for delay in [0.8, 2.0] {
                DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak webView] in
                    guard let webView else { return }
                    self.sampleChrome(webView)
                }
            }
        }

        private func sampleChrome(_ webView: WKWebView) {
            // The top bar's colour, falling back to the body's.
            // Sample what is actually painted at the top of the viewport and
            // walk up until something is opaque. Querying `nav`/`header` by
            // tag guessed at markup and picked the wrong element.
            let js = """
            (() => {
              let el = document.elementFromPoint(window.innerWidth / 2, 4);
              while (el) {
                const c = getComputedStyle(el).backgroundColor;
                const m = c.match(/[\\d.]+/g);
                if (m && m.length >= 3 && (m.length < 4 || parseFloat(m[3]) > 0)) return c;
                el = el.parentElement;
              }
              return getComputedStyle(document.body).backgroundColor;
            })()
            """
            webView.evaluateJavaScript(js) { [weak self] value, _ in
                guard let text = value as? String,
                      let color = Coordinator.parseCSSColor(text) else { return }
                self?.chromeColor.wrappedValue = Color(uiColor: color)
            }
        }

        /// `rgb(a)` is the only shape `getComputedStyle` returns here.
        static func parseCSSColor(_ text: String) -> UIColor? {
            let numbers = text
                .replacingOccurrences(of: "rgba(", with: "")
                .replacingOccurrences(of: "rgb(", with: "")
                .replacingOccurrences(of: ")", with: "")
                .split(separator: ",")
                .compactMap { Double($0.trimmingCharacters(in: .whitespaces)) }
            guard numbers.count >= 3 else { return nil }
            // A transparent bar tells us nothing about what shows through.
            if numbers.count >= 4 && numbers[3] == 0 { return nil }
            return UIColor(red: numbers[0] / 255, green: numbers[1] / 255,
                           blue: numbers[2] / 255, alpha: 1)
        }

        @objc func refresh(_ sender: UIRefreshControl) {
            webView?.reload()
            sender.endRefreshing()
        }
    }
}
