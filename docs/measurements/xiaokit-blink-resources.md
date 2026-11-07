# XiaoKit Blink build: flash and RAM report

Measured 2026-11-06 on the `xiaokit` binary built with
`--features bsp-xiaokit,lxmf-blink` (release profile), working tree
`d245e1bb` plus uncommitted changes. Image: `target/xiaokit_blink.uf2`,
flashed to a Seeed XIAO nRF52840 with the Wio-SX1262 module.

Static numbers come from the ELF (`size`, `nm`, `memory.x`). Runtime
numbers come from the board's debug port after about 80 minutes of uptime,
with one peer and 13 propagation peers known.

## Flash

| | Bytes | Share of application window |
| --- | ---: | ---: |
| `.vector_table` | 256 | |
| `.text` (code) | 601,072 | |
| `.rodata` | 78,344 | |
| `.data` (initial values) | 664 | |
| **Firmware image** | **680,336** | **93.3 %** |
| Application window (`FLASH`, 0x27000..0xD9000) | 729,088 | |
| **Free in the window** | **48,752** | **6.7 %** |

The UF2 file is 1,360,896 bytes (2,658 blocks of 512 B, 256 B payload each).
It is a transfer format, not the flash footprint.

The window ends at the boot record (0xD9000). Above it, never touched by
the firmware image: boot record 0xD9000 (4 KiB), message store 0xDA000
(64 KiB), the persistence pages 0xEA000 to 0xEC000 (telemetry and Blink
record, radio config, identity) and the bootloader at 0xF4000. The Blink
setting adds no page: its 8-byte record sits at `+0x600` of the telemetry
page.

## RAM

The RAM region is 0x20006DC0..0x20040000, 234,048 bytes (the SoftDevice
owns the 0x20000000..0x20006DC0 below it). The linker places the stack at
the bottom (with `flip-link`, so an overflow faults instead of corrupting
statics) and gives it everything the statics do not use.

| Part | Bytes | Share of region |
| --- | ---: | ---: |
| `.data` | 664 | 0.3 % |
| `.bss`, of which the heap `HEAP_MEM` is 98,304 (96 KiB) | 133,232 | 56.9 % |
| `.bss` without the heap | 34,928 | 14.9 % |
| **Stack region** (what is left) | **100,152** | **42.8 %** |
| Total | 234,048 | 100 % |

`.retained` (the record that survives a reset) is a separate 3,200-byte
region at 0x20006140: 3,144 bytes used, 56 free.

### At run time

| | Value |
| --- | ---: |
| Stack peak use | 30,412 of 100,152 B (30.4 %) |
| Stack never touched | 69,740 B |
| Heap in use | 67,600 of 98,304 B (68.8 %) |
| Heap free / largest free block | 30,704 / 30,644 B |

The stack figure is a high-water mark since boot. Heap use is the
`[HEAP_CENSUS]` line: the node itself 31,112 B, transport 8,271, links
2,860, destinations 7,544, propagation peers 4,768, other 7,541. It
grows with the peers the node hears and is not specific to the Blink app.
A fresh boot of the plain build showed 53,756 B in use after 212 s.

## What the app costs

Same board, same tree, three builds:

| Build | Image (flash) | Statics (data + bss + retained) |
| --- | ---: | ---: |
| plain `xiaokit` | 616,024 | 136,608 |
| `lxmf-echo` | 661,048 | 136,840 |
| `lxmf-blink` | 680,336 | 137,040 |

| Delta | Flash | Statics |
| --- | ---: | ---: |
| echo over plain | +45,024 | +232 |
| blink over plain | +64,312 | +432 |
| blink over echo | +19,288 | +200 |

Most of the flash cost of both apps is the full LXMF inbox (the plain
build only announces a delivery destination), which is why echo is already
+45 KB. What blink adds on top of echo is +19 KB of flash and 200 B of
RAM. The Blink code itself is small (the reply function is 496 B, the
two task polls 240 B and 380 B). The rest of that difference is not
attributed per symbol here; the build also contains the standard library's
decimal float parser (its power-of-five table alone is 10,416 B), used by
`blink <Hz>`, and the float and integer formatting for the replies.

Blink's own statics: the two signals 12 B each, the persistence channel 40 B,
its ticket slot 20 B, the two task pools 104 B and 72 B, the current
frequency 2 B. Their sum, about 262 B, is the whole RAM price of the app
besides its heap use, which is small and transient (one reply string per
command).

The Blink app also adds no startup stack or heap reserve: the stack
peak of 30 KB is the node's own.

## Memory that is not counted above

All figures above are the nRF52840's own memory: 1 MB of on-chip flash
(of which the firmware window is 729,088 B, the rest is SoftDevice, store,
persistence pages and bootloader) and 256 KB of RAM (of which the SoftDevice
takes the bottom 28,096 B).

The XIAO nRF52840's specification also lists 2 MB of onboard QSPI flash,
and **this unit has it**. A read-only probe build (`--features
bsp-xiaokit,qspi-probe`, 2026-11-06) asked the part for its JEDEC id at
boot and got:

```text
[QSPI] JEDEC id=85:60:15 id2=none expect=85:60:15 part=P25Q16H bytes=2097152 clk=16MHz rxdelay=1 match=1 state=ok
[QSPI] STORE state=unformatted sectors=512 note=this-batch-never-formats
```

A P25Q16H, 2,097,152 bytes, 512 sectors of 4 KiB, blank (no record log
formatted on it). The probe wrote nothing.

The Blink build does not use it: `qspi_part` is `None` for the XiaoKit board
and the firmware prints `[QSPI] NONE board=xiaokit` at boot. The repository's
note that the plain XIAO leaves the footprint unfitted (schematic `DNP`,
upstream variant lines commented out) is therefore wrong for at least this
unit, and that note cites the Sense schematic. The QSPI flash adds storage
only; it changes none of the on-chip flash or RAM numbers above.


## Headroom

- **Flash:** 48,752 B (6.7 %) of the application window is free. That is
  room for a handful of small apps the size of Blink on top of echo's
  inbox, not for a second large feature. `scripts/check-nrf-store-gap.sh`
  reports the same gap on every push and the linker fails the build if the
  image would reach the boot record.
- **RAM:** the stack has 69,740 B of margin over its peak, and the heap has
  30,704 B free at 80 minutes. The heap is the tighter of the two: it
  grows with the peers learned, and the node has a heap budget check
  (`heap_census`) that refuses a configuration that cannot fit.
- **Flash endurance:** the Blink record is written once per burst of
  commands, five quiet seconds after the last, and only if the value
  changed. At one change per day that is 365 erases of the telemetry page
  a year against a 10,000-cycle limit; the page is shared with the other
  settings, so any of them counts.

## Update: the QSPI probe is now part of every XiaoKit build

After the probe result above, the XiaoKit board declares the part
(`qspi_part = Some(P25Q16H)`) and every build asks it at boot, as the solar
node does. That pulls the QSPI driver into the image. The Blink build
(`target/xiaokit_blink_qspi.uf2`) measures:

| | Before | After | Delta |
| --- | ---: | ---: | ---: |
| Firmware image | 680,336 B | 690,832 B | +10,496 B |
| Free in the 729,088 B window | 48,752 B | 38,256 B | -10,496 B |
| Statics (data + bss + retained) | 137,040 B | 137,040 B | 0 |

RAM statics did not change; the RAM, stack and heap figures above still
hold apart from a few bytes. The run-time numbers were measured on the
earlier image and have not been repeated on this one.
