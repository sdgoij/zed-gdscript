"""Drive the bridge over stdio the way Zed does, then kill its engine.

Proves, in order: initialize is answered, a document gets diagnostics, the
engine dying is reported and restarted, and the replayed session answers again.
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path

BRIDGE = r"C:\Users\T\src\github.com\sdgoij\goats\zed-gdscript\bridge\target\release\godot-lsp-bridge.exe"
GODOT = r"C:\Users\T\bin\Godot_v4.7.2-stable_win64.exe"
PROJECT = Path(r"C:\Users\T\src\github.com\sdgoij\goats\godot")

# `py bridge/drive.py env` runs the bridge the way the extension starts it: no
# arguments at all, the engine from GODOT_BIN, the project from `initialize`, and
# a working directory that has nothing to do with either.
MODE = sys.argv[1] if len(sys.argv) > 1 else "kill"
if MODE == "env":
    ENV = dict(os.environ, GODOT_BIN=GODOT)
    ARGS = [BRIDGE]
    CWD = os.environ.get("TEMP")
else:
    ENV = dict(os.environ)
    ARGS = [BRIDGE, "--godot", GODOT, "--project", str(PROJECT)]
    CWD = None

proc = subprocess.Popen(
    ARGS,
    cwd=CWD,
    env=ENV,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(r"C:\Users\T\AppData\Local\Temp\bridge-stderr.log", "wb"),
)
messages = queue.Queue()


def reader():
    stream = proc.stdout
    while True:
        length = None
        while True:
            line = stream.readline()
            if not line:
                messages.put(None)
                return
            line = line.strip()
            if not line:
                if length is not None:
                    break
                continue
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        body = stream.read(length)
        try:
            messages.put(json.loads(body))
        except Exception:
            pass


threading.Thread(target=reader, daemon=True).start()


def send(obj):
    body = json.dumps(obj).encode()
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body))
    proc.stdin.write(body)
    proc.stdin.flush()


def wait_for(method=None, timeout=120):
    end = time.time() + timeout
    while time.time() < end:
        try:
            message = messages.get(timeout=0.5)
        except queue.Empty:
            continue
        if message is None:
            raise SystemExit("the bridge closed stdout")
        if method is None or message.get("method") == method:
            return message
    raise SystemExit(f"timed out waiting for {method}")


root_uri = PROJECT.as_uri()
send({
    "jsonrpc": "2.0", "id": 1, "method": "initialize",
    "params": {"processId": None, "rootUri": root_uri, "capabilities": {}},
})


def wait_id(wanted, timeout=180):
    end = time.time() + timeout
    while time.time() < end:
        try:
            message = messages.get(timeout=0.5)
        except queue.Empty:
            continue
        if message is None:
            raise SystemExit("the bridge closed stdout")
        if message.get("id") == wanted:
            return message
    raise SystemExit(f"timed out waiting for id {wanted}")


response = wait_id(1)
caps = response.get("result", {}).get("capabilities", {})
print("1. initialize answered:", json.dumps(caps)[:120])
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

target = PROJECT / "src" / "main.gd"
uri = target.as_uri()
probe = PROJECT / "src" / "__bridge_probe.gd"
probe_uri = probe.as_uri()
probe_uid = probe.parent / (probe.name + ".uid")


def drop_probe():
    """Godot writes a `.uid` beside every script it scans; both have to go."""
    probe.unlink(missing_ok=True)
    probe_uid.unlink(missing_ok=True)
send({
    "jsonrpc": "2.0", "method": "textDocument/didOpen",
    "params": {"textDocument": {
        "uri": uri, "languageId": "gdscript", "version": 1,
        "text": target.read_text(encoding="utf-8"),
    }},
})
first = wait_for("textDocument/publishDiagnostics", 120)
print("2. diagnostics before the kill:", len(first["params"].get("diagnostics", [])))

if MODE == "env":
    # The engine started, so GODOT_BIN and the worktree root were both enough.
    send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    proc.wait(timeout=30)
    print("3. no-argument bridge exited with", proc.returncode)
    sys.exit(0)

print("3. killing the engine...")
probe.write_text("extends Node\n\nfunc broken(:\n", encoding="utf-8")
subprocess.run(["taskkill", "/F", "/IM", "Godot_v4.7.2-stable_win64.exe"],
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

seen = []
deadline = time.time() + 120
second = None
restarted = False
while time.time() < deadline:
    try:
        message = messages.get(timeout=1.0)
    except queue.Empty:
        continue
    if message is None:
        raise SystemExit("the bridge closed stdout")
    method = message.get("method")
    if method:
        seen.append(method)
        print("   <- notification", method)
    else:
        print("   <- response id", message.get("id"))
    if method == "window/showMessage":
        print("4. message from the bridge:", message["params"]["message"])
        if "answering again" in message["params"]["message"]:
            restarted = True
            # A file no engine has seen: a live session after the restart has to
            # answer this, which also proves the replayed `initialize` took.
            send({
                "jsonrpc": "2.0", "method": "textDocument/didOpen",
                "params": {"textDocument": {
                    "uri": probe_uri, "languageId": "gdscript", "version": 1,
                    "text": probe.read_text(encoding="utf-8"),
                }},
            })
            print("5. opened a file with a syntax error after the restart")
    if method == "textDocument/publishDiagnostics":
        got = message["params"].get("uri", "")
        count = len(message["params"].get("diagnostics", []))
        print("   diagnostics for", got.rsplit("/", 1)[-1], "->", count)
        if restarted and (got.endswith("__bridge_probe.gd") or got.lower() == probe_uri.lower()):
            second = message
            if count > 0:
                break

if second is None or not second["params"].get("diagnostics"):
    print("6. FAILED: no diagnostics for the broken file after the restart; seen:", sorted(set(seen)))
    drop_probe()
    sys.exit(1)
print("6. diagnostics after the restart:", len(second["params"]["diagnostics"]))
print("   notifications seen meanwhile:", sorted(set(seen)))

send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
send({"jsonrpc": "2.0", "method": "exit", "params": None})
proc.wait(timeout=30)
drop_probe()
print("7. bridge exited with", proc.returncode)
