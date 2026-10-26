//! Captura da tela no Wayland pelo portal `org.freedesktop.portal.ScreenCast`.
//!
//! O portal pede à pessoa que escolha o que compartilhar e entrega um descritor do PipeWire. Quem
//! lê os quadros é um `gst-launch-1.0` filho (`pipewiresrc`), que já devolve RGBA no tamanho e na
//! taxa pedidos por um tubo; assim não há dependência de compilação do PipeWire, só do GStreamer
//! em tempo de execução (`gstreamer1.0-tools`, `gstreamer1.0-pipewire` e `gstreamer1.0-plugins-base`).

use std::{
    io::Read,
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use thiserror::Error;

use crate::video::{FPS, RawFrame, ScreenSource};

/// Quanto `grab` espera por um quadro novo antes de desistir (menos que um quadro a 15/s).
const GRAB_WAIT: Duration = Duration::from_millis(50);

/// Largura máxima enviada; telas maiores são reduzidas (a banda do circuito é pequena).
pub const MAX_WIDTH: usize = 1920;

#[derive(Debug, Error)]
pub enum PortalError {
    #[error("portal de captura: {0}")]
    Portal(String),
    #[error("a pessoa não escolheu o que compartilhar")]
    NoStream,
    #[error("o portal não informou o tamanho da tela")]
    NoSize,
    #[error("não deu para rodar o gst-launch-1.0 (instale gstreamer1.0-tools e gstreamer1.0-pipewire): {0}")]
    Gstreamer(String),
}

fn portal_err(e: impl std::fmt::Display) -> PortalError {
    PortalError::Portal(e.to_string())
}

/// Tamanho enviado para uma tela de `w`×`h`: no máximo `MAX_WIDTH` de largura, proporção mantida, pares.
pub fn target_size(w: usize, h: usize) -> Option<(usize, usize)> {
    if w < 2 || h < 2 {
        return None;
    }
    let tw = w.min(MAX_WIDTH) & !1;
    let th = (h * tw / w) & !1;
    (tw >= 2 && th >= 2).then_some((tw, th))
}

/// Argumentos do `gst-launch-1.0` que leem o nó `node` pelo descritor `fd` e escrevem RGBA no stdout.
pub fn pipeline_args(fd: i32, node: u32, (w, h): (usize, usize), fps: f32) -> Vec<String> {
    let fps = fps as u32;
    [
        "-q".to_string(),
        "pipewiresrc".into(),
        format!("fd={fd}"),
        format!("path={node}"),
        "do-timestamp=true".into(),
        "!".into(),
        "videorate".into(),
        "!".into(),
        format!("video/x-raw,framerate={fps}/1"),
        "!".into(),
        "videoconvert".into(),
        "!".into(),
        "videoscale".into(),
        "!".into(),
        format!("video/x-raw,format=RGBA,width={w},height={h}"),
        "!".into(),
        "fdsink".into(),
        "fd=1".into(),
    ]
    .to_vec()
}

/// Quadro mais novo ainda não entregue, com aviso a quem espera por ele.
type Latest = (Mutex<Option<RawFrame>>, Condvar);

/// Lê quadros RGBA de `src` e guarda sempre o mais novo em `slot`. Termina no fim do fluxo.
fn read_frames(mut src: impl Read, (w, h): (usize, usize), slot: &Latest) {
    loop {
        let mut rgba = vec![0u8; w * h * 4];
        if src.read_exact(&mut rgba).is_err() {
            return;
        }
        if let Ok(f) = RawFrame::new(w, h, rgba) {
            *slot.0.lock().unwrap() = Some(f);
            slot.1.notify_one();
        }
    }
}

/// Tela capturada pelo portal. Largar o valor encerra a captura.
pub struct PortalScreen {
    child: Child,
    latest: Arc<Latest>,
    reader: Option<JoinHandle<()>>,
    size: (usize, usize),
    // O portal fecha a captura quando a sessão acaba.
    _session: ashpd::desktop::Session<Screencast>,
}

impl PortalScreen {
    /// Abre o diálogo do portal (a pessoa escolhe o monitor ou a janela) e começa a capturar.
    pub async fn open() -> Result<Self, PortalError> {
        Self::open_with(FPS).await
    }

    /// Como [`Self::open`], capturando a `fps` quadros por segundo.
    pub async fn open_with(fps: f32) -> Result<Self, PortalError> {
        let proxy = Screencast::new().await.map_err(portal_err)?;
        let session = proxy.create_session(Default::default()).await.map_err(portal_err)?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(portal_err)?;
        let streams = proxy.start(&session, None, Default::default()).await.map_err(portal_err)?.response().map_err(portal_err)?;
        let stream = streams.streams().first().ok_or(PortalError::NoStream)?;
        let (sw, sh) = stream.size().ok_or(PortalError::NoSize)?;
        let size = target_size(sw.max(0) as usize, sh.max(0) as usize).ok_or(PortalError::NoSize)?;
        let remote = proxy.open_pipe_wire_remote(&session, Default::default()).await.map_err(portal_err)?;
        // `dup` não marca o descritor como "fechar ao executar", então o filho o herda.
        let inherited = rustix::io::dup(&remote).map_err(|e| PortalError::Gstreamer(e.to_string()))?;
        let mut child = Command::new("gst-launch-1.0")
            .args(pipeline_args(inherited.as_raw_fd(), stream.pipe_wire_node_id(), size, fps))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| PortalError::Gstreamer(e.to_string()))?;
        drop(inherited);
        let latest: Arc<Latest> = Arc::new((Mutex::new(None), Condvar::new()));
        let (out, slot) = (child.stdout.take().expect("stdout"), latest.clone());
        let reader = std::thread::spawn(move || read_frames(out, size, &slot));
        Ok(Self { child, latest, reader: Some(reader), size, _session: session })
    }

    /// Tamanho dos quadros entregues.
    pub fn size(&self) -> (usize, usize) {
        self.size
    }
}

impl ScreenSource for PortalScreen {
    /// O quadro mais novo ainda não entregue. Se não há, espera um pouco por ele (o quadro costuma
    /// chegar logo depois do instante da consulta); `None` se nada mudou nesse tempo.
    fn grab(&mut self) -> Option<RawFrame> {
        let (slot, news) = &*self.latest;
        let guard = slot.lock().unwrap();
        let (mut guard, _) = news.wait_timeout_while(guard, GRAB_WAIT, |f| f.is_none()).unwrap();
        guard.take()
    }
}

impl Drop for PortalScreen {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_size_caps_the_width_keeps_the_ratio_and_stays_even() {
        assert_eq!(target_size(2560, 1440), Some((1920, 1080)));
        assert_eq!(target_size(1920, 1200), Some((1920, 1200)));
        assert_eq!(target_size(800, 601), Some((800, 600)));
        assert_eq!(target_size(1, 100), None);
    }

    #[test]
    fn pipeline_reads_the_node_from_the_inherited_fd_and_writes_rgba() {
        let a = pipeline_args(7, 42, (640, 360), FPS);
        assert!(a.contains(&"fd=7".to_string()) && a.contains(&"path=42".to_string()));
        assert!(a.contains(&"video/x-raw,format=RGBA,width=640,height=360".to_string()));
        assert_eq!(a.last().unwrap(), "fd=1");
    }

    #[test]
    fn reader_keeps_only_the_newest_whole_frame() {
        let (w, h) = (2, 2);
        let mut bytes = vec![1u8; w * h * 4];
        bytes.extend(vec![2u8; w * h * 4]);
        bytes.extend(vec![3u8; 5]); // quadro cortado no fim do fluxo
        let slot: Latest = (Mutex::new(None), Condvar::new());
        read_frames(&bytes[..], (w, h), &slot);
        let f = slot.0.lock().unwrap().take().expect("quadro");
        assert!(f.rgba.iter().all(|b| *b == 2));
        assert!(slot.0.lock().unwrap().is_none());
    }

    /// Manual: abre o diálogo do portal de verdade. `cargo test --features portal -- --ignored real_portal`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn real_portal_delivers_frames() {
        let mut s = PortalScreen::open().await.expect("portal");
        let (w, h) = s.size();
        let f = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(f) = s.grab() {
                    return f;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("quadro em 10 s");
        assert_eq!((f.width, f.height), (w, h));
        assert!(f.rgba.iter().any(|b| *b != 0));
    }
}
