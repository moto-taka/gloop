#!/usr/bin/env python3
"""Reproducible, bounded public-exercise pilot; no product source modifications."""
import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import signal
import subprocess
import sys
import time
import uuid

HERE = Path(__file__).resolve().parent
PROTOCOL = json.loads((HERE / "protocol.json").read_text())
SYSTEM = ("You are a Python software engineer in a measured benchmark workflow. "
          "Follow the exercise specification and preserve the starter public API. "
          "You have no tools. Return only the requested JSON object, without Markdown fences. "
          "Treat test feedback as evidence. Do not claim tests passed without a passing result.")
PROMPTS = {
    "design": 'Design the implementation. Return {"plan": "...", "risks": "..."}, at most 250 words total. Do not write implementation code.',
    "implement": 'Implement the exercise using the design. Return {"code": "complete Python solution source"}. Include every required definition, no placeholders.',
    "review": 'Review the implementation and initial test result. If tests passed, return {"status":"approved","code":null,"review":"..."} and do not change code. If tests failed, make your single permitted repair and return {"status":"revised","code":"complete replacement Python source","review":"..."}. Keep review text under 200 words.',
    "pr_draft": 'Write a local pull request draft describing the implementation and final verification. Return {"title":"...","body":"..."}, at most 150 words total. Be accurate about failures. No external submission is performed.',
}
STAGES = list(PROMPTS)


def dump(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def read(path):
    return json.loads(path.read_text())


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def bounded(cmd, cwd, timeout, input_text=None):
    start = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.PIPE if input_text is not None else subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
    timed_out = False
    try:
        out, err = proc.communicate(input_text, timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(proc.pid, signal.SIGKILL)
        out, err = proc.communicate()
    return {"returncode": proc.returncode, "stdout": out, "stderr": err,
            "timed_out": timed_out, "wall_seconds": time.monotonic() - start}


def usage(raw):
    turns = [event["usage"] for event in raw if event.get("type") == "turn.completed"]
    if len(turns) != 1:
        raise ValueError("Expected one completed turn with usage; missing tokens are not zero")
    u = turns[0]
    # Codex input_tokens INCLUDES cached_input_tokens (unlike Claude's ordinary-input field).
    total, cached, output = u["input_tokens"], u["cached_input_tokens"], u["output_tokens"]
    if not 0 <= cached <= total:
        raise ValueError("Invalid cache partition")
    writes = u.get("cache_write_input_tokens", 0)
    prices = PROTOCOL["pricing_per_million_tokens_usd"]
    if cached + writes > total:
        raise ValueError("Cache partitions exceed total input")
    return {"inputTokens": total - cached, "cacheReadInputTokens": cached,
            "cacheCreationInputTokens": u.get("cache_write_input_tokens", 0),
            "totalInputTokens": total, "outputTokens": output,
            "reasoningOutputTokens": u.get("reasoning_output_tokens", 0),
            "costUSD": ((total - cached - writes) * prices["input"] + cached * prices["cached_input"] + writes * prices["cache_write"] + output * prices["output"]) / 1_000_000}


def call_model(taskdir, stage, prompt, resume=None, first_session=None):
    cfg = read(taskdir / "config.json")
    root = Path(cfg["run_root"])
    prior = list(root.glob("tasks/*/*/calls/*.metrics.json"))
    spent = sum(read(p)["usage"]["costUSD"] for p in prior)
    if len(prior) >= PROTOCOL["max_model_calls"] or spent + 0.20 > 6:
        raise RuntimeError("Global model-call/cost budget reached")
    if time.time() >= read(root / "run.json")["deadline_epoch"]:
        raise RuntimeError("Run deadline reached")
    call_dir = taskdir / "calls"
    call_dir.mkdir(exist_ok=True)
    if (call_dir / f"{stage}.prompt.txt").exists():
        raise RuntimeError(f"Refusing an implicit retry of {stage}")
    (call_dir / f"{stage}.prompt.txt").write_text(prompt)
    cmd = [cfg["codex"], "exec"]
    if resume:
        cmd += ["resume"]
    cmd += ["--ignore-user-config", "--ignore-rules", "--skip-git-repo-check", "--json", "--model", PROTOCOL["model"]]
    settings = {"model_reasoning_effort": PROTOCOL["effort"], "approval_policy": "never", "sandbox_mode": "read-only",
                "project_doc_max_bytes": 0, "features.hooks": False, "features.plugins": False,
                "features.memories": False, "features.multi_agent": False, "features.shell_tool": False,
                "features.unified_exec": False, "features.skip_host_skill_discovery": True,
                "web_search": "disabled", "developer_instructions": SYSTEM}
    for key, value in settings.items():
        cmd += ["-c", key + "=" + json.dumps(value)]
    if resume:
        cmd += [resume]
    cmd += [prompt]
    result = bounded(cmd, taskdir, PROTOCOL["model_call_timeout_seconds"])
    (call_dir / f"{stage}.stdout.json").write_text(result["stdout"])
    (call_dir / f"{stage}.stderr.txt").write_text(result["stderr"])
    dump(call_dir / f"{stage}.process.json", {k: v for k, v in result.items() if k not in ("stdout", "stderr")})
    raw = [json.loads(line) for line in result["stdout"].splitlines() if line.strip()]
    sessions = [event["thread_id"] for event in raw if event.get("type") == "thread.started"]
    items = [event["item"] for event in raw if event.get("type") == "item.completed"]
    tool_items = [item for item in items if item.get("type") not in ("agent_message", "reasoning", "error")]
    metrics = {"stage": stage, "usage": usage(raw), "wall_seconds": result["wall_seconds"],
               "session_id": sessions[0] if sessions else None,
               "main_usage": [event["usage"] for event in raw if event.get("type") == "turn.completed"][0],
               "tool_events": tool_items, "warnings": [item for item in items if item.get("type") == "error"],
               "num_turns": sum(event.get("type") == "turn.completed" for event in raw),
               "returncode": result["returncode"]}
    dump(call_dir / f"{stage}.metrics.json", metrics)
    if result["returncode"] or result["timed_out"] or tool_items or any(e.get("type") == "turn.failed" for e in raw):
        raise RuntimeError(f"Model failed or used tools: {stage}, exit={result['returncode']}")
    text = [item["text"] for item in items if item.get("type") == "agent_message"][-1].strip()
    # Deterministic transport normalization, never a second model call.
    if text.startswith("```"):
        text = re.sub(r"^```(?:json)?\s*", "", text)
        text = re.sub(r"\s*```$", "", text)
    artifact = json.loads(text)
    if not isinstance(artifact, dict):
        raise ValueError("Expected JSON object")
    if stage == "implement" and not isinstance(artifact.get("code"), str):
        raise ValueError("Missing implementation code")
    if stage == "review":
        initial = read(taskdir / "test_initial.json")
        expected = "approved" if initial["passed"] else "revised"
        if artifact.get("status") != expected:
            raise ValueError(f"Invalid review status: expected {expected}")
        if initial["passed"] and artifact.get("code") is not None:
            raise ValueError("Editing a passing solution is outside the frozen protocol")
        if not initial["passed"] and not isinstance(artifact.get("code"), str):
            raise ValueError("Missing repair code")
    dump(taskdir / "artifacts" / f"{stage}.json", artifact)
    print(f"CALL {cfg['task']} {cfg['arm']} {stage} input={metrics['usage']['totalInputTokens']} output={metrics['usage']['outputTokens']} usd={metrics['usage']['costUSD']:.6f}", file=sys.stderr, flush=True)
    return artifact, metrics


TEST_RUNNER = '''import json, sys, unittest
suite = unittest.defaultTestLoader.discover(".", pattern="*_test.py")
result = unittest.TextTestRunner(verbosity=1).run(suite)
print("BENCH_TEST_RESULT=" + json.dumps({"tests_run":result.testsRun,"failures":len(result.failures),"errors":len(result.errors),"skipped":len(result.skipped),"passed":result.wasSuccessful() and result.testsRun > 0}))
sys.exit(0 if result.wasSuccessful() and result.testsRun > 0 else 1)
'''


def evaluate(candidate, outdir, phase):
    name = "gloop-bench-" + uuid.uuid4().hex[:12]
    cmd = ["docker", "run", "--rm", "--name", name, "--network", "none", "--read-only", "--cap-drop", "ALL",
           "--security-opt", "no-new-privileges", "--pids-limit", "64", "--memory", "256m", "--cpus", "1",
           "--user", "65534:65534", "--tmpfs", "/tmp:rw,noexec,nosuid,size=16m", "-e", "PYTHONDONTWRITEBYTECODE=1",
           "-v", f"{candidate}:/exercise:ro", "-w", "/exercise", PROTOCOL["test_image"], "python", "-c", TEST_RUNNER]
    result = bounded(cmd, candidate, PROTOCOL["test_timeout_seconds"])
    if result["timed_out"]:
        bounded(["docker", "rm", "-f", name], candidate, 10)
    full = result["stderr"] + result["stdout"]
    (outdir / f"{phase}.log").write_text(full)
    matches = re.findall(r"^BENCH_TEST_RESULT=(.+)$", result["stdout"], re.M)
    stats = json.loads(matches[-1]) if matches else {"tests_run": None, "failures": None, "errors": None, "skipped": None, "passed": False}
    stats["passed"] = stats["passed"] and result["returncode"] == 0 and not result["timed_out"]
    stats.update({"returncode": result["returncode"], "timed_out": result["timed_out"], "wall_seconds": result["wall_seconds"]})
    # Exclude the timing-dependent unittest summary on success; failures retain the public feedback.
    stats["feedback"] = "All public tests passed." if stats["passed"] else full.encode()[:12000].decode(errors="replace")
    dump(outdir / f"{phase}.json", stats)
    return stats


def test_stage(taskdir, phase):
    cfg = read(taskdir / "config.json")
    artifact = read(taskdir / "artifacts" / "implement.json")
    if phase == "test_final":
        review = read(taskdir / "artifacts" / "review.json")
        if review["status"] == "revised":
            artifact = review
    (taskdir / "candidate" / cfg["solution_file"]).write_text(artifact["code"])
    return evaluate(taskdir / "candidate", taskdir, phase)


def feedback(stats):
    return {k: v for k, v in stats.items() if k not in ("wall_seconds",)}


def make_graph(taskdir):
    nodes = []
    for stage in ["design", "implement", "test_initial", "review", "test_final", "pr_draft"]:
        node = {"id": stage, "retry": {"max_attempts": 1}, "workspace": {"mode": "current"},
                "output": {"format": "json", "max_bytes": 131072}, "timeout_seconds": 135}
        if stage.startswith("test_"):
            node.update(kind="command", argv=[sys.executable, str(Path(__file__).resolve()), "test", "--taskdir", str(taskdir), "--phase", stage])
        else:
            node.update(kind="agent", profile="bench", model=PROTOCOL["model"], fan_out=1,
                        prompt=f"BENCH_STAGE={stage}\n{PROMPTS[stage]}",
                        context={"include_dependencies": True, "files": ["title.md" if stage == "pr_draft" else "brief.md"], "max_bytes": 262144})
        nodes.append(node)
    edges = [("design", "implement"), ("implement", "test_initial"), ("implement", "review"),
             ("test_initial", "review"), ("review", "test_final"), ("test_final", "pr_draft"), ("review", "pr_draft")]
    return {"apiVersion": "gloop.dev/v1alpha1", "kind": "Graph", "metadata": {"name": "public-exercise-pilot", "version": "1.0.0"},
            "spec": {"goal": "Design, implement, test, review or repair once, and draft a pull request for a public exercise",
                     "policies": {"max_parallel": 1, "failure": "fail_fast"}, "budgets": {"model_calls": 4, "wall_time_seconds": 600},
                     "nodes": nodes, "edges": [{"from": a, "to": b, "kind": "data"} for a, b in edges]}}


def prepare(args):
    root = args.root.resolve()
    if root.exists():
        raise RuntimeError("Choose a new run root; existing evidence must not be overwritten")
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.dataset, text=True).strip()
    if revision != PROTOCOL["dataset_revision"]:
        raise RuntimeError("Dataset revision mismatch")
    exercise_root = args.dataset.resolve() / "python/exercises/practice"
    names = sorted(p.name for p in exercise_root.iterdir() if p.is_dir())
    assert random.Random(20260908).sample(names, 6) == PROTOCOL["tasks"]
    root.mkdir(parents=True)
    binary = root / "gloop"
    shutil.copy2(args.gloop, binary)
    dump(root / "run.json", {"prepared_at": dt.datetime.now(dt.timezone.utc).isoformat(), "deadline_epoch": time.time() + 2400,
         "monitor_owner": "/root", "monitor_target": str(root), "terminal": ["completed", "failed", "deadline"],
         "deadline_policy": "40 minutes; process timeouts; stop after three consecutive workflow errors",
         "dataset_revision": revision, "gloop_binary_sha256": sha(binary), "harness_sha256": sha(Path(__file__)),
         "gloop_version": subprocess.check_output([str(binary), "--version"], text=True).strip(),
         "system_prompt": SYSTEM, "prompts": PROMPTS, "protocol": PROTOCOL})
    preflight = []
    for task in PROTOCOL["tasks"]:
        source = exercise_root / task
        config = read(source / ".meta/config.json")["files"]
        if len(config["solution"]) != 1:
            raise RuntimeError("This pilot expects one solution file")
        solution = config["solution"][0]
        brief = f"Public Exercism Python exercise: {task}\n\n"
        for doc in ["instructions.md", "instructions.append.md"]:
            path = source / ".docs" / doc
            if path.exists():
                brief += path.read_text() + "\n\n"
        brief += f"Starter source ({solution}):\n```python\n{(source / solution).read_text()}\n```\n"
        reference = root / "preflight" / task
        reference.mkdir(parents=True)
        for file in config["test"]:
            shutil.copy2(source / file, reference / file)
        shutil.copy2(source / config["example"][0], reference / solution)
        check = evaluate(reference, reference, "reference")
        preflight.append({"task": task, **check})
        print(f"PREFLIGHT {task} passed={check['passed']} tests={check['tests_run']}", flush=True)
        if not check["passed"]:
            raise RuntimeError(f"Upstream reference failed for {task}; do not run models")
        for arm in PROTOCOL["arms"]:
            taskdir = root / "tasks" / task / arm
            (taskdir / "candidate").mkdir(parents=True)
            for file in [solution, *config["test"]]:
                shutil.copy2(source / file, taskdir / "candidate" / file)
            (taskdir / "brief.md").write_text(brief)
            (taskdir / "title.md").write_text(f"Exercise: {task}\n")
            dump(taskdir / "config.json", {"task": task, "arm": arm, "solution_file": solution, "run_root": str(root),
                 "codex": shutil.which("codex"), "test_hashes": {f: sha(source / f) for f in config["test"]}})
            (taskdir / ".gloop").mkdir()
            argv = [sys.executable, str(Path(__file__).resolve()), "provider", "--taskdir", str(taskdir)]
            profile = '[profiles.bench]\nkind = "command"\nargv = ' + json.dumps(argv) + '\nprompt_mode = "stdin"\noutput = "json"\noutput_pointer = "/result"\nversion_args = ["--version"]\nmodel_args = ["--model", "{model}"]\ntimeout_seconds = 130\n'
            (taskdir / ".gloop/profiles.toml").write_text(profile)
            dump(taskdir / "graph.json", make_graph(taskdir))
    dump(root / "preflight.json", preflight)


def run_retained(taskdir):
    session = None
    for stage in STAGES:
        prompt = f"BENCH_STAGE={stage}\n{PROMPTS[stage]}"
        if stage == "design":
            prompt += "\n\n" + (taskdir / "brief.md").read_text()
        if stage == "review":
            prompt += "\nInitial test result:\n" + json.dumps(feedback(read(taskdir / "test_initial.json")))
        if stage == "pr_draft":
            prompt += "\nFinal test result:\n" + json.dumps(feedback(read(taskdir / "test_final.json")))
        _, metrics = call_model(taskdir, stage, prompt, resume=session)
        session = metrics["session_id"]
        if stage == "implement":
            test_stage(taskdir, "test_initial")
        if stage == "review":
            test_stage(taskdir, "test_final")


def run_all(args):
    root = args.root.resolve()
    meta = read(root / "run.json")
    if sha(Path(__file__)) != meta["harness_sha256"]:
        raise RuntimeError("Harness changed after prepare; use a new run root")
    errors = 0
    for index, task in enumerate(PROTOCOL["tasks"]):
        arms = PROTOCOL["arms"] if index % 2 == 0 else list(reversed(PROTOCOL["arms"]))
        for arm in arms:
            taskdir = root / "tasks" / task / arm
            if (taskdir / "outcome.json").exists():
                raise RuntimeError("Run already started; no implicit resume/retry")
            if time.time() >= meta["deadline_epoch"]:
                raise RuntimeError("Run deadline reached")
            print(f"START {task} {arm}", flush=True)
            start = time.monotonic()
            error = None
            try:
                if arm == "retained_conversation":
                    run_retained(taskdir)
                else:
                    run = bounded([str(root / "gloop"), "run", "--graph", str(taskdir / "graph.json"), "--repo", str(taskdir),
                                   "--trust-project-profiles", "--json", "--non-interactive"], taskdir, min(610, max(1, meta["deadline_epoch"] - time.time())))
                    (taskdir / "gloop.stdout.json").write_text(run["stdout"])
                    (taskdir / "gloop.stderr.log").write_text(run["stderr"])
                    dump(taskdir / "gloop.process.json", {k: v for k, v in run.items() if k not in ("stdout", "stderr")})
                    if run["returncode"] or run["timed_out"]:
                        raise RuntimeError(f"gloop execution failed exit={run['returncode']}; see gloop.stderr.log")
                if not (taskdir / "artifacts/pr_draft.json").exists():
                    raise RuntimeError("Workflow missing PR draft")
                for f, expected_hash in read(taskdir / "config.json")["test_hashes"].items():
                    if sha(taskdir / "candidate" / f) != expected_hash:
                        raise RuntimeError(f"Test file changed: {f}")
            except Exception as exc:
                error = f"{type(exc).__name__}: {exc}"
            errors = errors + 1 if error else 0
            initial = read(taskdir / "test_initial.json") if (taskdir / "test_initial.json").exists() else None
            final = read(taskdir / "test_final.json") if (taskdir / "test_final.json").exists() else None
            outcome = {"task": task, "arm": arm, "error": error, "wall_seconds": time.monotonic() - start,
                       "initial": initial, "final": final, "workflow_completed": error is None}
            dump(taskdir / "outcome.json", outcome)
            print(f"DONE {task} {arm} initial={initial and initial['passed']} final={final and final['passed']} error={error}", flush=True)
            if errors >= 3:
                raise RuntimeError("Three consecutive workflow errors: circuit breaker")
    dump(root / "completed.json", {"completed_at": dt.datetime.now(dt.timezone.utc).isoformat()})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "run", "provider", "test"])
    parser.add_argument("--root", type=Path)
    parser.add_argument("--dataset", type=Path)
    parser.add_argument("--gloop", type=Path)
    parser.add_argument("--taskdir", type=Path)
    parser.add_argument("--phase", choices=["test_initial", "test_final"])
    parser.add_argument("--model")
    parser.add_argument("--version", action="store_true")
    args = parser.parse_args()
    if args.version:
        print("gloop-context-pilot-provider 1")
    elif args.command == "prepare":
        prepare(args)
    elif args.command == "run":
        run_all(args)
    elif args.command == "provider":
        prompt = sys.stdin.read()
        match = re.search(r"BENCH_STAGE=(design|implement|review|pr_draft)\b", prompt)
        if not match:
            raise ValueError("Missing stage marker")
        artifact, metrics = call_model(args.taskdir, match[1], prompt)
        print(json.dumps({"result": artifact, "usage": {"input_tokens": metrics["usage"]["totalInputTokens"],
                                                        "output_tokens": metrics["usage"]["outputTokens"], "cached_input_tokens": metrics["usage"]["cacheReadInputTokens"], "cache_write_input_tokens": metrics["usage"]["cacheCreationInputTokens"], "reasoning_output_tokens": metrics["usage"]["reasoningOutputTokens"]}, "model": PROTOCOL["model"]}))
    elif args.command == "test":
        print(json.dumps(feedback(test_stage(args.taskdir, args.phase))))


if __name__ == "__main__":
    main()
