//! Captura da tela principal pelo GDI (`BitBlt` do DC da tela para um bitmap em memória).
//!
//! O bitmap é uma DIB section criada uma vez e reaproveitada: o `BitBlt` escreve direto na memória que
//! lemos, sem `GetDIBits` (que converte o quadro inteiro) nem criar e destruir DC e bitmap a cada quadro.
#![allow(unsafe_code)] // o módulo inteiro é FFI do Win32; cada bloco `unsafe` diz por que vale.

use super::CaptureError;
use crate::video::{RawFrame, ScreenSource};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SetProcessDPIAware, SM_CXSCREEN, SM_CYSCREEN};

/// Bitmap de `w`×`h` em memória, com o DC que o seleciona. Largar libera tudo.
struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    /// Início dos `w * h * 4` bytes (B G R X) da DIB section; vale enquanto `bmp` existir.
    bits: *const u8,
    w: usize,
    h: usize,
}

// SAFETY: o DC e o bitmap só são usados por quem tem o `&mut GdiScreen`, um de cada vez; o GDI aceita
// que um objeto criado numa thread seja usado em outra, desde que não ao mesmo tempo.
unsafe impl Send for Surface {}

impl Surface {
    fn new(screen: HDC, w: usize, h: usize) -> Option<Self> {
        // SAFETY: os objetos criados são liberados em `Drop` ou aqui, nos caminhos de falha.
        unsafe {
            let dc = CreateCompatibleDC(screen);
            if dc.is_null() {
                return None;
            }
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
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                if !bmp.is_null() {
                    DeleteObject(bmp);
                }
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bmp);
            Some(Self { dc, bmp, old, bits: bits.cast(), w, h })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: `dc` e `bmp` são nossos; o bitmap sai do DC antes de ser apagado.
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp);
            DeleteDC(self.dc);
        }
    }
}

/// Captura a tela principal.
pub struct GdiScreen {
    surface: Option<Surface>,
}

impl GdiScreen {
    pub fn open() -> Result<Self, CaptureError> {
        // Sem isto o Windows entrega o tamanho "virtualizado" em telas com escala.
        // SAFETY: função do user32 sem argumentos nem ponteiros.
        unsafe { SetProcessDPIAware() };
        let mut s = Self { surface: None };
        s.grab().ok_or(CaptureError::Gdi)?;
        Ok(s)
    }

    /// Dimensões da tela principal.
    pub fn size(&self) -> (usize, usize) {
        // SAFETY: `GetSystemMetrics` só recebe um índice e devolve um inteiro.
        unsafe { (GetSystemMetrics(SM_CXSCREEN).max(0) as usize, GetSystemMetrics(SM_CYSCREEN).max(0) as usize) }
    }
}

impl ScreenSource for GdiScreen {
    fn grab(&mut self) -> Option<RawFrame> {
        let (w, h) = self.size();
        if w < 2 || h < 2 {
            return None;
        }
        // SAFETY: o DC da tela é devolvido antes de sair; a superfície é nossa.
        let copied = unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            // Resolução mudou: recria. Largar a antiga primeiro libera a memória do bitmap.
            if self.surface.as_ref().is_none_or(|s| (s.w, s.h) != (w, h)) {
                self.surface = None;
                self.surface = Surface::new(screen, w, h);
            }
            let copied = self.surface.as_ref().is_some_and(|s| BitBlt(s.dc, 0, 0, w as i32, h as i32, screen, 0, 0, SRCCOPY | CAPTUREBLT) != 0);
            ReleaseDC(std::ptr::null_mut(), screen);
            // O GDI agrupa os desenhos: sem isto a leitura abaixo pode ver o quadro anterior.
            GdiFlush();
            copied
        };
        let surface = self.surface.as_ref().filter(|_| copied)?;
        // SAFETY: `bits` aponta para `w * h * 4` bytes da DIB section, viva enquanto `surface` existir.
        let src = unsafe { std::slice::from_raw_parts(surface.bits, w * h * 4) };
        let mut rgba = vec![0u8; w * h * 4];
        // GDI entrega B G R X; uma passada só converte e fecha o alfa.
        for (d, s) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], 255];
        }
        RawFrame::new(w, h, rgba).ok()
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
