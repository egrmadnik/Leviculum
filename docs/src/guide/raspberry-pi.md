# Migrating the node to a Raspberry Pi

How to move a running `lnsd` node — same identity, same
configuration, same clients — from another Linux machine (e.g. the
WSL2 setup in [Running on WSL2](wsl.md)) onto a Raspberry Pi.

The node's identity does not live in the config file. It lives in
`/etc/reticulum/storage/` (`transport_identity`, `identities/`,
`ratchets/`, `known_destinations`). Copy the whole `/etc/reticulum`
tree and the Pi *is* the same node: peers see the same transport
hash, remembered links keep working.

## What differs from WSL2

| Concern | WSL2 | Raspberry Pi |
|---|---|---|
| USB boards | `usbipd` attach from Windows | native `/dev/ttyACM*`, nothing to forward |
| Reachability | Windows localhost forwarding | reachable at its LAN IP directly |
| Package arch | amd64 .deb | arm64 .deb (cross-built) or build on the Pi |
| Python for MeshChatX | needed 3.12 | Pi OS Bookworm ships 3.11 — plain `python3` is enough |

## 1. OS and package

Use 64-bit Raspberry Pi OS (Bookworm or newer). The repo's musl
build produces an arm64 package on the amd64 dev machine — no
toolchain needed on the Pi:

```bash
# on the dev machine (requires cargo-deb, cargo-zigbuild, ziglang —
# `just _deb-prereqs` installs them)
scripts/build-deb.sh arm64
scp target/debian/leviculum-*-arm64.deb pi:<pi-host>:~
```

```bash
# on the Pi
sudo dpkg -i ~/leviculum-*-arm64.deb
sudo usermod -aG leviculum,dialout "$USER"   # re-login afterwards
```

The postinst creates the `leviculum` user/group and the systemd
unit, exactly as on the WSL install.

## 2. Copy `/etc/reticulum` over

Stop the daemon on both sides first — two live nodes carrying one
transport identity will fight for paths:

```bash
sudo systemctl stop lnsd          # old machine
sudo systemctl stop lnsd          # Pi (fresh install hasn't started it anyway)
```

Then mirror the tree. `rsync` preserves owner and mode bits:

```bash
sudo rsync -a /etc/reticulum/ pi:<pi-host>:/tmp/reticulum/
ssh pi 'sudo rsync -a /tmp/reticulum/ /etc/reticulum/ && rm -rf /tmp/reticulum'
```

(Two hops because the remote side needs root for the daemon-owned
files. If `rsync` is missing on the Pi: `sudo apt install rsync`.)

Re-assert ownership and the group-write bit — the same rule from the
WSL guide applies, and files copied as another user are the classic
cause of `Failed to persist known ratchet: Permission denied`:

```bash
ssh pi 'sudo chown -R leviculum:leviculum /etc/reticulum &&
        sudo chmod -R g+w /etc/reticulum/storage'
```

## 3. Serial boards

The same udev rule file produces the same stable names on the Pi:

```bash
scp leviculum-nrf/udev/99-leviculum.rules pi:~
ssh pi 'sudo cp ~/99-leviculum.rules /etc/udev/rules.d/ &&
        sudo udevadm control --reload-rules && sudo udevadm trigger'
```

Plug the board straight into the Pi's USB — no forwarding layer.
Because the config references `/dev/leviculum-*-transport-<SERIAL>`,
the `[interfaces]` section transferred in step 2 already points at
the right port unmodified, whichever ttyACM number the board lands
on.

## 4. Start and verify

```bash
sudo systemctl enable --now lnsd
journalctl -u lnsd -f    # expect: "Serial serial_N online on ..." +
                         # "radio config ACK received"
lnstatus                 # all interfaces Up; same transport identity
```

## 5. Clients on the Pi

Local clients (`lnstatus`, `rnstatus`, `lncp`, `lnmsg`) attach to
the shared instance `@rns/default` unchanged. Python RNS tools need
`pip install rns` if not already present.

### MeshChatX

Same venv recipe, no Python version juggling — Bookworm's `python3`
is 3.11:

```bash
sudo apt install python3-venv
python3 -m venv ~/.venvs/meshchatx
~/.venvs/meshchatx/bin/pip install reticulum-meshchatx
~/.venvs/meshchatx/bin/meshchatx --headless --no-https \
    --host 0.0.0.0 --port 8000 \
    --reticulum-config-dir /etc/reticulum \
    --storage-dir ~/.meshchatx
```

`--host 0.0.0.0` serves browsers anywhere on the LAN at
`http://<pi-host>:8000` — on a Pi that is the whole point, not a WSL
workaround. Keep `--auth` in mind if the LAN is shared.

The systemd user unit from `setup-wsl-mesh.sh` (step 8) works
verbatim on the Pi; the whole script in fact runs there, since only
its usbipd remark is WSL-specific.

A MeshChatX running elsewhere on the LAN peers over TCP exactly like
the Windows case: `TCPClientInterface` → `<pi-ip>:4242`, which is why
the config carries `enable_transport = True`.

## 6. Retire the old node

Once the Pi checks out:

```bash
sudo systemctl disable --now lnsd   # on the old machine
```

Do not leave both running — one transport identity on two machines
splits announces and path replies between them and the mesh sees a
node that answers only sometimes.
