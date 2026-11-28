# Roadmap

The aim: **the same binary runs on the board and in the simulator** - the
official kodeOS, the apps from apps.kode.diy, an Arduino upload through the
mailbox - with nothing linked in for the simulator. Each part that
Espressif's esp-emu gains takes its piece out of kemu (`firmware/kemu`);
when kemu is empty, `kode-sim build --board` and `kode-sim build` are the
same build.

Since esp-emu 1.1.0 a board binary runs unchanged: kodeOS v0.1.1's factory
pack boots in kode-sim with its screen, its microSD, and its C5's WiFi and
BLE. What kemu still adds to a build for the simulator is the USB port's
computer, the console over UART1 and the RTC's time.

## Where each part stands (on esp-emu 1.3.0)

| Part of the Kode Dot | Today | Waits for |
|---|---|---|
| ESP32-P4, flash, PSRAM | esp-emu | - |
| I2C chips, buttons, IRQs | esp-emu's virtual parts, answered by kode-sim | - |
| MIPI-DSI panel (410×502) | esp-emu (1.1.0); kode-sim is the panel on DSI virtual channel 0 over `--cosim-tcp`: the rows that changed, and DCS for lit, asleep and brightness. A read of the panel is answered with zeros. Since 1.2.0 the screen survives a flash write, so an app adopted from the mailbox keeps it ([esp-emulator#64](https://github.com/espressif/esp-emulator/issues/64)) | - |
| JPEG and PPA | esp-emu (1.1.0); no JPEG decode to YUV yet | - |
| I2S (speaker, microphone) | esp-emu; kode-sim is the ES8311's audio end. Since 1.1.0 the word clock's edges reach only a consumer that watches GPIO 37 ([esp-emulator#52](https://github.com/espressif/esp-emulator/issues/52)), so kemu no longer keeps it off its pad | - |
| microSD (SDMMC slot 0) | esp-emu (1.1.0, `--sd`); no card in or out while it runs, so the page's switch restarts the Kode Dot | removing a card at run time |
| ESP32-C5 over SDIO (WiFi, BLE) | esp-emu (1.1.0): Kode's kode_slave_c5 in a second esp-emu, `--hosted`; esp-emu's open soft AP "kode-sim" | - |
| The C5's infrared and 32 kHz crystal; light sleep | missing in esp-emu | [esp-emulator#54](https://github.com/espressif/esp-emulator/issues/54) |
| esp-emu's source and terms | - | [esp-emulator#10](https://github.com/espressif/esp-emulator/issues/10) |
| IMU, magnetometer | `kodedot-chips` (QMI8658A, QMC5883P) posed from the page (`MOTION`); no FIFO or IMU events | - |
| USB-OTG (dock port) | kemu (`kemu_usb.c`, `models/usbdcd.c`) and a host in kode-sim: mass storage and keyboard; no isochronous (UVC) yet. esp-emu has the controller, with a host that only takes a CDC console, so a board image's port has no computer on it | a host on a socket - [esp-emulator#51](https://github.com/espressif/esp-emulator/issues/51) |
| Console, RTC time | kemu over UART1 in a build for the simulator; a board image's console is its USB-Serial/JTAG through esp-emu's stdin and stdout | kemu's link goes with the USB port |
| NFC, RFID | absent, as on a board without them | not asked for: nothing to see on a desk |

## Next releases

- **0.3.0 - the board's own binaries.** The screen, the microSD and the C5
  leave kemu on esp-emu 1.1.0.
- **0.3.1 - esp-emu 1.2.0**, the window in English and its speed readout.
- **0.3.2 - esp-emu 1.3.0** (this release): no stall at the boot, and
  faster.
- **1.0.0 - no kemu.** When esp-emu has a USB host on a socket, the USB
  port's computer leaves kemu, and the console and the clock with it.
  `--board` is the only build.

Alongside these, whenever they are ready:

- **The web.** Today kode-sim runs on your own computer. A hosted
  simulator, for apps.kode.diy, will be designed again when it is needed.
- **Speed.** Each esp-emu runs on one core: about real time on an Apple
  M-series Mac, a third of it on a small x86 server - [esp-emulator#53](https://github.com/espressif/esp-emulator/issues/53).
  The C5 runs in its own process, on a core of its own.
  The stall while the P4 talked to the C5 is gone in esp-emu 1.3.0
  ([esp-emulator#70](https://github.com/espressif/esp-emulator/issues/70)).
- **`--cosim-wait N`** (to propose to Espressif): hold the chip in reset
  until N devices have registered, which saves the double boot at start.
