# CLAUDE.md — Ground Rules for LLMs in this Project

This file tells coding agents (Claude Code and others) exactly how work is done
in **wusel**. It takes precedence over default behaviour.

## Project in one sentence

`wusel` is a virtual Nextcloud filesystem written in Rust (**VFS-first**:
online-only, on-demand hydration) — more than just FUSE. See the
[Architecture](documentation/modules/ROOT/pages/explanation/architecture.adoc).

## Language

- **Source code and documentation are exclusively in English** — comments,
  doc-comments, AsciiDoc pages, README, config comments, commit messages, logs,
  CLI output, and terminal-facing errors.
- **One exception: end-user *notification* text is localized** (translated to the
  user's language). Non-technical users see OS notifications and many do not read
  English; logs and everything else stay English. Translations live behind the
  structured `desktop::Notice` enum (`Notice::localize`), never as scattered
  strings — so this stays the single, contained place we speak the user's language.
- Chat with the user is in the language the user uses; **repository artefacts are
  English, except localized notification strings.**

## Communication

Answers are crisp, specific, and to the point — no filler, no slang, no
marketing tone, no self-praise. Substance over polish: state facts, give a
recommendation instead of listing every option.

## Toolchain — mise only

- **All** binaries/toolchains are managed via `mise` and pinned in `mise.toml`.
  No direct `rustup`/`brew`/global `npm` for toolchains.
- Invoke tools via `mise exec -- <cmd>` or `mise run <task>`.
- Use mise inside container images too (same `mise.toml`), not a `rust:` base image.

## Dependencies

Keep third-party dependencies to a minimum — **as few as possible, as many as
necessary**. Prefer the standard library, or a few lines of our own code, over
pulling in a crate; when a crate is genuinely warranted, choose a small,
well-maintained one and justify it. Every dependency is attack surface, build
time, and maintenance cost. (Example already in the code: `config.rs` derives
XDG paths by hand instead of adding the `dirs` crate.)

## Git — feature-branch model

- **Never commit directly to `main`.** Create a feature branch for every change
  (`feat/…`, `fix/…`, `docs/…`) and integrate via pull/merge request.
- `main` stays buildable and green at all times.
- Commit messages follow **Conventional Commits 1.0.0**
  (https://www.conventionalcommits.org/en/v1.0.0/): `type(scope): summary`,
  English, imperative, topically focused (no catch-all commits).
  Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `build`, `ci`.
  Breaking changes: `!` after the type or a `BREAKING CHANGE:` footer.
  Example: `feat(webdav): parse PROPFIND multistatus`.
- **A commit or MR message states the *what*, short and factual.** The diff
  already shows the *how* — do not narrate it a second time. Say what the change
  does and, in a clause, why it matters.
- **No working through the past.** No account of the investigation, no story of
  how the defect came about, no dwelling on what the code used to do, no
  self-justification. One sentence of prior behaviour is allowed where the change
  is otherwise unintelligible; more is noise in `git log`.
- Mechanism belongs in the message only when the change is genuinely intricate
  and the diff cannot carry the reasoning alone — then a short paragraph, never
  an essay. Durable explanation belongs in code comments and the docs, which are
  read; a commit body is read once, if ever.

## Architecture & crates

- `wusel-core` — engine (auth, webdav, model, state/SQLite, config); platform-independent.
- `wusel-fuse` — FUSE frontend as a **library**, cfg-gated to Linux (`target_os = "linux"`).
- `wusel-desktop` — OS integration (notifications, D-Bus, GNOME search provider); Linux, no-op elsewhere.
- `wusel-mock` — mock Nextcloud server for the tests.
- `wusel` — daemon/CLI binary (the product); FUSE behind the `fuse` Cargo feature.
- Design and rationale live in the
  [Architecture](documentation/modules/ROOT/pages/explanation/architecture.adoc) docs, not
  here.

## Build & test

- Engine + CLI natively on macOS: `mise run check`, `mise run test`, `mise run clippy`.
- The FUSE mount needs a driver:
  - **Linux:** in the podman container — `mise run fuse-build` / `mise run fuse-shell`.
    The scripts probe where the podman VM sees the repo (`scripts/podman-lib.sh`):
    directly, via a `/Volumes/<disk>` → `/var/mnt/<disk>` disk share, or — as the
    fallback — an rsync mirror under `/private/tmp` (see the development docs).
  - **macOS:** the mount is Linux-only, so test it via the podman container
    (`mise run fuse-shell`). Native macOS support (a File Provider frontend, not
    FUSE) is far-future, experimental work.
- **Keep `main` green:** `check` + `test` must pass before merging.

## Project plans & decisions

What is *planned* — priorities, roadmap, design, and decisions — lives
**only in the docs**, never here. Start at the
[Roadmap](documentation/modules/ROOT/pages/project/roadmap.adoc) and
[Architecture](documentation/modules/ROOT/pages/explanation/architecture.adoc).
(The licence is decided: Apache-2.0 — see
[Licence](documentation/modules/ROOT/pages/project/licence.adoc).)

## Documentation

- Antora component under **`documentation/`** (not `docs/`).
- Build: `mise exec -- ./documentation/build.sh` (official) or
  `mise exec -- ./documentation/build.sh watch` (live). `mise exec` puts `antora`
  (pinned via the npm backend in `mise.toml`) on PATH.
- UI bundle under `documentation/ui-bundle` (adapted from the Antora Default
  UI; MPL-2.0 — see its `LICENSE` and `NOTICE`).
- The docs follow **Diátaxis** (https://diataxis.fr/): `tutorials/`, `how-to/`,
  `reference/`, `explanation/` and `project/` under
  `documentation/modules/ROOT/pages/`. Put a page in the quadrant that matches
  what it *does*, and do not mix modes on one page — a how-to explains nothing,
  an explanation instructs nobody.
- **No platform is the default.** Name a distribution only where it actually
  differs (the install command), and prefer the package family — RPM-based,
  Debian-based, Arch — to a single release. Linux is the subject; macOS is a
  deviation that lives on its own page for people developing from a Mac.
- As a Rust learning project: **didactic comments** — the *why* plus the Rust
  concepts — in the code itself.
