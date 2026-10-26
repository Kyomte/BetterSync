import SwiftUI

@main
struct BetterSyncApp: App {
    @Environment(\.scenePhase) private var phase
    @StateObject private var daemon = Daemon.shared

    init() {
        BackgroundSync.register()
    }

    var body: some Scene {
        WindowGroup {
            ContentView().environmentObject(daemon)
        }
        .onChange(of: phase) { newPhase in
            switch newPhase {
            case .active:
                Task { await daemon.start() }
            case .background:
                BackgroundSync.finishInBackground()
                BackgroundSync.schedule()
            default:
                break
            }
        }
    }
}
