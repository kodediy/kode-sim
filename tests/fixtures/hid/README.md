# HID descriptor inputs

`keyboard.hex` is a standard eight-byte keyboard layout, with modifiers,
constant reserved byte, LED output and a six-selector key array.
`mouse.hex` uses the TinyUSB mouse layout's five buttons, padding, X/Y and
vertical wheel, without its optional Consumer-page horizontal pan field.
These are valid descriptors; report lengths in the test devices are derived
from the descriptor bytes.

`tinyusb_keyboard.hex` and `tinyusb_mouse.hex` are the exact arrays emitted
by `TUD_HID_REPORT_DESC_KEYBOARD()` and `TUD_HID_REPORT_DESC_MOUSE()` in the
real fixture's `usb_main.c` object, compiled against the component manager's
resolved `espressif/tinyusb` **0.21.0~6**, through ESP-TinyUSB **2.2.0** and
ESP-IDF **v6.1**. They contain 65 and 77 bytes respectively. The latter
includes horizontal pan: tests require an explicit unsupported-usage
diagnostic while decoding the supported buttons and vertical motion.
The real emulator fixtures continue to use the actual TinyUSB macros;
these text inputs do not replace the firmware or its USB transport.

Sources: [TinyUSB HID descriptor macros](https://github.com/hathach/tinyusb/blob/master/src/class/hid/hid_device.h),
[HID 1.11](https://www.usb.org/sites/default/files/documents/hid1_11.pdf),
[HID Usage Tables](https://usb.org/hid).
