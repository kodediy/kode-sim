<div align="center" markdown="1">

<img src=".github/img/kode-dot.png" alt="The Kode Dot" width="200">

# kode-sim

**The Kode Dot simulator: kodeOS and your apps, without the board**

[![esp-emu 1.2.0](https://img.shields.io/badge/esp--emu-1.2.0-E7352C?logo=espressif&logoColor=white)](https://github.com/espressif/esp-emulator)
[![ESP-IDF v6.1](https://img.shields.io/badge/ESP--IDF-v6.1-E7352C?logo=espressif&logoColor=white)](https://github.com/espressif/esp-idf)
![Language: Rust](https://img.shields.io/badge/language-Rust-555555?logo=rust&logoColor=white)
[![Licence: GPL-3.0-or-later](https://img.shields.io/badge/licence-GPL--3.0--or--later-2ea44f)](LICENSE)

[The guide](docs/GUIDE.md) ·
[Roadmap](docs/ROADMAP.md) ·
[kode-sdk](https://github.com/kodediy/kode-sdk) ·
[kodeOS](https://github.com/kodediy/KodeOS) ·
[Discord](https://discord.gg/gZSNXRytFV)

</div>

---

kode-sim runs the Kode Dot's firmware on your computer. It builds kodeOS or any [kode-sdk](https://github.com/kodediy/kode-sdk) app, boots it in Espressif's ESP32-P4 emulator, `esp-emu`, and opens the Kode Dot in a window: the screen and touch, the six buttons, the LED, the vibration, the speaker and the microphone, the battery, the microSD, the WiFi and BLE of its ESP32-C5, the motion sensors and the USB port.

The firmware is the real one: FreeRTOS, QP/C, the Active Objects and Kode's drivers, run instruction by instruction on an emulated P4. You do not change your app to run it here: an app on kode-sdk 0.1.1 or newer builds and runs as it is.

```bash
git clone https://github.com/kodediy/kode-app-template.git
kode-sim run kode-app-template
```

<p align="center">
  <img src=".github/img/window.jpg" alt="The window: the Kode Dot alone" width="300">
  &nbsp;&nbsp;
  <img src=".github/img/settings.jpg" alt="Settings open: power, battery, microSD, motion, USB and the console" width="300">
</p>

The window shows only the Kode Dot. Click the screen to touch it, click the buttons or use the keys: <kbd>T</kbd> for the top button, <kbd>B</kbd> for the bottom one, the arrows for the D-pad. **Settings** opens the rest: the battery, the USB cable, the microSD, motion, a computer on the USB port, installing an app and the console.

## One simulator for everyone

Several people in the [Discord](https://discord.gg/gZSNXRytFV) are writing their own Kode Dot simulators. Let's build one, here. If you are working on a part of the Kode Dot (a chip, the screen, a tool), [open an issue with "I want to work on this"](https://github.com/kodediy/kode-sim/issues/new/choose) first: we will say where it fits and nobody will start the same thing twice. The [status](#status) below says what is done and what is left.

## Status

kode-sim runs on esp-emu 1.2.0, which models the Kode Dot's screen (MIPI-DSI), its microSD (SDMMC) and its ESP32-C5 (ESP-Hosted over SDIO). **A binary built for the board runs unchanged**: kodeOS's own release, `kode-sim build --board`'s, an app from apps.kode.diy. The C5 runs Kode's own C5 firmware, [kode_slave_c5](https://github.com/kodediy/kode_slave_c5), in a second esp-emu beside the P4.

What esp-emu does not have yet, `kemu` does from inside the firmware: a small component that `kode-sim build` links into the app. Today it holds only the USB port's computer, the console and the clock. When kemu is empty, `kode-sim build` and `kode-sim build --board` will be the same build.

| Part of the Kode Dot | Today | Waits for |
|---|---|---|
| ESP32-P4, 32 MB flash, 32 MB PSRAM | ✅ esp-emu | |
| Buttons, touch, LED, vibration, charger and gauge | ✅ chips modelled by kode-sim | |
| Speaker and microphone (ES8311) | ✅ esp-emu's I2S, through your computer's speaker and microphone | |
| IMU and magnetometer | ✅ posed from the window: tilt, heading, a shake, a magnet | |
| Screen (MIPI-DSI, 410×502) | ✅ esp-emu's DSI, the firmware's own `esp_lcd` | |
| microSD | ✅ esp-emu's SDMMC, a card image on your computer | taking it out while it runs restarts the Kode Dot |
| WiFi (ESP32-C5) | ✅ Kode's C5 firmware in esp-emu, over ESP-Hosted; one open network, `kode-sim` | |
| Bluetooth (ESP32-C5) | 🟡 the C5's BLE comes up and advertises; nothing on your computer connects to it yet | a BLE peer through esp-emu's `--ble-hci` |
| JPEG and PPA | 🟡 esp-emu has them; kode-sim's tests do not try them yet | |
| USB port | 🟡 kemu, with a simulated computer: its disk and its keyboard (a build for the simulator only) | a USB host in esp-emu ([esp-emulator#51](https://github.com/espressif/esp-emulator/issues/51)) |
| Infrared, the 32 kHz crystal, light sleep | ❌ | esp-emu ([esp-emulator#54](https://github.com/espressif/esp-emulator/issues/54)) |
| NFC, 125 kHz RFID | ❌ absent, as on a board without them | |
| Speed | 🟡 half to full real time on an Apple M-series Mac, slower on smaller computers: each emulator runs on one core. kodeOS stands still for a few seconds as it boots, while the P4 talks to the C5. Settings shows the speed | [esp-emulator#53](https://github.com/espressif/esp-emulator/issues/53), [esp-emulator#70](https://github.com/espressif/esp-emulator/issues/70) |

✅ works · 🟡 works, through a stand-in or with limits · ❌ not yet

**What is planned.** The USB port leaves kemu when esp-emu has a host on a socket ([esp-emulator#51](https://github.com/espressif/esp-emulator/issues/51)); then kemu is empty. The [roadmap](docs/ROADMAP.md) has the details and the open requests to Espressif.

## Install

kode-sim is made for macOS on Apple silicon. It also builds and runs firmware on Linux: tried on Debian 12, x86_64 (see [Linux](docs/GUIDE.md#linux) in the guide).

To install everything in one go (ESP-IDF, Rust, kode-sim and kodeOS) on macOS, Linux or Windows (WSL2), use the scripts in [`site/`](site/), which the download page offers.

1. **ESP-IDF v6.1**, the version kodeOS is built with, installed in `~/.espressif` with Espressif's installer.
2. **Rust**, stable, from [rustup.rs](https://rustup.rs).
3. **Google Chrome or Microsoft Edge**, for the simulator's window. Without them, the Kode Dot opens in your usual browser.
4. **kode-sim**, built from this repository:

```bash
git clone https://github.com/kodediy/kode-sim.git
cd kode-sim
cargo build --release
ln -sf "$PWD/target/release/kode-sim" ~/.local/bin/kode-sim
```

- Keep the folder where it is: kode-sim uses its `web/` and `firmware/`.
- On its first run, kode-sim installs esp-emu 1.2.0 with Espressif's installer, and builds the C5's firmware with your ESP-IDF, both in its own cache.
- On Linux, install `rsync` and `mtools` too (Debian/Ubuntu: `sudo apt install rsync mtools`): kode-sim copies the project with rsync and makes the microSD image with mtools.

## Use

```bash
kode-sim run ~/my-app                                  # an app in the window
kode-sim run --sdk ~/kode-sdk ~/kode-sdk/examples/display_lvgl   # a kode-sdk example, against that kode-sdk
kode-sim run ~/KodeOS                                  # kodeOS
kode-sim run --firmware kodeos.bin                     # a binary built for the board, as it is
kode-sim install ~/my-app                              # build an app and install it in the running kodeOS
kode-sim build ~/my-app                                # only build it
kode-sim review my-app.bin                             # boot it headless and check it, with a verdict
```

- `kode-sim run` builds the project for the simulator (your folder is never touched: it builds a copy in its cache), boots it and opens the window. Closing the window stops it.
- The terminal that runs kode-sim is the Kode Dot's console too: type `help`.
- **WiFi:** the Kode Dot sees one network, `kode-sim`, with no password. Join it as on a board; behind it is your computer's connection.

[The guide](docs/GUIDE.md) explains every command and option, the network, the microSD, installing apps, reviews, motion and the USB port.

## How it works

```
 ┌──────────────── esp-emu (ESP32-P4) ──────────────┐            ┌────────── kode-sim (Rust) ─────────┐
 │ your app / kodeOS        (unchanged)             │            │ the board's chips (kodedot-chips)  │
 │ kode.h → Active Objects → kdrv (unchanged)       │ cosim-tcp  │  TCA6408A, CST820, KTD2026,        │
 │ I2C, GPIO, I2S, MIPI-DSI ────────────────────────┼────────────┤  AW86233, BQ25896, CW2217, ES8311, │
 │ SDMMC: the microSD (a card image)                │            │  QMI8658A, QMC5883P, the AMOLED    │
 │ kemu: USB, console ──────────────────────────────┼── UART1 ───┤ bridge: the window, the console,   │
 │ SDIO (ESP-Hosted) ──┐                            │            │  the USB host                      │
 └─────────────────────┼────────────────────────────┘            └────────────────────────────────────┘
 ┌─ esp-emu (ESP32-C5) ┴────────────────────────────┐
 │ kode_slave_c5: WiFi, BLE ── the network "kode-sim" → your computer's connection
 └──────────────────────────────────────────────────┘
```

- **The chips** of the board are modelled in Rust (`crates/kodedot-chips`). They answer the P4's I2C and drive its pins through esp-emu's co-simulation bus, as the real chips answer kode-sdk's drivers.
- **The screen, the microSD and the C5** are esp-emu's: kode-sim takes the screen's frames from its MIPI-DSI link, gives it the card image, and starts a second esp-emu with Kode's C5 firmware on the other end of the SDIO link.
- **kemu** (`firmware/kemu`) stands in for the parts esp-emu does not have yet: the USB port's computer, the console and the clock. It replaces a few driver calls when the app is linked, and talks to kode-sim over a UART.
- **The window** (`web/`) is a page that kode-sim serves, connected by a WebSocket.

## Layout

| Folder | What it holds |
|---|---|
| `crates/kode-sim/` | the `kode-sim` command: build, run, install, review, and the bridge to the window |
| `crates/kodedot-chips/` | the board's chips, modelled |
| `crates/cosim/` | the client of esp-emu's co-simulation bus |
| `firmware/kemu/` | the stand-ins linked into the firmware |
| `web/` | the window: the page, the Kode Dot's picture, the audio |
| `tests/` | the host suites and the emulator suite |
| `docs/` | [the guide](docs/GUIDE.md), [the roadmap](docs/ROADMAP.md), [how a release is made](docs/RELEASING.md) |

## Contributing

1. Tell us what you want to build in an issue ([I want to work on this](https://github.com/kodediy/kode-sim/issues/new/choose)), or pick an open one.
2. Run the host suites before a pull request: `tools/host_tests.sh` (Rust, the command from outside, the page, kemu's C). The emulator suite, `pytest -m emu tests`, boots real firmware: run it when your change reaches the firmware.
3. Name the pull request in the commit format, `type(scope): Summary`, and fill in its template.

The [CHANGELOG](CHANGELOG.md) says what each release changed. Kode's [contributing guide](https://github.com/kodediy/.github/blob/main/CONTRIBUTING.md) and [code of conduct](https://github.com/kodediy/.github/blob/main/CODE_OF_CONDUCT.md) apply here.

## Licence

kode-sim is free software. Its licence is the GNU General Public License, version 3 or later. The file [LICENSE](LICENSE) holds the full text, and [NOTICE.md](NOTICE.md) says what each part is under, the trademarks, and what kode-sim downloads.
