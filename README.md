# Wusel

**Nextcloud, woven into your desktop.**

[![CI](https://github.com/itbh-at/wusel/actions/workflows/ci.yml/badge.svg)](https://github.com/itbh-at/wusel/actions/workflows/ci.yml)

A virtual filesystem for Nextcloud, written in Rust — **VFS-first**: files are
"online only" by default and are only fetched on access (on-demand hydration),
instead of mirroring everything locally.

The goal is what the official client on Linux still does not offer today:
a true on-demand filesystem. `wusel` is deliberately **more than just FUSE** —
FUSE is only *one* frontend; later, `libcloudproviders` (file-manager
integration) and possibly a native macOS File Provider frontend will join it.

## Architecture in one sentence

A platform-independent **engine** (`wusel-core`: Login Flow v2, WebDAV, sync,
state) plus interchangeable, thin **frontends** (`wusel-fuse` for Linux),
held together by the daemon **`wusel`**. This separation makes it possible to
develop and test the bulk of the work natively on macOS — only the mount needs
a FUSE driver.

Details: [Architecture](documentation/modules/ROOT/pages/architecture.adoc) ·
Order of work: [Roadmap](documentation/modules/ROOT/pages/roadmap.adoc). The
docs are an [Antora](https://antora.org) component under
[`documentation/`](documentation/) (build: `./documentation/build.sh`, live:
`./documentation/build.sh watch`).

## Crates

| Crate           | Role                                                                          | Platform                                     |
| --------------- | ----------------------------------------------------------------------------- | -------------------------------------------- |
| `wusel-core`    | Engine: auth, WebDAV, sync, state                                             | everywhere                                   |
| `wusel-fuse`    | FUSE frontend (library)                                                       | Linux                                        |
| `wusel-desktop` | OS integration (notifications, file-manager status) behind `desktop::Desktop` | Linux (no-op elsewhere)                      |
| `wusel-mock`    | Mock Nextcloud server for the tests                                           | dev-only                                     |
| `wusel`         | Daemon/CLI — the product                                                      | everywhere (mount behind the `fuse` feature) |

## Usage

```sh
wusel login https://cloud.example.org   # Login Flow v2 — confirm in the browser
wusel mount ~/Wusel                 # mount (binary built with --features fuse)
wusel service enable                     # optional: auto-mount at login (systemd user service)
```

Files appear *online-only* and are streamed from the server as you read them
(pin a file to keep it offline); server-side changes propagate via `notify_push`
(with a TTL fallback). Real mtimes and read-only permissions are reflected.

- **Pinning** ("always keep offline"): `wusel pin <path>` (a directory, a file,
  or nothing for the whole account), `wusel unpin`, `wusel pins`.
- **Multiple accounts** (optional): add `--account work` to `login`/`mount`;
  `wusel accounts`, `wusel account remove <name>`. The default account needs no flag.
- **Configuration** (optional): `~/.config/wusel/config.toml` — cache
  budget/age, revalidation TTL, TLS trust (`ca_cert` / `insecure` for
  self-signed servers), mountpoint. Step-by-step: [Trying it
  out](documentation/modules/ROOT/pages/trying-it-out.adoc).

## Toolchain

All binaries are managed via **mise** (see `mise.toml`, Rust pinned):

```sh
mise install        # install Rust 1.97.1
```

## Developing & testing

Engine + CLI run natively on macOS (without a FUSE driver):

```sh
mise run check      # cargo check (wusel-core + wusel, no FUSE)
mise run test       # cargo test
mise run clippy
```

### Mounting

The FUSE mount runs on **Linux**. On macOS, test it inside the podman container.
Step-by-step: [Trying it out](documentation/modules/ROOT/pages/trying-it-out.adoc).

```sh
mise run fuse-shell   # Linux shell with /dev/fuse (works on macOS too, via podman)
# inside the shell:
cargo run -p wusel --features fuse -- mount /mnt/nc
```

The mount is Linux-only. Native macOS and Windows support (their own File
Provider / Cloud Filter frontends, not FUSE) is far-future, experimental work; on
a Mac, test the mount inside the podman container.

## Status

See the [Roadmap](documentation/modules/ROOT/pages/roadmap.adoc). Working today:
authentication (Login Flow v2), the live read-only tree (listing + on-demand
content with real mtimes/permissions), whole-file caching with LRU/age eviction,
instant invalidation via `notify_push`, configurable TLS trust, multiple
accounts, pinning ("always keep offline"), a systemd user service, **writing**
(read-write mount: create/edit/rename/delete, chunked upload for large files,
lossless conflict handling with opt-in text merge), and a default-on, fail-soft
OS keyring for the app password (opt-out via `[auth] keyring = false`; if the
keyring is unusable the password stays in the 0600 file). Next:
desktop/file-manager integration.

## License

[Apache-2.0](LICENSE). Chosen so the free sources can also be shipped as signed,
commercial store builds — see _Licensing_ in the
[Roadmap](documentation/modules/ROOT/pages/roadmap.adoc).
