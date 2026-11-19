# Descriptor-driven USB HID

The virtual computer enumerates keyboard and mouse interfaces through the
existing kemu USB controller. No keyboard or pointer input is injected into
the operating system. The web panel “Ordenador (USB)” displays the decoded
keyboard state and a mouse cursor inside the virtual computer's desktop.

## Enumeration and transport

`usbconfig.rs` validates the complete configuration against `wTotalLength`,
descriptor lengths, interface/alternate identities, endpoint counts and
endpoint ownership. It retains every alternate setting, class/subclass/
protocol, endpoint address/direction/type/packet size/interval and HID
descriptor reference. Descriptor order does not select a driver or endpoint.
The host selects the first USB configuration and its default alternate
settings, as before; it does not issue `SET_INTERFACE` or offer configuration
switching. Alternate settings remain visible for diagnostics.

There is no 512-byte configuration truncation. Configuration and HID Report
descriptor lengths use the USB control request's complete `u16` length.
Endpoint zero's packet size is learned from its first eight descriptor bytes
before requesting the full device descriptor. Oversize control packets,
inconsistent configuration headers and nonempty status stages fail explicitly.

HID drivers require an interrupt IN endpoint representable by the existing
Full-Speed controller (endpoint numbers below `KEMU_USB_EPS`, without
high-bandwidth transactions). A boot-capable HID interface receives
`SET_PROTOCOL(Report)`; failure makes that interface unavailable. No boot
report layout is substituted for an invalid or unsupported Report descriptor.
`SET_IDLE(0)` retains the existing handling of a device that stalls this
optional request.

Each active HID interface has its own Report descriptor and error state.
An unsupported USB interface, an unavailable HID interface or an unsupported
HID application does not disable other supported HID interfaces. Structurally
invalid configurations fail enumeration. Unsupported interfaces and additional
MSC volumes are identified in the interface diagnostics. The existing
bulk-only SCSI/FAT driver mounts one supported MSC interface; its bulk IN/OUT
endpoints are selected within that interface. HID reports remain independent
of the bulk queues, including while SCSI commands are running.

## Report interpretation

`hid.rs` has no GUI, USB transport or external HID-library dependency. Its
parser retains compact main-item groups and usage ranges. Report Count does
not expand into a descriptor-time allocation. Usage lookups use a compact
indexed range list. Descriptor length and per-report bit counts are bounded
by the USB `u16` transfer length, including a declared Report ID byte. Constant
padding need not fit a scalar; decoded scalar fields support 1 through 32 bits.

The parser handles short items, checks complete long-item boundaries, tracks
nested collections and the nearest Application collection, and maintains
global Push/Pop state. Logical/physical bounds, units, exponents, report sizes,
counts and IDs are retained. Input, Output and Feature each have independent
offsets for every Report ID. Local usages and usage ranges reset after every
Main item. Extended usages carry their own Usage Page. Array values select
from the declared usage list relative to Logical Minimum; variable fields
follow the declared usage sequence, including repetition of its final usage.
Capability detection uses only usages reachable through the field's Report Count
or array selector range. A supported axis within a wider Usage Minimum/Maximum
range remains visible; unused trailing usages cannot advertise a capability.

Report ID presence comes exclusively from the descriptor. IDs are scoped to
their interface. Reports must have the exact declared byte length, including
byte rounding for unaligned fields. Values are signed when Logical Minimum
is negative. Logical bounds, array selectors, unknown IDs, rollover/error
selectors and unsupported encodings fail explicitly. A report is completely
decoded and checked before any key, button, text or motion state changes.
An invalid second collection cannot commit the first collection's state.

Keyboard/Keypad usages in keyboard/keypad applications drive key arrays,
bitmaps and left/right modifiers. Text translation retains US and Spanish
layouts, accents/dead keys and backspace. Wider usages remain observable even
when the existing text translator has no character for them. Held keys are
scoped by interface, endpoint, Report ID and application collection. Modifier
and dead-key translation is confined to its application, including when that
application splits its controls across Report IDs.

Mouse applications interpret Button-page usages and Generic Desktop X, Y and
Wheel. Button numbers come from usages, including buttons beyond the first
three. Relative axes produce deltas; absolute axes retain logical values in
a separate map. No physical-unit scaling or conversion of absolute positions
into relative movement occurs. Repeated identical relative reports accumulate
again. Keyboard reports never clear mouse buttons or replay mouse deltas.

Unsupported applications, unknown usages and long items are explicitly
diagnosed. Their offsets are preserved so supported collections can still
operate. Long-item payloads have no defined HID 1.11 semantics and are not
interpreted as controls. Numeric scalar bounds are also checked in unsupported
fields. An input report containing only unsupported controls returns an
explicit unsupported-report error. Local Delimiters, Buffered Bytes, relevant
scalar fields wider than 32 bits, relative keyboard/button controls and axis
arrays are not decoded. Malformed descriptors never trigger a boot decoder.
Designator/string metadata does not affect input layouts.

## WebSocket snapshots

The existing `{"usb": ...}` envelope and keyboard/MSC fields remain. A simple
keyboard still has `stage: "keyboard"` and `device.class: "keyboard"`; its
`typed` and `layout` fields work as before. A mouse uses `stage: "mouse"`.
Keyboard and mouse together use `stage: "hid"` and `device.class: "composite"`.
A mounted MSC/HID device keeps `stage: "mounted"` so disk commands remain
available. Multiple active interfaces are announced as a composite device.

| Field | Meaning |
| --- | --- |
| `configuration` | Actual `bConfigurationValue` |
| `interfaces[]` | All interface numbers/alternates, class values, endpoints and HID metadata |
| `interfaces[].active` | Whether default alternate zero is selected |
| `interfaces[].keyboard`, `.mouse` | Capabilities discovered from Input fields and application usages |
| `interfaces[].buttons` | Sorted Button usages declared by mouse Input fields, available before the first press |
| `interfaces[].reports[]` | Declared input `report_id` and complete byte `length` |
| `interfaces[].collections[]` | Descriptor collection indices, parents, types and usages |
| `interfaces[].error`, `.diagnostics` | Driver/report errors and unsupported constructs |
| `keys_down`, `modifiers` | Sorted union of held non-modifier usages and modifier bit mask |
| `mouse.buttons` | Sorted union of active Button-page usages |
| `hid.keyboard`, `hid.mouse` | Available decoded interface capabilities |
| `hid.reports[]` | Latest states keyed by interface, endpoint, Report ID and collection |
| `hid.reports[].report_id` | Actual ID, or JSON `null` for an unnumbered report |
| `hid.reports[].keys_down`, `.modifiers` | Collection/report keyboard state; `null` for a mouse |
| `hid.reports[].mouse` | Mouse state; `null` for a keyboard |
| `mouse.relative`, `.absolute` inside a report | Maps with declared `x`, `y`, `wheel` values only |
| `mouse.total` inside a report | Checked cumulative relative movement since attachment |
| `mouse.sequence` inside a report | Checked event counter; identical nonzero motion still advances it |
| `mouse.presses`, `.releases` inside a report | Checked cumulative transition counts keyed by Button usage |

Relative values describe the latest report, not a persistent velocity. A
client must not add them again on unrelated snapshots. Cumulative totals and
sequence counters preserve motion across the existing five-per-second
publication limit. Late WebSocket clients receive the current snapshot through
the existing bridge mechanism. Repeated identical held-key/stationary-button
reports do not cause unnecessary publications.
Button press/release counters preserve quick clicks whose two edges occur
between snapshots; their JSON object keys are decimal usage numbers.

Cable removal, pull-up loss, bus reset, firmware reboot and re-enumeration clear
all HID layouts and held/button/motion/dead-key state. Endpoint closure clears
its input state; closing all endpoints clears all input state. Previously typed
text remains, preserving the existing `clear_typed` behaviour. The existing
phase-1 restart barrier drops old-boot transfers until the new boot's pull-up;
no wire protocol or firmware delivery barrier is replaced.
Closing one endpoint preserves pending accents and held state belonging to the
remaining keyboard collections.

## Web visualization (phase 3)

Keyboard and mouse sections use `hid.keyboard` and `hid.mouse`, independently
of `stage`. A composite MSC/HID device retains its file list and disk commands
alongside its keyboard and mouse. Interface numbers, interrupt IN endpoints,
Report IDs and collection identities are displayed without selecting a
preferred interface. Keyboard usages and modifier state come from the decoded
Rust snapshot; the page does not translate usages into text. `typed` and
`layout` always follow the server, including while HID is unavailable.

Each mouse source has its own previous sequence, cumulative totals and button
transition counters, keyed by the complete interface/endpoint/Report
ID/collection tuple. The first snapshot after opening a WebSocket establishes
a baseline and shows current held keys and buttons immediately. It does not
animate historical motion or count historical clicks as new. Sources that
first appear in later snapshots contribute their new cumulative motion and
transitions from zero. Repeated snapshots do not add movement or transitions.
The latest relative values remain visible as diagnostics; only differences
in cumulative relative totals move the cursor or advance the wheel display.
Absolute values are displayed separately and never converted into deltas.

Button badges distinguish the current union of held buttons from newly
observed press and release counts. Both edges of a click remain visible when
the button is already released at publication time. Per-source readouts show
the received sequence and totals and describe accumulated new transitions
without inventing their order. Declared buttons are shown even before input
arrives; unknown Button usages keep their numeric labels.

The cursor's bounds are the desktop's current DOM client dimensions minus the
cursor's measured size. A `ResizeObserver` clamps the position after layout
changes, including small screens and expanded embed controls. Movement
received while the desktop has no layout is retained until it can be shown.
The cursor is a graphical element inside the USB panel and never draws on the
AMOLED canvas or invokes host input, Pointer Lock or WebHID APIs.

Snapshots that remove HID capabilities clear transient state. Removing a
source removes its local counters; removing all mouse sources clears the
virtual mouse session. Counter decreases establish a new baseline without
replaying motion or transitions. Endpoint closure publishes its empty source
state immediately, so a close/reopen within the publication interval cannot
hide a counter reset behind higher replacement counters. Firmware state
changes, HELLO, POWER_OFF and WebSocket closure also clear held states,
position and local counters. None of these local resets clears `typed`.

`tests/test_page.mjs` exercises these DOM behaviours with the WebSocket JSON
structure used by `Host::snapshot`, including composite devices, sparse
buttons, all four source identity components, quick clicks, late clients,
reconnection and layout changes. The parser's button metadata and immediate
endpoint-closure publication have Rust regression tests.

## Tests and reproduction

The pure parser tests cover standard/numbered keyboards, arrays, bitmaps,
modifiers, signed 8/16/32-bit axes, unaligned/reordered fields, buttons, wheel,
absolute values, nested/multiple collections, independent report kinds/IDs,
Push/Pop, padding, null selectors, unsupported usages/encodings and malformed
or truncated input. Exact descriptors emitted by the resolved TinyUSB macros
are also checked; see `tests/fixtures/hid/README.md`.

Host tests exercise independent/nontrivial interfaces and endpoints, shared
IDs, interleaved keyboard/mouse reports, text layouts, MSC coexistence,
configurations beyond 512 bytes, transactional rejection, endpoint closure,
disconnect/reset/reboot and JSON compatibility. `tests/test_usb.py` checks the
existing keyboard and all four descriptor architectures over real WebSockets
against the explicitly fake host-test device.

The `usb_hid_firmware` fixture in `tests/test_usb_emu.py` builds keyboard,
mouse, separate-interface, shared-interface and shared-HID/MSC firmware with ESP-IDF 6.1,
ESP-TinyUSB 2.2.0 and the actual resolved TinyUSB component. It uses the same
local-SDK build route as phase 1 and never uses `fake_emu`. Reports are sent
through `tud_hid_n_keyboard_report` / `tud_hid_n_mouse_report`, the real DCD,
Rust host and WebSockets. Tests require held/released states, interleaving,
repeated deltas, late clients, cable reconnection, bus reset, device
disconnect/reconnect, uninstall/reinstall and firmware reboot. The three
original lifecycle fixture variants remain.

Additional real-firmware checks cover all modifier bits, simultaneous keys,
partial release, US/ES punctuation, invalid report lengths, unknown IDs,
rollover, mouse bounds and button edges, endpoint close/reopen, WebSocket
reconnection and repeated simulator shutdown/startup. MSC uses the official
ESP-TinyUSB SPI flash backend, with FAT writes read back and checked by CRC.
`tests/hid_page.mjs` feeds the resulting Rust snapshots to the same page harness
as the standalone JavaScript suite; it checks displayed state, cumulative motion,
duplicate snapshots and the embedded viewport. The device remains the real
`esp-emu`, independently of this DOM harness.

Heap integrity, free bytes and task counts are checked during the real USB
lifecycle tests. The Python host suite continues to identify its fake device
explicitly. Node is required by `tools/host_tests.sh`; its absence fails the
command instead of reporting a successful run with the page tests omitted.

Run from the repository root:

```sh
node --test tests/test_page.mjs
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release -p kode-sim
tools/host_tests.sh
cmake -S firmware/tests -B build_tests
cmake --build build_tests
ctest --test-dir build_tests --output-on-failure
.venv/bin/pytest -vv -s tests/test_usb.py
.venv/bin/pytest -vv -s -m emu tests/test_usb_emu.py
.venv/bin/pytest -vv -s -m emu tests
git diff --check
```

### Current validation

The former `kemu_radio.c` return-type compilation blocker is resolved: the
fixtures build against the actual SDK's `bool` declarations. The real USB
suite exercises ESP-TinyUSB 2.2.0, resolved TinyUSB 0.21.0~6, direct TinyUSB and
the explicitly pinned 0.19.0~3 variant. The four HID architectures and the
additional HID/MSC configuration execute in the real emulator.

Unmodified KodeOS still fails to compile with `CONFIG_KODE_LOWPOWER=n` because
`kgui_ao.c` uses conditionally omitted fields and functions. Results for that
official checkout must remain separate from a locally corrected checkout
selected by `KODEOS`. Firmware console milestones are checked within the new
boot's journal window without imposing an order on independent SDK messages.

The phase-2/3 validation table from the parent history is historical evidence
only. It does not validate the current source tree.

See [Phase 4 validation](USB_VALIDATION.md) for the dated command results,
tested commits, resolved versions, initial failures and corrections, ELF
evidence, and the separately recorded official/local KodeOS outcomes.
