#!/usr/bin/env python3
"""Render ANSI-coloured text from stdin to a PNG through headless Chrome.

    demo/mcp_tools_demo.py | demo/ansi_to_png.py out.png [title]
"""
import html
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

PALETTE = {"30": "#6e7681", "31": "#ff7b72", "32": "#7ee787", "33": "#e3b341", "35": "#d2a8ff", "36": "#79c0ff", "37": "#e6edf3"}
TOKEN = re.compile(r"\x1b\[([0-9;]*)m")


def to_html(text):
    out, open_spans = [], 0
    position = 0
    for match in TOKEN.finditer(text):
        out.append(html.escape(text[position : match.start()]))
        position = match.end()
        for code in match.group(1).split(";") or ["0"]:
            if code in ("0", ""):
                out.append("</span>" * open_spans)
                open_spans = 0
            elif code == "1":
                out.append('<span style="font-weight:700">')
                open_spans += 1
            elif code == "2":
                out.append('<span style="opacity:.6">')
                open_spans += 1
            elif code in PALETTE:
                out.append(f'<span style="color:{PALETTE[code]}">')
                open_spans += 1
    out.append(html.escape(text[position:]))
    out.append("</span>" * open_spans)
    return "".join(out)


def main():
    target = sys.argv[1]
    title = sys.argv[2] if len(sys.argv) > 2 else "ookcite-mcp"
    text = sys.stdin.read().rstrip("\n")
    lines = text.splitlines()
    width = min(max((len(TOKEN.sub("", line)) for line in lines), default=60), 190)
    page = f"""<!doctype html><meta charset="utf-8"><style>
body{{margin:0;background:#0d1117}}
.win{{margin:0;border-radius:0;background:#0d1117}}
.bar{{background:#161b22;color:#8b949e;font:13px 'DejaVu Sans',sans-serif;padding:9px 16px;border-bottom:1px solid #30363d}}
.dot{{display:inline-block;width:11px;height:11px;border-radius:50%;margin-right:7px}}
pre{{margin:0;padding:16px 20px 20px;color:#e6edf3;font:14px/1.45 'DejaVu Sans Mono',monospace;white-space:pre-wrap;word-break:break-word}}
</style><div class="win"><div class="bar"><span class="dot" style="background:#ff5f56"></span><span class="dot" style="background:#ffbd2e"></span><span class="dot" style="background:#27c93f"></span>{html.escape(title)}</div>
<pre>{to_html(text)}</pre></div>"""
    height = 46 + int(len(lines) * 14 * 1.45) + 50
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as handle:
        handle.write(page)
    profile = tempfile.mkdtemp(prefix="ansi-png-")
    target = os.path.abspath(target)
    if os.path.exists(target):
        os.unlink(target)
    chrome = subprocess.Popen(
        ["google-chrome", "--headless", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage",
         "--hide-scrollbars", f"--user-data-dir={profile}",
         f"--window-size={int(width * 8.6) + 60},{height}", f"--screenshot={target}", f"file://{handle.name}"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        # Chrome writes the file and can linger; stop it once the file is there.
        for _ in range(300):
            if os.path.exists(target) and os.path.getsize(target) > 0 or chrome.poll() is not None:
                break
            time.sleep(0.1)
        time.sleep(0.3)
    finally:
        chrome.kill()
        os.unlink(handle.name)
        shutil.rmtree(profile, ignore_errors=True)
    if not os.path.exists(target):
        sys.exit("ansi_to_png: Chrome produced no screenshot")

if __name__ == "__main__":
    main()
