#!/usr/bin/env python3
"""Interactive regression test: drive the real rail manager in a pty with real
keystrokes and check state.json outcomes.

  PART 1: Ctrl+R rename on a static (exited) session -> title must change.
  PART 2: Ctrl+R rename on a RUNNING session (live worker) -> title must STICK
          (not get clobbered by the worker's periodic state writes).
"""
import fcntl, json, os, pty, select, signal, struct, subprocess, sys, termios, threading, time

RAIL, FAKE = sys.argv[1], sys.argv[2]

def setup(root, sid, title, status, codex="codex"):
    data, run = root + "/data", root + "/run"
    jobs = data + "/codex-rail/jobs"
    subprocess.run(["rm", "-rf", root])
    os.makedirs(jobs + "/" + sid, exist_ok=True)
    os.makedirs(run, exist_ok=True)
    now = int(time.time())
    st = {"id": sid, "title": title, "cwd": "/tmp", "codex": codex, "status": status,
          "worker_pid": None, "child_pid": None, "socket": run + "/" + sid + ".sock",
          "created_at": now, "updated_at": now, "exit_code": 0 if status == "exited" else None,
          "last_error": None, "codex_session_id": None, "codex_rollout_path": None,
          "initial_prompt": None, "title_pinned": False, "last_output_at": 0}
    json.dump(st, open(jobs + "/" + sid + "/state.json", "w"), indent=2)
    return data, run, jobs + "/" + sid + "/state.json"

def run_manager(data, run, keys, hold=1.5):
    env = os.environ.copy()
    env.update({"XDG_DATA_HOME": data, "XDG_RUNTIME_DIR": run, "HOME": "/root",
                "CODEX_RAIL_CODEX": FAKE,
                "TERM": "xterm-256color", "COLUMNS": "90", "LINES": "24"})
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 90, 0, 0))
    p = subprocess.Popen([RAIL], stdin=s, stdout=s, stderr=s, env=env,
                         preexec_fn=os.setsid, close_fds=True)
    os.close(s)
    # Drain the pty master continuously — otherwise the manager's renders fill
    # the pty buffer, block on write, and stop reading our keystrokes (a real
    # terminal always drains, so not draining here is a harness bug).
    stop = threading.Event()
    def drain():
        while not stop.is_set():
            r, _, _ = select.select([m], [], [], 0.1)
            if m in r:
                try:
                    os.read(m, 65536)
                except OSError:
                    return
    threading.Thread(target=drain, daemon=True).start()
    time.sleep(1.5)
    for delay, payload in keys:
        os.write(m, payload)
        time.sleep(delay)
    time.sleep(hold)
    os.write(m, b'\x1b'); time.sleep(0.2); os.write(m, b'\x1b'); time.sleep(0.3)
    stop.set()
    try:
        os.killpg(os.getpgid(p.pid), signal.SIGKILL)
    except Exception:
        pass

RENAME_KEYS = [
    (0.5, b'\x12'),          # Ctrl+R
    (0.3, b'\x7f' * 24),     # clear the prefilled current title
    (0.3, None),             # placeholder, replaced below
    (0.5, b'\r'),            # Enter
]

def rename_keys(newname):
    return [(0.5, b'\x12'), (0.3, b'\x7f' * 24), (0.3, newname.encode()), (0.5, b'\r')]

# ---- PART 1: static exited session ----
data, run, sp = setup("/tmp/rg1", "a1", "alpha", "exited")
run_manager(data, run, rename_keys("renamed-alpha"))
t1 = json.load(open(sp))
p1 = t1["title"] == "renamed-alpha" and t1.get("title_pinned") is True
print("PART1 Ctrl+R on exited: title=%r pinned=%s -> %s"
      % (t1["title"], t1.get("title_pinned"), "PASS" if p1 else "FAIL"))

# ---- PART 2: running session with a live worker ----
data, run, sp = setup("/tmp/rg2", "b1", "beta", "starting", codex=FAKE)
env = os.environ.copy()
env.update({"XDG_DATA_HOME": data, "XDG_RUNTIME_DIR": run})
w = subprocess.Popen([RAIL, "--worker", "b1"], env=env,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     start_new_session=True)
time.sleep(2.0)
print("PART2 worker status before rename:", json.load(open(sp))["status"])
run_manager(data, run, rename_keys("renamed-live"), hold=1.0)
print("PART2 right after rename:", json.load(open(sp))["title"])
for i in range(5):
    time.sleep(1)
    s = json.load(open(sp))
    print("  [%ds] title=%r pinned=%s" % (i + 1, s["title"], s.get("title_pinned")))
t2 = json.load(open(sp))
p2 = t2["title"] == "renamed-live"
print("PART2 Ctrl+R on RUNNING: final title=%r -> %s"
      % (t2["title"], "PASS" if p2 else "FAIL (worker clobbered the rename)"))
try:
    os.killpg(os.getpgid(w.pid), signal.SIGKILL)
except Exception:
    pass

# ---- PART 3: create a session via `e` + first message ----
data, run, _ = setup("/tmp/rg3", "seed", "seed-session", "exited")
jobs3 = data + "/codex-rail/jobs"
run_manager(data, run, [
    (0.5, b'e'),
    (0.3, b'my new task'),
    (0.6, b'\r'),          # submit -> create + attach
    (1.2, b'\x1a'),        # Ctrl+Z -> detach back to manager
], hold=1.0)
titles3 = sorted(json.load(open(jobs3 + "/" + d + "/state.json"))["title"] for d in os.listdir(jobs3))
p3 = "my new task" in titles3
print("PART3 new session via e: titles=%s -> %s" % (titles3, "PASS" if p3 else "FAIL"))

# ---- PART 4: Space is reserved and must NOT open the composer / create ----
data, run, _ = setup("/tmp/rg4", "only", "only-session", "exited")
jobs4 = data + "/codex-rail/jobs"
run_manager(data, run, [(0.4, b' '), (0.4, b' '), (0.4, b'\r')], hold=0.8)
n4 = len(os.listdir(jobs4))
p4 = n4 == 1
print("PART4 space reserved: session count=%d -> %s" % (n4, "PASS" if p4 else "FAIL"))

# Kill only workers our own test created (by pid from their state files) — never
# pattern-kill, so we can't touch the user's real rail workers.
for root in ("/tmp/rg3", "/tmp/rg4"):
    jd = root + "/data/codex-rail/jobs"
    if os.path.isdir(jd):
        for d in os.listdir(jd):
            try:
                st = json.load(open(jd + "/" + d + "/state.json"))
                for k in ("child_pid", "worker_pid"):
                    if st.get(k):
                        try:
                            os.kill(st[k], signal.SIGKILL)
                        except Exception:
                            pass
            except Exception:
                pass
subprocess.run(["rm", "-rf", "/tmp/rg1", "/tmp/rg2", "/tmp/rg3", "/tmp/rg4"])
print("\nSUMMARY: " + " | ".join("P%d %s" % (i + 1, "PASS" if x else "FAIL")
                                 for i, x in enumerate((p1, p2, p3, p4))))
