#!/usr/bin/env python3
"""Rail Cockpit — a comprehensive, time-aware, *visual* test harness.

It drives the REAL `rail` binary in a pty, reads the REAL rendered screen (pyte),
can render that screen to a PNG so a human (or I) can actually SEE it — colours,
CJK alignment, layout — operate every feature with arbitrary input at any time,
and sample the screen over time to catch things a single snapshot can't (a
flickering age column, a status that never latches, a repaint that leaves junk).

    python3 tests/cockpit.py [./target/release/rail] [--png OUTDIR]
    python3 tests/cockpit.py [./target/release/rail] --import-only
    python3 tests/cockpit.py [./target/release/rail] --mouse-only [--png OUTDIR]

With no flags it runs the full feature audit and prints a PASS/FAIL table. The
Cockpit class is also importable for ad-hoc driving:

    c = Cockpit(RAIL); c.boot(); c.new("hello"); c.png("after_new.png"); c.quit()
"""
import fcntl
import copy
import glob
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import pyte

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
RAIL = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("-") \
       else os.path.join(REPO, "target/release/rail")
FAKE_STREAM = os.path.join(HERE, "fakecodex")            # streams "tick" forever (a live turn)
FAKE_SLEEP = os.path.join(HERE, "fakecodex_sleep")       # quiet long-lived child
FAKE_DETACHED = os.path.join(HERE, "fakecodex_spawn_detached")
FAKE_PROMPT_TUI = os.path.join(HERE, "fakecodex_prompt_tui")
FAKE_STARTUP_ERROR = os.path.join(HERE, "fakecodex_startup_error")  # prints a version error, exits 1

COLS, ROWS = 110, 40

# ---- pyte colour -> RGB, for PNG snapshots ------------------------------------
_NAMED = {
    "black": (0, 0, 0), "red": (205, 60, 60), "green": (122, 208, 142),
    "brown": (236, 188, 92), "yellow": (236, 188, 92), "blue": (90, 140, 214),
    "magenta": (200, 120, 200), "cyan": (100, 200, 210), "white": (230, 230, 238),
}
def _rgb(color, default):
    if color in (None, "default"):
        return default
    if isinstance(color, str) and len(color) == 6:
        try:
            return (int(color[0:2], 16), int(color[2:4], 16), int(color[4:6], 16))
        except ValueError:
            return default
    return _NAMED.get(color, default)


class Cockpit:
    def __init__(self, rail=RAIL, root=None, codex=FAKE_SLEEP, cols=COLS, rows=ROWS):
        self.rail = os.path.abspath(rail)      # workers spawn with cwd=/tmp; must be absolute
        # Root MUST be short: the worker binds $XDG_RUNTIME_DIR/codex-rail/<id>.sock
        # and a Unix socket path over ~108 bytes (SUN_LEN) fails to bind. A long
        # root (e.g. under the job tmp dir) makes every worker fail with
        # "path must be shorter than SUN_LEN" — which reads as a fake rail bug.
        # PID alone is not unique across concurrently-run container namespaces
        # (both processes may be PID 2 while sharing /tmp). Reserve a genuinely
        # unique, still-short path so parallel visual/import audits cannot delete
        # each other's fixtures during boot/close.
        self.root = root or tempfile.mkdtemp(prefix="rc-", dir="/tmp")
        self.codex = codex
        self.cols = cols
        self.nrows = rows
        self.data = self.root + "/data"
        self.run = self.root + "/run"
        self.home = self.root + "/home"
        self.config = self.home + "/.config"
        self.codex_home = self.home + "/.codex"
        self.jobs = self.data + "/codex-rail/jobs"
        self.p = None
        self.m = None
        self.screen = pyte.Screen(self.cols, self.nrows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self._zombies = []
        self._guardians = []

    # ---- lifecycle -----------------------------------------------------------
    def boot(self, settle=1.8, reset=True):
        if reset:
            shutil.rmtree(self.root, ignore_errors=True)
        for d in (self.jobs, self.run, self.home, self.config, self.codex_home):
            os.makedirs(d, exist_ok=True)
        env = os.environ.copy()
        # The cockpit is explicitly a graphical regression harness. Its parent
        # may set NO_COLOR for non-interactive command output, but inheriting it
        # would silently turn every colour/background assertion into a no-op.
        env.pop("NO_COLOR", None)
        env.update({"XDG_DATA_HOME": self.data, "XDG_RUNTIME_DIR": self.run,
                    "XDG_CONFIG_HOME": self.config, "CODEX_HOME": self.codex_home,
                    "HOME": self.home,
                    "CODEX_RAIL_CODEX": self.codex, "TERM": "xterm-256color",
                    "COLORTERM": "truecolor",
                    # 4s detach-hint progress bar -> fast in tests; raise it to watch it fill
                    "CODEX_RAIL_HINT_MS": os.environ.get("COCKPIT_HINT_MS", "60"),
                    # rescan ~/.codex for cwd-matching sessions fast so tests don't wait 20s
                    "CODEX_RAIL_ADOPT_MS": "300",
                    # don't hit GitHub for an update check during tests
                    "CODEX_RAIL_NO_UPDATE_CHECK": "1",
                    "COLUMNS": str(self.cols), "LINES": str(self.nrows)})
        self.m, s = pty.openpty()
        fcntl.ioctl(s, termios.TIOCSWINSZ,
                    struct.pack("HHHH", self.nrows, self.cols, 0, 0))
        self.p = subprocess.Popen([self.rail], stdin=s, stdout=s, stderr=s, env=env,
                                  preexec_fn=os.setsid, close_fds=True)
        os.close(s)
        threading.Thread(target=self._drain, daemon=True).start()
        time.sleep(settle)
        return self

    def _drain(self):
        while not self.stopped.is_set():
            r, _, _ = select.select([self.m], [], [], 0.1)
            if self.m in r:
                try:
                    chunk = os.read(self.m, 65536)
                except OSError:
                    return
                with self.lock:
                    self.raw += chunk
                    self.stream.feed(chunk)

    def close(self):
        self.stopped.set()
        for grp in (self.p,):
            try:
                os.killpg(os.getpgid(grp.pid), signal.SIGKILL)
            except Exception:
                pass
        # reap any zombies we forked, and SIGKILL any workers we spawned
        self._kill_workers()
        for z in self._zombies:
            try:
                os.waitpid(z, 0)
            except Exception:
                pass
        for guardian in self._guardians:
            try:
                guardian.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.kill(guardian.pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
        shutil.rmtree(self.root, ignore_errors=True)

    def cpu_ticks(self):
        try:
            f = open(f"/proc/{self.p.pid}/stat").read().split()
            return int(f[13]) + int(f[14])          # utime+stime, USER_HZ (100/s)
        except Exception:
            return None

    def simulate_terminal_death(self):
        """Close the pty master — as if the SSH session dropped or the window closed."""
        self.stopped.set()
        time.sleep(0.15)
        try:
            os.close(self.m)
        except Exception:
            pass

    def exited_within(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            if self.p.poll() is not None:
                return True
            time.sleep(0.1)
        return False

    def _kill_workers(self):
        if not os.path.isdir(self.jobs):
            return
        for d in os.listdir(self.jobs):
            try:
                st = json.load(open(f"{self.jobs}/{d}/state.json"))
                for k in ("child_pid", "worker_pid"):
                    if st.get(k):
                        try:
                            os.kill(st[k], signal.SIGKILL)
                        except Exception:
                            pass
            except Exception:
                pass

    # ---- input (arbitrary, any time) ----------------------------------------
    def key(self, payload, settle=0.35):
        os.write(self.m, payload)
        time.sleep(settle)

    def resize(self, cols, rows, settle=0.6):
        """Resize the real PTY and pyte's model together."""
        self.cols = cols
        self.nrows = rows
        with self.lock:
            self.screen.resize(lines=rows, columns=cols)
        fcntl.ioctl(self.m, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.p.pid, signal.SIGWINCH)
        time.sleep(settle)

    def type(self, text, settle=0.3):
        self.key(text.encode(), settle)

    # ---- screen reading ------------------------------------------------------
    def rows(self):
        with self.lock:
            return [r.rstrip() for r in self.screen.display]

    def text(self):
        return "\n".join(self.rows())

    def row_with(self, needle):
        return next((r for r in self.rows() if needle in r), None)

    def row_has_background(self, needle):
        """Whether any cell on the matching rendered row has a non-default bg."""
        with self.lock:
            display = list(self.screen.display)
            y = next((i for i, row in enumerate(display) if needle in row), None)
            if y is None:
                return False
            return any(self.screen.buffer[y][x].bg not in (None, "default")
                       for x in range(self.cols))

    def selected_row(self):
        return next((r.strip() for r in self.rows() if r.lstrip().startswith("▌")), None)

    def section_counts(self):
        out = {}
        for r in self.rows():
            for name in ("Needs input", "Working", "Stopped"):
                if name in r:
                    for tok in r.replace("✱", " ").replace("●", " ").replace("○", " ").split():
                        if tok.isdigit():
                            out[name] = int(tok)
                            break
        return out

    def age_of(self, title):
        """The age cell (e.g. '42m','3s') shown on the row whose title contains `title`."""
        row = self.row_with(title)
        if not row:
            return None
        toks = row.split()
        for tok in reversed(toks):
            if tok and tok[0].isdigit() and tok[-1] in "smhd":
                return tok
        return None

    def mark(self):
        with self.lock:
            return len(self.raw)

    def raw_since(self, m):
        with self.lock:
            return bytes(self.raw[m:])

    # ---- time-awareness ------------------------------------------------------
    def sample(self, fn, seconds, hz=5):
        """Sample fn() `hz` times/sec for `seconds`; return the list of samples."""
        out = []
        n = int(seconds * hz)
        for _ in range(max(1, n)):
            out.append(fn())
            time.sleep(1.0 / hz)
        return out

    def wait_until(self, pred, timeout=15, poll=0.2):
        end = time.time() + timeout
        while time.time() < end:
            if pred():
                return True
            time.sleep(poll)
        return False

    # ---- PNG snapshot (so the UI can actually be SEEN) ------------------------
    def png(self, path, scale=1):
        from PIL import Image, ImageDraw, ImageFont
        # A render is emitted as several PTY chunks. Snapshotting between them
        # captures a valid pyte state but not a complete Rail frame, so wait for
        # a short quiet gap first (well below Rail's 700ms refresh interval).
        deadline = time.time() + 1.0
        stable_since = time.time()
        with self.lock:
            last_size = len(self.raw)
        while time.time() < deadline:
            time.sleep(0.02)
            with self.lock:
                size = len(self.raw)
            if size != last_size:
                last_size = size
                stable_since = time.time()
            elif time.time() - stable_since >= 0.12:
                break
        cw, ch = 9, 19
        mono = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 15)
        try:
            cjk = ImageFont.truetype("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 16)
        except Exception:
            cjk = mono
        bg0 = (24, 22, 26)
        img = Image.new("RGB", (self.cols * cw, self.nrows * ch), bg0)
        d = ImageDraw.Draw(img)
        with self.lock:
            # The drain thread keeps mutating pyte's buffer. Holding only a
            # reference here can produce a torn PNG (half old frame, half new)
            # after the lock is released, which defeats visual regression.
            buf = copy.deepcopy(self.screen.buffer)
        for y in range(self.nrows):
            x = 0
            while x < self.cols:
                cell = buf[y][x]
                chdata = cell.data or " "
                fg = _rgb(cell.fg, (210, 210, 218))
                bg = _rgb(cell.bg, bg0)
                if cell.reverse:
                    fg, bg = bg, fg
                is_cjk = any(ord(c) >= 0x2E80 for c in chdata)
                w = 2 if is_cjk else 1
                if bg != bg0:
                    d.rectangle([x * cw, y * ch, (x + w) * cw, (y + 1) * ch], fill=bg)
                if chdata.strip():
                    d.text((x * cw, y * ch), chdata, font=(cjk if is_cjk else mono), fill=fg)
                x += w
        if scale != 1:
            img = img.resize((img.width * scale, img.height * scale), Image.NEAREST)
        os.makedirs(os.path.dirname(path), exist_ok=True) if os.path.dirname(path) else None
        img.save(path)
        return path

    # ---- session seeding (deterministic states) ------------------------------
    def _base(self, sid, title, **over):
        now = int(time.time())
        st = {"id": sid, "title": title, "cwd": "/tmp", "codex": self.codex, "status": "running",
              "worker_pid": None, "child_pid": None,
              "socket": f"{self.run}/codex-rail/{sid}.sock",
              "created_at": now, "updated_at": now, "exit_code": None, "last_error": None,
              "codex_session_id": None, "codex_rollout_path": None, "initial_prompt": None,
              "title_pinned": False, "last_output_at": 0}
        st.update(over)
        return st

    def seed(self, sid, title, **over):
        os.makedirs(f"{self.jobs}/{sid}", exist_ok=True)
        json.dump(self._base(sid, title, **over), open(f"{self.jobs}/{sid}/state.json", "w"), indent=2)
        json.dump({"title": title, "title_pinned": over.get("title_pinned", False)},
                  open(f"{self.jobs}/{sid}/label.json", "w"))

    def seed_running_worker(self, sid, title, codex=None, **over):
        """Spawn the production guardian -> worker chain with a fake Codex."""
        codex = codex or FAKE_STREAM
        self.seed(sid, title, status="starting", codex=codex, **over)
        self.start_guardian(sid, codex)
        self.wait_until(lambda: self._pid(sid, "child_pid") is not None, timeout=8)
        return sid

    def start_guardian(self, sid, codex=None):
        """Start a guardian for an already-persisted session (resume/crash tests)."""
        codex = codex or self.codex
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": self.data, "XDG_RUNTIME_DIR": self.run,
                    "XDG_CONFIG_HOME": self.config, "CODEX_HOME": self.codex_home,
                    "HOME": self.home,
                    "CODEX_RAIL_CODEX": codex})
        guardian = subprocess.Popen([self.rail, "--guardian", sid], cwd="/tmp", env=env,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                    start_new_session=True)
        self._guardians.append(guardian)
        return guardian

    def seed_codex_history(self, n_sessions=2, msgs_each=4):
        """Write synthetic codex rollouts under HOME/.codex so distillation has
        real user messages to aggregate (event_msg.user_message lines)."""
        voice = ["直接开始，别问我，pls just do it, thx",
                 "这个不对，重新想想，要通用的方案别搞一次性的",
                 "先 push 再改，我出门了你自己回归测试全程 yes",
                 "很好，但要测它真的 work，别只说 it should work"]
        total = 0
        for si in range(n_sessions):
            day = f"{(si % 28) + 1:02d}"
            d = os.path.join(self.home, ".codex", "sessions", "2026", "07", day)
            os.makedirs(d, exist_ok=True)
            sid = f"000000{si}-aaaa-bbbb-cccc-0000000000{si}"
            path = os.path.join(d, f"rollout-2026-07-{day}T12-0{si}-00-{sid}.jsonl")
            lines = [json.dumps({"type": "session_meta",
                                 "payload": {"id": sid, "session_id": sid, "cwd": "/tmp"}})]
            for mi in range(msgs_each):
                lines.append(json.dumps({"type": "event_msg", "payload": {
                    "type": "user_message", "message": voice[(si + mi) % len(voice)] + f" (s{si}m{mi})"}}))
                total += 1
            open(path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
        return total

    def seed_import_rollout(self, sid, message, *, cwd=None, age_days=0):
        """Create one import candidate with a controlled cwd, user turn and mtime.

        `message=None` deliberately creates a transcript with no genuine user
        turn. A marker-like message can be supplied to exercise the production
        marker filter. Import freshness is based on filesystem mtime, not the
        date encoded in the fixture filename.
        """
        cwd = os.getcwd() if cwd is None else cwd
        d = os.path.join(self.codex_home, "sessions", "2099", "01", "01")
        os.makedirs(d, exist_ok=True)
        path = os.path.join(d, f"rollout-2099-01-01T00-00-00-{sid}.jsonl")
        lines = [json.dumps({"type": "session_meta", "payload": {
            "id": sid, "session_id": sid, "cwd": cwd,
            "timestamp": "2099-01-01T00:00:00Z"}})]
        if message is not None:
            lines.append(json.dumps({"type": "event_msg", "payload": {
                "type": "user_message", "message": message}}))
        with open(path, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
        mtime = time.time() - age_days * 24 * 60 * 60
        os.utime(path, (mtime, mtime))
        return path

    def seed_zombie(self, sid, title):
        z = os.fork()
        if z == 0:
            os._exit(0)
        self._zombies.append(z)
        time.sleep(0.15)
        self.seed(sid, title, status="running", worker_pid=z, child_pid=99999999)
        return z

    def _pid(self, sid, key):
        try:
            return json.load(open(f"{self.jobs}/{sid}/state.json")).get(key)
        except Exception:
            return None

    def alive(self, sid, key="child_pid"):
        p = self._pid(sid, key)
        if not p:
            return False
        try:
            os.kill(int(p), 0)
            return True
        except Exception:
            return False

    def dir_exists(self, sid):
        return os.path.exists(f"{self.jobs}/{sid}/state.json")

    # ---- named feature actions ----------------------------------------------
    def down(self):
        self.key(b"\x1b[B")

    def up(self):
        self.key(b"\x1b[A")

    def goto(self, title, tries=30):
        for _ in range(tries):
            sel = self.selected_row() or ""
            if title in sel:
                return True
            self.key(b"\x1b[B", 0.12)
        return False

    def ctrl_x_twice(self):
        self.key(b"\x18", 0.4)
        self.key(b"\x18", 0.8)

    def rename(self, newname):
        self.key(b"\x12", 0.4)                 # Ctrl-R
        self.key(b"\x7f" * 40, 0.2)            # clear
        self.type(newname, 0.2)
        self.key(b"\r", 0.6)

    def new(self, msg):
        if msg:
            self.type(msg, 0.3)                 # printable key enters composer verbatim
        else:
            self.key(b"\x0e", 0.4)             # Ctrl+N creates a blank composer
        self.key(b"\r", 2.0)


def visual_audit(rail, pngdir):
    """Sandbox-safe manager rendering audit: no worker, socket, or child process."""
    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f"  — {detail}" if detail else ""))

    def snap(c, tag):
        if pngdir:
            c.png(os.path.join(pngdir, f"{tag}.png"))

    print(f"\n==== RAIL VISUAL-ONLY AUDIT ({rail}) ====")
    c = Cockpit(rail, cols=20, rows=8).boot()
    try:
        tiny_20 = "terminal too small" in c.text().lower() and c.p.poll() is None
        snap(c, "visual_20x8")
        check("20x8: graceful too-small frame", tiny_20, repr(c.text()))

        c.resize(40, 12)
        tiny_40 = "terminal too small" in c.text().lower() and c.p.poll() is None
        snap(c, "visual_40x12")
        check("40x12: graceful too-small frame", tiny_40, repr(c.text()))

        c.resize(110, 40)
        mark = c.mark()
        title_osc = b"\x1b]0;TITLE-ATTACK\x07"
        preview_osc = b"\x1b]52;c;PREVIEW-ATTACK\x07"
        rollout = os.path.join(c.codex_home, "sessions", "2026", "07", "13",
                               "rollout-2026-07-13T00-00-00-visual.jsonl")
        os.makedirs(os.path.dirname(rollout), exist_ok=True)
        with open(rollout, "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": "visual", "cwd": "/tmp"}}) + "\n")
            f.write(json.dumps({"type": "event_msg", "payload": {
                "type": "agent_message",
                "message": "PREVIEW e\u0301 ❤️ 👩‍💻 🇨🇳 中文 "
                           + preview_osc.decode("ascii")}}) + "\n")
        unicode_titles = ["U1 e\u0301", "U2 ❤️", "U3 👩‍💻", "U4 🇨🇳", "U5 中文"]
        for index, title in enumerate(unicode_titles):
            c.seed(f"visual-unicode-{index}",
                   title + (" " + title_osc.decode("ascii") if index == 4 else ""),
                   status="exited",
                   codex_rollout_path=(rollout if index == 4 else None),
                   codex_session_id=("visual" if index == 4 else None),
                   title_pinned=True)
        rendered = c.wait_until(
            lambda: all(c.row_with(f"U{index}") is not None for index in range(1, 6))
                    and "中文" in c.text() and "PREVIEW" in c.text(),
            timeout=8)
        raw = c.raw_since(mark)
        sanitized = title_osc not in raw and preview_osc not in raw
        healthy = c.p.poll() is None and any("Codex Rail" in row for row in c.rows())
        snap(c, "visual_110x40_unicode")
        check("resize 110x40: Unicode/CJK clusters render and manager remains healthy",
              rendered and healthy, f"rendered={rendered} healthy={healthy}")
        check("terminal safety: hostile title/preview controls never reach raw PTY",
              sanitized, f"title_leaked={title_osc in raw} preview_leaked={preview_osc in raw}")
    finally:
        c.close()

    passed = sum(ok for _, ok, _ in results)
    print(f"\n==== {passed}/{len(results)} visual-only checks PASS ====")
    if pngdir:
        print(f"PNG snapshots written to {pngdir}/")
    return all(ok for _, ok, _ in results)


def _run_import_contract(rail, check, snap):
    """Black-box PTY checks shared by `--import-only` and the full audit."""

    # The empty-state onboarding must explain both ways to reach beyond the
    # conservative seven-day automatic window. Check the rendered screen rather
    # than implementation constants so this guards the actual discoverability.
    c = Cockpit(rail).boot(settle=0.3)
    try:
        hinted = c.wait_until(
            lambda: "/import 15d" in c.text() and "/import <session_id>" in c.text(),
            timeout=5)
        check("import: startup hint advertises 15d and exact-session expansion",
              hinted, repr(c.text()))
        snap(c, "import_01_startup_hint")
    finally:
        c.close()

    # Seed all candidates BEFORE boot: this is specifically the initial automatic
    # import contract, not a later incremental rescan. Filename dates are fixed;
    # production must use rollout mtime for the seven-day boundary.
    c = Cockpit(rail)
    cwd = os.getcwd()
    old_start_ms = int((time.time() - 90 * 24 * 60 * 60) * 1000)

    def import_sid(serial):
        # Real Codex ids are UUIDv7: their first 48 bits encode start millis.
        # Keep fixture creation older than every tested mtime so the UI age cell
        # reflects that mtime instead of max(mtime, UUID start time).
        stamp = f"{old_start_ms + serial * (1 << 16):012x}"
        return f"{stamp[:8]}-{stamp[8:]}-7000-8000-{serial:012x}"

    recent_id = import_sid(1)
    old_15d_id = import_sid(2)
    exact_old_id = import_sid(3)
    dismissed_id = import_sid(4)
    other_cwd_id = import_sid(5)
    no_turn_id = import_sid(6)
    marker_only_id = import_sid(7)
    exact_other_id = import_sid(8)
    missing_id = import_sid(9)
    ambiguous_id = import_sid(10)

    c.seed_import_rollout(recent_id, "IMPORT_RECENT genuine turn", age_days=6)
    c.seed_import_rollout(old_15d_id, "IMPORT_OLD_TEN_DAYS", age_days=10)
    c.seed_import_rollout(exact_old_id, "IMPORT_EXACT_OLD", age_days=30)
    c.seed_import_rollout(dismissed_id, "IMPORT_DISMISSED_OLD", age_days=40)
    c.seed_import_rollout(other_cwd_id, "IMPORT_OTHER_CWD", cwd="/tmp", age_days=1)
    c.seed_import_rollout(no_turn_id, None, age_days=1)
    c.seed_import_rollout(marker_only_id, "<EXTERNAL SESSION IMPORTED>", age_days=1)
    c.seed_import_rollout(exact_other_id, "IMPORT_EXACT_OTHER_CWD", cwd="/tmp", age_days=30)
    ambiguous_path = c.seed_import_rollout(
        ambiguous_id, "IMPORT_AMBIGUOUS", age_days=30)
    ambiguous_copy = ambiguous_path.replace("/2099/01/01/", "/2099/01/02/")
    os.makedirs(os.path.dirname(ambiguous_copy), exist_ok=True)
    shutil.copy2(ambiguous_path, ambiguous_copy)

    dismiss_file = os.path.join(c.data, "codex-rail", ".adopt_dismissed")
    os.makedirs(os.path.dirname(dismiss_file), exist_ok=True)
    with open(dismiss_file, "w", encoding="utf-8") as f:
        f.write(dismissed_id + "\n")

    c.boot(reset=False, settle=0.3)
    try:
        recent = c.wait_until(lambda: c.row_with("IMPORT_RECENT") is not None, timeout=8)
        # Let at least one fast rescan complete before asserting every exclusion.
        time.sleep(1.0)
        excluded = {
            "older_than_7d": c.row_with("IMPORT_OLD_TEN_DAYS") is None,
            "exact_old": c.row_with("IMPORT_EXACT_OLD") is None,
            "dismissed": c.row_with("IMPORT_DISMISSED_OLD") is None,
            "other_cwd": c.row_with("IMPORT_OTHER_CWD") is None,
            "no_user_turn": all(no_turn_id[:8] not in row for row in c.rows()),
            "marker_only": all(marker_only_id[:8] not in row for row in c.rows()),
        }
        check("import: automatic scan is current-cwd + genuine-user + last-7d only",
              recent and all(excluded.values()),
              f"recent={recent} excluded={excluded}")
        snap(c, "import_02_default_7d")

        c.type("/import 15d", 0.2)
        c.key(b"\r", 0.4)
        expanded = c.wait_until(lambda: c.row_with("IMPORT_OLD_TEN_DAYS") is not None,
                                timeout=8)
        still_bounded = (c.row_with("IMPORT_EXACT_OLD") is None
                         and c.row_with("IMPORT_AMBIGUOUS") is None)
        check("import: /import 15d expands the mtime window",
              expanded and still_bounded,
              f"row={c.row_with('IMPORT_OLD_TEN_DAYS')!r} bounded={still_bounded}")

        # Repeating a range import is idempotent: it must not duplicate rows or
        # create a Rail-managed job for an in-memory adopted transcript.
        c.type("/import 15d", 0.2)
        c.key(b"\r", 0.4)
        repeat_finished = c.wait_until(
            lambda: "imported 0 additional chat(s)" in c.text(), timeout=8)
        repeated = (repeat_finished
                    and sum("IMPORT_OLD_TEN_DAYS" in row for row in c.rows()) == 1)
        check("import: repeating a day-window import is idempotent", repeated,
              f"finished={repeat_finished} "
              f"matching_rows={sum('IMPORT_OLD_TEN_DAYS' in row for row in c.rows())}")

        c.type(f"/import {exact_old_id}", 0.2)
        c.key(b"\r", 0.4)
        exact = c.wait_until(lambda: c.row_with("IMPORT_EXACT_OLD") is not None, timeout=8)
        check("import: exact session id bypasses the age window",
              exact, f"row={c.row_with('IMPORT_EXACT_OLD')!r}")

        c.type(f"/import {dismissed_id}", 0.2)
        c.key(b"\r", 0.4)
        restored = c.wait_until(lambda: c.row_with("IMPORT_DISMISSED_OLD") is not None,
                                timeout=8)
        dismissed_after = (open(dismiss_file, encoding="utf-8").read()
                            if os.path.exists(dismiss_file) else "")
        undismissed = dismissed_id not in dismissed_after.splitlines()
        check("import: exact id restores a previously dismissed session",
              restored and undismissed,
              f"restored={restored} undismissed={undismissed}")

        jobs_before = set(os.listdir(c.jobs)) if os.path.isdir(c.jobs) else set()
        c.type(f"/import {exact_other_id}", 0.2)
        c.key(b"\r", 0.8)
        exact_other_absent = c.row_with("IMPORT_EXACT_OTHER_CWD") is None
        jobs_after_other = set(os.listdir(c.jobs)) if os.path.isdir(c.jobs) else set()
        check("import: exact id still refuses a different cwd",
              exact_other_absent and jobs_after_other == jobs_before and c.p.poll() is None,
              f"absent={exact_other_absent} jobs_before={jobs_before} "
              f"jobs_after={jobs_after_other} status={c.rows()[-1]!r}")

        c.type(f"/import {missing_id}", 0.2)
        c.key(b"\r", 0.4)
        missing_reported = c.wait_until(lambda: "was not found" in c.text(), timeout=5)
        check("import: a missing exact id reports not-found without creating a job",
              missing_reported and set(os.listdir(c.jobs)) == jobs_before,
              f"reported={missing_reported} status={c.rows()[-1]!r}")

        c.type(f"/import {ambiguous_id}", 0.2)
        c.key(b"\r", 0.4)
        ambiguous_reported = c.wait_until(lambda: "is ambiguous" in c.text(), timeout=5)
        check("import: duplicate rollout identity fails closed",
              ambiguous_reported and c.row_with("IMPORT_AMBIGUOUS") is None
              and set(os.listdir(c.jobs)) == jobs_before,
              f"reported={ambiguous_reported} status={c.rows()[-1]!r}")

        # Invalid forms must be consumed by Rail and report usage. If any one is
        # accidentally treated as a Codex prompt it creates a job and leaves the
        # manager screen, both of which are asserted against here.
        bad_usage = []
        invalid_commands = ("/import", "/import 0d", "/import 15", "/import 15days",
                            "/import 15d extra", "/import 3651d")
        for command in invalid_commands:
            mark = c.mark()
            c.type(command, 0.15)
            c.key(b"\r", 0.6)
            wire = c.raw_since(mark).lower()
            bad_usage.append(b"usage" in wire and b"/import" in wire)
            if c.p.poll() is not None or "Codex Rail" not in c.text():
                break
        jobs_after_bad = set(os.listdir(c.jobs)) if os.path.isdir(c.jobs) else set()
        check("import: invalid arguments show rail-side usage and create no session",
              len(bad_usage) == len(invalid_commands) and all(bad_usage)
              and jobs_after_bad == jobs_before,
              f"usage={bad_usage} jobs_before={jobs_before} jobs_after={jobs_after_bad}")

        c.type("/review", 0.25)
        review_passthrough = ("Enter start" in c.text()
                              and "/review" in "\n".join(c.rows()[-7:])
                              and "/distill" not in c.text())
        check("slash: unknown /review remains ordinary composer input",
              review_passthrough, repr("\n".join(c.rows()[-8:])))
        c.key(b"\x1b", 0.2)
        snap(c, "import_03_manual")
    finally:
        c.close()

    # Prefix filtering is a separate regression because `/di Enter` once fell
    # through to ordinary prompt submission even while `/distill` was highlighted.
    c = Cockpit(rail).boot()
    try:
        c.seed_codex_history(n_sessions=1, msgs_each=2)
        c.key(b"/", 0.35)
        palette = all(cmd in c.text()
                      for cmd in ("/distill", "/import", "/update", "/config", "/help"))
        c.key(b"\x1b", 0.15)
        c.type("/di", 0.25)
        filtered = "/distill" in c.text() and "/update" not in c.text()
        snap(c, "import_04_distill_prefix")
        c.key(b"\r", 2.5)
        corpora = glob.glob(os.path.join(c.home, ".config", "codex-rail", "distill",
                                        "runs", "run-*", "corpus"))
        ran = any(os.path.isdir(corpus)
                  and glob.glob(os.path.join(corpus, "corpus-*.md")) for corpus in corpora)
        check("slash: /di Enter executes highlighted /distill rail-side",
              palette and filtered and ran,
              f"palette={palette} filtered={filtered} ran={ran}")
    finally:
        c.close()


def import_audit(rail, pngdir=None):
    """Focused import/slash PTY regression, runnable with `--import-only`."""
    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f"  — {detail}" if detail else ""))

    def snap(c, tag):
        if pngdir:
            c.png(os.path.join(pngdir, f"{tag}.png"))

    print(f"\n==== RAIL IMPORT AUDIT ({rail}) ====")
    _run_import_contract(rail, check, snap)
    passed = sum(ok for _, ok, _ in results)
    print(f"\n==== {passed}/{len(results)} import checks PASS ====")
    for name, ok, _ in results:
        if not ok:
            print(f"   FAILED: {name}")
    if pngdir:
        print(f"PNG snapshots written to {pngdir}/")
    return all(ok for _, ok, _ in results)


def _run_mouse_contract(rail, check, snap):
    """Hover paints the physical row without stealing durable selection."""
    c = Cockpit(rail, rows=18).boot()
    try:
        for i in range(16):
            c.seed(f"ms{i}", f"mouse-sess-{i}", status="exited")
        loaded = c.wait_until(
            lambda: sum("mouse-sess-" in row for row in c.rows()) >= 6,
            timeout=6)

        def selected_title():
            match = re.search(r"mouse-sess-\d+", c.selected_row() or "")
            return match.group(0) if match else None

        def visible_layout():
            return [(match.group(0), y) for y, row in enumerate(c.rows())
                    if (match := re.search(r"mouse-sess-\d+", row))]

        # Force the overflowing list away from its initial viewport. This is the
        # old failure mode: hover used to mutate selection and snap the viewport.
        for _ in range(12):
            c.key(b"\x1b[B", 0.05)
        before_selection = selected_title()
        before_view = visible_layout()
        target_entry = next((entry for entry in before_view
                             if entry[0] != before_selection), None)
        target, target_y = target_entry if target_entry is not None else (None, None)
        initially_plain = target is not None and not c.row_has_background(target)

        if target_y is not None:
            c.key(f"\x1b[<35;20;{target_y + 1}M".encode(), 0.4)  # SGR Moved
        hover_visible = target is not None and c.row_has_background(target)
        selection_stable = selected_title() == before_selection
        viewport_stable = visible_layout() == before_view
        snap(c, "mouse_01_hover")

        # Moving onto a non-session line must remove the visual hover.
        c.key(b"\x1b[<35;20;3M", 0.4)
        hover_cleared = target is not None and not c.row_has_background(target)
        selection_still_stable = selected_title() == before_selection

        # Wheel behavior remains selection-based and clears any stale hover.
        wheel_y = (target_y + 1) if target_y is not None else 5
        c.key(f"\x1b[<65;20;{wheel_y}M".encode(), 0.4)
        wheel_moved = selected_title() != before_selection
        check("mouse: hover highlights without selecting or jumping; leave clears; wheel scrolls",
              loaded and initially_plain and hover_visible and selection_stable
              and viewport_stable and hover_cleared and selection_still_stable and wheel_moved,
              f"loaded={loaded} target={target!r} initial={initially_plain} "
              f"hover={hover_visible} selection={selection_stable} viewport={viewport_stable} "
              f"cleared={hover_cleared} wheel={wheel_moved}")
    finally:
        c.close()


def mouse_audit(rail, pngdir=None):
    """Focused sandbox-safe hover regression, runnable with `--mouse-only`."""
    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f"  — {detail}" if detail else ""))

    def snap(c, tag):
        if pngdir:
            c.png(os.path.join(pngdir, f"{tag}.png"))

    print(f"\n==== RAIL MOUSE AUDIT ({rail}) ====")
    _run_mouse_contract(rail, check, snap)
    passed = sum(ok for _, ok, _ in results)
    print(f"\n==== {passed}/{len(results)} mouse checks PASS ====")
    if pngdir:
        print(f"PNG snapshots written to {pngdir}/")
    return all(ok for _, ok, _ in results)


# ============================ FEATURE AUDIT ====================================
def audit(rail, pngdir=None):
    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f"  — {detail}" if detail else ""))

    def snap(c, tag):
        if pngdir:
            c.png(f"{pngdir}/{tag}.png")

    def recv_line(peer):
        data = bytearray()
        while not data.endswith(b"\n"):
            chunk = peer.recv(4096)
            if not chunk:
                break
            data.extend(chunk)
        return bytes(data)

    def control_request(path, command, timeout=3):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as peer:
            peer.settimeout(timeout)
            peer.connect(path)
            peer.sendall(command)
            return recv_line(peer)

    print(f"\n==== RAIL COCKPIT AUDIT ({rail}) ====")

    # 1) boot: header; 0 sessions -> friendly hint; >=1 session -> all 3 sections
    c = Cockpit(rail).boot()
    try:
        check("boot: header renders", any("Codex Rail" in r for r in c.rows()), c.row_with("Codex Rail"))
        check("empty: shows the 'no sessions yet' hint (not blank)",
              "No sessions yet" in c.text())
        snap(c, "01_empty")
        c.seed("seed-0", "SEED_ROW", status="exited")
        time.sleep(1.0)
        counts = c.section_counts()
        txt = c.text()
        check("sections: empty ones hidden (1 exited -> only Stopped shows)",
              "Stopped" in counts and "Needs input" not in txt and "Working" not in txt,
              str(counts))
        snap(c, "01b_sections")
    finally:
        c.close()

    # 2) new session via printable composer input: the private prompt is not argv and is not blindly
    #     typed after a fixed sleep. The fake TUI consumes any early bytes, then
    #     explicitly enables bracketed paste and records the exact submission.
    c = Cockpit(rail, codex=FAKE_PROMPT_TUI)
    prompt_record = os.path.join(c.root, "prompt-record.json")
    os.environ["CODEX_RAIL_PROMPT_RECORD"] = prompt_record
    os.environ["CODEX_RAIL_PROMPT_MODE"] = "ready"
    c.boot()
    try:
        c.new("investigate the thing")
        attached = c.wait_until(lambda: any("tick" in r for r in c.rows()), timeout=8)
        recorded = c.wait_until(
            lambda: os.path.exists(prompt_record)
                    and json.load(open(prompt_record)).get("payload") is not None
                    and json.load(open(prompt_record)).get("postcheck_done") is True,
            timeout=8)
        prompt_result = json.load(open(prompt_record)) if os.path.exists(prompt_record) else {}
        prompt_ok = prompt_result.get("payload") == "investigate the thing"
        framed = prompt_result.get("framed") is True
        no_early_input = prompt_result.get("early_hex") == ""
        expected_wire = b"\x1b[200~investigate the thing\x1b[201~\r".hex()
        exact_wire = prompt_result.get("wire_hex") == expected_wire
        submitted_once = prompt_result.get("extra_hex") == ""
        argv_private = all("investigate the thing" not in arg
                           for arg in prompt_result.get("argv", []))

        def prompt_state_cleared():
            try:
                states = [json.load(open(f"{c.jobs}/{d}/state.json")) for d in os.listdir(c.jobs)
                          if os.path.exists(f"{c.jobs}/{d}/state.json")]
            except Exception:
                return False
            return len(states) == 1 and states[0].get("initial_prompt") is None \
                   and states[0].get("initial_prompt_injecting", False) is False

        state_cleared = c.wait_until(prompt_state_cleared, timeout=5)
        snap(c, "02_attached")
        c.key(b"\x1a", 1.0)                      # Ctrl-Z detach
        listed = c.row_with("investigate the thing") is not None
        labels = [json.load(open(f"{c.jobs}/{d}/label.json")).get("title")
                  for d in os.listdir(c.jobs) if os.path.exists(f"{c.jobs}/{d}/label.json")]
        check("new session: private prompt waits for TUI readiness and is submitted once",
              attached and recorded and prompt_ok and framed and exact_wire and submitted_once
              and no_early_input and argv_private and state_cleared,
              f"attached={attached} recorded={recorded} prompt_ok={prompt_ok} framed={framed} "
              f"exact_wire={exact_wire} submitted_once={submitted_once} early={not no_early_input} "
              f"argv_private={argv_private} state_cleared={state_cleared}")
        check("new session: detach repaints list", listed and any("Codex Rail" in r for r in c.rows()))
        check("new session: title saved to label.json", "investigate the thing" in labels, str(labels))
        snap(c, "03_after_detach")
    finally:
        os.environ.pop("CODEX_RAIL_PROMPT_RECORD", None)
        os.environ.pop("CODEX_RAIL_PROMPT_MODE", None)
        c.close()

    # 3) THE AGE BUG: a streaming session's age must be STABLE, not bounce 0/1/2
    c = Cockpit(rail, codex=FAKE_STREAM).boot()
    try:
        c.seed_running_worker("agez-0", "AGE_STABILITY")
        c.wait_until(lambda: c.row_with("AGE_STABILITY") is not None, timeout=6)
        time.sleep(3)                            # let it stream (worker bumps last_output_at)
        ages = [a for a in c.sample(lambda: c.age_of("AGE_STABILITY"), 6, hz=4) if a]
        # parse seconds; a bounce shows the value going DOWN repeatedly
        def secs(a):
            try:
                return int(a[:-1]) * {"s": 1, "m": 60, "h": 3600, "d": 86400}[a[-1]]
            except Exception:
                return None
        nums = [secs(a) for a in ages if secs(a) is not None]
        drops = sum(1 for i in range(1, len(nums)) if nums[i] < nums[i - 1])
        distinct = sorted(set(ages))
        check("age column: stable (no 0/1/2 bounce)", drops == 0 and len(nums) >= 5,
              f"samples={ages[:10]} drops={drops}")
        snap(c, "04_age")
    finally:
        c.close()

    # 4) idle = zero full-screen clears (no flicker)
    c = Cockpit(rail).boot()
    try:
        c.seed("idle-0", "IDLE_ROW", status="exited")
        time.sleep(1.0)
        m = c.mark()
        time.sleep(3.0)
        chunk = c.raw_since(m)
        clears = chunk.count(b"\x1b[2J")
        check("idle: no full-screen clears (no flicker)", clears == 0, f"clears={clears} bytes={len(chunk)}")
    finally:
        c.close()

    # 5) stop a running session -> moves to Stopped, child dies, still listed
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        c.seed_running_worker("stop-0", "STOP_ME", codex=FAKE_SLEEP)
        c.wait_until(lambda: c.row_with("STOP_ME") is not None, 6)
        c.goto("STOP_ME")
        c.ctrl_x_twice()
        died = c.wait_until(lambda: not c.alive("stop-0"), timeout=8)
        row = c.row_with("STOP_ME")
        check("stop: running session stopped (child dies), still listed",
              died and row is not None and "○" in (row or ""), repr(row))
        snap(c, "05_stopped")
    finally:
        c.close()

    # 6) remove a stopped session -> leaves the list AND disk
    c = Cockpit(rail).boot()
    try:
        c.seed("rm-0", "REMOVE_ME", status="exited")
        time.sleep(0.8)
        c.goto("REMOVE_ME")
        c.key(b"\x18", 0.4)
        confirm = "remove this stopped" in c.text()
        c.key(b"\x18", 0.9)
        time.sleep(0.6)
        gone = c.row_with("REMOVE_ME") is None and not c.dir_exists("rm-0")
        check("remove: stopped session leaves list + disk", gone and confirm,
              f"confirm={confirm} gone={gone}")
    finally:
        c.close()

    # 7) zombie worker -> shown Stopped (not running) and removable
    c = Cockpit(rail).boot()
    try:
        c.seed_zombie("zomb-0", "ZOMBIE_ROW")
        time.sleep(1.0)
        row = c.row_with("ZOMBIE_ROW") or ""
        reconciled = "○" in row
        c.goto("ZOMBIE_ROW")
        c.ctrl_x_twice()
        time.sleep(0.6)
        removed = c.row_with("ZOMBIE_ROW") is None and not c.dir_exists("zomb-0")
        check("zombie worker: shown Stopped and removable", reconciled and removed,
              f"reconciled={reconciled} removed={removed}")
    finally:
        c.close()

    # 8) rename pins the title and survives a reload
    c = Cockpit(rail).boot()
    try:
        c.seed("ren-0", "old-name", status="exited")
        time.sleep(0.8)
        c.goto("old-name")
        c.rename("pinned-name")
        time.sleep(1.2)                          # >1 refresh
        lbl = json.load(open(f"{c.jobs}/ren-0/label.json"))
        check("rename: pins title, survives reload",
              c.row_with("pinned-name") is not None and lbl == {"title": "pinned-name", "title_pinned": True},
              str(lbl))
        snap(c, "08_renamed")
    finally:
        c.close()

    # 9) Navigation is arrow-only, Ctrl+N starts a blank composer, Ctrl+A owns
    #    autopilot, and every printable key (including the former w/s/d/e
    #    shortcuts, Shift, and a leading Space) enters the composer verbatim.
    c = Cockpit(rail).boot()
    try:
        c.seed("key-0", "KEY_ROW_0", status="exited")
        c.seed("key-1", "KEY_ROW_1", status="exited")

        # Compare a STABLE identity, not the whole selected-row string: the age
        # cell ticks (0s -> 1s) between the down and up reads, so a full-string
        # `first == up` compare flaked even though selection was correct.
        def sel_id():
            r = c.selected_row() or ""
            return next((k for k in ("KEY_ROW_0", "KEY_ROW_1") if k in r), None)

        c.wait_until(lambda: c.row_with("KEY_ROW_0") and c.row_with("KEY_ROW_1"), timeout=6)
        c.wait_until(lambda: sel_id() is not None, timeout=6)
        first = sel_id()
        c.down()
        moved = c.wait_until(lambda: sel_id() not in (None, first), timeout=6)
        after_down = sel_id()
        c.up()
        restored = c.wait_until(lambda: sel_id() == first, timeout=6)
        nav_ok = bool(first) and moved and after_down != first and restored

        c.key(b"\x0e", 0.0)                    # Ctrl+N
        blank_ok = c.wait_until(lambda: "Enter start" in c.text(), timeout=6)
        c.key(b"\x1b", 0.0)
        c.wait_until(lambda: "Enter start" not in c.text(), timeout=6)

        # Every printable (incl. the former w/s/d/e shortcuts, Shift, a leading
        # Space) enters the composer verbatim. Drive it with waits, not fixed
        # sleeps: on a slow runner the composer repaint lagged the read, so the
        # uppercase-W in_composer probe fired before the box was up.
        printable_ok = True
        seen = []
        for payload, visible in ((b"w", "w"), (b"s", "s"), (b"d", "d"),
                                 (b"e", "e"), (b"W", "W"), (b" ", None)):
            c.key(payload, 0.0)
            in_composer = c.wait_until(lambda: "Enter start" in c.text(), timeout=6)
            verbatim = visible is None or c.wait_until(
                lambda v=visible: v in "\n".join(c.rows()[-5:]), timeout=6)
            printable_ok &= bool(in_composer and verbatim)
            seen.append((payload.decode(), bool(in_composer), bool(verbatim)))
            c.key(b"\x1b", 0.0)               # close the composer before the next byte
            c.wait_until(lambda: "Enter start" not in c.text(), timeout=6)

        c.key(b"\x01", 0.4)                    # Ctrl+A, never composer input
        ctrl_a_ok = "Enter start" not in c.text()
        check("keys: arrows navigate; Ctrl+N blank; printable keys compose; Ctrl+A autopilot",
              nav_ok and blank_ok and printable_ok and ctrl_a_ok,
              f"nav={nav_ok} blank={blank_ok} printable={seen} ctrl_a={ctrl_a_ok}")
    finally:
        c.close()

    # 10) terminal death -> manager exits (no 100% CPU orphan)
    c = Cockpit(rail).boot()
    try:
        c.seed("td-0", "TD_ROW", status="exited")
        time.sleep(0.6)
        c.simulate_terminal_death()
        exited = c.exited_within(2.5)
        detail = "exited cleanly"
        if not exited:
            a = c.cpu_ticks()
            time.sleep(1.0)
            b = c.cpu_ticks()
            per = (b - a) if (a is not None and b is not None) else "?"
            detail = f"STILL ALIVE, cpu={per}/s (100=full core)"
        check("terminal death: manager exits, no 100% CPU orphan", exited, detail)
    finally:
        c.close()

    # 11) detach hint: a full-screen note with a progress bar that fills as the
    #     handoff nears, shown before the first N attaches then stopped for good.
    #     Pre-seed the counter to the cap to test the stop without 10 real attaches.
    c = Cockpit(rail, codex=FAKE_STREAM).boot()
    try:
        flag = os.path.join(os.path.dirname(c.jobs), ".detach_hint_count")
        m0 = c.mark()
        c.new("hello world")                         # create -> first auto-attach
        raw0 = c.raw_since(m0)
        bar = "█".encode()                       # █ — a cell of the progress bar
        counted = b"more time" in raw0               # "...show N more time(s), then stop."
        taught = b"come back to rail" in raw0 and bar in raw0 and counted
        snap(c, "11_detach_hint")
        c.key(b"\x1a", 1.0)                           # detach
        # jump the counter to the cap; the NEXT attach must NOT show the hint
        with open(flag, "w") as f:
            f.write("10\n")
        m1 = c.mark()
        c.goto("hello world")
        c.key(b"\r", 2.2)                            # attach again, now capped
        capped = b"come back to rail" not in c.raw_since(m1)
        c.key(b"\x1a", 0.6)
        check("detach hint: bar + remaining-count shown, then stops after the cap",
              taught and capped, f"taught={taught} counted={counted} stopped_after_cap={capped}")
    finally:
        c.close()

    # 12) layout: the panel floats to the vertical middle when it fits, so the
    #     eye rests on the selected row instead of the top edge (empty-state hint
    #     and the first section header are both pushed well below the header rule).
    c = Cockpit(rail).boot()
    try:
        rws = c.rows()
        empty_idx = next((i for i, r in enumerate(rws) if "No sessions yet" in r), -1)
        c.seed("mid-0", "MIDDLE_ROW", status="exited")
        time.sleep(1.0)
        rws = c.rows()
        hdr_idx = next((i for i, r in enumerate(rws)
                        if any(k in r for k in ("Needs input", "Working", "Stopped"))), -1)
        # top-pinned would sit at row ~3; floated pushes both well down (~15 @ 40 rows)
        check("layout: panel floats to vertical middle when it fits",
              empty_idx >= 8 and hdr_idx >= 8, f"empty_row={empty_idx} section_row={hdr_idx}")
        snap(c, "12_centered")
    finally:
        c.close()

    # 13) redesign: Windows-style hints (arrows + spelled Ctrl/Esc) on the status
    #     line, and a confirm prompt shows THERE — not jammed into the composer box.
    c = Cockpit(rail).boot()
    try:
        c.seed("hint-0", "HINT_ROW", status="exited")
        c.wait_until(lambda: c.row_with("HINT_ROW") is not None, timeout=6)
        # Boot leaves a one-shot "auto-imported N chat(s)…" note on the footer row.
        # A harmless Left-arrow in normal mode forces a repaint (no select/compose)
        # so the footer settles to the key hints before we assert on them.
        c.key(b"\x1b[D", 0.0)
        win_keys = c.wait_until(
            lambda: all(k in c.rows()[-1] for k in ("↑↓", "Ctrl+R", "twice")), timeout=6)
        c.key(b"\x1b", 0.0)                       # one Esc -> arm the quit confirm
        on_status = c.wait_until(lambda: "Esc again to quit" in c.rows()[-1], timeout=6)
        rows_now = c.rows()
        box_area = "\n".join(rows_now[-5:-1])     # the composer box, just above the status line
        clean_box = "Esc again" not in box_area
        check("redesign: Win-style hints + confirms on the status line, not in the box",
              win_keys and on_status and clean_box,
              f"win_keys={win_keys} on_status={on_status} clean_box={clean_box}")
        snap(c, "13_redesign")
    finally:
        c.close()

    # 14) path placement: the selected session's cwd floats on the spacer row
    #     just ABOVE the composer box (faint, right-aligned) — not on the box
    #     border, where it read as clutter.
    c = Cockpit(rail).boot()
    try:
        c.seed("path-0", "PATH_ROW", status="exited", cwd="/tmp/where-it-runs")
        time.sleep(1.0)
        rws = c.rows()
        above_box = rws[-5]                        # box_top-1: the spacer row
        bottom_border = rws[-2]                    # box_top+2: composer bottom border
        on_spacer = "where-it-runs" in above_box
        off_border = "where-it-runs" not in bottom_border and "╯" in bottom_border
        check("path: selected cwd floats above the box, not on the border",
              on_spacer and off_border,
              f"above={above_box.strip()!r} border_clean={off_border}")
        snap(c, "14_path_above_box")
    finally:
        c.close()

    # 15) distill (Ctrl+D): aggregate the user's codex history into a compact,
    #     codex-readable corpus and launch an AUTONOMOUS codex session to
    #     summarize their response style. (Fake codex here — this checks rail's
    #     half: the corpus is built and the session carries the right args.)
    c = Cockpit(rail).boot()
    try:
        c.seed_codex_history(n_sessions=2, msgs_each=4)
        c.key(b"\x04", 2.5)                        # Ctrl+D
        distill_dir = os.path.join(c.home, ".config", "codex-rail", "distill")
        distill_states = []
        for entry in (os.listdir(c.jobs) if os.path.isdir(c.jobs) else []):
            try:
                state = json.load(open(os.path.join(c.jobs, entry, "state.json")))
                if state.get("distill_version") is not None:
                    distill_states.append(state)
            except Exception:
                pass
        corpus_rel = distill_states[0].get("distill_corpus_rel") if distill_states else None
        corpus = os.path.join(distill_dir, corpus_rel) if corpus_rel else ""
        chunks = sorted(glob.glob(os.path.join(corpus, "corpus-*.md"))) \
                 if corpus and os.path.isdir(corpus) else []
        made = len(chunks) >= 1
        shown = "distill" in c.text()              # a distill session is in the list
        args_ok = False                            # ...and it carries the autonomous flags
        for st in distill_states:
            ca = st.get("codex_args", [])
            if "distill" in st.get("title", "") and "workspace-write" in ca and "-C" in ca:
                args_ok = True
        # Production runs the distill session FROM the stable distill root (its
        # cwd == the -C arg == distill_dir) and pre-trusts THAT in codex's config
        # so the TUI session doesn't stall on the first-run "trust this folder?"
        # gate. The corpus is a private per-run child (runs/<run>/corpus), not the
        # cwd, so a stale/old session can't read a later run's rewritten corpus.
        cfg = os.path.join(c.home, ".codex", "config.toml")
        cwd_ok = False
        for st in distill_states:
            ca = st.get("codex_args", [])
            c_flag = ca[ca.index("-C") + 1] if "-C" in ca and ca.index("-C") + 1 < len(ca) else None
            if st.get("cwd") == distill_dir and c_flag == distill_dir:
                cwd_ok = True
        trust_ok = cwd_ok and os.path.exists(cfg) \
                   and f'[projects."{distill_dir}"]' in open(cfg).read()
        corpus_ok = bool(corpus) \
                    and os.path.dirname(os.path.dirname(corpus)) == os.path.join(distill_dir, "runs")
        private_dirs = [os.path.dirname(distill_dir), distill_dir]
        if corpus:
            private_dirs.extend([os.path.dirname(os.path.dirname(corpus)),
                                 os.path.dirname(corpus), corpus])
        private = all((os.stat(d).st_mode & 0o777) == 0o700
                      for d in private_dirs) \
                  and all((os.stat(p).st_mode & 0o777) == 0o600 for p in chunks)
        check("distill (Ctrl+D): corpus aggregated + autonomous session + distill root pre-trusted",
              made and shown and args_ok and trust_ok and corpus_ok and private,
              f"chunks={len(chunks)} shown={shown} args_ok={args_ok} trust_ok={trust_ok} "
              f"corpus_ok={corpus_ok} private={private}")
        snap(c, "15_distill")
    finally:
        c.close()

    # 16) detach-hint occlusion fix: the Ctrl+Z hint draws in its OWN alternate
    #     screen and never clears codex's real (primary) screen. A primary clear,
    #     against the worker's tail-only replay, is what wiped a reattached codex
    #     (its recent output is partial updates, not a whole frame) from attach 2 on.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        c.seed_running_worker("occ-0", "OCCTEST", codex=FAKE_SLEEP)
        c.wait_until(lambda: c.row_with("OCCTEST") is not None, timeout=8)
        m = c.mark()
        c.key(b"\r", 1.0)                         # Enter -> attach -> show_detach_hint
        raw = c.raw_since(m)
        in_alt, primary_clears, bracketed = 1, 0, False   # rail boots inside its own alt
        for mo in re.finditer(rb"\x1b\[\?1049h|\x1b\[\?1049l|\x1b\[2J|\x1b\[3J", raw):
            t = mo.group()
            if t.endswith(b"1049h"):
                in_alt += 1
            elif t.endswith(b"1049l"):
                in_alt -= 1
            elif in_alt <= 0:
                primary_clears += 1              # a clear on codex's real screen = the bug
            else:
                bracketed = True                 # a clear safely inside the hint's own buffer
        check("detach hint: drawn in own alt screen, never clears codex's real screen",
              primary_clears == 0 and bracketed,
              f"primary_screen_clears={primary_clears} used_own_alt={bracketed}")
        snap(c, "16_occlusion")
    finally:
        c.close()

    # 17) distill session UX: a distinct "[distill vN]" label (not codex's prompt
    #     line); a finished distill (style file present) reads as "Done", not
    #     "Needs input"; a running one shows an elapsed/ETA hint so it never looks stuck.
    c = Cockpit(rail).boot()
    try:
        ddir = os.path.join(c.home, ".config", "codex-rail", "distill")
        os.makedirs(ddir, exist_ok=True)
        # a DONE distillation: cleanup and coverage have both been certified.
        c.seed("dist-done", "[distill v7]", status="exited", distill_version=7,
               distill_validated=True, worker_token=None, last_error=None)
        open(os.path.join(ddir, "style-v007.md"), "w").write("# style\n")
        open(os.path.join(ddir, "style-v007.validated"), "w").write(
            "validated-by-codex-rail\n")
        # a WORKING distillation: a running session whose rollout shows an
        # in-progress turn (task_started, no task_complete) -> Active -> Working.
        roll = os.path.join(c.home, ".codex", "sessions", "2026", "07", "06",
                            "rollout-2026-07-06T10-00-00-distwork.jsonl")
        os.makedirs(os.path.dirname(roll), exist_ok=True)
        with open(roll, "w") as f:
            f.write(json.dumps({"type": "event_msg", "payload": {"type": "task_started"}}) + "\n")
        c.seed("dist-work", "[distill v3]", status="running", distill_version=3,
               codex_rollout_path=roll, codex_session_id="cs-distwork")
        c.wait_until(lambda: c.row_with("distill v3") is not None, timeout=8)
        time.sleep(1.0)                           # let the manager reload + scan the rollout
        txt = c.text()
        label_ok = "[distill v7]" in txt and "[distill v3]" in txt
        done_ok = "Done" in txt and "✓" in txt
        hint_ok = "distilling" in txt and "~15 min" in txt
        check("distill UX: [distill vN] label + Done status + running time hint",
              label_ok and done_ok and hint_ok,
              f"label={label_ok} done={done_ok} hint={hint_ok}")
        snap(c, "17_distill_ux")
    finally:
        c.close()

    # 18) adopt: a codex session whose session_meta.cwd matches the manager's cwd
    #     is imported into the list as a resumable row, even though rail never
    #     created it — so rail manages the project's whole codex history.
    c = Cockpit(rail).boot()  # rail inherits this process's cwd
    try:
        cwd = os.getcwd()
        sid = "019f0000-ad07-7000-8000-00000000abcd"
        d = os.path.join(c.home, ".codex", "sessions", "2026", "07", "07")
        os.makedirs(d, exist_ok=True)
        roll = os.path.join(d, f"rollout-2026-07-07T00-00-00-{sid}.jsonl")
        with open(roll, "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": sid, "cwd": cwd,
                                            "timestamp": "2026-07-07T00:00:00"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "user_message",
                                            "message": "ADOPTMARKER hello there"}}) + "\n")
        with open(os.path.join(c.home, ".codex", "history.jsonl"), "a") as f:
            f.write(json.dumps({"session_id": sid, "ts": 0, "text": "ADOPTMARKER hello there"}) + "\n")
        # startup already scanned (empty); wait for the throttled rescan (300ms) to import it
        c.wait_until(lambda: c.row_with("ADOPTMARKER") is not None, timeout=8)
        imported = c.row_with("ADOPTMARKER") is not None
        # a non-matching-cwd session must NOT be imported
        sid2 = "019f0000-ad08-7000-8000-00000000ef01"
        roll2 = os.path.join(d, f"rollout-2026-07-07T00-00-01-{sid2}.jsonl")
        with open(roll2, "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": sid2, "cwd": "/tmp/some-other-dir",
                                            "timestamp": "2026-07-07T00:00:01"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "user_message", "message": "OTHERCWD nope"}}) + "\n")
        time.sleep(1.0)
        other_absent = c.row_with("OTHERCWD") is None
        check("adopt: cwd-matching codex session imported (non-matching excluded)",
              imported and other_absent, f"imported={imported} other_absent={other_absent}")
        snap(c, "18_adopt")
    finally:
        c.close()

    # 19) remove an IMPORTED row: it has no on-disk footprint, so "remove" must
    #     DISMISS it (record its codex id) — the bug was that it re-imported and
    #     wouldn't go away.
    c = Cockpit(rail).boot()
    try:
        cwd = os.getcwd()
        sid = "019f0000-d15a-7000-8000-000000001234"
        d = os.path.join(c.home, ".codex", "sessions", "2026", "07", "07")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"rollout-2026-07-07T00-00-05-{sid}.jsonl"), "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": sid, "cwd": cwd, "timestamp": "2026-07-07T00:00:05"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "user_message", "message": "DISMISSME please"}}) + "\n")
        c.wait_until(lambda: c.row_with("DISMISSME") is not None, timeout=8)
        for _ in range(15):
            if "DISMISSME" in (c.selected_row() or ""):
                break
            c.key(b"\x1b[B", 0.2)
        c.key(b"\x18", 0.4)   # Ctrl+X (confirm remove)
        c.key(b"\x18", 0.6)   # Ctrl+X (remove -> dismiss)
        gone = c.row_with("DISMISSME") is None
        dfile = os.path.join(c.data, "codex-rail", ".adopt_dismissed")
        dismissed = os.path.exists(dfile) and sid in open(dfile).read()
        time.sleep(1.2)       # let a rescan cycle run
        stays_gone = c.row_with("DISMISSME") is None
        check("adopt: removing an imported row dismisses it (stays gone, not re-imported)",
              gone and dismissed and stays_gone,
              f"gone={gone} dismissed={dismissed} stays_gone={stays_gone}")
    finally:
        c.close()

    # 20) attaching a maybe-live imported session (rollout just written) WARNS and
    #     confirms before resuming — so it can't silently start a 2nd codex on the
    #     same transcript.
    c = Cockpit(rail).boot()
    try:
        cwd = os.getcwd()
        sid = "019f0000-11e0-7000-8000-00000000abed"
        d = os.path.join(c.home, ".codex", "sessions", "2026", "07", "07")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"rollout-2026-07-07T00-00-09-{sid}.jsonl"), "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": sid, "cwd": cwd, "timestamp": "2026-07-07T00:00:09"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "user_message", "message": "LIVEMARK now"}}) + "\n")
        c.wait_until(lambda: c.row_with("LIVEMARK") is not None, timeout=8)
        for _ in range(15):
            if "LIVEMARK" in (c.selected_row() or ""):
                break
            c.key(b"\x1b[B", 0.2)
        c.key(b"\r", 0.7)     # Enter -> should WARN (not attach), rollout is fresh = maybe live
        warned = any("active elsewhere" in r for r in c.rows()) and c.row_with("LIVEMARK") is not None
        check("adopt: attaching a maybe-live session warns before resuming", warned, f"warned={warned}")
    finally:
        c.close()

    # 21) The focused import/slash contract is shared verbatim with
    #     `--import-only`, so the standalone gate and full audit cannot drift.
    _run_import_contract(rail, check, snap)

    # 22) The focused mouse contract is shared verbatim with `--mouse-only`, so
    #     the visual-hover gate and full audit cannot drift.
    _run_mouse_contract(rail, check, snap)

    # 23) preview skips codex's synthetic "<EXTERNAL SESSION IMPORTED>" marker and
    #     shows the last REAL agent message instead.
    c = Cockpit(rail).boot()
    try:
        roll = os.path.join(c.home, ".codex", "sessions", "2026", "07", "08",
                            "rollout-2026-07-08T00-00-00-mk.jsonl")
        os.makedirs(os.path.dirname(roll), exist_ok=True)
        with open(roll, "w") as f:
            f.write(json.dumps({"type": "session_meta", "payload": {"id": "mk", "cwd": "/tmp"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "agent_message", "message": "REALPREVIEW the actual answer"}}) + "\n")
            # last two are codex <> system tags — both must be skipped
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "agent_message", "message": "<command-name>compact</command-name>"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "agent_message", "message": "<EXTERNAL SESSION IMPORTED>"}}) + "\n")
        c.seed("mk", "marker-sess", status="exited", codex_rollout_path=roll, codex_session_id="mk")
        c.wait_until(lambda: c.row_with("marker-sess") is not None, timeout=6)
        time.sleep(0.8)
        row = c.row_with("marker-sess") or ""
        check("preview: skips codex <> system tags (<EXTERNAL…>, <command-name>), shows real message",
              "REALPREVIEW" in row and "SESSION IMPORTED" not in row and "command-name" not in row,
              f"row={row!r}")
    finally:
        c.close()

    # 24) the header "↑ update available" note is clickable — a left-click on it
    #     runs the update (forced on via CODEX_RAIL_FAKE_UPDATE so no GitHub call).
    os.environ["CODEX_RAIL_FAKE_UPDATE"] = "abc1234"
    try:
        c = Cockpit(rail).boot()
        try:
            c.wait_until(lambda: any("update available" in r for r in c.rows()), timeout=6)
            note = any("update available" in r for r in c.rows())
            hdr = next((r for r in c.rows() if "update available" in r), "")
            col0 = hdr.index("update available")               # 0-indexed col of the note text
            c.key(("\x1b[<0;%d;1M" % (col0 + 1)).encode(), 0.2)  # SGR left-press on the note (row 1)
            c.key(("\x1b[<0;%d;1m" % (col0 + 1)).encode(), 0.7)  # release
            after = c.text()
            fired = any(k in after for k in ("checking for updates", "up to date", "update failed", "updated to"))
            check("update notice: clicking the header note triggers the update", note and fired,
                  f"note={note} fired={fired}")
        finally:
            c.close()
    finally:
        os.environ.pop("CODEX_RAIL_FAKE_UPDATE", None)

    # 25) Ctrl+A toggles autopilot on the selected session: an "⟳ auto N/cap" badge
    #     and an "autopilot ON" status appear; Ctrl+A again turns it off.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        c.seed_running_worker("ap-1", "AUTOPILOT ME")
        c.wait_until(lambda: c.row_with("AUTOPILOT ME") is not None, timeout=8)
        c.key(b"\x01", 0.5)                                # Ctrl+A -> autopilot ON
        on_badge = any("auto 0/" in r for r in c.rows())
        on_status = any("autopilot on" in r.lower() for r in c.rows())
        c.key(b"\x01", 0.5)                                # Ctrl+A -> OFF
        off_badge = not any("auto 0/" in r for r in c.rows())
        off_status = any("autopilot off" in r.lower() for r in c.rows())
        check("autopilot: Ctrl+A toggles it (⟳ badge + status on, then off)",
              on_badge and on_status and off_badge and off_status,
              f"on={on_badge}/{on_status} off={off_badge}/{off_status}")
    finally:
        c.close()

    # 26) a live session whose socket is GONE (e.g. XDG_RUNTIME_DIR was cleared
    #     while the worker stayed alive) can still be stopped. Poison the persisted
    #     pid too: the generation-scoped control marker must work without /proc,
    #     which is the recovery path used on macOS.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        c.seed_running_worker("sock-gone", "STUCK")
        c.wait_until(lambda: c.row_with("STUCK") is not None, timeout=8)
        worker = c._pid("sock-gone", "worker_pid")
        child = c._pid("sock-gone", "child_pid")
        state_path = f"{c.jobs}/sock-gone/state.json"
        persisted = json.load(open(state_path))
        sock = persisted["socket"]
        if sock and os.path.exists(sock):
            os.remove(sock)                                  # vanish the socket
        persisted["worker_pid"] = 4294967295                 # unusable as a pid
        json.dump(persisted, open(state_path, "w"), indent=2)
        c.key(b"\x18", 0.5)
        c.key(b"\x18", 1.2)                                  # Ctrl+X twice -> stop
        def _alive(p):
            try:
                os.kill(p, 0)
                return True
            except OSError:
                return False
        killed = child is not None and not _alive(child)
        check("stop: socket-gone worker stops via generation-scoped control (no pid/proc)",
              killed, f"child={child} killed={killed}")
    finally:
        try:
            if worker:
                os.kill(worker, signal.SIGKILL)
        except Exception:
            pass
        c.close()

    # 27) SAFETY: the startup orphan-reaper must ONLY touch its own data dir. A
    #     worker in a DIFFERENT data dir (another install / the user's real rail)
    #     must survive a manager boot. (Regression for a scoping bug that once
    #     reaped live sessions across data dirs.)
    byroot = "/tmp/rc-bystdr-" + str(os.getpid())
    bydata, byrun, byhome = byroot + "/data", byroot + "/run", byroot + "/home"
    byjobs = bydata + "/codex-rail/jobs"
    for d in (byjobs, byrun, byhome):
        os.makedirs(d, exist_ok=True)
    bsid = "bystander"
    now = int(time.time())
    os.makedirs(f"{byjobs}/{bsid}", exist_ok=True)
    json.dump({"id": bsid, "title": "BYST", "cwd": "/tmp", "codex": FAKE_SLEEP, "status": "starting",
               "worker_pid": None, "child_pid": None, "socket": f"{byrun}/{bsid}.sock",
               "created_at": now, "updated_at": now, "exit_code": None, "last_error": None,
               "codex_session_id": None, "codex_rollout_path": None, "initial_prompt": None,
               "title_pinned": False, "last_output_at": 0},
              open(f"{byjobs}/{bsid}/state.json", "w"))
    json.dump({"title": "BYST", "title_pinned": False}, open(f"{byjobs}/{bsid}/label.json", "w"))
    byenv = os.environ.copy()
    byenv.update({"XDG_DATA_HOME": bydata, "XDG_RUNTIME_DIR": byrun,
                  "XDG_CONFIG_HOME": byhome + "/.config", "CODEX_HOME": byhome + "/.codex",
                  "HOME": byhome,
                  "CODEX_RAIL_CODEX": FAKE_SLEEP})
    byw = subprocess.Popen([rail, "--guardian", bsid], cwd="/tmp", env=byenv,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    byworker = bychild = None
    try:
        for _ in range(40):
            try:
                bystate = json.load(open(f"{byjobs}/{bsid}/state.json"))
                byworker = bystate.get("worker_pid")
                bychild = bystate.get("child_pid")
                if bychild:
                    break
            except Exception:
                pass
            time.sleep(0.1)
        shutil.rmtree(f"{byjobs}/{bsid}", ignore_errors=True)   # orphan it in ITS OWN dir
        time.sleep(0.3)
        c = Cockpit(rail, codex=FAKE_SLEEP).boot()               # a manager in a DIFFERENT data dir
        try:
            survived = byw.poll() is None
            check("reaper safety: does NOT reap a worker from another data dir", survived,
                  f"bystander_pid={byw.pid} survived={survived}")
        finally:
            c.close()
    finally:
        try:
            byw.kill()
        except Exception:
            pass
        if byworker:
            try:
                os.kill(byworker, signal.SIGKILL)
            except Exception:
                pass
        if bychild:
            try:
                os.kill(bychild, signal.SIGKILL)
            except Exception:
                pass
        shutil.rmtree(byroot, ignore_errors=True)

    # 28) Two managers can race to resume the same session. Only one worker may
    #     own it: the loser exits without spawning a second codex, rewriting
    #     state, or unlinking the winner's socket.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        sid = "lock-race"
        c.seed_running_worker(sid, "LOCK RACE", codex=FAKE_SLEEP)
        before = json.load(open(f"{c.jobs}/{sid}/state.json"))
        socket_before = os.stat(before["socket"]).st_ino
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": c.data, "XDG_RUNTIME_DIR": c.run,
                    "XDG_CONFIG_HOME": c.config, "CODEX_HOME": c.codex_home, "HOME": c.home,
                    "CODEX_RAIL_CODEX": FAKE_SLEEP})
        loser = subprocess.Popen([rail, "--worker", sid], cwd="/tmp", env=env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True)
        loser_exited = loser.wait(timeout=5) == 0
        time.sleep(0.4)
        after = json.load(open(f"{c.jobs}/{sid}/state.json"))
        unchanged = all(after.get(k) == before.get(k)
                        for k in ("worker_pid", "child_pid", "status", "last_error"))
        same_socket = os.path.exists(after["socket"]) \
                      and os.stat(after["socket"]).st_ino == socket_before
        winner_alive = c.alive(sid, "worker_pid") and c.alive(sid, "child_pid")
        check("worker lock: duplicate worker loses without touching the live session",
              loser_exited and unchanged and same_socket and winner_alive,
              f"loser_exited={loser_exited} unchanged={unchanged} same_socket={same_socket} winner_alive={winner_alive}")
    finally:
        c.close()

    # 29) A non-socket at the canonical path is never unlinked. The worker owns
    #     its state by this point, so the setup failure must be visible instead
    #     of leaving a silent/stuck `starting` row.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        sid = "blocked-socket"
        c.seed(sid, "BLOCKED SOCKET", status="starting", codex=FAKE_SLEEP)
        socket_path = f"{c.run}/codex-rail/{sid}.sock"
        os.makedirs(os.path.dirname(socket_path), exist_ok=True)
        open(socket_path, "w").write("do not delete")
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": c.data, "XDG_RUNTIME_DIR": c.run,
                    "XDG_CONFIG_HOME": c.config, "CODEX_HOME": c.codex_home, "HOME": c.home,
                    "CODEX_RAIL_CODEX": FAKE_SLEEP})
        failed = subprocess.Popen([rail, "--worker", sid], cwd="/tmp", env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  start_new_session=True)
        failed.wait(timeout=5)
        after = json.load(open(f"{c.jobs}/{sid}/state.json"))
        visible = after.get("status") == "failed" and "non-socket" in (after.get("last_error") or "")
        preserved = os.path.isfile(socket_path) and open(socket_path).read() == "do not delete"
        check("worker setup failure: non-socket path is preserved and reported",
              visible and preserved, f"visible={visible} preserved={preserved} state={after.get('status')}")
    finally:
        c.close()

    # 29b) A codex that dies at STARTUP (e.g. a too-old codex 400s the model)
    #      must surface codex's OWN error in the session's last_error, not just a
    #      bare "codex exited with status 1". This is the failure that made a
    #      wrong-version codex look like an unexplained, repeatedly-triggered bug.
    c = Cockpit(rail, codex=FAKE_STARTUP_ERROR).boot()
    try:
        c.seed("boom-0", "BOOMSESSION", status="starting", codex=FAKE_STARTUP_ERROR)
        c.start_guardian("boom-0", FAKE_STARTUP_ERROR)

        def boom_state():
            try:
                return json.load(open(f"{c.jobs}/boom-0/state.json"))
            except (FileNotFoundError, ValueError):
                return {}

        failed = c.wait_until(lambda: boom_state().get("status") == "failed", timeout=10)
        st = boom_state()
        err = st.get("last_error") or ""
        reason_visible = "requires a newer version of Codex" in err
        # the surfaced reason must be plain text — no ANSI escape / SGR leftovers
        ansi_free = "\x1b" not in err and "[31m" not in err and "[0m" not in err
        check("codex startup failure surfaces codex's own error, not a bare exit code",
              failed and reason_visible and ansi_free,
              f"failed={failed} status={st.get('status')!r} reason={reason_visible} "
              f"ansi_free={ansi_free} err={err!r}")
    finally:
        c.close()

    # 30) Two full manager TUIs racing Enter on the same stopped row must converge
    #     on one worker/one codex start. init.lock protects bootstrap state while
    #     worker.lock protects the lifetime owner.
    shared = "/tmp/rc-two-managers-" + str(os.getpid())
    count_file = shared + "/codex-starts"
    os.environ["CODEX_RAIL_FAKE_COUNT"] = count_file
    c1 = c2 = None
    try:
        c1 = Cockpit(rail, root=shared, codex=FAKE_SLEEP).boot()
        c2 = Cockpit(rail, root=shared, codex=FAKE_SLEEP).boot(reset=False)
        sid = "manager-race"
        c1.seed(sid, "MANAGER RACE", status="exited", codex=FAKE_SLEEP)
        c1.wait_until(lambda: c1.row_with("MANAGER RACE") is not None, timeout=6)
        c2.wait_until(lambda: c2.row_with("MANAGER RACE") is not None, timeout=6)
        os.write(c1.m, b"\r")
        os.write(c2.m, b"\r")
        end = time.time() + 8
        starts = []
        while time.time() < end:
            if os.path.exists(count_file):
                starts = [line for line in open(count_file).read().splitlines() if line]
                if starts:
                    break
            time.sleep(0.1)
        time.sleep(0.5)
        if os.path.exists(count_file):
            starts = [line for line in open(count_file).read().splitlines() if line]
        os.write(c1.m, b"\x1a")
        os.write(c2.m, b"\x1a")
        time.sleep(0.8)
        if os.path.exists(count_file):
            starts = [line for line in open(count_file).read().splitlines() if line]
        state_after = json.load(open(f"{c1.jobs}/{sid}/state.json"))
        one_start = len(starts) == 1
        owned = state_after.get("status") == "running" \
                and state_after.get("worker_lock_protocol") is True \
                and state_after.get("worker_pid") \
                and state_after.get("child_pid")
        check("manager race: two simultaneous resumes start exactly one codex",
              one_start and owned,
              f"starts={starts} status={state_after.get('status')} worker={state_after.get('worker_pid')} child={state_after.get('child_pid')}")
    finally:
        os.environ.pop("CODEX_RAIL_FAKE_COUNT", None)
        if c1:
            c1.close()
        if c2:
            c2.close()
        shutil.rmtree(shared, ignore_errors=True)

    # 31) STOP owns the whole launched process tree, including a descendant that
    #     calls setsid(), moves to an independent PGID, and ignores SIGTERM. It is
    #     not enough for the recorded direct child to die while a tool/sub-agent
    #     survives in the background.
    c = Cockpit(rail, codex=FAKE_DETACHED).boot()
    detached_records = []
    pid_file = os.path.join(c.root, "detached-pids")
    os.environ["CODEX_RAIL_DETACHED_PIDS"] = pid_file
    def read_detached():
        try:
            return [tuple(map(int, line.split())) for line in open(pid_file)
                    if len(line.split()) == 3]
        except (FileNotFoundError, ValueError):
            return []

    try:
        sid = "detached-tree"
        c.seed_running_worker(sid, "DETACHED TREE", codex=FAKE_DETACHED)
        spawned = c.wait_until(lambda: bool(read_detached()), timeout=8)
        detached_records = read_detached()
        child = c._pid(sid, "child_pid")

        def still_running(pid):
            try:
                stat = open(f"/proc/{pid}/stat").read()
                return stat[stat.rfind(") ") + 2:].split()[0] != "Z"
            except (FileNotFoundError, ProcessLookupError):
                return False

        c.goto("DETACHED TREE")
        c.ctrl_x_twice()
        direct_gone = child is not None and c.wait_until(
            lambda: not still_running(child), timeout=9)
        descendants_gone = bool(detached_records) and c.wait_until(
            lambda: all(not still_running(pid) for pid, _, _ in detached_records),
            timeout=3)
        check("stop: kills TERM-resistant descendants in independent process groups",
              spawned and direct_gone and descendants_gone,
              f"spawned={spawned} child={child} direct_gone={direct_gone} "
              f"descendants={detached_records} descendants_gone={descendants_gone}")
    finally:
        os.environ.pop("CODEX_RAIL_DETACHED_PIDS", None)
        # This cleanup is intentionally independent of rail: a failing regression
        # must never leave its adversarial detached process on the host.
        cleanup_records = set(detached_records + read_detached())
        for pid, pgid, session_id in cleanup_records:
            try:
                if pid == pgid == session_id:
                    os.killpg(pgid, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        c.close()

    # 32) Every imported rollout gets a first-resume confirmation, even when its
    #     mtime is older than the old 180s "maybe live" heuristic. Age is not
    #     proof that no other terminal still owns the transcript.
    c = Cockpit(rail)
    cwd = os.getcwd()
    sid = "019f0000-01d0-7000-8000-00000000cafe"
    d = os.path.join(c.home, ".codex", "sessions", "2026", "07", "07")
    os.makedirs(d, exist_ok=True)
    roll = os.path.join(d, f"rollout-2026-07-07T00-00-11-{sid}.jsonl")
    with open(roll, "w") as f:
        f.write(json.dumps({"type": "session_meta",
                            "payload": {"id": sid, "cwd": cwd,
                                        "timestamp": "2026-07-07T00:00:11"}}) + "\n")
        f.write(json.dumps({"type": "event_msg",
                            "payload": {"type": "user_message",
                                        "message": "OLDADOPT first attach"}}) + "\n")
    old = time.time() - 600
    os.utime(roll, (old, old))
    c.boot(reset=False)  # startup performs a full scan, including old rollouts
    try:
        imported = c.wait_until(lambda: c.row_with("OLDADOPT") is not None, timeout=8)
        c.goto("OLDADOPT")
        c.key(b"\r", 0.8)
        row = c.row_with("OLDADOPT")
        status = c.rows()[-1]
        warned = "take over" in status and row is not None
        check("adopt: first attach confirms even when rollout mtime is old",
              imported and warned,
              f"imported={imported} warned={warned} rollout_age={int(time.time() - old)}s "
              f"status={status!r} row={row!r}")
    finally:
        c.close()

    # 33) Session titles and rollout previews are untrusted terminal text. OSC
    #     sequences must be sanitized before Print reaches the user's terminal;
    #     checking raw PTY bytes catches attacks that a pyte screen would consume.
    c = Cockpit(rail).boot()
    try:
        title_osc = b"\x1b]0;TC\x07"
        preview_osc = b"\x1b]52;c;PC\x07"
        roll = os.path.join(c.home, ".codex", "sessions", "2026", "07", "08",
                            "rollout-2026-07-08T00-00-01-control.jsonl")
        os.makedirs(os.path.dirname(roll), exist_ok=True)
        with open(roll, "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": "control", "cwd": "/tmp"}}) + "\n")
            f.write(json.dumps({"type": "event_msg", "payload": {
                "type": "agent_message",
                "message": "PREVIEW_SAFE " + preview_osc.decode("ascii")}}) + "\n")
        mark = c.mark()
        c.seed("control", "TITLE_SAFE " + title_osc.decode("ascii"), status="exited",
               codex_rollout_path=roll, codex_session_id="control", title_pinned=True)
        rendered = c.wait_until(
            lambda: c.row_with("TITLE_SAFE") is not None
                    and "PREVIEW_SAFE" in (c.row_with("TITLE_SAFE") or ""),
            timeout=8)
        raw = c.raw_since(mark)
        title_leaked = title_osc in raw
        preview_leaked = preview_osc in raw
        check("terminal safety: title/preview control sequences never reach raw output",
              rendered and not title_leaked and not preview_leaked,
              f"rendered={rendered} title_leaked={title_leaked} preview_leaked={preview_leaked}")
    finally:
        c.close()

    # 34) Two managers may display one enabled autopilot, but only the manager
    #     holding its lifetime lease may drive or toggle it. The second window's
    #     Ctrl+A must be refused instead of racing a duplicate injection cycle.
    shared = "/tmp/rc-autopilot-managers-" + str(os.getpid())
    c1 = c2 = None
    try:
        c1 = Cockpit(rail, root=shared, codex=FAKE_SLEEP).boot()
        c2 = Cockpit(rail, root=shared, codex=FAKE_SLEEP).boot(reset=False)
        sid = "autopilot-owner"
        c1.seed_running_worker(sid, "ONE AUTOPILOT", codex=FAKE_SLEEP)
        c1.wait_until(lambda: c1.row_with("ONE AUTOPILOT") is not None, timeout=8)
        c2.wait_until(lambda: c2.row_with("ONE AUTOPILOT") is not None, timeout=8)
        c1.goto("ONE AUTOPILOT")
        c1.key(b"\x01", 0.8)
        apath = os.path.join(c1.jobs, sid, "autopilot.json")

        def autopilot_enabled():
            try:
                return json.load(open(apath)).get("enabled") is True
            except Exception:
                return False

        enabled = c1.wait_until(autopilot_enabled, timeout=5)
        c2.goto("ONE AUTOPILOT")
        c2.key(b"\x01", 0.8)
        refused = any("another Rail window" in r for r in c2.rows())
        still_enabled = autopilot_enabled()
        check("autopilot lease: a second manager cannot drive/toggle the same enabled autopilot",
              enabled and refused and still_enabled,
              f"enabled={enabled} refused={refused} still_enabled={still_enabled}")
    finally:
        if c2:
            c2.close()
        if c1:
            c1.close()
        shutil.rmtree(shared, ignore_errors=True)

    # 35) A distill worker is one-shot infrastructure. Once its style output is
    #     present and its rollout lifecycle is Waiting, the manager must stop the
    #     real worker and child automatically instead of leaking an idle Codex.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        sid = "distill-autostop"
        version = 41
        markers = ["a1b2c3d4", "e5f60718"]
        expected_turns = 7
        corpus_rel = "runs/run-cockpit-41/corpus"
        roll = os.path.join(c.home, ".codex", "sessions", "2026", "07", "09",
                            "rollout-2026-07-09T00-00-00-distill-autostop.jsonl")
        os.makedirs(os.path.dirname(roll), exist_ok=True)
        with open(roll, "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": "distill-autostop-codex", "cwd": "/tmp"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_started"}}) + "\n")
        c.seed_running_worker(
            sid, "[distill v41]", codex=FAKE_SLEEP, distill_version=version,
            codex_rollout_path=roll, codex_session_id="distill-autostop-codex",
            distill_expected_markers=markers,
            distill_expected_user_turns=expected_turns,
            distill_corpus_rel=corpus_rel, distill_validated=False)
        worker = c._pid(sid, "worker_pid")
        child = c._pid(sid, "child_pid")
        ddir = os.path.join(c.home, ".config", "codex-rail", "distill")
        os.makedirs(ddir, exist_ok=True)
        corpus = os.path.join(ddir, corpus_rel)
        os.makedirs(corpus, exist_ok=True)
        style = os.path.join(ddir, f"style-v{version:03}.md")
        with open(style, "w") as f:
            f.write("# complete\n\n## Coverage\n")
            for marker in markers:
                f.write(f"CHUNK_ID={marker}\n")
            f.write(f"USER_TURNS_READ={expected_turns}\n")
        # Only now transition the rollout from Active to Waiting. This prevents
        # the worker from observing a partial style footer during fixture setup.
        with open(roll, "a") as f:
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_complete",
                                            "last_agent_message": "style written"}}) + "\n")

        def process_running(pid):
            if not pid:
                return False
            try:
                stat = open(f"/proc/{pid}/stat").read()
                return stat[stat.rfind(") ") + 2:].split()[0] != "Z"
            except (FileNotFoundError, ProcessLookupError):
                return False

        stopped = c.wait_until(
            lambda: not process_running(worker) and not process_running(child),
            timeout=18)
        try:
            final_state = json.load(open(f"{c.jobs}/{sid}/state.json"))
        except Exception:
            final_state = {}
        validated = os.path.join(ddir, f"style-v{version:03}.validated")
        marker_ok = os.path.exists(validated) \
                    and open(validated).read() == "validated-by-codex-rail\n"
        check("distill lifecycle: exact Coverage validates, marks, cleans corpus, and stops",
              worker is not None and child is not None and stopped
              and final_state.get("status") == "exited"
              and final_state.get("distill_validated") is True
              and final_state.get("worker_pid") is None
              and final_state.get("child_pid") is None
              and final_state.get("worker_token") is None
              and final_state.get("last_error") is None
              and marker_ok and not os.path.exists(os.path.dirname(corpus)),
              f"worker={worker} child={child} stopped={stopped} status={final_state.get('status')} "
              f"validated={final_state.get('distill_validated')} marker={marker_ok} "
              f"corpus_clean={not os.path.exists(os.path.dirname(corpus))} "
              f"error={final_state.get('last_error')!r}")
    finally:
        c.close()

    # 36 / check 40) SIGKILL the worker itself, bypassing RunGuard::drop. Its
    #     guardian must reap both direct Codex and detached setsid descendants,
    #     clear the token only after certification, and leave a failed/resumable row.
    shared = "/tmp/rc-abandoned-generation-" + str(os.getpid())
    pid_file = os.path.join(shared, "detached-pids")
    os.environ["CODEX_RAIL_DETACHED_PIDS"] = pid_file
    c1 = c2 = None
    worker = child = None
    detached_records = []

    def read_abandoned_detached():
        try:
            return [tuple(map(int, line.split())) for line in open(pid_file)
                    if len(line.split()) == 3]
        except (FileNotFoundError, ValueError):
            return []

    def abandoned_process_running(pid):
        if not pid:
            return False
        try:
            stat = open(f"/proc/{pid}/stat").read()
            return stat[stat.rfind(") ") + 2:].split()[0] != "Z"
        except (FileNotFoundError, ProcessLookupError):
            return False

    try:
        c1 = Cockpit(rail, root=shared, codex=FAKE_DETACHED).boot()
        sid = "abandoned-generation"
        c1.seed_running_worker(sid, "ABANDONED GENERATION", codex=FAKE_DETACHED)
        spawned = c1.wait_until(lambda: bool(read_abandoned_detached()), timeout=8)
        detached_records = read_abandoned_detached()
        before = json.load(open(f"{c1.jobs}/{sid}/state.json"))
        worker = before.get("worker_pid")
        child = before.get("child_pid")
        token = before.get("worker_token")
        if worker:
            os.kill(worker, signal.SIGKILL)  # worker only; deliberately bypass Drop
        c1.wait_until(lambda: not abandoned_process_running(worker), timeout=4)

        c2 = Cockpit(rail, root=shared, codex=FAKE_DETACHED).boot(reset=False)

        def recovered_state():
            try:
                state = json.load(open(f"{c1.jobs}/{sid}/state.json"))
            except Exception:
                return None
            descendants = [child] + [pid for pid, _, _ in read_abandoned_detached()]
            clean = all(not abandoned_process_running(pid) for pid in descendants)
            error = state.get("last_error") or ""
            cleanup_recorded = "guardian cleaned its process generation" in error
            if state.get("status") == "failed" and cleanup_recorded and clean:
                return state
            return None

        recovered = c2.wait_until(lambda: recovered_state() is not None, timeout=12)
        after = recovered_state() or {}
        descendants = [child] + [pid for pid, _, _ in read_abandoned_detached()]
        descendants_gone = all(not abandoned_process_running(pid) for pid in descendants)
        check("guardian: worker SIGKILL reaps direct + detached generation and clears token",
              spawned and worker is not None and child is not None and token
              and recovered and descendants_gone
              and after.get("worker_pid") is None and after.get("child_pid") is None,
              f"spawned={spawned} worker={worker} child={child} token={bool(token)} "
              f"recovered={recovered} descendants={descendants} gone={descendants_gone} "
              f"status={after.get('status')} error={after.get('last_error')!r}")
    finally:
        os.environ.pop("CODEX_RAIL_DETACHED_PIDS", None)
        detached_records = list(set(detached_records + read_abandoned_detached()))
        for pid, pgid, session_id in detached_records:
            try:
                if pid == pgid == session_id:
                    os.killpg(pgid, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        if child:
            try:
                os.kill(child, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        if c2:
            c2.close()
        if c1:
            c1.close()
        if worker:
            try:
                os.waitpid(worker, os.WNOHANG)
            except (ChildProcessError, ProcessLookupError):
                pass
        shutil.rmtree(shared, ignore_errors=True)

    # 37 / check 41) Bracketed-paste mode alone is not prompt readiness. A fake
    #     TUI that never renders its composer must time out as failed, with zero
    #     bytes written to its stdin and no private prompt in argv.
    c = Cockpit(rail, codex=FAKE_PROMPT_TUI)
    prompt_record = os.path.join(c.root, "never-ready-prompt.json")
    os.environ["CODEX_RAIL_PROMPT_RECORD"] = prompt_record
    os.environ["CODEX_RAIL_PROMPT_MODE"] = "never"
    os.environ["CODEX_RAIL_PROMPT_READY_TIMEOUT_SECS"] = "2"
    c.boot()
    try:
        private_prompt = "NEVER_READY_PRIVATE_PROMPT"
        c.new(private_prompt)

        def never_ready_state():
            try:
                for entry in os.listdir(c.jobs):
                    path = os.path.join(c.jobs, entry, "state.json")
                    if os.path.exists(path):
                        return json.load(open(path))
            except Exception:
                pass
            return {}

        failed = c.wait_until(
            lambda: never_ready_state().get("status") == "failed", timeout=12)
        prompt_result = json.load(open(prompt_record)) if os.path.exists(prompt_record) else {}
        # portable-pty 0.8.1's Unix writer Drop emits newline+VEOF ("0a04") during
        # timeout cleanup — that's dependency shutdown, not blind prompt injection.
        # Accept empty or exactly that sentinel; still forbid framed/private bytes.
        no_blind_input = prompt_result.get("early_hex") in ("", "0a04") \
                         and prompt_result.get("payload") is None \
                         and prompt_result.get("framed") is not True
        argv_private = all(private_prompt not in arg for arg in prompt_result.get("argv", []))
        state = never_ready_state()
        check("initial prompt: never-ready TUI fails without argv or blind PTY injection",
              failed and no_blind_input and argv_private,
              f"failed={failed} no_blind_input={no_blind_input} argv_private={argv_private} "
              f"status={state.get('status')} error={state.get('last_error')!r}")
    finally:
        os.environ.pop("CODEX_RAIL_PROMPT_RECORD", None)
        os.environ.pop("CODEX_RAIL_PROMPT_MODE", None)
        os.environ.pop("CODEX_RAIL_PROMPT_READY_TIMEOUT_SECS", None)
        c.close()

    # 38 / check 42) Natural root exit is also a process-generation boundary.
    #     A successful direct fake Codex exits after spawning a TERM-resistant
    #     setsid child; the worker's ChildExit path must reap that descendant and
    #     persist a clean exited state without requiring an explicit STOP.
    c = Cockpit(rail, codex=FAKE_DETACHED).boot()
    pid_file = os.path.join(c.root, "natural-exit-detached-pids")
    os.environ["CODEX_RAIL_DETACHED_PIDS"] = pid_file
    os.environ["CODEX_RAIL_DETACHED_ROOT_MODE"] = "exit"
    detached_records = []

    def read_natural_detached():
        try:
            return [tuple(map(int, line.split())) for line in open(pid_file)
                    if len(line.split()) == 3]
        except (FileNotFoundError, ValueError):
            return []

    def natural_process_running(pid):
        if not pid:
            return False
        try:
            stat = open(f"/proc/{pid}/stat").read()
            return stat[stat.rfind(") ") + 2:].split()[0] != "Z"
        except (FileNotFoundError, ProcessLookupError):
            return False

    try:
        sid = "natural-exit-tree"
        c.seed_running_worker(sid, "NATURAL EXIT TREE", codex=FAKE_DETACHED)
        spawned = c.wait_until(lambda: bool(read_natural_detached()), timeout=8)
        detached_records = read_natural_detached()

        def clean_exit_state():
            try:
                state = json.load(open(f"{c.jobs}/{sid}/state.json"))
            except Exception:
                return None
            gone = all(not natural_process_running(pid)
                       for pid, _, _ in read_natural_detached())
            if state.get("status") == "exited" and gone:
                return state
            return None

        cleaned = c.wait_until(lambda: clean_exit_state() is not None, timeout=10)
        state = clean_exit_state() or {}
        descendants_gone = bool(detached_records) and all(
            not natural_process_running(pid) for pid, _, _ in detached_records)
        check("natural exit: ChildExit cleanup reaps detached process generation",
              spawned and cleaned and descendants_gone and state.get("last_error") is None,
              f"spawned={spawned} cleaned={cleaned} descendants={detached_records} "
              f"gone={descendants_gone} status={state.get('status')} error={state.get('last_error')!r}")
    finally:
        os.environ.pop("CODEX_RAIL_DETACHED_PIDS", None)
        os.environ.pop("CODEX_RAIL_DETACHED_ROOT_MODE", None)
        detached_records = list(set(detached_records + read_natural_detached()))
        for pid, pgid, session_id in detached_records:
            try:
                if pid == pgid == session_id:
                    os.killpg(pgid, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        c.close()

    # 39) The private first prompt is a logical submission, not argv. Preserve
    #     multiline/CJK/tab, strip terminal controls, and consume it once so a
    #     later guarded resume cannot replay an externally-visible side effect.
    c = Cockpit(rail, codex=FAKE_PROMPT_TUI).boot()
    prompt_record = os.path.join(c.root, "complex-initial-prompt.json")
    os.environ["CODEX_RAIL_PROMPT_RECORD"] = prompt_record
    os.environ["CODEX_RAIL_PROMPT_MODE"] = "ready"
    try:
        sid = "complex-initial"
        private_prompt = "第一行\n\tsecond 👩‍💻\x1b[201~evil"
        safe_prompt = "第一行\n\tsecond 👩‍💻[201~evil"
        c.seed_running_worker(sid, "COMPLEX INITIAL", codex=FAKE_PROMPT_TUI,
                              initial_prompt=private_prompt)
        first_done = c.wait_until(
            lambda: os.path.exists(prompt_record)
                    and json.load(open(prompt_record)).get("postcheck_done") is True,
            timeout=12)
        first = json.load(open(prompt_record)) if os.path.exists(prompt_record) else {}
        expected_wire = (b"\x1b[200~" + safe_prompt.encode("utf-8") + b"\x1b[201~\r").hex()
        first_state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        once = (first_done and first.get("payload") == safe_prompt
                and first.get("wire_hex") == expected_wire
                and first.get("early_hex") == "" and first.get("extra_hex") == ""
                and all(private_prompt not in arg for arg in first.get("argv", []))
                and first_state.get("initial_prompt") is None
                and first_state.get("initial_prompt_injecting") is False)

        first_pid = first.get("pid")
        first_guardian = c._guardians[-1]
        stop_ack = control_request(first_state["socket"], b"STOP\n") == b"STOPPING\n"
        stopped = c.wait_until(
            lambda: json.load(open(os.path.join(c.jobs, sid, "state.json"))).get("status")
                    == "exited", timeout=10)
        try:
            first_guardian.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass

        resume_state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        resume_state["status"] = "starting"
        resume_state["updated_at"] = int(time.time())
        with open(os.path.join(c.jobs, sid, "state.json"), "w") as f:
            json.dump(resume_state, f, indent=2)
        c.start_guardian(sid, FAKE_PROMPT_TUI)
        resumed = c.wait_until(
            lambda: os.path.exists(prompt_record)
                    and json.load(open(prompt_record)).get("pid") != first_pid
                    and json.load(open(prompt_record)).get("composer_emitted") is True,
            timeout=10)
        second = json.load(open(prompt_record)) if os.path.exists(prompt_record) else {}
        no_replay = resumed and second.get("payload") is None and second.get("early_hex") == ""
        second_state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        second_stop_ack = control_request(second_state["socket"], b"STOP\n") == b"STOPPING\n"
        check("initial prompt: multiline/CJK/tab exact, controls stripped, argv private, resume once",
              once and stop_ack and stopped and no_replay and second_stop_ack,
              f"first={first_done}/{once} stop={stop_ack}/{stopped} resumed={resumed} "
              f"no_replay={no_replay} second_stop={second_stop_ack}")
    finally:
        os.environ.pop("CODEX_RAIL_PROMPT_RECORD", None)
        os.environ.pop("CODEX_RAIL_PROMPT_MODE", None)
        c.close()

    # 40) Headless delivery has a strict two-phase protocol. Active/stale or
    #     human-attached sessions are BUSY; an idle Waiting session is READY,
    #     accepts exactly one framed bracketed submission, then says DELIVERED.
    c = Cockpit(rail, codex=FAKE_PROMPT_TUI).boot()
    prompt_record = os.path.join(c.root, "inject-record.json")
    os.environ["CODEX_RAIL_PROMPT_RECORD"] = prompt_record
    os.environ["CODEX_RAIL_PROMPT_MODE"] = "ready"
    attach_peer = inject_peer = None
    try:
        sid = "inject-handshake"
        rollout = os.path.join(c.codex_home, "sessions", "2026", "07", "10",
                               "rollout-2026-07-10T00-00-00-inject.jsonl")
        os.makedirs(os.path.dirname(rollout), exist_ok=True)
        with open(rollout, "w") as f:
            f.write(json.dumps({"type": "session_meta",
                                "payload": {"id": "inject-codex", "cwd": "/tmp"}}) + "\n")
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_started"}}) + "\n")
        c.seed_running_worker(sid, "INJECT HANDSHAKE", codex=FAKE_PROMPT_TUI,
                              codex_rollout_path=rollout,
                              codex_session_id="inject-codex")
        state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        sock_path = state["socket"]
        composer = c.wait_until(
            lambda: os.path.exists(prompt_record)
                    and json.load(open(prompt_record)).get("composer_emitted") is True,
            timeout=8)
        active_busy = control_request(sock_path, b"INJECT\n") == b"BUSY\n"

        with open(rollout, "a") as f:
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_complete"}}) + "\n")
        attach_peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        attach_peer.settimeout(3)
        attach_peer.connect(sock_path)
        attach_peer.sendall(b"ATTACH 24 80\n")
        time.sleep(0.4)
        attached_busy = control_request(sock_path, b"INJECT\n") == b"BUSY\n"
        attach_zero = json.load(open(prompt_record)).get("payload") is None
        attach_peer.close()
        attach_peer = None
        time.sleep(0.5)

        inject_peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        inject_peer.settimeout(3)
        inject_peer.connect(sock_path)
        inject_peer.sendall(b"INJECT\n")
        ready = recv_line(inject_peer) == b"READY\n"
        raw_message = "idle 第一行\n\tsecond 👩‍💻\x1b[201~evil"
        safe_message = "idle 第一行\n\tsecond 👩‍💻[201~evil"
        wire = b"\x1b[200~" + safe_message.encode("utf-8") + b"\x1b[201~\r"
        inject_peer.sendall(b"\x00" + struct.pack("!I", len(wire)) + wire)
        delivered = recv_line(inject_peer) == b"DELIVERED\n"
        # INJECT is a one-frame protocol: the worker closes this headless slot the
        # instant it emits DELIVERED (worker.rs ~590). Sending a follow-up detach
        # frame here races that close and raises BrokenPipe — just close.
        inject_peer.close()
        inject_peer = None
        recorded = c.wait_until(
            lambda: os.path.exists(prompt_record)
                    and json.load(open(prompt_record)).get("postcheck_done") is True,
            timeout=8)
        record = json.load(open(prompt_record)) if os.path.exists(prompt_record) else {}
        exact = (recorded and record.get("payload") == safe_message
                 and record.get("wire_hex") == wire.hex()
                 and record.get("extra_hex") == "")

        with open(rollout, "a") as f:
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_started"}}) + "\n")
        stale_busy = control_request(sock_path, b"INJECT\n") == b"BUSY\n"
        unchanged = json.load(open(prompt_record)).get("wire_hex") == wire.hex()
        check("INJECT: BUSY when active/attached, READY→DELIVERED idle exact wire, stale sends zero",
              composer and active_busy and attached_busy and attach_zero and ready
              and delivered and exact and stale_busy and unchanged,
              f"composer={composer} active={active_busy} attached={attached_busy}/{attach_zero} "
              f"ready={ready} delivered={delivered} exact={exact} stale={stale_busy}/{unchanged}")
        control_request(sock_path, b"STOP\n")
    finally:
        if inject_peer:
            inject_peer.close()
        if attach_peer:
            attach_peer.close()
        os.environ.pop("CODEX_RAIL_PROMPT_RECORD", None)
        os.environ.pop("CODEX_RAIL_PROMPT_MODE", None)
        c.close()

    # 41) STOP acknowledgement is immediate but cleanup is separately certified.
    #     Freeze the worker immediately after ACK; the manager watchdog must wait
    #     8s, verify identity, SIGKILL it, and let the guardian reap descendants.
    c = Cockpit(rail, codex=FAKE_DETACHED).boot()
    pid_file = os.path.join(c.root, "hung-stop-detached-pids")
    os.environ["CODEX_RAIL_DETACHED_PIDS"] = pid_file
    detached_records = []

    def read_hung_detached():
        try:
            return [tuple(map(int, line.split())) for line in open(pid_file)
                    if len(line.split()) == 3]
        except (FileNotFoundError, ValueError):
            return []

    def hung_running(pid):
        if not pid:
            return False
        try:
            stat = open(f"/proc/{pid}/stat").read()
            return stat[stat.rfind(") ") + 2:].split()[0] != "Z"
        except (FileNotFoundError, ProcessLookupError):
            return False

    try:
        sid = "hung-stop"
        c.seed_running_worker(sid, "HUNG STOP", codex=FAKE_DETACHED)
        spawned = c.wait_until(lambda: bool(read_hung_detached()), timeout=8)
        detached_records = read_hung_detached()
        state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        worker, child = state.get("worker_pid"), state.get("child_pid")
        stop_ack = control_request(state["socket"], b"STOP\n") == b"STOPPING\n"
        if worker:
            os.kill(worker, signal.SIGSTOP)
        c.goto("HUNG STOP")
        c.ctrl_x_twice()
        accepted = c.wait_until(
            lambda: "stop accepted; waiting for verified cleanup" in c.text(), timeout=4)
        samples = []
        deadline = time.time() + 13
        while time.time() < deadline:
            samples.append(c.text())
            if not hung_running(worker) and not hung_running(child) \
                    and all(not hung_running(pid) for pid, _, _ in read_hung_detached()):
                break
            time.sleep(0.05)
        escalated = any("escalated to verified SIGKILL" in sample for sample in samples)
        gone = (not hung_running(worker) and not hung_running(child)
                and all(not hung_running(pid) for pid, _, _ in read_hung_detached()))
        # The guardian writes the certified terminal state (cleared pids/token +
        # failed status) AFTER the tree dies, so poll for it rather than reading
        # the file the instant `gone` flips — the write can lag process death.
        def is_certified():
            try:
                f = json.load(open(os.path.join(c.jobs, sid, "state.json")))
            except (FileNotFoundError, ValueError):
                return False
            return (f.get("worker_pid") is None and f.get("child_pid") is None
                    and f.get("worker_token") is None and f.get("status") == "failed")
        certified = c.wait_until(is_certified, timeout=8)
        final = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        check("STOP: exact ACK; SIGSTOP worker escalates at 8s; guardian certifies full cleanup",
              spawned and stop_ack and accepted and escalated and gone and certified,
              f"spawned={spawned} ack={stop_ack} accepted={accepted} escalated={escalated} "
              f"gone={gone} certified={certified} status={final.get('status')!r} "
              f"wpid={final.get('worker_pid')} cpid={final.get('child_pid')} "
              f"token={final.get('worker_token') is not None} error={final.get('last_error')!r}")
    finally:
        os.environ.pop("CODEX_RAIL_DETACHED_PIDS", None)
        detached_records = list(set(detached_records + read_hung_detached()))
        for pid, pgid, session_id in detached_records:
            try:
                if pid == pgid == session_id:
                    os.killpg(pgid, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        try:
            if 'worker' in locals() and worker:
                os.kill(worker, signal.SIGCONT)
        except (ProcessLookupError, PermissionError):
            pass
        c.close()

    # 42) A wrong Coverage footer is a hard failure: no validated marker, no
    #     Done state, and the immutable corpus is retained for diagnosis/retry.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        sid, version = "distill-invalid", 42
        corpus_rel = "runs/run-cockpit-42/corpus"
        rollout = os.path.join(c.codex_home, "sessions", "2026", "07", "11",
                               "rollout-2026-07-11T00-00-00-distill-invalid.jsonl")
        os.makedirs(os.path.dirname(rollout), exist_ok=True)
        with open(rollout, "w") as f:
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_started"}}) + "\n")
        c.seed_running_worker(
            sid, "[distill v42]", codex=FAKE_SLEEP, distill_version=version,
            codex_rollout_path=rollout, codex_session_id="distill-invalid-codex",
            distill_expected_markers=["expected01"], distill_expected_user_turns=3,
            distill_corpus_rel=corpus_rel, distill_validated=False)
        ddir = os.path.join(c.config, "codex-rail", "distill")
        corpus = os.path.join(ddir, corpus_rel)
        os.makedirs(corpus, exist_ok=True)
        style = os.path.join(ddir, f"style-v{version:03}.md")
        with open(style, "w") as f:
            f.write("# invalid\n\n## Coverage\nCHUNK_ID=wrong000\nUSER_TURNS_READ=3\n")
        with open(rollout, "a") as f:
            f.write(json.dumps({"type": "event_msg",
                                "payload": {"type": "task_complete"}}) + "\n")
        failed = c.wait_until(
            lambda: json.load(open(os.path.join(c.jobs, sid, "state.json"))).get("status")
                    == "failed", timeout=12)
        state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
        marker = os.path.join(ddir, f"style-v{version:03}.validated")
        rejected = (failed and state.get("distill_validated") is False
                    and not os.path.exists(marker) and os.path.isdir(corpus)
                    and (state.get("last_error") or "").startswith(
                        "distillation output failed coverage validation:"))
        check("distill validation: wrong/missing/duplicate coverage fails closed and self-stops",
              rejected and state.get("worker_token") is None
              and state.get("worker_pid") is None and state.get("child_pid") is None,
              f"failed={failed} rejected={rejected} error={state.get('last_error')!r}")
    finally:
        c.close()

    # 43) Concurrent prepares share one global lease/version allocator but use
    #     immutable per-run corpora. Both may succeed sequentially under the lock;
    #     they must never share a version claim or corpus directory.
    c = Cockpit(rail)
    for directory in (c.jobs, c.run, c.home, c.config, c.codex_home):
        os.makedirs(directory, exist_ok=True)
    try:
        c.seed_codex_history(n_sessions=3, msgs_each=6)
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": c.data, "XDG_RUNTIME_DIR": c.run,
                    "XDG_CONFIG_HOME": c.config, "CODEX_HOME": c.codex_home,
                    "HOME": c.home, "CODEX_RAIL_DISTILL_CHUNK_BYTES": "900",
                    "CODEX_RAIL_DISTILL_MSG_CAP": "700"})
        prepares = [subprocess.Popen([rail, "--distill-prepare"], env=env,
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     text=True) for _ in range(2)]
        outputs = []
        returncodes = []
        for prepare in prepares:
            output, _ = prepare.communicate(timeout=45)
            outputs.append(output)
            returncodes.append(prepare.returncode)
        versions, corpora = [], []
        for output in outputs:
            version_match = re.search(r"next output: \S+/style-v(\d+)\.md", output)
            corpus_match = re.search(r"available -> (\S+/runs/run-[^/]+/corpus)", output)
            if version_match:
                versions.append(int(version_match.group(1)))
            if corpus_match:
                corpora.append(corpus_match.group(1))
        # The run lock is non-blocking (distill.rs acquire_run_lock: LOCK_EX|LOCK_NB):
        # a distillation holds it for its whole prepare->codex->validate lifetime, so
        # a second prepare that OVERLAPS the first is refused fail-closed rather than
        # queued for minutes. Whichever prepares DO win must never share a version
        # claim or corpus directory; any loser must be exactly that fail-closed
        # refusal ("already preparing or running"), never a crash.
        succeeded = [out for rc, out in zip(returncodes, outputs) if rc == 0]
        losers = [out for rc, out in zip(returncodes, outputs) if rc != 0]
        claims = glob.glob(os.path.join(c.config, "codex-rail", "distill", "claims",
                                        "style-v*.claim"))
        isolated = (len(succeeded) >= 1
                    and len(versions) == len(corpora) == len(succeeded)
                    and len(set(versions)) == len(versions)
                    and len(set(corpora)) == len(corpora)
                    and all(os.path.isdir(p) for p in corpora)
                    and all(glob.glob(os.path.join(p, "corpus-*.md")) for p in corpora)
                    and all("already preparing or running" in out for out in losers)
                    and len(claims) == len(succeeded))
        check("distill prepare: global lock/version claims and concurrent run-corpus isolation",
              isolated,
              f"rc={returncodes} versions={versions} corpora={corpora} "
              f"claims={len(claims)} succeeded={len(succeeded)}")
    finally:
        c.close()

    # 44) Tiny terminals fail gracefully, resize repaints, and common grapheme
    #     clusters/CJK stay renderable. CI persists all three frames as PNGs.
    c = Cockpit(rail, cols=20, rows=8).boot()
    try:
        tiny_20 = "terminal too small" in c.text().lower() and c.p.poll() is None
        snap(c, "44_tiny_20x8")
        c.resize(40, 12)
        tiny_40 = "terminal too small" in c.text().lower() and c.p.poll() is None
        snap(c, "44_tiny_40x12")
        c.resize(80, 20)
        unicode_titles = ["U1 e\u0301", "U2 ❤️", "U3 👩‍💻", "U4 🇨🇳", "U5 中文"]
        for index, title in enumerate(unicode_titles):
            c.seed(f"unicode-layout-{index}", title, status="exited", title_pinned=True)
        rendered = c.wait_until(
            lambda: all(c.row_with(f"U{index}") is not None for index in range(1, 6))
                    and "中文" in c.text(), timeout=6)
        healthy = rendered and c.p.poll() is None and any("Codex Rail" in row for row in c.rows())
        snap(c, "44_resized_unicode")
        check("visual: 20x8 + 40x12 graceful, resize recovers, Unicode/CJK render, PNG captured",
              tiny_20 and tiny_40 and healthy,
              f"20x8={tiny_20} 40x12={tiny_40} unicode={rendered} healthy={healthy}")
    finally:
        c.close()

    # 45) cleanup_pending survives a manager crash. A frozen pilot keeps its
    #     durable main link while the watchdog works; only certified guardian
    #     cleanup may remove the pilot and clear that link on restart.
    c = Cockpit(rail, codex=FAKE_SLEEP)
    for directory in (c.jobs, c.run, c.home, c.config, c.codex_home):
        os.makedirs(directory, exist_ok=True)
    worker = None
    try:
        main_id, pilot_id = "cleanup-main", "cleanup-pilot"
        c.seed(main_id, "CLEANUP MAIN", status="exited")
        c.seed(pilot_id, "↳ pilot · CLEANUP MAIN", status="starting", codex=FAKE_SLEEP)
        c.start_guardian(pilot_id, FAKE_SLEEP)
        started = c.wait_until(lambda: c._pid(pilot_id, "child_pid") is not None, timeout=8)
        worker = c._pid(pilot_id, "worker_pid")
        control_path = os.path.join(c.jobs, main_id, "autopilot.json")
        with open(control_path, "w") as f:
            json.dump({"enabled": False, "marker_version": 2, "pilot_id": pilot_id,
                       "replies": 0, "cap": 8, "phase": "Idle",
                       "main_marker": "", "pilot_marker": "", "pending_reply": "",
                       "last_reason": "restart cleanup", "cleanup_pending": True,
                       "phase_started_at": 0}, f, indent=2)
        if worker:
            os.kill(worker, signal.SIGSTOP)
        c.boot(reset=False)
        time.sleep(0.5)
        during = json.load(open(control_path))
        preserved = (during.get("pilot_id") == pilot_id
                     and during.get("cleanup_pending") is True
                     and os.path.exists(os.path.join(c.jobs, pilot_id, "state.json")))

        def cleanup_finished():
            try:
                control = json.load(open(control_path))
            except Exception:
                return False
            return (control.get("pilot_id") is None
                    and control.get("cleanup_pending") is False
                    and not os.path.exists(os.path.join(c.jobs, pilot_id, "state.json")))

        finished = c.wait_until(cleanup_finished, timeout=15)
        check("autopilot restart: hung cleanup_pending pilot link preserved then certified/removed",
              started and preserved and finished
              and os.path.exists(os.path.join(c.jobs, main_id, "state.json")),
              f"started={started} preserved={preserved} finished={finished} main_kept="
              f"{os.path.exists(os.path.join(c.jobs, main_id, 'state.json'))}")
    finally:
        try:
            if worker:
                os.kill(worker, signal.SIGCONT)
        except (ProcessLookupError, PermissionError):
            pass
        c.close()

    # 46) Removing a main may not orphan its internal pilot. The first removal
    #     request retires the pilot; after verification, the second removes the
    #     main and its now-unused control file.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        main_id, pilot_id = "remove-main", "remove-pilot"
        c.seed(main_id, "REMOVE MAIN WITH PILOT", status="exited")
        c.seed_running_worker(pilot_id, "↳ INTERNAL PILOT", codex=FAKE_SLEEP)
        control_path = os.path.join(c.jobs, main_id, "autopilot.json")
        with open(control_path, "w") as f:
            json.dump({"enabled": True, "marker_version": 2, "pilot_id": pilot_id,
                       "replies": 0, "cap": 8, "phase": "Generating",
                       "main_marker": "v2:1", "pilot_marker": "",
                       "pending_reply": "", "last_reason": None,
                       "cleanup_pending": False, "phase_started_at": int(time.time())},
                      f, indent=2)
        c.wait_until(lambda: c.row_with("REMOVE MAIN WITH PILOT") is not None, timeout=6)
        c.goto("REMOVE MAIN WITH PILOT")
        c.ctrl_x_twice()
        pilot_retired = c.wait_until(
            lambda: not os.path.exists(os.path.join(c.jobs, pilot_id, "state.json")), timeout=12)
        control = json.load(open(control_path)) if os.path.exists(control_path) else {}
        link_cleared = control.get("pilot_id") is None and control.get("cleanup_pending") is False
        c.goto("REMOVE MAIN WITH PILOT")
        c.ctrl_x_twice()
        main_removed = c.wait_until(
            lambda: not os.path.exists(os.path.join(c.jobs, main_id, "state.json")), timeout=5)
        check("autopilot ownership: main deletion retires linked pilot before removing main",
              pilot_retired and link_cleared and main_removed and not os.path.exists(control_path),
              f"pilot_retired={pilot_retired} link_cleared={link_cleared} "
              f"main_removed={main_removed} control_exists={os.path.exists(control_path)}")
    finally:
        c.close()

    # 47) Crash-state phases are fail-closed. Injecting never resends a pending
    #     reply; a planned-but-missing pilot cannot become an orphan; an expired
    #     phase pauses at the configured timeout.
    c = Cockpit(rail, codex=FAKE_PROMPT_TUI)
    for directory in (c.jobs, c.run, c.home, c.config, c.codex_home):
        os.makedirs(directory, exist_ok=True)
    prompt_record = os.path.join(c.root, "autopilot-duplicate-record.json")
    os.environ["CODEX_RAIL_PROMPT_RECORD"] = prompt_record
    os.environ["CODEX_RAIL_PROMPT_MODE"] = "ready"
    os.environ["CODEX_RAIL_AUTOPILOT_PHASE_TIMEOUT_SECS"] = "2"
    try:
        now = int(time.time())
        inject_id = "phase-injecting"
        c.seed(inject_id, "PHASE INJECTING", status="starting", codex=FAKE_PROMPT_TUI)
        c.start_guardian(inject_id, FAKE_PROMPT_TUI)
        c.wait_until(lambda: c._pid(inject_id, "child_pid") is not None, timeout=8)
        c.seed("phase-planned", "PHASE PLANNED", status="exited")
        c.seed("phase-timeout", "PHASE TIMEOUT", status="exited")

        def write_control(sid, **over):
            control = {"enabled": True, "marker_version": 2, "pilot_id": None,
                       "replies": 0, "cap": 8, "phase": "Idle",
                       "main_marker": "v2:1", "pilot_marker": "",
                       "pending_reply": "", "last_reason": None,
                       "cleanup_pending": False, "phase_started_at": now}
            control.update(over)
            with open(os.path.join(c.jobs, sid, "autopilot.json"), "w") as f:
                json.dump(control, f, indent=2)

        write_control(inject_id, phase="Injecting", pending_reply="SEND EXACTLY ONCE")
        write_control("phase-planned", phase="StartingPilot", pilot_id="planned-no-job")
        write_control("phase-timeout", phase="Generating", phase_started_at=now - 30)
        c.boot(reset=False)

        def phase_controls_settled():
            try:
                values = [json.load(open(os.path.join(c.jobs, sid, "autopilot.json")))
                          for sid in (inject_id, "phase-planned", "phase-timeout")]
            except Exception:
                return False
            return all(not value.get("enabled") and value.get("phase") == "Idle"
                       for value in values)

        settled = c.wait_until(phase_controls_settled, timeout=8)
        inject = json.load(open(os.path.join(c.jobs, inject_id, "autopilot.json")))
        planned = json.load(open(os.path.join(c.jobs, "phase-planned", "autopilot.json")))
        timed = json.load(open(os.path.join(c.jobs, "phase-timeout", "autopilot.json")))
        time.sleep(1.0)
        record = json.load(open(prompt_record)) if os.path.exists(prompt_record) else {}
        duplicate_suppressed = (record.get("payload") is None and record.get("early_hex") == ""
                                and "delivery was interrupted" in (inject.get("last_reason") or "")
                                and inject.get("pending_reply") == "")
        no_orphan = (planned.get("pilot_id") is None
                     and not os.path.exists(os.path.join(c.jobs, "planned-no-job")))
        timed_out = "timed out" in (timed.get("last_reason") or "")
        check("autopilot crash phases: duplicate suppressed, planned pilot no orphan, timeout pauses",
              settled and duplicate_suppressed and no_orphan and timed_out,
              f"settled={settled} duplicate={duplicate_suppressed} no_orphan={no_orphan} "
              f"timeout={timed_out} reasons={[inject.get('last_reason'), planned.get('last_reason'), timed.get('last_reason')]}")
        state = json.load(open(os.path.join(c.jobs, inject_id, "state.json")))
        control_request(state["socket"], b"STOP\n")
    finally:
        os.environ.pop("CODEX_RAIL_PROMPT_RECORD", None)
        os.environ.pop("CODEX_RAIL_PROMPT_MODE", None)
        os.environ.pop("CODEX_RAIL_AUTOPILOT_PHASE_TIMEOUT_SECS", None)
        c.close()

    # 48) Same-cwd launches are correlated by persisted session identity, never
    #     prompt text or timing. Race different prompts, identical prompts, and
    #     an empty prompt; every guarded worker keeps its own rollout and wire.
    c = Cockpit(rail, codex=FAKE_PROMPT_TUI).boot()
    record_template = os.path.join(c.root, "multi-prompt-{pid}.json")
    os.environ["CODEX_RAIL_PROMPT_RECORD"] = record_template
    os.environ["CODEX_RAIL_PROMPT_MODE"] = "ready"
    try:
        cases = [
            ("multi-a", "resume-a", "different A"),
            ("multi-b", "resume-b", "different B"),
            ("multi-x1", "resume-x1", "same prompt"),
            ("multi-x2", "resume-x2", "same prompt"),
            ("multi-empty", "resume-empty", None),
        ]
        expected_rollouts = {}
        for index, (sid, codex_id, prompt) in enumerate(cases):
            rollout = os.path.join(c.codex_home, "sessions", "2026", "07", "12",
                                   f"rollout-2026-07-12T00-00-0{index}-{codex_id}.jsonl")
            os.makedirs(os.path.dirname(rollout), exist_ok=True)
            with open(rollout, "w") as f:
                f.write(json.dumps({"type": "session_meta",
                                    "payload": {"id": codex_id, "cwd": "/tmp"}}) + "\n")
            expected_rollouts[sid] = rollout
            c.seed(sid, f"MULTI {sid}", status="starting", codex=FAKE_PROMPT_TUI,
                   initial_prompt=prompt, codex_session_id=codex_id,
                   codex_rollout_path=rollout)
        for sid, _, _ in cases:
            c.start_guardian(sid, FAKE_PROMPT_TUI)

        def records_ready():
            paths = glob.glob(os.path.join(c.root, "multi-prompt-*.json"))
            if len(paths) != len(cases):
                return False
            records = [json.load(open(path)) for path in paths]
            by_id = {}
            for record in records:
                argv = record.get("argv", [])
                if "resume" in argv and argv.index("resume") + 1 < len(argv):
                    by_id[argv[argv.index("resume") + 1]] = record
            return (len(by_id) == len(cases)
                    and all(by_id[codex_id].get("composer_emitted") for _, codex_id, _ in cases)
                    and all(by_id[codex_id].get("postcheck_done") is True
                            for _, codex_id, prompt in cases if prompt is not None))

        ready = c.wait_until(records_ready, timeout=14)
        records = [json.load(open(path))
                   for path in glob.glob(os.path.join(c.root, "multi-prompt-*.json"))]
        by_resume = {}
        for record in records:
            argv = record.get("argv", [])
            if "resume" in argv:
                position = argv.index("resume")
                if position + 1 < len(argv):
                    by_resume[argv[position + 1]] = record
        prompt_exact = True
        state_exact = True
        for sid, codex_id, prompt in cases:
            record = by_resume.get(codex_id, {})
            prompt_exact &= record.get("payload") == prompt
            state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
            state_exact &= (state.get("codex_session_id") == codex_id
                            and state.get("codex_rollout_path") == expected_rollouts[sid]
                            and state.get("initial_prompt") is None)
        unique_rollouts = len(set(expected_rollouts.values())) == len(cases)
        check("same cwd concurrency: different/same/empty prompts keep unique rollout identity, no cross",
              ready and len(by_resume) == len(cases) and prompt_exact
              and state_exact and unique_rollouts,
              f"ready={ready} records={len(records)} mapped={sorted(by_resume)} "
              f"prompt_exact={prompt_exact} state_exact={state_exact} unique={unique_rollouts}")
        for sid, _, _ in cases:
            try:
                state = json.load(open(os.path.join(c.jobs, sid, "state.json")))
                control_request(state["socket"], b"STOP\n")
            except Exception:
                pass
    finally:
        os.environ.pop("CODEX_RAIL_PROMPT_RECORD", None)
        os.environ.pop("CODEX_RAIL_PROMPT_MODE", None)
        c.close()

    # ---- summary
    npass = sum(1 for _, ok, _ in results if ok)
    print(f"\n==== {npass}/{len(results)} checks PASS ====")
    for name, ok, _ in results:
        if not ok:
            print(f"   FAILED: {name}")
    if pngdir:
        print(f"\nPNG snapshots written to {pngdir}/")
    return all(ok for _, ok, _ in results)


if __name__ == "__main__":
    pngdir = None
    if "--png" in sys.argv:
        pngdir = sys.argv[sys.argv.index("--png") + 1]
    if "--import-only" in sys.argv:
        ok = import_audit(RAIL, pngdir)
    elif "--mouse-only" in sys.argv:
        ok = mouse_audit(RAIL, pngdir)
    elif "--visual-only" in sys.argv:
        ok = visual_audit(RAIL, pngdir)
    else:
        ok = audit(RAIL, pngdir)
    sys.exit(0 if ok else 1)
