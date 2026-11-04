# Konfiguracija hitre lokalne Reticulum mreže za stacionarne naprave

Ta vodič opisuje, kako optimizirati Reticulum (`lnsd`) in vgradna vozlišča (`leviculum-nrf` na Seeed XIAO nRF52840 + Wio-SX1262 ali `leviculum-esp`), kadar so vse naprave **stacionarno nameščene** (npr. domača ali industrijska avtomatizacija, senzorji in krmilniki v stavbi ali na posestvu) in želimo **najnižjo latenco ter 100 % zanesljivost** brez vpliva zunanjega prometa.

---

## 1. Izklop javnih internetnih TCP vozlišč (ključni korak)

Privzeta namestitev pogosto vključuje javna testna vozlišča (`TCPClientInterface`), ki v lokalni `lnsd` neprestano pošiljajo tisoče globalnih `ANNOUNCE` paketov.

Zakaj to upočasni vgradne naprave:
1. `lnsd` z vklopljenim `enable_transport = True` posreduje tuje najave na serijski/LoRa vmesnik.
2. Na nRF52840 ima `EmbeddedStorage` omejeno tabelo identitet (`known_identities = 8` mest) in poti (`path_table = 32` mest). Poplava tujih najav izrine identitete vaših lokalnih krmilnikov in senzorjev.
3. Vsaka posredovana najava na LoRa zasede radijski kanal (airtime) in sproži zakasnitve oddajanja (`PacingDelay`).

**Rešitev:** V `/etc/reticulum/config` nastavite `enabled = no` na vseh zunanjih `TCPClientInterface` povezavah (ali jih odstranite).

---

## 2. Izbira hitrih LoRa radijskih parametrov (PHY)

Pri stacionarnih napravah na kratki do srednji razdalji (znotraj stavbe, med objekti, nekaj 100 m do nekaj km) ne potrebujemo počasnih nastavitev za ekstremni doseg:

| Profil | `spreadingfactor` | `bandwidth` | `codingrate` | Hitrost na zraku | Čas 250 B paketa | Priporočena uporaba |
|---|---|---|---|---|---|---|
| **Hitro lokalno (priporočeno)** | **`7`** | **`250000`** | **`5`** | **~10,9 kbps** | **~180 ms** | Stacionarna avtomatizacija v stavbi / posestvu |
| Srednje (`eu_medium` privzeto) | `8` | `125000` | `5` | ~3,1 kbps | ~650 ms | Večje razdalje / debelejše ovire |
| Dolg doseg | `9` ali `10` | `125000` | `5` | 0,9 – 1,7 kbps | 1,2 – 2,2 s | Oddaljene terenske točke (visoka latenca) |

> **Pomembno:** Vsa LoRa vozlišča v isti mreži morajo uporabljati **identične** parametre `frequency`, `bandwidth`, `spreadingfactor` in `codingrate`.
> Ko `lnsd` zaženete s spremenjenimi parametri na `SerialInterface`, jih prek kontrolnega okvirja samodejno zapiše v priključeni XIAO (ki jih shrani v svoj interni flash, zato ostanejo aktivni tudi po odklopu z USB-ja).

---

## 3. Nastavitve vmesnikov za hitro odzivnost

V razdelku `[interfaces]` datoteke `/etc/reticulum/config` uporabite naslednje nastavitve (`leviculum-std/src/config.rs`):

- **`mode = gateway` ali `mode = access_point`:**
  - `gateway`: vmesnik aktivno odkriva neznane poti (`DISCOVER_PATHS_FOR`) v imenu naprav za njim.
  - `access_point`: pot na tem vmesniku velja **24 ur** (namesto standardnega poteka), nepotrebno posredovanje najav na radijski del pa je zmanjšano.
- **`ingress_control = no`:**
  - Izklopi omejevalnik sunkov najav (`burst limiter`). V zasebni stacionarni mreži želimo, da se ob hkratnem vklopu napajanja vsa vozlišča registrirajo takoj, ne da bi bila zadržana v čakalni vrsti `held_announces`.
- **`announce_cap = 10`:**
  - Privzeta omejitev pasovne širine za najave je `2 %` (`announce_cap = 2`). V majhni lokalni mreži jo dvignite na `10` (10 % kapacitete), da se najave in odgovori na `PATH_REQUEST` oddajo brez čakanja.

---

## 4. Zaščita in izolacija lokalne mreže z IFAC (opcijsko)

Če v bližini delujejo druga LoRa Reticulum vozlišča na isti frekvenci, dodajte na vsak radijski/serijski vmesnik:

```ini
networkname = moja-avtomatizacija
passphrase = skrivno-geslo-lokalne-mreze
```

Paketi brez veljavnega IFAC podpisa so zavrženi takoj na vhodu, še preden dosežejo usmerjevalno tabelo ali preverjanje Ed25519 podpisov.

---

## 5. Priporočena datoteka `/etc/reticulum/config`

```ini
[reticulum]
  enable_transport = True
  share_instance = Yes
  instance_name = default
  panic_on_interface_error = No

[logging]
  loglevel = 4

[interfaces]

  # 1. Samodejna povezava med stacionarnimi strežniki / Raspberry Pi na istem LAN/Wi-Fi
  #    Latenca: < 2 ms (IPv6 multicast odkrivanje + UDP prenos)
  [[Local LAN]]
    type = AutoInterface
    enabled = yes
    mode = full
    ingress_control = no

  # 2. Lokalni TCP strežnik za nadzorne aplikacije (MeshChatX, oddaljeni odjemalci)
  [[TCP]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242
    ingress_control = no

  # 3. Stacionarni XIAO LoRa prehod (hiter profil SF7 / BW 250 kHz)
  [[XiaoKIt]]
    type = SerialInterface
    enabled = yes
    port = /dev/leviculum-xiao-transport-CBAAFBF74BA851E6
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
    mode = gateway
    ingress_control = no
    announce_cap = 10
    # Hitri LoRa parametri (~10.9 kbps, 3.5x hitreje od privzetega SF8/125k):
    frequency = 869463000
    bandwidth = 250000
    spreadingfactor = 7
    codingrate = 5
    txpower = 22

  # 4. Javna internetna vozlišča — IZKLOPLJENA za čisto lokalno mrežo
  [[RNS TCP Node Germany 001]]
    type = TCPClientInterface
    enabled = no
    target_host = 202.61.243.41
    target_port = 4965

  [[RNS TCP Node Germany 002]]
    type = TCPClientInterface
    enabled = no
    target_host = 193.26.158.230
    target_port = 4965

  [[RNS Transport US-East]]
    type = TCPClientInterface
    enabled = no
    target_host = 45.77.109.86
    target_port = 4965
```

---

## 6. Postopek uveljavitve na XIAO vozliščih

1. **Če imate več ločenih XIAO LoRa enot:**
   Vsako enoto enkrat priključite prek USB na gostitelja z zgornjim `/etc/reticulum/config` (ali prilagodite `port = /dev/leviculum-xiao-transport`), da ji `lnsd` ob zagonu pošlje nove radijske nastavitve (`SF7 / BW250000`), ki si jih XIAO trajno shrani v flash (`radio_config_flash_page`).
2. **Ponovno zaženite sistemski `lnsd`:**
   ```bash
   sudo systemctl restart lnsd
   ```
3. **Preverite stanje vmesnikov in pot:**
   ```bash
   lnstatus
   lnpath <delivery_hash_naprave>
   ```
   V dnevniku (`journalctl -u lnsd -n 20`) boste ob uspešni nastavitvi videli:
   ```text
   Serial serial_4: sending radio config (attempt 1/3): freq=869463000 sf=7 bw=250000 cr=5 txp=22
   Serial serial_4: radio config ACK received
   ```
