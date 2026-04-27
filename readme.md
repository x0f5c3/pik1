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
make toolchain   # one-time: downloads musl.cc cross-compilers into .toolchain/
make mipsel      # K1 binary  → build/serialmux.mipsel
make aarch64     # Pi binary  → build/serialmux.aarch64
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
    make install-pi
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
    make install-k1
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
    make uninstall-k1
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
