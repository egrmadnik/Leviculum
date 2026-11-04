# Reticulum lokalna mreža z LNode enotami

Navodila za postavitev lokalne Reticulum mreže z več XIAO nRF52840 LNode enotami,
Raspberry Pi kot centralnim hubom in povezavo na oddaljeno (remote) vozlišče.

---

## Arhitektura

```
Internet
    │
    │ TCP (port 4242)
    │
┌───▼─────────────────────┐
│  Remote vozlišče        │
│  VPS ali domača RPi     │
│  lnsd + TCPServerInterface │
└─────────────────────────┘
    │
    │ TCP
    │
┌───▼───────────────────────────────────────────┐
│  Raspberry Pi — lokalni hub                   │
│  lnsd  (enable_transport = yes)               │
│                                               │
│  ├── AutoInterface      (LAN / WiFi)          │
│  ├── TCPClientInterface  → remote             │
│  ├── SerialInterface     → LNode A /dev/ACM1  │
│  └── SerialInterface     → LNode B /dev/ACM3  │
└───────────┬───────────────────────────────────┘
            │ USB serial          │ USB serial
        ┌───▼────────┐       ┌────▼───────┐
        │ XIAO LNode │ LoRa  │ XIAO LNode │
        │     A      │◄─────►│     B      │
        │ lxmf_echo  │       │ lxmf_echo  │
        └────────────┘       └────────────┘
```

**Podatkovni tok:**
- LNode ↔ RPi: USB serial (Reticulum transport frami)
- LNode ↔ LNode: LoRa radio (direct mesh)
- RPi ↔ Remote: TCP (internet)
- Lokalni klienti (PC, telefon): AutoInterface ali shared instance

---

## Zahteve

### Hardware

| Komponenta | Hardware | Opomba |
|---|---|---|
| Remote hub | VPS (Linux, javni IP) ali RPi z javnim IP | Vsaj 512 MB RAM |
| Lokalni hub | Raspberry Pi 4 | Priporočen, deluje tudi na RPi 3 |
| LNode | Seeed XIAO nRF52840 + Wio-SX1262 | USB + LoRa |
| Klient | PC / Android (Sideband) | Opcijsko |

### Software

```sh
# Na RPi in remote vozlišču
which lnsd        # /usr/bin/lnsd
lnsd --version    # preverite verzijo

# Na RPi — orodja
which lnmsg lnstatus lnprobe
```

---

## 1. Remote vozlišče

Remote vozlišče je javno dostopno Reticulum vozlišče, ki deluje kot TCP relay
med internet in lokalno mrežo.

### Konfiguracija

```sh
sudo mkdir -p /etc/lnsd-remote
sudo nano /etc/lnsd-remote/config
```

Vsebina `/etc/lnsd-remote/config`:

```ini
[reticulum]
enable_transport = yes
share_instance = yes
instance_name = remote-hub

[logging]
loglevel = 4

[interfaces]

  # Lokalna IPv6 mreža na remote strežniku
  [[AutoInterface]]
    type = AutoInterface
    enabled = yes

  # TCP server — čaka na vhodne povezave iz RPi
  [[TCPServer]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242
```

### Zagon kot systemd storitev

```sh
# Namesti kot systemd service
sudo lnsd --config /etc/lnsd-remote -s

# ali ročno
sudo lnsd --config /etc/lnsd-remote -v
```

### Firewall

```sh
# Ubuntu/Debian — odprite TCP port 4242
sudo ufw allow 4242/tcp
sudo ufw reload
```

---

## 2. Lokalni hub — Raspberry Pi

RPi deluje kot router med LNode enotami (USB serial), lokalnim omrežjem
(AutoInterface) in remote vozliščem (TCP).

### Konfiguracija

```sh
mkdir -p ~/.config/lnsd-hub
nano ~/.config/lnsd-hub/config
```

Vsebina `~/.config/lnsd-hub/config`:

```ini
[reticulum]
enable_transport = yes
share_instance = yes
instance_name = hub

[logging]
loglevel = 4

[interfaces]

  # Lokalna LAN/WiFi mreža — samodejno odkritje sosedov
  [[AutoInterface]]
    type = AutoInterface
    enabled = yes

  # TCP povezava na remote vozlišče (zamenjajte IP/domeno)
  [[RemoteHub]]
    type = TCPClientInterface
    enabled = yes
    target_host = <IP_ALI_DOMENA_REMOTE>
    target_port = 4242

  # LNode A — priključen na /dev/ttyACM1 (transport port)
  [[LNode-A]]
    type = SerialInterface
    enabled = yes
    port = /dev/ttyACM1
    speed = 115200

  # LNode B — priključen na /dev/ttyACM3 (transport port)
  # Odkomentirajte ko priključite drugi LNode
  # [[LNode-B]]
  #   type = SerialInterface
  #   enabled = yes
  #   port = /dev/ttyACM3
  #   speed = 115200
```

### Zagon

```sh
lnsd --config ~/.config/lnsd-hub -v
```

### Preverjanje stanja

```sh
lnstatus --config ~/.config/lnsd-hub
```

Pričakovan izhod:

```
 Shared Instance[rns/hub]
    Status    : Up
    Serving   : 2 programs

 serial_1
    Status    : Up
    Rate      : 115.20 kbps
    Traffic   : ↑4.88 KB    ↓5.92 KB

 auto/eth0/...
    Status    : Up
    Peers     : 3 reachable

 TCPClientInterface[RemoteHub]
    Status    : Up
    Traffic   : ↑1.2 KB    ↓800 B

 Transport Instance <...> running
```

---

## 3. XIAO nRF52840 LNode

Vsak XIAO mora imeti naložen `lxmf_echo` firmware (ali drug Leviculum firmware).

### Identifikacija USB portov

XIAO ustvari **dva** serial porta:

| Port | Vloga | Uporaba |
|---|---|---|
| `/dev/ttyACM0` | Debug izhod | `cat /dev/ttyACM0` |
| `/dev/ttyACM1` | Reticulum transport | lnsd SerialInterface |

```sh
# Poišči vse priklopljene LNode enote
ls /dev/ttyACM*

# Preveri kateri port je kateri LNode (po serijskem numeru)
ls -la /dev/serial/by-id/ | grep -i "2886\|seeed"
```

### Stabilni port symlinki (udev)

Da se porti ne spremenijo ob ponovnem priključitvi, dodajte udev pravilo:

```sh
sudo nano /etc/udev/rules.d/99-lnode.rules
```

```udev
# XIAO nRF52840 — transport port (bInterfaceNumber 02 = ACM1)
SUBSYSTEM=="tty", \
  ATTRS{idVendor}=="2886", \
  ATTRS{idProduct}=="0045", \
  ATTRS{bInterfaceNumber}=="02", \
  SYMLINK+="lnode-%k", \
  MODE="0660", GROUP="dialout"
```

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger
# Porti bodo zdaj dostopni kot /dev/lnode-ttyACM1 itd.
```

### Delivery hash LNode

Po priključitvi in zagonu lnsd preverite delivery hash iz debug porta:

```sh
cat /dev/ttyACM0 | grep "delivery_hash"
# [INFO!] [LXMF] inbox ready -- delivery_hash=1ef5d2e9a58ef444f16740b806aa2080
```

---

## 4. Testni terminal (lxmf-term)

Pošiljanje in sprejem LXMF sporočil na LNode.

### Pogoji

- lnsd teče na RPi (`run-lnsd.sh` ali zgornja konfiguracija)
- LNode priključen in prepoznan

### Postopek

```sh
# Terminal 1 — lnsd (hub)
lnsd --config ~/.config/lnsd-hub -v

# Terminal 2 — debug log LNode A
cat /dev/ttyACM0

# Terminal 3 — testni terminal
python3 ~/lxmf-test/lxmf-term.py 1ef5d2e9a58ef444f16740b806aa2080
```

Ko se prikaže:
```
► Pritisnite RESET na XIAO, nato čakam 60s na announce...
```

Pritisnite **RESET** gumb na XIAO (enkrat kratek pritisk).
Po ~10 sekundah:

```
  ✓ XIAO announce prejet! Pošiljanje omogočeno.
> 
```

### Ukazi

| Ukaz | Opis |
|---|---|
| `hello` | Pošlje LXMF sporočilo na LNode |
| `/announce` | Pošlje announce (za path discovery) |
| `/quit` | Izhod |

### Pričakovani izhod

```
> hello
  ✓ poslano: 'hello'

  ← ECHO od 1ef5d2e9: 'echo: hello'
>
```

---

## 5. Varnost

### IFAC mrežni ključ (priporočeno za produkcijo)

Brez IFAC ključa lahko katerokoli Reticulum vozlišče sodeluje v mreži.
Za zaprto mrežo dodajte skupni ključ na TCP vmesnike:

**Na RPi** (`~/.config/lnsd-hub/config`):

```ini
[[RemoteHub]]
  type = TCPClientInterface
  target_host = moj-vps.example.com
  target_port = 4242
  ifac_size = 16
  ifac_netkey = <skupni_nakljucni_kljuc_hex_32_znakov>
```

**Na remote vozlišču** (`/etc/lnsd-remote/config`):

```ini
[[TCPServer]]
  type = TCPServerInterface
  listen_ip = 0.0.0.0
  listen_port = 4242
  ifac_size = 16
  ifac_netkey = <isti_kljuc_kot_zgoraj>
```

Generirajte ključ:

```sh
python3 -c "import secrets; print(secrets.token_hex(16))"
```

---

## 6. Diagnostika

### lnsd

```sh
# Stanje vseh vmesnikov
lnstatus --config ~/.config/lnsd-hub

# Preverite ali je znana pot do LNode
lnprobe --config ~/.config/lnsd-hub 1ef5d2e9a58ef444f16740b806aa2080

# Pošlji ping (path request)
lnprobe --config ~/.config/lnsd-hub -v 1ef5d2e9a58ef444f16740b806aa2080
```

### LNode serial debug

```sh
# Debug log (115200 baud, /dev/ttyACM0)
cat /dev/ttyACM0

# Ključne log vrstice:
# [LXMF] inbox ready -- delivery_hash=...   ← LNode delivery naslov
# [LXMF] msg rx src=... content="..."       ← prejeto sporočilo
# [LXMF] echo sent dest=...                 ← echo poslan
# [TRANSPORT] iface=serial_usb rxb=... txb=... ← serial promet
# [LORA] TX ... bytes                        ← LoRa oddaja
```

### Pogoste napake

| Napaka | Vzrok | Rešitev |
|---|---|---|
| `open /dev/ttyACM1 failed` | LNode ni priključen | Priključite USB, preverite `ls /dev/ttyACM*` |
| `shared-instance name already served` | lnsd že teče | Spremenite `instance_name` ali ustavite star proces |
| `wait_for_peer timeout` | Zamudili XIAO announce | Pritisnite RESET na XIAO |
| `identity_not_known` | LNode ni bil še announce-an | Počakajte na `wait_for_peer_ok` |
| `EncryptionFailed` | Ni javnega ključa pošiljatelja | Firmware zahteva path → auto retry |
| TCP `Connection refused` | Remote lnsd ne teče | Preverite remote strežnik in port |

---

## 7. Referenčne vrednosti

### LoRa radio (eu_medium profil)

| Parameter | Vrednost |
|---|---|
| Frekvenca | 869,463 MHz |
| Spreading Factor | SF8 |
| Bandwidth | 125 kHz |
| Coding Rate | CR4/5 |
| TX moč | 22 dBm |
| CSMA | vklopljen |

### LNode delivery hashi (primer)

| LNode | Identity hash | Delivery hash |
|---|---|---|
| LNode A | `7d5e0d46fcd6d937bd154182e357d425` | `1ef5d2e9a58ef444f16740b806aa2080` |
| LNode B | *(odvisno od flash-a)* | *(beri iz debug porta)* |

---

## 8. Datoteke

```
~/lxmf-test/
├── lnsd-config/
│   └── config              # lnsd konfiguracija (SerialInterface + AutoInterface)
├── lxmf-storage/
│   ├── lxmf_identity       # lxmf-node identiteta (64 bajtov raw)
│   └── transport_identity  # Reticulum transport identiteta
├── lxmf-term.py            # Interaktivni testni terminal
├── run-lnsd.sh             # Zažene lnsd z lokalno konfiguracijo
└── docs/
    └── reticulum-lokalna-mreza.md   # Ta dokument
```

---

## 9. Hitri začetek (TL;DR)

```sh
# 1. Remote vozlišče (enkrat)
sudo lnsd --config /etc/lnsd-remote -s

# 2. RPi hub
lnsd --config ~/.config/lnsd-hub -v &

# 3. Priključite XIAO LNode (USB)
#    lnsd se samodejno poveže na /dev/ttyACM1

# 4. Testni terminal
python3 ~/lxmf-test/lxmf-term.py 1ef5d2e9a58ef444f16740b806aa2080
#    → Pritisnite RESET na XIAO
#    → Vtipkajte sporočilo
```
