//! Captura da tela inteira pelo X11 (`GetImage` na janela raiz).
//!
//! Vale para sessões X11 e, sob Wayland, só para janelas do XWayland: o Wayland não deixa um
//! programa ler a tela sem o portal do compositor (PipeWire), que ainda não está aqui.

use crate::video::{RawFrame, ScreenSource};
use thiserror::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("sem conexão com o servidor X11: {0}")]
    Connect(String),
    #[error("o servidor X11 não deixa ler a tela (sob Wayland só janelas do XWayland aparecem)")]
    Unreadable,
    #[error("formato de tela sem suporte (profundidade {0}, {1} bits por pixel)")]
    Format(u8, u8),
}

/// Captura a tela inteira do servidor X11 de `$DISPLAY`.
pub struct X11Screen {
    conn: RustConnection,
    root: u32,
    msb_first: bool,
}

impl X11Screen {
    pub fn open() -> Result<Self, CaptureError> {
        let (conn, num) = x11rb::connect(None).map_err(|e| CaptureError::Connect(e.to_string()))?;
        let setup = conn.setup();
        let screen = &setup.roots[num];
        let depth = screen.root_depth;
        let ok = setup.pixmap_formats.iter().any(|f| f.depth == depth && f.bits_per_pixel == 32);
        if !ok {
            let bpp = setup.pixmap_formats.iter().find(|f| f.depth == depth).map_or(0, |f| f.bits_per_pixel);
            return Err(CaptureError::Format(depth, bpp));
        }
        let mut screen = Self { root: screen.root, msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST, conn };
        screen.grab().ok_or(CaptureError::Unreadable)?;
        Ok(screen)
    }

    /// Dimensões da tela, para quem quiser avisar a GUI.
    pub fn size(&self) -> Option<(u16, u16)> {
        let g = self.conn.get_geometry(self.root).ok()?.reply().ok()?;
        Some((g.width, g.height))
    }
}

impl ScreenSource for X11Screen {
    fn grab(&mut self) -> Option<RawFrame> {
        let g = self.conn.get_geometry(self.root).ok()?.reply().ok()?;
        let img = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, g.width, g.height, !0).ok()?.reply();
        let img = img.ok()?;
        let (w, h) = (g.width as usize, g.height as usize);
        if img.data.len() < w * h * 4 {
            return None;
        }
        let mut rgba = img.data;
        rgba.truncate(w * h * 4);
        for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
            // LSB first: B G R X; MSB first: X R G B.
            if self.msb_first {
                px.copy_within(1..4, 0);
            } else {
                px.swap(0, 2);
            }
            px[3] = 255;
        }
        RawFrame::new(w, h, rgba).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grabs_the_screen_when_a_display_exists() {
        let Ok(mut s) = X11Screen::open() else { return };
        let f = s.grab().expect("quadro");
        assert_eq!(f.rgba.len(), f.width * f.height * 4);
        assert!(f.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }
}
