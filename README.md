# aorus-lcd-rs

A small Rust tool and daemon that drives the **LCD Edge View** side screen of the
**Gigabyte AORUS GeForce RTX 5090 MASTER** on Linux (Arch), with no Gigabyte software.

On Linux the panel otherwise just loops a built-in animation. With this tool it can:

- **Show live GPU stats** (temperature, clocks, usage, fan, VRAM, power), fed by a background
  daemon that starts at boot.
- **Show your own content**: image, text or animated GIF.
- **Control the panel**: on and off, built-in screens, carousel list.
- **Repair a frozen or dark panel** with `aorus-lcd recover`.

It has been tested on one card, and parts of it have never run on real hardware: see
[Status](#status). Fan curves, overclocking, firmware flashing and RGB lighting are out of scope.

## Setup on Arch

You need the proprietary NVIDIA driver (`nvidia` or `nvidia-open`, plus `nvidia-utils`) and this
exact card. `aorus-lcd` does not check the card's identity, so check it yourself first:

```bash
lspci -nnk -d 10de: | grep -A2 VGA          # Subsystem: Gigabyte [1458:416e]
```

The subsystem ID is the reliable identifier: `Board Part Number` in `nvidia-smi -q` can read `N/A`.
Other Gigabyte LCD cards may use a newer protocol at address `0x76`, which this tool does not
support. If `aorus-lcd probe` fails on your card after the wait below, stop there.

### Manual install

```bash
sudo pacman -S --needed base-devel rust
git clone https://github.com/Altrabeth/Gigabyte-RTX5090-Monitor-Tool
cd Gigabyte-RTX5090-Monitor-Tool
cargo build --release --locked

# I2C access, and the bus lock file, which non-root users cannot create
sudo groupadd -f i2c
sudo install -Dm644 dist/i2c-dev.conf /etc/modules-load.d/i2c-dev.conf
sudo install -Dm644 dist/99-i2c.rules /etc/udev/rules.d/99-i2c.rules
sudo install -Dm644 dist/aorus-lcd.tmpfiles /etc/tmpfiles.d/aorus-lcd.conf
sudo modprobe i2c-dev
sudo udevadm control --reload && sudo udevadm trigger
sudo systemd-tmpfiles --create /etc/tmpfiles.d/aorus-lcd.conf
sudo usermod -aG i2c "$USER"
```

The group only applies after a reboot. Logging out is usually not enough, because `systemd --user`
survives it with the old groups, and a Plasma session runs under it. To carry on without rebooting,
start a shell with the group for now. The service does not need this.

```bash
newgrp i2c
```

Then install the binary and the daemon:

```bash
sudo install -Dm755 target/release/aorus-lcd /usr/local/bin/aorus-lcd
aorus-lcd probe --wait 60
sudo install -Dm644 dist/aorus-lcd.service /etc/systemd/system/aorus-lcd.service
sudo systemctl enable --now aorus-lcd
```

The service runs as an unprivileged dynamic user in group `i2c`. Optionally, install the example
config, which changes nothing until a setting in it is uncommented:

```bash
sudo install -Dm644 dist/aorus-lcd.toml /etc/aorus-lcd.toml
```

### Boot into the stat screen

The daemon does not set the display mode, so after a cold boot the panel shows what is saved in its
NVRAM, which may not be a screen with widgets. Run this once. `save` writes the panel's NVRAM:

```bash
aorus-lcd mode 0
aorus-lcd save
```

Whether a save survives a cold power-off has not been verified.

### Package

[`dist/PKGBUILD`](dist/PKGBUILD) builds a package from the `v0.1.0` tag, but that tag does not exist
yet and nothing is on the AUR, so the manual install is the only route for now. The PKGBUILD has
never been installed.

If you later move from the manual install to the package, clear these first:

- An existing `/etc/aorus-lcd.toml` makes pacman stop with a file conflict. Move it aside and put it
  back after installing.
- `/etc/systemd/system/aorus-lcd.service` shadows the packaged unit and still points at
  `/usr/local/bin`. Remove it and the binary:

```bash
sudo systemctl disable --now aorus-lcd
sudo rm /etc/systemd/system/aorus-lcd.service /usr/local/bin/aorus-lcd
sudo systemctl daemon-reload
```

## Status

The project is still being verified. Everything below was established on one RTX 5090 Master:
firmware 1.3, driver 615.78.08, subsystem `1458:416e`, kernel 6.18.

**Confirmed by eye on the panel:**

- Image upload shows correctly.
- GIFs play all frames, with the first frame sent twice.
- `E3` layout: fixed test values showed exactly on all 8 widgets.
- Adaptive feed under load: widgets stay visible and update about every 6 s.
- `text` renders correctly, but not in the colour asked for. `text "..." --no-effect --color ffaa00`
  showed the right text in a wandering rainbow, not orange. No `AA` was sent in that run, and one had
  been sent earlier in mode 0. That is one run, and whether the rainbow is permanent in text mode or
  was enabled by that save is unknown.
- `dashboard --off` read back as no widgets, and the overlay disappeared from the panel.

**Confirmed by read-back only**, so the panel's answers were right but nobody checked the screen:

- `probe`, `status`, `on`, `off`
- `mode` 0, 1, 7 and 8
- `carousel` storing its list
- `dashboard` with all 8 widgets at 4 s
- `recover` with default options
- `save`

**Not verified:**

- What `AA` does in text mode: the `text` run above sent none.
- Whether the carousel actually plays after `mode 7`, and the unit of its interval.
- `recover` on an actually dark panel, or with `--overlay` or `--clear-overlay`.
- `--no-overlay`, and an `E1` sent by the daemon. `E1` has reached the real panel only through the
  two `dashboard` runs above, with no dark-panel event.
- The systemd unit and the PKGBUILD have never been installed, because no root was available.
- Frame-rate effect in real games. The 35 ms driver stall under [Feed cost](#feed-cost) is a proxy.
- Suspend and resume.

**After a power loss**, from one shutdown on 2026-11-06:

- The panel started in mode 8 with no widgets, every 0 s. Before, it had been in mode 0 with all
  eight widgets every 4 s, and `save` had been sent in mode 0. So `save` did not keep the mode.
- The stored text, image and GIF all came back. Uploads write persistent memory.
- So there is no screen with several stats at once. The only way to draw one would be to upload a
  picture of the stats, and refreshing it every few seconds could wear out that memory.

**Soak test:** 3 hours with the default settings on an idle GPU, never above 5%. 10,749 of 10,755
`E3` writes succeeded, with no reconnects and no unexplained mode change. It says nothing about long
runs under load, and nobody watched the screen for the whole run.

**Open issue:** once the panel reported mode 5 (GIF) after feed tests without any `E5` being sent,
and neither three later attempts nor the soak test reproduced it. It is unexplained. If it happens, run
`aorus-lcd mode 0`.

## Commands

```
aorus-lcd probe [--wait SECS]
aorus-lcd status
aorus-lcd on
aorus-lcd off
aorus-lcd mode <0-8>
aorus-lcd image <FILE> [--save] [--no-overlay]
aorus-lcd text <TEXT> [--color RRGGBB] [--bg RRGGBB] [--size PIXELS] [--no-effect] [--no-overlay]
aorus-lcd gif <FILE> [--frame-delay MS] [--no-overlay]
aorus-lcd carousel <MODES> [--interval INTERVAL]
aorus-lcd dashboard [--widgets LIST] [--interval SECS] [--off]
aorus-lcd daemon [--config PATH] [--widgets LIST] [--interval SECS]
                 [--frame-bytes 256|128] [--feed steady|adaptive]
aorus-lcd recover [--mode 0-8] [--color RRGGBB] [--no-powercycle] [--no-upload]
                  [--overlay | --clear-overlay] [--save]
aorus-lcd save
```

Every command also takes `--bus N`, which replaces autodetection and must still name an NVIDIA
adapter, `--dry-run`, which prints the frames and touches no bus, and `-v`, repeatable. Use
`aorus-lcd <command> --help` for the full text of each option.

- **`probe`** checks that the controller answers and shows the adapter, bus and raw status.
  `--wait SECS` keeps retrying, for after a boot.
- **`status`** shows firmware, mode, on-state, widgets and the carousel banks.
- **`mode`**: 0 to 2 are the stat screens, 3 image, 4 text, 5 GIF, 6 chibi clock, 7 carousel, 8 preset
  rotation. No built-in screen shows more than one stat at a time, checked by eye on modes 0, 1, 2
  and 6 and on an image: widgets rotate one by one. Mode 5 shows the last GIF uploaded, which the
  panel keeps without power. A GIF of numbers is a picture, so its values never change. Mode 8 is
  where the panel starts after a power loss. After one, it showed the stored text, then the stored
  image, then the built-in animation, which fits upstream notes on an RTX 5080: a rotation of the
  built-in screens plus stored user content. The firmware overlay
  is drawn over GIF content too, checked by eye: a value appeared over a GIF while widgets were
  enabled, and vanished once they were cleared.
- **`image`**, **`gif`**: the picture is resized to 320x170 without keeping its aspect ratio. `image`
  reads PNG, JPEG, BMP, GIF and WebP. A GIF can have at most 255 frames. `--frame-delay` is at least
  1, and more than 255 is sent as 255. Without it, the mean delay of the GIF's frames is used.
- **`text`** centres the text on the panel. A line break or a literal `\n` starts a new line, and long
  lines wrap on spaces. The default colour is `8b8d8b`, the reference's grey for the panel's rainbow,
  on `000000` at size 28. By default this also sends `AA`, which writes NVRAM, and the references
  expect that frame to start the rainbow. `--no-effect` skips `AA`, so it means no NVRAM write. Do not
  expect it to stop the rainbow: the one run seen had the rainbow without `AA`, and `--color` made no
  visible difference, see [Status](#status). For fixed-colour text, render it to a picture and use
  `image`, which avoids text mode.
- **`carousel`** takes modes such as `0,1,4`, each from 0 to 6, and an `--interval` that defaults to
  5 and is sent as it is. It only stores the list and does not change the mode, so `aorus-lcd mode 7`
  is needed to play it. Whether it then plays is [unverified](#status).
- **`dashboard`** chooses the widgets, from `temp`, `clock`, `usage`, `fan`, `ramclock`, `vram`, `fps`
  and `power`. The defaults are `temp,clock,usage,fan,vram,power` every 3 s. `--off` turns every
  widget off. The setting is not saved, and widgets freeze without a running daemon.
- **`recover`** is described under [Troubleshooting](#troubleshooting).
- **`save`** sends `AA`, which the references describe as saving the panel configuration to NVRAM.
  On this panel it did not keep the mode: one sent in mode 0 was followed by mode 8 after a power
  loss, see [Status](#status). The references also send this frame to start the rainbow in text
  mode, see `text` above.

Commands that send `AA`: `save`, `image --save`, `recover --save`, and `text` without
`--no-effect`. The daemon never does.

Every upload, from `image`, `text`, `gif` or `recover`, writes the panel's persistent memory: all
three kinds of content survived a power loss. That is fine for an upload now and then, but do not
run uploads in a loop, because nobody knows how many writes that memory can take.

An upload sends no `E1`, so a sensor overlay stays on top of the content. `--no-overlay` sends `E1`
with no widgets after a successful upload, which clears the panel's selection rather than hiding it.
It stays cleared until `dashboard` sets it or a daemon connects, because the daemon puts widgets back
on a panel that has none.

The daemon does not need stopping for an upload or `recover`: they share the bus lock, and it skips
its ticks while they run.

## Daemon

`aorus-lcd daemon` pushes the sensor values the widgets need, and retries by itself while `i2c-dev`,
the adapter, NVML or the panel are not ready. It stops on SIGTERM or SIGINT and sends nothing on the
way out, so the panel is left as it is.

```bash
systemctl status aorus-lcd
journalctl -u aorus-lcd -b
```

For more detail in the journal, add `-v` before `daemon` in a drop-in for `ExecStart`.

### Config

The daemon reads `/etc/aorus-lcd.toml` once at start, if it exists. `--config PATH` reads another
file instead, which then must exist. Options on the command line beat the file, and the file beats
the defaults. An unknown key, a bad value, or a file that exists but cannot be read stops the daemon
before it touches the bus. See [`dist/aorus-lcd.toml`](dist/aorus-lcd.toml).

| Key | Option | Values | Default |
|---|---|---|---|
| `widgets` | `--widgets` | One comma-separated string of the widget names above | Keep what the panel has |
| `interval` | `--interval` | Seconds per widget, 1 to 255. Needs `widgets`, or it is an error. | 3, when widgets are named |
| `feed` | `--feed` | `"adaptive"` or `"steady"` | `"adaptive"` |
| `frame_bytes` | `--frame-bytes` | 256 or 128 | 256 |
| `bus` | `--bus` | An NVIDIA adapter's bus number | Autodetect |

The daemon sends `E1` only if the panel needs it, so a config that names no widgets keeps the panel's
selection and interval. The exception is a panel with no widget enabled, which gets the defaults
above. With `widgets` set, `E1` goes out on connect whenever the panel's widgets or interval differ.
`E1` has been sent to this panel twice with no dark-panel event, but it is still the prime suspect
in upstream's dark-panel incidents. The daemon avoids it unless needed, so leave `widgets` out unless
you need it.

### Feed cost

The panel needs regular `E3` frames, and each one is an I2C write while you may be gaming.

- One 256-byte write cost 28 ms on average and 38 ms at most, consistent with a 100 kHz bus. The same
  bus runs at 400 kHz on Windows, and Linux offers no way to change this.
- With a 1 Hz feed, other NVIDIA driver calls stalled once per second, for up to 35 ms. With the feed
  off, none stalled over 10 ms in 60 s. This stall is a proxy, measured as `nvmlDeviceGetMemoryInfo`
  latency. It is not a measured game frame drop.

That is why the default is `--feed adaptive`: a frame every second under 50% GPU usage, and from there
up a pair of frames about every 6 s, carrying the last 5 s of peaks (temperature, fan, power) and
averages (clocks, usage). The widgets stay visible under load but update less often. `--feed steady`
sends a frame every second at any load. What either feed costs in frame rate is unknown, and no
frame-rate gain is claimed.

Upstream measured 1% lows of about 141 fps with a 1 Hz feed against about 182 with almost no writes.
That was on an RTX 5080 on Windows and has not been reproduced here.

`--frame-bytes 128` means less time on the bus but is unreliable on this card: 21 of 163 writes were
refused against 1 of 162 at 256 bytes, and 5 of 12 `status` polls failed during the feed.

## Troubleshooting

### The widgets do not match `nvidia-smi`

This is expected. Fixed test values proved that the panel shows exactly what is sent.

| Widget | Panel | `nvidia-smi` |
|---|---|---|
| Fan | RPM, or percent if the RPM call fails | Percent |
| RAM usage | Percent of VRAM | MiB |
| Power | Instant | Averaged |

Values lag about 1 s, and under load the adaptive feed adds the peaks and averages described in
[Feed cost](#feed-cost). The FPS widget stays at 0 because NVML has no FPS source. A sensor that fails
also shows 0.

### The panel is frozen or stale

The panel keeps standby power across a reboot, so old content can stay on it after a crash. Without a
running daemon the widgets freeze or go blank, and a daemon that stops or crashes sends nothing to
clear them.

```bash
systemctl status aorus-lcd
sudo systemctl restart aorus-lcd
```

The unit restarts after a failure, but gives up after 5 failed starts in 5 minutes. Fix the cause
shown in the journal, then run `sudo systemctl reset-failed aorus-lcd` before starting it again.

### Recovering the panel

This order comes from [`docs/RECOVERY.md`](https://github.com/CodeTorchAI/AorusLcd/blob/main/docs/RECOVERY.md)
of CodeTorchAI/AorusLcd, whose incidents these are. `recover` on an actually dark panel is
[unverified](#status) here. Start with `aorus-lcd status`, which sends only the probe and read-backs,
and make sure nothing else is using the bus.

1. **Wedged framebuffer.** The panel reports a sane mode and on-state but shows a frozen image. A mode
   change alone does not repaint it, because the wedged content sits in the framebuffer and only an
   upload clears it. `aorus-lcd recover` does this: it turns the panel off and on, uploads a solid
   frame, and switches to the target mode, 0 by default. It sends no `E1` and writes no NVRAM.
   `--mode`, `--color`, `--no-powercycle` and `--no-upload` adjust those steps.

2. **Dark panel that reports healthy.** Every read looks right and there is no light in any mode, and
   status cannot tell this from step 1. Upstream hit this twice. Every attempt known to have sent `E1`
   stayed dark, and both attempts without it lit the panel, each as a repeated power cycle. So run
   `aorus-lcd recover` again, with no `--overlay`, and keep `widgets` out of the daemon's config while
   you do. Upstream's working runs used a red frame (`--color FF0000`), probably by chance. There is
   no backlight command to fall back on.

3. **Cold power cycle.** If nothing lights it, shut down fully, then switch the PSU off or pull the
   cord for about 30 seconds. A reboot is not enough, because the module keeps standby power across
   one.

`--save` also writes the result to NVRAM, and is off by default so that a repair persists nothing.
`--overlay` sends `E1` with GPU temperature and TGP, and `--clear-overlay` sends it with no widgets.
A daemon started after `--clear-overlay` puts its default widgets back.

### Messages

The controller can need up to a minute after boot. Use `aorus-lcd probe --wait 60`. The daemon waits
by itself.

| Message | Cause and fix |
|---|---|
| `writing to the panel on /dev/i2c-N: no answer from the panel controller` | The controller is not up yet, or this is not a supported card. Wait as above. |
| `no adapter named "NVIDIA i2c adapter 1 at ..."; adapters seen:` | `sudo modprobe i2c-dev`, and check that the NVIDIA driver is loaded. |
| `opening /dev/i2c-N: no such device, is the i2c-dev kernel module loaded?` | `sudo modprobe i2c-dev`. |
| `opening /dev/i2c-N: permission denied, is this user in the i2c group?` | Add yourself to `i2c` and reboot, or run `newgrp i2c` for this shell, and check that the udev rule is installed. |
| `cannot open bus lock /run/aorus-lcd.lock: check the aorus-lcd tmpfiles.d entry and membership of the i2c group, ...` | Run the `systemd-tmpfiles --create` step from Setup. If you just joined `i2c`, reboot or run `newgrp i2c`: check with `id`, not `groups katee`, which reads the group file instead of your session. |
| `bus lock /run/aorus-lcd.lock is held by another process` | Another `aorus-lcd` kept the bus for over 5 s, usually an upload. Wait for it to finish. |
| `cannot recover while another process holds the bus: stop the daemon with ...` | The same for `recover`. Let the upload finish, or follow the message. |
| `refusing --bus N: i2c-N is "...", not an NVIDIA adapter` | `--bus` named the wrong adapter. Leave it out to autodetect. |
| Service fails with status `216/GROUP` | The `i2c` group does not exist: `sudo groupadd -f i2c`. |

The lock message also offers `AORUS_LCD_LOCK`, which replaces the lock path until the tmpfiles entry
is installed. Every process sharing the bus must use the same value. Two tools with different values
no longer exclude each other, and that includes the service, which does not see your shell's
environment.

## Safety

- Writes only to `0x61` on a verified NVIDIA adapter, never to the RGB controller at `0x75`, and
  refuses to write if the `EB 03` probe fails.
- No firmware flashing, and no persistent GPU state beyond the panel's own image memory and NVRAM.
- Worst failure seen in the reference project: stale or black panel until the next upload or a power
  cycle, never a bricked card. Still: use at your own risk.
- Don't run GCC-like tools or OpenRGB writes to this bus at the same time. They do not honour the
  lock.

## Protocol notes

[`docs/PROTOCOL.md`](docs/PROTOCOL.md) has the frame formats, opcodes, upload sequence, design rules
and source layout.

## Credits and license

Protocol reverse engineering: [albancreton/aorus-master-linux](https://github.com/albancreton/aorus-master-linux)
(MIT) and [CodeTorchAI/AorusLcd](https://github.com/CodeTorchAI/AorusLcd) (MIT).
This project is a clean reimplementation using protocol facts only; no vendor code.
Not affiliated with Gigabyte or NVIDIA.

The `text` command embeds [DejaVu Sans Bold](https://dejavu-fonts.github.io/) 2.37, under
the Bitstream Vera and Arev font licences (the DejaVu changes are in the public domain).
See [`assets/DejaVuSans-Bold.LICENSE`](assets/DejaVuSans-Bold.LICENSE).

License: MIT, except the font above.
