# Phase 4 USB validation — 2026-11-18

Repository: `Subzero710/kode-sim`, branch `feat/esp-tinyusb`.
Base: `242fa0c3d3edb5b34930daaeb24a68e9dd3f4a18`.
Final code under test: `cd18ad678bea73759d7858cacb60fe9057d8f6e9`.
A subsequent commit adds this documentation and removes one extra blank line
at the end of the shared JavaScript harness. Post-commit JavaScript and full
patch checks are recorded separately in the accompanying archive.

The firmware checks use real ESP32-P4 images and the installed Espressif
`esp-emu` 1.0.0 executable. The Python host suite identifies its stand-in
device separately. The official KodeOS build remains blocked by its
`CONFIG_KODE_LOWPOWER=n` compilation regression; a separately selected local
KodeOS checkout is used for the additional complete runtime validation.

## Resolved environment

| Component | Observed version / revision |
| --- | --- |
| ESP-IDF | v6.1 / lockfile `6.1.0`, `fff9895c82d744c7237be8847347bdd1b07c6643` |
| ESP-TinyUSB | `2.2.0` in all eight USB fixture resolutions |
| TinyUSB | `0.21.0~6`; additional direct-stack variant `0.19.0~3` |
| USB fixture's local kode-sdk | `0.1.2`, `c11bc435d88d6fe29b47aa6124282ed404c0d1eb` |
| KodeOS | `5734e0e2951492111ed3b12ddaacd762cec0933f` |
| KodeOS-pinned kode-sdk | `0.1.2`, `0a2d8e9dbc920a02372ecadbb6f2ec2ab33a03c8` |
| esp-emu | `1.0.0`, installed from Espressif's release installer |
| Rust / Cargo | `1.100.0` / `1.100.0` |
| Node | `22.23.5`, matching CI's declared major 22 |
| Python / pytest | `3.12.14` / `9.2.0` |
| CMake / Ninja | `4.4.5` / `1.13.2.git.kitware.jobserver-pipe-1` |
| Host C compiler | Debian GCC `14.2.0-19` |
| ESP compiler | `riscv32-esp-elf-gcc`, Espressif `15.2.0_20251204` |

Versions come from generated dependency locks, the actual compiler/tool
executables and the checked-out sources. The USB fixture requests registry
kode `0.1.1`; the existing explicit local-SDK build route resolves the selected
checkout as `0.1.2`. No registry or SDK header is edited to change that result.
The installed emulator's SHA-256 and source status are included in the
accompanying `environment.json`.

## Final command results

All commands below ran from the repository root. Evidence paths refer to
files in the accompanying archive. `cd18ad6` identifies the full code commit
above; `4c435ec` is `4c435ec06cb441533ff03700de503e33656cd41b`.

| Exact command | Tested commit | Observed result | Evidence |
| --- | --- | --- | --- |
| `cargo fmt --all --check` | `cd18ad6` | exit 0 | `validation/final-host-current/00.log` |
| `cargo clippy --all-targets -- -D warnings` | `cd18ad6` | exit 0 | `validation/final-host-current/01.log` |
| `cargo test --all-targets` | `cd18ad6` | 203 passed, 0 failed, 0 ignored | `validation/final-host-current/02.log` |
| `cargo build --release -p kode-sim` | `cd18ad6` | exit 0 | `validation/final-host-current/03.log` |
| `node --test tests/test_page.mjs` | `cd18ad6` | 52 passed, 0 failed, 0 skipped | `validation/final-host-current/04.log` |
| `cmake -S firmware/tests -B build_tests` | `cd18ad6` | exit 0 | `validation/final-host-current/05.log` |
| `cmake --build build_tests` | `cd18ad6` | exit 0 | `validation/final-host-current/06.log` |
| `ctest --test-dir build_tests --output-on-failure` | `cd18ad6` | 3 programs passed, 0 failed, 0 skipped | `validation/final-host-current/07.log` |
| `tools/host_tests.sh` | `cd18ad6` | 203 Rust, 119 Python host, 52 JavaScript, 3 CTest passed; 44 emu cases deselected | `validation/final-host-current/08.log` |
| `.venv/bin/pytest -vv -s tests/test_usb.py` | `cd18ad6` | 8 passed, 0 failed, 0 skipped; explicitly fake device | `validation/final-host-current/09.log` |
| `.venv/bin/pytest -vv -s -m emu tests/test_usb_emu.py` | `cd18ad6` | 18 passed, 0 failed, 0 skipped; real firmware/emulator | `validation/final-usb-current/00.log` |
| `.venv/bin/pytest -vv -s -m emu tests` | `4c435ec` | official KodeOS: 34 passed, 10 setup errors, 119 deselected, 0 skipped; exit 1 | `validation/final-emu-verified/01.log` |
| `env KODEOS=/workspace/scratch/05fc6e061240/kode-sim/.cache/KodeOS-lowpower-validation .venv/bin/pytest -vv -s -m emu tests` | `cd18ad6` | separate corrected KodeOS: 44 passed, 119 deselected, 0 failed, 0 skipped | `validation/final-emu-verified/02.log` |
| `git diff --check` | `cd18ad6` | exit 0 | `validation/final-host-current/10.log` |

The official full-suite attempt predates the final HID transfer-completion
synchronization change. Its count is reported for its exact recorded commit;
it is not reused as proof of the later code. Both the complete 44-case local
run and the isolated 18-case USB run execute the final code commit.

Additional C sanitizer commands at `cd18ad6`:

```sh
cmake -S firmware/tests -B ../validation/c-sanitized -DCMAKE_C_FLAGS="-fsanitize=address,undefined -fno-omit-frame-pointer" -DCMAKE_EXE_LINKER_FLAGS="-fsanitize=address,undefined"
cmake --build ../validation/c-sanitized
env ASAN_OPTIONS=detect_leaks=0 UBSAN_OPTIONS=halt_on_error=1 ctest --test-dir ../validation/c-sanitized --output-on-failure
```

All three commands exit 0, with 3 CTest programs passed and no sanitizer
findings. Evidence: `validation/final-asan/00.log` through `02.log`.
Leak scanning is explicitly disabled for this separate run; its unavailable
initial attempt remains recorded in `validation/c-sanitized.log`.


The host and `emu` markers select different suites: deselection is intentional
and is reported alongside the selected test count. No selected test is skipped.
The eight `test_usb.py` cases are already part of the Python host total; the
18 USB firmware cases are already part of the complete emulator total.

## Failures diagnosed and corrections

| First evidence | Diagnosis | Correction and subsequent evidence |
| --- | --- | --- |
| Parser regression: one failure for a range containing Wheel | Capability detection inspected the first usage only; unused trailing usages could also claim capabilities | Limit reachable usages by field count or array selector range and inspect supported axes within each range; Rust regression cases pass |
| Endpoint regression: four new C cases fail against the base model | Queued completions survived endpoint close/reopen, close-all or a new EP0 SETUP | Remove the affected undelivered completions while preserving other endpoints and EP0 where required; all 3 CTest programs pass |
| Pending-accent regression: one Rust failure | Closing the mouse endpoint cleared the unrelated keyboard's compose state | Retain compose state belonging to remaining HID sources; unit and real separate-interface tests pass |
| Initial real HID/MSC run: three timeouts while waiting for a mount | The official SPI-flash MSC backend exposes 4096-byte logical blocks; the host rejected every size except 512 | Use READ CAPACITY's block size throughout SCSI/FAT I/O, seek and caching; test all FAT-supported power-of-two sizes 512–4096 and reject unsupported sizes; real firmware file write/read CRC succeeds |
| Local KodeOS runtime run: 12 pass, 2 fail | A destructive console cursor discarded the app-region-ready milestone when the SDK logged it before the project-name milestone | Match both unchanged milestones within the same fresh boot journal window; both isolated regressions pass |
| Host run at `78873dd`: 117 pass, 1 startup failure | The fake emulator exposed UART before opening control/chip listeners, while the real host attaches chips after UART readiness | Open the fake service listeners before UART; a regression connects control/chips before UART and completes the real initial UART frames |
| Another diagnostic host launch reports address already in use; allocation reproduced ports `[37993, 37993, 35965]` | Three independent bind-and-close allocations can select the same ephemeral port | Reserve all three service sockets together, derive their OS-selected addresses, release immediately before spawning the emulator; no retry path or fixed port is introduced |
| USB rerun at `4c435ec`: 17 pass, 1 HID/MSC failure with genuine `ESP_FAIL` | The test sent a second report before TinyUSB completed the first transfer on their shared endpoint | Observe the real `tud_hid_report_complete_cb` before sending the next report; retain the genuine API return assertion and all Rust snapshot expectations |
| Initial firmware setup: missing IDF mbedTLS/Unity directories | ESP-IDF submodule initialization was incomplete | Complete the official recursive checkout; all USB fixture builds then execute against unchanged IDF sources |
| LeakSanitizer: 3 CTest failures with fatal unsupported-ptrace diagnostics | LeakSanitizer cannot operate in this execution environment | LeakSanitizer remains unvalidated; a separate explicit AddressSanitizer/UndefinedBehaviorSanitizer run passes with leak scanning disabled |

An early version of the new startup regression closed UART before reading its
boot frames and received `ConnectionResetError`. The regression now consumes
HELLO and FB_RECT before testing clean shutdown; the fake emulator's error
handling was not changed to ignore that reset.

## Real firmware and UI evidence

The eight ELF profiles are ESP-TinyUSB lifecycle, direct TinyUSB, direct
TinyUSB 0.19, keyboard, mouse, separate interfaces, shared interface, and shared
HID plus MSC. Actual ELF symbols and disassembly confirm the strong virtual
`dcd_deinit`, its use from `tud_deinit`, the real TinyUSB task, the queue-send
wrapper and the consumer-deletion barrier. The hardware DWC2 DCD is not linked.
The six ESP-TinyUSB profiles link the real driver/task implementation; the two
direct profiles retain their own task and descriptors. The MSC profile links
`tinyusb_msc_new_storage_spiflash` from the official component.

The four former radio declaration conflicts are absent when compiling the
real SDK headers. All four definitions have the SDK's `bool` return type and
appear as weak symbols in the compiled radio object. USB-only images discard
unused radio functions during linking; this is recorded explicitly rather
than presented as execution of those functions. The real radio fixture links
all four weak symbols in its final ELF; `radio-linkage.json` and the associated
disassembly record them. The complete emulator suite exercises the network
and C5-failure paths. The state requests return
their actual publish status; Wi-Fi start rejects a failed initial publication,
and its success return means accepted work, not a successful network connection.
Fast-mode acceptance remains the simulator's documented no-op, with no invented
network-success response.

The HID tests send genuine TinyUSB keyboard/mouse reports, including all
modifier bits, multiple keys, partial releases, US/ES layout differences,
interleaved collections and repeated relative deltas. Descriptor-derived
metadata selects interfaces, endpoints, Report IDs and malformed report lengths.
Short/long reports, unknown IDs, rollover and out-of-range axes leave prior
valid Rust state unchanged. Endpoint closure rejects further sends and preserves
unrelated interface state.

Lifecycle boundaries include cable removal/reinsertion, bus reset, pull-up
loss/recovery, stack uninstall/reinstall, endpoint close/open, firmware reboot
and three fresh simulator runs for each HID mode. ESP-IDF's actual heap
integrity check succeeds; the measured free heap does not decrease and the
FreeRTOS task count remains nine across the checked stable boundaries.
These observations cover detectable lifecycle leaks, not an unavailable
LeakSanitizer run or electrical/hardware PHY behavior.

Real Rust snapshots received over WebSockets feed the actual `web/sim.js`
through the shared DOM harness, in ordinary and small embedded viewports.
Assertions compare displayed keys, modifiers, buttons, source identities,
cumulative motion, duplicate snapshots and MSC file listings. The standalone
52-case JavaScript suite additionally covers quick edges, late clients,
reconnection and counter resets. This validates page logic in a controlled DOM;
it does not claim a graphical browser rendering test. No host input API is
invoked, and the HID cursor does not draw on the AMOLED canvas.

## KodeOS sources and remaining external block

The official KodeOS checkout and both SDK checkouts remain unchanged.
With `CONFIG_KODE_LOWPOWER=n`, `components/kgui/kgui_ao.c` uses `park_seq` and
`card_full_due` where their struct members have been omitted, and calls
`gui_card_full_say` / `gui_card_full_due_say` where their definitions have been
omitted. The first compiler errors are retained in the official-suite log.

The separately selected checkout is
`/workspace/scratch/05fc6e061240/kode-sim/.cache/KodeOS-lowpower-validation` at
the same official revision, with only `components/kgui/kgui_ao.c` modified.
Its validation-only patch moves the always-used card-full state/helpers outside
the low-power conditional and guards the conditional `park_seq` assignment.
That patch is not part of the kode-sim commits. The accompanying archive keeps
it under `validation-only/` for reproducing the explicitly separate run.

For reproduction on another machine, prepare a separate KodeOS checkout and
select its actual path with `KODEOS`; keep the ordinary official run separate:

```sh
.venv/bin/pytest -vv -s -m emu tests
KODEOS="$kodeos_validation_checkout" .venv/bin/pytest -vv -s -m emu tests
```

## CI and delivery status

The existing workflow jobs and OS matrix are unchanged. They install Node 22
and call `tools/host_tests.sh`; that script now requires the JavaScript suite
instead of treating a missing Node executable as success. The compatible host
suites pass locally with Node 22. No new GitHub Actions run or macOS execution
is claimed: workflow dispatch and file publication return HTTP 403,
`Resource not accessible by integration`.

The remote feature branch remained at the reference commit when checked.
The deliverable contains local commits as an applicable patch, the command
logs with tested commits and exit codes, resolved environment, ELF evidence
and the separately labelled KodeOS validation-only patch. Overall official
KodeOS integration remains blocked despite the separately passing local
validation; this report does not mark the unmodified official build as passing.
