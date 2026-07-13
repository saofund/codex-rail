#!/usr/bin/env python3
"""Regenerate README's deterministic Rail screenshot and short animated demo.

The capture drives the real release binary through the same PTY/pyte path as
the graphical regression suite. It starts no workers and uses only synthetic
state/rollout fixtures, so the published demo is reproducible and contains no
local paths, session ids, or conversation data.

    python3 tests/demo_capture.py [./target/release/rail] [--out docs]
"""

import argparse
import json
import os
import signal
import time

from PIL import Image

from cockpit import Cockpit, RAIL


def rollout(cockpit, sid, preview, active):
    """Create the minimal lifecycle + preview rollout used by the real UI."""
    path = os.path.join(cockpit.root, f"{sid}.jsonl")
    lifecycle = "task_started" if active else "task_complete"
    records = [
        {"type": "event_msg", "payload": {"type": lifecycle}},
        {"type": "event_msg", "payload": {
            "type": "agent_message", "message": preview}},
    ]
    with open(path, "w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")
    return path


def title_row(cockpit, title):
    return next(i for i, row in enumerate(cockpit.rows()) if title in row)


def move_pointer(cockpit, title):
    # SGR mouse coordinates are one-based. 35 means no-button pointer motion.
    row = title_row(cockpit, title)
    cockpit.key(f"\x1b[<35;18;{row + 1}M".encode(), 0.35)


def capture(rail, outdir):
    cockpit = Cockpit(rail, cols=100, rows=24).boot()
    try:
        now = int(time.time())
        cwd = "/work/codex-rail"
        fixtures = [
            ("demo-needs-1", "Review crash cleanup", "All owned process generations are clean.",
             "running", False, 301),
            ("demo-needs-2", "Import recent Codex chats", "7-day scan complete · /import 15d for more.",
             "running", False, 481),
            ("demo-work-1", "Run visual regression", "Driving real PTY mouse events and snapshots…",
             "running", True, 661),
            ("demo-work-2", "Verify atomic updater", "Checking release SHA, asset name, and ELF format…",
             "running", True, 841),
            ("demo-stop-1", "Fix hover highlight", "Hover paints only; keyboard selection stays put.",
             "exited", False, 1201),
            ("demo-stop-2", "Draft release notes", "Guardian, import, and visual checks are green.",
             "exited", False, 1801),
        ]
        for sid, title, preview, status, active, age in fixtures:
            path = rollout(cockpit, sid, preview, active)
            cockpit.seed(
                sid,
                title,
                status=status,
                cwd=cwd,
                codex_rollout_path=path,
                created_at=now - age,
                updated_at=now - age,
                last_output_at=now - age,
            )

        if not cockpit.wait_until(
                lambda: all(cockpit.row_with(title) for _, title, *_ in fixtures),
                timeout=8):
            raise RuntimeError("demo sessions did not render")

        frame_dir = os.path.join(cockpit.root, "demo-frames")
        frame_paths = []

        required_rows = [title for _, title, *_ in fixtures]

        def snap(name, extra_rows=()):
            path = os.path.join(frame_dir, f"{name}.png")
            markers = required_rows + list(extra_rows)
            deadline = time.time() + 3
            while time.time() < deadline:
                if not all(cockpit.row_with(marker) for marker in markers):
                    time.sleep(0.05)
                    continue
                # Rail repaints changed rows in one flush, but a PTY may split
                # that flush into chunks. Freeze the manager and re-check the
                # parsed screen before copying it so published media can never
                # contain a half-cleared frame.
                os.kill(cockpit.p.pid, signal.SIGSTOP)
                try:
                    time.sleep(0.08)
                    if all(cockpit.row_with(marker) for marker in markers):
                        cockpit.png(path)
                        break
                finally:
                    os.kill(cockpit.p.pid, signal.SIGCONT)
            else:
                raise RuntimeError(f"could not capture complete demo frame {name}")
            frame_paths.append(path)

        snap("01-overview")
        move_pointer(cockpit, "Run visual regression")
        snap("02-hover-working")
        move_pointer(cockpit, "Fix hover highlight")
        snap("03-hover-stopped")
        cockpit.key(b"\x1b[B", 0.35)
        snap("04-keyboard-selection")
        cockpit.type("/i", 0.45)
        snap("05-import-palette", ("/import",))

        os.makedirs(outdir, exist_ok=True)
        png_path = os.path.join(outdir, "rail-demo.png")
        gif_path = os.path.join(outdir, "rail-demo.gif")
        with Image.open(frame_paths[1]) as source:
            source.convert("RGB").save(png_path, optimize=True)

        frames = []
        for path in frame_paths:
            with Image.open(path) as source:
                frames.append(source.convert("P", palette=Image.Palette.ADAPTIVE,
                                             colors=128))
        frames[0].save(
            gif_path,
            save_all=True,
            append_images=frames[1:],
            duration=[1300, 1100, 1100, 1100, 1700],
            loop=0,
            optimize=True,
            disposal=2,
        )
        return png_path, gif_path
    finally:
        cockpit.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("rail", nargs="?", default=RAIL)
    parser.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "..", "docs"))
    args = parser.parse_args()
    png, gif = capture(os.path.abspath(args.rail), os.path.abspath(args.out))
    for path in (png, gif):
        print(f"{path}  {os.path.getsize(path)} bytes")


if __name__ == "__main__":
    main()
