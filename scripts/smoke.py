#!/usr/bin/env python3
"""Exercise a packaged binary and independently check its SHA-256 record."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="strata-smoke-", dir="/tmp") as scratch:
    root = Path(scratch)
    events, socket = root / "events", root / "s"
    payload = {"client": "alice", "content": "游泳 🏊", "duration": 30}
    previous_receipt = None
    for restart in range(2):
        daemon = subprocess.Popen(
            [binary, "serve", "--dir", str(events), "--socket", str(socket)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 10
            while True:
                if daemon.poll() is not None:
                    raise RuntimeError(daemon.stderr.read().decode())
                # A stale socket can exist during restart; retrying the same ID
                # is safe even if this readiness check races socket replacement.
                result = subprocess.run(
                    [binary, "append", "--socket", str(socket), "--type",
                     "client.exercise", "--id", "smoke-swim"],
                    input=json.dumps(payload), text=True, capture_output=True,
                    timeout=10,
                )
                if result.returncode == 0:
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError(result.stderr)
                time.sleep(0.05)
            receipt = json.loads(result.stdout)
            if previous_receipt is not None:
                assert receipt == previous_receipt, "restart retry duplicated event"
            previous_receipt = receipt
            lines = (events / receipt["file"]).read_bytes().splitlines(keepends=True)
            assert len(lines) == 1
            envelope = json.loads(lines[0])
            event_bytes = lines[0].split(b',"event":', 1)[1][:-2]
            expected = hashlib.sha256(b"strata-event-v1\0" + event_bytes).hexdigest()
            assert envelope["hash"] == expected == receipt["hash"]
            assert envelope["event"]["data"] == payload
            assert envelope["event"]["previous"] is None
            assert envelope["event"]["sequence"] == 1
        finally:
            if daemon.poll() is None:
                daemon.kill()
            daemon.wait(timeout=10)
            daemon.stderr.close()
        result = subprocess.run(
            [binary, "verify", "--dir", str(events)],
            check=True, capture_output=True, text=True,
        )
        assert json.loads(result.stdout)["events"] == 1
print("Packaged binary: append, independent hash, crash/restart, retry and verify passed")
