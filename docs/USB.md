# USB device initialization and lifecycle

ESP-TinyUSB 2.2.0 and applications that own TinyUSB use the same virtual
Full-Speed controller. ESP-TinyUSB's real `tinyusb_driver_install()` validates
the configuration, sets up descriptors, creates its FreeRTOS task, initializes
TinyUSB and owns its callbacks and cleanup. No replacement stack or descriptors
are installed by kemu.

On ESP32-P4, `TINYUSB_PORT_FULL_SPEED_0` is TinyUSB root port 0 and
`TINYUSB_PORT_HIGH_SPEED_0` is root port 1. These are not the ordering of
ESP-IDF's `usb_dwc_info.controllers` array. kemu uses ESP-TinyUSB's enum when
that component is present, and TinyUSB's documented P4 convention otherwise.
The virtual PHY accepts the internal FSLS PHY in OTG device mode. A High-Speed
port, UTMI PHY, host role or unsupported speed is rejected; `skip_setup` does
not bypass the root-port guard. The simulator does not validate GPIO24/25
routing, PHY switching or electrical operation.

## Linkage

`firmware/kemu/CMakeLists.txt` discovers TinyUSB only when the application
already includes it. kemu uses that target's headers, compile options and
`tusb_config.h`, including its actual `dcd_event_t` and transfer prototypes.
The transfer wrappers cover the signatures through 0.20 and the additional
`is_isr` argument introduced in 0.21. Compile-time type declarations check the
wrapped transfer ABI. The virtual controller's transfers run in task context;
ISR-context transfers and dedicated hardware FIFO transfers are not supported.

The ESP-TinyUSB lifecycle wrappers forward to the real driver. They serialize
install/uninstall, reject High-Speed before startup and mask virtual event
production before the driver deletes its consumer task. The TinyUSB lifecycle
wrappers validate the controller before stack allocation and quiesce events
before queue destruction. They do not replace the initialization functions.

Lifecycle wrappers reside in a separate, ordinary archive, rescanned with the
stack libraries because `--wrap` references are invisible to CMake's graph.
Its ESP-TinyUSB member is extracted only for callers of that API. Direct users
keep their own descriptors, callbacks and task; merely depending on the
ESP-TinyUSB component does not extract its descriptor implementation.

TinyUSB's `usbd.c` contains weak DCD defaults called from that same object.
`--wrap=dcd_deinit` alone does not override that definition. kemu supplies
strong DCD `deinit`, `connect` and `disconnect` symbols as well as the wrappers
for external DCD calls. The integration tests inspect the final ELF symbols
and disassembly, including the call from `tud_deinit()` to the virtual DCD.
They also reject extraction of the hardware DWC2 driver.

## Event ownership

One lazily created task delivers virtual interrupts through the real
`dcd_event_handler(..., false)`. It never holds the controller's model mutex
while sending to TinyUSB's potentially blocking event queue. DCD operations
also hold the lifecycle mutex, so the consumer cannot be
deleted while it owns the model mutex. The dispatcher bypasses that mutex
for ordinary DCD callbacks and is joined by the delivery barrier; lifecycle
calls from this virtual interrupt context are rejected. A forwarding
`tud_task_ext` wrapper records the real consumer. Deleting that task with
`vTaskDelete` first quiesces delivery and excludes DCD calls, including for
direct owners that delete their task before `tusb_deinit`. Foreign task
deletion also excludes DCD critical sections, covering startup before the first
consumer call; unrelated deletions forward their original arguments without
masking USB delivery. Stack teardown from the consumer itself is rejected,
since it is still executing TinyUSB. The dispatcher,
model and lifecycle locks persist for the simulated peripheral's lifetime;
reinstalling a stack does not allocate another dispatcher or another set of
locks. Its stack size is configured by `CONFIG_KEMU_USB_EVENT_STACK_SIZE`.

Masking interrupts waits for the in-flight delivery and retains model events
for re-enabling. Teardown additionally handles a consumer already deleted or
suspended with a full queue: a forwarding-only `xQueueGenericSend` wrapper
observes the exact event pointer sent by the dispatcher. Teardown can reset
that retiring queue to unblock the sender, then waits at the delivery barrier
before TinyUSB deletes it. Other FreeRTOS sends retain their original arguments
and return values. No send is reported successful without reaching the real
queue. The queue identity is cleared at DCD teardown and on a new initialization.

The controller discards armed transfers, queued OUT buffers and old events on
reset, detach and deinit. Deinit closes EP0 too; a new initialization or bus
reset opens it again. Cable state survives stack teardown. PHY handles are
exclusive and validated; invalid or repeated deletion cannot disconnect the
active device. The host's `{"usb":{"cmd":"reset"}}` command resets and
enumerates a device without toggling its cable.

Closing or reopening an endpoint also removes that endpoint's undelivered
completion events from the controller model. Closing all non-control endpoints
retains EP0 completions; a new SETUP removes the retiring control transfer's
completions while preserving other endpoints' events.

## Reproducing the checks

Use ESP-IDF v6.1, a working kode-sdk checkout, Rust with rustfmt/clippy, the
Python dependencies in `tests/requirements.txt`, CMake, and the real esp-emu
version selected by kode-sim. `KODE_SDK` selects a local SDK;
`KODE_SIM_ESP_EMU` selects an explicitly installed emulator. The integration
tests use neither `fake_emu` nor `fake_esp_emu.py`.

```sh
tools/host_tests.sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cmake -S firmware/tests -B build_tests
cmake --build build_tests
ctest --test-dir build_tests --output-on-failure
.venv/bin/pytest -q -m emu tests/test_usb_emu.py
```

The fixture pins ESP-TinyUSB 2.2.0 and permits IDF 6.1 only. Its ordinary
resolution currently selects `espressif/tinyusb` 0.21.0~6. The direct variant
also tests 0.19.0~3. Tests read each resolved lockfile and compare its version
with the firmware's reported TinyUSB version; queue saturation uses the actual
`usbd.c` preprocessor configuration rather than a copied queue-size default.

The real firmware tests cover custom device, configuration, string and boot
keyboard report descriptors, official attach/detach callbacks, Full-Speed
selection, cable changes, deferred interrupts, bus reset, direct API use,
repeated and concurrent installation, rejected configuration, descriptor-startup
failure, `skip_setup`, interrupt masking from event hooks, full-queue consumer
shutdown, stale frames after stopping
and repeated uninstall/reinstall. Host C tests cover control, keyboard IN and
MSC-style bulk OUT transfers, endpoint validation, stalls, resets and buffer
cleanup. Existing Rust/Python suites exercise keyboard and MSC host behavior.

The real firmware suite additionally exercises shared HID plus ESP-TinyUSB's
official SPI flash MSC storage API. The storage partition is discovered through
the SDK's partition table and its logical sector size comes from wear levelling.
The host uses the size returned by READ CAPACITY throughout FAT reads, writes,
seeks and its cache, including the fixture's 4096-byte sectors. It rejects sizes
outside the FAT library's supported range instead of substituting a sector size.

Lifecycle checks call ESP-IDF's heap integrity checker and compare free heap and
FreeRTOS task counts before and after the tested transitions. Real Rust snapshots
are also passed to `web/sim.js` through the shared DOM test harness, including
repeated snapshots and a small embedded viewport. No OS input API is used.

Exact tested commits, resolved versions, command results and the separate
official/local KodeOS outcomes are recorded in [Phase 4 validation](USB_VALIDATION.md).
