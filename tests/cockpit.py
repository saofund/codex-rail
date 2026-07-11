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
import fcntl, glob, json, os, pty, re, select, shutil, signal, struct, subprocess, sys, termios, threading, time
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
                    # 4s detach-hint progress bar -> fast in tests; raise it to watch it fill
                    "CODEX_RAIL_HINT_MS": os.environ.get("COCKPIT_HINT_MS", "60"),
                    # rescan ~/.codex for cwd-matching sessions fast so tests don't wait 20s
                    "CODEX_RAIL_ADOPT_MS": "300",
                    # don't hit GitHub for an update check during tests
                    "CODEX_RAIL_NO_UPDATE_CHECK": "1",
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
        txt = c.text()
        check("sections: empty ones hidden (1 exited -> only Stopped shows)",
              "Stopped" in counts and "Needs input" not in txt and "Working" not in txt,
              str(counts))
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

    # 9) Space reserved: does not open the composer ("Enter start" = New-mode hint)
    c = Cockpit(rail).boot()
    try:
        c.seed("sp-0", "SPACE_ROW", status="exited")
        time.sleep(0.8)
        before = len(os.listdir(c.jobs))
        c.key(b" ", 0.4)
        new_mode = "Enter start" in c.text() or "new session" in c.text()
        after = len(os.listdir(c.jobs))
        check("space: reserved (no composer, no new session)", not new_mode and after == before,
              f"new_mode={new_mode} dirs {before}->{after}")
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
            a = c.cpu_ticks(); time.sleep(1.0); b = c.cpu_ticks()
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
        c.goto("hello world"); c.key(b"\r", 2.2)      # attach again, now capped
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
        time.sleep(0.8)
        last = c.rows()[-1]
        win_keys = ("↑↓" in last) and ("Ctrl+R" in last) and ("twice" in last)
        c.key(b"\x1b", 0.5)                       # one Esc -> arm the quit confirm
        rows_now = c.rows()
        status = rows_now[-1]
        box_area = "\n".join(rows_now[-5:-1])     # the composer box, just above the status line
        on_status = "Esc again to quit" in status
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
        corpus = os.path.join(c.home, ".config", "codex-rail", "distill", "corpus")
        chunks = sorted(glob.glob(corpus + "/corpus-*.md")) if os.path.isdir(corpus) else []
        made = len(chunks) >= 1
        shown = "distill" in c.text()              # a distill session is in the list
        args_ok = False                            # ...and it carries the autonomous flags
        for d in (os.listdir(c.jobs) if os.path.isdir(c.jobs) else []):
            try:
                st = json.load(open(f"{c.jobs}/{d}/state.json"))
                ca = st.get("codex_args", [])
                if "distill" in st.get("title", "") and "workspace-write" in ca and "-C" in ca:
                    args_ok = True
            except Exception:
                pass
        # rail pre-trusts the distill dir in codex's config so the TUI session
        # doesn't stall on the first-run "trust this folder?" gate.
        cfg = os.path.join(c.home, ".codex", "config.toml")
        distill_dir = os.path.join(c.home, ".config", "codex-rail", "distill")
        trust_ok = os.path.exists(cfg) and f'[projects."{distill_dir}"]' in open(cfg).read()
        check("distill (Ctrl+D): corpus aggregated + autonomous session + dir pre-trusted",
              made and shown and args_ok and trust_ok,
              f"chunks={len(chunks)} shown={shown} args_ok={args_ok} trust_ok={trust_ok}")
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
        # a DONE distillation: idle session whose style file is already on disk
        c.seed("dist-done", "[distill v7]", distill_version=7)
        open(os.path.join(ddir, "style-v007.md"), "w").write("# style\n")
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

    # 21) slash-command palette: typing '/' in the composer opens a command menu
    #     that filters as you type and runs the command rail-side (not sent to codex).
    c = Cockpit(rail).boot()
    try:
        c.seed_codex_history(n_sessions=1, msgs_each=2)  # so /distill has input
        c.key(b"/", 0.4)                                  # open the palette
        palette = all(cmd in c.text() for cmd in ("/distill", "/update", "/config", "/help"))
        c.key(b"di", 0.3)                                 # filter -> only /distill
        filtered = "/distill" in c.text() and "/update" not in c.text()
        c.key(b"\r", 2.5)                                 # Enter -> run /distill (like Ctrl+D)
        corpus = os.path.join(c.home, ".config", "codex-rail", "distill", "corpus")
        ran = os.path.isdir(corpus) and len(glob.glob(corpus + "/corpus-*.md")) >= 1
        check("slash palette: / opens + filters commands and runs one (/distill)",
              palette and filtered and ran, f"palette={palette} filtered={filtered} ran={ran}")
        snap(c, "21_slash")
    finally:
        c.close()

    # 22) mouse: moving the pointer must NOT change the selection (the hover-select
    #     bug jerked a scrolled list back to the top), and the wheel scrolls it.
    c = Cockpit(rail).boot()
    try:
        for i in range(6):
            c.seed(f"ms{i}", f"mouse-sess-{i}", status="exited")
        c.wait_until(lambda: c.row_with("mouse-sess-0") is not None, timeout=6)
        # Compare the selected SESSION, not the whole row (which contains a ticking
        # age column that would otherwise look like a change).
        def sel():
            m = re.search(r"mouse-sess-\d", c.selected_row() or "")
            return m.group(0) if m else None
        c.key(b"\x1b[B", 0.2); c.key(b"\x1b[B", 0.2)   # Down twice
        before = sel()
        c.key(b"\x1b[<35;20;5M", 0.3)                  # SGR mouse MOVE over a different row
        move_ok = sel() == before                       # selected session unchanged
        c.key(b"\x1b[<65;20;10M", 0.3)                 # SGR wheel scroll-down
        scroll_ok = sel() != before                     # selection moved to a different session
        check("mouse: move doesn't hijack selection; wheel scrolls it",
              move_ok and scroll_ok,
              f"move_ok={move_ok} scroll_ok={scroll_ok} before={before!r}")
    finally:
        c.close()

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

    # 25) Space toggles autopilot on the selected session: an "⟳ auto N/cap" badge
    #     and an "autopilot ON" status appear; Space again turns it off.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        c.seed_running_worker("ap-1", "AUTOPILOT ME")
        c.wait_until(lambda: c.row_with("AUTOPILOT ME") is not None, timeout=8)
        c.key(b" ", 0.5)                                   # Space -> autopilot ON
        on_badge = any("auto 0/" in r for r in c.rows())
        on_status = any("autopilot on" in r.lower() for r in c.rows())
        c.key(b" ", 0.5)                                   # Space -> OFF
        off_badge = not any("auto 0/" in r for r in c.rows())
        off_status = any("autopilot off" in r.lower() for r in c.rows())
        check("autopilot: Space toggles it (⟳ badge + status on, then off)",
              on_badge and on_status and off_badge and off_status,
              f"on={on_badge}/{on_status} off={off_badge}/{off_status}")
    finally:
        c.close()

    # 26) a live session whose socket is GONE (e.g. XDG_RUNTIME_DIR was cleared
    #     while the worker stayed alive) can still be stopped — Ctrl+X twice falls
    #     back to killing the worker by its recorded pid instead of wedging.
    c = Cockpit(rail, codex=FAKE_SLEEP).boot()
    try:
        c.seed_running_worker("sock-gone", "STUCK")
        c.wait_until(lambda: c.row_with("STUCK") is not None, timeout=8)
        child = c._pid("sock-gone", "child_pid")
        sock = json.load(open(f"{c.jobs}/sock-gone/state.json"))["socket"]
        if sock and os.path.exists(sock):
            os.remove(sock)                                  # vanish the socket
        c.key(b"\x18", 0.5); c.key(b"\x18", 1.2)             # Ctrl+X twice -> stop
        def _alive(p):
            try: os.kill(p, 0); return True
            except OSError: return False
        killed = child is not None and not _alive(child)
        check("stop: socket-gone live session is killed by the pid fallback",
              killed, f"child={child} killed={killed}")
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
