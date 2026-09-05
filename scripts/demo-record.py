#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

"""Record a wusel demo as an asciicast v2 file.

This is NOT a screen-scraper of a live TTY. It runs a fixed sequence of REAL
commands against the REAL mount (set up by scripts/demo-nextcloud.sh, which
mounts against a real Nextcloud), captures their genuine output, and lays it
out as an asciicast with a calm, chosen typing cadence. The bytes on screen are
the mount's own; only the pacing is authored, so the result is honest but not
hostage to a runner's timing or a recorder's TTY quirks.

Output format: https://docs.asciinema.org/manual/asciicast/v2/ — a JSON header
line followed by ``[time, "o", data]`` event lines. Nothing here needs a
terminal, so it produces the same cast headless in CI as on a desktop.

Env:
  DEMO_HOME   home whose ``~/Wusel`` is the mount (default: $HOME)
  DEMO_MOUNT  mount directory name under home (default: Wusel)
  DEMO_CAST   output path (default: wusel-demo.cast)
"""

import json
import os
import subprocess
import sys

HOME = os.environ.get("DEMO_HOME", os.environ.get("HOME", "/root"))
MOUNT = os.environ.get("DEMO_MOUNT", "Wusel")
CAST = os.environ.get("DEMO_CAST", "wusel-demo.cast")

COLS, ROWS = 90, 28

# A friendly, generic prompt — no real hostname, nothing to redact later.
PROMPT = "\x1b[1;32myou@laptop\x1b[0m:\x1b[1;34m~\x1b[0m$ "

# Pacing (seconds). Deliberately unhurried: this plays in docs, where a reader
# needs time to read the command before its output scrolls in.
CHAR = 0.045          # per typed character
AFTER_ENTER = 0.35    # beat between Enter and the command's output
AFTER_OUTPUT = 1.8    # beat to read the output before the next command
LEAD_COMMENT = 1.1    # beat after a narration comment line


# A step is either a spoken comment (shown as a shell # line) or a command.
# `show` is what the viewer sees typed; `run` is what actually executes (kept
# identical here — the point is that these are the real commands).
def comment(text):
    return {"kind": "comment", "text": text}


def command(show, run=None):
    return {"kind": "command", "show": show, "run": run or show}


STEPS = [
    comment("Wusel mounts your whole Nextcloud as a normal folder."),
    comment("Nothing is downloaded until you actually open a file."),
    command(f"ls -lh ~/{MOUNT}"),
    comment("The real storage quota from the server -- not a fake petabyte."),
    command(f"df -h ~/{MOUNT}"),
    comment("Open a file: it is fetched on demand, transparently."),
    command(f"cat ~/{MOUNT}/notes/welcome.txt"),
    comment("Keep a folder offline for good (downloads it now):"),
    command(f"wusel pin Documents"),
    command("wusel pins"),
    comment("And see what the mount is doing right now, by file name:"),
    command("wusel status"),
]


class Cast:
    """Accumulate asciicast events with a running clock."""

    def __init__(self):
        self.t = 0.0
        self.events = []

    def out(self, data, dt=0.0):
        self.t += dt
        self.events.append([round(self.t, 3), "o", data])

    def type_line(self, text):
        """Emit `text` one character at a time, then a newline."""
        for ch in text:
            self.out(ch, CHAR)
        self.out("\r\n", AFTER_ENTER)


def to_crlf(s):
    # A terminal wants CRLF; captured command output has bare LFs.
    return s.replace("\r\n", "\n").replace("\n", "\r\n")


def run_capture(cmd):
    """Run `cmd` under bash so ~ and PATH resolve, capture combined output."""
    env = dict(os.environ, HOME=HOME)
    proc = subprocess.run(
        ["bash", "-lc", cmd],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return proc.stdout


def main():
    cast = Cast()
    # A brief still frame so the player does not open mid-keystroke.
    cast.out(PROMPT, 0.6)

    for step in STEPS:
        if step["kind"] == "comment":
            cast.type_line(f"\x1b[2m# {step['text']}\x1b[0m")
            cast.out(PROMPT, LEAD_COMMENT)
            continue

        cast.type_line(step["show"])
        output = to_crlf(run_capture(step["run"]))
        if output and not output.endswith("\r\n"):
            output += "\r\n"
        cast.out(output, 0.15)
        cast.out(PROMPT, AFTER_OUTPUT)

    # Linger on the final prompt.
    cast.out("", 1.5)

    header = {
        "version": 2,
        "width": COLS,
        "height": ROWS,
        "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"},
        "title": "Wusel -- your Nextcloud as a normal folder",
    }
    with open(CAST, "w", encoding="utf-8") as fh:
        fh.write(json.dumps(header) + "\n")
        for ev in cast.events:
            fh.write(json.dumps(ev, ensure_ascii=False) + "\n")

    print(f">> wrote {CAST} ({len(cast.events)} events, {cast.t:.1f}s)", file=sys.stderr)


if __name__ == "__main__":
    main()
