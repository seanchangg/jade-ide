#!/usr/bin/env python3
"""Stand-in for jade-ide's telemetry server: verifies the probe's socket
protocol end-to-end. Tracks only model.weights and reports what arrives."""
import json, os, socket, subprocess, sys, threading, time

SOCK = "/tmp/jade-telemetry-test.sock"
if os.path.exists(SOCK):
    os.unlink(SOCK)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(SOCK)
srv.listen(1)

stats = {"decl": [], "timing": 0, "tensor": {}, "scalar": 0}

def handle(conn):
    f = conn.makefile("r")
    for line in f:
        try:
            m = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = m.get("type")
        if t == "decl":
            stats["decl"].append(f'{m.get("kind")}:{m.get("name")}')
            # The IDE-side rule: user checked model.weights in the sidebar.
            if m.get("kind") == "buffer" and m.get("name") == "model.weights":
                conn.sendall((json.dumps({"type": "track", "kind": "buffer",
                    "name": "model.weights", "enabled": True, "maxDim": 32}) + "\n").encode())
        elif t == "timing":
            stats["timing"] += 1
        elif t == "tensor":
            stats["tensor"][m["name"]] = stats["tensor"].get(m["name"], 0) + 1
            stats["shape"] = (m["rows"], m["cols"])
        elif t == "scalar":
            stats["scalar"] += 1

env = dict(os.environ, DYLD_INSERT_LIBRARIES=os.path.abspath("jade_probe.dylib"),
           JADE_TELEMETRY_SOCK=SOCK)
env.pop("JADE_TRACK_ALL", None)  # selection must come from track messages only
proc = subprocess.Popen(["./test_train"], env=env)

conn, _ = srv.accept()
th = threading.Thread(target=handle, args=(conn,), daemon=True)
th.start()
proc.wait()
time.sleep(0.5)

print("decls:", stats["decl"])
print("timings received:", stats["timing"])
print("tensor frames by buffer:", stats["tensor"])
print("tensor shape (maxDim=32 requested):", stats.get("shape"))
ok = (stats["tensor"].get("model.weights", 0) > 0
      and "model.grads" not in stats["tensor"]
      and "buffer#2" not in stats["tensor"]
      and stats.get("shape") == (32, 32)
      and stats["timing"] > 10)
print("END-TO-END:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
