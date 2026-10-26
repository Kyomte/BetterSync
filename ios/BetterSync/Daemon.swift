import BsyncFFI
import Foundation
import Network

/// Where the daemon keeps its state, and where synced folders go.
enum Paths {
    static let config = FileManager.default
        .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("BetterSync", isDirectory: true)
    /// Shown in Files › On My iPhone › BetterSync.
    static let folders = FileManager.default
        .urls(for: .documentDirectory, in: .userDomainMask)[0]
}

/// The embedded daemon (crates/ffi) plus the Bonjour glue iOS needs: raw
/// multicast (the daemon's own mDNS) requires a special entitlement, the
/// system's Bonjour does not.
@MainActor
final class Daemon: ObservableObject {
    static let shared = Daemon()

    @Published private(set) var statusURL: URL?
    @Published private(set) var error: String?

    private var started = false
    private var browser: NWBrowser?
    private var advert: NetService?

    /// Start the daemon once; later calls reconnect to paired devices
    /// (after a suspension the backoff would otherwise delay that).
    func start() async {
        if started {
            bsync_network_changed()
            return
        }
        started = true
        let config = Paths.config.path
        let folders = Paths.folders.path
        let result = await Task.detached {
            Daemon.take(bsync_start_mobile(config, folders, 1))
        }.value
        if result.hasPrefix("error: ") {
            error = String(result.dropFirst(7))
            started = false
            return
        }
        statusURL = URL(string: result)
        browse()
        advertise()
    }

    /// The daemon's status (GET /api/status), or nil if it is not running.
    nonisolated static func status() -> [String: Any]? {
        let text = take(bsync_status_json(Paths.config.path))
        guard !text.hasPrefix("error: ") else { return nil }
        return (try? JSONSerialization.jsonObject(with: Data(text.utf8))) as? [String: Any]
    }

    nonisolated static func take(_ s: UnsafeMutablePointer<CChar>?) -> String {
        guard let s else { return "error: no result" }
        defer { bsync_free_string(s) }
        return String(cString: s)
    }

    // ----- Bonjour -----

    /// Report every BetterSync device on the LAN to the daemon.
    private func browse() {
        let browser = NWBrowser(
            for: .bonjourWithTXTRecord(type: "_bsync._udp", domain: nil), using: .udp)
        browser.browseResultsChangedHandler = { results, _ in
            for result in results {
                Daemon.resolve(result)
            }
        }
        browser.start(queue: .global(qos: .utility))
        self.browser = browser
    }

    /// Turn a Bonjour result into `ip:port` (a UDP "connection" resolves
    /// the service without sending anything).
    nonisolated private static func resolve(_ result: NWBrowser.Result) {
        guard case let .bonjour(txt) = result.metadata, let id = txt["id"] else { return }
        let connection = NWConnection(to: result.endpoint, using: .udp)
        connection.stateUpdateHandler = { state in
            switch state {
            case .ready:
                if case let .hostPort(host, port)? = connection.currentPath?.remoteEndpoint,
                    let addr = format(host, port)
                {
                    _ = bsync_discovered(id, addr)
                }
                connection.cancel()
            case .failed, .waiting:
                connection.cancel()
            default:
                break
            }
        }
        connection.start(queue: .global(qos: .utility))
    }

    nonisolated private static func format(_ host: NWEndpoint.Host, _ port: NWEndpoint.Port)
        -> String?
    {
        // Drop any "%en0" interface suffix.
        func bare(_ s: String) -> String { String(s.split(separator: "%").first ?? Substring(s)) }
        switch host {
        case .ipv4(let a): return "\(bare("\(a)")):\(port.rawValue)"
        case .ipv6(let a): return "[\(bare("\(a)"))]:\(port.rawValue)"
        default: return nil
        }
    }

    /// Announce this device the way desktop daemons do (`_bsync._udp`
    /// with its ID in the TXT record), so they can find the phone too.
    private func advertise() {
        guard let status = Daemon.status(),
            let id = status["device_id"] as? String,
            let listen = status["listen"] as? String,
            let port = listen.split(separator: ":").last.flatMap({ Int32($0) })
        else { return }
        let name = String(id.replacingOccurrences(of: "-", with: "").prefix(63))
        let service = NetService(domain: "local.", type: "_bsync._udp.", name: name, port: port)
        service.setTXTRecord(NetService.data(fromTXTRecord: ["id": Data(id.utf8)]))
        service.publish()
        advert = service
    }
}
