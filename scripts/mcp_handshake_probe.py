#!/usr/bin/env python3
"""Probe Mindreader's MCP stdio handshake without printing secrets."""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROTOCOLS = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]
UNKNOWN_PROTOCOL = "2099-01-01"


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


def handshake(binary: Path, protocol: str, env: dict[str, str], timeout: float):
    t0 = time.time()
    proc = subprocess.Popen(
        [str(binary)],
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
    init = wait_id(stdout_buf, 1, proc, t0 + timeout)
    tools = None
    if init is not None:
        send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        tools = wait_id(stdout_buf, 2, proc, time.time() + timeout)
    try:
        proc.kill()
        proc.wait(timeout=2)
    except Exception:
        pass
    return init, tools, b"".join(stderr_buf).decode("utf-8", "replace"), time.time() - t0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/mindreader")
    parser.add_argument("--env-file", type=Path, default=ROOT / ".env")
    parser.add_argument("--timeout", type=float, default=15.0)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    binary = args.binary.resolve()
    if not binary.exists():
        print(f"missing binary: {binary}", file=sys.stderr)
        return 2
    env = load_env(args.env_file)
    env.setdefault("NEO4J_PASSWORD", "mindreader-handshake-probe")
    print(f"BIN={binary}")
    print(f"NEO4J_PASSWORD_SET={'yes' if env.get('NEO4J_PASSWORD') else 'no'}")
    with tempfile.TemporaryDirectory(prefix="mindreader-handshake-") as config_home:
        env["XDG_CONFIG_HOME"] = config_home
        env["APPDATA"] = config_home
        env["HOME"] = config_home
        print(f"CONFIG_HOME={config_home}")

        succeeded = True
        for proto in PROTOCOLS:
            print("=" * 72)
            print(f"PROTOCOL {proto}")
            init, tools, stderr, elapsed = handshake(binary, proto, env, args.timeout)
            print(f"elapsed_s={elapsed:.3f}")
            print("--- initialize ---")
            print(json.dumps(init, indent=2) if init is not None else "TIMEOUT/NONE")
            print("--- tools/list ---")
            print(json.dumps(tools, indent=2) if tools is not None else "TIMEOUT/NONE")
            print("--- stderr ---")
            print(stderr)
            names = []
            listed = []
            if tools and isinstance(tools.get("result"), dict):
                listed = tools["result"].get("tools") or []
                names = [t.get("name") for t in listed]
            print("--- tool names ---")
            print(names)
            negotiated = (init or {}).get("result", {}).get("protocolVersion")
            print(f"negotiated={negotiated}")
            if init is None or negotiated != proto or not handshake_contract_ok(init, listed):
                succeeded = False

        print("=" * 72)
        print(f"UNKNOWN PROTOCOL {UNKNOWN_PROTOCOL}")
        init, tools, stderr, elapsed = handshake(
            binary, UNKNOWN_PROTOCOL, env, args.timeout
        )
        negotiated = (init or {}).get("result", {}).get("protocolVersion")
        print(f"elapsed_s={elapsed:.3f}")
        print(f"negotiated={negotiated}")
        print("--- stderr ---")
        print(stderr)
        names = []
        if tools and isinstance(tools.get("result"), dict):
            names = [t.get("name") for t in tools["result"].get("tools") or []]
        print("--- tool names ---")
        print(names)
        if init is None or negotiated == UNKNOWN_PROTOCOL or not handshake_contract_ok(
            init, [t for t in ((tools or {}).get("result") or {}).get("tools") or []]
        ):
            succeeded = False
        return 0 if succeeded else 1


UNION_KEYS = {"anyOf", "oneOf", "allOf"}
EXPECTED_TOOLS = {
    "memory_judge",
    "memory_place",
    "memory_recall",
    "memory_recall_semantic",
    "memory_revise",
    "memory_unify",
    "memory_withdraw",
    "memory_write",
}


def contains_union(value) -> bool:
    if isinstance(value, dict):
        if UNION_KEYS.intersection(value):
            return True
        return any(contains_union(child) for child in value.values())
    if isinstance(value, list):
        return any(contains_union(child) for child in value)
    return False


def handshake_contract_ok(init: dict, listed: list) -> bool:
    caps = ((init or {}).get("result") or {}).get("capabilities") or {}
    tools_cap = caps.get("tools")
    if not isinstance(tools_cap, dict):
        return False
    if tools_cap.get("listChanged") not in (None, False):
        return False
    names = [tool.get("name") for tool in listed]
    if set(names) != EXPECTED_TOOLS or len(names) != 8:
        return False
    for tool in listed:
        if not isinstance(tool.get("title"), str) or not tool["title"].strip():
            return False
        if not isinstance(tool.get("annotations"), dict):
            return False
        if contains_union(tool.get("inputSchema")) or contains_union(tool.get("outputSchema")):
            return False
        if not isinstance(tool.get("outputSchema"), dict):
            return False
    return True


if __name__ == "__main__":
    raise SystemExit(main())
