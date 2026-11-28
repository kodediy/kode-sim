"""The harness itself (sim.py, emu_helpers.py): what a test that waits on a
page, or on `kode-sim run` coming up, sees when the other end goes away."""

import asyncio
import json
import re

import aiohttp
import pytest
from aiohttp import web

import kd
import sim as simmod
from sim import next_frame, next_json, start


async def _closing_page():
    """A /ws that says one line and closes, as a kode-sim that died would."""
    async def handler(request):
        w = web.WebSocketResponse()
        await w.prepare(request)
        await w.send_str(json.dumps({"console": "bye"}))
        await w.close()
        return w
    app = web.Application()
    app.router.add_get("/ws", handler)
    runner = web.AppRunner(app)
    await runner.setup()
    port = kd.free_port()
    await web.TCPSite(runner, "127.0.0.1", port).start()
    return runner, port


async def _page_ws(port):
    s = aiohttp.ClientSession()
    w = await s.ws_connect(f"http://127.0.0.1:{port}/ws")
    w.session = s
    return w


async def test_waiting_for_json_on_a_page_that_closed_raises():
    runner, port = await _closing_page()
    try:
        w = await _page_ws(port)
        with pytest.raises(EOFError, match="WebSocket closed"):
            await next_json(w, lambda j: j.get("console") == "never", timeout=5)
        await simmod.close(w)
    finally:
        await runner.cleanup()


async def test_waiting_for_a_frame_on_a_page_that_closed_raises():
    runner, port = await _closing_page()
    try:
        w = await _page_ws(port)
        with pytest.raises(EOFError, match="WebSocket closed"):
            await next_frame(w, kd.FB_RECT, timeout=5)
        await simmod.close(w)
    finally:
        await runner.cleanup()


def _script(tmp_path, name, body):
    exe = tmp_path / name
    exe.write_text("#!/bin/sh\n" + body + "\n")
    exe.chmod(0o755)
    return exe


async def test_a_simulator_that_exits_by_itself_fails_its_teardown(tmp_path):
    exe = _script(tmp_path, "dies", "exit 0")
    s = await start(exe, tmp_path, wait=False)
    await asyncio.to_thread(s.proc.wait, 5)
    with pytest.raises(AssertionError, match="exited by itself"):
        s.stop_clean()


async def test_a_simulator_that_ends_badly_on_sigterm_fails_its_teardown(tmp_path):
    exe = _script(tmp_path, "badterm", "trap 'exit 3' TERM\nwhile :; do sleep 0.05; done")
    s = await start(exe, tmp_path, wait=False)
    await asyncio.sleep(0.3)
    with pytest.raises(AssertionError, match="exited with 3"):
        s.stop_clean()


async def test_a_simulator_stopped_by_its_test_and_by_sigterm_tears_down_clean(tmp_path):
    exe = _script(tmp_path, "goodterm", "trap 'exit 0' TERM\nwhile :; do sleep 0.05; done")
    s = await start(exe, tmp_path, wait=False)
    await asyncio.sleep(0.3)
    assert s.stop() == 0                 # as test_sigterm_stops_... does
    assert s.stop_clean() == 0


async def test_waiting_for_a_console_line_on_a_page_that_closed_raises():
    from types import SimpleNamespace

    from emu_helpers import Kode, console_line
    runner, port = await _closing_page()
    try:
        stub = SimpleNamespace(url=f"http://127.0.0.1:{port}/", proc=SimpleNamespace(poll=lambda: None),
                               stderr=lambda: "")
        k = Kode(stub, None)
        await k.connect()
        assert await console_line(k, r"^bye$", 5) == "bye"      # what came before the close is still there
        with pytest.raises(EOFError, match="WebSocket closed"):
            await console_line(k, r"never", 5)
        k._pump.cancel()
        await k.session.close()
    finally:
        await runner.cleanup()


async def test_a_simulator_that_never_comes_up_is_killed(tmp_path):
    exe = _script(tmp_path, "hangs", "exec sleep 600")
    with pytest.raises(AssertionError, match="never came up") as e:
        await start(exe, tmp_path, up_timeout=0.3)
    s = e.value.sim                       # what it started is gone
    assert s.proc.poll() is not None


async def test_a_boot_that_never_comes_up_is_killed(tmp_path):
    from emu_helpers import boot
    exe = _script(tmp_path, "hangs", "exec sleep 600")
    with pytest.raises(AssertionError, match="no page on|never came up") as e:
        await boot(exe, tmp_path, {"merged_bin": tmp_path / "m.bin", "elf": None}, timeout=0.5)
    s = e.value.sim
    assert s.proc.poll() is not None


async def test_fake_startup_opens_control_and_chips_before_announcing_uart():
    import socket
    import sys
    from contextlib import ExitStack, suppress
    from pathlib import Path

    with ExitStack() as reservations:
        ports = {}
        for name in ("uart1", "control", "cosim"):
            listener = reservations.enter_context(socket.socket())
            listener.bind(("127.0.0.1", 0))
            ports[name] = listener.getsockname()[1]
    arguments = [part for name, port in ports.items() for part in (f"--{name}-tcp", f"127.0.0.1:{port}")]
    process = await asyncio.create_subprocess_exec(
        sys.executable, str(Path(__file__).with_name("fake_esp_emu.py")), "--chip", "esp32p4",
        *arguments, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    writers = []
    try:
        async with asyncio.timeout(5):
            announcement = await process.stdout.readline()
            assert announcement == b"fake: up\n", announcement.decode()
            _, chips = await asyncio.open_connection("127.0.0.1", ports["cosim"])
            writers.append(chips)
            reader, control = await asyncio.open_connection("127.0.0.1", ports["control"])
            writers.append(control)
            control.write(b"ping\n")
            await control.drain()
            assert await reader.readline() == b"ok\n"

            reader, uart = await asyncio.open_connection("127.0.0.1", ports["uart1"])
            writers.append(uart)
            decoder, frames = kd.Decoder(), []
            while len(frames) < 2:
                chunk = await reader.read(65536)
                assert chunk, "UART closed before its initial frames"
                frames.extend(decoder.feed(chunk))
            assert [kind for kind, _ in frames] == [kd.HELLO, kd.FB_RECT]

            for writer in reversed(writers):
                writer.close()
                await writer.wait_closed()
            output, _ = await process.communicate()
            assert process.returncode == 0, output.decode()
    finally:
        for writer in writers:
            writer.close()
        if process.returncode is None:
            with suppress(ProcessLookupError):
                process.kill()
            await process.wait()


# ---- which esp-emu --------------------------------------------------------------

def test_the_environment_names_an_esp_emu_only_when_the_person_did(tmp_path, monkeypatch):
    monkeypatch.delenv("KODE_SIM_ESP_EMU", raising=False)
    assert "KODE_SIM_ESP_EMU" not in simmod.env(tmp_path / "cache")
    monkeypatch.setenv("KODE_SIM_ESP_EMU", "/theirs/esp-emu")
    assert simmod.env(tmp_path / "cache2")["KODE_SIM_ESP_EMU"] == "/theirs/esp-emu"


async def test_the_esp_emu_run_is_the_one_kode_sim_keeps_in_the_person_s_cache(exe, fake_emu, tmp_path, monkeypatch):
    """No KODE_SIM_ESP_EMU: the simulator, in a cache of its own, runs the
    pinned esp-emu kept in the person's cache (here the fake, put there)."""
    import os
    import shutil
    fake = os.environ["KODE_SIM_ESP_EMU"]
    monkeypatch.delenv("KODE_SIM_ESP_EMU")
    person = tmp_path / "person"
    monkeypatch.setenv("KODE_SIM_TOOLS", str(person))
    version = re.search(r'pub const ESP_EMU_VERSION: &str = "([^"]+)"',
                        (simmod.ROOT / "crates" / "kode-sim" / "src" / "emu.rs").read_text())[1]
    pinned = person / f"esp-emu-{version}" / "esp-emu"
    pinned.parent.mkdir(parents=True)
    shutil.copy(fake, pinned)
    s = await start(exe, tmp_path)
    try:
        for _ in range(100):
            if fake_emu.exists():
                break
            await asyncio.sleep(0.05)
        assert fake_emu.exists(), f"the pinned esp-emu never ran: {s.stderr()}"
        assert s.cache != person and not (person / "running").exists()
    finally:
        s.stop_clean()


# ---- waiting on a signal, not a sleep ---------------------------------------------

async def _boot_fake(exe, tmp_path):
    from emu_helpers import boot
    return await boot(exe, tmp_path, {"merged_bin": tmp_path / "m.bin", "elf": None}, card=simmod.card(tmp_path))


async def test_console_ready_is_ping_answered_with_ok(exe, fake_emu, tmp_path):
    from emu_helpers import console_ready
    k = await _boot_fake(exe, tmp_path)
    try:
        await console_ready(k, 20)
        assert ">ok" in k.console and "kode> ping" in k.console
    finally:
        await k.stop()


async def test_screen_quiet_waits_for_the_frames_to_stop(exe, fake_emu, tmp_path):
    from emu_helpers import screen_quiet
    k = await _boot_fake(exe, tmp_path)
    try:
        await k.console_in("flood 60")
        await screen_quiet(k, 300, timeout=30)
        assert "flooded" in k.console, "quiet before the flood was over"
        assert any(t == kd.FB_RECT for t, _ in k.backlog)   # left for frame_where (the bridge may skip some)
        with pytest.raises(TimeoutError):
            await screen_quiet(k, 5000, timeout=0.5)        # never 5 s quiet in half a second
    finally:
        await k.stop()


async def test_screen_quiet_on_a_screen_that_never_draws_times_out(exe, fake_emu, tmp_path):
    from emu_helpers import screen_quiet
    k = await _boot_fake(exe, tmp_path)
    try:
        await asyncio.sleep(0.5)                            # the fake's boot square is in
        k.last_frame_at.clear()
        with pytest.raises(TimeoutError, match="never drew"):
            await screen_quiet(k, 100, timeout=1)
    finally:
        await k.stop()
