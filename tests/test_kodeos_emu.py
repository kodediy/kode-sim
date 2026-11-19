"""kodeOS - kodediy/KodeOS, the newest, with the kode-sdk it pins - in the simulator."""

import asyncio
import json
import mmap
import re
import os
import subprocess
import time
import zlib
from pathlib import Path

import pytest

import kd
from emu_helpers import boot, clock_runs, console_line, console_ready, firmware, frame_where, install, no_panic
from screen import Screen
from test_io_emu import click
from test_panel_emu import collect

pytestmark = pytest.mark.emu

RADIO = Path(__file__).parent / "fixtures" / "radio"
# kodeOS booting, in whichever line names it first: ESP-IDF's app_init (at an
# info level KodeOS keeps quiet), kode-sdk's boot line, or esp-emu's loader.
KODEOS_BOOTS = r"Project name:\s+kodeos|\bkode kodeos \S+ \(|\bApp kodeos \S+ from flash offset"


@pytest.fixture(scope="module")
def kodeos(exe):
    """The newest kodeOS: kodediy/KodeOS at its default branch, with the
    kode-sdk it pins, brought up to date at every session (offline, it stays
    as it was). $KODEOS points at a checkout to test as it is."""
    from conftest import CACHE
    if os.environ.get("KODEOS"):
        return firmware(exe, Path(os.environ["KODEOS"]))
    src = CACHE / "KodeOS"
    git = ["git", "-c", "url.https://github.com/.insteadOf=git@github.com:"]
    if not src.exists():
        subprocess.run(git + ["clone", "-q", "--depth", "1", "--recurse-submodules", "--shallow-submodules",
                              "https://github.com/kodediy/KodeOS.git", str(src)], check=True)
    else:
        fetched = subprocess.run(git + ["-C", str(src), "fetch", "-q", "--depth", "1", "origin", "HEAD"],
                                 capture_output=True, text=True)
        if fetched.returncode == 0:
            subprocess.run(["git", "-C", str(src), "reset", "-q", "--hard", "FETCH_HEAD"], check=True)
            subprocess.run(git + ["-C", str(src), "submodule", "update", "-q", "--init", "--depth", "1"], check=True)
        else:
            print(f"KodeOS: could not fetch, testing {src} as it is: {fetched.stderr.strip()}")
    return firmware(exe, src)


@pytest.fixture(scope="module")
def display_lvgl(exe, kode_sdk):
    return firmware(exe, kode_sdk / "examples" / "display_lvgl", sdk=kode_sdk)


@pytest.fixture(scope="module")
def radio(exe, kode_sdk):
    return firmware(exe, RADIO, sdk=kode_sdk)


def heard_from(k):
    """The console lines from now on, and a wait for one: the lines up to the
    one matched are used up, so the next wait is for a later line."""
    heard: list[str] = []
    k.on_line.append(heard.append)

    async def until(pattern, timeout=60):
        loop = asyncio.get_running_loop()
        end = loop.time() + timeout
        while loop.time() < end:
            for i, l in enumerate(heard):
                if re.search(pattern, l):
                    del heard[:i + 1]
                    return l
            await asyncio.sleep(0.05)
        raise TimeoutError(f"no {pattern!r}; last:\n" + "\n".join(k.console[-30:]))
    return heard, until


async def put_file(k, until, path, data):
    """What kcli's file-put does: `file_put`, then the raw bytes a chunk per
    >|ack over the serial port, and kodeOS checks the CRC."""
    await k.console_in(f'file_put path="{path}" size={len(data)} crc32={zlib.crc32(data)}')
    ready = await until(r"^>\|ready chunk=\d+|^>err")
    assert ready.startswith(">|ready"), ready
    step = int(re.search(r"chunk=(\d+)", ready).group(1))
    for at in range(0, len(data), step):
        await k.console_bytes(data[at:at + step])
        if at + step <= len(data):
            ack = await until(r"^>\|ack|^>err")
            assert ack.startswith(">|ack"), ack
    done = await until(r"^>ok|^>err", 30)
    assert done.startswith(">ok"), done


async def test_kodeos_reaches_its_ui(exe, kodeos, tmp_path):
    k = await boot(exe, tmp_path, kodeos)
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        screen = Screen()
        await collect(k, screen, 25)
        assert screen.lit_fraction() > 0.05, f"{screen.rects} rects, {screen.lit_fraction():.3f} lit"
        before = bytes(screen.pixels)
        await click(k, kd.BTN_BOT)
        await collect(k, screen, 5)
        assert bytes(screen.pixels) != before, "BOT changed nothing on the screen"
        no_panic(k)
    finally:
        await k.stop()


async def test_kodeos_battery_and_power_off(exe, kodeos, tmp_path):
    k = await boot(exe, tmp_path, kodeos)
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await k.send(kd.BATTERY, bytes([15, 0]))
        k.backlog.clear()
        await asyncio.sleep(8)
        await k.console_in("bat_info")
        line = await console_line(k, r"soc_x256=\d+", 20)
        assert int(re.search(r"soc_x256=(\d+)", line).group(1)) // 256 == 15, line
        # The charger's ship mode, as the board's off: KodeOS's `sleep` and
        # `power_off` are AO_LowPower's, which the simulator's build has not.
        await k.console_in("ship_mode")
        await frame_where(k, lambda t, p: t == kd.POWER_OFF, 20)
    finally:
        await k.stop()


async def test_kodeos_left_alone_stays_alive(exe, kodeos, tmp_path):
    """Untouched for minutes - its animation over, both cores idle - kodeOS
    is still running, and its clock with it: every QTimeEvt (debounce, the
    touch poll, the rest, the console's pacing) hangs on that one tick.
    (On a first boot "Set the time" holds the rest off, so this is the idle,
    not the rest.)"""
    k = await boot(exe, tmp_path, kodeos)
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await asyncio.sleep(20)
        assert await clock_runs(k), "QP's clock is not running after boot"
        for _ in range(18):
            await asyncio.sleep(10)
            k.backlog.clear()                      # the screen is not what this is about
            assert k.alive(), "esp-emu died:\n" + "\n".join(k.console[-30:])
            assert await clock_runs(k), "QP's clock stopped:\n" + "\n".join(k.console[-30:])
        no_panic(k)
    finally:
        await k.stop()


async def test_kodeos_takes_a_binary_payload_over_the_console(exe, kodeos, tmp_path):
    """What kcli's app-install and file-put do, over the serial port: every
    byte value is in it, newlines and NULs among them."""
    k = await boot(exe, tmp_path, kodeos)
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await console_ready(k)
        _, until = heard_from(k)
        await put_file(k, until, "/storage/payload.bin", bytes(range(256)) * 40)
        no_panic(k)
    finally:
        await k.stop()


async def test_an_app_goes_home_to_kodeos_by_the_held_button_and_by_reset(exe, kodeos, display_lvgl, tmp_path):
    """kode-sim install's conversation with the real kodeOS: the app goes into
    the apps region and kodeOS reboots into it. Then the two ways home a
    person has on the board: BOT held (the SDK's CONFIG_KODEOS_HOME_HOLD) and
    the reset (the page's Reset) - after which kodeOS boots, the app still
    installed. Both hang on the bootloader having retired the one-shot
    request before jumping into the app."""
    k = await boot(exe, tmp_path, kodeos)
    heard, until = heard_from(k)

    async def install_and_boot_it():
        heard.clear()
        r = await asyncio.to_thread(install, exe, k, display_lvgl["app_bin"])
        assert r.returncode == 0 and "installed" in r.stderr, r.stderr
        await until("LVGL running")
        heard.clear()

    try:
        await console_ready(k)
        await install_and_boot_it()
        await k.send(kd.BUTTONS, bytes([kd.BTN_BOT]))
        await asyncio.sleep(1.0)
        await k.send(kd.BUTTONS, bytes([0]))
        await until(KODEOS_BOOTS)

        await install_and_boot_it()
        reset_at = len(k.console)
        await k.send_json({"power": "reset"})
        await console_line(k, KODEOS_BOOTS, 60, since=reset_at)
        await console_line(k, r"kdrv\.apps apps region ready: 1 app", 60, since=reset_at)
        assert k.alive(), k.state
    finally:
        await k.stop()


async def test_the_way_home_keeps_the_glass_lit(exe, kodeos, display_lvgl, tmp_path):
    """An app going home to kodeOS keeps its last frame on the glass across
    the reset (kdrv_display_keep_for_restart): the video stops in the
    vertical blanking, which kdrv_display hears as on_vsync. A panel that
    never says vsync makes it give up and reset the panel instead."""
    k = await boot(exe, tmp_path, kodeos)
    heard, until = heard_from(k)
    try:
        await console_ready(k)
        r = await asyncio.to_thread(install, exe, k, display_lvgl["app_bin"])
        assert r.returncode == 0 and "installed" in r.stderr, r.stderr
        await until("LVGL running")
        await asyncio.sleep(2)
        home_at = len(k.console)
        await k.send(kd.BUTTONS, bytes([kd.BTN_BOT]))
        await asyncio.sleep(1.0)
        await k.send(kd.BUTTONS, bytes([0]))
        line = await until(r"kaos\.display the restart: the glass")
        assert "stays lit" in line, "\n".join(l for l in k.console if "vsync" in l or "kdrv.display" in l)
        await console_line(k, KODEOS_BOOTS, 60, since=home_at)
        k.backlog.clear()
        await console_line(k, r"kdrv\.apps apps region ready", 60, since=home_at)
        screen = Screen()
        await collect(k, screen, 15)
        # The launcher after an app, a few icons on black: some 3 % lit.
        assert screen.lit_fraction() > 0.01, "kodeOS did not draw on the glass it kept"
        assert not any("vsync" in l for l in k.console), [l for l in k.console if "vsync" in l]
        no_panic(k)
    finally:
        await k.stop()


async def join_kode_sim(k):
    """The C5 up, join the one network it sees, as a person would once: the
    vault of a C5 that starts blank holds none."""
    await console_line(k, r"radio radio link=2\b", 90)
    await k.console_in('wifi_join ssid="kode-sim"')


async def test_an_app_gets_an_address_and_its_server_is_reachable_from_the_mac(exe, radio, tmp_path):
    """WiFi in the simulator: the C5 (Kode's firmware, in an esp-emu of its
    own) over ESP-Hosted, on esp-emu's soft AP. The app asks for WiFi and
    hears what a board on a network says - RADIO_STATE_SIG UP (2),
    WIFI_STATE_SIG GOT_IP (2) with its address - and a TCP server it opens is
    reached from the Mac through the port `kode-sim run --forward` opens."""
    port = kd.free_port()
    k = await boot(exe, tmp_path, radio, "--forward", f"{port}:7777")
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await join_kode_sim(k)
        await console_line(k, r"radio wifi state=2 ip=192\.168\.4\.\d+ enabled=1", 30)
        await console_line(k, r"radio echo listening on 7777", 20)
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        writer.write(b"hola kode\n")
        await writer.drain()
        assert await asyncio.wait_for(reader.readline(), 10) == b"hola kode\n"
        writer.close()
        no_panic(k)
    finally:
        await k.stop()


async def test_with_the_network_up_the_top_button_still_works(exe, radio, tmp_path):
    """TOP is a P4 pin (GPIO35) the board's chips drive. The network takes
    the SDIO pads and none of the others: TOP's pad stays a GPIO input
    (IO_MUX function 1, PIN_FUNC_GPIO), and an app with its network up still
    hears TOP pressed and released."""
    k = await boot(exe, tmp_path, radio)
    try:
        await join_kode_sim(k)
        await console_line(k, r"radio wifi state=2 ip=192\.168\.4\.\d+", 30)
        pad = await console_line(k, r"radio top pad fun_sel=", 10)
        assert "fun_sel=1 ie=1 oe=0" in pad, pad
        await asyncio.sleep(1)
        await click(k, kd.BTN_TOP)
        await console_line(k, r"radio button id=4 pressed=1", 10)
        await console_line(k, r"radio button id=4 pressed=0", 10)
        no_panic(k)
    finally:
        await k.stop()


async def test_with_the_c5_dead_an_app_hears_the_radio_failed(exe, radio, tmp_path):
    """`kode-sim run --no-wifi`: a board whose C5 never comes up. The app
    hears KRADIO_FAILED (4) and WIFI_STATE_SIG with no address, the wish kept."""
    k = await boot(exe, tmp_path, radio, "--no-wifi")
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await console_line(k, r"radio up; asked for the radio", 60)
        await console_line(k, r"radio radio link=4\b", 20)
        await console_line(k, r"radio wifi state=0 ip=0\.0\.0\.0 enabled=1", 20)
        no_panic(k)
    finally:
        await k.stop()


async def test_kodeos_mounts_the_card_image_and_a_file_lands_in_it(exe, kodeos, tmp_path):
    """The microSD: esp-emu's SDMMC host with a card image on the Mac in slot
    0 (`kode-sim run --sd`, made as a Mac formats a card). kodeOS mounts it
    as it would a card, and a file put on /sdcard is in the image afterwards."""
    img = tmp_path / "sd" / "card.img"
    k = await boot(exe, tmp_path, kodeos, "--sd", str(img))
    _, until = heard_from(k)
    data = b"KODE-SIM-SD " * 400 + b"\n"
    try:
        mounted = await until(r"kdrv\.sd card mounted at /sdcard")
        assert "4-bit" in mounted, mounted              # as the board's slot is wired
        await until(r"kaos\.storage sd mounted at /sdcard")
        await put_file(k, until, "/sdcard/hello.txt", data)
    finally:
        await k.stop()
    with open(img, "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
        assert m.find(data) >= 0, "the file is not in the card image"


async def test_an_app_that_waits_for_the_radio_finds_its_card_mounted(exe, radio, tmp_path):
    """On the board AO_Radio has AO_Storage mount the card around the link's
    lifetime, so an app that asks about the card once the radio has settled
    (KodeCraft does) finds it mounted."""
    k = await boot(exe, tmp_path, radio, "--sd", str(tmp_path / "sd" / "card.img"))
    try:
        line = await console_line(k, r"radio sd mounted=", 60)
        assert "mounted=1" in line, "\n".join(k.console[-30:])
    finally:
        await k.stop()


async def test_kodeos_boots_with_the_clock_set_and_no_network(exe, kodeos, tmp_path):
    """With no network out to ask the time of, a Kode Dot keeps it in its
    P4's RTC. kode-sim sets that RTC from the Mac's clock at every boot, so
    kodeOS boots knowing the time instead of asking for it."""
    k = await boot(exe, tmp_path, kodeos, "--no-internet")
    try:
        await console_line(k, r"kdrv\.apps apps region ready", 60)
        assert not any("the clock is not set" in l for l in k.console), "\n".join(k.console[-30:])
        heard, _ = heard_from(k)
        await k.console_in("time")
        loop = asyncio.get_running_loop()
        end = loop.time() + 10
        while loop.time() < end and not any(l.startswith(">|time") for l in heard):
            await asyncio.sleep(0.1)
        said = next((l for l in heard if l.startswith(">|time")), "")
        assert "state=known" in said and "source=rtc" in said, said
        assert f'utc="{time.gmtime().tm_year}-' in said, said
    finally:
        await k.stop()


async def test_review_puts_a_real_app_through_kodeos(exe, kodeos, display_lvgl, tmp_path):
    """kode-sim review end to end: kodeOS boots, the app is installed and
    boots, the scenario plays, BOT held brings kodeOS back, nothing crashed."""
    os_dir = tmp_path / "kodeos"
    os_dir.mkdir()
    (os_dir / "merged-binary.bin").symlink_to(kodeos["merged_bin"])
    (os_dir / "kodeos.elf").symlink_to(kodeos["elf"])
    scenario = tmp_path / "scenario.json"
    scenario.write_text(json.dumps({"steps": [
        {"expect_console": "LVGL running", "timeout_s": 30},
        {"button": "right"},
        {"screenshot": "after_right"},
    ]}))
    out = tmp_path / "out"
    from sim import env
    r = await asyncio.to_thread(subprocess.run, [str(exe), "review", str(display_lvgl["app_bin"]), str(scenario),
                                                 "--out", str(out), "--kodeos", str(os_dir)],
                                env=env(tmp_path / "cache"), capture_output=True, text=True, timeout=600)
    v = json.loads((out / "verdict.json").read_text())
    assert r.returncode == 0 and v["ok"], json.dumps(v, indent=2) + r.stderr[-2000:]
    assert [c["name"] for c in v["checks"]] == ["kodeos_boots", "installs", "app_starts",
                                                "expect_console: LVGL running", "home_by_hold", "no_crash"]
    for name in ("kodeos.png", "app.png", "after_right.png", "home.png"):
        assert (out / name).stat().st_size > 1000, name


def test_kodeos_is_built_without_the_link_rest(kodeos):
    """kodeOS's link rest reads the panel back over raw DSI packets on the
    wake, which esp-emu cannot answer: sdkconfig.emu turns it off, and the
    light sleep that hangs on it with it."""
    cfg = (kodeos["elf"].parent / "config" / "sdkconfig.h").read_text()
    assert "CONFIG_KODEOS_DISPLAY_SLEEP_LINK 1" not in cfg
    assert "CONFIG_KODEOS_LIGHT_SLEEP 1" not in cfg
