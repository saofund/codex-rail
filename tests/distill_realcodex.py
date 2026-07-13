#!/usr/bin/env python3
"""Real-codex verification of archive distillation's core promise: that the
launched codex session reads the ENTIRE aggregated corpus (not a grep-sample).

It builds a small synthetic CODEX_HOME with a recognizable user "voice", runs
`rail --distill-prepare` (forcing tiny chunks so a handful of messages still
spans several files), then runs a REAL `codex exec` with the exact prompt rail
would use. Success requires the same exact machine-readable Coverage contract
as the worker: each per-run marker exactly once and the exact USER-turn count.

    python3 tests/distill_realcodex.py [./target/release/rail]

Slow (a real codex run) — not part of the fast cockpit/regress suites.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RAIL = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.join(REPO, "target/release/rail")
JOBTMP = os.environ.get("CLAUDE_JOB_DIR", "/tmp")
ROOT = os.path.join(JOBTMP, "tmp", "distill-rc-" + str(os.getpid()))
SYNTH_CODEX = os.path.join(ROOT, "codex-home")   # fake ~/.codex with synthetic sessions
CFG = os.path.join(ROOT, "config")               # XDG_CONFIG_HOME -> workdir under here
CODEX = shutil.which("codex") or "/usr/local/bin/codex"

# A deliberately recognizable voice so we can eyeball the summary too: terse,
# imperative, code-switching, impatient, sign-offs.
VOICE = [
    "直接开始，别问我 yes/no，我批准你执行。pls just do it, don't overthink. thx",
    "这个不对，你重新想想。要有个通用的方案，后面还要复用。别搞一次性的东西",
    "先提交并 push，然后再改。我要出门了，你自己回归测试，全程都是 yes",
    "很好的东西。但是你要测试它是否真的 work，不要只是说 it should work",
    "我建议你显式的先汇总一波，确保完整读取，别偷懒 grep。搞个进度条更符合预期",
    "参考下别的项目的做法，别闭门造车。做完给我一个 best-state 的可用产品",
]

def write_synth_sessions(n_sessions=3, msgs_each=6):
    """Create sessions/YYYY/MM/DD/rollout-*.jsonl with event_msg user_message lines."""
    total = 0
    for si in range(n_sessions):
        day = f"{(si % 28) + 1:02d}"
        d = os.path.join(SYNTH_CODEX, "sessions", "2026", "07", day)
        os.makedirs(d, exist_ok=True)
        sid = f"0000000{si}-aaaa-bbbb-cccc-00000000000{si}"
        path = os.path.join(d, f"rollout-2026-07-{day}T12-0{si}-00-{sid}.jsonl")
        lines = [json.dumps({"timestamp": "t", "type": "session_meta",
                             "payload": {"id": sid, "session_id": sid, "cwd": "/tmp"}})]
        for mi in range(msgs_each):
            txt = VOICE[(si * msgs_each + mi) % len(VOICE)] + f"  (s{si}m{mi})"
            lines.append(json.dumps({"timestamp": "t", "type": "event_msg",
                                     "payload": {"type": "user_message", "message": txt}}))
            # noise the filter must drop
            lines.append(json.dumps({"timestamp": "t", "type": "event_msg",
                                     "payload": {"type": "agent_message", "message": "ok, working on it " * 20}}))
            total += 1
        open(path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
    return total

def run(cmd, env=None, timeout=600, cwd=None):
    return subprocess.run(cmd, env=env, cwd=cwd, timeout=timeout,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT, exist_ok=True)
    n_msgs = write_synth_sessions()

    # 1) aggregate — tiny chunks so a few messages still span multiple files
    env = os.environ.copy()
    env.update({"CODEX_HOME": SYNTH_CODEX, "XDG_CONFIG_HOME": CFG,
                "CODEX_RAIL_DISTILL_CHUNK_BYTES": "900", "CODEX_RAIL_DISTILL_MSG_CAP": "700"})
    prep = run([RAIL, "--distill-prepare"], env=env, timeout=60)
    print(prep.stdout.rstrip())
    if prep.returncode != 0:
        print("FAIL: --distill-prepare errored")
        return 1
    markers = re.findall(r"^  corpus-\d+\.md id=([0-9a-f]{8})$",
                         prep.stdout, flags=re.MULTILINE)
    corpus_m = re.search(r"available -> (\S+/runs/run-[^/]+/corpus)$",
                         prep.stdout, flags=re.MULTILINE)
    out_m = re.search(r"^next output: (\S+/style-v\d+\.md)$",
                      prep.stdout, flags=re.MULTILINE)
    prompt_m = re.search(r"^prompt: (\S+)$", prep.stdout, flags=re.MULTILINE)
    turns_m = re.search(r"· (\d+) user turns ·", prep.stdout)
    if not all((corpus_m, out_m, prompt_m, turns_m)):
        print("FAIL: could not parse sealed --distill-prepare output")
        return 1
    corpus = corpus_m.group(1)
    out_path = out_m.group(1)
    workdir = os.path.dirname(out_path)
    prompt_path = prompt_m.group(1)
    expected_turns = int(turns_m.group(1))
    prompt = open(prompt_path, encoding="utf-8").read()
    n_chunks = len(markers)
    print(f"\nprepared: {n_chunks} chunks, {n_msgs} synthetic msgs, "
          f"expected_turns={expected_turns}, corpus={corpus}, workdir={workdir}")
    if n_chunks < 3:
        print(f"FAIL: expected >=3 chunks to test multi-file reading, got {n_chunks}")
        return 1
    if not os.path.isdir(corpus):
        print(f"FAIL: immutable run corpus is missing: {corpus}")
        return 1

    # 2) REAL codex reads the whole corpus and writes the style file
    print("\nrunning real codex exec (this takes a while) …")
    t0 = time.time()
    cx = run([CODEX, "exec", "-C", workdir, "--skip-git-repo-check",
              "-s", "workspace-write",
              "-c", f'projects."{workdir}".trust_level="trusted"',
              prompt],
             timeout=1200)  # real model latency over several chunks
    dt = time.time() - t0
    print(f"codex exec done in {dt:.0f}s, rc={cx.returncode}")
    tail = cx.stdout[-800:]
    print("---- codex tail ----\n" + tail + "\n--------------------")

    # 3) verify: the style file exists and echoes back EVERY chunk marker
    if not os.path.exists(out_path):
        print(f"FAIL: {out_path} was not written")
        return 1
    style = open(out_path, encoding="utf-8").read()
    lines = style.splitlines()
    coverage_indexes = [i for i, line in enumerate(lines) if line.strip() == "## Coverage"]
    coverage = lines[coverage_indexes[-1] + 1:] if coverage_indexes else []
    found = [line.strip().removeprefix("CHUNK_ID=") for line in coverage
             if line.strip().startswith("CHUNK_ID=")]
    turn_claims = [line.strip().removeprefix("USER_TURNS_READ=") for line in coverage
                   if line.strip().startswith("USER_TURNS_READ=")]
    exact_markers = (len(found) == len(markers)
                     and len(set(found)) == len(found)
                     and set(found) == set(markers))
    exact_turns = turn_claims == [str(expected_turns)]
    missing = sorted(set(markers) - set(found))
    has_sections = sum(k.lower() in style.lower() for k in
                       ("voice", "tone", "feedback", "instruction", "coverage")) >= 3
    print(f"\nstyle file: {len(style)} chars, sections_ok={has_sections}")
    print(f"coverage markers={found} missing={missing} exact={exact_markers}")
    print(f"turn claims={turn_claims} expected={expected_turns} exact={exact_turns}")
    ok = exact_markers and exact_turns and has_sections
    print("\n" + ("PASS — exact full-corpus Coverage contract satisfied"
                  if ok else "FAIL — incomplete/duplicate/wrong Coverage or thin summary"))
    # leave ROOT for inspection on failure; clean on success
    if ok:
        shutil.rmtree(ROOT, ignore_errors=True)
    return 0 if ok else 1

if __name__ == "__main__":
    sys.exit(main())
