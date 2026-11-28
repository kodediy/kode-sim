import asyncio

import pytest

import kd
from emu_helpers import boot, console_line, firmware, frame_where, no_panic

pytestmark = pytest.mark.emu


@pytest.fixture(scope="module")
def buttons_led(exe, kode_sdk):
    return firmware(exe, kode_sdk / "examples" / "buttons_led", sdk=kode_sdk)


def is_led(rgb):
    return lambda t, p: t == kd.LED and p == bytes(rgb)


def lit(channel):
    """An LED frame with only @channel (0 r, 1 g, 2 b) lit."""
    return lambda t, p: (t == kd.LED and p[channel] > 0
                         and all(p[i] == 0 for i in range(3) if i != channel))


async def click(k, mask):
    """A press and a release at a person's pace."""
    await k.send(kd.BUTTONS, bytes([mask]))
    await asyncio.sleep(0.15)
    await k.send(kd.BUTTONS, bytes([0]))
    await asyncio.sleep(0.4)


async def test_top_button_swaps_led(exe, buttons_led, tmp_path):
    k = await boot(exe, tmp_path, buttons_led)
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await frame_where(k, lit(1), 60)                      # starts green
        await click(k, kd.BTN_TOP)
        await frame_where(k, lit(0), 10)                      # now red
        await click(k, kd.BTN_TOP)
        await frame_where(k, lit(1), 10)                      # green again
        await asyncio.sleep(3)
        no_panic(k)
    finally:
        await k.stop()


async def test_power_chips_found(exe, buttons_led, tmp_path):
    k = await boot(exe, tmp_path, buttons_led)
    try:
        # The charger's own line is a debug one since kode-sdk 0.1.1: the policy
        # is what kaos.power writes into it, so it answered.
        await console_line(k, r"kaos\.power policy applied: .*charging at", 60)
        await console_line(k, r"kaos\.park the ES8311 parked", 30)   # the boot's probes are done
        bad = [l for l in k.console if "not a CW221x" in l or "not an AW8623X" in l
               or "not a bq25896" in l]
        assert not bad, bad
        no_panic(k)
    finally:
        await k.stop()


async def test_a_battery_sent_in_reply_to_hello_is_kept(exe, buttons_led, tmp_path):
    """The page answers HELLO with the battery; the gauge must keep it."""
    k = await boot(exe, tmp_path, buttons_led)
    try:
        await frame_where(k, lambda t, p: t == kd.HELLO, 30)
        await k.send(kd.BATTERY, bytes([15, 0]))
        await console_line(k, r"kaos\.power", 60)
        await asyncio.sleep(3)
        await k.console_in("bat_info")
        line = await console_line(k, r"soc_x256=\d+", 20)
        assert "soc_x256=3840" in line, line
    finally:
        await k.stop()


async def test_the_same_binary_as_the_board_runs_with_the_board_s_chips(exe, kode_sdk, tmp_path):
    """kode-emu's goal: the image a Kode Dot runs, unchanged - no kemu in it -
    on esp-emu with the board's chips beside it. The LED starts green, and
    TOP, a P4 pin the chips drive, turns it red."""
    fw = firmware(exe, kode_sdk / "examples" / "buttons_led", sdk=kode_sdk, board=True)
    assert b"kemu_rx" not in fw["app_bin"].read_bytes()
    k = await boot(exe, tmp_path, fw)
    try:
        await frame_where(k, lit(1), 60)                      # starts green
        await click(k, kd.BTN_TOP)
        await frame_where(k, lit(0), 10)                      # now red
        no_panic(k)
    finally:
        await k.stop()
