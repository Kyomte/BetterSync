import SwiftUI
import WebKit

/// The daemon's status page (pairing, folders, conflicts): the same page
/// the desktop tray app opens in a browser.
struct ContentView: View {
    @EnvironmentObject var daemon: Daemon

    var body: some View {
        VStack(spacing: 0) {
            Text(
                "Folders your other devices share with this one appear in Files › On My iPhone › BetterSync. Syncing runs while the app is open and for a short while after."
            )
            .font(.footnote)
            .foregroundStyle(.secondary)
            .padding(.horizontal)
            .padding(.vertical, 8)
            if let url = daemon.statusURL {
                StatusPage(url: url)
            } else if let error = daemon.error {
                Text("BetterSync could not start: \(error)").padding()
                Spacer()
            } else {
                ProgressView().padding()
                Spacer()
            }
        }
    }
}

struct StatusPage: UIViewRepresentable {
    let url: URL

    func makeUIView(context: Context) -> WKWebView {
        let view = WKWebView()
        view.load(URLRequest(url: url))
        return view
    }

    func updateUIView(_ view: WKWebView, context: Context) {
        // A restarted daemon listens on a new port.
        if view.url?.port != url.port {
            view.load(URLRequest(url: url))
        }
    }
}
