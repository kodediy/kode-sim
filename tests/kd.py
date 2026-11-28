"""The KD frames, as the tests see them: what crosses the page's WebSocket.

A frame is b"KD", a type byte, a little-endian u32 length and the payload
(crates/kode-emu/src/proto.rs is the real one). The page never sees
FB_PACKED: kode-emu unpacks the pixels and hands on FB_RECT.
"""

import re
import socket
import struct

MAGIC = b"KD"
HEADER = 7
MAX_LEN = 600_000

HELLO = 0x01
FB_RECT = 0x10
PANEL = 0x11
LED = 0x20
HAPTIC = 0x21
PCM_OUT = 0x30
MIC_START = 0x31
MIC_STOP = 0x32
POWER_OFF = 0x40
TOUCH = 0x80
BUTTONS = 0x81
PCM_IN = 0x82
BATTERY = 0x83
CONSOLE_IN = 0x84
TIME = 0x88
MOTION = 0x8D
USB_IN = 0x60
USB_DEV = 0x61
USB_ACK = 0x62
USB_BUS = 0x8A
USB_SETUP = 0x8B
USB_OUT = 0x8C

BTN_TOP, BTN_BOT, BTN_LEFT, BTN_RIGHT, BTN_UP, BTN_DOWN = (1 << i for i in range(6))

# What kode-emu review counts as a crash (crates/kode-emu/src/review.rs).
CRASH = re.compile(r"task_wdt|Guru Meditation|abort\(\)|Firmware abort|assertion|Q_onError|failed assert")


def encode(type_: int, payload: bytes = b"") -> bytes:
    return MAGIC + struct.pack("<BI", type_, len(payload)) + payload


class Decoder:
    """Frames out of a byte stream, resyncing on b"KD"."""

    def __init__(self) -> None:
        self._buf = bytearray()

    def feed(self, data: bytes) -> list[tuple[int, bytes]]:
        buf = self._buf
        buf += data
        out: list[tuple[int, bytes]] = []
        while True:
            i = buf.find(MAGIC)
            if i < 0:
                del buf[: len(buf) - 1 if buf.endswith(b"K") else len(buf)]
                return out
            del buf[:i]
            if len(buf) < HEADER:
                return out
            type_, length = struct.unpack_from("<BI", buf, 2)
            if length > MAX_LEN:
                del buf[:2]
                continue
            if len(buf) < HEADER + length:
                return out
            out.append((type_, bytes(buf[HEADER:HEADER + length])))
            del buf[:HEADER + length]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
