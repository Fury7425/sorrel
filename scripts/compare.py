"""Launch an app, time its first visible window, then measure idle memory and
CPU of its whole process tree. Windows only.

    python compare.py NAME EXE [ARGS...]
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import subprocess
import sys
import time

import psutil

user32 = ctypes.windll.user32
EnumProc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def visible_window_pids():
    pids = set()

    def callback(hwnd, _):
        if user32.IsWindowVisible(hwnd) and user32.GetWindowTextLengthW(hwnd) > 0:
            pid = wt.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
            pids.add(pid.value)
        return True

    user32.EnumWindows(EnumProc(callback), 0)
    return pids


def tree(root):
    try:
        return [root] + root.children(recursive=True)
    except psutil.Error:
        return [root]


def sample(root):
    rss = private = 0
    for p in tree(root):
        try:
            rss += p.memory_info().rss
            private += p.memory_full_info().uss
        except psutil.Error:
            pass
    return rss, private


def cpu_seconds(root):
    total = 0.0
    for p in tree(root):
        try:
            t = p.cpu_times()
            total += t.user + t.system
        except psutil.Error:
            pass
    return total


def main():
    name, exe, *args = sys.argv[1:]
    t0 = time.perf_counter()
    proc = subprocess.Popen([exe, *args], env=dict(os.environ))
    root = psutil.Process(proc.pid)
    first_window = None
    while time.perf_counter() - t0 < 60:
        pids = {p.pid for p in tree(root)}
        if pids & visible_window_pids():
            first_window = (time.perf_counter() - t0) * 1000
            break
        time.sleep(0.01)
    time.sleep(20)  # settle: let startup work finish
    cpu0, w0 = cpu_seconds(root), time.perf_counter()
    time.sleep(10)
    cpu1, w1 = cpu_seconds(root), time.perf_counter()
    rss, private = sample(root)
    result = {
        "app": name,
        "processes": len(tree(root)),
        "first_window_ms": round(first_window or -1),
        "idle_rss_mb": round(rss / 1e6, 1),
        "idle_private_mb": round(private / 1e6, 1),
        "idle_cpu_pct": round(100 * (cpu1 - cpu0) / (w1 - w0), 2),
    }
    for p in reversed(tree(root)):
        try:
            p.kill()
        except psutil.Error:
            pass
    print(json.dumps(result))


if __name__ == "__main__":
    main()
