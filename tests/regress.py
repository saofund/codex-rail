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
    F  Working status latches through a long silent turn (incremental lifecycle scan)
    G  idle manager does zero full-screen clears (line-diff renderer, no flicker)
    H  attach then detach repaints the manager (no blank screen)
    I  Ctrl-X twice removes a stopped session from the list and disk
    J  a zombie worker is treated as stopped (not alive) and is removable

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
                    "CODEX_RAIL_HINT_MS": "60",  # 4s detach-hint progress bar -> fast in tests
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
        self.raw = bytearray()
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
                    self.raw += chunk
                    self.stream.feed(chunk)

    def mark(self):
        with self.lock:
            return len(self.raw)

    def raw_since(self, mark):
        with self.lock:
            return bytes(self.raw[mark:])

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


# ---- F: Working latches through a silent gap (the mtime bug) ----
def test_f():
    print("\n== F: Working status latches through a silent gap ==")
    sid = "019f25a9-adfd-7b43-a6f7-f3682b46ed54"
    meta = {"type": "session_meta", "payload": {"session_id": sid, "id": sid, "cwd": "/tmp/cw"}}
    started = {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t"}}
    complete = {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t",
                                                 "last_agent_message": "done"}}
    start = sid_start_secs(sid)
    data, run, jobs, sp = setup("w1", "worker", cwd="/tmp/cw", created_at=start,
                                codex_session_id=sid)
    chome = ROOT + "/ch/sessions/2026/07/03"
    os.makedirs(chome, exist_ok=True)
    rp = chome + f"/rollout-2026-07-03T09-48-25-{sid}.jsonl"
    open(rp, "w").write("".join(json.dumps(x) + "\n" for x in (meta, started)))
    # Point the session at this rollout.
    st = json.load(open(sp)); st["codex_rollout_path"] = rp; write_state(sp, st)

    # Count sessions in a named section. Empty sections are now hidden, so an
    # absent section reads as None (not 0) — which is exactly how we assert the
    # session LEFT Working and arrived in Needs input after task_complete.
    def scount(mgr, name):
        for r in mgr.rows():
            if name in r:
                toks = r.replace("✻", " ").replace("●", " ").replace("○", " ").split()
                for t in reversed(toks):
                    if t.isdigit():
                        return int(t)
                return None
        return None

    mgr = Manager(data, run, codex_home=ROOT + "/ch")
    try:
        time.sleep(1.2); c0 = scount(mgr, "Working")
        time.sleep(8.0); c8 = scount(mgr, "Working")      # 8s of silence (> old 6s threshold)
        with open(rp, "a") as f: f.write(json.dumps(complete) + "\n")
        time.sleep(1.5)
        wf = scount(mgr, "Working")                       # now empty -> hidden -> None
        nf = scount(mgr, "Needs input")                   # session moved here -> 1
        ok = c0 == 1 and c8 == 1 and wf is None and nf == 1
        print(f"   Working: t0={c0} t8s(silent)={c8} after_complete: Working={wf} NeedsInput={nf}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close()


# ---- G: idle renders emit no full-screen clears (flicker) ----
def test_g():
    print("\n== G: idle emits no full-screen clears ==")
    data, run, jobs, _ = setup("idle", "idle-session", status="exited")
    mgr = Manager(data, run)
    try:
        time.sleep(1.0)
        mark = mgr.mark()
        time.sleep(4.0)                                   # ~5 refresh cycles, no interaction
        chunk = mgr.raw_since(mark)
        clears = chunk.count(b"\x1b[2J")                  # Clear(ClearType::All)
        ok = clears == 0
        print(f"   idle bytes={len(chunk)} full_clears={clears}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close()


# ---- H: create -> attach -> detach repaints the list (diff-render round-trip) ----
def test_h():
    print("\n== H: attach then detach repaints the manager (no blank screen) ==")
    data, run, jobs, _ = setup("seed", "seed-session", status="exited")
    mgr = Manager(data, run)
    try:
        # `e` + text creates a session, spawns a real worker (fakecodex), and
        # auto-attaches. fakecodex prints "tick ..." so we can see the attach.
        mgr.send(b"e", 0.4); mgr.send(b"tick-test", 0.3); mgr.send(b"\r", 2.5)
        attached_ok = any("tick" in r for r in mgr.rows())
        # Ctrl+Z detaches back to the manager, which must fully repaint.
        mgr.send(b"\x1a", 1.5)
        rows = mgr.rows()
        repainted = any("Codex Rail" in r for r in rows) and any("tick-test" in r for r in rows)
        ok = attached_ok and repainted
        print(f"   attach showed codex output={attached_ok}  list repainted after detach={repainted}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close(); kill_children(jobs)


# ---- I: Ctrl-X twice removes a stopped session from the list ----
def test_i():
    print("\n== I: Ctrl-X twice removes a stopped session ==")
    data, run, jobs, sp = setup("gone", "REMOVE_ME", status="exited")
    mgr = Manager(data, run)
    try:
        listed_before = mgr.row_with("REMOVE_ME") is not None
        mgr.send(b"\x18", 0.4)                       # first Ctrl-X: arm + confirm prompt
        confirm = "remove this stopped" in mgr.text()
        mgr.send(b"\x18", 0.8)                       # second Ctrl-X: remove
        time.sleep(0.6)
        gone_screen = mgr.row_with("REMOVE_ME") is None
        dir_gone = not os.path.exists(jobs + "/gone")
        ok = listed_before and confirm and gone_screen and dir_gone
        print(f"   listed_before={listed_before} confirm_prompt={confirm} "
              f"gone_from_screen={gone_screen} dir_deleted={dir_gone}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close()


# ---- J: a zombie worker (defunct, unreaped) is treated as stopped, not alive ----
def test_j():
    print("\n== J: zombie worker reconciles to Stopped and is removable ==")
    zpid = os.fork()
    if zpid == 0:
        os._exit(0)                                  # exits at once -> zombie (never reaped here)
    time.sleep(0.2)
    stt = open(f"/proc/{zpid}/stat").read()
    zstate = stt[stt.rfind(") ") + 2:].strip()[0]    # sanity: kernel state char is 'Z'
    data, run, jobs, sp = setup("zomb", "ZOMBIE_ROW",
                                status="running", worker_pid=zpid, child_pid=99999999)
    mgr = Manager(data, run)
    try:
        row = mgr.row_with("ZOMBIE_ROW") or ""
        reconciled = "○" in row                 # ○ hollow = Stopped bucket (kill(pid,0) alone would say alive)
        mgr.send(b"\x18", 0.4); mgr.send(b"\x18", 0.8); time.sleep(0.6)
        removed = mgr.row_with("ZOMBIE_ROW") is None and not os.path.exists(jobs + "/zomb")
        ok = zstate == "Z" and reconciled and removed
        print(f"   proc_state={zstate!r} reconciled_to_stopped={reconciled} removable={removed}")
        print("   ", "PASS" if ok else "FAIL")
        return ok
    finally:
        mgr.close()
        try:
            os.waitpid(zpid, 0)
        except Exception:
            pass


if __name__ == "__main__":
    results = {}
    for name, fn in [("A", test_a), ("B", test_b), ("C", test_c), ("D", test_d),
                     ("E", test_e), ("F", test_f), ("G", test_g), ("H", test_h),
                     ("I", test_i), ("J", test_j)]:
        try:
            results[name] = fn()
        except Exception as e:
            print(f"   EXCEPTION in {name}: {e}")
            results[name] = False
    shutil.rmtree(ROOT, ignore_errors=True)
    print("\nSUMMARY: " + " | ".join(f"{k} {'PASS' if v else 'FAIL'}" for k, v in results.items()))
    sys.exit(0 if all(results.values()) else 1)
