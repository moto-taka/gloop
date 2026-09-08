"""Local recording fixture: three real analyses, followed by one file-based merge."""
import json
import pathlib
import sys
import time

lane = sys.argv[1]
root = pathlib.Path(".")
data = json.loads((root / "sample.json").read_text())
out = root / "results"
out.mkdir(exist_ok=True)
if lane == "merge":
    parts = [json.loads((out / f"{name}.json").read_text()) for name in ("quality", "coverage", "release")]
    assert all(part["passed"] for part in parts), parts
    time.sleep(3)
    report = {"status": "ready", "checks": parts}
    (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("3 independent results combined.\nAll checks passed.\nReport saved: results/report.json")
else:
    # Visible hold times belong to this demo fixture, never to gloop itself.
    time.sleep({"quality": 5, "coverage": 8, "release": 11}[lane])
    if lane == "quality":
        result = {"check": lane, "passed": all(item["lint_errors"] == 0 for item in data), "modules": len(data)}
    elif lane == "coverage":
        total = sum(item["tests"] for item in data)
        passed = sum(item["passed"] for item in data)
        result = {"check": lane, "passed": total == passed, "tests": total}
    else:
        result = {"check": lane, "passed": all(item["license"] == "Apache-2.0" for item in data), "packages": len(data)}
    assert result["passed"], result
    (out / f"{lane}.json").write_text(json.dumps(result) + "\n")
    print(json.dumps(result, indent=2))
