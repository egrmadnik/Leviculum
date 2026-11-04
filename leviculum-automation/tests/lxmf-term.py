#!/usr/bin/env python3
"""
lxmf-term — interaktivni LXMF terminal za testiranje lxmf_echo LNode.

Postopek:
  1. Zagotovite da teče:   ~/lxmf-test/run-lnsd.sh
  2. Zaženite terminal:    python3 lxmf-term.py 1ef5d2e9a58ef444f16740b806aa2080
  3. Pritisnite RESET na XIAO (pošlje announce čez ~10s)
  4. Ko se pojavi prompt "> ", vtipkajte sporočilo

Protokol lxmf-node:
  stdin  → "announce"
  stdin  → "wait_for_peer <hex16> <timeout_secs>"
  stdin  → "send <hex16> <base64body>"
  stdout ← "EVENT lxmf_ready hash=<hex>"
  stdout ← "EVENT lxmf_wait_for_peer_ok peer=<hex>"
  stdout ← "EVENT lxmf_msg_received src=<hex> body_b64=<b64> ..."
  stdout ← "EVENT lxmf_msg_sent body_b64=<b64> ..."
"""

import base64
import os
import queue
import readline  # noqa: F401  — lepši vnos v terminalu
import subprocess
import sys
import threading
import time

# ---------------------------------------------------------------------------
# Konfiguracija
# ---------------------------------------------------------------------------

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

# Timeout za wait_for_peer v sekundah.
# XIAO pošlje announce ~10s po RESET-u. Damo 60s za varnostno rezervo.
PEER_WAIT_TIMEOUT = int(os.environ.get("PEER_WAIT_TIMEOUT", "60"))


# ---------------------------------------------------------------------------
# Pomožne funkcije
# ---------------------------------------------------------------------------

def b64enc(data: bytes) -> str:
    return base64.b64encode(data).decode()


def b64dec(s: str) -> bytes:
    return base64.b64decode(s)


def fmt_hash(h: str) -> str:
    return h[:8] if len(h) >= 8 else h


def fmt_body(b64: str) -> str:
    try:
        return b64dec(b64).decode("utf-8")
    except Exception:
        return f"<binary:{b64dec(b64).hex()}>"


def parse_kv(parts: list) -> dict:
    result = {}
    for p in parts:
        if "=" in p:
            k, v = p.split("=", 1)
            result[k] = v
    return result


# ---------------------------------------------------------------------------
# Niti
# ---------------------------------------------------------------------------

def reader_thread(proc, ev_queue):
    for line in proc.stdout:
        ev_queue.put(line.rstrip("\n"))
    ev_queue.put(None)


_STDERR_SKIP = ("PersistenceRequested", "router event: Persistence")

def stderr_thread(proc):
    for line in proc.stderr:
        s = line.rstrip()
        if any(skip in s for skip in _STDERR_SKIP):
            continue
        print(f"\r  \x1b[2m[node] {s}\x1b[0m")
        print("\r> ", end="", flush=True)


# ---------------------------------------------------------------------------
# Prikaz dogodkov
# ---------------------------------------------------------------------------

def handle_event(line):
    if not line.startswith("EVENT "):
        return
    parts = line.split()
    etype = parts[1] if len(parts) > 1 else "?"
    kv = parse_kv(parts[2:])

    if etype in ("lxmf_msg_received", "lxmf_received"):
        src = kv.get("src") or kv.get("source") or "?"
        b64 = kv.get("body_b64") or kv.get("body") or ""
        body = fmt_body(b64) if b64 else "<prazno>"
        print(f"\r\n\x1b[32m  ← ECHO od {fmt_hash(src)}: {body!r}\x1b[0m")

    elif etype == "lxmf_msg_sent":
        body = fmt_body(kv.get("body_b64", "")) if kv.get("body_b64") else ""
        print(f"\r\x1b[90m  ✓ poslano: {body!r}\x1b[0m")

    elif etype == "lxmf_error":
        detail = kv.get("detail", "?")
        print(f"\r\x1b[31m  ✗ napaka: {detail}\x1b[0m")

    elif etype not in ("lxmf_ready", "lxmf_announce_sent", "lxmf_wait_for_peer_timeout"):
        print(f"\r  [{etype}] {' '.join(f'{k}={v}' for k, v in kv.items())}")


def drain(ev_queue, timeout=0.05):
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        try:
            line = ev_queue.get(timeout=max(remaining, 0))
            if line is None:
                return False
            handle_event(line)
        except queue.Empty:
            return True


# ---------------------------------------------------------------------------
# Glavna zanka
# ---------------------------------------------------------------------------

def main():
    if len(sys.argv) < 2:
        dest = "1ef5d2e9a58ef444f16740b806aa2080"
        print(f"Uporaba: {sys.argv[0]} <delivery_hash>")
        print(f"Privzeto: {dest}")
    else:
        dest = sys.argv[1].replace(" ", "").lower()

    if len(dest) != 32 or not all(c in "0123456789abcdef" for c in dest):
        print("Napaka: delivery_hash mora biti 32 hex znakov", file=sys.stderr)
        return 2

    lxmf_node = os.path.realpath(LXMF_NODE_BIN)
    if not os.path.isfile(lxmf_node):
        print(f"Napaka: lxmf-node ni najden: {lxmf_node}", file=sys.stderr)
        return 1

    os.makedirs(LXMF_STORAGE, exist_ok=True)
    env = os.environ.copy()
    env["LXMF_STORAGE"] = LXMF_STORAGE

    try:
        proc = subprocess.Popen(
            [lxmf_node, "--config", LNSD_CONFIG, "lxmf-term"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
    except FileNotFoundError:
        print(f"Napaka: ne morem zagnati {lxmf_node}", file=sys.stderr)
        return 1

    ev_queue = queue.Queue()
    threading.Thread(target=reader_thread, args=(proc, ev_queue), daemon=True).start()
    threading.Thread(target=stderr_thread, args=(proc,), daemon=True).start()

    def send_cmd(line):
        try:
            proc.stdin.write(line + "\n")
            proc.stdin.flush()
        except BrokenPipeError:
            pass

    # Announce + wait_for_peer
    time.sleep(0.5)
    send_cmd("announce")

    print(f"\x1b[1mlxmf-term\x1b[0m — cilj: \x1b[33m{dest}\x1b[0m")
    print(f"\x1b[33m► Pritisnite RESET na XIAO, nato čakam {PEER_WAIT_TIMEOUT}s na announce...\x1b[0m")
    send_cmd(f"wait_for_peer {dest} {PEER_WAIT_TIMEOUT}")
    last_announce = time.monotonic()

    peer_found = False
    deadline = time.monotonic() + PEER_WAIT_TIMEOUT + 5
    while time.monotonic() < deadline:
        now = time.monotonic()
        if now - last_announce >= 5.0:
            send_cmd("announce")
            last_announce = now
        try:
            line = ev_queue.get(timeout=1.0)
        except queue.Empty:
            continue
        if line is None:
            print("lxmf-node se je zaprl.", file=sys.stderr)
            return 1
        if "lxmf_wait_for_peer_ok" in line:
            peer_found = True
            print(f"\x1b[32m  ✓ XIAO announce prejet! Pošiljanje omogočeno.\x1b[0m")
            break
        elif "lxmf_wait_for_peer_timeout" in line:
            print(f"\x1b[31m  ✗ Timeout — XIAO ni odgovoril v {PEER_WAIT_TIMEOUT}s.\x1b[0m")
            print("  Pritisnite RESET na XIAO in znova zaženite lxmf-term.")
            break
        elif "lxmf_error" in line:
            print(f"\r  [napaka pri čakanju] {line}")
        else:
            handle_event(line)

    if not peer_found:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
        return 1

    print("Vtipkaj sporočilo + Enter. Ukazi: /announce  /quit")
    print()

    try:
        while True:
            drain(ev_queue, 0.1)
            try:
                msg = input("> ").strip()
            except (EOFError, KeyboardInterrupt):
                print()
                break
            if not msg:
                continue
            if msg.lower() in ("/quit", "/exit", "quit", "exit"):
                break
            elif msg.lower() == "/announce":
                send_cmd("announce")
                print("  announce poslan")
            else:
                send_cmd(f"send {dest} {b64enc(msg.encode())}")
            drain(ev_queue, 0.5)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()

    return 0


if __name__ == "__main__":
    sys.exit(main())
