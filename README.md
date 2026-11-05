# aorus-lcd-rs

A small Rust tool and daemon that drives the **LCD Edge View** side screen of the
**Gigabyte AORUS GeForce RTX 5090 MASTER** on Linux (Arch), with no Gigabyte software.

> Status: planning / not yet implemented. This README is the spec.

---

## Goal

On Windows, Gigabyte Control Center (GCC) can show live GPU stats, images, text and GIFs
on the card's 320x170 side panel. On Linux the panel just loops a built-in animation.

This project should:

1. **Show live GPU stats** (temperature, clocks, usage, fan, VRAM, power) on the panel,
   fed by a lightweight background daemon that starts at boot.
2. **Upload content**: static image, text, animated GIF, choose built-in screens,
   carousel, save to panel NVRAM.
3. Ship as **one static-ish Rust binary** plus a **systemd unit**, later packaged for the AUR.

Out of scope: fan curves, overclocking, BIOS/firmware flashing, RGB lighting (maybe later).

---

## Hardware

- Card: Gigabyte AORUS GeForce RTX 5090 MASTER 32G (`GV-N5090AORUS M-32GD`).
  Verify with the sticker on the card, or:
  ```bash
  lspci -nnk -d 10de: | grep -A2 VGA          # Subsystem: Gigabyte [1458:416e]
  nvidia-smi -q | grep -iE "product name|vbios"
  ```
  The subsystem ID `1458:416e` is the reliable identifier; `Board Part Number` can read `N/A`.
- The panel hangs off the GPU's internal I2C bus, exposed by the proprietary NVIDIA
  driver as a Linux I2C adapter named `NVIDIA i2c adapter 1 at ...`.
- Other Gigabyte LCD cards may use a newer protocol ("LcdEx", address `0x76`).
  **This project only supports the legacy `0x61` protocol.** If the probe fails, stop.

---

## Prior art (read these first)

| Project | What it is | What we take from it |
|---|---|---|
| [albancreton/aorus-master-linux](https://github.com/albancreton/aorus-master-linux) | Python script (`aorus_lcd.py`, `smbus2` + `Pillow`) that already controls the panel on Linux: probe, on/off, mode, image, text, gif, carousel. Alpha, tested on one RTX 5090 Master. MIT. | **Reference implementation.** Bus discovery, frame format, upload sequence, RLE encoder, selftest vectors. Our Rust output must be byte-identical. |
| [CodeTorchAI/AorusLcd](https://github.com/CodeTorchAI/AorusLcd) | C#/Avalonia port for Windows (NVAPI). Adds the live sensor dashboard and a background feed service. MIT. | **[`docs/PROTOCOL.md`](https://github.com/CodeTorchAI/AorusLcd/blob/main/docs/PROTOCOL.md)**: complete opcode reference incl. `E1` (dashboard config) and `E3` (sensor feed) byte layouts. Also `docs/RECOVERY.md` for unsticking a frozen panel. |
| [OpenRGB](https://github.com/CalcProgrammer1/OpenRGB) | RGB control | Only relevant if we ever add RGB. Not now. |

**What is missing on Linux today:** the Python tool cannot drive the live stats dashboard
(nothing pushes the `E3` sensor feed), has no daemon/systemd integration, and runs from a
venv that breaks on Arch Python version bumps. That gap is this project.

---

## Protocol summary

Facts collected from the two projects above. **`PROTOCOL.md` and `aorus_lcd.py` are
authoritative; check byte layouts there, do not guess.**

### Bus
- Load `i2c-dev`; find the adapter whose sysfs name starts with `NVIDIA i2c adapter 1`
  (`/sys/class/i2c-dev/i2c-N/name`). Never pick a `/dev/i2c-N` by number heuristics.
  Allow a `--bus N` override, but it must still name an NVIDIA adapter.
- LCD controller address: **`0x61`**. This is the only address we ever write to.
- `0x71` / `0x75` is the RGB controller on the same bus (`0x75` on this card). **Never
  touch it.** It is write-only; reads to it can wedge the GPU's I2C engine.
- Frames are 256 bytes, larger than the SMBus 32-byte block limit, so use **raw I2C
  transfers** (`I2C_RDWR` ioctl, or `I2C_SLAVE` + `write(2)`). Mirror what `aorus_lcd.py`
  does with `smbus2`.

### Frame format
Every command is one 256-byte write:
```
[opcode, 0xCB, 0x55, 0xAC, 0x38, params..., 0x00 padding up to 256]
```

### Presence probe
Send the status query `EB 03` and **require a valid read-back** before any other write.
Do not use a zero-length "quick write" probe (the NVIDIA adapter rejects it and unconnected
DDC ports falsely ACK it). The controller can need up to a minute after boot to answer.

### Opcodes
| Opcode | Command | Notes |
|---|---|---|
| `E7` | OpenLcd | panel on / off |
| `E5` | SetMode | display mode (see below) |
| `E1` | SetDisplay | dashboard overlay: sensor-element bitmask + rotation interval. Send at most once on (re)connect, only if `DF` shows the panel needs it, never periodically; it is the prime suspect in dark-panel incidents (see `RECOVERY.md`). |
| `E3` | Sensor feed | live GPU values, must be pushed about once per second |
| `EA` / `EB 03` | Set / Get image template | `EB 03` doubles as presence probe |
| `F3` | SetLoop | carousel |
| `F2` / `F1` | Upload begin/end, header | content uploads |
| `AA` | Save | persist current config to panel NVRAM; the same frame triggers the text rainbow effect, so applying that effect also saves. Never send it from the daemon. |
| `D6`, `DE`, `DF`, `F4` | Get FW / mode / display / loop | read-backs |

Display modes: `0-2` built-in stat screens, `3` image, `4` text, `5` gif, `6` chibi,
`7` carousel (GCC quirk: the wire byte is `0x0A`, internal mode 9 + 1), `8` preset
rotation (wire byte `0x0B`; the number is only this tool's label). The real 5090 reported
this raw mode at rest. Its behaviour is known only from upstream notes on an RTX 5080: a
rotation of the built-in screens plus stored user content. `F3` carousels accept modes
`0-6` only.

### Panel and uploads
- 320x170 pixels, RGB565 little-endian, row-major.
- Upload sequence: `F2` BEGIN, `F1` header, payload in 256-byte zero-padded chunks,
  `F2` END, then `E5` SetMode. Chunk count is `len / 256 + 1`, so an exact multiple of
  256 still gets one full padding chunk.
- Image and text payloads are the 12-byte descriptor `01 00 0B A9 01 00 40 01 AA 00 01 00`
  followed by the pixels.
- Pacing is set by the firmware: about 0.5 s after BEGIN, 1.0 s after the header,
  about 10 ms per chunk.
- Framebuffer targets in the `F1` header: image `0x01300000`, text `0x01320000`,
  gif `0x00000000` (live buffer).
- **Order matters:** for GIFs send `SetMode 5` *before* streaming; for image/text send
  SetMode *after* the upload.
- GIF payload: `u16 LE frame count`, then per frame a 10-byte entry
  (`u32 LE` **inclusive** end offset `2 + 10*N + sum(sizes) - 1`, `u16 width`, `u16 height`,
  `u16 format` where `3` = RLE), then the concatenated RLE frames.
- RLE tokens: 2-byte LE head, bit 15 = run flag, low 15 bits = pixel count.
  Run (only for 3+ identical pixels): head + one pixel. Literal: head + that many pixels.
  Encoding works in windows of at most `0x7FFF` pixels. A window with no repeat, or with
  fewer than 4 pixels left, is one literal even if its pixels are equal. Reproduce these
  quirks exactly; the firmware decoder mirrors the encoder.

### Sensor dashboard
- `E1` selects which widgets show (GPU temp, GPU clock, GPU usage, fan, RAM clock,
  RAM usage, FPS, TGP) and the rotation interval; the firmware rotates them itself.
- `E3` carries the live values. If it is not pushed at a steady ~1 Hz keep-alive the
  widgets freeze or go blank. This is the daemon's job.
- NVML has no FPS source: send 0.
- `nvmlDeviceGetPowerUsage` reports a stuck value on RTX 50 cards; read the instant power
  field first and fall back to it.
- The periodic I2C write can cost frame time. Upstream measured 1% lows of about 141 fps
  with a 256-byte 1 Hz feed against about 182 with almost no writes (RTX 5080, Windows,
  unverified here). 128-byte `E3` frames and an adaptive feed reduce this.

---

## Architecture

```
aorus-lcd-rs/
├── Cargo.toml
├── src/
│   ├── lib.rs         # library root, so tests/ can reach the modules
│   ├── main.rs        # clap CLI, subcommand dispatch
│   ├── bus/           # discovery.rs, transport.rs (raw I2C), lock.rs (flock)
│   ├── protocol.rs    # pure frame builders (no I/O), opcodes, enums
│   ├── panel.rs       # high-level ops: probe, on/off, mode, upload, save
│   ├── image.rs       # resize to 320x170, RGB565 conversion, text render
│   ├── rle.rs         # RLE encoder + GIF payload builder
│   ├── sensors.rs     # NVML polling -> E3 values
│   ├── session.rs     # the write gate: adapter, lock, device, EB 03 probe
│   └── daemon.rs      # 1 Hz loop, signal handling, reconnect
├── tests/             # byte-parity tests against vectors from aorus_lcd.py
└── dist/
    ├── aorus-lcd.service
    ├── aorus-lcd.tmpfiles   # creates the bus lock file
    ├── i2c-dev.conf
    ├── 99-i2c.rules
    └── PKGBUILD       # later
```

Design rules:
- `protocol.rs` and `rle.rs` are **pure functions** that return byte buffers, so they can
  be unit-tested without hardware.
- Every command supports `--dry-run`: hex-dump frames instead of writing.
- All bus access goes through one exclusive file lock (`/run/aorus-lcd.lock`) so the
  daemon and an image upload never interleave on the bus. Non-root users cannot create
  that file, so `dist/aorus-lcd.tmpfiles` does. Never fall back silently to another path;
  that would break mutual exclusion.
- Refuse to write if the `EB 03` probe fails. Never write to any address except `0x61`.

Suggested crates: `clap`, `anyhow`/`thiserror`, `i2cdev` or `nix` (ioctls),
`nvml-wrapper` (loads `libnvidia-ml.so` from `nvidia-utils`), `image`, `gif`,
`ab_glyph` or `fontdue` (text), `signal-hook`, `fs4` or `rustix` (flock), `toml`/`serde`
(config).

### CLI sketch
```
aorus-lcd probe [--wait SECS]
aorus-lcd on | off
aorus-lcd mode <0-8>
aorus-lcd image <file> [--save]
aorus-lcd text "hello" [--color ff8800] [--bg 000000] [--size 32] [--no-effect]
aorus-lcd gif <file> [--frame-delay MS]
aorus-lcd carousel 0,1,4 --interval 5
aorus-lcd dashboard --widgets temp,clock,usage,fan,vram,power --interval 3
aorus-lcd daemon [--config /etc/aorus-lcd.toml]
aorus-lcd save
global: --bus N  --dry-run  -v
```

---

## Milestones

**M0 – Verify hardware.** Confirm the subsystem ID and adapter name, then run a real
`EB 03` probe. Note the bus number and that `probe` succeeds. If it fails, stop here.
The reference tool can do this before any code exists:
```bash
sudo modprobe i2c-dev
git clone https://github.com/albancreton/aorus-master-linux /tmp/ref && cd /tmp/ref
python -m venv .venv && .venv/bin/pip install smbus2 Pillow
sudo .venv/bin/python aorus_lcd.py probe
sudo .venv/bin/python aorus_lcd.py selftest
```
This project uses its own `aorus-lcd probe` for the gate instead, which sends the same frame.

**M1 – Bus + probe.** Adapter discovery by name, raw I2C transfer, `EB 03` probe,
`probe` subcommand, lock file, `--dry-run`.

**M2 – Basic commands.** `on`, `off`, `mode`, `save`, `carousel`. Frame builders with
unit tests that compare bytes to what `aorus_lcd.py` produces.

**M3 – Sensor daemon (main goal).** Read the `E1`/`E3` layouts from `PROTOCOL.md`,
implement `dashboard` and `daemon`: NVML poll at 1 Hz, push `E3`, reconnect on bus errors,
clean shutdown on SIGTERM. Add `dist/aorus-lcd.service`. Verify the widgets keep updating
for hours.

**M4 – Content uploads.** RGB565 conversion, image upload, RLE + GIF payload. Port the
`selftest` vectors from `aorus_lcd.py` as Rust tests; output must be byte-identical.

**M5 – Polish.** Text rendering, config file, PKGBUILD for the AUR, maybe a
`--keep-sensors` option and the recovery sequence from `RECOVERY.md`.

---

## Setup on Arch

```bash
# load i2c-dev at boot
echo i2c-dev | sudo tee /etc/modules-load.d/i2c-dev.conf
sudo modprobe i2c-dev

# allow non-root access to I2C (create group if it does not exist)
sudo groupadd -f i2c
echo 'KERNEL=="i2c-[0-9]*", GROUP="i2c", MODE="0660"' | sudo tee /etc/udev/rules.d/99-i2c.rules
sudo udevadm control --reload && sudo udevadm trigger
sudo usermod -aG i2c "$USER"   # re-login afterwards

# build
cargo build --release
./target/release/aorus-lcd probe
```

Requires the proprietary NVIDIA driver (`nvidia` / `nvidia-open` + `nvidia-utils`).

### Daemon
```bash
sudo install -Dm755 target/release/aorus-lcd /usr/local/bin/aorus-lcd
sudo install -Dm644 dist/aorus-lcd.service /etc/systemd/system/aorus-lcd.service
sudo systemctl enable --now aorus-lcd
```

---

## Safety

- Writes only to `0x61` on the verified NVIDIA adapter. Never to the RGB controller.
- No firmware flashing, no persistent GPU state beyond the panel's own image memory/NVRAM.
- Worst failure seen in the reference project: stale or black panel until the next upload
  or a power cycle, never a bricked card. Still: use at your own risk.
- Don't run GCC-like tools or OpenRGB writes to this bus at the same time.

---

## Notes for implementing with Claude Code

1. Before writing protocol code, read `aorus_lcd.py` from albancreton/aorus-master-linux
   and `docs/PROTOCOL.md` from CodeTorchAI/AorusLcd. Copy byte layouts from there, never
   invent them.
2. Treat the Python script as the oracle: generate test vectors with it (frames as hex)
   and assert byte equality in Rust tests.
3. Build the pure layers (`protocol`, `rle`) with tests first, hardware layers last.
4. Use `--dry-run` for anything not yet verified on hardware; ask before the first real
   write of each new opcode.
5. Work milestone by milestone; M3 (daemon) is the priority.

---

## Credits and license

Protocol reverse engineering: [albancreton/aorus-master-linux](https://github.com/albancreton/aorus-master-linux)
(MIT) and [CodeTorchAI/AorusLcd](https://github.com/CodeTorchAI/AorusLcd) (MIT).
This project is a clean reimplementation using protocol facts only; no vendor code.
Not affiliated with Gigabyte or NVIDIA.

License: MIT.
