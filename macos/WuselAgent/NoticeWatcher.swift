// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation
import UserNotifications

/// Turns the engine's user notices into Notification Center banners.
///
/// The socket's `notices` op pushes a `notice` frame whenever the engine reports
/// something the user should see — a conflict copy was made, an upload cannot
/// complete, the connection was lost or came back. Each arrives already localized
/// to the user's language (the engine is the one place that translates), so this
/// only maps it onto a `UNNotificationRequest` and posts it.
///
/// It lives in the agent, not the File Provider extension, for two reasons: only
/// a full app may post to Notification Center (an app extension cannot), and the
/// agent is the persistent process, while the extension is ephemeral. It is the
/// twin of `ChangeWatcher`: one long-lived `notices` connection, re-established
/// with a short backoff if the stream drops (e.g. `wusel serve` restarted).
///
/// It is also the notification-center **delegate**: without one, macOS shows no
/// banner while the posting app counts as active (an accessory agent does),
/// filing the notice straight into Notification Center. The delegate's
/// `willPresent` asks for the banner explicitly, so a notice pops up rather than
/// only landing in the list.
final class NoticeWatcher: NSObject, UNUserNotificationCenterDelegate {
    private let socketPath: String
    private let domain: NSFileProviderDomain
    private let center = UNUserNotificationCenter.current()
    private let queue = DispatchQueue(label: "at.itbh.wusel.notice-watcher")
    private var stopped = false
    private let reconnectDelay: TimeInterval = 5

    init(socketPath: String, domain: NSFileProviderDomain) {
        self.socketPath = socketPath
        self.domain = domain
        super.init()
    }

    /// Ask the user to allow notifications, then start listening. Best-effort:
    /// if permission is denied, posts are dropped by the system and the stream
    /// still runs harmlessly, so a later grant needs no restart.
    func start() {
        // Become the delegate before posting, so `willPresent` can force a banner
        // even when the agent is the active app.
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { granted, error in
            if let error = error {
                AgentLog.log("notices: authorization request failed: \(error.localizedDescription)")
            } else {
                AgentLog.log("notices: authorization \(granted ? "granted" : "denied")")
            }
            // Log what the system actually thinks our notification settings are —
            // authorization status, the alert style (0 none / 1 banner / 2 alert),
            // and whether alerts are enabled at all — so a missing banner points at
            // the real cause (Focus, "none" style, list-only) rather than a guess.
            UNUserNotificationCenter.current().getNotificationSettings { s in
                AgentLog.log(
                    "notices: settings auth=\(s.authorizationStatus.rawValue) "
                        + "alertStyle=\(s.alertStyle.rawValue) alertSetting=\(s.alertSetting.rawValue) "
                        + "center=\(s.notificationCenterSetting.rawValue) "
                        + "lockScreen=\(s.lockScreenSetting.rawValue)")
            }
        }
        queue.async { [self] in
            stopped = false
            loop()
        }
    }

    func stop() {
        queue.async { [self] in stopped = true }
    }

    /// Called when a notice arrives while the agent counts as the active app.
    /// Returning `.banner`/`.sound` makes it pop up as if the app were in the
    /// background; without this the notice would only appear in Notification
    /// Center. `.list` keeps it in the center afterwards too.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .sound, .list])
    }

    private func loop() {
        guard !stopped else { return }
        do {
            let client = try SocketClient(socketPath: socketPath)
            try client.notices()
            AgentLog.log("notices: subscribed to the notice stream")
            while !stopped, let response = try client.nextPush() {
                if case .notice(let kind, let severity, let title, let body) = response {
                    post(severity: severity, title: title, body: body)
                    react(toNoticeKind: kind)
                }
            }
        } catch {
            AgentLog.log("notices: \(error.localizedDescription)")
        }
        // The stream ended or failed; retry unless we are shutting down.
        if !stopped {
            queue.asyncAfter(deadline: .now() + reconnectDelay) { [self] in loop() }
        }
    }

    /// Act on a notice beyond showing it. When the connection is restored, run
    /// the reconcile that a change-log reset had to skip while the server was
    /// unreachable (the extension defers it precisely so a reimport never wedges a
    /// folder with a stuck upload error): nudge the working set and re-import the
    /// opened folders, now safely against a reachable server. Keyed on the stable
    /// notice id, never the translated text.
    private func react(toNoticeKind kind: String) {
        guard kind == "connection-restored" else { return }
        AgentLog.log("notices: connection restored — reconciling opened folders")
        if let manager = NSFileProviderManager(for: domain) {
            manager.signalEnumerator(for: .workingSet) { _ in }
        }
        KnownContainers.reconcileOpenedFolders(domain: domain, log: AgentLog.log)
    }

    /// Post one notice as a banner. A fresh identifier each time so notices stack
    /// rather than replace one another.
    private func post(severity: Severity, title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        // All severities show a banner (interruption level .active); only the
        // sound differs. Good news (a resolved problem) is seen but silent —
        // .passive would drop it into Notification Center with no banner at all,
        // which reads as "nothing happened".
        content.interruptionLevel = .active
        switch severity {
        case .success:
            break  // banner, no sound
        case .warning, .error:
            content.sound = .default
        }
        AgentLog.log("notices: posting [\(severity.rawValue)] \(title)")
        let request = UNNotificationRequest(
            identifier: UUID().uuidString, content: content, trigger: nil)
        center.add(request) { error in
            if let error = error {
                AgentLog.log("notices: could not post: \(error.localizedDescription)")
            }
        }
    }
}
