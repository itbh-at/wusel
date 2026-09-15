# CLAUDE.md — Ground Rules for LLMs in this Project

This file tells coding agents (Claude Code and others) exactly how work is done
in **wusel**. It takes precedence over default behaviour. It carries standing
rules and settled facts only — never plans, rationale or design (those live in
the docs).

## Project in one sentence

`wusel` is a virtual Nextcloud filesystem written in Rust (**VFS-first**:
online-only, on-demand hydration) — more than just FUSE. See the
[Architecture](documentation/modules/ROOT/pages/explanation/architecture.adoc).

## Language

- **Everything in this repository is English** — code, comments, doc-comments,
  AsciiDoc pages, README, config comments, commit messages, logs, CLI output,
  terminal-facing errors, and this file.
- **One exception: end-user *notification* text is localized** (translated to the
  user's language). Non-technical users see OS notifications and many do not read
  English. Translations live behind the structured `desktop::Notice` enum
  (`Notice::localize`), never as scattered strings — so this stays the single,
  contained place we speak the user's language.
- Chat with the user is in the language the user uses.

## Communication

Answers are crisp, specific, and to the point — no filler, no slang, no
marketing tone, no self-praise. Adopt the stance of a seasoned, pragmatic
old-school senior developer: direct and minimal.

- **No small talk, no preamble, no narration.** Start straight with the answer or
  the code. Do not announce what you are about to do, do not retell the steps at
  the end.
- **No jargon, no buzzwords, no marketing language.** No flowery description of
  your thought process ("I solved this elegantly…"), no self-congratulatory
  explanations.
- **Report only failures and assumptions** — a failing test or check, and any
  assumption you had to make. Nothing else: no summary of what was done.
- **Explain only on request.** When code is asked for, deliver only the code plus
  the minimum necessary inline comments — no prose around it.
- **Recommend, do not enumerate.** Give one recommendation instead of listing
  every option.

## Scope

- **Change nothing that is not directly related to the request.** The one
  expected exception is the pre-commit hook (`mise run setup-hooks`), which
  reformats what it touches.
- A genuine problem with the request is worth one or two sentences — then finish
  the work under a stated assumption rather than stopping.

## Toolchain — mise only

- **All** language toolchains and dev tools are pinned in `mise.toml` and used
  through `mise`. No direct `rustup`/`brew`/global `npm`.
- `mise run <task>` when a task exists (the list is in `mise.toml`), otherwise
  `mise exec -- <cmd>`.
- Use mise inside container images too (same `mise.toml`), not a `rust:` base image.
- Not managed by mise, and therefore host prerequisites: `podman` and the FUSE
  driver (`/dev/fuse`, libfuse3).

## Dependencies

Keep third-party dependencies to a minimum — **as few as possible, as many as
necessary**. Prefer the standard library, or a few lines of our own code, over
pulling in a crate; when a crate is genuinely warranted, choose a small,
well-maintained one and justify it. Every dependency is attack surface, build
time, and maintenance cost. (Example already in the code: `config.rs` derives
XDG paths by hand instead of adding the `dirs` crate.)

## Git — feature-branch model

- **Never commit directly to `main`.** Create a feature branch for every change
  (`feat/…`, `fix/…`, `docs/…`) and integrate via merge request. `main` stays
  buildable and green — the gates are under [Build & test](#build--test).
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

- `wusel-fsm` — the decision core: occupancy and flow steps as decisions over
  plain data. No I/O, no dependencies at all.
- `wusel-core` — engine (auth, webdav, model, state/SQLite, config); platform-independent.
- `wusel-ipc` — socket frontend speaking the engine's intent protocol, for
  out-of-process frontends; platform-independent.
- `wusel-fuse` — FUSE frontend as a **library**, cfg-gated to Linux (`target_os = "linux"`).
- `wusel-desktop` — OS integration (notifications, D-Bus, GNOME search provider); Linux, no-op elsewhere.
- `wusel-mock` — mock Nextcloud server for the tests.
- `wusel` — daemon/CLI binary (the product); FUSE behind the `fuse` Cargo feature.

## Build & test

- **Linux** — the target platform, and what CI builds. Everything runs natively:
  `mise run check`, `mise run test`, `mise run clippy`, `mise run build-fuse`.
  The mount tests additionally need `/dev/fuse`: `mise run fuse-test`, or
  `mise run fuse-shell` for an interactive mount.
- **macOS** — a deviation for developing from a Mac. `wusel-fuse` cannot be
  compiled without the Linux FUSE driver, so `check`/`test`/`clippy` cover engine
  and CLI only; anything FUSE goes through the podman container
  (`mise run fuse-build` / `fuse-shell` / `fuse-test`). Those scripts probe how
  the podman VM sees the repo (`scripts/podman-lib.sh`): directly, via a
  `/Volumes/<disk>` → `/var/mnt/<disk>` disk share, or — as the fallback — an
  rsync mirror under `/private/tmp` (see the development docs). Native macOS
  support (a File Provider frontend, not FUSE) is far-future, experimental work.
- **Keep `main` green:** the full CI gate is `fmt-check`, `headers-check`,
  `shellcheck`, `clippy`, `check`, `test`, `build-fuse` — all of them, before
  merging. The `.githooks/pre-push` hook (`mise run setup-hooks`) runs that same
  gate — including `fuse-test`, which lints wusel-fuse in the container the host
  checks cannot — before a feature branch is pushed, so a break is caught before
  review rather than after.

## Project plans & decisions

Everything *planned or argued* — priorities, roadmap, design, rationale,
decisions — lives **only in the docs**, never here. Start at the
[Roadmap](documentation/modules/ROOT/pages/project/roadmap.adoc). The licence is
settled: Apache-2.0, headers applied by `mise run headers` (see
[Licence](documentation/modules/ROOT/pages/project/licence.adoc)).

## Documentation

- Antora component under **`documentation/`** (not `docs/`).
- Build: `mise exec -- ./documentation/build.sh` (official) or
  `mise exec -- ./documentation/build.sh watch` (live). `mise exec` puts `antora`
  (pinned via the npm backend in `mise.toml`) on PATH.
- Diagrams are d2 sources; re-render with `mise run docs-diagrams` after editing
  one.
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
