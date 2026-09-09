#!/usr/bin/env python3
"""Audit saved CLI evidence and export per-call, per-task, and aggregate metrics."""
import argparse
import csv
import json
from pathlib import Path
from benchmark import PROTOCOL, STAGES, dump, read, sha, usage


def analyze(root, output):
    meta = read(root / "run.json")
    calls, tasks, checks, format_deviations = [], [], [], []
    for task in PROTOCOL["tasks"]:
        for arm in PROTOCOL["arms"]:
            directory = root / "tasks" / task / arm
            outcome = read(directory / "outcome.json")
            cfg = read(directory / "config.json")
            for filename, expected in cfg["test_hashes"].items():
                assert sha(directory / "candidate" / filename) == expected, (task, arm, "tests changed")
            task_calls = []
            for stage in STAGES:
                path = directory / "calls" / f"{stage}.metrics.json"
                if not path.exists():
                    continue
                metrics = read(path)
                raw_path = directory / "calls" / f"{stage}.stdout.json"
                events = [json.loads(line) for line in raw_path.read_text().splitlines()]
                assert usage(events) == metrics["usage"], (task, arm, stage, "usage mismatch")
                assert not metrics["tool_events"], (task, arm, stage, "tool access")
                assert metrics["returncode"] == 0 and metrics["num_turns"] == 1
                row = {"task": task, "arm": arm, "stage": stage, **metrics["usage"],
                       "wall_seconds": metrics["wall_seconds"], "session_id": metrics["session_id"],
                       "prompt_sha256": sha(directory / "calls" / f"{stage}.prompt.txt"),
                       "raw_sha256": sha(raw_path), "artifact_sha256": sha(directory / "artifacts" / f"{stage}.json"),
                       "main_usage": metrics["main_usage"], "warnings": metrics["warnings"]}
                artifact = read(directory / "artifacts" / f"{stage}.json")
                requested = {"design": ["plan", "risks"], "implement": ["code"],
                             "review": ["status", "code", "review"], "pr_draft": ["title", "body"]}[stage]
                missing = [field for field in requested if field not in artifact]
                if missing:
                    format_deviations.append({"task": task, "arm": arm, "stage": stage, "missing_requested_fields": missing})
                calls.append(row)
                task_calls.append(row)
            sessions = {call["session_id"] for call in task_calls}
            if outcome["workflow_completed"]:
                assert len(task_calls) == 4
                assert len(sessions) == (1 if arm == "retained_conversation" else 4), (task, arm, sessions)
            runtime = None
            if arm == "gloop_artifact_handoff":
                runtime = read(directory / "gloop.stdout.json")
                if outcome["workflow_completed"]:
                    assert runtime["success"] is True
                    assert len(runtime["summary"]["nodes"]) == 6
                    assert all(node["status"] == "succeeded" and node["attempts"] == 1
                               for node in runtime["summary"]["nodes"].values())
                runtime = {"success": runtime["success"], "run_id": runtime["summary"]["run_id"],
                           "status": runtime["summary"]["status"], "nodes": runtime["summary"]["nodes"],
                           "provenance": runtime["summary"]["provenance"]}
            review_path = directory / "artifacts/review.json"
            review = read(review_path) if review_path.exists() else None
            initial, final = outcome["initial"], outcome["final"]
            tasks.append({"task": task, "arm": arm, "workflow_completed": outcome["workflow_completed"],
                          "error": outcome["error"], "calls": len(task_calls),
                          "initial_pass": bool(initial and initial["passed"]), "final_pass": bool(final and final["passed"]),
                          "initial_tests": initial, "final_tests": final,
                          "repair_count": int(bool(review and review["status"] == "revised")),
                          "wall_seconds": outcome["wall_seconds"], "gloop": runtime,
                          "usage": {key: sum(c[key] for c in task_calls) for key in ["totalInputTokens", "inputTokens", "cacheReadInputTokens", "cacheCreationInputTokens", "outputTokens", "reasoningOutputTokens", "costUSD"]}})
    totals = {}
    for arm in PROTOCOL["arms"]:
        rows = [t for t in tasks if t["arm"] == arm]
        totals[arm] = {"tasks": len(rows), "completed_workflows": sum(t["workflow_completed"] for t in rows),
                       "initial_pass": sum(t["initial_pass"] for t in rows), "final_pass": sum(t["final_pass"] for t in rows),
                       "repairs": sum(t["repair_count"] for t in rows), "calls": sum(t["calls"] for t in rows),
                       "wall_seconds": sum(t["wall_seconds"] for t in rows),
                       "usage": {key: sum(t["usage"][key] for t in rows) for key in rows[0]["usage"]},
                       "by_stage": {stage: {key: sum(c[key] for c in calls if c["arm"] == arm and c["stage"] == stage)
                                            for key in ["totalInputTokens", "cacheReadInputTokens", "outputTokens", "costUSD"]} for stage in STAGES}}
    retained, gloop = [totals[arm] for arm in PROTOCOL["arms"]]
    differences = {key: {"gloop_minus_retained": gloop["usage"][key] - retained["usage"][key],
                         "reduction_percent": (1 - gloop["usage"][key] / retained["usage"][key]) * 100 if retained["usage"][key] else None}
                   for key in ["totalInputTokens", "outputTokens", "costUSD"]}
    checks = ["Public test hashes unchanged in all candidate directories",
              "Each saved metric matches a fresh parse of raw turn.completed usage",
              "No observed tool events in measured model turns",
              "Completed retained workflows each used one session; gloop workflows each used four distinct sessions",
              "Completed gloop summaries each show six succeeded nodes and one attempt per node"]
    result = {"run": meta, "completed": read(root / "completed.json") if (root / "completed.json").exists() else None,
              "preflight": read(root / "preflight.json"), "totals": totals, "differences": differences,
              "tasks": tasks, "calls": calls, "audit_checks": checks, "format_deviations": format_deviations,
              "raw_evidence_directory": str(root), "cost_note": "Calculated standard API list-price equivalent, not actual ChatGPT subscription billing"}
    dump(output / "results.json", result)
    fields = ["task", "arm", "stage", "totalInputTokens", "inputTokens", "cacheReadInputTokens", "outputTokens", "reasoningOutputTokens", "costUSD", "wall_seconds", "session_id"]
    with (output / "calls.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(calls)
    print(json.dumps({"totals": totals, "differences": differences, "audit_checks": checks}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    analyze(args.root.resolve(), args.output.resolve())
