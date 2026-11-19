# The kode-sim guide

Everything the [README](../README.md) leaves out: how the simulator is put together, every command, and each part of the Kode Dot it models.

## How it works

```
 ┌──────────────── esp-emu (ESP32-P4) ──────────────┐            ┌────────── kode-sim (Rust) ─────────┐
 │ your app / kodeOS        (unchanged)             │            │ the board's chips (kodedot-chips)  │
 │ kode.h → Active Objects → kdrv (unchanged)       │ cosim-tcp  │  TCA6408A, CST820, KTD2026,        │
 │ I2C, GPIO, I2S, MIPI-DSI ────────────────────────┼────────────┤  AW86233, BQ25896, CW2217, ES8311, │
 │ SDMMC slot 0 ── --sd card.img                    │            │  the AMOLED (CO5300)               │
 │ SDMMC slot 1 ── --hosted ──┐                     │            │                                    │
 │ kemu: USB port, console ───┼─────────────────────┼── UART1 ───┤ bridge: screen/LED/state, /ws ↔   │
 └────────────────────────────┼─────────────────────┘            │  the page, the serial port         │
 ┌─ esp-emu (ESP32-C5) ───────┴─────────────────────┐            └────────────────────────────────────┘
 │ kode_slave_c5 (Kode's C5 firmware): WiFi, BLE    │
 │ --net user: esp-emu's soft AP "kode-sim" → Mac   │
 └──────────────────────────────────────────────────┘
```

The board's chips are esp-emu's virtual parts: kode-sim answers the P4's
I2C and drives its GPIOs over esp-emu's `--cosim-tcp`, in-process, as the
real chips do for the kode-sdk drivers - the KTD2026 LED, the TCA6408A with
five buttons and the touch IRQ, the CST820 touch, the AW86233 haptic, the
BQ25896 charger and the CW2217 gauge sharing one battery, and the ES8311
codec, and the QMI8658A IMU and the QMC5883P magnetometer, which measure
what the page's "Movimiento" card poses. The codec's sound is esp-emu's I2S
too: kode-sim is the codec's end of the data pads, so what the firmware plays
reaches the Mac's speaker and the Mac's microphone reaches the firmware's
recordings, under the stock I2S driver. Chips that are not simulated (the NFC
reader) do not answer, so the drivers see them absent, as on a board without them.

The rest is esp-emu's too, since 1.1.0. The AMOLED is the panel on the P4's
MIPI-DSI link: the firmware's own `esp_lcd` DSI bus, DBI commands and DPI
panel run, and kode-sim takes the rows of each frame that changed, and the
DCS commands that light it, dim it and put it to sleep, over `--cosim-tcp`.
The microSD is esp-emu's SDMMC host with a card image in slot 0 (`--sd`).
The ESP32-C5 is a second esp-emu running Kode's own C5 firmware,
[kode_slave_c5](https://github.com/kodediy/kode_slave_c5), bridged to the
P4's SDIO slot 1 (`--hosted`): the real ESP-Hosted link, with the WiFi, the
BLE and the vault on the C5, as on the board.

What esp-emu cannot do yet - the USB-OTG port with a computer on its cable -
`kemu` does inside the firmware, linked in by `kode-sim build`
(`-Wl,--wrap=` of the few calls the drivers make), with the console and the
RTC's time, and sends to the Mac as frames over UART1. A board binary - one
built with `kode-sim build --board`, kodeOS's own release, an app from
apps.kode.diy - runs as it is: the screen, the card, the WiFi and BLE work;
only the USB port's computer is kemu's alone (see [Board images](#board-images)).

`kode-sim` never touches the project: it copies it to
`~/Library/Caches/kode-sim/<name>-<hash>/src`, adds one `include()` before the
top `project()` call, and builds there. It builds against the kode-sdk the
project pins. The copy leaves out the project's top-level `build/`,
`build_*/`, `managed_components/`, `.git`, `sdkconfig` and `sdkconfig.old`
(the same names deeper down, `main/build_info.c` say, are source and go
along), and keeps what the last build made there: its `sdkconfig`, its
`managed_components/` and its `dependencies.lock` (the project's own, when it
has one, goes in whenever it changes). Files kode-sim rewrites are written
only when their text changes, and the `sdkconfig` goes only when what it is
made from does - the project's `sdkconfig.defaults*`, the emu overlay, the
`--sdk`, `--board` - so building an unchanged project again neither
configures nor solves its dependencies again.

## Install

1. ESP-IDF **v6.1** in `~/.espressif` (the one kodeOS ships with; kode-sim
   sources `~/.espressif/tools/activate_idf_v6.1.sh`, or, without it,
   `$IDF_PATH/export.sh`; with neither, a build says what it looked for).
2. Rust (stable), and kode-sim built, with a link so any terminal finds it:
   ```
   cargo build --release
   ln -sf "$PWD/target/release/kode-sim" ~/.local/bin/kode-sim
   ```
   The binary uses the page (`web/`) and kemu (`firmware/`) of the checkout
   it was built from, so keep the checkout where it is
   (`$KODE_SIM_WEB` and `$KODE_SIM_FIRMWARE` point elsewhere).
3. `esp-emu` **1.1.0** - installed by kode-sim on its first run, with
   Espressif's installer, into `~/Library/Caches/kode-sim/esp-emu-1.1.0`
   (`$KODE_SIM_TOOLS` moves that folder; an `esp-emu` on PATH is left alone;
   `KODE_SIM_ESP_EMU` names another binary to run instead). Beside it, on the
   first run too, the C5's firmware: kode_slave_c5, fetched from GitHub and
   built with your ESP-IDF into `kode_c5-<version>` in the same folder
   (`$KODE_SIM_C5` names another image instead).

kode-sim is made for the Mac (Apple silicon). Its host suites also run on
Linux, where the cache is `$XDG_CACHE_HOME/kode-sim` (or
`~/.cache/kode-sim`) and a card image is formatted with mtools' `mformat`
(on the Mac, `diskutil` when mtools is not installed). `$KODE_SIM_CACHE` moves the cache: the builds, the
card and the notes of the simulators running.

## Use

```
kode-sim run <project>                 # build, boot, open the Kode Dot's window
kode-sim run --no-open <project>       # no window: the page stays at the address it prints
kode-sim run --sdk <kode-sdk> <example> # an example, against a local kode-sdk
kode-sim run --firmware factory-flash.bin # a flash from elsewhere; --elf is optional
kode-sim build <project>               # only build (--json: what it built, on stdout)
kode-sim build --board <app>           # built as for the board: no kemu in it
kode-sim install <app>                 # build an app and install it in the running kodeOS
kode-sim install --port 8766 <app>     # into the simulator whose page is on 8766, when two run
kode-sim install --mailbox <app>       # into app_pending, and a reset: as an Arduino upload
kode-sim run --exit-on-crash <project> # exit non-zero if the firmware crashes (scripts, CI)
kode-sim run --forward 25565 <project> # 127.0.0.1:25565 on the Mac reaches the Kode Dot's 25565
kode-sim run --no-wifi <project>       # a Kode Dot whose C5 never comes up
kode-sim run --no-internet <project>   # a network with no way out but DNS
kode-sim run --sd my-card.img <project> # another microSD (made if it does not exist)
kode-sim run --no-sd <project>         # the slot empty at start
```

Without `--exit-on-crash` a crash is printed and the page offers a restart.

**The window.** `kode-sim run` opens the Kode Dot in a window of its own:
Chrome or Edge (or another Chromium) in app mode, with a profile kept in
the cache, so its sound plays and the microphone opens without a click or a
prompt. Closing the window stops the simulator. `$KODE_SIM_BROWSER` names
another browser; with none found, the page opens as a tab of the default
browser, where the first click on the device starts the sound.

The window shows only the Kode Dot. Click the glass to touch it (drag
works), click the buttons or use the keys - <kbd>T</kbd> top,
<kbd>B</kbd> bottom, the arrows for the D-pad. **Ajustes** slides out the
rest: power and reset, the battery and the USB cable, the microSD, motion,
the computer on the USB cable, installing an app and the console.
"Micrófono del Mac" sends the Mac's microphone to the Kode Dot's; it is on
unless you turn it off, and the page remembers that. Off, the Kode Dot hears
only its own speaker through the case and its ADC's floor, as on a quiet
desk. The console takes the device's commands (`help`, `info`, `bat_info`,
…) and the terminal running `kode-sim` is the console too.

`http://127.0.0.1:PORT/?embed=1` is the same page made to live inside
another page's `<iframe>`: the device takes the room the frame gives it and
the side panel shrinks to a row (Más / Reset / Encender; the battery, the
card, the console and the rest are behind Más). While someone presses,
touches or types in it, it posts `{kodeSim: "activity"}` to its parent, at
most every 10 s, so that a page that started the simulator can stop it when
nobody has used it for a while.

## WiFi and the network

The Kode Dot's radio is its ESP32-C5, and kode-sim runs one: a second
esp-emu with Kode's C5 firmware, kode_slave_c5 at the release kode-sim pins
(built from source the first time, into kode-sim's cache: esp-emu needs its
ELF). The P4 reaches it over the SDIO link as on the board, through
esp-hosted, so everything above is the board's own: AO_Radio brings the link
up, kdrv_wifi scans and joins, the vault keeps the networks on the C5, BLE
advertises through NimBLE on the P4 and the C5's controller.

The C5 sees one network, esp-emu's soft AP: **`kode-sim`**, open (no
password), on channel 1. The C5's vault starts blank, as a new board's
does, so the first time the radio comes up with no network kode-sim joins
it for you, typing `wifi_join ssid="kode-sim"` at the console, and the C5
saves it in its vault like any other. An app built without the console
cannot be joined that way: join it from a kodeOS with `--save-state`, whose
C5 keeps the network for the runs after. Behind it is esp-emu's user-mode network: DHCP gives
192.168.4.2, the gateway 192.168.4.1 is the Mac, and the Kode Dot's sockets
reach the Mac and the internet. Inbound, only the ports `--forward` opens:
`--forward 25565` makes `127.0.0.1:25565` on the Mac reach the Kode Dot's
25565, `--forward 8080:80` the Kode Dot's 80; repeatable.

`--no-wifi` is the board whose C5 never comes up: no C5 runs, the P4's SDIO
link finds no card, and the radio fails as it does on the board.
`--no-internet` keeps the network but lets nothing out except DNS.
`--save-state` keeps the C5's flash too (its vault: the networks it joined),
in `~/Library/Caches/kode-sim/c5/flash.bin`; without it every run starts
with a C5 that has joined nothing. The C5's own console goes to
`~/Library/Caches/kode-sim/c5.log`. `$KODE_SIM_C5` names a folder with
another C5 image (`merged-binary.bin` and `kode_c5.elf`).

What the C5 does not have in esp-emu: its 32 kHz crystal (it says so at
boot and runs on its internal clock), the infrared, and the Kode cloud's
identity (a C5 that was never provisioned: "no identity in the C5").

A firmware built for the simulator boots with the clock set from the Mac's
(kemu sets the P4's RTC at every boot, as an RTC that kept its supply), so a
Kode Dot with no network is not stuck at "Set the time". A board image has
no kemu: it sets its clock as a board does, from the network.

## The microSD

The slot holds a card: a card image on the Mac, by default
`~/Library/Caches/kode-sim/sd/card.img` (1 GB, made on the first run as a
Mac formats a card - MBR, one FAT32 volume called KODEDOT; sparse, so it
takes a few MB). `--sd my-card.img` uses another, making it if it is not
there; `--no-sd` starts with the slot empty.
A card image is in one simulator at a time: a second `kode-sim run` on the
same image stops and says so (give it `--sd OTHER.img`).

The card is esp-emu's: its SDMMC host has the image in slot 0, the Kode
Dot's slot, and the firmware's own driver mounts it, 4-bit at 40 MHz, as
on the board - khal_sdmmc, FATFS, kdrv_sd, AO_Storage. esp-emu takes no
card in or out while it runs, so the page's **SD insertada** restarts the
Kode Dot with the slot as it now is, as one would with the card pulled under
it.

To look at the card from the Mac, with the simulator stopped:
`diskutil image attach --readOnly ~/Library/Caches/kode-sim/sd/card.img`
(and `diskutil eject` it afterwards). Not while it runs: two writers on one
FAT break it. Files kodeOS writes before its clock is set are dated 1980.

## Board images

A binary built for the board runs as it is: `kode-sim build --board`'s, a
KodeOS release, an app from apps.kode.diy. `kode-sim run --firmware
MERGED.bin` boots one (`--elf` is optional). A KodeOS factory pack is six
files at their offsets; merge them into one image first, as `flash.sh` would
write them:

```bash
python -m esptool --chip esp32p4 merge-bin --pad-to-size 32MB -o kodeos.bin \
  0x2000 bootloader.bin 0x8000 partition-table.bin 0x1B000 ota_data_initial.bin \
  0x20000 kodeos.bin 0xC40000 apps.bin 0x1800000 storage.bin
kode-sim run --firmware kodeos.bin
```

Known issue, in a build for the simulator too: once kodeOS has adopted an
app from the mailbox, esp-emu 1.1.0's DMA refuses the screen's frame buffer
("is not in memory", "frame underrun") and the screen is lost. Install
through the console where you can.

Without kemu in it, the console is the firmware's USB-Serial/JTAG through
esp-emu (the terminal and the page reach it a line at a time, so an install
through the console, whose chunks are raw bytes, does not go through, and
the mailbox has the known issue above), its clock
starts unset until the network sets it, and the USB port has no computer on
its cable: esp-emu's USB-OTG controller is there, but only a CDC console
answers on it, not the page's "Ordenador (USB)"
([esp-emulator#51](https://github.com/espressif/esp-emulator/issues/51)).

## Installing apps

With kodeOS running in one terminal, from another:

```
kode-sim run ~/Documents/git/KodeOS                              # terminal 1
kode-sim install --sdk ~/Documents/git/kode-sdk ~/Documents/git/kode-sdk/examples/display_lvgl   # terminal 2
```

`install` builds the app for the simulator, finds the running simulator
(`kode-sim run` leaves a note in the cache while it runs) and installs the
app into the apps region over its serial port, as kcli.py does on the board;
kodeOS reboots into it. With no simulator running it says so before building.
kodeOS takes an install only while it is on screen itself: with an app
running, go home first (hold the bottom button). `install` also takes an
app image already built (a `.bin`) instead of a project.

`--mailbox` needs no kodeOS console: the simulator reads the partition
table at 0x8000 of the flash esp-emu is running, writes the app into
app_pending through esp-emu's control port (`write-region`) and resets the
chip; kodeOS adopts the app as it boots ("adopted from the mailbox"), as on
the board. `install --mailbox --board <app>` puts the board's image into a
factory kodeOS's mailbox. An image may take all of app_pending but its last
4 KB, kodeOS's intent flag (kode-sdk's `KBSP_MAILBOX_IMAGE_MAX`, 0x5FF000).

`kode-sim run <app>` boots an app on its own, from the factory slot; an app
bigger than that slot is refused at build time - run kodeOS and `install` it
instead, as on the board.

### With kcli.py

The simulator's console is also a serial port, standing where the board's
USB-Serial/JTAG would; `kode-sim run` prints it:

```
kode-sim: its console is the serial port /dev/ttys003  (kcli.py -p /dev/ttys003 ...)
```

So KodeOS's `tools/kcli.py` works on it as on the board, payloads
included - `app-install`, `file-put`, `cmd`, `screenshot`. An app has to be
built for the simulator to run in it (one built for the board drives
hardware the emulator does not have yet); `kode-sim build` says which image
to install:

```
kode-sim build --sdk <kode-sdk> <app>          # prints .../out/<app>.bin
kcli.py -p /dev/ttys003 app-install .../out/<app>.bin
```

With no SD card in the simulator, kcli installs into the apps region and
kodeOS reboots into the app. kcli needs pyserial (any ESP-IDF Python has
it). The console reaches the firmware over UART1, not esp-emu's stdin,
which only hands a line over once its newline arrives.

Going home from an app works as on the board: hold the bottom button (the
SDK's CONFIG_KODEOS_HOME_HOLD, 600 ms), or reset - the page's **Reset**, or
BOT + PAD_LEFT held about a second (B and ← together on the keyboard), the
board's reset circuit. Any reset boots kodeOS, the app still installed.

Power works as on the board: unplug USB on the page, then `sleep`, and the
Kode Dot switches itself off (the BQ25896's BATFET opens); the bottom button
(its QON) or the page's button switches it on again - a cold boot.

## Reviewing apps: kode-sim review

For apps.kode.diy: an app put through a scenario in a Kode Dot of its own,
headless, with a verdict. It boots kodeOS from a fresh flash, installs the
app as a person would, plays the scenario and holds the bottom button to
check the app goes home.

```
kode-sim review <app>.bin [scenario.json] --out review/ --kodeos <kodeOS>
```

`--kodeos` (or `$KODE_SIM_KODEOS`) is kodeOS built for the simulator - a
folder with `merged-binary.bin` and `kodeos.elf` - or a KodeOS checkout
to build. The app is what `kode-sim build` prints (`.../out/<app>.bin`), or
its project; one built for the board is refused. The scenario, every key
optional:

```json
{
  "timeout_s": 300, "settle_s": 5, "network": "none", "wifi": true, "sd": true,
  "home_hold": true, "forbid": ["E \\(\\d+\\) mytag"],
  "steps": [
    {"expect_console": "LVGL running", "timeout_s": 30},
    {"button": "top", "hold_ms": 150},
    {"tap": [205, 250]},
    {"console": "some command"},
    {"wait": 2},
    {"screenshot": "after_top"}
  ]
}
```

Buttons are `top`, `bottom`, `left`, `right`, `up`, `down` (the pad); taps
are on the 410×502 screen. Out come `verdict.json` - `ok`, and each check
with its detail: `kodeos_boots`, `installs`, `app_starts`, each
`expect_console`, `home_by_hold`, `no_crash` (a panic, a watchdog, an abort,
a failed assert), each `forbid` - `console.log` and the screenshots
(`kodeos.png`, `app.png`, the scenario's, `home.png`, `failed.png`). The exit
code is 0 when it passed, 1 when a check failed, 2 when it could not run (not
an app for the simulator, a scenario it does not understand, an out folder
it cannot write). A SIGTERM, SIGINT or SIGHUP stops the emulator, takes the
review's temporary files (its flash and card) away and exits 128 + the
signal, with no verdict.

Every time is a number from 0 to 86400 seconds (a day); `hold_ms` is from 0
to 60000. A screenshot's name is a plain file name (up to 100 characters,
no `/`, `\` or `..`): it becomes `<name>.png` in the out folder. Button holds
and taps are timed on the firmware's clock - the emulated time the chips
hear from esp-emu - so a slow Mac does not make them shorter for the app.

`forbid` and `expect_console` patterns are searched for in each console
line, as Python's `re.search` does, with
[fancy-regex](https://docs.rs/fancy-regex): lookarounds (`(?<=temp=)\d+`),
backreferences (`(\w+) \1`, `(?P<w>\w+)-(?P=w)`), and a `{` that starts no
`{m}`, `{m,}`, `{,n}` or `{m,n}` is a `{` (`{"ok":true}` finds that JSON).
Where it is not Python's: inside a class, `[[`, `&&` and `~~` are Rust's
nested classes and set operations (write `\[`, `\&`, `\~`); `\Z` is also
before a final newline; `{2,1}` is accepted and finds nothing.

## Motion: the IMU and the magnetometer

The page's "Movimiento (IMU y brújula)" card poses the board with three
sliders (pitch, roll, heading clockwise from north), a magnet at a distance
from its back, and a shake of 1.2 s. The page works out what the chips would
measure in the board's own axes (1 g on z flat and face up; the Earth's field
at Madrid, 25 uT north and 38 uT down) and sends it as one `MOTION` frame
(0x8D: accelerometer in mg, gyroscope in mdps, magnetometer in mG, three each,
and the temperature in tenths of a degree). The QMI8658A and the QMC5883P
(`kodedot-chips`) turn that into register reads in the full scale, output rate
and mode the driver set, with the 16-bit saturation and the magnetometer's
overflow flag; the FIFO and the IMU's tap, motion and wake events are not
modelled. A board that reboots is told the pose again.

kode-app-compass (after the sweep of its calibration), kode-app-level and
kode-app-metal_detector run on it. `tests/test_page.mjs` checks the page's
maths against the compass's own formula.

## The USB cable

The dock port is a USB device controller; the apps that use it (SD Reader,
BadUSB, Air Mouse, Webcam) run the real TinyUSB over it. kemu replaces
TinyUSB's controller driver (`dcd_*`, `firmware/kemu/models/usbdcd.c`, pure
C and tested on the host) so the app's stack, descriptors and classes run
unchanged, and the "bus" is the page: `crates/kode-sim/src/usbhost.rs` is
the computer on the other end of the cable (USB_* frames in `kemu_proto.h`).

On the page, **Ordenador (USB)** plugs the cable (which also powers the
board) and pulls it out, with or without ejecting first. The computer
enumerates what answers: its descriptors, `SET_ADDRESS`,
`SET_CONFIGURATION`. Then:

- a **mass-storage** device (bulk-only transport, SCSI) is mounted as a
  computer does: capacity, write protect, the FAT volume (MBR or not) over
  `READ(10)`/`WRITE(10)`. The page lists its files, copies a file in
  (up to 700 KB, or 1 MB of random bytes to measure the speed), reads one
  back with its CRC, and ejects (`START STOP UNIT`). It shows the MB/s and
  what the computer is doing, command by command;
- a **keyboard** has its reports decoded into the text a computer would
  type, on a Spanish or a US layout.

A restart of the Kode Dot finds the cable where it was and is enumerated
again. Limits: no 64-byte packets or bus timing (an IN transfer goes out
whole, so speeds are the emulator's, not the bus's), no isochronous
endpoints yet (so the Webcam's video is not shown). ESP-TinyUSB 2.2.0 also
runs its real driver, descriptors, task and callbacks on the Full-Speed
port (`TINYUSB_PORT_FULL_SPEED_0` on ESP32-P4). The High-Speed port is
rejected explicitly, including when PHY setup is skipped. The computer's
class drivers remain mass storage and keyboard. See [USB lifecycle and
regression tests](USB.md) for the emulation boundary and validation.

## What is simulated, and what is not

| Part | State |
|---|---|
| ESP32-P4, PSRAM 32 MB, flash 32 MB | esp-emu |
| Panel 410×502 (MIPI-DSI), brightness, on/off | ✓ esp-emu's DSI host, the firmware's own `esp_lcd` |
| Touch (CST820 via the expander's P6) | ✓ |
| Six buttons (GPIO35 + TCA6408A) | ✓ |
| RGB LED (KTD2026) | ✓ |
| Haptic (AW86233, RTP) | ✓ shake on the page |
| Speaker / microphone (ES8311) | ✓ through the Mac, on esp-emu's I2S; the Mac's microphone with "Micrófono del Mac" |
| Charger + gauge, ship mode | ✓ slider and USB switch |
| Storage (FAT on flash) | ✓ within one run |
| microSD | ✓ esp-emu's SDMMC, a card image on the Mac kept across runs; "SD insertada" on the page restarts with the slot changed |
| ESP32-C5: WiFi, scans and joins, the vault | ✓ Kode's C5 firmware in a second esp-emu, over ESP-Hosted; one open network, `kode-sim`, joined for you the first time; `--forward` for inbound ports |
| ESP32-C5: BLE | ~ it comes up and advertises; nothing on the Mac connects to it yet |
| The C5's infrared, its 32 kHz crystal | ✗ |
| JPEG and PPA | ~ esp-emu's (not decoding to YUV); not yet tried by kode-sim's tests |
| Light sleep | ✗ not in esp-emu 1.1.0 |
| IMU (QMI8658A), magnetometer (QMC5883P) | ✓ the page poses the board (pitch, roll, heading), shakes it and brings a magnet near; the chips answer in the scales the driver asked for (below) |
| NFC, 125 kHz RFID | ✗ absent |
| Console (USB-Serial/JTAG) | ✓ the terminal, the page and a serial port for kcli.py |
| Wall clock (the P4's RTC) | ✓ set from the Mac's at every boot |
| USB-OTG (dock port) | ✓ a device on a cable to a simulated computer: an app's TinyUSB runs as is, the page's "Ordenador (USB)" mounts its disk or reads its keyboard (below) |
| Expansion header UARTs | ✗ UART1 is kemu's link to the Mac (below); the other HP UARTs are the apps' |

Known limitation: kemu's link to the Mac takes UART1, the only UART besides
UART0 that esp-emu can bridge out (`--uart1-tcp`; `--uart-tcp` is UART0's).
kode-sdk lends an app the HP UARTs for the expansion header in order, UART0
first and UART1 second (`KBSP_EXP_CTRL`), so an app's first expansion UART
works as on the board, but a second one is handed UART1 and its driver
install fails ("UART driver already installed") only in the simulator.
UART0 would be the worse choice: it is the first an app is lent, and the
ROM and the bootloader print on it.

With `kode-sim run`, flash does not persist between runs or power cycles:
each start is from the freshly built image, so an installed app lasts until
the next power-off. `--save-state` keeps it: `run <project> --save-state`
boots a `flash.bin` kept beside the build in the cache, made from the build
when there is none or the build changed, and esp-emu writes it back;
`run --firmware X --save-state` writes back into X.

## Adding a chip

A chip is a file in `crates/kodedot-chips/src/`: a `Chip` with its
address and `reset`/`read`/`write`, pure logic with no I/O, tested on the
Mac (`crates/kodedot-chips/tests/models.rs`). Make it answer what the
kode-sdk driver reads and add it to the `Board`; if the page has to see or
drive it, give it an `Event` and a frame type in `crates/kode-sim/src/proto.rs`.

## Tests

```
python3 -m venv .venv && .venv/bin/pip install -r tests/requirements.txt   # once
tools/host_tests.sh                    # Rust, the binary against a fake esp-emu, kemu's C (fast)
.venv/bin/pytest -m emu tests          # boots examples and kodeOS in esp-emu through kode-sim (minutes)
```

The emu suite clones kode-sdk and KodeOS (kodediy/KodeOS, its default
branch) into `.cache/` over HTTPS and brings both to their newest at every
run; `KODE_SDK` and `KODEOS` point at checkouts to test as they are.
