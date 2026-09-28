// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import AppKit

/// Carries out the web actions the File Provider extension cannot perform itself.
///
/// "Open in Nextcloud" / "Copy Internal Link" need a browser and the clipboard,
/// which a sandboxed `.appex` may not touch. The extension builds the link (via
/// the engine) and posts it over `WebActionBridge`; this responder runs in the
/// agent — an ordinary app — and opens or copies it. The Nautilus extension does
/// this in-process; on macOS the work crosses to the agent.
final class WebActionResponder {
    /// Begin listening. Drains once immediately in case a request arrived before
    /// we were watching, then on every posted notification.
    func start() {
        WebActionBridge.observe { [weak self] in self?.handle() }
        handle()
    }

    private func handle() {
        let actions = WebActionBridge.drain()
        guard !actions.isEmpty else { return }
        // NSWorkspace/NSPasteboard are main-thread work.
        DispatchQueue.main.async {
            for action in actions {
                guard let url = URL(string: action.url) else {
                    AgentLog.log("web action: unparseable url \(action.url)")
                    continue
                }
                switch action.kind {
                case .open:
                    NSWorkspace.shared.open(url)
                    AgentLog.log("web action: opened \(url.absoluteString)")
                case .copy:
                    let pasteboard = NSPasteboard.general
                    pasteboard.clearContents()
                    pasteboard.setString(action.url, forType: .string)
                    AgentLog.log("web action: copied a link to the clipboard")
                }
            }
        }
    }
}
