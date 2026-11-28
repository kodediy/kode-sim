//! The Kode Dot in a window of its own: Chrome or Edge (or another Chromium)
//! in app mode - no tabs, no address bar - with a profile kept in the cache,
//! so the window is its own process and its flags hold: the sound plays and
//! the microphone opens without a click or a prompt. Without such a browser,
//! the page opens in the default one, as a tab.

use std::path::{Path, PathBuf};

/// The browsers tried, in order, when `$KODE_EMU_BROWSER` names none.
fn candidates() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        let apps = ["Google Chrome", "Microsoft Edge", "Chromium", "Brave Browser"];
        let mut dirs = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join("Applications"));
        }
        dirs.iter().flat_map(|d| apps.iter().map(move |a| d.join(format!("{a}.app/Contents/MacOS/{a}")))).collect()
    } else {
        let names = [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ];
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).flat_map(|d| names.iter().map(move |n| d.join(n))).collect())
            .unwrap_or_default()
    }
}

/// The browser that opens the window: `$KODE_EMU_BROWSER`, or the first
/// Chromium found.
pub fn browser() -> Option<PathBuf> {
    match std::env::var_os("KODE_EMU_BROWSER") {
        Some(b) if !b.is_empty() => Some(PathBuf::from(b)),
        _ => candidates().into_iter().find(|c| c.is_file()),
    }
}

/// How the browser is started: the page as an app, in its own profile.
pub fn app_args(url: &str, profile: &Path) -> Vec<String> {
    vec![
        format!("--app={url}"),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--autoplay-policy=no-user-gesture-required".into(),
        "--use-fake-ui-for-media-stream".into(),
        "--window-size=460,780".into(),
    ]
}

/// Open the window; the browser's process when there is one to wait on
/// (dropped, it closes the window: kode-emu stopped takes its window along),
/// None when the page went to the default browser (or nowhere).
pub fn open(url: &str) -> Option<tokio::process::Child> {
    if let Some(b) = browser() {
        let profile = crate::emu::cache_dir().join("window");
        let spawned = tokio::process::Command::new(&b)
            .args(app_args(url, &profile))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        match spawned {
            Ok(child) => return Some(child),
            Err(e) => eprintln!("kode-emu: could not start {}: {e}; opening the page as a tab", b.display()),
        }
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_an_app_in_a_profile_of_its_own() {
        let a = app_args("http://127.0.0.1:8765/", Path::new("/c/window"));
        assert_eq!(a[0], "--app=http://127.0.0.1:8765/");
        assert_eq!(a[1], "--user-data-dir=/c/window");
        assert!(a.contains(&"--autoplay-policy=no-user-gesture-required".to_string()));
        assert!(a.contains(&"--use-fake-ui-for-media-stream".to_string()));
    }
}
