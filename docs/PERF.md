# Performance log

One row per measured build. Budget from `ARCHITECTURE.md`: idle RSS 150 MB or less, idle CPU about 0%, first frame under 300 ms, no dropped frames while streaming or scrolling 10,000 messages.

Measure with `python scripts/perf.py target/release/sorrel --seed 10000 --pace-ms 8` on a release build with `--features bench`. Frame cost is time inside `Window::draw` from GPUI's profiler; RSS is the OS resident set (working set on Windows).

| Date | Commit | OS / GPU | First frame (ms) | Idle RSS (MB) | Peak RSS (MB) | Idle CPU (%) | Stream draw p99 (ms) | Scroll draw p99 (ms) | Idle frames / 15 s | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-10-04 | Phases 0 to 3 written | Windows 11 | not measured | not measured | not measured | not measured | not measured | not measured | not measured | Not built locally (no Windows SDK); first numbers will come from CI or a local build. |
