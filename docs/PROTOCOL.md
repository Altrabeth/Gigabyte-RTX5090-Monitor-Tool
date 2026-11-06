# Protocol notes

What this tool relies on to talk to the panel of the RTX 5090 Master, and the rules the code
follows. These notes are a summary. The authority for byte layouts is upstream:
[`aorus_lcd.py`](https://github.com/albancreton/aorus-master-linux) and
[`docs/PROTOCOL.md`](https://github.com/CodeTorchAI/AorusLcd/blob/main/docs/PROTOCOL.md) of
CodeTorchAI/AorusLcd. Check there, do not guess.

Sections 1 to 6 follow upstream's numbering. A code comment that cites "`docs/PROTOCOL.md`
section N" means the upstream file, commit `3818b12`, and the same section here covers the same
topic. Likewise `RECOVERY.md` in a comment is upstream's. Upstream's sections on RGB are left out,
since this tool never talks to the RGB controller.

Where a note says "on this card", it was seen on the card named in the README's Status section.

## 1. Transport and addressing

- Load `i2c-dev` and find the adapter whose name in `/sys/class/i2c-dev/i2c-N/name` starts with
  `NVIDIA i2c adapter 1 at`. Never pick a `/dev/i2c-N` by number. `--bus N` overrides the choice,
  but it must still name an NVIDIA adapter.
- The LCD controller is at **`0x61`**, the only address this tool ever uses. It is a private
  constant, and no API accepts an address.
- `0x71` / `0x75` is the RGB controller on the same bus (`0x75` on this card). **Never touch it.**
  It is write-only, and reads to it can wedge the GPU's I2C engine.
- Frames are 256 bytes, over the SMBus limit of 32, so transfers are raw `I2C_RDWR`.
- Presence probe: send `EB 03` and require a valid read-back before any other write. Do not use a
  zero-length quick write: the NVIDIA adapter rejects it and unconnected DDC ports falsely ACK it.
- The controller can need up to a minute after boot to answer.
- A write of 256 bytes took 28 ms on average and 38 ms at most on this card, which fits a 100 kHz
  bus. The C# reference pins the bus to 400 kHz on Windows. Linux offers no way to change this.
- Other Gigabyte cards may use a newer protocol ("LcdEx", address `0x76`), which this tool does not
  support.

## 2. Command frames

Every command is one 256-byte write:

```
[opcode, 0xCB, 0x55, 0xAC, 0x38, params..., 0x00 padding up to 256]
```

| Opcode | Command | Notes |
|---|---|---|
| `E7` | OpenLcd | `[5]` = 1 on, 2 off |
| `E5` | SetMode | `[5]` = mode + 1, with the exceptions in section 6 |
| `E1` | SetDisplay | Widget flags and rotation interval, section 3 |
| `E3` | SensorFeed | Live values, section 4 |
| `EA` | SetImageTpl | Overlay template, section 5 |
| `AA` | Save | Persists the configuration to panel NVRAM. The references also send it as the text rainbow trigger, see below. The daemon never sends it. |
| `F3` | SetLoop | `[5]` = interval, `[6..]` = mode + 1 list. Accepts modes 0 to 6 only. |
| `F2` / `F1` | Upload marker, header | Section 6 |
| `D6` | GetFWVersion | Read 4 |
| `DE` | GetMode | Read 4 |
| `DF` | GetDisplay | Read 4: widget bitmask in byte 1, interval in byte 2 |
| `F4` | GetLoop | `[5]` = bank 1 to 5, read 8 |
| `EB 03` | GetImageTpl | Read 8. Doubles as the presence probe. |

`aorus_lcd.py` and `PanelController.cs` send `AA` in text mode as the trigger of the panel's text
rainbow. What was seen here differs. On firmware 1.3, `text --no-effect --color ffaa00` showed the
correct text in a wandering rainbow, not orange, with no `AA` in that run and one `AA` sent earlier
in mode 0. That is one run. Whether the rainbow is permanent in text mode or was enabled by that
save is unknown.

`F3` stores the list and does not change the mode, so playing a carousel needs `E5` as well.

`F4` layout, read back on this card after `carousel 0,1,2,3,4 --interval 7`:

```
bank 1: 07 01 02 03 03 04 05 50
bank 2: 07 04 05 00 03 04 05 50
bank 3: 07 00 00 00 03 04 05 50
```

Byte 0 is the interval, repeated in every bank. Bytes 1 to 3 are mode + 1 entries, continuing into
the next bank. Bytes 4 to 7 read `03 04 05 50` throughout and did not change with `F3`, so they are
not part of the carousel, and their meaning is unknown. A shorter list clears the old entries. The
15-entry limit that 5 banks of 3 imply is unverified, as are playback and the unit of the interval.

## 3. Sensor dashboard: `E1`

`E1` selects which widgets show and the rotation interval. The firmware rotates them itself.

```
[5] GPU temp   [6] GPU clock   [7] GPU usage   [8] fan
[9] RAM clock  [10] RAM usage  [11] FPS        [12] TGP
[13] interval in seconds
```

`DF` reports the same widgets as a bitmask, `0x01` for GPU temp up to `0x80` for TGP, in this order.
All-zero flags turn the dashboard off.

Send `E1` at most once on (re)connect, only if `DF` shows the panel needs it, and never
periodically. It is the prime suspect in upstream's dark-panel incidents. `dashboard` has sent it
to the real panel twice, with no dark-panel event: `--off` read back as no widgets and the overlay
went from the panel, and all eight widgets every 4 s read back correctly. Before that, `DF` read
those eight widgets on a panel this tool had not written to.

The `brightness` command of `aorus_lcd.py` reuses `E1` with inferred meaning and is left out here.

## 4. Live sensor feed: `E3`

13 bytes of tail, with 16-bit fields big-endian and values clamped to their field:

```
[5]      GPU temp   u8, °C
[6..7]   GPU clock  u16, MHz
[8]      GPU usage  u8, %
[9..10]  fan        u16, RPM
[11..12] RAM clock  u16, MHz
[13]     RAM usage  u8, %
[14..15] FPS        u16
[16..17] TGP        u16, whole watts
```

- If `E3` is not pushed regularly the widgets freeze or go blank. Upstream pushes at 1 Hz and warns
  that slower starves the panel. On this card the adaptive feed's gaps of 5 s were fine: the widgets
  stayed visible.
- Upstream found that a frame only displays once the next one arrives within about 1 s, which is why
  the adaptive feed sends pairs.
- NVML has no FPS source, so that field is 0.
- `nvmlDeviceGetPowerUsage` reports a stuck value on RTX 50 cards. Read the instant power field
  first and fall back to that call.
- RAM usage is computed here as VRAM used over total. The C# reference sends memory-controller
  utilization in this field.
- Upstream found that the panel ignores frames of 18, 64 and 96 bytes. 128 bytes works but is
  unreliable on this card: 21 of 163 writes were refused against 1 of 162 at 256 bytes, and 5 of 12
  `status` polls failed during the feed.

The cost of the feed and the reasoning behind the adaptive default are in the README, under Daemon.

## 5. Overlay template: `EA`

A frame builder exists, but no command sends `EA`.

```
[5]      type: 1 GIF, 2 image, 3 pet
[6..8]   colour R, G, B
[9..10]  image X   [11..12] image Y
[13..14] data X    [15..16] data Y
[17]     enabled
```

Positions are big-endian.

## 6. Content upload

- 320x170 pixels, RGB565 little-endian, row-major.
- Sequence: `F2` BEGIN, `F1` header, payload in 256-byte zero-padded chunks, `F2` END, then `E5`.
  The chunk count is `len / 256 + 1`, so an exact multiple of 256 still gets one full padding chunk.
- Image and text payloads are the 12-byte descriptor `01 00 0B A9 01 00 40 01 AA 00 01 00` followed
  by the pixels.
- The firmware sets the pace: about 0.5 s after BEGIN, 1.0 s after the header, about 10 ms per chunk.
- Framebuffer targets in the `F1` header: image `0x01300000`, text `0x01320000`, GIF `0x00000000`
  (live buffer).
- **Order matters:** for GIFs send `E5` mode 5 before streaming. For image and text send it after.
- An `E5` alone does not repaint a wedged framebuffer, and without a genuine mode change it does not
  re-render. That is why `recover` uploads first, and goes through mode 6 if the target is mode 3.

### Display modes

| Mode | Screen | `E5` byte |
|---|---|---|
| 0 to 2 | Built-in stat screens | mode + 1 |
| 3 | Image | `0x04` |
| 4 | Text | `0x05` |
| 5 | GIF | `0x06` |
| 6 | Chibi clock | `0x07` |
| 7 | Carousel | `0x0A`, a GCC quirk (internal mode 9, plus 1) |
| 8 | Preset rotation | `0x0B` |

Mode 8 is only this tool's label for raw `0x0B`, which the real 5090 reported at rest. Its behaviour
is known only from upstream notes on an RTX 5080: a rotation of the built-in screens plus stored user
content. `DE` reads the carousel back as `0x08` or `0x0A`.

### GIF payload

- `u16 LE` frame count, then a 10-byte entry per frame: `u32 LE` **inclusive** end offset
  `2 + 10*N + sum(sizes) - 1`, `u16` width, `u16` height, `u16` format, where 3 is RLE. The
  concatenated RLE frames follow.
- The panel skips the first frame of a GIF: seen on firmware 1.3 of this card, by position and
  regardless of content. `gif` therefore sends the first frame twice, so the frame count on the wire
  is one more than the GIF has, and a GIF can have at most 255 frames. The 256 this leaves is a
  conservative cap, not a known firmware limit. This is a workaround for observed behaviour, not
  documented protocol.
- RLE tokens: a 2-byte LE head, where bit 15 is the run flag and the low 15 bits are the pixel count.
  A run, only for 3 or more identical pixels, is the head and one pixel. A literal is the head and
  that many pixels. Encoding works in windows of at most `0x7FFF` pixels. A window with no repeat, or
  with fewer than 4 pixels left, is one literal even if its pixels are equal. These quirks are
  reproduced exactly, because the firmware decoder mirrors the encoder.

## Design rules

- `protocol.rs` and `rle.rs` are pure functions that return byte buffers, so they are tested without
  hardware. A frame needs a named builder: there is no public way to frame arbitrary bytes.
- Byte layouts are copied from the references, never invented. Tests compare against vectors from
  `aorus_lcd.py` (see `tests/vectors/`).
- Every command supports `--dry-run`, which prints the frames, opens no device and takes no lock.
- All bus access goes through one exclusive file lock, `/run/aorus-lcd.lock`, so the daemon and an
  upload never interleave. Non-root users cannot create that file, so `dist/aorus-lcd.tmpfiles` does.
  There is no fallback to another path, which would break mutual exclusion.
- Refuse to write if the `EB 03` probe fails.
- The daemon sends only `EB 03`, `DF`, at most one `E1` per connection and `E3`, and nothing on
  shutdown. It never sends `AA`.
- Out of scope: `E1` "brightness", `FA` poweroff mode, the `0x76` LcdEx protocol, RGB, fan control.

## Source layout

```
src/
├── main.rs, commands/   clap CLI, one file per subcommand
├── bus/                 discovery.rs, transport.rs (raw I2C), lock.rs (flock)
├── protocol.rs          pure frame builders, opcodes, read-back parsers
├── rle.rs               RLE encoder and GIF payload builder
├── panel.rs             high-level operations: on/off, mode, upload, save, recover
├── session.rs           the write gate: adapter, lock, device, EB 03 probe
├── image.rs, text.rs    resizing to 320x170, RGB565, text rendering
├── sensors.rs           NVML polling
├── daemon.rs            the feed loop, signals, reconnect
└── config.rs            the daemon's config file, merged with the command line
assets/                  the embedded font and its licence
dist/                    systemd unit, config example, tmpfiles, udev, modules-load, PKGBUILD
tests/                   CLI tests in --dry-run, byte-parity tests against vectors
```
