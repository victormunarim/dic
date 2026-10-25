//! Captura da tela inteira: X11 (`GetImage` na janela raiz) no Unix e GDI no Windows.
//!
//! Sob Wayland o X11 só enxerga janelas do XWayland: o Wayland não deixa um programa ler a tela
//! sem o portal do compositor ([`crate::portal`]).

use thiserror::Error;

#[cfg(unix)]
mod x11;
#[cfg(unix)]
pub use x11::X11Screen;
#[cfg(windows)]
mod gdi;
#[cfg(windows)]
pub use gdi::GdiScreen;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("sem conexão com o servidor X11: {0}")]
    Connect(String),
    #[error("o servidor X11 não deixa ler a tela (sob Wayland só janelas do XWayland aparecem)")]
    Unreadable,
    #[error("formato de tela sem suporte (profundidade {0}, {1} bits por pixel)")]
    Format(u8, u8),
    #[error("o Windows não deixou ler a tela")]
    Gdi,
}
