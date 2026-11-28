"""`kode-emu run` opens the Kode Dot in a window of its own: Chrome or Edge
in app mode (no tabs, no address bar), with a profile of its own, sound
and the microphone allowed without a click, and the simulator stopped when
that window's browser goes. Here the browser is a stand-in that writes down
how it was started and quits when told."""

import asyncio
import os
import sys

import pytest

from sim import start


@pytest.fixture
def browser(tmp_path, monkeypatch):
    """A stand-in for Chrome: its arguments go to argv.txt, and it quits
    once quit.flag exists."""
    exe = tmp_path / "browser"
    exe.write_text(f"""#!{sys.executable}
import os, sys, time
open({str(tmp_path / 'pid.txt')!r}, 'w').write(str(os.getpid()))
open({str(tmp_path / 'argv.txt')!r}, 'w').write('\\n'.join(sys.argv[1:]))
while not os.path.exists({str(tmp_path / 'quit.flag')!r}):
    time.sleep(0.05)
""")
    exe.chmod(0o755)
    monkeypatch.setenv("KODE_EMU_BROWSER", str(exe))
    return tmp_path


async def _argv(dir, timeout=10):
    loop = asyncio.get_running_loop()
    end = loop.time() + timeout
    while loop.time() < end:
        f = dir / "argv.txt"
        if f.exists() and f.read_text():
            return f.read_text().splitlines()
        await asyncio.sleep(0.05)
    raise AssertionError("the browser was never started")


async def test_the_kode_dot_opens_in_an_app_window_of_its_own(exe, fake_emu, browser, tmp_path):
    s = await start(exe, tmp_path, window=True)
    try:
        argv = await _argv(browser)
        assert f"--app=http://127.0.0.1:{s.port}/" in argv
        profile = [a for a in argv if a.startswith("--user-data-dir=")]
        assert profile and profile[0].split("=", 1)[1].startswith(str(s.cache)), argv   # its own, in the cache
        assert "--autoplay-policy=no-user-gesture-required" in argv                   # sound without a click
        assert "--use-fake-ui-for-media-stream" in argv                                # the microphone without a prompt
    finally:
        s.stop()


async def test_closing_the_window_stops_the_simulator(exe, fake_emu, browser, tmp_path):
    s = await start(exe, tmp_path, window=True)
    await _argv(browser)
    (browser / "quit.flag").write_text("")
    loop = asyncio.get_running_loop()
    end = loop.time() + 15
    while s.proc.poll() is None and loop.time() < end:
        await asyncio.sleep(0.1)
    rc = s.proc.poll()
    s.stop()
    assert rc == 0, f"kode-emu run went on (or failed) after its window closed: {rc}\n{s.stderr()[-1500:]}"


async def test_no_open_opens_nothing(exe, fake_emu, browser, tmp_path):
    s = await start(exe, tmp_path)
    try:
        await asyncio.sleep(1.5)
        assert not (browser / "argv.txt").exists()
    finally:
        s.stop_clean()


async def test_stopping_the_simulator_closes_its_window(exe, fake_emu, browser, tmp_path):
    s = await start(exe, tmp_path, window=True)
    await _argv(browser)
    pid = int((browser / "pid.txt").read_text())
    assert s.stop() == 0, s.stderr()[-1500:]
    loop = asyncio.get_running_loop()
    end = loop.time() + 5
    while loop.time() < end:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        await asyncio.sleep(0.1)
    os.kill(pid, 9)
    raise AssertionError("the window outlived the simulator")


@pytest.fixture
def mac_browser(tmp_path, monkeypatch):
    """A stand-in for Chrome on a Mac: it opens the page (a WebSocket, as the
    window's page does), and when the window is closed (close.flag) the page
    goes but the browser lives on, as a Mac app does with no window left."""
    exe = tmp_path / "browser"
    exe.write_text(f"""#!{sys.executable}
import asyncio, os, sys
import aiohttp
url = [a for a in sys.argv[1:] if a.startswith('--app=')][0][len('--app='):]
async def main():
    async with aiohttp.ClientSession() as s:
        w = await s.ws_connect(url + 'ws')
        open({str(tmp_path / 'paged.flag')!r}, 'w').write('')
        while not os.path.exists({str(tmp_path / 'close.flag')!r}):
            try:
                await asyncio.wait_for(w.receive(), 0.05)
            except asyncio.TimeoutError:
                pass
        await w.close()
    while True:
        await asyncio.sleep(1)
asyncio.run(main())
""")
    exe.chmod(0o755)
    monkeypatch.setenv("KODE_EMU_BROWSER", str(exe))
    return tmp_path


async def test_closing_the_window_stops_the_simulator_where_the_browser_lives_on(exe, fake_emu, mac_browser, tmp_path):
    s = await start(exe, tmp_path, window=True)
    loop = asyncio.get_running_loop()
    end = loop.time() + 10
    while not (mac_browser / "paged.flag").exists() and loop.time() < end:
        await asyncio.sleep(0.05)
    assert (mac_browser / "paged.flag").exists(), "the window never opened its page"
    (mac_browser / "close.flag").write_text("")
    end = loop.time() + 15
    while s.proc.poll() is None and loop.time() < end:
        await asyncio.sleep(0.1)
    rc = s.proc.poll()
    s.stop()
    assert rc == 0, f"kode-emu run went on after its window's page went: {rc}\n{s.stderr()[-1500:]}"
