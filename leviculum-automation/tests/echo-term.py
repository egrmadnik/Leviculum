#!/usr/bin/env python3
"""
echo-term — pošilja ECHC parametre na auto_echo LNode in dekodira ECHS echo.

Uporaba:
    python3 echo-term.py <delivery_hash> [sporocilo]
    python3 echo-term.py 85c390a37c34762a3af3b2024aa63fed "ping"

Wire format (leviculum-automation/src/frame.rs):
    frame = magic(4) + ver(1) + payload + crc32_ieee(4 LE)
    ECHC payload = len(1) + payload[32]      (33 B)
    ECHS payload = len(1) + payload[32] + applied(4 LE) + uptime(4 LE)  (41 B)
"""

import base64
import os
import queue
import subprocess
import sys
import threading
import time
import zlib

def _default_lxmf_node_bin() -> str:
    candidates = [
        os.path.expanduser("~/code/Leviculum/target/x86_64-unknown-linux-musl/release/lxmf-node"),
        os.path.expanduser("~/code/Leviculum/target/release/lxmf-node"),
    ]
    existing = [p for p in candidates if os.path.isfile(p)]
    if existing:
        existing.sort(key=lambda p: os.path.getmtime(p), reverse=True)
        return existing[0]
    return candidates[0]


LXMF_NODE_BIN = os.environ.get("LXMF_NODE_BIN", _default_lxmf_node_bin())
LNSD_CONFIG = os.environ.get(
    "LNSD_CONFIG",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "lnsd-config"),
)
LXMF_STORAGE = os.environ.get(
    "LXMF_STORAGE",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "lxmf-storage"),
)
ECHO_MAX = 32
REANNOUNCE_INTERVAL_S = 5.0


def echc_frame(payload: bytes) -> bytes:
    """ECHC v1: len + payload[32], ovit v frame envelope."""
    payload = payload[:ECHO_MAX]
    body = b"ECHC" + bytes([1, len(payload)]) + payload.ljust(ECHO_MAX, b"\x00")
    return body + zlib.crc32(body).to_bytes(4, "little")


def echs_decode(frame: bytes):
    """ECHS v1 → (content: bytes, applied: int, uptime_s: int)."""
    if len(frame) != 50:
        raise ValueError(f"bad len {len(frame)}")
    body, crc = frame[:-4], frame[-4:]
    if zlib.crc32(body) != int.from_bytes(crc, "little"):
        raise ValueError("bad crc")
    if body[:4] != b"ECHS" or body[4] != 1:
        raise ValueError("bad magic/ver")
    ln = body[5]
    return (
        body[6 : 6 + ln],
        int.from_bytes(body[38:42], "little"),
        int.from_bytes(body[42:46], "little"),
    )


def reader_thread(proc, q):
    for line in proc.stdout:
        q.put(line.rstrip("\n"))
    q.put(None)


def stderr_thread(proc):
    for line in proc.stderr:
        s = line.rstrip()
        if "PersistenceRequested" in s or "router event" in s:
            continue
        print(f"  \x1b[2m[node] {s}\x1b[0m", flush=True)


def main() -> int:
    dest = sys.argv[1].replace(" ", "").lower() if len(sys.argv) > 1 else ""
    payload = sys.argv[2].encode() if len(sys.argv) > 2 else b"ping"
    if len(dest) != 32 or not all(c in "0123456789abcdef" for c in dest):
        print(f"Uporaba: {sys.argv[0]} <delivery_hash> [sporocilo]", file=sys.stderr)
        return 2

    os.makedirs(LXMF_STORAGE, exist_ok=True)
    env = os.environ.copy()
    env["LXMF_STORAGE"] = LXMF_STORAGE
    proc = subprocess.Popen(
        [os.path.realpath(LXMF_NODE_BIN), "--config", LNSD_CONFIG, "echo-term"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env,
    )
    q = queue.Queue()
    threading.Thread(target=reader_thread, args=(proc, q), daemon=True).start()
    threading.Thread(target=stderr_thread, args=(proc,), daemon=True).start()

    def send(cmd):
        try:
            proc.stdin.write(cmd + "\n")
            proc.stdin.flush()
        except BrokenPipeError:
            pass

    # Počakaj na lxmf_ready preden pošljemo prve ukaze
    ready_deadline = time.monotonic() + 10
    while time.monotonic() < ready_deadline:
        try:
            line = q.get(timeout=0.5)
        except queue.Empty:
            continue
        if line is None:
            return 1
        if line.startswith("EVENT lxmf_ready"):
            break

    print("Čakam na XIAO announce (resetirajte če treba) ...")
    send("announce")
    send(f"wait_for_peer {dest} 60")
    last_announce = time.monotonic()

    peer = False
    deadline = time.monotonic() + 65
    while time.monotonic() < deadline:
        now = time.monotonic()
        if now - last_announce >= REANNOUNCE_INTERVAL_S:
            send("announce")
            last_announce = now
        try:
            line = q.get(timeout=1)
        except queue.Empty:
            continue
        if line is None:
            return 1
        if "lxmf_wait_for_peer_ok" in line:
            peer = True
            print("  peer najden")
            break
        if "lxmf_wait_for_peer_timeout" in line:
            print("  timeout — resetirajte XIAO in poskusite znova")
            proc.terminate()
            return 1

    if not peer:
        proc.terminate()
        return 1

    # Pošlji še en announce, da ima XIAO zagotovo našo povratno pot/identiteto,
    # nato pošlji ECHC frame kot opportunistično (single-packet) LXMF sporočilo.
    send("announce")
    frame = echc_frame(payload)
    b64_frame = base64.b64encode(frame).decode()
    print(f"→ ECHC {payload!r}  ({len(frame)} B frame)")
    send(f"send_opportunistic {dest} {b64_frame}")

    # Čakaj na ECHS echo
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            line = q.get(timeout=1)
        except queue.Empty:
            continue
        if line is None:
            return 1
        if line.startswith("EVENT lxmf_msg_received") or line.startswith("EVENT lxmf_received"):
            kv = dict(p.split("=", 1) for p in line.split()[2:] if "=" in p)
            src = (kv.get("src") or kv.get("source") or "").lower()
            if src and src != dest:
                continue
            body_b64 = kv.get("body_b64") or kv.get("body") or ""
            body = base64.b64decode(body_b64)
            try:
                content, applied, uptime = echs_decode(body)
                print(f"← ECHS {content!r}  applied={applied} uptime={uptime}s")
                print("   echo OK" if content == payload else "   echo MISMATCH!")
            except ValueError as e:
                print(f"← prejeto, ni ECHS: {e}  ({body[:16].hex()}…)")
            proc.terminate()
            return 0 if content == payload else 1
        if "lxmf_error" in line:
            print(f"  napaka: {line}")
            if "unknown_command:_send_opportunistic" in line:
                send(f"send {dest} {b64_frame}")

    print("  timeout — echo ni prišel v 30s")
    proc.terminate()
    return 1


if __name__ == "__main__":
    sys.exit(main())
