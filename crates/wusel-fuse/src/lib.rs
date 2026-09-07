// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! wusel-fuse — the **FUSE frontend** of wusel, as a library.
//!
//! Task: translate FUSE kernel callbacks (inode-based) into calls against
//! [`wusel_core`] (path-based). The actual logic lives in `wusel-core`; here we only
//! have the kernel bridge.
//!
//! ## Rust learning note: conditional compilation (`cfg`)
//! The mount needs a kernel FUSE driver (libfuse3), so this frontend is
//! **Linux-only**. On other platforms it should not even be built: the
//! `#[cfg(...)]` attribute makes the marked code exist for the compiler only when
//! its condition holds, so macOS and Windows builds see `mount` as a stub instead
//! of a build error. (macOS and Windows will get their own native frontends much
//! later — File Provider and Cloud Filter — not FUSE.)

#[cfg(target_os = "linux")]
mod diag;
#[cfg(target_os = "linux")]
mod dispatch;
#[cfg(target_os = "linux")]
mod fs;

/// Mounts the filesystem at the path `mountpoint` (blocks until unmount).
///
/// The daemon builds the [`wusel_core::provider::Provider`] from stored credentials
/// and hands it in. On Linux this delegates to the real mount; elsewhere it
/// returns an error instead of breaking the build.
#[cfg(target_os = "linux")]
pub fn mount(
    mountpoint: &std::path::Path,
    provider: wusel_core::provider::Provider,
) -> anyhow::Result<()> {
    fs::mount(mountpoint, provider)
}

/// Everything the daemon wires into a mount besides the engine itself — a
/// second frontend sharing the one substrate. See [`mount_with`].
#[cfg(target_os = "linux")]
pub use fs::Extras;

/// Mounts the filesystem, additionally hosting a second frontend on the same
/// engine: the IPC socket a file manager queries for per-file status.
///
/// The mount and the socket cannot each start their own substrate — two engines
/// over one state database — so the mount starts it and lends the other a
/// [`wusel_core::runtime::SubmitHandle`] plus a route for its answers.
#[cfg(target_os = "linux")]
pub fn mount_with(
    mountpoint: &std::path::Path,
    provider: wusel_core::provider::Provider,
    extras: Extras,
) -> anyhow::Result<()> {
    fs::mount_with(mountpoint, provider, extras)
}

/// Stub for non-Linux platforms (no FUSE driver).
#[cfg(not(target_os = "linux"))]
pub fn mount(
    _mountpoint: &std::path::Path,
    _provider: wusel_core::provider::Provider,
) -> anyhow::Result<()> {
    anyhow::bail!("the FUSE mount is Linux-only; on macOS run it inside the podman container")
}
