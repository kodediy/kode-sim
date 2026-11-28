"""`kode-emu install`, from outside: the binary, the fake esp-emu and a
`kode-emu run` to install into - through kodeOS's console, as kcli does,
and `--mailbox`: the app into app_pending of the flash esp-emu runs, and a
reset, as an Arduino upload does.

Each test has a cache of its own (KODE_EMU_CACHE), so the notes the
simulators leave there are never beside the one you have open.
"""

import asyncio
import json
import os
import subprocess
import time

import aiohttp

from sim import start

APP = b"\xe9" + bytes(range(256)) * 40          # an app image, two whole chunks and a tail


def install(exe, cache, *args, timeout=60):
    return subprocess.run([str(exe), "install", *[str(a) for a in args]],
                          env={**os.environ, "KODE_EMU_CACHE": str(cache)},
                          capture_output=True, text=True, timeout=timeout)


def app(tmp_path, data=APP):
    f = tmp_path / "app.bin"
    f.write_bytes(data)
    return f


def note(cache, pid, serial, url, age=0):
    d = cache / "running"
    d.mkdir(parents=True, exist_ok=True)
    f = d / f"{pid}.json"
    f.write_text(json.dumps({"pid": pid, "serial": serial, "url": url}))
    t = time.time() - age
    os.utime(f, (t, t))
    return f


# ---- through the console --------------------------------------------------------

async def test_install_streams_the_image_a_chunk_per_ack(exe, sim, tmp_path):
    r = await asyncio.to_thread(install, exe, sim.cache, app(tmp_path))
    assert r.returncode == 0, r.stderr
    assert f"installing app.bin in the simulator at {sim.url}" in r.stderr
    assert f"-> 4096/{len(APP)} bytes" in r.stderr
    assert f"installed {len(APP)} bytes in " in r.stderr
    await sim.said("Project name:     app")              # kodeOS booted it


async def test_install_reports_kodeos_refusing(exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_INSTALL_REFUSE", "1")
    s = await start(exe, tmp_path)
    try:
        r = await asyncio.to_thread(install, exe, s.cache, app(tmp_path))
        assert r.returncode == 1
        assert "install: >err code=5" in r.stderr, r.stderr
    finally:
        s.stop()


async def test_install_with_nothing_listening_says_so(exe, tmp_path):
    """A serial port with no kodeOS console behind it (an app running, say)."""
    master, slave = os.openpty()
    try:
        note(tmp_path, os.getpid(), os.ttyname(slave), "http://127.0.0.1:1/")
        r = await asyncio.to_thread(install, exe, tmp_path, app(tmp_path))
        assert r.returncode == 1
        assert f"no kodeOS console on {os.ttyname(slave)}" in r.stderr, r.stderr
    finally:
        os.close(master)
        os.close(slave)


def test_a_note_from_a_dead_simulator_is_not_a_simulator(exe, tmp_path):
    dead = note(tmp_path, 999999, "/dev/null", "http://127.0.0.1:1/")
    r = install(exe, tmp_path, app(tmp_path))
    assert r.returncode == 1
    assert "no simulator is running" in r.stderr and "kode-emu run" in r.stderr
    assert not dead.exists(), "the dead simulator's note is cleared"


def test_install_with_no_simulator_running_stops_before_building(exe, tmp_path):
    project = tmp_path / "proj"
    project.mkdir()
    (project / "CMakeLists.txt").write_text("project(proj)\n")
    r = install(exe, tmp_path / "cache", project)
    assert r.returncode == 1
    assert "kode-emu run" in r.stderr and "building" not in r.stderr
    assert not any((tmp_path / "cache").glob("proj-*")), "built with nowhere to install"
    r = install(exe, tmp_path / "cache", "--port", "9999", project)
    assert r.returncode == 1 and "no simulator is on port 9999" in r.stderr


async def test_install_finds_the_newest_of_two_simulators_or_the_one_on_a_port(exe, fake_emu, tmp_path):
    """Two `kode-emu run` at once: install uses the one started last, --port
    the one whose page is on that port; when the newer stops, its note goes
    and installs go to the other."""
    cache = tmp_path / "cache"
    older = await start(exe, tmp_path, cache=cache)
    card2 = tmp_path / "card2.img"                       # a card is in one simulator at a time
    with open(card2, "wb") as f:
        f.truncate(8 << 20)
    newer = await start(exe, tmp_path, "--sd", str(card2), cache=cache, wait=False)
    try:
        for _ in range(400):
            if len(list((cache / "running").glob("*.json"))) == 2:
                break
            await asyncio.sleep(0.05)
        assert len(list((cache / "running").glob("*.json"))) == 2, newer.stderr()
        f = app(tmp_path)
        r = await asyncio.to_thread(install, exe, cache, f)
        assert r.returncode == 0 and f"at {newer.url}" in r.stderr, r.stderr
        await newer.said("Project name:     app")
        assert not any("Project name:     app" in line for line in older.printed())
        r = await asyncio.to_thread(install, exe, cache, "--port", older.port, f)
        assert r.returncode == 0 and f"at {older.url}" in r.stderr, r.stderr
        await older.said("Project name:     app")
        newer.stop()
        r = await asyncio.to_thread(install, exe, cache, f)
        assert r.returncode == 0 and f"at {older.url}" in r.stderr, r.stderr
    finally:
        older.stop()
        newer.stop()


def test_what_is_not_an_app_is_not_installed(exe, tmp_path):
    master, slave = os.openpty()
    try:
        note(tmp_path, os.getpid(), os.ttyname(slave), "http://127.0.0.1:1/")
        r = install(exe, tmp_path, app(tmp_path, b"not an app at all"))
        assert r.returncode == 1 and "not an ESP-IDF app image" in r.stderr, r.stderr
    finally:
        os.close(master)
        os.close(slave)


# ---- through the mailbox -----------------------------------------------------------

async def test_mailbox_puts_the_app_in_app_pending_and_resets(exe, sim, tmp_path):
    """As an Arduino upload does: the image into app_pending of the running
    flash, a reset, and kodeOS adopts it as it boots."""
    r = await asyncio.to_thread(install, exe, sim.cache, "--mailbox", app(tmp_path))
    assert r.returncode == 0, r.stderr
    assert f"{len(APP)} bytes at 0x1a00000" in r.stderr, r.stderr
    await sim.said(f"fake: write-region 0x1a00000 {len(APP)}")
    await sim.said("fake: reset")
    await sim.said("adopted from the mailbox")
    await sim.said("Project name:     app")
    await sim.said("[kode-emu] ")
    lines = sim.printed()
    resets = [i for i, line in enumerate(lines) if line == "fake: reset"]    # the first is the chips' at boot
    assert lines.index(f"fake: write-region 0x1a00000 {len(APP)}") < resets[-1]


async def test_mailbox_needs_no_kodeos_console(exe, fake_emu, tmp_path, monkeypatch):
    """kodeOS refusing installs on its console does not matter to the mailbox."""
    monkeypatch.setenv("FAKE_INSTALL_REFUSE", "1")
    s = await start(exe, tmp_path)
    try:
        r = await asyncio.to_thread(install, exe, s.cache, "--mailbox", app(tmp_path))
        assert r.returncode == 0, r.stderr
        await s.said("adopted from the mailbox")
    finally:
        s.stop()


async def test_mailbox_without_app_pending_says_so(exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_NO_MAILBOX", "1")
    s = await start(exe, tmp_path)
    try:
        r = await asyncio.to_thread(install, exe, s.cache, "--mailbox", app(tmp_path))
        assert r.returncode == 1
        assert "no app_pending partition" in r.stderr, r.stderr
        assert not any("fake: write-region" in line for line in s.printed())
        assert s.printed().count("fake: reset") == 1, "only the chips' reset as it booted"
    finally:
        s.stop()


async def test_mailbox_reads_the_table_of_the_running_flash(exe, fake_emu, tmp_path):
    """app_pending is wherever the image's own partition table puts it."""
    import struct
    table = b"".join(struct.pack("<2sBBII16sI", b"\xaa\x50", t, st, off, size, name, 0) for name, t, st, off, size in
                     [(b"factory", 0, 0, 0x20000, 0x400000), (b"app_pending", 1, 0x41, 0x800000, 0x2000)])
    with open(tmp_path / "m.bin", "wb") as f:              # the firmware `start` boots
        f.write(b"\xff" * 0x8000 + table.ljust(0xC00, b"\xff"))
    s = await start(exe, tmp_path)
    try:
        r = await asyncio.to_thread(install, exe, s.cache, "--mailbox", app(tmp_path, b"\xe9" + bytes(100)))
        assert r.returncode == 0 and "101 bytes at 0x800000" in r.stderr, r.stderr
        # Its last 4 KB are kodeOS's intent flag: an image gets the rest.
        r = await asyncio.to_thread(install, exe, s.cache, "--mailbox", app(tmp_path, b"\xe9" + bytes(0x1000)))
        assert r.returncode == 1 and "app_pending holds 4096 bytes" in r.stderr, r.stderr
        assert "intent flag" in r.stderr
    finally:
        s.stop()


async def test_the_mailbox_takes_pages_from_itself_only(sim):
    async with aiohttp.ClientSession() as s:
        r = await s.post(f"{sim.url}api/mailbox", data=APP, headers={"Origin": "https://evil.example"})
        assert r.status == 403
        r = await s.post(f"{sim.url}api/mailbox", data=b"not an app")
        assert r.status == 400
    assert not any("fake: write-region" in line for line in sim.printed())
