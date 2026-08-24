#!/usr/bin/env python3
import array
import os
import socket
import sys
import time

path, out_path = sys.argv[1], sys.argv[2]
deadline = time.monotonic() + 20

if os.path.exists(path):
    os.unlink(path)

listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(path)
listener.listen(1)
listener.settimeout(20)
print("listening", flush=True)

conn, _ = listener.accept()
payload, fds, _, _ = socket.recv_fds(conn, 4096, 1)

if not fds:
    sys.exit("no file descriptor arrived over SCM_RIGHTS")

master = fds[0]
print("pty name from the runtime:", payload.decode(errors="replace"), flush=True)
print("isatty(master):", os.isatty(master), flush=True)

collected = bytearray()
while time.monotonic() < deadline:
    try:
        chunk = os.read(master, 4096)
    except OSError:
        break
    if not chunk:
        break
    collected += chunk

with open(out_path, "wb") as handle:
    handle.write(collected)

os.close(master)
conn.close()
listener.close()
os.unlink(path)
