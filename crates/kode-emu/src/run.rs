//! `kode-emu run`: boot a built firmware and serve the Kode Dot to the browser.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};

use crate::bridge::{Bridge, Config};
use crate::emu::{self, Launch};
use crate::{install, sdcard, web};

pub struct Args {
    pub firmware: PathBuf,
    pub elf: Option<PathBuf>,
    pub port: u16,
    pub open: bool,
    pub forwards: Vec<(u16, u16)>,
    pub wifi: bool,
    /// Nothing out but DNS.
    pub restrict: bool,
    pub sd: PathBuf,
    pub sd_in: bool,
    pub exit_on_crash: bool,
    pub save_state: bool,
}

/// `PORT` or `PORT:KODE_PORT`, for `--forward`.
pub fn parse_forward(text: &str) -> Result<(u16, u16), String> {
    let (host, guest) = text.split_once(':').unwrap_or((text, ""));
    let guest = if guest.is_empty() { host } else { guest };
    let bad = || format!("{text:?} is not PORT or PORT:KODE_PORT");
    let (h, g) = (host.parse::<u32>().map_err(|_| bad())?, guest.parse::<u32>().map_err(|_| bad())?);
    if !(1..65536).contains(&h) || !(1..65536).contains(&g) {
        return Err(format!("{text:?}: ports go from 1 to 65535"));
    }
    Ok((h as u16, g as u16))
}

/// The flash `run PROJECT --save-state` boots, and esp-emu writes back
/// into: flash.bin beside the build's out/ in the cache, not the build's
/// own merged image, which the next build makes again. Seeded from that
/// image when there is none yet, or when the build has changed since (its
/// SHA-1 is kept beside it, in flash.bin.sha1); kept otherwise, with what
/// was installed in it. In one simulator at a time: the lock that comes
/// with it is held for the run, as two would each write their flash back
/// over the other's (and seed it under the other's feet).
pub fn saved_flash(merged: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use sha1::{Digest, Sha1};
    use std::os::fd::AsRawFd;
    let work = merged.parent().and_then(Path::parent).unwrap_or(Path::new("."));
    let flash = work.join("flash.bin");
    let lock =
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(work.join("flash.bin.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::other(format!(
            "{} is in another simulator's hands (kode-emu run --save-state): stop that one first",
            flash.display()
        )));
    }
    let stamp = work.join("flash.bin.sha1");
    let built: String = Sha1::digest(std::fs::read(merged)?).iter().map(|b| format!("{b:02x}")).collect();
    let seeded = std::fs::read_to_string(&stamp).ok();
    if !flash.exists() || seeded.as_deref().map(str::trim) != Some(built.as_str()) {
        let fresh = work.join("flash.bin.new");
        std::fs::copy(merged, &fresh)?;
        std::fs::rename(&fresh, &flash)?;
        std::fs::write(&stamp, format!("{built}\n"))?;
    }
    Ok((flash, lock))
}

/// How long the window's page may be gone (a reload is well under it)
/// before the window counts as closed.
const WINDOW_GONE: std::time::Duration = std::time::Duration::from_secs(3);

/// The exit code: 0 when stopped (Ctrl-C, SIGTERM, SIGHUP), 2 when the
/// firmware crashed under `--exit-on-crash`, 1 when it could not start.
pub async fn run(a: Args) -> i32 {
    if !a.sd.exists() {
        eprintln!("kode-emu: making a card image at {} (1 GB, FAT32) ...", a.sd.display());
        if let Err(e) = sdcard::make_card(&a.sd, sdcard::DEFAULT_MB, sdcard::LABEL) {
            eprintln!("kode-emu: {e}");
            return 1;
        }
    }
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", a.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("kode-emu: could not serve the page on 127.0.0.1:{}: {e}", a.port);
            return 1;
        }
    };
    // The port it got: --port 0 leaves the choice to the Mac.
    let port = listener.local_addr().map(|l| l.port()).unwrap_or(a.port);
    let bridge = Bridge::new(Config {
        launch: Launch {
            firmware: a.firmware.clone(),
            elf: a.elf.clone(),
            forwards: a.forwards.clone(),
            restrict: a.restrict,
            save_state: a.save_state,
            sd: None,
            wifi: a.wifi,
        },
        http_port: port,
        sd: Some(a.sd.clone()),
        sd_in: a.sd_in,
    });
    // Not println!, which panics once the terminal's pipe is closed
    // (`kode-emu run | head`): those lines are lost there, and nowhere else.
    bridge.on_line(Box::new(|line| {
        let _ = writeln!(std::io::stdout().lock(), "{line}");
    }));
    let (state_tx, mut states) = mpsc::unbounded_channel();
    bridge.on_state(state_tx);

    let app = web::app(bridge.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut stop = stopped();
    let code = tokio::select! {
        started = bridge.start() => match started {
            Ok(()) => serve(&bridge, &a, &mut states, &mut stop).await,
            Err(e) => {
                eprintln!("kode-emu: {e}");
                1
            }
        },
        _ = stop.changed() => 0,
    };
    install::withdraw();
    server.abort();
    bridge.close().await;
    emu::stop_all().await;
    code
}

/// SIGTERM, SIGHUP (a closed terminal) and SIGINT (Ctrl-C), from the
/// start: each ends it the same way, the emulator stopped and the note taken
/// away, nothing left behind. `review` stops on them too.
pub fn stopped() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    let kinds = [SignalKind::terminate(), SignalKind::hangup(), SignalKind::interrupt()];
    let mut signals: Vec<_> = kinds.into_iter().filter_map(|k| signal(k).ok()).collect();
    tokio::spawn(async move {
        let any = futures_util::future::select_all(signals.iter_mut().map(|s| Box::pin(s.recv())));
        any.await;
        let _ = tx.send(true);
    });
    rx
}

async fn serve(
    bridge: &Arc<Bridge>,
    a: &Args,
    states: &mut mpsc::UnboundedReceiver<(String, String)>,
    stop: &mut watch::Receiver<bool>,
) -> i32 {
    let url = format!("http://127.0.0.1:{}/", bridge.cfg.http_port);
    let serial = bridge.serial_path().unwrap_or_default();
    eprintln!("kode-emu: the Kode Dot is at {url}  (Ctrl-C to stop)");
    eprintln!("kode-emu: its console is the serial port {serial}  (kcli.py -p {serial} ...)");
    eprintln!("kode-emu: install an app in it from another terminal: kode-emu install path/to/the/app");
    if !a.wifi {
        eprintln!("kode-emu: no WiFi - its C5 never comes up");
    }
    if a.restrict {
        eprintln!("kode-emu: its network lets nothing out but DNS");
    }
    eprintln!("kode-emu: its microSD is {}{}", a.sd.display(), if a.sd_in { "" } else { " (out of the slot for now)" });
    for (h, g) in &a.forwards {
        eprintln!("kode-emu: 127.0.0.1:{h} on this Mac reaches the Kode Dot's port {g}");
    }
    if let Err(e) = install::announce(&serial, &url) {
        eprintln!("kode-emu: could not leave a note for kode-emu install: {e}");
    }
    // The window's browser: when it goes, the window was closed, and so is
    // the simulator.
    let mut window = if a.open { crate::window::open(&url) } else { None };

    // The terminal is the device's console too. A thread of its own: a
    // read from a terminal cannot be abandoned, and the process exits
    // without waiting for it.
    // Run in the background of a shell (`kode-emu run &`), a read from the
    // terminal would stop the whole process (SIGTTIN): ignored, the read
    // fails with EIO instead, and is tried again until the job is in front.
    unsafe { libc::signal(libc::SIGTTIN, libc::SIG_IGN) };
    let typed = bridge.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break, // stdin closed: keep serving
                Ok(_) => typed.console_in(line.trim_end_matches(['\n', '\r'])),
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    std::thread::sleep(std::time::Duration::from_millis(500))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    // A browser that outlives its last window (Chrome on a Mac does) is told
    // closed by its page: gone, and not back within WINDOW_GONE.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut empty_since = None::<std::time::Instant>;
    loop {
        tokio::select! {
            _ = stop.changed() => return 0,
            _ = tick.tick(), if window.is_some() => {
                if bridge.page_count() > 0 || !bridge.ever_joined() {
                    empty_since = None;
                } else if empty_since.get_or_insert_with(std::time::Instant::now).elapsed() >= WINDOW_GONE {
                    eprintln!("kode-emu: its window was closed");
                    return 0;
                }
            }
            _ = async { window.as_mut().unwrap().wait().await }, if window.is_some() => {
                eprintln!("kode-emu: its window was closed");
                return 0;
            }
            Some((state, detail)) = states.recv() => {
                if state == "crashed" {
                    eprintln!("kode-emu: the firmware crashed - {}", detail.lines().next().unwrap_or(""));
                    if a.exit_on_crash {
                        return 2;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_s_saved_flash_outlives_its_next_build_until_the_build_changes() {
        let work = std::env::temp_dir().join(format!("kode-emu-test-{}-flash", std::process::id()));
        let out = work.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let merged = out.join("merged-binary.bin");
        std::fs::write(&merged, b"build one").unwrap();

        let (flash, lock) = saved_flash(&merged).unwrap();
        drop(lock);
        assert_eq!(flash, work.join("flash.bin")); // beside the build, not in it
        assert_eq!(std::fs::read(&flash).unwrap(), b"build one"); // seeded from it
        std::fs::write(&flash, b"build one, an app installed").unwrap(); // esp-emu wrote it back

        std::fs::write(&merged, b"build one").unwrap(); // built again, the same
        assert_eq!(std::fs::read(saved_flash(&merged).unwrap().0).unwrap(), b"build one, an app installed");

        std::fs::write(&merged, b"build two").unwrap(); // the build changed: a new flash
        assert_eq!(std::fs::read(saved_flash(&merged).unwrap().0).unwrap(), b"build two");
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_saved_flash_is_in_one_simulator_at_a_time() {
        let work = std::env::temp_dir().join(format!("kode-emu-test-{}-flashlock", std::process::id()));
        let out = work.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let merged = out.join("merged-binary.bin");
        std::fs::write(&merged, b"build one").unwrap();
        let (flash, held) = saved_flash(&merged).unwrap();
        std::fs::write(&flash, b"build one, an app installed").unwrap();
        std::fs::write(&merged, b"build two").unwrap(); // rebuilt while the first still runs
        let e = saved_flash(&merged).unwrap_err().to_string();
        assert!(e.contains("flash.bin") && e.contains("--save-state"), "{e}");
        assert_eq!(std::fs::read(&flash).unwrap(), b"build one, an app installed"); // not seeded over
        drop(held);
        // Free once the first lets go - give a child another test forks,
        // which holds the descriptor until its exec, a moment to let go too.
        let again = (0..100)
            .find_map(|_| {
                saved_flash(&merged).ok().or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                })
            })
            .expect("the flash is free again");
        assert_eq!(std::fs::read(again.0).unwrap(), b"build two");
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_forward_is_a_port_or_two() {
        assert_eq!(parse_forward("25565"), Ok((25565, 25565)));
        assert_eq!(parse_forward("8080:80"), Ok((8080, 80)));
        assert_eq!(parse_forward("80:"), Ok((80, 80)));
        assert!(parse_forward("x").unwrap_err().contains("PORT"));
        assert!(parse_forward("0").unwrap_err().contains("1 to 65535"));
        assert!(parse_forward("70000").unwrap_err().contains("1 to 65535"));
        assert!(parse_forward("80:x").is_err());
    }
}
