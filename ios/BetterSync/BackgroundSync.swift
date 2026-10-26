import BackgroundTasks
import Foundation

/// iOS gives apps no long-running background work: BetterSync syncs while
/// it is open, finishes for up to ~25 s after it is left, and catches up
/// whenever iOS grants background time (app refresh, processing tasks).
enum BackgroundSync {
    static let refreshID = "com.bettersync.refresh"
    static let syncID = "com.bettersync.sync"

    static func register() {
        for id in [refreshID, syncID] {
            BGTaskScheduler.shared.register(forTaskWithIdentifier: id, using: nil) { handle($0) }
        }
    }

    static func schedule() {
        let refresh = BGAppRefreshTaskRequest(identifier: refreshID)
        refresh.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(refresh)
        let sync = BGProcessingTaskRequest(identifier: syncID)
        sync.requiresNetworkConnectivity = true
        try? BGTaskScheduler.shared.submit(sync)
    }

    /// Keep syncing for a moment after the app is left.
    static func finishInBackground() {
        ProcessInfo.processInfo.performExpiringActivity(withReason: "Finish syncing") { expired in
            guard !expired else { return }
            let done = DispatchSemaphore(value: 0)
            Task.detached {
                await waitUntilCaughtUp(limit: 25)
                done.signal()
            }
            done.wait()
        }
    }

    private static func handle(_ task: BGTask) {
        schedule()
        let limit: TimeInterval = task is BGProcessingTask ? 600 : 25
        let work = Task {
            await Daemon.shared.start()
            await waitUntilCaughtUp(limit: limit)
            if !Task.isCancelled {
                task.setTaskCompleted(success: true)
            }
        }
        task.expirationHandler = {
            work.cancel()
            task.setTaskCompleted(success: false)
        }
    }

    /// Wait until a paired device is connected and nothing is syncing, or
    /// nobody is reachable, or `limit` seconds pass.
    static func waitUntilCaughtUp(limit: TimeInterval) async {
        let start = Date()
        while Date().timeIntervalSince(start) < limit, !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 2_000_000_000)
            guard let status = Daemon.status() else { continue }
            let elapsed = Date().timeIntervalSince(start)
            let peers = status["peers"] as? [[String: Any]] ?? []
            let connected = peers.contains { $0["connected"] as? Bool == true }
            let folders = status["folders"] as? [[String: Any]] ?? []
            let busy = folders.contains { $0["state"] as? String == "syncing" }
            if connected && !busy && elapsed > 6 { return }
            if !connected && elapsed > 20 { return }
        }
    }
}
