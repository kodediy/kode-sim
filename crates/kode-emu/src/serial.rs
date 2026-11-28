//! The console as a serial port: a pty standing where the board's
//! USB-Serial/JTAG would, so `kcli.py -p <it>` works as on the board.
//!
//! Bytes written to it are console input, raw; the console's lines come out
//! of it, each ended `\r\n`, whole: a line the port has no room for is
//! dropped, as on a cable nobody reads, and never glued to the next.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::Notify;

pub struct Serial {
    master: AsyncFd<OwnedFd>,
    /// Held open, so the master never reads EIO between two kcli runs.
    _slave: OwnedFd,
    pub path: String,
    /// The end of a line the pty took only the start of (it holds about
    /// 1 KB): it goes out before any other line, by [`Serial::flush`] as
    /// soon as there is room.
    rest: Mutex<Vec<u8>>,
    rest_waits: Notify,
    /// The lines dropped for want of room.
    dropped: AtomicU64,
}

/// As much of `data` as the pty takes now, without waiting.
fn write_now(fd: i32, data: &[u8]) -> usize {
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    n.max(0) as usize
}

impl Serial {
    pub fn open() -> io::Result<Serial> {
        let (mut m, mut s) = (0, 0);
        if unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }
            != 0
        {
            return Err(io::Error::last_os_error());
        }
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        // Bytes as they are: no echo, no CR/LF games.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(s, &mut t) == 0 {
                libc::cfmakeraw(&mut t);
                libc::tcsetattr(s, libc::TCSANOW, &t);
            }
            let flags = libc::fcntl(m, libc::F_GETFL);
            libc::fcntl(m, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let name = unsafe { libc::ttyname(s) };
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let path = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
        // SAFETY: an OwnedFd always answers the descriptor it owns, open
        // until the AsyncFd drops it.
        let master = unsafe { AsyncFd::register_with_interest(master, Interest::READABLE.add(Interest::WRITABLE)) }
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Serial {
            master,
            _slave: slave,
            path,
            rest: Mutex::new(vec![]),
            rest_waits: Notify::new(),
            dropped: AtomicU64::new(0),
        })
    }

    /// A console line out, whole or not at all, never waiting: with the
    /// pty full (nobody reading) it is dropped and counted. A line the pty
    /// took part of is finished before any other goes.
    pub fn write_line(&self, line: &str) {
        let fd = self.master.as_raw_fd();
        let mut rest = self.rest.lock().unwrap_or_else(PoisonError::into_inner);
        if !rest.is_empty() {
            let n = write_now(fd, &rest);
            rest.drain(..n);
            if !rest.is_empty() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let mut data = Vec::with_capacity(line.len() + 2);
        data.extend_from_slice(line.as_bytes());
        data.extend_from_slice(b"\r\n");
        let n = write_now(fd, &data);
        if n == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else if n < data.len() {
            *rest = data.split_off(n);
            self.rest_waits.notify_one();
        }
    }

    /// The lines dropped so far for want of room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Finish each line the pty took part of, as soon as it has room: run
    /// for as long as the port is open.
    pub async fn flush(&self) {
        loop {
            self.rest_waits.notified().await;
            loop {
                if self.rest.lock().unwrap_or_else(PoisonError::into_inner).is_empty() {
                    break;
                }
                let Ok(mut room) = self.master.writable().await else { return };
                let wrote = room.try_io(|fd| {
                    let mut rest = self.rest.lock().unwrap_or_else(PoisonError::into_inner);
                    let n = unsafe { libc::write(fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    rest.drain(..n as usize);
                    Ok(())
                });
                if let Ok(Err(_)) = wrote {
                    return; // the port is broken: nothing will get through
                }
            }
        }
    }

    /// The next bytes someone wrote to the port.
    pub async fn read(&self) -> io::Result<Vec<u8>> {
        loop {
            let mut guard = self.master.readable().await?;
            let mut buf = vec![0u8; 65536];
            let got = guard.try_io(|fd| {
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match got {
                Ok(Ok(n)) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// What is waiting on the port's other end, read as kcli would.
    fn drain(slave: &OwnedFd) -> Vec<u8> {
        let fd = slave.as_raw_fd();
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        let mut got = vec![];
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return got;
            }
            got.extend_from_slice(&buf[..n as usize]);
        }
    }

    #[tokio::test]
    async fn a_port_nobody_reads_drops_whole_lines_and_counts_them() {
        let s = Arc::new(Serial::open().unwrap());
        let flusher = {
            let s = s.clone();
            tokio::spawn(async move { s.flush().await })
        };
        // A pty takes about 1 KB on the Mac, some tens of KB on Linux; 1000
        // lines of 100 bytes fit in neither.
        let lines: Vec<String> = (0..1000).map(|i| format!("line {i:04} {}", "x".repeat(90))).collect();
        for l in &lines {
            s.write_line(l);
        }
        let mut got = vec![];
        for _ in 0..50 {
            got.extend(drain(&s._slave));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        flusher.abort();
        let text = String::from_utf8(got).unwrap();
        assert!(text.ends_with("\r\n"), "a line left half-written: {text:?}");
        let came: Vec<&str> = text.split_terminator("\r\n").collect();
        for l in &came {
            assert!(lines.iter().any(|x| x == l), "not a whole line: {l:?}");
        }
        assert!(s.dropped() > 0, "everything fitted: {} lines came", came.len());
        assert_eq!(came.len() as u64 + s.dropped(), lines.len() as u64);
        // In order, the dropped ones simply missing.
        let at: Vec<usize> = came.iter().map(|l| lines.iter().position(|x| x == l).unwrap()).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{at:?}");
    }
}
