# GL.iNet Mudi V2 (experimental)

Rayhunter can run on the GL.iNet Mudi V2 travel router (GL-E750 V2). It is not
part of a release: build the daemon from source and install it by hand.

## Supported Bands

| Technology | Bands |
| ---------- | ----- |
| 4G LTE FDD | B1/B2/B3/B4/B5/B7/B8/B12/B13/B14/B17/B18/B19/B20/B25/B26/B28/B29/B30/B32/B66/B71 |
| 4G LTE TDD | B34/B38/B39/B40/B41/B42/B43/B46/B48 |

*Source: the LTE bands the EM060K-GL in a Mudi V2 reported to a test network
(UE Capability Information). The GL firmware keeps the modem in LTE.*

GL.iNet sells one global version of the modem; the region only changes the power
plug. GL.iNet does not offer cellular in some countries (see the product page).
Rayhunter has only been tested in Germany.

## Hardware and firmware versions

| Mudi V2 (GL-E750V2) | GL firmware | Status |
| --- | --- | --- |
| Quectel EM060K-GL | 4.3.26, 4.3.30 (OpenWrt 22.03.4) | **Tested** |
| Quectel EM060K-GL | other versions | Not tested |
| Quectel EM05-G (USB PID `030e`) | any | Not tested |

The router has no `/dev/diag`. Rayhunter reads diag from the modem's USB serial
port instead (`diag_serial`), on a MIPS 24Kc big-endian CPU.

## Building

Build the web UI first, otherwise the daemon embeds an empty page:

```sh
cd daemon/web && npm install && npm run build && cd ../..
```

MIPS is a Tier 3 target in Rust, so the standard library is built with nightly.
Use the linker from the OpenWrt 22.03.4 SDK for `ath79/nand`:

```sh
export CARGO_TARGET_MIPS_UNKNOWN_LINUX_MUSL_LINKER=<sdk>/staging_dir/toolchain-mips_24kc_gcc-11.2.0_musl/bin/mips-openwrt-linux-musl-gcc
cargo +nightly build --profile firmware -Z build-std=std,panic_abort \
  --target mips-unknown-linux-musl -p rayhunter-daemon --no-default-features --features pq-tls
```

Then build the package with the same SDK:

```sh
ln -s <repo>/dist/openwrt/rayhunter package/rayhunter
make package/rayhunter/compile RAYHUNTER_BIN=<repo>/target/mips-unknown-linux-musl/firmware/rayhunter-daemon
```

## Obtaining a shell

The router allows root SSH with the admin password of the GL web UI:

```sh
ssh root@192.168.8.1
```

## Installing

Download `rayhunter_<version>_mips_24kc.ipk` from the
[releases page](https://github.com/dialecticdaemon/rayhunter/releases), then
install it in one of two ways.

**In the router's web interface:** open *Advanced Settings* in the GL web UI to
get to LuCI and log in as `root` with the admin password. On a new or reset
router, LuCI has to be installed there first, which needs internet. Go to
*System → Software*, click *Upload Package…* and choose the file. Before
installing, LuCI shows its SHA256; compare it with the one on the releases page.

**Over SSH:**

```sh
scp rayhunter_<version>_mips_24kc.ipk root@192.168.8.1:/tmp/
ssh root@192.168.8.1 'opkg install /tmp/rayhunter_*_mips_24kc.ipk'
```

The package stores recordings on the SD card if one is mounted, otherwise in RAM
(see [Where recordings are stored](#where-recordings-are-stored)). It starts
Rayhunter right away and at every boot, about 2.5 minutes after power-on.
Logs: `logread -e rayhunter`. To remove it: `opkg remove rayhunter`; recordings
are kept.

The web UI is at `http://192.168.8.1:8089/`. It has no login: anyone on the
router's LAN or WiFi can see, download and delete recordings.

## Configuration

The package installs this `/etc/rayhunter/config.toml`; it is kept on upgrades:

```toml
qmdl_store_path = "/tmp/rayhunter/qmdl"
port = 8089
device = "mudi"
diag_serial = "usb:2c7c:030b:00"
diag_signalling_only = true
auto_check_updates = false
min_space_to_start_recording_mb = 20
min_space_to_continue_recording_mb = 15
```

`port` must not clash with the GL web UI on port 80.

| Key | Default | Description |
| --- | --- | --- |
| `diag_serial` | not set | Read diag from a USB serial port instead of `/dev/diag`. `usb:VID:PID:INTERFACE` finds the tty again after the modem re-enumerates; a path such as `/dev/ttyUSB0` can change. |
| `diag_signalling_only` | `false` | Only log the RRC and NAS messages the analyzers use. Leaves out user IP traffic, cell measurements and MAC logs. Identities in signalling (IMSI, IMEISV, TMSI) and cell IDs are still recorded. |
| `store_informational_events` | `false` | Also store analysis rows that only contain Informational events. |

## Where recordings are stored

| `qmdl_store_path` | Kept after reboot | Notes |
| --- | --- | --- |
| `/tmp/rayhunter/qmdl` (RAM) | no | No SD card needed, nothing is written to flash. |
| `/mnt/sda1/rayhunter/qmdl` (SD card) | yes | Rayhunter waits until the card is mounted. |
| on the router's flash, e.g. `/root/rayhunter/qmdl` | yes | About 60 MB free. Hard to wipe. |

In a real network, recordings grew by about 0.15 MB per hour. To switch, change
`qmdl_store_path` and restart Rayhunter, for example to RAM:

```sh
sed -i 's#^qmdl_store_path = .*#qmdl_store_path = "/tmp/rayhunter/qmdl"#' /etc/rayhunter/config.toml
/etc/init.d/rayhunter restart
```

Old recordings stay where they were. If the SD card is removed while Rayhunter
runs, recording stops and does not resume when the card is put back. After
putting it back, reboot the router or run `/etc/init.d/rayhunter restart`.

## Redacting identities and location (experimental)

This branch adds two options. Both are off by default.

* `redact_identities` replaces the IMSI, IMEI and IMEISV, TMSI, the router's
  IP address and SMS content in LTE RRC and NAS messages before a recording is
  written to the SD card or flash. Messages where that isn't possible are left
  out of the recording.
* `redact_location` does that too, and also clears the cell ID, tracking area
  and frequency, the MME group and code and the network's time zone. Messages
  that mostly describe the location, such as other system information,
  measurement reports and handovers, are left out.

Detection always runs on the messages as they come from the modem, in RAM, so
warnings appear as usual.

To turn one on, add it as the first line of `/etc/rayhunter/config.toml`:

```toml
redact_location = true
```

It has to come before any line in square brackets, such as `[analyzers]`.
Everything below such a line belongs to that section, so the setting would be
ignored there. Then run `/etc/init.d/rayhunter restart`.

Which one to use:

* With `redact_identities` only, a recording still contains the cell IDs and
  times, which show where and when the router was. Over a day that is a
  movement profile.
* With `redact_location`, a saved recording can no longer prove every warning.
  Warnings based on system information, such as a priority downgrade in SIB6
  or SIB7, are shown live but are missing when the recording is analysed again
  later.

Recordings made before an option was turned on are not changed. Delete or
overwrite them. `rayhunter-check --redact -P` writes a redacted pcap of an
existing recording.

Tested on GL firmware 4.3.30 in a lab cell: attach, tracking area update with a
new TMSI, detach and a NAS null cipher with `redact_identities`, and detach and
a NAS null cipher with `redact_location`. The null cipher was detected each
time. The recording on the SD card contained no IMSI, IMEI or TMSI, and with
`redact_location` no cell ID, tracking area or frequency. 155 lab and field
recordings were also redacted offline, with none of their identities left.

Not tested on the device yet: a tracking area update with `redact_location`,
SMS, the router's IP address, runs over several hours and a real network.

## Together with blue-merle (experimental)

[blue-merle](https://github.com/srlabs/blue-merle) changes the router's IMEI
and then powers the router off. Rayhunter can run next to it:

* On GL firmware 4.3.30 with blue-merle 3.1.5, the package installs and
  Rayhunter detects attacks as usual.
* On GL firmware 4.3.26 with blue-merle 3.0.5, Rayhunter kept running through
  an IMEI change. The recording ends when the router powers off and can still
  be read. After the next start Rayhunter starts again on its own, about
  2.5 minutes after power-on, so the first attach with the new identity is not
  recorded.

Recordings from before the change stay on the SD card. Without redaction they
contain the old IMSI and IMEI, so they can link the old and the new identity.
With `redact_identities` they contain no identities, but the cells and times
can still link them; `redact_location` removes those too. An IMEI change with
redaction turned on has not been tested yet.

## Display

A Low warning shows once on the OLED. Medium and High repeat every few seconds,
and High alternates with what to do. With `ui_level = 0` nothing is shown.

## Battery

The web UI shows the battery level and whether the charger is connected.
Low-battery notifications work as on the other devices.

## Limitations

* The GL firmware keeps the modem in LTE, so the 2G/3G downgrade heuristic does
  not fire on the Mudi V2.
* The home network is not read from the SIM, so a disconnect after an IMSI
  request is more often rated Low than High.
* The GL firmware pings every 5 s over the modem, so the modem never goes idle
  and no new Security Mode Command is seen for hours. A 60 s interval avoids
  that; it takes effect after a reboot:

  ```sh
  uci set mwan3.modem_1_1_2.interval=60
  uci set mwan3.modem_1_1_2.failure_interval=5
  uci set mwan3.modem_1_1_2.recovery_interval=5
  uci commit mwan3
  ```

* Right after a reboot the first packets can carry a timestamp from 1970 or 1980.
* Not recorded: the first attach after power-on, and the attach right after a
  modem restart (the modem attaches a few seconds before its USB ports are back).
* A modem that still answers but stopped logging is not noticed.
* Tested in a lab cell and a real network. Not tested against real IMSI catchers,
  on 2G, or over several days.

## Other links

* [GL.iNet Mudi V2 product page](https://www.gl-inet.com/products/gl-e750/)
* [GL.iNet firmware downloads](https://dl.gl-inet.com/)
