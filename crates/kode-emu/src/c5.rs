//! The Kode Dot's radio: an ESP32-C5 running Kode's own firmware,
//! kode_slave_c5 (esp-hosted's slave with the Kode services), in an esp-emu
//! of its own beside the P4's. esp-emu 1.1.0 bridges the two over the SDIO
//! link (`--hosted`), so the firmware's real esp-hosted path - the link, the
//! scans, the joins, the vault, BLE - runs as on the board.
//!
//! The image is built from source, once, at the release kode-emu pins: esp-emu
//! needs its ELF to find the Wi-Fi blob's entry points, which the
//! c5_slave.bin a KodeOS release carries comes without.

use std::path::{Path, PathBuf};

use crate::build::{idf, idf_env, BuildError};

/// kode_slave_c5's release: the one kode-sdk pairs with esp_hosted 2.12.9.
pub const C5_VERSION: &str = "v0.1.2";
const C5_REPO: &str = "https://github.com/kodediy/kode_slave_c5.git";

/// The network the C5 sees: esp-emu's soft AP, open, behind the Mac's
/// connection.
pub const SSID: &str = "kode-emu";

/// A built C5 image: the flash esp-emu boots and the ELF beside it.
#[derive(Clone, Debug)]
pub struct C5Image {
    pub flash: PathBuf,
    pub elf: PathBuf,
}

fn image_in(dir: &Path) -> C5Image {
    C5Image { flash: dir.join("merged-binary.bin"), elf: dir.join("kode_c5.elf") }
}

/// The C5's image: `$KODE_EMU_C5` (a folder with merged-binary.bin and
/// kode_c5.elf), or kode_slave_c5 at [`C5_VERSION`], built into the tools
/// folder the first time.
pub fn ensure() -> Result<C5Image, BuildError> {
    if let Some(dir) = std::env::var_os("KODE_EMU_C5").filter(|p| !p.is_empty()) {
        let img = image_in(Path::new(&dir));
        if !img.flash.is_file() || !img.elf.is_file() {
            return Err(BuildError(format!(
                "$KODE_EMU_C5 is {}, which has no merged-binary.bin and kode_c5.elf",
                Path::new(&dir).display()
            )));
        }
        return Ok(img);
    }
    let dir = crate::emu::tools_dir().join(format!("kode_c5-{C5_VERSION}"));
    let img = image_in(&dir);
    if img.flash.is_file() && img.elf.is_file() {
        return Ok(img);
    }
    std::fs::create_dir_all(&dir).map_err(|e| BuildError(format!("could not make {}: {e}", dir.display())))?;
    // One build at a time: a second simulator started meanwhile waits for
    // it and takes what it made. The lock goes with the file, and with the
    // process if it is stopped half-way.
    let lock = std::fs::File::create(dir.join(".lock")).map_err(|e| BuildError(e.to_string()))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(BuildError(format!("could not lock {}", dir.display())));
    }
    if img.flash.is_file() && img.elf.is_file() {
        return Ok(img);
    }
    eprintln!("kode-emu: building the C5's firmware (kode_slave_c5 {C5_VERSION}), once ...");
    let src = dir.join("src");
    if !src.join("CMakeLists.txt").is_file() {
        let _ = std::fs::remove_dir_all(&src);
        let ok = std::process::Command::new("git")
            .args(["-c", "advice.detachedHead=false", "clone", "-q", "--depth", "1", "--branch", C5_VERSION, C5_REPO])
            .arg(&src)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return Err(BuildError(format!("could not fetch kode_slave_c5 {C5_VERSION} from {C5_REPO}")));
        }
    }
    let env = idf_env()?;
    let log = dir.join("build.log");
    let build = dir.join("build");
    let arg = |s: &str| s.to_string();
    let path = |p: &Path| p.display().to_string();
    let steps: [Vec<String>; 2] = [
        vec![arg("-C"), path(&src), arg("-B"), path(&build), arg("set-target"), arg("esp32c5")],
        vec![arg("-C"), path(&src), arg("-B"), path(&build), arg("build"), arg("merge-bin")],
    ];
    for (i, args) in steps.iter().enumerate() {
        if !idf(&env, args, &log, i > 0)? {
            return Err(BuildError(format!("the C5's firmware did not build: see {}", log.display())));
        }
    }
    // The ELF first and the flash last, each whole before it has its name:
    // the flash there is what says the image is.
    for (from, to) in [(build.join("kode_c5.elf"), &img.elf), (build.join("merged-binary.bin"), &img.flash)] {
        let new = to.with_extension("new");
        std::fs::copy(&from, &new).map_err(|e| BuildError(format!("{}: {e}", from.display())))?;
        std::fs::rename(&new, to).map_err(|e| BuildError(format!("{}: {e}", to.display())))?;
    }
    Ok(img)
}

/// The C5's flash for one run: `scratch`, a copy of the image; or, with
/// `kept`, that file (its vault, the networks it joined), seeded from the
/// image when there is none yet or the image is newer.
pub fn flash_for(img: &C5Image, kept: Option<&Path>, scratch: &Path) -> std::io::Result<PathBuf> {
    let Some(kept) = kept else {
        std::fs::copy(&img.flash, scratch)?;
        return Ok(scratch.to_path_buf());
    };
    if let Some(dir) = kept.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let stale = match std::fs::metadata(kept) {
        Ok(m) => std::fs::metadata(&img.flash)?.modified()? > m.modified()?,
        Err(_) => true,
    };
    if stale {
        std::fs::copy(&img.flash, kept)?;
    }
    Ok(kept.to_path_buf())
}
