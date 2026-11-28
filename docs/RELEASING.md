# Releasing kode-emu

1. **Host suites green:** `tools/host_tests.sh` locally, and the `ci`
   workflow on the pull request (Linux) and on main (Linux and the Mac).
2. **The emulator suite green, in full, on the Mac:**
   ```
   .venv/bin/pytest -m emu tests        # ~10 min on an M-series Mac
   ```
   It builds kode-sdk's examples and kodeOS with ESP-IDF v6.1 and boots
   them in the pinned esp-emu through `kode-emu`: the chips, the screen,
   sound, the card, the network, `install`, `install --mailbox` and
   `review`.
3. **By hand, once:**
   - `kode-emu run` on KodeOS (kodediy/KodeOS), from a clean cache. Touch, the buttons,
     the LED, sound and the console on the page.
   - `kode-emu install` an app into it, then go home by the held bottom
     button.
4. **The version:** `version` in each `crates/*/Cargo.toml`, and a new
   section at the top of `CHANGELOG.md` with the date.
5. **Merge to main, tag, release:**
   ```
   git tag -a vX.Y.Z -m "kode-emu X.Y.Z"
   git push origin vX.Y.Z
   gh release create vX.Y.Z --title "kode-emu X.Y.Z" --notes-file <that CHANGELOG section>
   ```
   CI runs again on the tag.

kode-emu is installed from its checkout (`cargo build --release`), so a
release is a tag and its notes, not a binary to download.
