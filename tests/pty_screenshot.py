#!/usr/bin/env python3
"""Drive `rail` inside a real pty and render what the terminal would show.

Feeds captured pty output through pyte (a VT100/xterm emulator) to get an
actual character/color grid, then rasterizes it with Pillow so it can be
viewed as an image instead of just inferred from raw ANSI codes.

Usage: .venv/bin/python3 tests/pty_screenshot.py <rail-binary> <out-dir>
"""
import fcntl
import glob
import json
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time

import pyte
from PIL import Image, ImageDraw, ImageFont

COLS, ROWS = 100, 30
CELL_W, CELL_H = 9, 18
FONT_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
DEFAULT_FG = (229, 229, 229)  # what a typical dark-theme terminal uses for "default" fg
DEFAULT_BG = (0, 0, 0)  # typical dark-theme terminal default bg


def hex_to_rgb(h):
    if h in ("default",):
        return None
    if len(h) == 6:
        return tuple(int(h[i : i + 2], 16) for i in (0, 2, 4))
    return None


def render_screen(screen, out_path):
    font = ImageFont.truetype(FONT_PATH, 14)
    img = Image.new("RGB", (COLS * CELL_W, ROWS * CELL_H), DEFAULT_BG)
    draw = ImageDraw.Draw(img)
    for y in range(ROWS):
        row = screen.buffer[y]
        for x in range(COLS):
            ch = row[x]
            fg = hex_to_rgb(ch.fg) or DEFAULT_FG
            bg = hex_to_rgb(ch.bg) or DEFAULT_BG
            if ch.reverse:
                fg, bg = bg, fg
            px, py = x * CELL_W, y * CELL_H
            if bg != DEFAULT_BG:
                draw.rectangle([px, py, px + CELL_W, py + CELL_H], fill=bg)
            if ch.data and ch.data != " ":
                draw.text((px, py), ch.data, font=font, fill=fg)
    img.save(out_path)
    print(f"wrote {out_path}")


class Harness:
    def __init__(self, rail_bin, work_dir, xdg_runtime_dir):
        self.work_dir = work_dir
        env = os.environ.copy()
        env["XDG_DATA_HOME"] = os.path.join(work_dir, "data")
        env["XDG_RUNTIME_DIR"] = xdg_runtime_dir
        env["CODEX_RAIL_CODEX"] = os.path.join(work_dir, "fake-codex")
        env["TERM"] = "xterm-256color"
        os.makedirs(xdg_runtime_dir, exist_ok=True)

        with open(env["CODEX_RAIL_CODEX"], "w") as f:
            f.write("#!/bin/sh\nexec cat\n")
        os.chmod(env["CODEX_RAIL_CODEX"], 0o755)

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.master = master
        self.proc = subprocess.Popen(
            [rail_bin],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            preexec_fn=os.setsid,
            close_fds=True,
        )
        os.close(slave)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)

    def send(self, data):
        os.write(self.master, data.encode() if isinstance(data, str) else data)

    def pump(self, timeout=1.0):
        end = time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], 0.2)
            if self.master in r:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.stream.feed(chunk)

    def close(self):
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


def main():
    rail_bin = sys.argv[1]
    out_dir = sys.argv[2]
    os.makedirs(out_dir, exist_ok=True)
    work_dir = os.path.join(out_dir, "work")
    os.makedirs(work_dir, exist_ok=True)
    xdg_runtime_dir = "/tmp/rail-screenshot-run"
    import shutil

    shutil.rmtree(xdg_runtime_dir, ignore_errors=True)

    h = Harness(rail_bin, work_dir, xdg_runtime_dir)
    h.pump(0.6)
    render_screen(h.screen, os.path.join(out_dir, "01_empty_manager.png"))

    h.send("demo-session\r")
    h.pump(1.2)
    render_screen(h.screen, os.path.join(out_dir, "02_after_attach.png"))

    h.send(bytes([0x1A]))  # Ctrl-Z detach
    h.pump(0.6)
    render_screen(h.screen, os.path.join(out_dir, "03_back_in_manager.png"))

    h.send(bytes([0x18]))
    h.pump(0.3)
    h.send(bytes([0x18]))
    h.pump(1.2)
    render_screen(h.screen, os.path.join(out_dir, "04_after_stop.png"))

    h.send(b"\x1b")
    h.pump(0.3)
    h.send(b"\x1b")
    h.pump(0.6)
    h.close()

    shutil.rmtree(xdg_runtime_dir, ignore_errors=True)
    print("manager exit code:", h.proc.returncode)


if __name__ == "__main__":
    main()
