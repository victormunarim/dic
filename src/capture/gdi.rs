//! Captura da tela principal pelo GDI (`BitBlt` do DC da tela para um bitmap em memória).

use super::CaptureError;
use crate::video::{RawFrame, ScreenSource};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SetProcessDPIAware, SM_CXSCREEN, SM_CYSCREEN};

/// Captura a tela principal.
pub struct GdiScreen;

impl GdiScreen {
    pub fn open() -> Result<Self, CaptureError> {
        // Sem isto o Windows entrega o tamanho "virtualizado" em telas com escala.
        unsafe { SetProcessDPIAware() };
        let mut s = Self;
        s.grab().ok_or(CaptureError::Gdi)?;
        Ok(s)
    }

    /// Dimensões da tela principal.
    pub fn size(&self) -> (usize, usize) {
        unsafe { (GetSystemMetrics(SM_CXSCREEN).max(0) as usize, GetSystemMetrics(SM_CYSCREEN).max(0) as usize) }
    }
}

impl ScreenSource for GdiScreen {
    fn grab(&mut self) -> Option<RawFrame> {
        let (w, h) = self.size();
        if w < 2 || h < 2 {
            return None;
        }
        // SAFETY: todos os objetos GDI criados aqui são liberados antes de sair.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let dc = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, w as i32, h as i32);
            let mut out = None;
            if !dc.is_null() && !bmp.is_null() {
                let old = SelectObject(dc, bmp);
                if BitBlt(dc, 0, 0, w as i32, h as i32, screen, 0, 0, SRCCOPY | CAPTUREBLT) != 0 {
                    let mut info: BITMAPINFO = std::mem::zeroed();
                    info.bmiHeader = BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w as i32,
                        // Altura negativa: linhas de cima para baixo.
                        biHeight: -(h as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB,
                        ..std::mem::zeroed()
                    };
                    let mut px = vec![0u8; w * h * 4];
                    if GetDIBits(dc, bmp, 0, h as u32, px.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS) != 0 {
                        out = Some(px);
                    }
                }
                SelectObject(dc, old);
            }
            if !bmp.is_null() {
                DeleteObject(bmp);
            }
            if !dc.is_null() {
                DeleteDC(dc);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
            let mut rgba = out?;
            for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
                // GDI entrega B G R X.
                px.swap(0, 2);
                px[3] = 255;
            }
            RawFrame::new(w, h, rgba).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grabs_the_screen_when_one_exists() {
        let Ok(mut s) = GdiScreen::open() else { return };
        let f = s.grab().expect("quadro");
        assert_eq!(f.rgba.len(), f.width * f.height * 4);
    }
}
