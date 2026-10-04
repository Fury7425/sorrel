"""Renders crates/app/icon/*.svg into sorrel.ico, sorrel.icns and sorrel.png.

    python scripts/icon.py

Headless Chrome or Edge draws each size straight from the SVG (no resampling);
the ICO and ICNS containers are written by hand with PNG entries. Sizes of 32 px
and under come from sorrel-small.svg, the simpler cut.
"""
import base64, json, os, shutil, struct, subprocess, tempfile

ICON = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "crates", "app", "icon")
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
# ICNS types and their pixel sizes; the @2x types reuse the next size up.
ICNS = [("icp4", 16), ("icp5", 32), ("ic11", 32), ("icp6", 64), ("ic12", 64), ("ic07", 128),
        ("ic08", 256), ("ic13", 256), ("ic09", 512), ("ic14", 512), ("ic10", 1024)]
LINUX = 512


def browser():
    for path in (
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ):
        if os.path.exists(path):
            return path
    for name in ("google-chrome", "chromium", "chromium-browser", "chrome", "msedge"):
        if shutil.which(name):
            return shutil.which(name)
    raise SystemExit("needs Chrome, Chromium or Edge to render the SVG")


def render(sizes):
    """Returns {size: png bytes}, each drawn by the browser onto a canvas of that size."""
    svgs = {}
    for name in ("sorrel", "sorrel-small"):
        with open(os.path.join(ICON, f"{name}.svg"), encoding="utf-8") as fh:
            svgs[name] = "data:image/svg+xml;base64," + base64.b64encode(fh.read().encode()).decode()
    page = f"""<!doctype html><body><pre id="out"></pre><script>
const svgs = {json.dumps(svgs)}, sizes = {json.dumps(sorted(set(sizes)))};
const load = (src) => new Promise((ok) => {{ const i = new Image(); i.onload = () => ok(i); i.src = src; }});
(async () => {{
  const full = await load(svgs['sorrel']), small = await load(svgs['sorrel-small']);
  const out = {{}};
  for (const s of sizes) {{
    const c = document.createElement('canvas'); c.width = c.height = s;
    c.getContext('2d').drawImage(s <= 32 ? small : full, 0, 0, s, s);
    out[s] = c.toDataURL('image/png').split(',')[1];
  }}
  document.getElementById('out').textContent = JSON.stringify(out);
}})();
</script></body>"""
    with tempfile.TemporaryDirectory() as tmp:
        html = os.path.join(tmp, "render.html")
        with open(html, "w", encoding="utf-8") as fh:
            fh.write(page)
        dom = subprocess.run(
            [browser(), "--headless=new", "--disable-gpu", f"--user-data-dir={tmp}",
             "--virtual-time-budget=10000", "--dump-dom", "file:///" + html.replace("\\", "/")],
            capture_output=True, text=True, timeout=120,
        ).stdout
    start = dom.index('<pre id="out">') + len('<pre id="out">')
    data = json.loads(dom[start:dom.index("</pre>", start)].replace("&quot;", '"'))
    return {int(k): base64.b64decode(v) for k, v in data.items()}


def ico(pngs):
    entries, blobs, offset = b"", b"", 6 + 16 * len(pngs)
    for size, png in pngs:
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(png), offset)
        blobs += png
        offset += len(png)
    return struct.pack("<HHH", 0, 1, len(pngs)) + entries + blobs


def icns(chunks):
    body = b"".join(kind.encode() + struct.pack(">I", 8 + len(png)) + png for kind, png in chunks)
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


if __name__ == "__main__":
    pngs = render(ICO_SIZES + [s for _, s in ICNS] + [LINUX])
    with open(os.path.join(ICON, "sorrel.ico"), "wb") as fh:
        fh.write(ico([(s, pngs[s]) for s in ICO_SIZES]))
    with open(os.path.join(ICON, "sorrel.icns"), "wb") as fh:
        fh.write(icns([(k, pngs[s]) for k, s in ICNS]))
    with open(os.path.join(ICON, "sorrel.png"), "wb") as fh:
        fh.write(pngs[LINUX])
    for name in ("sorrel.ico", "sorrel.icns", "sorrel.png"):
        print(name, os.path.getsize(os.path.join(ICON, name)))
