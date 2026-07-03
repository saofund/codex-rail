#!/usr/bin/env python3
"""Rail Cockpit — a comprehensive, time-aware, *visual* test harness.

It drives the REAL `rail` binary in a pty, reads the REAL rendered screen (pyte),
can render that screen to a PNG so a human (or I) can actually SEE it — colours,
CJK alignment, layout — operate every feature with arbitrary input at any time,
and sample the screen over time to catch things a single snapshot can't (a
flickering age column, a status that never latches, a repaint that leaves junk).

    python3 tests/cockpit.py [./target/release/rail] [--png OUTDIR]

With no flags it runs the full feature audit and prints a PASS/FAIL table. The
Cockpit class is also importable for ad-hoc driving:

    c = Cockpit(RAIL); c.boot(); c.new("hello"); c.png("after_new.png"); c.quit()
"""
import fcntl, glob, json, os, pty, select, shutil, signal, struct, subprocess, sys, termios, threading, time
import pyte

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
RAIL = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("-") \
       else os.path.join(REPO, "target/release/rail")
JOBTMP = os.environ.get("CLAUDE_JOB_DIR", "/tmp") + "/tmp" if os.environ.get("CLAUDE_JOB_DIR") \
         else "/data/wuyitao/folder_113_train/.docker/home/.claude/jobs/ea952819/tmp"
FAKE_STREAM = os.path.join(HERE, "fakecodex")            # streams "tick" forever (a live turn)
FAKE_SLEEP = JOBTMP + "/fakecodex_sleep.sh"              # quiet long-lived child

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
    def __init__(self, rail=RAIL, root=None, codex=FAKE_SLEEP):
        self.rail = os.path.abspath(rail)      # workers spawn with cwd=/tmp; must be absolute
        # Root MUST be short: the worker binds $XDG_RUNTIME_DIR/codex-rail/<id>.sock
        # and a Unix socket path over ~108 bytes (SUN_LEN) fails to bind. A long
        # root (e.g. under the job tmp dir) makes every worker fail with
        # "path must be shorter than SUN_LEN" — which reads as a fake rail bug.
        self.root = root or ("/tmp/rc-" + str(os.getpid()))
        self.codex = codex
        self.data = self.root + "/data"
        self.run = self.root + "/run"
        self.home = self.root + "/home"
        self.jobs = self.data + "/codex-rail/jobs"
        self.p = None
        self.m = None
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self._zombies = []

    # ---- lifecycle -----------------------------------------------------------
    def boot(self, settle=1.8):
        shutil.rmtree(self.root, ignore_errors=True)
        for d in (self.jobs, self.run, self.home):
            os.makedirs(d, exist_ok=True)
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": self.data, "XDG_RUNTIME_DIR": self.run, "HOME": self.home,
                    "CODEX_RAIL_CODEX": self.codex, "TERM": "xterm-256color",
                    "COLUMNS": str(COLS), "LINES": str(ROWS)})
        self.m, s = pty.openpty()
        fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
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
        shutil.rmtree(self.root, ignore_errors=True)

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
        cw, ch = 9, 19
        mono = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 15)
        try:
            cjk = ImageFont.truetype("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 16)
        except Exception:
            cjk = mono
        bg0 = (24, 22, 26)
        img = Image.new("RGB", (COLS * cw, ROWS * ch), bg0)
        d = ImageDraw.Draw(img)
        with self.lock:
            buf = self.screen.buffer
        col = 0
        for y in range(ROWS):
            x = 0
            while x < COLS:
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
              "worker_pid": None, "child_pid": None, "socket": f"{self.run}/{sid}.sock",
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

    def seed_running_worker(self, sid, title, codex=None):
        """Spawn a REAL worker (fake codex) so status=running with live pids."""
        codex = codex or FAKE_STREAM
        self.seed(sid, title, status="starting", codex=codex)
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": self.data, "XDG_RUNTIME_DIR": self.run, "HOME": self.home,
                    "CODEX_RAIL_CODEX": codex})
        subprocess.Popen([self.rail, "--worker", sid], cwd="/tmp", env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.wait_until(lambda: self._pid(sid, "child_pid") is not None, timeout=8)
        return sid

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
        self.key(b"s")

    def up(self):
        self.key(b"w")

    def goto(self, title, tries=30):
        for _ in range(tries):
            sel = self.selected_row() or ""
            if title in sel:
                return True
            self.key(b"s", 0.12)
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
        self.key(b"e", 0.4)
        if msg:
            self.type(msg, 0.3)
        self.key(b"\r", 2.0)


# ============================ FEATURE AUDIT ====================================
def audit(rail, pngdir=None):
    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f"  — {detail}" if detail else ""))

    def snap(c, tag):
        if pngdir:
            c.png(f"{pngdir}/{tag}.png")

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
        check("sections: all 3 shown once a session exists",
              set(counts) >= {"Needs input", "Working", "Stopped"}, str(counts))
        snap(c, "01b_sections")
    finally:
        c.close()

    # 2) new session via `e` starts a real turn; title stored; auto-attach shows output
    c = Cockpit(rail, codex=FAKE_STREAM).boot()
    try:
        c.new("investigate the thing")
        attached = any("tick" in r for r in c.rows())
        snap(c, "02_attached")
        c.key(b"\x1a", 1.0)                      # Ctrl-Z detach
        listed = c.row_with("investigate the thing") is not None
        labels = [json.load(open(f"{c.jobs}/{d}/label.json")).get("title")
                  for d in os.listdir(c.jobs) if os.path.exists(f"{c.jobs}/{d}/label.json")]
        check("new session: turn started (codex output seen on attach)", attached)
        check("new session: detach repaints list", listed and any("Codex Rail" in r for r in c.rows()))
        check("new session: title saved to label.json", "investigate the thing" in labels, str(labels))
        snap(c, "03_after_detach")
    finally:
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

    # 9) Space reserved: does not open the composer ("Enter to start" = New-mode border)
    c = Cockpit(rail).boot()
    try:
        c.seed("sp-0", "SPACE_ROW", status="exited")
        time.sleep(0.8)
        before = len(os.listdir(c.jobs))
        c.key(b" ", 0.4)
        new_mode = "Enter to start" in c.text()
        after = len(os.listdir(c.jobs))
        check("space: reserved (no composer, no new session)", not new_mode and after == before,
              f"new_mode={new_mode} dirs {before}->{after}")
    finally:
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
    ok = audit(RAIL, pngdir)
    sys.exit(0 if ok else 1)
