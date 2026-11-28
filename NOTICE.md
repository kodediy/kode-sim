# Licences of kode-emu

This file says which licence applies to each part of this repository. Read it before you distribute kode-emu, or a firmware that you built with it.

| Part | Owner | Licence |
|---|---|---|
| everything that this file does not name below | Kode | GPL-3.0-or-later |
| `web/kode-dot.png` and the screenshots in `.github/img/` | Kode | the pictures of the Kode Dot and of Kody: see the trademarks below |

## The code of Kode

Copyright (C) 2026 Kode.

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

The file `LICENSE` holds the full text of the GNU General Public License, version 3.

### Trademarks, under GNU GPL version 3 section 7(e)

This licence gives no right to use the names Kode, Kode Dot, kodeOS and Kody, or the character Kody, as trademarks. `TRADEMARKS.md` in the repository [`kodediy/.github`](https://github.com/kodediy/.github/blob/main/TRADEMARKS.md) says how you can use them.

## What kode-emu downloads

These are not in this repository, and each has its own licence:

- **esp-emu**, Espressif's emulator. kode-emu installs the pinned version on its first run with Espressif's installer, in its own cache. Its terms are Espressif's ([espressif/esp-emulator](https://github.com/espressif/esp-emulator)).
- **The C5's firmware**, [kode_slave_c5](https://github.com/kodediy/kode_slave_c5): kode-emu fetches the pinned release from GitHub on its first run and builds it with your ESP-IDF, in its own cache. It is Espressif's esp-hosted slave with Kode's changes, under Apache-2.0 (its `NOTICE.md`).
- **The Rust crates** in `Cargo.lock`, downloaded by `cargo build`. Each one says its licence on crates.io.
- **The firmware** that kode-emu builds and runs: [kode-sdk](https://github.com/kodediy/kode-sdk), [kodeOS](https://github.com/kodediy/KodeOS) and the apps. Their `NOTICE.md` files say which licence applies to each part. `firmware/kemu/` is linked into that firmware, under GPL-3.0-or-later like the rest of this repository.
