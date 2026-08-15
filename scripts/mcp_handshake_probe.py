#!/usr/bin/env python3
"""Probe mindreader MCP stdio handshake. Loads .env internally; never prints secrets."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

BIN = Path("/workspace/mindreader/target/debug/mindreader")
ENV_FILE = Path("/workspace/mindreader/.env")
TIMEOUT = 15.0
PROTOCOLS = ["2024-11-05", "2025-03-26", "2025-06-18"]


def load_env(path: Path) -> dict[str, str]:
    env = os.environ.copy()
    if not path.exists():
        return env
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def send(proc: subprocess.Popen, obj: dict) -> None:
    line = json.dumps(obj, separators=(",", ":")) + "\n"
    proc.stdin.write(line.encode("utf-8"))
    proc.stdin.flush()


def wait_id(stdout_buf: list[bytes], rid: int, proc: subprocess.Popen, deadline: float):
    while time.time() < deadline:
        raw = b"".join(stdout_buf).decode("utf-8", "replace")
        for line in raw.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("id") == rid:
                return obj
        if proc.poll() is not None:
            return None
        time.sleep(0.02)
    return None


def handshake(protocol: str, env: dict):
    t0 = time.time()
    proc = subprocess.Popen(
        [str(BIN)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        bufsize=0,
    )
    stderr_buf: list[bytes] = []
    stdout_buf: list[bytes] = []

    def pump(src, dest):
        while True:
            chunk = src.read(4096)
            if not chunk:
                break
            dest.append(chunk)

    threading.Thread(target=pump, args=(proc.stderr, stderr_buf), daemon=True).start()
    threading.Thread(target=pump, args=(proc.stdout, stdout_buf), daemon=True).start()

    # Host-like: send initialize immediately (do not wait for Neo4j).
    send(
        proc,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol,
                "capabilities": {},
                "clientInfo": {"name": "mindreader-probe", "version": "0.0.1"},
            },
        },
    )
    init = wait_id(stdout_buf, 1, proc, t0 + TIMEOUT)
    tools = None
    if init is not None:
        send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        tools = wait_id(stdout_buf, 2, proc, time.time() + TIMEOUT)
    try:
        proc.kill()
        proc.wait(timeout=2)
    except Exception:
        pass
    return init, tools, b"".join(stderr_buf).decode("utf-8", "replace"), time.time() - t0


def main() -> int:
    if not BIN.exists():
        print(f"missing binary: {BIN}", file=sys.stderr)
        return 2
    env = load_env(ENV_FILE)
    print(f"BIN={BIN}")
    print(f"NEO4J_URI={env.get('NEO4J_URI')}")
    print(f"MINDREADER_PROJECT={env.get('MINDREADER_PROJECT')}")
    print(f"NEO4J_USER={env.get('NEO4J_USER')}")
    print(f"NEO4J_PASSWORD_SET={'yes' if env.get('NEO4J_PASSWORD') else 'no'}")

    for proto in PROTOCOLS:
        print("=" * 72)
        print(f"PROTOCOL {proto}")
        init, tools, stderr, elapsed = handshake(proto, env)
        print(f"elapsed_s={elapsed:.3f}")
        print("--- initialize ---")
        print(json.dumps(init, indent=2) if init is not None else "TIMEOUT/NONE")
        print("--- tools/list ---")
        print(json.dumps(tools, indent=2) if tools is not None else "TIMEOUT/NONE")
        print("--- stderr ---")
        print(stderr)
        if init is not None:
            names = []
            if tools and isinstance(tools.get("result"), dict):
                names = [t.get("name") for t in tools["result"].get("tools") or []]
            print(f"--- tool names ---")
            print(names)
            return 0
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
