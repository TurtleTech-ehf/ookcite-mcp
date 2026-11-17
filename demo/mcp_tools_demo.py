#!/usr/bin/env python3
"""Call the four discovery and bibliography tools through a real MCP session.

Starts demo/mock_upstream.py, starts the server binary built with
`--features demo-mock` against it, speaks MCP over stdio, and prints each call
and result in colour. No live host is contacted.

    cargo build --features demo-mock
    demo/mcp_tools_demo.py [--tool NAME] [path/to/ookcite-mcp]

--tool limits the calls to one tool, so each can be captured on its own.
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ARGS = sys.argv[1:]
ONLY = None
if "--tool" in ARGS:
    at = ARGS.index("--tool")
    ONLY = ARGS[at + 1]
    del ARGS[at : at + 2]
BIN = ARGS[0] if ARGS else os.path.join(HERE, "..", "target", "debug", "ookcite-mcp")

BIB = """@article{clean2020, title={Crystal nucleation in sheared colloidal glasses}, author={Ada Example and Ben Sample}, year={2020}, doi={10.5555/clean.2020}}
@article{clean2020b, title={Crystal nucleation in sheared colloidal glasses (extended)}, year={2020}, doi={10.5555/clean.2020}}
@article{retracted2019, title={A claim that did not survive}, year={2019}, doi={10.5555/retracted.2019}}
@misc{preprint2021, title={Shear-induced ordering, a preprint}, year={2021}, doi={10.5555/preprint.2021}}
@article{published2021, title={Shear-induced ordering}, year={2021}, doi={10.5555/pub.2021}}
@misc{arxiv2022, title={An arXiv-only study}, year={2022}, doi={10.48550/arXiv.2201.00001}}
@article{nodoi2018, title={Fast crystallisation of sheared colloidal suspensions}, author={Smith, Jo}, year={2018}}
@article{ghost2017, title={An entirely unknown manuscript about nothing}, author={Doe, Jane}, year={2017}}
"""

CALLS = [
    ("find_open_access_pdf", {"doi": "10.5555/demo.oa.2024"}),
    ("find_open_access_pdf", {"doi": "10.5555/demo.limited"}),
    ("related_papers", {"doi": "10.5555/demo.seed", "relation": "related", "limit": 2}),
    ("cite_while_writing", {"claim": "Sheared colloidal glasses nucleate crystals faster than quiescent suspensions do.", "limit": 2}),
    ("check_bibliography", {"content": BIB}),
]

BOLD, DIM, RESET = "\033[1m", "\033[2m", "\033[0m"
CYAN, GREEN, YELLOW, RED, MAGENTA = "\033[36m", "\033[32m", "\033[33m", "\033[31m", "\033[35m"


def colour(line):
    rules = [
        ("RATE LIMITED", RED), ("PAYMENT REQUIRED", RED), ("RETRACTED", RED), ("HELD BACK", RED),
        ("UPGRADE", YELLOW), ("NO DOI", YELLOW), ("UNCHECKED", YELLOW), ("NOT OPEN ACCESS", YELLOW),
        ("OPEN ACCESS", GREEN), ("CITATION CANDIDATES", CYAN), ("RELATED WORKS", CYAN),
        ("BIBLIOGRAPHY CHECK", CYAN), ("Summary:", BOLD), ("Duplicates:", MAGENTA),
    ]
    for key, code in rules:
        if key in line:
            return line.replace(key, f"{code}{BOLD}{key}{RESET}", 1)
    if line.startswith("[") and "] OK" in line:
        return line.replace("] OK", f"] {GREEN}OK{RESET}", 1)
    return line


class Client:
    def __init__(self, base):
        env = dict(os.environ, OOKCITE_DEMO_MOCK_BASE=base)
        env.pop("OOKCITE_API_KEY", None)
        env["OOKCITE_MCP_CONTACT_EMAIL"] = "demo@example.org"
        self.proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
        self.next_id = 0

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            reply = json.loads(self.proc.stdout.readline())
            if reply.get("id") == self.next_id:
                return reply

    def start(self):
        self.request("initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "demo-client", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, name, arguments):
        return self.request("tools/call", {"name": name, "arguments": arguments})["result"]


def main():
    mock = subprocess.Popen([sys.executable, os.path.join(HERE, "mock_upstream.py")], stdout=subprocess.PIPE, text=True)
    base = mock.stdout.readline().strip()
    client = Client(base)
    try:
        client.start()
        tools = client.request("tools/list", {})["result"]["tools"]
        names = [t["name"] for t in tools if t["name"] in {c[0] for c in CALLS}]
        print(f"{DIM}$ ookcite-mcp (demo-mock build) against {base}  -- no live host{RESET}")
        print(f"{DIM}tools/list includes: {', '.join(sorted(names))}{RESET}\n")
        for name, arguments in CALLS:
            if ONLY and name != ONLY:
                continue
            shown = json.dumps(arguments)
            if len(shown) > 110:
                shown = json.dumps({**arguments, "content": arguments["content"].splitlines()[0][:40] + " ... (8 entries)"})
            print(f"{BOLD}{GREEN}> {name}{RESET} {shown}")
            result = client.call(name, arguments)
            text = "\n".join(block.get("text", "") for block in result["content"])
            for line in text.splitlines():
                print(colour(line))
            if result.get("isError"):
                structured = result.get("structuredContent", {})
                print(f"{DIM}isError=true structuredContent: {json.dumps(structured)}{RESET}")
            print()
    finally:
        client.proc.kill()
        mock.kill()


if __name__ == "__main__":
    main()
