# Connecting a Creality K1 to a Raspberry Pi

> This guide covers the current `serialmux`-based setup which replaces the
> earlier socat approach documented in the
> [original guide](https://rentry.co/k1-with-pi).

## Overview

`serialmux` is a C daemon that multiplexes MCU serial ports and TCP listener
forwardings over a single USB CDC ACM link between, in this case, a Creality K1
and Raspberry Pi (or any two Linux devices).

The TCP tunnel option is used in the case where you need to expose a Moonraker
listener port backwards over the tunnel to the MCU exporting machine, and should
not be used for any high-bandwidth application that might starve the USB link of
bandwidth for the MCU links.

## Prerequisites

- **Make sure your K1 mainboard has a populated micro-USB header. This is very
  risky to do unless you have a way to recover your K1 to stock.** See
  [Creality's recovery flashing instructions](https://github.com/CrealityOfficial/K1_Series_Annex/releases/tag/V1.0.0).
- You should already be using a probe supported by
  [Simple AF](https://pellcorp.github.io/creality-wiki/) because stock Klipper
  does not support the K1's multiple load cells. This guide assumes a
  Cartographer over USB, but most probes should work.
- You will need an SBC that supports USB OTG mode. This guide assumes a
  Raspberry Pi 4, but other devices are technically possible.

## Hardware

1. #### Power considerations
    The Pi may backfeed power to the K1 via the VCC+ line of the USB cable. **This is very
    risky and can cause all sorts of hard-to-debug issues.** The Pi and K1 must still share
    ground, so only the VCC+ line should be cut -- leave shielding and GND intact.

    Preventing backfeeding (pick one):
    - Kapton tape over the VCC+ pin of the USB cable plugged into the printer
    - Cable surgery to snip the VCC+ wire on the cable
    - A USB power blocker dongle (widely available)
    - [A printed jig](https://www.thingiverse.com/thing:3044586/files) to block the VCC+ pin
    - JST connectors wired to the K1 mainboard's USB header with the VCC+ wire removed

    Powering the Pi (its USB-C port is occupied by OTG, pick one):
    - A USB-C power/data splitter (still needs VCC+ cut on the K1 side)
    - A PoE adapter or HAT
    - Regulated voltage to the GPIO power pins if you know exactly what you are doing

    > **Note for Pi 5 users:** The Pi 5 has unique power requirements and software-side
    > checks. A USB-C power splitter can supply rated power with an appropriate supply.

2. #### Data cables
    - Connect the Pi's USB-C port to the K1's USB header, **having addressed the VCC+ issue
      above**. Shielded cables are strongly recommended.
        - Simple: USB-A male to USB-C male into the K1's front USB port
        - Neat: a JST to USB cable plugged directly into the K1 mainboard header
        - Combined: a USB-C power + data splitter for the Pi plus a JST to USB cable with
          the power wire snipped
    - Connect the Cartographer to the Pi over USB using the included cable
    - Recommended: unplug the camera cable from the K1, adapt it from JST to USB, and plug
      into the Pi
    - Optional: redirect the K1's front USB port to the Pi using the same JST adapter

    You may also need male/male or female/female JST adapter cables.

## Software

The following files are involved:

- `src/` -- C source for `serialmux` (runs on both K1 and Pi)
- `S99pik1` -- K1 init script
- `setup_pik1.sh` -- Pi gadget setup script
- `pik1.service.in` -- Pi systemd service template

### Building

Pre-built binaries for K1 (`build/serialmux.mipsel`) and Pi
(`build/serialmux.aarch64`) are included in the repo and are updated with each
release. If you want to build from source:

```bash
task toolchain   # one-time: downloads musl.cc cross-compilers into .toolchain/
task mipsel      # K1 binary  → build/serialmux.mipsel
task aarch64     # Pi binary  → build/serialmux.aarch64
```

### Raspberry Pi side

1. #### Install Simple AF for RPi
    Install [Simple AF for RPi](https://pellcorp.github.io/creality-wiki/rpi/).

2. #### Enable USB OTG mode
    Run the following script once to configure the Pi to act as a USB gadget. This
    puts the USB-C port into OTG/peripheral mode, which disables host mode on that
    port.

    ```bash
    #!/usr/bin/env bash
    set -euo pipefail

    BOOT=/boot/firmware   # Use /boot for pre-Bookworm images
    CFG="$BOOT/config.txt"
    CMD="$BOOT/cmdline.txt"

    # Enable dwc2 overlay (puts USB-C into OTG mode)
    grep -qxF 'dtoverlay=dwc2' "$CFG" || echo 'dtoverlay=dwc2' | tee -a "$CFG"

    # Add dwc2 to kernel cmdline (guard against running twice)
    grep -qF 'modules-load=dwc2' "$CMD" || sed -i.bak -E 's/$/ modules-load=dwc2/' "$CMD"

    # Autoload libcomposite at boot
    grep -qxF 'libcomposite' /etc/modules || echo 'libcomposite' | tee -a /etc/modules

    echo "Done. Reboot for changes to take effect."
    ```

    Reboot the Pi after running this.

    > **If the gadget fails to bind after reboot**, try changing `dtoverlay=dwc2`
    > to `dtoverlay=dwc2,dr_mode=peripheral` in `/boot/firmware/config.txt`. Some
    > Pi configurations require the mode to be set explicitly.

3. #### Install pik1
    From the repo directory on the Pi (pre-built binary included):

    ```bash
    task install-pi
    ```

    This copies the binary and setup script to `/opt/pik1/`, installs and enables
    `pik1.service`, and runs `systemctl daemon-reload`. Pass `SUDO=` if running as
    root, or `PI_DIR=/your/path` to override the install prefix.

    The service runs `setup_pik1.sh` as root first (needed for configfs access),
    then starts the serialmux host daemon as UID 1000 so the PTY devices it creates
    are accessible to Klipper.

    If there are any weird permissions errors then make sure UID 1000 (your klipper user,
    typically `pi` or similar) is in the `dialout` group so it can open `/dev/ttyGS0`:

    ```bash
    sudo usermod -aG dialout $(id -un 1000)
    ```

4. #### Configure printer.cfg
    Add or update the MCU serial paths in your `printer.cfg`:

    ```ini
    [mcu]
    serial: /tmp/klipper_mcu
    restart_method: command

    [mcu nozzle_mcu]
    serial: /tmp/klipper_toolhead
    restart_method: command
    ```

    > `restart_method: command` is required. Hardware reset via DTR/RTS does not work over the serialmux tunnel as no hardware control lines are available.

### K1 side

1. #### Install Simple AF
    Install [Simple AF](https://pellcorp.github.io/creality-wiki/) on the K1.

2. #### Install pik1
    From the repo directory on the K1 (pre-built binary included):

    ```bash
    task install-k1
    ```

    This copies `build/serialmux.mipsel` to `/usr/data/pik1/serialmux`, installs
    `S99pik1` to `/etc/init.d/`, and disables the services below by renaming them
    with a `_` prefix so the init system skips them:

    | Service | Reason |
    |---|---|
    | `S55klipper_service` | Must not run — K1 is now bridge-only |
    | `S56moonraker_service` | Must not run — no local Klipper |
    | `S55klipper_mcu` | Host MCU software, not needed |
    | `S50nginx_service` | Proxied Moonraker, no longer relevant |
    | `S50unslung` | Unrelated to printing |
    | `S50webcam` | Camera can be moved to the Pi |
    | `S99guppyscreen` | See optional TCP tunnel section below |

    If transferring via scp rather than running from a repo clone on the K1:

    ```sh
    scp build/serialmux.mipsel root@<k1-ip>:/usr/data/pik1/serialmux
    scp S99pik1 root@<k1-ip>:/etc/init.d/S99pik1
    ssh root@<k1-ip> chmod +x /usr/data/pik1/serialmux /etc/init.d/S99pik1
    ```

    Then run the service rename loop manually on the K1.

3. #### Restart the K1
    ```sh
    reboot
    ```
    Upon start, the daemon logs to `/tmp/pik1.log`.

## Optional: K1 touchscreen (TCP tunnel)

The serialmux TCP tunnel forwards the SimpleAF K1 touchscreen's (guppyscreen) Moonraker
requests to the Pi over the USB link. This is strongly recommended over the
alternative of pointing guppyscreen at the Pi's WiFi IP address -- WiFi is
unreliable enough that you will eventually lose display functionality mid-print.
The tunnel runs over the same wired USB link as the MCU bridge and stays up as
long as the physical connection does.

The tunnel is low-bandwidth and intended for Moonraker API traffic only
(temperatures, print status, controls). Do not route webcam streams or file
transfers through it.

guppyscreen requires no configuration changes -- it continues talking to
`localhost:7125` as normal and the tunnel forwards those connections to the Pi
transparently.

To enable, add a `tcp` channel spec to both sides.

**K1 init script** -- edit `/etc/init.d/S99pik1` and add the tcp channel to
`DAEMON_ARGS`:

```sh
DAEMON_ARGS="exporter --usb $USB_ID mcu:0:$MCU_DEV:$MCU_BAUD mcu:1:$NOZZLE_DEV:$NOZZLE_BAUD tcp:2:0.0.0.0:7125"
```

Also re-enable guppyscreen if you disabled it:
```sh
mv /etc/init.d/_S99guppyscreen /etc/init.d/S99guppyscreen
```

**Pi systemd service** -- edit `/etc/systemd/system/pik1.service` and add the
tcp channel to `ExecStart`:

```ini
ExecStart=/opt/pik1/serialmux host /dev/ttyGS0 \
    mcu:0:/tmp/klipper_mcu \
    mcu:1:/tmp/klipper_toolhead \
    tcp:2:127.0.0.1:7125
```

Then reload and restart:

```bash
sudo systemctl daemon-reload
sudo systemctl restart pik1
```

No changes are needed to guppyscreen's configuration -- it continues
talking to `127.0.0.1:7125` as if Moonraker were local.

## Post-install verification

1. #### Pi service
    ```bash
    sudo systemctl status pik1
    journalctl -u pik1 -f
    ```
    Should show `active (running)`. A normal startup looks like:
    ```
    setup_pik1: loading libcomposite
    setup_pik1: creating gadget at /sys/kernel/config/usb_gadget/pik1
    setup_pik1: binding gadget to UDC: fe980000.usb
    setup_pik1: ttyGS0 ready
    Link: opened /dev/ttyGS0
    serialmux host started
    Link: received HELLO from peer
    Link: handshake complete (host)
    PTY ch0: READY -> ACTIVE
    PTY ch0: opened /dev/pts/2 -> /tmp/klipper_mcu
    PTY ch1: READY -> ACTIVE
    PTY ch1: opened /dev/pts/3 -> /tmp/klipper_toolhead
    ```

2. #### K1 log
    ```bash
    cat /tmp/pik1.log
    ```
    A normal startup looks like:
    ```
    08:54:10 MCU ch0: opened /dev/ttyS7 @ 230400
    08:54:10 MCU ch1: opened /dev/ttyS1 @ 230400
    08:54:10 Link: opened /dev/ttyACM0
    08:54:10 serialmux exporter started
    08:54:10 MCU ch0: 0x7E seen at offset 13 -> ACTIVE
    08:54:10 MCU ch0: INIT -> ACTIVE
    08:54:10 MCU ch1: 0x7E seen at offset 13 -> ACTIVE
    08:54:10 MCU ch1: INIT -> ACTIVE
    08:54:28 Link: received HELLO from peer
    08:54:28 Link: handshake complete (exporter)
    ```

3. #### dmesg (Pi)
    ```
    [    7.614279] dwc2 fe980000.usb: bound driver configfs-gadget.pik1
    [    7.846025] dwc2 fe980000.usb: new device is high-speed
    ```

4. #### Klipper behaviour
    Klipper should connect to both MCUs within about 15 seconds of the K1 booting --
    this is the GD32 bootloader dwell time and is normal. `FIRMWARE_RESTART` also takes
    approximately 15 seconds for the same reason.

    A good indicator of successful first-boot setup is the printer's LEDs turning off
    and then back on as the MCUs initialise under Klipper control.

## Switching back to standalone K1 / Simple AF

1. #### Uninstall pik1 from K1
    ```sh
    task uninstall-k1
    ```
    This removes the init script and binary and restores all disabled services.
    Alternatively:
    ```sh
    mv /etc/init.d/S99pik1 /etc/init.d/_S99pik1
    mv /etc/init.d/_S55klipper_service  /etc/init.d/S55klipper_service
    mv /etc/init.d/_S56moonraker_service /etc/init.d/S56moonraker_service
    # restore any other services as needed
    ```

2. #### Stop Pi service
    ```bash
    sudo systemctl stop pik1
    ```
    Optional -- the service will just sit idle if left running.

3. #### Revert printer.cfg and recable
    Revert `printer.cfg` serial paths and reconfigure cables as needed.

    You can run both the Pi and K1 standalone simultaneously (e.g. for camera
    services) without conflict as long as the serialmux init script is disabled.

## Optional extras

- **Pi mount:** Once everything is working you can print
  [a nice combined mount](https://www.printables.com/model/585116-motherboard-cover-optional-rpi-mount-extension-cre)
  for the K1 mainboard and a Raspberry Pi.
- **KlipperScreen:** The Pi runs KlipperScreen by default. Connect an HDMI
  touchscreen to the Pi's HDMI port to use it, or uninstall it if not needed.

---

## CB1 / K1C adapter notes

### BTT CB1 (and CM4-compatible adapters)

The CB1 (and similar CM4-footprint modules) exposes its OTG/device-mode USB on
the **USB-C** port labelled "OTG" on most carrier boards (e.g. Manta, SKR
mini E3 v3 in OTG mode).

1. Ensure the carrier board's USB-C port is wired to the CB1's OTG USB2
   controller (check your carrier board's schematic — it must NOT be connected
   to the host-mode USB-A hub).
2. Enable the `dwc2` overlay in `/boot/BOOT/BoardEnv.txt` (Armbian/BTT image)
   or via the standard `/boot/firmware/config.txt` mechanism, same as Pi 4:
   ```
   overlays=dwc2
   ```
   or for explicit peripheral mode:
   ```
   overlays=dwc2
   param_dwc2_dr_mode=peripheral
   ```
3. `setup_pik1.sh` probes `ls /sys/class/udc/` to find the UDC name — this
   will differ from the Pi 4's `fe980000.usb`.  Inspect the output and, if
   needed, hard-code the UDC in `setup_pik1.sh`.
4. Everything else (gadget, serialmux, service) is identical to the Pi 4 path.

### K1C front USB → Pi routing

On the **K1C**, the front-panel USB-A port is connected to the main SoC over a
USB hub.  You can route it to the Pi by plugging a short USB-A to USB-C cable
from the K1C front port into the Pi's OTG port.

- The `S99pik1` init script uses `--usb 1d6b:0104` (default for the K1/K1C CDC
  gadget composite device).  Update `USB_ID` in the script if your K1C firmware
  uses a different VID:PID (check with `lsusb` on the Pi once connected).
- The front USB port's hub controller may re-enumerate devices on reboot;
  `serialmux` handles this automatically by re-scanning sysfs on reconnect.

---

## Using the Rust binary (`serialmux-rs`)

A Rust port of the daemon is available in the `serialmux-rs/` directory.
It exposes **exactly the same CLI** as `serialmux.py` and is a drop-in
replacement — only the binary path changes in `S99pik1` / `pik1.service`.

The Rust binary is useful when you need lower CPU overhead on constrained
hardware (e.g. CB1 at 1.2 GHz), or want a single static binary with no Python
runtime dependency.

### Build

Cross-compile for 32-bit ARMv7 (K1 / Manta CB1) or 64-bit AArch64 (Pi 4 /
CB1 in 64-bit mode).  A native Rust toolchain on the target also works.

```bash
# Install Rust (once)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Native build (run on the target machine, or in a Pi 4 SSH session)
cd serialmux-rs
cargo build --release
# Binary: target/release/serialmux

# ARM cross-compile from x86-64 (e.g. on your laptop or CI)
rustup target add armv7-unknown-linux-gnueabihf  # for K1 / CB1 ARMv7
cargo build --release --target armv7-unknown-linux-gnueabihf
# Binary: target/armv7-unknown-linux-gnueabihf/release/serialmux
```

> **CB1 / Pi 4 AArch64:**
> ```bash
> rustup target add aarch64-unknown-linux-gnu
> cargo build --release --target aarch64-unknown-linux-gnu
> ```

### Install

Copy the binary to `/opt/pik1/` on both machines:

```bash
sudo cp target/release/serialmux /opt/pik1/serialmux
sudo chmod +x /opt/pik1/serialmux
```

### Update `S99pik1` (K1 exporter)

Change the interpreter line:

```sh
# Before (Python):
/usr/bin/python3 /usr/data/pik1/serialmux.py exporter ...

# After (Rust binary):
/usr/data/pik1/serialmux exporter ...
```

### Update `pik1.service` (Pi host)

```ini
# Before:
ExecStart=/usr/bin/python3 /opt/pik1/serialmux.py host /dev/ttyGS0 ...

# After:
ExecStart=/opt/pik1/serialmux host /dev/ttyGS0 ...
```

Then reload systemd:
```bash
sudo systemctl daemon-reload && sudo systemctl restart pik1
```

---

## USB back-power — hardware fix

> **This is a hardware-only issue.  There is no software workaround that is
> reliable on all boards.**

When the Pi's OTG port supplies 5 V VBUS it can back-power the K1 through the
cable.  This causes unpredictable resets, brown-outs, and boot failures on both
sides.

**Fix: remove VBUS (5 V) from the cable — keep GND and the two data lines.**

| Method | Notes |
|---|---|
| Kapton tape over the VBUS pin | Quick; reversible; can slip |
| Snip the red wire inside the cable | Permanent; best for dedicated cables |
| USB power-blocker dongle | Easiest off-the-shelf option |
| [Printed VBUS pin jig](https://www.thingiverse.com/thing:3044586/files) | Precise; reusable; recommended |
| JST wiring harness to mainboard header, VBUS omitted | Cleanest permanent solution |

GND **must** remain connected for the data link to work.

---

## Windlass bridge (native Klipper transport)

> **opt-in, not compatible with `serialmux`** — both the K1 exporter and the
> Pi/CB1 host must run `windlass-bridge` at the same time.

`windlass-bridge` is an alternative to `serialmux` that relays raw Klipper
transport frames directly over the USB CDC ACM link instead of wrapping them
in the serialmux envelope.

### Comparison

| Property | serialmux | windlass-bridge |
|---|---|---|
| Framing overhead per MCU frame | ~10 bytes (serialmux envelope) | 1 byte (channel index only) |
| Host PTY required | Yes | No (Unix domain socket) |
| TCP channel tunnelling | Yes | No |
| I/O model | `mio` single-thread | `tokio` async tasks |
| Klipper config change required | No | Yes (socket path) |
| Compatible with C/Python daemon | Yes | No |

### Tunnel wire format

```
[ ch_id : u8 ][ raw Klipper frame : 5..=64 bytes ]
```

The raw Klipper frame is self-delimiting (length byte at `[0]`, sync byte
`0x7E` at `[end]`), so the tunnel adds only **one byte** of overhead per frame.

### Build

```bash
cd serialmux-rs
cargo build --release --features windlass
# Produces: target/release/windlass-bridge
```

### Deploy

**K1 / K1C SoC (exporter)**  
Copy `windlass-bridge` to `/usr/data/pik1/windlass-bridge`, then edit
`/etc/init.d/S99pik1` — follow the commented-out instructions in that file to
switch from the `serialmux.py` block to the `windlass-bridge` block.

**Pi / BTT CB1 (host)**  
Copy `windlass-bridge` to `/opt/pik1/windlass-bridge`, then edit
`/etc/systemd/system/pik1.service` — follow the commented-out instructions to
switch `ExecStart` from `serialmux.py` to `windlass-bridge host`.

### Klipper config

Replace the PTY device path with the Unix socket path in `printer.cfg`:

```ini
# Before (serialmux):
[mcu]
serial: /tmp/klipper_mcu        # symlink to /dev/pts/…

# After (windlass-bridge):
[mcu]
serial: /tmp/klipper_mcu0
restart_method: command

[mcu nozzle_mcu]
serial: /tmp/klipper_mcu1
restart_method: command
```

Klipper supports Unix domain socket paths directly in the `serial:` field, so
no additional configuration is needed.

### Limitations

- **No TCP channel tunnelling** — `F_TCONN`/`F_TDATA`/`F_TCLOSE` frames are
  not supported.  Users who rely on the TCP tunnel (e.g. for Moonraker) must
  keep using `serialmux`.
- **No PTY** — Klipper's `serial:` field must point to the Unix socket path,
  not a `/dev/pts/…` device.
- **Both ends must match** — mixing a `serialmux` exporter with a
  `windlass-bridge` host (or vice versa) will not work.
