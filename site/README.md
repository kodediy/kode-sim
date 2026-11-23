# site

The download page and the install scripts, meant for `sim.kode.diy`:

| File | Served at |
| --- | --- |
| `download/index.html`, `download/kode-dot.png` | `https://sim.kode.diy/download/` |
| `install-mac.sh` | `https://sim.kode.diy/install-mac.sh` |
| `install-linux.sh` | `https://sim.kode.diy/install-linux.sh` |
| `install-windows.ps1` | `https://sim.kode.diy/install-windows.ps1` |

Each script installs what kode-sim needs, then kode-sim itself and kodeOS.
It starts kodeOS in the simulator and opens the Kode Dot in Chrome. The pieces it installs are:

- the system packages;
- Rust;
- ESP-IDF v6.1, for esp32p4 and esp32c5.

It can run again: each step skips what is already there.
The Windows script installs WSL2 with Ubuntu, then runs the Linux script inside it.

Served from any other `https://` host, the page shows that host in its commands instead of `sim.kode.diy`, so a copy can be tried before it is published.

The page is a single file and needs no build. For a local look:

```bash
python3 -m http.server -d site 8000   # then open http://127.0.0.1:8000/download/
```
