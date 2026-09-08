"""Record unmodified gloop in a real PTY. No desktop or private project is captured."""
import codecs
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import select
import shutil
import signal
import struct
import subprocess
import termios
import time

media = pathlib.Path(__file__).resolve().parents[1]
binary = media.parent / "target/debug/gloop"
workspace = pathlib.Path("/tmp/gloop-demo")
workspace.mkdir(exist_ok=False)
(workspace / ".gloop/graphs").mkdir(parents=True)
shutil.copy(media / "scripts/sample-task.py", workspace / "task.py")
(workspace / "sample.json").write_text(json.dumps([
    {"module": name, "lint_errors": 0, "tests": count, "passed": count, "license": "Apache-2.0"}
    for name, count in [("core", 24), ("runtime", 36), ("cli", 18)]
]))
python = "python3"
graph = {
    "apiVersion": "gloop.dev/v1alpha1", "kind": "Graph",
    "metadata": {"name": "parallel-checks", "version": "1.0.0"},
    "spec": {"goal": "Three independent checks. One combined report.",
        "policies": {"max_parallel": 3},
        "budgets": {"model_calls": 0, "wall_time_seconds": 30},
        "nodes": [{"id": lane, "label": label, "kind": "command",
                   "argv": [python, "task.py", lane], "output": {"format": "text"}}
                  for lane, label in [("quality", "Code quality"), ("coverage", "Test coverage"),
                                      ("release", "Release check"), ("merge", "Combine results")]],
        "edges": [{"from": lane, "to": "merge", "kind": "data"}
                  for lane in ["quality", "coverage", "release"]]}}
(workspace / ".gloop/graphs/parallel-checks.yaml").write_text(json.dumps(graph, indent=2))
# The shell has no startup files/history or identifying prompt. gloop remains untouched.
env = dict(os.environ, TERM="xterm-256color", PS1="$ ", PROMPT="$ ", HISTFILE="/dev/null")
env["PATH"] = str(binary.parent) + ":" + env["PATH"]
pid, fd = pty.fork()
if pid == 0:
    os.chdir(workspace)
    os.execve("/bin/bash", ["bash", "--noprofile", "--norc", "-i"], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 128, 0, 0))
decoder = codecs.getincrementaldecoder("utf-8")("replace")
events = []
start = time.monotonic()
actions = [(0.8 + i * 0.14, ch.encode()) for i, ch in enumerate("gloop")]
actions += [(1.9, b"\r"), (4.5, b"\r"), (7.0, b"r"),
            (23.5, b"\x1b[B"), (24.0, b"\x1b[B"), (24.5, b"\x1b[B"),
            (31.0, b"q"), (32.5, b"q")]
try:
    while time.monotonic() - start < 34:
        elapsed = time.monotonic() - start
        while actions and elapsed >= actions[0][0]:
            _, keys = actions.pop(0)
            os.write(fd, keys)
        if select.select([fd], [], [], 0.02)[0]:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            text = decoder.decode(chunk)
            if text:
                events.append([round(time.monotonic() - start, 4), "o", text])
                # Answer terminal capability probes, without fabricating screen output.
                if "\x1b[6n" in text:
                    os.write(fd, b"\x1b[1;1R")
finally:
    os.kill(pid, signal.SIGTERM)
    os.close(fd)
    os.waitpid(pid, 0)
raw = "".join(event[2] for event in events)
for private in [str(pathlib.Path.home()), os.environ.get("USER", ""), "/Users/"]:
    if private and private in raw:
        raise RuntimeError("Private identifier detected; recording will not be saved")
capture = {"version": 2, "width": 128, "height": 32, "duration": 34,
           "title": "gloop terminal demo", "env": {"TERM": "xterm-256color"}}
(media / "public/terminal.cast").write_text("\n".join([json.dumps(capture)] + [json.dumps(e) for e in events]) + "\n")
run = next((workspace / ".gloop/runs").iterdir())
journal = [json.loads(line)["event"] for line in (run / "journal.jsonl").read_text().splitlines()]
evidence = {
    "gloop_version": subprocess.check_output([str(binary), "--version"], text=True).strip(),
    "source_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=media.parent, text=True).strip(),
    "run_id": run.name, "capture": "terminal.cast",
    "capture_sha256": hashlib.sha256((media / "public/terminal.cast").read_bytes()).hexdigest(),
    "events": [{key: event[key] for key in ["sequence", "timestamp", "node_id", "kind"] if key in event}
               for event in journal if event["kind"] in ["run_started", "node_started", "node_succeeded", "run_finished"]],
    "report": json.loads((workspace / "results/report.json").read_text()),
}
(media / "public/run-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
print(json.dumps({"events": len(events), "duration": 34, "privacy_scan": "passed",
                  "version": subprocess.check_output([str(binary), "--version"], text=True).strip()}))
