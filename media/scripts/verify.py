"""Verify delivery properties and the evidence from the real terminal take."""
import datetime
import hashlib
import json
import os
import pathlib
import subprocess

media = pathlib.Path(__file__).resolve().parents[1]
for path in (media / "public").iterdir():
    text = path.read_text()
    for identifier in [str(pathlib.Path.home()), os.environ.get("USER", ""), "/Users/"]:
        assert not identifier or identifier not in text, (path.name, "private identifier")

evidence = json.loads((media / "public/run-evidence.json").read_text())
assert evidence["capture_sha256"] == hashlib.sha256((media / "public/terminal.cast").read_bytes()).hexdigest()
assert evidence["run_id"] in (media / "public/terminal.cast").read_text()
events = evidence["events"]
def instant(node, kind):
    return datetime.datetime.fromisoformat(next(e["timestamp"] for e in events if e.get("node_id") == node and e["kind"] == kind))
lanes = ["quality", "coverage", "release"]
assert max(instant(n, "node_started") for n in lanes) < min(instant(n, "node_succeeded") for n in lanes)
assert instant("merge", "node_started") > max(instant(n, "node_succeeded") for n in lanes)
assert evidence["report"]["status"] == "ready"
assert all(check["passed"] for check in evidence["report"]["checks"])
results = {"privacy": "passed", "parallel_overlap": "passed", "merge_after_dependencies": "passed", "videos": []}
for name, duration in [("terminal", 31), ("parallel", 21)]:
    path = media.parent / "assets/videos" / f"gloop-{name}.mp4"
    info = json.loads(subprocess.check_output(["ffprobe", "-v", "error", "-show_streams", "-show_format", "-of", "json", str(path)]))
    assert all(stream["codec_type"] != "audio" for stream in info["streams"])
    video = next(stream for stream in info["streams"] if stream["codec_type"] == "video")
    assert (video["width"], video["height"], video["r_frame_rate"]) == (1920, 1080, "30/1")
    assert abs(float(info["format"]["duration"]) - duration) < .1
    results["videos"].append({"file": path.name, "duration_seconds": duration, "size_bytes": path.stat().st_size, "audio_streams": 0})
print(json.dumps(results, indent=2))
