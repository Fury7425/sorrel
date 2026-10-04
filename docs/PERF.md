# Performance log

One row per measured build. Budget from `ARCHITECTURE.md`: idle RSS 150 MB or less, idle CPU about 0%, first frame under 300 ms, no dropped frames while streaming or scrolling 10,000 messages.

Measure with `python scripts/perf.py target/release/sorrel --seed 10000 --pace-ms 8` on a release build with `--features bench`. Frame cost is time inside `Window::draw` from GPUI's profiler; RSS is the OS resident set (working set on Windows).

| Date | Commit | OS / GPU | First frame (ms) | Idle RSS (MB) | Peak RSS (MB) | Idle CPU (%) | Stream draw p99 (ms) | Scroll draw p99 (ms) | Idle frames / 15 s | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-10-04 | 317bf43+ | macOS 15, GitHub runner (Metal) | 2308 | 116.5 | 116.6 | 0.8 | 9.0 | 18.0 | 2 | Release build, 10k rows. First frame is a cold CI runner; scroll is the worst case (20-row jumps every 8 ms). |
| 2026-10-04 | 317bf43+ | Windows 11, local laptop GPU | 257–297 warm, 640–802 cold | 206.4 (private 160.2) | 214.9 | 0.0 | 4.2 | 3.6 | 0 | `fast` profile, 10k rows, 3 s settle before idle. Empty window: 171 RSS / 127 private. gpui-component's own hello_world on the same machine: 221 RSS / 155 private, so the Windows floor is the framework's. |
| 2026-10-04 | T3 UI | Windows 11, local laptop GPU | 270 warm, 686 cold | 210.0 (private 162.9) | 215.6 | 0.0–0.2 | 4.4–5.9 | 3.7–3.9 | 0 | T3-style UI, 10k rows. Screen switches: settings 2.6–3.3, tasks 2.6–3.0, project 2.3, home 1.3–1.4, thread 3.2 ms. |
| 2026-10-04 | 317bf43 | Ubuntu 24.04, GitHub runner (llvmpipe, Xvfb) | 3394 | 243.8 | 243.8 | 30.0 | 649.9 | 4.4 | 42 | Software rendering, before the idle and bench fixes; regression tripwire only. |
