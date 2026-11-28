"""`kode-emu run`, from outside: the binary, its page's WebSocket, its serial port.

Each simulator gets its own cache (KODE_EMU_CACHE), so its running note
and its card stay in the test's folder, never beside the one you have open.
The esp-emu it runs is $KODE_EMU_ESP_EMU when the person (or a test, for
the fake) set it, and otherwise the binary's own: the pinned esp-emu it
keeps in the person's cache ($KODE_EMU_CACHE as they set it, or
~/Library/Caches/kode-emu), installed there by the binary on first use. A
test's cache links esp-emu-<version> to that folder, so the binary finds
it, or installs it there, and never a second one in the test's folder.
"""

import asyncio
import json
import os
import re
import signal
import subprocess
from pathlib import Path

import aiohttp

import kd

ROOT = Path(__file__).resolve().parents[1]
EXE = ROOT / "target" / "release" / "kode-emu"
# The esp-emu the binary pins, read from the source it is built from (emu.rs).
class Sim:
    """One `kode-emu run`, its page's port, its note and what it printed."""

    def __init__(self, proc, port, cache, out, err):
        self.proc, self.port, self.cache, self.out, self.err = proc, port, cache, out, err
        self.stopped = False            # whether anyone stopped it, or it went by itself

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}/"

    def note(self):
        notes = list((self.cache / "running").glob("*.json"))
        return json.loads(notes[0].read_text()) if notes else None

    @property
    def serial(self):
        return self.note()["serial"]

    def printed(self) -> list[str]:
        """The console lines it printed on stdout, as the terminal shows them."""
        return self.out.read_text(errors="replace").splitlines()

    def stderr(self) -> str:
        return self.err.read_text(errors="replace")

    async def said(self, text, timeout=5):
        loop = asyncio.get_running_loop()
        end = loop.time() + timeout
        while loop.time() < end:
            if any(text in line for line in self.printed()):
                return
            await asyncio.sleep(0.05)
        raise AssertionError(f"no {text!r}; printed {self.printed()[-8:]}")

    def stop(self, sig=signal.SIGTERM) -> int:
        if self.proc.poll() is None:
            self.stopped = True
            self.proc.send_signal(sig)
            try:
                self.proc.wait(15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        return self.proc.returncode

    def stop_clean(self) -> int:
        """stop(), as a test's teardown: a `kode-emu run` started without
        --exit-on-crash serves its page until it is told to stop, and a
        SIGTERM ends it with 0. Gone before anyone stopped it, ended by a
        signal, killed for not stopping, or any other exit code fails."""
        by_itself = self.proc.poll() is not None and not self.stopped
        rc = self.stop()
        tail = self.stderr()[-2000:]
        assert not by_itself, f"kode-emu run exited by itself (with {rc}) before its test ended:\n{tail}"
        assert rc == 0, f"kode-emu run exited with {rc} when stopped:\n{tail}"
        return rc


def env(cache) -> dict:
    """kode-emu's environment with @cache as its cache. Its esp-emu is the
    person's $KODE_EMU_ESP_EMU if they set one, or else the one the binary
    pins and keeps in its tools folder ($KODE_EMU_TOOLS, or the person's
    cache), which KODE_EMU_CACHE does not move."""
    e = {**os.environ, "KODE_EMU_CACHE": str(cache)}
    if not e.get("KODE_EMU_ESP_EMU"):
        e.pop("KODE_EMU_ESP_EMU", None)
    return e


async def start(exe, tmp_path, *args, cache=None, sd=True, wait=True, firmware=None, elf=None,
                up_timeout=20, window=False) -> Sim:
    """`kode-emu run` on @firmware (by default one that is never read: the fake ignores it).
    With @wait, until it announces itself; if it does not in @up_timeout s,
    it is stopped and the AssertionError carries it as `.sim`."""
    cache = cache or tmp_path / "cache"
    port = kd.free_port()
    firmware = firmware or tmp_path / "m.bin"
    elf = elf or (tmp_path / "a.elf" if firmware == tmp_path / "m.bin" else None)
    argv = [str(exe), "run", "--firmware", str(firmware), *(["--elf", str(elf)] if elf else []),
            "--port", str(port), *([] if window else ["--no-open"]), *args]
    if sd and "--sd" not in args:
        argv += ["--sd", str(card(tmp_path))]
    out, err = tmp_path / f"out-{port}", tmp_path / f"err-{port}"
    files = (out.open("wb"), err.open("wb"))
    try:
        proc = subprocess.Popen(argv, env=env(cache), stdin=subprocess.DEVNULL, stdout=files[0], stderr=files[1])
    finally:
        for f in files:                  # the child has its own; the files are read by name
            f.close()
    sim = Sim(proc, port, cache, out, err)
    sim._files = files
    if wait:
        try:
            loop = asyncio.get_running_loop()
            end = loop.time() + up_timeout
            while sim.note() is None and proc.poll() is None and loop.time() < end:
                await asyncio.sleep(0.05)
            assert sim.note() is not None, f"kode-emu run never came up: {sim.stderr()}"
        except BaseException as e:           # a timeout, or the test cancelled: nothing left running
            sim.stop()
            e.sim = sim
            raise
    return sim


def card(tmp_path, mb=8):
    """A blank card image of @mb MB (all zeros: no filesystem on it)."""
    img = tmp_path / "card.img"
    if not img.exists():
        with open(img, "wb") as f:
            f.truncate(mb * 1024 * 1024)
    return img


# ---- the page ------------------------------------------------------------------

async def ws(sim, **kw):
    s = aiohttp.ClientSession()
    w = await s.ws_connect(f"http://127.0.0.1:{sim.port}/ws", **kw)
    w.session = s
    w.sim = sim
    return w


def _closed(w, waiting_for) -> EOFError:
    """What a wait on @w raises when the page's WebSocket closes under it."""
    why = f"the page's WebSocket closed before {waiting_for}: code {w.close_code}"
    if w.exception() is not None:
        why += f", {w.exception()!r}"
    sim = getattr(w, "sim", None)
    if sim is not None:
        rc = sim.proc.poll()
        why += f"; kode-emu run {'is still running' if rc is None else f'exited with {rc}'}"
        why += f"; its stderr ends:\n{sim.stderr()[-2000:]}"
    return EOFError(why)


async def next_json(w, pred, timeout=10):
    """The first JSON message for which @pred holds; EOFError if the page's
    WebSocket closes first, TimeoutError if none comes in @timeout s."""
    async def scan():
        async for m in w:
            if m.type == aiohttp.WSMsgType.TEXT:
                j = json.loads(m.data)
                if pred(j):
                    return j
        raise _closed(w, "the JSON it was waiting for")
    return await asyncio.wait_for(scan(), timeout)


async def next_frame(w, type_, timeout=10):
    """The payload of the first frame of @type_; EOFError if the page's
    WebSocket closes first, TimeoutError if none comes in @timeout s."""
    d = kd.Decoder()

    async def scan():
        async for m in w:
            if m.type == aiohttp.WSMsgType.BINARY:
                for t, p in d.feed(m.data):
                    if t == type_:
                        return p
        raise _closed(w, f"a frame of type {type_:#04x}")
    return await asyncio.wait_for(scan(), timeout)


async def close(w):
    await w.close()
    await w.session.close()


# ---- the serial port -------------------------------------------------------------

def open_serial(path):
    import termios
    import tty
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    tty.setraw(fd)
    termios.tcflush(fd, termios.TCIOFLUSH)
    return fd


async def serial_write(fd, data: bytes) -> None:
    """All of it, as pyserial's blocking write would: the pty takes a little at a time."""
    while data:
        try:
            data = data[os.write(fd, data):]
        except BlockingIOError:
            await asyncio.sleep(0.005)


async def serial_read_until(fd, needle: bytes, timeout=10) -> bytes:
    got = b""
    loop = asyncio.get_running_loop()
    end = loop.time() + timeout
    while needle not in got and loop.time() < end:
        try:
            got += os.read(fd, 4096)
        except BlockingIOError:
            await asyncio.sleep(0.02)
    return got
