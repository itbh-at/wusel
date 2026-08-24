<!-- SPDX-License-Identifier: Apache-2.0 -->
# Security Policy

Wusel holds a Nextcloud app password and mounts a filesystem into your session.
We take reports about either seriously, and we would rather hear about a problem
privately first.

## Reporting a vulnerability

**Do not open a public issue for a suspected vulnerability.**

Use either private channel:

- **GitHub private vulnerability reporting** — on
  <https://github.com/itbh-at/wusel>, go to *Security* → *Report a
  vulnerability*. This is the preferred route; it keeps the report, the
  discussion and the fix in one private thread.
- **Email** — <support@itbh.at>. Put "Wusel Security" in the subject.

A useful report contains: the affected version or commit, your OS and desktop,
what an attacker gains, and the smallest set of steps that reproduces it. A proof
of concept helps; a scanner's raw output on its own usually does not.

## What to expect

Wusel is developed by a small team at IT Beratung Hermann GmbH, so these are
realistic commitments rather than enterprise SLAs:

| Stage | Target |
| ----- | ------ |
| Acknowledgement that we received the report | within 5 working days |
| First assessment (valid / not / need more info) | within 10 working days |
| Fix or a stated plan, for a confirmed issue | as fast as the severity warrants |

We will keep you updated if something takes longer, tell you plainly when we
consider a report out of scope, and credit you in the release notes when a fix
ships — unless you would rather stay anonymous. Please give us a chance to
release a fix before disclosing publicly.

## Scope

**In scope** — anything in this repository:

- the engine and CLI (`crates/`): credential handling and storage, TLS trust
  decisions, the WebDAV/OCS client, the SQLite state and the blob cache;
- the FUSE frontend: path handling, permission mapping, anything that lets one
  local user reach another's mount or cached data;
- the desktop integration (`integration/`, `wusel-desktop`): the D-Bus surfaces,
  the Nautilus extension, the GNOME Shell search provider;
- the packaging (`packaging/`): file ownership and modes, the systemd unit,
  install and removal scriptlets;
- the release pipeline in `.github/workflows/`.

**Out of scope:**

- vulnerabilities in Nextcloud Server itself — report those to
  <https://nextcloud.com/security/>;
- vulnerabilities in third-party dependencies, unless Wusel's use of them is what
  makes the issue exploitable (tell us anyway if you are unsure);
- the deliberate, documented escape hatches: `[tls] insecure = true` disables
  certificate verification and says so loudly, and `[auth] keyring = false` keeps
  the app password in a `0600` file. Both are opt-in by design.

Note also that the shipped systemd unit carries **no sandboxing directives**.
That is deliberate and documented: unprivileged FUSE mounting goes through the
setuid `fusermount3` helper, and every relevant hardening option implies
`NoNewPrivileges=yes`, which breaks the mount. It is not an oversight — but if
you see a way to harden the unit that keeps mounting working, we want to hear it.

## Supported versions

Wusel is pre-1.0 and ships as a rolling release: fixes land on `main` and go out
with the next tagged release. Only the latest release is supported — there are no
backports to earlier tags.
