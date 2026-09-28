// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import Foundation

/// A web action the File Provider extension asks the agent to carry out: open a
/// URL in the browser, or copy it to the clipboard.
///
/// The extension builds the Nextcloud link (via the engine) but **cannot act on
/// it** — a File Provider `.appex` is sandboxed away from `NSWorkspace` and
/// `NSPasteboard`. So it hands the finished request to the agent, which is an
/// ordinary app and may open a browser or write the pasteboard. The Nautilus
/// extension does this in-process; on macOS it needs this hop.
public enum WebActionKind: String, Codable {
    /// Open the URL in the default browser.
    case open
    /// Copy the URL to the clipboard.
    case copy
}

public struct WebAction: Codable {
    public let kind: WebActionKind
    public let url: String
    public init(kind: WebActionKind, url: String) {
        self.kind = kind
        self.url = url
    }
}

/// The extension→agent channel for web actions: a request file dropped in the
/// App Group container, announced by a Darwin notification.
///
/// Darwin notifications carry no payload and cross the sandbox, so the request
/// rides in a file and the notification only says "look". One file per request
/// (a UUID name) rather than a single slot, so two quick clicks cannot clobber
/// each other; the agent drains the whole directory on each wake.
public enum WebActionBridge {
    private static let darwinName = "at.itbh.wusel.web-action"

    private static var directory: URL? {
        SharedPaths.containerURL?.appendingPathComponent("web-actions", isDirectory: true)
    }

    /// Extension side: record a request and wake the agent. Best-effort — if the
    /// App Group container is unavailable there is nowhere shared to leave it, and
    /// the action simply does nothing rather than crash the menu.
    public static func post(_ action: WebAction) {
        guard let directory = directory else { return }
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let file = directory.appendingPathComponent(UUID().uuidString + ".json")
        guard let data = try? JSONEncoder().encode(action) else { return }
        try? data.write(to: file, options: .atomic)
        CFNotificationCenterPostNotification(
            CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(darwinName as CFString), nil, nil, true)
    }

    /// Agent side: take and remove every pending request. Ordering is not
    /// promised, which is fine — each request is independent.
    public static func drain() -> [WebAction] {
        guard let directory = directory,
            let files = try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)
        else { return [] }
        var actions: [WebAction] = []
        for file in files where file.pathExtension == "json" {
            if let data = try? Data(contentsOf: file),
                let action = try? JSONDecoder().decode(WebAction.self, from: data)
            {
                actions.append(action)
            }
            try? FileManager.default.removeItem(at: file)
        }
        return actions
    }

    /// Agent side: run `handler` whenever the extension posts a request. The
    /// Darwin callback is a bare C function pointer that captures nothing, so it
    /// dispatches through a stored closure. Call once, from the agent.
    public static func observe(_ handler: @escaping () -> Void) {
        stored = handler
        let callback: CFNotificationCallback = { _, _, _, _, _ in WebActionBridge.stored?() }
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDarwinNotifyCenter(),
            Unmanaged.passUnretained(token).toOpaque(),
            callback, darwinName as CFString, nil, .deliverImmediately)
    }

    private static var stored: (() -> Void)?
    private static let token = NSObject()
}
