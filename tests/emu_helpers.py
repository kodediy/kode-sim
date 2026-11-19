"""The firmware in esp-emu, driven through kode-sim as a person's page drives it.

`firmware()` builds a project with `kode-sim build --json`; `boot()` runs it
with `kode-sim run --firmware ... --no-open` and connects a page to it. The
page (`Kode`) hears every frame the firmware sends and every console line,
and sends what the page sends: the board's inputs as KD frames, console
lines and power as JSON. Raw console bytes go through the serial port, as
kcli's do.
"""

import asyncio
import collections
import json
import os
import re
import subprocess
from pathlib import Path

import aiohttp

import kd
from sim import env, open_serial, serial_write, start


def firmware(exe, project: Path, sdk: Path | None = None, board: bool = False) -> dict:
    """`kode-sim build` of @project: {"merged_bin", "elf", "app_bin"} as Paths.
    In kode-sim's own cache, so a project is built once for every test."""
    argv = [str(exe), "build", "--json", *(["--sdk", str(sdk)] if sdk else []), *(["--board"] if board else []),
            str(project)]
    e = {k: v for k, v in os.environ.items() if k != "KODE_SIM_CACHE"}
    r = subprocess.run(argv, env=e, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    assert r.returncode == 0, r.stderr[-4000:]
    return {k: Path(v) if v else None for k, v in json.loads(r.stdout.strip().splitlines()[-1]).items()}


class Kode:
    """A `kode-sim run` and a page on it, its WebSocket read as it comes."""

    def __init__(self, sim, cache):
        self.sim, self.cache = sim, cache
        self.backlog: collections.deque[tuple[int, bytes]] = collections.deque()
        self.console: list[str] = []          # every line, from the first
        self.on_line: list = []
        self.state = None
        self.last_frame_at: dict[int, float] = {}   # frame type: loop time it last came
        self._more = asyncio.Event()
        self._eof = False
        self._serial = None

    async def connect(self, timeout: float = 60):
        self.session = aiohttp.ClientSession()
        loop = asyncio.get_running_loop()
        end = loop.time() + timeout
        try:
            while True:
                try:
                    self.ws = await self.session.ws_connect(f"{self.sim.url}ws", max_msg_size=0)
                    break
                except aiohttp.ClientError:
                    if loop.time() > end or self.sim.proc.poll() is not None:
                        raise AssertionError(f"no page on {self.sim.url}: {self.sim.stderr()}")
                    await asyncio.sleep(0.05)
        except BaseException:
            await self.session.close()
            raise
        self._pump = asyncio.create_task(self._read())

    async def _read(self):
        """Keep the WebSocket drained, as the page does: a page that stops
        reading loses screen and sound frames."""
        d = kd.Decoder()
        loop = asyncio.get_running_loop()
        try:
            async for m in self.ws:
                if m.type == aiohttp.WSMsgType.BINARY:
                    frames = d.feed(m.data)
                    for t, _ in frames:
                        self.last_frame_at[t] = loop.time()
                    self.backlog.extend(frames)
                elif m.type == aiohttp.WSMsgType.TEXT:
                    j = json.loads(m.data)
                    if "console" in j:
                        self.console.append(j["console"])
                        for f in list(self.on_line):
                            f(j["console"])
                    if "state" in j:
                        self.state = j["state"]
                self._more.set()
        finally:
            self._eof = True
            self._more.set()

    # ---- the page's side ----------------------------------------------------

    async def send(self, type_: int, payload: bytes = b"") -> None:
        await self.ws.send_bytes(kd.encode(type_, payload))

    async def send_json(self, j: dict) -> None:
        await self.ws.send_str(json.dumps(j))

    async def console_in(self, line: str) -> None:
        await self.send_json({"console_in": line})

    async def console_bytes(self, data: bytes) -> None:
        """Raw console input - a payload, newlines and NULs among it - through the serial port."""
        if self._serial is None:
            self._serial = open_serial(self.sim.serial)
        await serial_write(self._serial, data)

    async def uart_bytes(self) -> int:
        async with self.session.get(f"{self.sim.url}api/stats") as r:
            return (await r.json())["uart_bytes"]

    def closed(self, waiting_for: str) -> EOFError:
        """What a wait raises when the page's WebSocket closed under it."""
        rc = self.sim.proc.poll()
        return EOFError(f"the page's WebSocket closed before {waiting_for}; kode-sim run "
                        f"{'is still running' if rc is None else f'exited with {rc}'}; its stderr ends:\n"
                        f"{self.sim.stderr()[-2000:]}")

    def alive(self) -> bool:
        return self.sim.proc.poll() is None and self.state == "running"

    async def stop(self):
        self._pump.cancel()
        await self.ws.close()
        await self.session.close()
        if self._serial is not None:
            os.close(self._serial)
        await asyncio.to_thread(self.sim.stop)


async def boot(exe, tmp_path, fw: dict, *args, card=None, timeout: float = 60) -> Kode:
    """`kode-sim run` on @fw with @args, and a page on it. Without --sd its
    slot is empty (@card, the image kept out of it, is made once). If it is
    not up in @timeout s it is stopped, and the AssertionError carries it as `.sim`."""
    cache = tmp_path / "cache"
    if "--sd" not in args:
        args = (*args, "--sd", str(card or tmp_path / "card.img"), "--no-sd")
    sim = await start(exe, tmp_path, *args, cache=cache, sd=False, wait=False,
                      firmware=fw["merged_bin"], elf=fw.get("elf"))
    k = Kode(sim, cache)
    try:
        loop = asyncio.get_running_loop()
        end = loop.time() + timeout
        await k.connect(timeout)
        while sim.note() is None:                    # up, its serial port open and announced
            assert sim.proc.poll() is None and loop.time() < end, f"kode-sim run never came up: {sim.stderr()}"
            await asyncio.sleep(0.05)
    except BaseException as e:                       # a timeout, or the test cancelled: nothing left running
        if hasattr(k, "_pump"):
            k._pump.cancel()
            await k.ws.close()
            await k.session.close()
        await asyncio.to_thread(sim.stop)
        e.sim = sim
        raise
    return k


def install(exe, k: Kode, app: Path, *args) -> subprocess.CompletedProcess:
    """`kode-sim install` into @k, as a person types it in another terminal."""
    return subprocess.run([str(exe), "install", *args, str(app)], env=env(k.cache),
                          capture_output=True, text=True, timeout=120)


# ---- waiting on the firmware --------------------------------------------------

async def frame_where(k: Kode, pred, timeout: float) -> tuple[int, bytes]:
    """The first frame from the firmware for which pred(type, payload) holds."""
    async def scan():
        while True:
            while k.backlog:
                t, p = k.backlog.popleft()
                if pred(t, p):
                    return t, p
            if k._eof:
                raise k.closed("the frame it was waiting for")
            k._more.clear()
            await k._more.wait()
    return await asyncio.wait_for(scan(), timeout)


async def console_line(k: Kode, pattern: str, timeout: float, *, since: int = 0) -> str:
    """A matching console line at or after @since in the page's journal."""
    rx = re.compile(pattern)
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    seen = since
    while loop.time() < deadline:
        eof = k._eof                            # before the lines: what came before the close is in them
        lines = k.console[:]
        for line in lines[seen:]:
            if rx.search(line):
                return line
        seen = len(lines)
        if eof:
            raise k.closed(f"a console line matching {pattern!r}")
        await asyncio.sleep(0.05)
    raise TimeoutError(f"no console line matching {pattern!r}; last lines:\n" + "\n".join(k.console[-30:]))


async def clock_runs(k: Kode, timeout: float = 10) -> bool:
    """Whether kodeOS's QP clock still ticks: `help` answers in slices paced
    by a QTimeEvt, so its closing >ok needs time events to fire. (`info`
    answers in one go, and does even with the clock stopped.)"""
    answer: list[str] = []
    listen = answer.append                      # only what comes after the question
    k.on_line.append(listen)
    try:
        await k.console_in("help")
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while loop.time() < deadline:
            if any(l.startswith(">ok") for l in answer):
                return True
            if any(l.startswith(">err") for l in answer):
                return False
            if k._eof:
                raise k.closed("help's >ok")
            await asyncio.sleep(0.1)
        return False
    finally:
        k.on_line.remove(listen)


async def console_ready(k: Kode, timeout: float = 60, every: float = 1.0) -> None:
    """Until the firmware's console takes commands: `ping`, asked again
    every @every s, answered with >ok (kaos's console, kodeOS's and an
    app's, as `kode-sim install` asks before it sends anything). Only what
    comes after a question counts. TimeoutError if it never answers."""
    answer: list[str] = []
    listen = answer.append
    k.on_line.append(listen)
    try:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while loop.time() < deadline:
            await k.console_in("ping")
            asked = loop.time()
            while loop.time() < min(asked + every, deadline):
                if any(l.startswith(">ok") for l in answer):
                    return
                if k._eof:
                    raise k.closed("ping's >ok")
                await asyncio.sleep(0.05)
        raise TimeoutError(f"ping never answered >ok in {timeout} s; last lines:\n" + "\n".join(k.console[-30:]))
    finally:
        k.on_line.remove(listen)


async def screen_quiet(k: Kode, quiet_ms: float, timeout: float, types=(kd.FB_RECT,)) -> None:
    """Until no frame of @types (the screen's, by default) has come for
    @quiet_ms, after at least one came: an animation over, a screen drawn.
    The frames stay in k.backlog for frame_where. TimeoutError if it never
    draws, or is never that quiet."""
    loop = asyncio.get_running_loop()
    since = loop.time()
    deadline = since + timeout
    while True:
        if k._eof:                                  # no frames because no page: not quiet
            raise k.closed(f"{quiet_ms} ms without a screen frame")
        last = max([since, *(k.last_frame_at.get(t, since) for t in types)])
        if last > since and (loop.time() - last) * 1000 >= quiet_ms:
            return
        if loop.time() >= deadline:
            raise TimeoutError(f"the screen never drew and went quiet for {quiet_ms} ms in {timeout} s")
        await asyncio.sleep(min(0.05, quiet_ms / 4000))


def no_panic(k: Kode) -> None:
    bad = [l for l in k.console if kd.CRASH.search(l)]
    assert not bad, "\n".join(bad[:10])
    assert k.state == "running", f"the Kode Dot is {k.state}"
