# Running the mesh on WSL2

How to run `lnsd`, attach the LoRa boards and serve clients on a
Windows machine through WSL2. The automated equivalent of this page
is `scripts/setup-wsl-mesh.sh`.

Topology:

```text
Windows host                     WSL2 VM
─────────────                    ─────────────────────────────────
browser ── localhost:8000 ──►    MeshChatX (venv, --host 0.0.0.0)
MeshChatX ── localhost:4242 ──►  lnsd TCPServerInterface 0.0.0.0:4242
usbipd attach ── ttyACM ──►      /dev/leviculum-*-transport-<serial>
                                 lnsd shared instance @rns/default
                                 lnstatus / rnstatus / lncp (local clients)
```

## 1. USB boards into WSL

WSL2 sees no USB devices by itself; `usbipd-win` forwards them. On the
Windows side (elevated PowerShell):

```powershell
usbipd list                          # find the board's BUSID
usbipd bind --busid 4-2              # once per board, persists
usbipd attach --wsl --busid 4-2      # attach into WSL
```

Inside WSL each board then appears as two CDC-ACM ports (debug +
transport). `usbipd attach` does not survive reboots or replugs;
repeat it after either.

## 2. Stable device names

ttyACM numbers shuffle on every replug. The udev rule set at
`leviculum-nrf/udev/99-leviculum.rules` creates serial-numbered
symlinks instead:

```bash
sudo cp leviculum-nrf/udev/99-leviculum.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
```

Every board then gets `/dev/leviculum-<type>-debug-<SERIAL>` and
`/dev/leviculum-<type>-transport-<SERIAL>` (`xiao`, `rak`, `solar`,
or unprefixed for the T114). Always reference the **suffixed** name
in the config — the short unsuffixed symlink lands on whichever
board udev saw first.

## 3. lnsd and the serial interface

Install the package, then edit `/etc/reticulum/config`:

```bash
sudo dpkg -i leviculum-nightly-amd64.deb
sudo usermod -aG leviculum,dialout "$USER"   # re-login afterwards
```

```ini
[reticulum]
  # True when other peers dial in over TCP and need their announces
  # relayed onward (e.g. a Windows-side MeshChatX). A pure leaf
  # serving only local shared-instance clients can leave this False.
  enable_transport = True
  share_instance = Yes
  instance_name = default

[interfaces]

  [[TCP]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242

  [[XiaoKit]]
    type = SerialInterface
    enabled = yes
    port = /dev/leviculum-xiao-transport-CBAAFBF74BA851E6
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
    # LoRa PHY pushed to the board in the radio-config frame.
    # EU868 example — adjust to your band plan.
    frequency = 869463000
    bandwidth = 125000
    spreadingfactor = 8
    codingrate = 5
    txpower = 22
```

Serial notes:

- `SerialInterface` (not `RNodeInterface`) is the right type for an
  LNode — the board frames HDLC, and this interface also pushes the
  `frequency`/`bandwidth`/`spreadingfactor`/`codingrate`/`txpower`
  block to the radio and waits for the firmware's ACK (see
  [configuration reference](configuration.md)).
- An unplugged board is not fatal: the interface retries the open
  every 5 s and comes online by itself when the device returns —
  `journalctl -u lnsd` shows `Serial serial_N online on ...`.
- Outbound traffic counters on a leaf node (`lnstatus`) may sit at
  `↑0 B` for a long time: without transport the daemon only emits
  local traffic. Any path request (`rnpath <hash>`) exercises TX.

Storage permissions: directories under `/etc/reticulum/storage/`
must be group-writable — the daemon runs as user `leviculum` and a
`drwxr-sr-x` directory owned by another group member makes every
persisted ratchet fail with `Permission denied`. After restoring or
hand-editing files there, run:

```bash
sudo chmod -R g+w /etc/reticulum/storage
```

## 4. Local clients

Any Python-RNS program (rnstatus, rncp, MeshChatX, …) with
`share_instance = Yes` and `instance_name = default` attaches to the
lnsd shared instance over the abstract socket `@rns/default` — no
interface entry is needed and the client inherits every interface
the daemon owns. `lnstatus` shows `Serving: N programs`.

## 5. MeshChatX inside WSL

The PyPI wheel ships the web UI; it needs Python ≥ 3.11:

```bash
python3.12 -m venv ~/.venvs/meshchatx
~/.venvs/meshchatx/bin/pip install reticulum-meshchatx
~/.venvs/meshchatx/bin/meshchatx \
    --headless --no-https --host 0.0.0.0 --port 8000 \
    --reticulum-config-dir /etc/reticulum \
    --storage-dir ~/.meshchatx
```

Two WSL-specific traps:

- **`--host 0.0.0.0` is required.** WSL2 forwards Windows `localhost`
  connections to the VM's eth0 address; a service bound to
  `127.0.0.1` inside WSL is unreachable from the Windows browser.
  `--no-https` keeps plain HTTP, which the forwarder passes through
  fine.
- **`--storage-dir` should be absolute.** The default is
  `./storage` relative to the working directory — files land
  wherever the process was launched (including inside the repo
  checkout). If the process exits uncleanly it leaves
  `.meshchatx.lock` behind; delete it and start again.

Then open `http://127.0.0.1:8000` in the Windows browser.

## 6. MeshChatX on Windows instead

A MeshChatX running on the Windows side cannot reach the abstract
unix socket. It peers over TCP instead: Interfaces → Add Interface →
`TCPClientInterface` → `target_host = 127.0.0.1`,
`target_port = 4242` (same localhost forwarding). For its announces
to propagate onward, `lnsd` needs `enable_transport = True` — a
non-transport node does not rebroadcast a remote peer's announces,
so a TCP-peered client behind one would be invisible to the mesh.

## 7. Verify

```bash
systemctl status lnsd                 # active
journalctl -u lnsd -f                 # Serial serial_N online ... radio config ACK
lnstatus                              # interfaces Up, Serving: N programs
ss -tln | grep -E '4242|8000'         # both listening
ss -tn  | grep -E '4965|4242'         # ESTAB uplinks / clients
```
