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
PROTOCOL = "2026-07-28"
LEGACY_PROTOCOLS = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]
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
        key = k.strip()
        if not env.get(key):
            env[key] = v.strip().strip('"').strip("'")
    return env


def send(proc: subprocess.Popen, obj: dict) -> None:
    line = json.dumps(obj, separators=(",", ":")) + "\n"
    proc.stdin.write(line.encode("utf-8"))
    proc.stdin.flush()


class StdoutProtocolError(RuntimeError):
    """Raised when serving-mode stdout contains anything except JSON-RPC."""


def parse_stdout(stdout_buf: list[bytes], *, include_partial: bool = False) -> list[dict]:
    raw = b"".join(stdout_buf).decode("utf-8", "replace")
    messages = []
    for part in raw.splitlines(keepends=True):
        if not include_partial and not part.endswith(("\n", "\r")):
            continue
        line = part.strip()
        if not line:
            raise StdoutProtocolError("empty non-JSON line on stdout")
        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            raise StdoutProtocolError(f"non-JSON stdout: {line!r}") from exc
        if not isinstance(message, dict):
            raise StdoutProtocolError(f"non-object JSON-RPC stdout: {line!r}")
        messages.append(message)
    return messages


def wait_id(stdout_buf: list[bytes], rid: int, proc: subprocess.Popen, deadline: float):
    while time.time() < deadline:
        for obj in parse_stdout(stdout_buf):
            if obj.get("id") == rid:
                return obj
        if proc.poll() is not None:
            return None
        time.sleep(0.02)
    return None


def request_meta() -> dict:
    return {
        "io.modelcontextprotocol/protocolVersion": PROTOCOL,
        "io.modelcontextprotocol/clientInfo": {
            "name": "mindreader-probe",
            "version": "0.0.1",
        },
        "io.modelcontextprotocol/clientCapabilities": {},
    }


def run_exchange(binary: Path, env: dict[str, str], timeout: float, first: dict):
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

    stderr_thread = threading.Thread(
        target=pump, args=(proc.stderr, stderr_buf), daemon=True
    )
    stdout_thread = threading.Thread(
        target=pump, args=(proc.stdout, stdout_buf), daemon=True
    )
    stderr_thread.start()
    stdout_thread.start()

    send(proc, first)
    first_result = None
    tools = None
    contamination = None
    try:
        first_result = wait_id(stdout_buf, 1, proc, t0 + timeout)
        if first["method"] == "server/discover" and first_result is not None:
            send(
                proc,
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                    "params": {"_meta": request_meta()},
                },
            )
            tools = wait_id(stdout_buf, 2, proc, time.time() + timeout)
    except StdoutProtocolError as exc:
        contamination = str(exc)
    try:
        proc.kill()
        proc.wait(timeout=2)
    except Exception:
        pass
    stdout_thread.join(timeout=1)
    stderr_thread.join(timeout=1)
    try:
        parse_stdout(stdout_buf, include_partial=True)
    except StdoutProtocolError as exc:
        contamination = str(exc)
    return (
        first_result,
        tools,
        b"".join(stderr_buf).decode("utf-8", "replace"),
        contamination,
        time.time() - t0,
    )


def discover(binary: Path, env: dict[str, str], timeout: float):
    return run_exchange(
        binary,
        env,
        timeout,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {"_meta": request_meta()},
        },
    )


def initialize(binary: Path, protocol: str, env: dict[str, str], timeout: float):
    return run_exchange(
        binary,
        env,
        timeout,
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
    if not env.get("NEO4J_PASSWORD"):
        env["NEO4J_PASSWORD"] = "mindreader-handshake-probe"
    print(f"BIN={binary}")
    print(f"NEO4J_PASSWORD_SET={'yes' if env.get('NEO4J_PASSWORD') else 'no'}")
    with tempfile.TemporaryDirectory(prefix="mindreader-handshake-") as config_home:
        env["XDG_CONFIG_HOME"] = config_home
        env["APPDATA"] = config_home
        env["HOME"] = config_home
        print(f"CONFIG_HOME={config_home}")

        print("=" * 72)
        print(f"DISCOVER {PROTOCOL}")
        discovery, tools, stderr, contamination, elapsed = discover(
            binary, env, args.timeout
        )
        print(f"elapsed_s={elapsed:.3f}")
        print("--- server/discover ---")
        print(json.dumps(discovery, indent=2) if discovery is not None else "TIMEOUT/NONE")
        print("--- tools/list ---")
        print(json.dumps(tools, indent=2) if tools is not None else "TIMEOUT/NONE")
        print("--- stderr ---")
        print(stderr)
        print(f"stdout_contamination={contamination or 'none'}")
        listed = []
        if tools and isinstance(tools.get("result"), dict):
            listed = tools["result"].get("tools") or []
        print("--- tool names ---")
        print([tool.get("name") for tool in listed])
        succeeded = contamination is None and discovery_contract_ok(discovery, tools)

        print("=" * 72)
        print(f"INITIALIZE {PROTOCOL}")
        response, _, stderr, contamination, elapsed = initialize(
            binary, PROTOCOL, env, args.timeout
        )
        print(f"elapsed_s={elapsed:.3f}")
        print(json.dumps(response, indent=2) if response is not None else "TIMEOUT/NONE")
        print("--- stderr ---")
        print(stderr)
        print(f"stdout_contamination={contamination or 'none'}")
        if contamination is not None or not initialized_current_protocol(response):
            succeeded = False

        for protocol in [*LEGACY_PROTOCOLS, UNKNOWN_PROTOCOL]:
            print("=" * 72)
            print(f"REJECT INITIALIZE {protocol}")
            response, _, stderr, contamination, elapsed = initialize(
                binary, protocol, env, args.timeout
            )
            print(f"elapsed_s={elapsed:.3f}")
            print(json.dumps(response, indent=2) if response is not None else "TIMEOUT/NONE")
            print("--- stderr ---")
            print(stderr)
            print(f"stdout_contamination={contamination or 'none'}")
            if contamination is not None or not rejected_protocol(response):
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


def discovery_contract_ok(discovery: dict | None, tools: dict | None) -> bool:
    result = ((discovery or {}).get("result") or {})
    if result.get("resultType") != "complete":
        return False
    if result.get("supportedVersions") != [PROTOCOL]:
        return False
    caps = result.get("capabilities") or {}
    tools_cap = caps.get("tools")
    if not isinstance(tools_cap, dict):
        return False
    if tools_cap.get("listChanged") not in (None, False):
        return False
    tools_result = ((tools or {}).get("result") or {})
    if tools_result.get("resultType") != "complete":
        return False
    listed = tools_result.get("tools") or []
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


def rejected_protocol(response: dict | None) -> bool:
    return (
        isinstance(response, dict)
        and "result" not in response
        and isinstance(response.get("error"), dict)
        and response["error"].get("code") == -32022
    )


def initialized_current_protocol(response: dict | None) -> bool:
    return (
        isinstance(response, dict)
        and "error" not in response
        and ((response.get("result") or {}).get("protocolVersion") == PROTOCOL)
    )


if __name__ == "__main__":
    raise SystemExit(main())
