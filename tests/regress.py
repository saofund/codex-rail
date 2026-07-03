#!/usr/bin/env python3
"""End-to-end regression tests for the rail manager.

These drive the REAL rail binary in a pty and read the REAL rendered screen
(via pyte) plus the on-disk state — no mocked internals. They exist because an
earlier round of "unit" checks passed while the shipped binary was still broken:
the bugs lived in the manager<->worker file interaction and in parsing real
codex rollouts, neither of which a fake could exercise.

    A  rename survives a clobbering worker   (title lives in manager-only label.json)
    B  message preview recovered from a real-format rollout (cwd+start correlation)
    C  `e` + text creates a session whose title is stored in label.json
    D  Space is reserved and never opens the composer
    E  rename on an idle/exited session sticks

Usage:  python3 tests/regress.py ./target/release/rail [./tests/fakecodex]
"""
import fcntl, json, os, pty, select, shutil, signal, struct, subprocess, sys, termios, threading, time
import pyte

HERE = os.path.dirname(os.path.abspath(__file__))
RAIL = sys.argv[1] if len(sys.argv) > 1 else "./target/release/rail"
FAKE = sys.argv[2] if len(sys.argv) > 2 else os.path.join(HERE, "fakecodex")
ROOT = "/tmp/rail-regress"
COLS, ROWS = 110, 40


def sid_start_secs(session_id):
    return int(session_id.replace("-", "")[:12], 16) // 1000


def write_state(sp, st, old_schema=False):
    s = dict(st)
    if old_schema:  # a worker build that predates these fields
        for k in ("title_pinned", "initial_prompt"):
            s.pop(k, None)
    tmp = sp + ".tmp"
    json.dump(s, open(tmp, "w"), indent=2)
    os.replace(tmp, sp)


def base_state(sid, title, cwd, **over):
    now = int(time.time())
    st = {"id": sid, "title": title, "cwd": cwd, "codex": FAKE, "status": "running",
          "worker_pid": None, "child_pid": None, "socket": f"{ROOT}/run/{sid}.sock",
          "created_at": now, "updated_at": now, "exit_code": None, "last_error": None,
          "codex_session_id": None, "codex_rollout_path": None, "initial_prompt": None,
          "title_pinned": False, "last_output_at": 0}
    st.update(over)
    return st


class Manager:
    def __init__(self, data, run, codex_home=None):
        env = os.environ.copy()
        env.update({"XDG_DATA_HOME": data, "XDG_RUNTIME_DIR": run, "HOME": ROOT + "/home",
                    "CODEX_RAIL_CODEX": FAKE, "TERM": "xterm-256color",
                    "COLUMNS": str(COLS), "LINES": str(ROWS)})
        if codex_home:
            env["CODEX_HOME"] = codex_home
        os.makedirs(ROOT + "/home", exist_ok=True)
        self.m, s = pty.openpty()
        fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.p = subprocess.Popen([RAIL], stdin=s, stdout=s, stderr=s, env=env,
                                  preexec_fn=os.setsid, close_fds=True)
        os.close(s)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        threading.Thread(target=self._drain, daemon=True).start()
        time.sleep(1.6)

    def _drain(self):
        while not self.stopped.is_set():
            r, _, _ = select.select([self.m], [], [], 0.1)
            if self.m in r:
                try:
                    chunk = os.read(self.m, 65536)
                except OSError:
                    return
                with self.lock:
                    self.stream.feed(chunk)

    def send(self, payload, settle=0.4):
        os.write(self.m, payload)
        time.sleep(settle)

    def rows(self):
        with self.lock:
            return [r.rstrip() for r in self.screen.display]

    def text(self):
        return "\n".join(self.rows())

    def row_with(self, needle):
        return next((r for r in self.rows() if needle in r), None)

    def close(self):
        self.stopped.set()
        try:
            os.killpg(os.getpgid(self.p.pid), signal.SIGKILL)
        except Exception:
            pass


def kill_children(jobs):
    if not os.path.isdir(jobs):
        return
    for d in os.listdir(jobs):
        try:
            st = json.load(open(jobs + "/" + d + "/state.json"))
            for k in ("child_pid", "worker_pid"):
                if st.get(k):
                    try:
                        os.kill(st[k], signal.SIGKILL)
                    except Exception:
                        pass
        except Exception:
            pass


def setup(sid, title, **over):
    old_schema = over.pop("old_schema", False)
    cwd = over.pop("cwd", "/tmp")
    data, run = ROOT + "/data", ROOT + "/run"
    jobs = data + "/codex-rail/jobs"
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(jobs + "/" + sid, exist_ok=True)
    os.makedirs(run, exist_ok=True)
    sp = jobs + "/" + sid + "/state.json"
    write_state(sp, base_state(sid, title, cwd, **over), old_schema=old_schema)
    return data, run, jobs, sp


# ---- A: rename survives a clobbering worker ----
def test_a():
    print("\n== A: rename sticks against a clobbering worker ==")
    data, run, jobs, sp = setup("clob", "hi", old_schema=True)
    st = base_state("clob", "hi", "/tmp")
    stop = threading.Event(); n = [0]
    def clobber():
        while not stop.is_set():
            st["updated_at"] = int(time.time())
            write_state(sp, st, old_schema=True); n[0] += 1
            time.sleep(0.4)
    threading.Thread(target=clobber, daemon=True).start()
    mgr = Manager(data, run)
    try:
        mgr.send(b"\x12", 0.5); mgr.send(b"\x7f" * 24, 0.3)
        mgr.send(b"hi2", 0.3); mgr.send(b"\r", 0.6)
        time.sleep(4.0)
        lp = jobs + "/clob/label.json"
        label = json.load(open(lp)) if os.path.exists(lp) else None
        disk = json.load(open(sp))
        ok = (mgr.row_with("hi2") is not None and label == {"title": "hi2", "title_pinned": True}
              and n[0] > 3 and "title_pinned" not in disk)
        print(f"   clobber writes={n[0]}  label={label}  state.title={disk.get('title')!r}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        stop.set(); mgr.close()


# ---- B: preview recovered from a real-format rollout ----
def test_b():
    print("\n== B: preview recovered from a real-format rollout ==")
    fix = os.path.join(HERE, "fixtures",
                       "rollout-2026-07-02T13-51-07-019f2161-87b9-76e2-b7d3-64edc6cda7d1.jsonl")
    start = sid_start_secs("019f2161-87b9-76e2-b7d3-64edc6cda7d1")
    data, run, jobs, sp = setup("orphan", "review",
                                cwd="/tmp/rail-fixture-cwd", created_at=start - 20)
    chome = ROOT + "/codexhome/sessions/2026/07/02"
    os.makedirs(chome, exist_ok=True)
    shutil.copy(fix, chome + "/" + os.path.basename(fix))
    mgr = Manager(data, run, codex_home=ROOT + "/codexhome")
    try:
        time.sleep(1.2)
        row = mgr.row_with("review")
        ok = row is not None and "PREVIEW_OK" in row
        print(f"   row={row.strip()!r}" if row else "   row MISSING")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close()


# ---- C: `e` + text creates a session titled via label.json ----
def test_c():
    print("\n== C: new session via `e` stores title in label.json ==")
    data, run, jobs, _ = setup("seed", "seed-session", status="exited")
    mgr = Manager(data, run)
    try:
        mgr.send(b"e", 0.4); mgr.send(b"my new task", 0.3)
        mgr.send(b"\r", 1.2); mgr.send(b"\x1a", 1.0)  # submit, then Ctrl+Z detach
        titles = []
        for d in os.listdir(jobs):
            lp = jobs + "/" + d + "/label.json"
            if os.path.exists(lp):
                titles.append(json.load(open(lp)).get("title"))
        ok = "my new task" in titles
        print(f"   label titles={titles}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close(); kill_children(jobs)


# ---- D: Space is reserved ----
def test_d():
    print("\n== D: Space reserved (no composer, no create) ==")
    data, run, jobs, _ = setup("only", "only-session", status="exited")
    mgr = Manager(data, run)
    try:
        mgr.send(b" ", 0.3); mgr.send(b" ", 0.3); mgr.send(b"\r", 0.5)
        n = len(os.listdir(jobs))
        ok = n == 1
        print(f"   session count={n}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close(); kill_children(jobs)


# ---- E: rename an idle/exited session ----
def test_e():
    print("\n== E: rename on an idle/exited session sticks ==")
    data, run, jobs, sp = setup("stat", "alpha", status="exited")
    mgr = Manager(data, run)
    try:
        mgr.send(b"\x12", 0.5); mgr.send(b"\x7f" * 24, 0.3)
        mgr.send("renamed-alpha".encode(), 0.3); mgr.send(b"\r", 0.6)
        time.sleep(0.6)
        lp = jobs + "/stat/label.json"
        label = json.load(open(lp)) if os.path.exists(lp) else None
        ok = mgr.row_with("renamed-alpha") is not None and label == {"title": "renamed-alpha", "title_pinned": True}
        print(f"   label={label}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close()


if __name__ == "__main__":
    results = {}
    for name, fn in [("A", test_a), ("B", test_b), ("C", test_c), ("D", test_d), ("E", test_e)]:
        try:
            results[name] = fn()
        except Exception as e:
            print(f"   EXCEPTION in {name}: {e}")
            results[name] = False
    shutil.rmtree(ROOT, ignore_errors=True)
    print("\nSUMMARY: " + " | ".join(f"{k} {'PASS' if v else 'FAIL'}" for k, v in results.items()))
    sys.exit(0 if all(results.values()) else 1)
