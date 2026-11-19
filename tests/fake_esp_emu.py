"""A stand-in for esp-emu: the same flags, the same UART1 socket, a console.

On connect it says HELLO and paints a 2x2 white square at (0,0). Console
input arrives as CONSOLE_IN frames, as kemu takes it: "crash" dies like a
panic, "spam N" prints N long lines, "reboot" boots again as a panic's reboot does, "cut N" sends the
start of an FB_PACKED that promises N bytes and never ends (what a reset
leaves on the wire), "poweroff" sends POWER_OFF, "flood N" sends N whole-screen FB_RECTs,
"count N" takes the next N raw bytes and reports their CRC, "ping" and
'install src="terminal" dst=app size= crc32= run= [name=]' answer as KodeOS does (the
ready behind a prompt and its erase, an ack per chunk, the CRC checked;
FAKE_INSTALL_REFUSE makes it refuse, $FAKE_INSTALL_SLOW seconds of erase
before the ready), "key MODS KEY" (decimal) sends a keyboard report, anything else is echoed.
The USB cable has a keyboard on it: it raises its pull-up when the cable comes in and
answers the host's SETUP packets as a boot keyboard's TinyUSB would. Every other frame from the Mac is printed as "rx <type> <len>".
Like kodeOS, every boot prints its "Project name:" (kodeos); an
install with run=on reboots into the app ("Project name: app", or
$FAKE_APP_NAME), whose console echoes; BOT held 0.6 s in the app goes home
to kodeos, as the SDK's CONFIG_KODEOS_HOME_HOLD does.
--cosim-tcp takes the board's chips as esp-emu 1.0 does: it answers a
Register with a RegisterAck (esp32p4, accepted), prints each pin driven as
"gpio PIN LEVEL", and when the expander's INT (GPIO 6) goes low it reads
the expander's input register over I2C, as kodeOS does, to see BOT. The
console command "i2c ADDR REG N" (hex) reads a chip and prints
"i2c ADDR REG: HEX" or "i2c ADDR REG: nack"; "i2cw ADDR REG BYTE..." (hex)
writes one and prints "i2cw ADDR REG: ok" or "... nack". The chips' I2S
claim prints "fake: i2s dout D din I"; "play N" sends them the transmit
format (48 kHz, 2 slots, 16 bits) and N frames of 1000 left, -1 right;
"rxformat RATE CH BITS" sends the receive format and from then a heartbeat
every 10 ms; "i2sstat" prints "i2s_in frames F peak P", what the chips fed
the receiver and its left slot's loudest sample.
--control-tcp answers "ping" and "reset" with "ok"; a reset prints
"fake: reset" and says HELLO again, as a firmware booting would. Its
"read-region OFFSET LENGTH FILE" reads the flash's partition table (the
--firmware file's, or kode-sdk's with app_pending at 0x1A00000; none with
$FAKE_NO_MAILBOX) and "write-region OFFSET FILE" writes the flash, printed
as "fake: write-region OFFSET LEN"; anything else is "err: ...", as are
write-region with $FAKE_WRITE_REGION_ERR and reset with $FAKE_RESET_ERR;
write-region takes $FAKE_WRITE_REGION_SLOW seconds. Like
kodeOS, a boot that finds an app image in app_pending adopts it ("... adopted
from the mailbox") and boots it.
With --chip esp32c5 it is the C5 instead: it listens on the socket its
--hosted bridge:slave:PATH names (as esp-emu's slave does, for the P4 to
connect to), prints "fake c5: up" and waits to be stopped; its launches
are lines in $FAKE_LAUNCH_LOG.c5.
With $FAKE_BOARD_IMAGE it is a firmware built for the board: no kemu, so no
HELLO and nothing on UART1; each line on its stdin (the USB-Serial/JTAG
console) is printed back as "stdin: LINE".
Each launch appends a line to $FAKE_LAUNCH_LOG. It exits once UART1's
client has gone, unless $FAKE_OUTLIVE_UART (then it runs on, as esp-emu
does, until it is stopped).
"""

import asyncio
import os
import struct
import sys
import zlib
from pathlib import Path

import kd


async def c5(args) -> int:
    """The C5's esp-emu: its SDIO socket open, nothing on it."""
    path = args[args.index("--hosted") + 1].removeprefix("bridge:slave:")
    server = await asyncio.start_unix_server(lambda r, w: None, path)
    print("fake c5: up", flush=True)
    async with server:
        await asyncio.sleep(3600)
    return 0


async def main() -> int:
    args = sys.argv[1:]
    c5_too = args[args.index("--chip") + 1] == "esp32c5"
    if os.environ.get("FAKE_LAUNCH_LOG"):
        with open(os.environ["FAKE_LAUNCH_LOG"] + (".c5" if c5_too else ""), "a") as f:
            f.write(" ".join(args) + "\n")
    if c5_too:
        return await c5(args)
    host, port = args[args.index("--uart1-tcp") + 1].rsplit(":", 1)
    conn: asyncio.Future = asyncio.get_running_loop().create_future()

    async def on_client(reader, writer):
        if not conn.done():
            conn.set_result((reader, writer))

    mode = ["kodeos"]
    app_name = os.environ.get("FAKE_APP_NAME", "app")

    # The flash, as far as the mailbox goes: the partition table at 0x8000
    # (the --firmware file's, when it has one; else kode-sdk's, its
    # app_pending at 0x1A00000, or none with FAKE_NO_MAILBOX) and what
    # write-region has put in it since.
    firmware = args[args.index("--firmware") + 1] if "--firmware" in args else ""
    table = b""
    if os.path.exists(firmware) and os.path.getsize(firmware) >= 0x8000 + 0xC00:
        with open(firmware, "rb") as f:
            f.seek(0x8000)
            table = f.read(0xC00)
    if not table.startswith(b"\xaa\x50"):
        parts = [(b"nvs", 1, 0x02, 0xB000, 0x10000), (b"factory", 0, 0x00, 0x20000, 0x400000)]
        if not os.environ.get("FAKE_NO_MAILBOX"):
            parts.append((b"app_pending", 1, 0x41, 0x1A00000, 0x600000))
        table = b"".join(struct.pack("<2sBBII16sI", b"\xaa\x50", t, st, off, size, name, 0)
                         for name, t, st, off, size in parts).ljust(0xC00, b"\xff")
    written: dict[int, bytes] = {}

    def flash(at: int, n: int) -> bytes:
        out = bytearray(b"\xff" * n)
        for off, data in [(0x8000, table), *written.items()]:
            lo, hi = max(at, off), min(at + n, off + len(data))
            if lo < hi:
                out[lo - at:hi - at] = data[lo - off:hi - off]
        return bytes(out)

    def mailbox() -> int | None:
        for at in range(0, len(table) - 31, 32):
            magic, _t, _st, off, _size, name = struct.unpack_from("<2sBBII16s", table, at)
            if magic != b"\xaa\x50":
                break
            if name.rstrip(b"\x00") == b"app_pending":
                return off
        return None

    async def boot():
        _, boot_writer = await conn
        # kodeOS, as it boots, adopts an app it finds in the mailbox.
        box = mailbox()
        if mode[0] == "kodeos" and box is not None and written.get(box, b"")[:1] == b"\xe9":
            del written[box]
            print(f"I (629) app_init: Project name:     {mode[0]}", flush=True)
            print(f"!N 4431 kaos.appmgr dev app '{app_name}' v1 adopted from the mailbox", flush=True)
            mode[0] = app_name
        print(f"I (629) app_init: Project name:     {mode[0]}", flush=True)
        if os.environ.get("FAKE_BOARD_IMAGE"):
            return
        _, boot_writer = await conn
        boot_writer.write(kd.encode(kd.HELLO, struct.pack("<HHH", 1, 410, 502)))
        boot_writer.write(kd.encode(kd.FB_RECT, struct.pack("<HHHH", 0, 0, 2, 2) + b"\xff\xff" * 4))
        await boot_writer.drain()

    if os.environ.get("FAKE_BOARD_IMAGE"):
        async def board_console():
            # A board image's console is its USB-Serial/JTAG: esp-emu's stdin.
            while line := await asyncio.to_thread(sys.stdin.readline):
                print(f"stdin: {line.rstrip()}", flush=True)
        asyncio.get_running_loop().create_task(board_console())

    if "--control-tcp" in args:
        chost, cport = args[args.index("--control-tcp") + 1].rsplit(":", 1)

        async def on_control(creader, cwriter):
            while line := await creader.readline():
                cmd, *rest = line.decode().split()
                answer = "ok"
                if cmd == "read-region":                 # OFFSET LENGTH FILE
                    with open(rest[2], "wb") as f:
                        f.write(flash(int(rest[0], 0), int(rest[1], 0)))
                elif cmd == "write-region" and os.environ.get("FAKE_WRITE_REGION_ERR"):
                    answer = "err: write-region: the flash would not take it"
                elif cmd == "reset" and os.environ.get("FAKE_RESET_ERR"):
                    answer = "err: reset: not now"
                elif cmd == "write-region":              # OFFSET FILE
                    await asyncio.sleep(float(os.environ.get("FAKE_WRITE_REGION_SLOW", "0")))
                    with open(rest[1], "rb") as f:
                        data = f.read()
                    written[int(rest[0], 0)] = data
                    print(f"fake: write-region 0x{int(rest[0], 0):x} {len(data)}", flush=True)
                elif cmd not in ("ping", "reset"):
                    answer = f"err: unknown command {cmd}"
                cwriter.write(f"{answer}\n".encode())
                await cwriter.drain()
                if cmd == "reset" and answer == "ok":
                    print("fake: reset", flush=True)
                    await boot()

        await asyncio.start_server(on_control, chost, int(cport))

    console: asyncio.Queue = asyncio.Queue()

    bot_down = [None]
    loop = asyncio.get_running_loop()

    async def go(to):
        mode[0] = to
        await boot()

    # ---- --cosim-tcp: the board's chips, beside the chip -------------------
    chips = {"writer": None, "next": 1, "waiting": {}}
    t0 = loop.time()
    i2s = {"rx": None, "frames": 0, "peak": 0}

    def sim_ns():
        return int((loop.time() - t0) * 1e9)

    def vint(n):
        out = b""
        while n > 0x7F:
            out += bytes([(n & 0x7F) | 0x80])
            n >>= 7
        return out + bytes([n])

    def vf(num, v):
        return vint(num << 3) + vint(int(v))

    def ld(num, inner):
        return vint((num << 3) | 2) + vint(len(inner)) + inner

    def fields(buf):
        return dict(repeated_all(buf))

    def repeated(buf, num):
        return [v for n, v in repeated_all(buf) if n == num]

    def repeated_all(buf):
        out, i = [], 0
        while i < len(buf):
            tag = 0
            shift = 0
            while True:
                b = buf[i]; i += 1
                tag |= (b & 0x7F) << shift; shift += 7
                if not b & 0x80:
                    break
            num, wire = tag >> 3, tag & 7
            if wire == 0:
                v = 0; shift = 0
                while True:
                    b = buf[i]; i += 1
                    v |= (b & 0x7F) << shift; shift += 7
                    if not b & 0x80:
                        break
            else:
                n = 0; shift = 0
                while True:
                    b = buf[i]; i += 1
                    n |= (b & 0x7F) << shift; shift += 7
                    if not b & 0x80:
                        break
                v, i = buf[i:i + n], i + n
            out.append((num, v))
        return out

    def to_chips(msg):
        topic = b"@tcp:1"
        frame = struct.pack("<H", len(topic)) + topic + msg
        chips["writer"].write(struct.pack("<I", len(frame)) + frame)

    async def i2c(addr, write, n):
        """One transaction on the board's bus, answered by the chips."""
        if chips["writer"] is None:
            return None
        cid = chips["next"]
        chips["next"] += 1
        done = loop.create_future()
        chips["waiting"][cid] = done
        t = vf(2, addr) + ld(3, bytes(write)) + vf(4, n)
        to_chips(vf(1, int(loop.time() * 1e9)) + ld(7, vf(1, cid) + ld(2, t)))
        await chips["writer"].drain()
        ack, data = await asyncio.wait_for(done, 5)
        return data if ack else None

    async def expander_int():
        data = await i2c(0x20, [0x00], 1)
        if data is None:
            return
        pressed = not data[0] & 0x01            # BOT, P0, active low
        if pressed and bot_down[0] is None:
            bot_down[0] = loop.time()
        elif not pressed and bot_down[0] is not None:
            held, bot_down[0] = loop.time() - bot_down[0], None
            if held >= 0.6 and mode[0] != "kodeos":
                await go("kodeos")

    async def on_chips(creader, cwriter):
        chips["writer"] = cwriter
        while True:
            head = await creader.readexactly(4) if True else b""
            msg = await creader.readexactly(struct.unpack("<I", head)[0])
            m = fields(msg)
            if 4 in m:                                    # Register
                print("fake: chips registered", flush=True)
                for c in repeated(m[4], 3):               # Register.claims
                    c = fields(c)
                    if 17 in c:                           # Claim.i2s
                        p = fields(c[17])
                        print(f"fake: i2s dout {p.get(3)} din {p.get(4)}", flush=True)
                ack = vf(1, 1) + ld(2, b"esp32p4") + vf(3, 55) + vf(5, 1)
                to_chips(ld(5, ack))
                await cwriter.drain()
            elif 6 in m:                                  # GpioDrive
                g = fields(m[6])
                pin, level = g.get(1, 0), g.get(2, 0)
                print(f"gpio {pin} {level}", flush=True)
                if pin == 6 and level == 0:
                    loop.create_task(expander_int())
            elif 31 in m:                                 # I2sIn
                pcm = bytes(fields(m[31]).get(1, b""))
                rate, ch, bits = i2s["rx"] or (48000, 2, 16)
                frame = ch * bits // 8
                for k in range(0, len(pcm) - frame + 1, frame):
                    v = int.from_bytes(pcm[k + frame // ch - 2:k + frame // ch], "little", signed=True)
                    i2s["peak"] = max(i2s["peak"], abs(v))
                    i2s["frames"] += 1
            elif 8 in m:                                  # BusReply
                r = fields(m[8])
                fut = chips["waiting"].pop(r.get(1, 0), None)
                if fut is not None and not fut.done():
                    fut.set_result((bool(r.get(2, 0)), bytes(r.get(3, b""))))

    async def serve_chips(creader, cwriter):
        try:
            await on_chips(creader, cwriter)
        except (asyncio.IncompleteReadError, ConnectionError):
            chips["writer"] = None

    if "--cosim-tcp" in args:
        khost, kport = args[args.index("--cosim-tcp") + 1].rsplit(":", 1)
        await asyncio.start_server(serve_chips, khost, int(kport))

    # UART availability is the simulator's startup boundary: the chip and
    # control listeners must already exist when it accepts that connection.
    await asyncio.sleep(float(os.environ.get("FAKE_SLOW_START", "0")))
    server = await asyncio.start_server(on_client, host, int(port))
    print("fake: up", flush=True)
    if os.environ.get("FAKE_CRASH_AFTER"):
        async def crash_later():
            await asyncio.sleep(float(os.environ["FAKE_CRASH_AFTER"]))
            print("Guru Meditation Error: Core 0 panic'ed", flush=True)
            os._exit(1)
        asyncio.create_task(crash_later())
    reader, writer = await conn
    await boot()

    descriptors = Path(__file__).parent / "fixtures" / "hid"
    keyboard_report = bytes.fromhex((descriptors / "keyboard.hex").read_text())
    mouse_report = bytes.fromhex((descriptors / "mouse.hex").read_text())
    usb_mode = os.environ.get("FAKE_USB_MODE", "keyboard")
    if usb_mode == "keyboard":
        hid_interfaces = [(0, 0x81, keyboard_report, {"key": None}, 1, 1)]
    elif usb_mode == "mouse":
        hid_interfaces = [(3, 0x85, mouse_report, {"mouse": None}, 0, 0)]
    elif usb_mode == "separate":
        hid_interfaces = [(7, 0x86, mouse_report, {"mouse": None}, 0, 0),
                          (4, 0x83, keyboard_report, {"key": None}, 0, 0)]
    elif usb_mode == "shared":
        report_ids = {"key": 37, "mouse": 193}
        report = bytes([0x85, report_ids["key"]]) + keyboard_report + bytes([0x85, report_ids["mouse"]]) + mouse_report
        hid_interfaces = [(6, 0x84, report, report_ids, 0, 0)]
    else:
        raise ValueError(f"unknown fake USB mode: {usb_mode}")
    config = bytearray([9, 2, 0, 0, len(hid_interfaces), 1, 0, 0x80, 50])
    report_descriptors = {}
    report_sources = {}
    for number, endpoint, report, ids, subclass, protocol in hid_interfaces:
        config.extend([9, 4, number, 0, 1, 3, subclass, protocol, 0])
        config.extend(bytes([9, 0x21, 0x11, 1, 0, 1, 0x22]) + len(report).to_bytes(2, "little"))
        config.extend([7, 5, endpoint, 3, 64, 0, 10])
        report_descriptors[number] = report
        for name, report_id in ids.items():
            report_sources[name] = endpoint, report_id
    config[2:4] = len(config).to_bytes(2, "little")
    kbd_config = bytes(config)
    kbd_device = bytes([18, 1, 0, 2, 0, 0, 0, 64, 0x6A, 0x30, 0x35, 0x12, 0, 1, 1, 2, 0, 1])

    def usb_in(ep, data):
        writer.write(kd.encode(kd.USB_IN, bytes([ep]) + data))

    def usb_setup(s):
        """What TinyUSB answers a host's control request with, for a keyboard."""
        kind, req, _, vtype, _, _, wl = s[0], s[1], s[2], s[3], s[4], s[5], int.from_bytes(s[6:8], "little")
        if (kind, req) == (0x80, 6):
            d = {1: kbd_device, 2: kbd_config,
                 3: bytes([2 + 2 * len("Fake"), 3]) + "Fake".encode("utf-16-le")}.get(vtype)
            if d is None:
                writer.write(kd.encode(kd.USB_DEV, bytes([2, 0x80, 0, 0])))        # stall
                return
            d = d[:wl]
            for i in range(0, len(d), 64):
                usb_in(0x80, d[i:i + 64])
            if len(d) % 64 == 0 and len(d) < wl:
                usb_in(0x80, b"")
        elif kind == 0x81 and req == 6 and vtype == 0x22 and s[4] in report_descriptors:
            d = report_descriptors[s[4]][:wl]
            for i in range(0, len(d), 64):
                usb_in(0x80, d[i:i + 64])
            if len(d) % 64 == 0 and len(d) < wl:
                usb_in(0x80, b"")
        elif kind in (0x00, 0x21):
            usb_in(0x80, b"")                                                      # status of a write
        else:
            writer.write(kd.encode(kd.USB_DEV, bytes([2, 0x80, 0, 0])))

    async def uart():
        d = kd.Decoder()
        while data := await reader.read(65536):
            for t, p in d.feed(data):
                if t == kd.CONSOLE_IN:
                    console.put_nowait(p)
                elif t == kd.USB_BUS and p[0] == 4:
                    pass                             # reset keeps the cable and pull-up
                elif t == kd.USB_BUS:
                    print(f"usb bus {p[0]}", flush=True)
                    writer.write(kd.encode(kd.USB_DEV, bytes([1, 1 if p[0] == 1 else 0, 0, 0])))   # the pull-up
                elif t == kd.USB_SETUP:
                    usb_setup(p)
                elif t == kd.USB_OUT:
                    pass
                else:
                    print(f"rx {t} {len(p)}" + (f" {p.hex()}" if t == kd.TIME else ""), flush=True)
        console.put_nowait(None)

    asyncio.create_task(uart())
    pending = b""
    counting = 0
    install = None                     # [size, crc, got, acked] while an install streams
    run_after = False
    while (chunk := await console.get()) is not None:
        pending += chunk
        if install is not None:
            size, crc, got, acked = install
            take, pending = pending[:size - len(got)], pending[size - len(got):]
            got += take
            while acked < len(got) // 4096:
                print(">|ack", flush=True)
                acked += 1
            install[3] = acked
            if len(got) == size:
                ok = zlib.crc32(bytes(got)) == crc
                print(">ok" if ok else ">err code=3 msg=crc", flush=True)
                install = None
                if ok and run_after:
                    loop.call_later(0.3, lambda: asyncio.ensure_future(go(app_name)))
            continue
        if counting:
            if len(pending) >= counting:
                data, pending = pending[:counting], pending[counting:]
                print(f"counted {len(data)} crc {zlib.crc32(data)}", flush=True)
                counting = 0
            continue
        while b"\n" in pending or b"\r" in pending:
            cut = min(i for i in (pending.find(b"\n"), pending.find(b"\r")) if i >= 0)
            line, pending = pending[:cut].decode(errors="replace").strip(), pending[cut + 1:]
            if not line:
                continue
            if line == "crash":
                print("Guru Meditation Error: Core 0 panic'ed", flush=True)
                return 1
            if line == "poweroff":
                writer.write(kd.encode(kd.POWER_OFF))
                await writer.drain()
            elif line.startswith("play ") and chips["writer"]:   # N frames to the speaker
                n = int(line.split()[1])
                to_chips(vf(1, sim_ns()) + ld(29, vf(2, 48000) + vf(3, 2) + vf(4, 16)))
                to_chips(vf(1, sim_ns()) + ld(30, ld(1, struct.pack("<hh", 1000, -1) * n)))
                await chips["writer"].drain()
            elif line.startswith("rxformat ") and chips["writer"]:   # RATE CH BITS: the receiver starts
                rate, ch, bits = (int(x) for x in line.split()[1:4])
                first = i2s["rx"] is None
                i2s["rx"] = (rate, ch, bits)
                to_chips(vf(1, sim_ns()) + ld(29, vf(1, 1) + vf(2, rate) + vf(3, ch) + vf(4, bits)))
                await chips["writer"].drain()

                async def heartbeat():
                    while chips["writer"] is not None:
                        to_chips(vf(1, sim_ns()) + ld(2, b""))
                        await chips["writer"].drain()
                        await asyncio.sleep(0.01)
                if first:
                    loop.create_task(heartbeat())
            elif line == "i2sstat":
                print(f"i2s_in frames {i2s['frames']} peak {i2s['peak']}", flush=True)
            elif line.startswith("spam "):                   # N console lines, as fast as they go
                for i in range(int(line.split()[1])):
                    print(f"spam {i} {'x' * 100}")
                print("spammed", flush=True)
            elif line.startswith("key "):                    # MODS KEY: a boot keyboard's report
                mods, key = (int(x) for x in line.split()[1:3])
                endpoint, report_id = report_sources["key"]
                prefix = b"" if report_id is None else bytes([report_id])
                usb_in(endpoint, prefix + bytes([mods, 0, key, 0, 0, 0, 0, 0]))
            elif line.startswith("mouse "):
                buttons, x, y, wheel = (int(x) for x in line.split()[1:5])
                endpoint, report_id = report_sources["mouse"]
                prefix = b"" if report_id is None else bytes([report_id])
                usb_in(endpoint, prefix + struct.pack("Bbbb", buttons, x, y, wheel))
                await writer.drain()
            elif line.startswith("cut "):                    # N: a frame a reset cuts short
                writer.write(b"KD" + struct.pack("<BI", 0x12, int(line.split()[1])) + bytes(4))
                await writer.drain()
                print("cut", flush=True)
            elif line == "reboot":                           # a panic's: nobody asked for it
                print("fake: reboot", flush=True)
                await boot()
            elif line.startswith("flood "):
                full = kd.encode(kd.FB_RECT, struct.pack("<HHHH", 0, 0, 410, 502) + bytes(410 * 502 * 2))
                for _ in range(int(line.split()[1])):
                    writer.write(full)
                    await writer.drain()
                print("flooded", flush=True)
            elif line == "ping":
                print("kode> ping", flush=True)
                print(">|pong", flush=True)
                print(">ok", flush=True)
            elif line.startswith("install "):                # KodeOS's: src=terminal dst=app ...
                f = dict(kv.split("=", 1) for kv in line.split()[1:])
                if os.environ.get("FAKE_INSTALL_REFUSE") or (f.get("src"), f.get("dst")) != ('"terminal"', "app"):
                    print(">err code=5 msg=install", flush=True)
                    continue
                if "name" in f:
                    print(f'>|evicted name={f["name"]} kept=off', flush=True)
                await asyncio.sleep(float(os.environ.get("FAKE_INSTALL_SLOW", "0")))   # a slow erase
                print("kode> \r\x1b[K>|ready chunk=4096", flush=True)
                install = [int(f["size"]), int(f["crc32"]), bytearray(), 0]
                run_after = f.get("run") == "on"
                pending = b""          # residue before the ready, as kodeOS drains it
                break
            elif line.startswith("i2cw "):                   # ADDR REG BYTE..., hex
                addr, reg, *data = (int(x, 16) for x in line.split()[1:])

                async def write_chip(addr=addr, reg=reg, data=data):
                    ok = await i2c(addr, [reg, *data], 0)
                    print(f"i2cw {addr:02x} {reg:02x}: {'ok' if ok is not None else 'nack'}", flush=True)
                loop.create_task(write_chip())
            elif line.startswith("i2c "):                    # ADDR REG N, hex
                addr, reg, n = (int(x, 16) for x in line.split()[1:4])

                async def read_chip(addr=addr, reg=reg, n=n):
                    data = await i2c(addr, [reg], n)
                    print(f"i2c {addr:02x} {reg:02x}: {data.hex() if data is not None else 'nack'}", flush=True)
                loop.create_task(read_chip())
            elif line.startswith("count "):
                counting = int(line.split()[1])
                print("counting", flush=True)
                break
            else:
                print(f"echo: {line}", flush=True)
    server.close()
    if os.environ.get("FAKE_OUTLIVE_UART"):
        await asyncio.sleep(3600)       # as esp-emu does: its UART1's client gone, it runs on
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
