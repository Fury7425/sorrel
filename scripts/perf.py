#!/usr/bin/env python3
"""Performance gate.

Launches `sorrel --bench` on a replayed stream over a long seeded transcript,
then checks cold start, frame cost, idle RSS and idle CPU against the budget in
docs/ARCHITECTURE.md. Exits 1 if any metric is over budget.

    pip install psutil
    python scripts/perf.py target/release/sorrel[.exe] [--seed 10000] [--pace-ms 8]
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time

import psutil

ROOT = pathlib.Path(__file__).resolve().parent.parent
BUDGET = {
    "first_frame_ms": 300,
    "idle_rss_mb": 150,
    "idle_cpu_pct": 1.0,
    "stream_draw_p99_ms": 16.7,
    "scroll_draw_p99_ms": 16.7,
    "screen_switch_max_ms": 33.3,
    "screens_draw_p99_ms": 16.7,
}


def read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--seed", type=int, default=10000)
    ap.add_argument("--pace-ms", type=int, default=8)
    ap.add_argument("--fixture", default=str(ROOT / "fixtures/claude/synthetic_stream.jsonl"))
    ap.add_argument("--timeout", type=float, default=180)
    args = ap.parse_args()

    out = pathlib.Path(tempfile.mkdtemp()) / "bench.json"
    env = dict(os.environ, SORREL_T0_MS=str(time.time_ns() // 1_000_000))
    proc = subprocess.Popen(
        [args.binary, "--bench", str(out), "--replay", args.fixture,
         "--seed", str(args.seed), "--pace-ms", str(args.pace_ms)],
        env=env,
    )
    app = psutil.Process(proc.pid)
    deadline = time.monotonic() + args.timeout
    peak_rss = 0

    def wait_for(phase):
        nonlocal peak_rss
        while time.monotonic() < deadline:
            # Read first: the app writes its final report and exits right away.
            report = read_json(out)
            if report and report.get("phase") == phase:
                return report
            if proc.poll() is not None:
                sys.exit(f"sorrel exited with code {proc.returncode} before phase {phase!r}")
            try:
                peak_rss = max(peak_rss, app.memory_info().rss)
            except psutil.Error:
                pass
            time.sleep(0.1)
        proc.kill()
        sys.exit(f"timed out waiting for phase {phase!r}")

    wait_for("idle")
    time.sleep(2)  # let the last scroll frame settle
    cpu_before, t_before = app.cpu_times(), time.monotonic()
    time.sleep(10)
    cpu_after, t_after = app.cpu_times(), time.monotonic()
    idle_rss = app.memory_info().rss
    # Private memory leaves out shared and mapped pages (fonts, GPU driver).
    idle_private = app.memory_full_info().uss
    report = wait_for("done")
    proc.wait(timeout=30)

    busy = (cpu_after.user - cpu_before.user) + (cpu_after.system - cpu_before.system)
    measured = {
        # -1 means the app never reported a first frame.
        "first_frame_ms": report["first_frame_ms"] if report["first_frame_ms"] >= 0 else float("inf"),
        "idle_rss_mb": idle_rss / 1e6,
        "idle_cpu_pct": 100 * busy / (t_after - t_before),
        "stream_draw_p99_ms": report["stream"]["draw_ms"]["p99"],
        "scroll_draw_p99_ms": report["scroll"]["draw_ms"]["p99"],
        # Opening a screen may take two frames at 60 Hz; staying on it must fit in one.
        "screen_switch_max_ms": max(s["draw_ms"]["max"] for s in report["screens"].values()),
        "screens_draw_p99_ms": max(s["draw_ms"]["p99"] for s in report["screens"].values()),
        "idle_private_mb": idle_private / 1e6,
        "peak_rss_mb": peak_rss / 1e6,
        "idle_frames": report["idle"]["frames"],
    }

    failed = False
    print(f"{'metric':<22}{'measured':>12}{'budget':>10}")
    for name, value in measured.items():
        budget = BUDGET.get(name)
        over = budget is not None and value > budget
        failed |= over
        print(f"{name:<22}{value:>12.1f}{budget if budget is not None else '-':>10}{'  OVER' if over else ''}")
    for name, screen in report["screens"].items():
        print(f"  screen {name:<9} frames {screen['frames']:>3}  draw p50 {screen['draw_ms']['p50']:>6.2f}  max {screen['draw_ms']['max']:>6.2f} ms")
    print(json.dumps(report))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
