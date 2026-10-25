//! Tela compartilhada numa chamada 1:1: captura e envio de quem transmite, decodificação de quem assiste.

use std::sync::atomic::AtomicU64;

use super::*;
use clam::{
    node::Dialer,
    phone::ScreenEvent,
    video::{FPS, H264Decoder, H264Encoder, RawFrame, ScreenSource, pump},
};

/// Estado da tela compartilhada, visto pela janela e pela chamada.
#[derive(Default)]
pub(crate) struct ScreenState {
    /// A pessoa quer transmitir a própria tela.
    pub(crate) want_share: AtomicBool,
    /// Há uma tarefa de transmissão viva (ligando ou enviando).
    pub(crate) share_task: AtomicBool,
    /// Os quadros estão saindo.
    pub(crate) sharing: AtomicBool,
    /// Última imagem recebida e o número dela; a janela só refaz a textura quando o número muda.
    pub(crate) image: Mutex<Option<RawFrame>>,
    pub(crate) seq: AtomicU64,
    /// Motivo de a transmissão não ter começado, para a janela.
    pub(crate) note: Mutex<String>,
}

impl ScreenState {
    fn set_image(&self, img: Option<RawFrame>) {
        *self.image.lock().unwrap() = img;
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    /// Fim da chamada: some a imagem e a transmissão é interrompida.
    pub(crate) fn reset(&self) {
        self.want_share.store(false, Ordering::Relaxed);
        self.set_image(None);
    }
}

/// Trata um evento de tela recebido. Devolve `false` se o decodificador quebrou e o evento foi ignorado.
pub(crate) fn on_screen_event(state: &ScreenState, dec: &mut Option<H264Decoder>, ev: ScreenEvent) -> bool {
    match ev {
        ScreenEvent::Stopped => {
            *dec = None;
            state.set_image(None);
            true
        }
        ScreenEvent::Frame(f) => {
            let d = match dec {
                Some(d) => d,
                None => match H264Decoder::new() {
                    Ok(d) => dec.insert(d),
                    Err(_) => return false,
                },
            };
            match d.decode(&f.data) {
                Ok(Some(img)) => {
                    state.set_image(Some(img));
                    true
                }
                Ok(None) => true,
                // Quadro corrompido: recomeça no próximo quadro-chave.
                Err(_) => {
                    *dec = None;
                    false
                }
            }
        }
    }
}

/// Abre a captura da tela: pelo portal (PipeWire) no Wayland e pelo X11 nos outros casos; se a
/// primeira falhar, tenta a outra. Fora daqui só há o trait `ScreenSource`.
async fn open_source() -> Result<Box<dyn ScreenSource + Send>, String> {
    async fn portal() -> Result<Box<dyn ScreenSource + Send>, String> {
        clam::portal::PortalScreen::open().await.map(|s| Box::new(s) as Box<dyn ScreenSource + Send>).map_err(|e| e.to_string())
    }
    fn x11() -> Result<Box<dyn ScreenSource + Send>, String> {
        clam::capture::X11Screen::open().map(|s| Box::new(s) as Box<dyn ScreenSource + Send>).map_err(|e| e.to_string())
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match portal().await {
            Ok(s) => Ok(s),
            Err(e) => x11().map_err(|_| e),
        }
    } else {
        match x11() {
            Ok(s) => Ok(s),
            Err(e) => portal().await.map_err(|_| e),
        }
    }
}

/// Liga o circuito de tela e envia quadros enquanto a pessoa quiser e a chamada durar.
pub(crate) async fn share_task(dialer: Dialer, peer: [u8; 32], keys: clam::media::keys::CallKeys, shared: Arc<Shared>, out: Sink) {
    let st = &shared.screen;
    let fail = |why: String| {
        out.line(format!("tela: {why}"));
        *st.note.lock().unwrap() = why;
        st.want_share.store(false, Ordering::Relaxed);
    };
    st.note.lock().unwrap().clear();
    let wanted = || st.want_share.load(Ordering::Relaxed) && !shared.hangup.load(Ordering::Relaxed);
    let started = match open_source().await {
        Ok(src) => H264Encoder::new(H264Encoder::default_bitrate()).map(|e| (src, e)).map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    let (mut src, mut enc) = match started {
        Ok(x) => x,
        Err(e) => {
            fail(format!("não deu para capturar a tela ({e})"));
            st.share_task.store(false, Ordering::Relaxed);
            return;
        }
    };
    out.line("tela: ligando o circuito de vídeo...");
    let opened = if wanted() { until_hangup(dialer.share_screen_keys(peer, &keys, Duration::from_secs(15)), &shared.hangup).await } else { None };
    let share = match opened {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            fail(format!("não abriu o circuito de vídeo ({e})"));
            st.share_task.store(false, Ordering::Relaxed);
            return;
        }
        None => {
            st.share_task.store(false, Ordering::Relaxed);
            return;
        }
    };
    st.sharing.store(true, Ordering::Relaxed);
    out.line("tela: transmitindo");
    let flag = shared.clone();
    // Capturar e codificar pesa: fora do executor.
    let share = tokio::task::spawn_blocking(move || {
        let every = Duration::from_secs_f32(1.0 / FPS);
        let mut next = std::time::Instant::now();
        while flag.screen.want_share.load(Ordering::Relaxed) && !flag.hangup.load(Ordering::Relaxed) {
            if let Err(e) = pump(src.as_mut(), &mut enc, &share) {
                *flag.screen.note.lock().unwrap() = format!("codec: {e}");
            }
            next += every;
            std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
        }
        share
    })
    .await;
    st.sharing.store(false, Ordering::Relaxed);
    st.want_share.store(false, Ordering::Relaxed);
    if let Ok(share) = share {
        share.stop().await;
    }
    out.line("tela: transmissão encerrada");
    st.share_task.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clam::media::screen::ScreenFrame;

    fn frame(w: usize, h: usize, rgb: [u8; 3]) -> RawFrame {
        RawFrame::new(w, h, [rgb[0], rgb[1], rgb[2], 255].repeat(w * h)).unwrap()
    }

    #[test]
    fn received_frames_become_the_current_image_until_the_peer_stops() {
        let st = ScreenState::default();
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = None;
        let f = enc.encode(&frame(64, 48, [10, 200, 10]), false).unwrap().unwrap();
        assert!(on_screen_event(&st, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: f.keyframe })));
        let img = st.image.lock().unwrap().clone().expect("imagem");
        assert_eq!((img.width, img.height), (64, 48));
        let seq = st.seq.load(Ordering::Relaxed);
        assert!(on_screen_event(&st, &mut dec, ScreenEvent::Stopped));
        assert!(st.image.lock().unwrap().is_none());
        assert!(st.seq.load(Ordering::Relaxed) > seq);
    }

    #[test]
    fn garbage_frames_do_not_break_the_next_keyframe() {
        let st = ScreenState::default();
        let mut dec = None;
        let junk = ScreenFrame { data: vec![0xff; 300], keyframe: false };
        let _ = on_screen_event(&st, &mut dec, ScreenEvent::Frame(junk));
        assert!(st.image.lock().unwrap().is_none());
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let f = enc.encode(&frame(32, 32, [1, 2, 3]), false).unwrap().unwrap();
        assert!(on_screen_event(&st, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: true })));
        assert!(st.image.lock().unwrap().is_some());
    }

    #[test]
    fn reset_clears_the_image_and_the_share_request() {
        let st = ScreenState::default();
        st.want_share.store(true, Ordering::Relaxed);
        *st.image.lock().unwrap() = Some(frame(2, 2, [0; 3]));
        st.reset();
        assert!(!st.want_share.load(Ordering::Relaxed));
        assert!(st.image.lock().unwrap().is_none());
    }
}
