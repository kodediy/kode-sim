//! The AMOLED (CO5300, 410 x 502) on the P4's MIPI-DSI link, as esp-emu
//! 1.1.0 hands it over `--cosim-tcp`: the commands the firmware sends the
//! panel, the video stream's format, and the rows of each frame that
//! changed. The firmware's own `esp_lcd` path drives it, unchanged.
//!
//! What the page needs from it is what kemu's stand-in sent over UART1: the
//! picture as FB_RECT bands, and the panel's state as PANEL (lit, and the
//! brightness in percent), which the DCS commands set - DISPON/DISPOFF,
//! SLPOUT/SLPIN and WRDISBV (0x51).

use cosim::{DpiPixels, Dsi};

use crate::proto;

use crate::bridge::{H, W};

const DCS_SLPIN: u8 = 0x10;
const DCS_SLPOUT: u8 = 0x11;
const DCS_DISPOFF: u8 = 0x28;
const DCS_DISPON: u8 = 0x29;
const DCS_WRDISBV: u8 = 0x51;

/// What the panel shows the page: a frame for the bridge, as kemu sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    /// An FB_RECT payload: x, y, w, h (u16 LE), then RGB565 LE pixels.
    Rect(Vec<u8>),
    /// A PANEL payload: lit (0/1), brightness in percent.
    Panel([u8; 2]),
}

#[derive(Debug)]
pub struct Panel {
    /// The stream's format, once it is one the page can draw.
    format: Option<DpiPixels>,
    on: bool,
    awake: bool,
    brightness: u8,
}

impl Default for Panel {
    fn default() -> Self {
        Panel { format: None, on: false, awake: true, brightness: 0 }
    }
}

impl Panel {
    fn state(&self) -> Shown {
        Shown::Panel([(self.on && self.awake) as u8, self.brightness])
    }

    pub fn event(&mut self, e: Dsi) -> Option<Shown> {
        match e {
            Dsi::Command { dt, data } => self.command(dt, &data),
            Dsi::Format { width, height, pixels, .. } => {
                // Another shape is nothing the page has a place for.
                let drawable = width as usize == W
                    && height as usize == H
                    && matches!(pixels, DpiPixels::Rgb565 | DpiPixels::Bgr888);
                self.format = drawable.then_some(pixels);
                None
            }
            Dsi::Rows { y, rows, pixels, .. } => self.rows(y as usize, rows as usize, &pixels),
        }
    }

    /// A DCS write (short, with or without its parameter, or long) that
    /// changes what is seen; the CO5300's other commands change nothing.
    fn command(&mut self, dt: u8, data: &[u8]) -> Option<Shown> {
        if !matches!(dt, 0x05 | 0x15 | 0x39) {
            return None;
        }
        match *data.first()? {
            DCS_DISPOFF => self.on = false,
            DCS_DISPON => self.on = true,
            DCS_SLPIN => self.awake = false,
            DCS_SLPOUT => self.awake = true,
            DCS_WRDISBV => self.brightness = ((*data.get(1)? as u32 * 100 + 127) / 255) as u8,
            _ => return None,
        }
        Some(self.state())
    }

    fn rows(&self, y: usize, rows: usize, pixels: &[u8]) -> Option<Shown> {
        let format = self.format?;
        if rows == 0 || y + rows > H {
            return None;
        }
        let mut p = Vec::with_capacity(8 + W * rows * 2);
        for v in [0, y, W, rows] {
            p.extend_from_slice(&(v as u16).to_le_bytes());
        }
        match format {
            DpiPixels::Rgb565 => p.extend_from_slice(pixels.get(..W * rows * 2)?),
            _ => {
                for px in pixels.get(..W * rows * 3)?.as_chunks::<3>().0 {
                    let (b, g, r) = (px[0] as u16, px[1] as u16, px[2] as u16);
                    let v = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
                    p.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        Some(Shown::Rect(p))
    }
}

/// The frame a [`Shown`] goes to the page as.
pub fn frame(s: &Shown) -> (u8, Vec<u8>) {
    match s {
        Shown::Rect(p) => (proto::FB_RECT, p.clone()),
        Shown::Panel(p) => (proto::PANEL, p.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(pixels: DpiPixels) -> Dsi {
        Dsi::Format { streaming: true, width: W as u32, height: H as u32, pixels }
    }

    #[test]
    fn the_bring_up_lights_the_panel_at_the_brightness_it_was_given() {
        let mut p = Panel::default();
        // kode-sdk's bring-up, as esp-emu 1.1.0 relayed it.
        assert_eq!(p.event(Dsi::Command { dt: 0x15, data: vec![0x3a, 0x55] }), None);
        assert_eq!(p.event(Dsi::Command { dt: 0x05, data: vec![0x11, 0] }), Some(Shown::Panel([0, 0])));
        assert_eq!(p.event(Dsi::Command { dt: 0x05, data: vec![0x29, 0] }), Some(Shown::Panel([1, 0])));
        assert_eq!(p.event(Dsi::Command { dt: 0x15, data: vec![0x51, 0xff] }), Some(Shown::Panel([1, 100])));
        assert_eq!(p.event(Dsi::Command { dt: 0x05, data: vec![0x10, 0] }), Some(Shown::Panel([0, 100])));
        // A read asks; it changes nothing.
        assert_eq!(p.event(Dsi::Command { dt: 0x06, data: vec![0x0a, 0] }), None);
    }

    #[test]
    fn rows_become_a_full_width_band() {
        let mut p = Panel::default();
        let rows = |y| Dsi::Rows { frame: 1, y, rows: 2, pixels: vec![0xab; W * 2 * 2] };
        assert_eq!(p.event(rows(0)), None, "no format yet");
        p.event(fmt(DpiPixels::Rgb565));
        let Some(Shown::Rect(r)) = p.event(rows(500)) else { panic!("no band") };
        assert_eq!(&r[..8], &[0, 0, 0xf4, 1, 0x9a, 1, 2, 0]);
        assert_eq!(r.len(), 8 + W * 2 * 2);
        assert_eq!(p.event(rows(501)), None, "past the last row");
    }

    #[test]
    fn bgr888_is_drawn_as_rgb565() {
        let mut p = Panel::default();
        p.event(fmt(DpiPixels::Bgr888));
        let mut px = vec![0u8; W * 3];
        px[..3].copy_from_slice(&[0x00, 0x00, 0xff]); // red
        let Some(Shown::Rect(r)) = p.event(Dsi::Rows { frame: 1, y: 0, rows: 1, pixels: px }) else { panic!() };
        assert_eq!(&r[8..10], &0xf800u16.to_le_bytes());
    }

    #[test]
    fn a_stream_of_another_size_is_not_drawn() {
        let mut p = Panel::default();
        p.event(Dsi::Format { streaming: true, width: 320, height: 240, pixels: DpiPixels::Rgb565 });
        assert_eq!(p.event(Dsi::Rows { frame: 1, y: 0, rows: 1, pixels: vec![0; 640] }), None);
    }
}
