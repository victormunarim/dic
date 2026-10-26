//! Tela compartilhada numa chamada 1:1: captura e envio de quem transmite, decodificação de quem assiste.

use std::{collections::HashMap, sync::atomic::AtomicU64};

use super::*;
use clam::{
    node::Dialer,
    phone::{OPEN_SCREEN_SLOT, SCREEN_SLOT, ScreenEvent},
    video::{H264Decoder, H264Encoder, RawFrame, ScreenSource, fps_for, pump},
};

/// Estado da tela compartilhada, visto pela janela e pela chamada.
#[derive(Default)]
pub(crate) struct ScreenState {
    /// A pessoa quer transmitir a própria tela.
    pub(crate) want_share: AtomicBool,
    /// Há uma tarefa de transmissão viva (ligando ou enviando).
    pub(crate) share_task: AtomicBool,
    /// A pessoa abriu mão da privacidade da tela (sem cover, o dobro do ritmo); falso é o padrão. A janela
    /// copia a escolha da configuração a cada quadro.
    pub(crate) open_path: AtomicBool,
    /// Os quadros estão saindo.
    pub(crate) sharing: AtomicBool,
    /// Última imagem recebida de cada pessoa que transmite, com o número dela; a janela só refaz a
    /// textura quando o número muda.
    pub(crate) images: Mutex<HashMap<[u8; 32], (u64, RawFrame)>>,
    seq: AtomicU64,
    /// Contadores para diagnóstico: quadros que chegaram, que viraram imagem e que não decodificaram.
    rx: [AtomicU64; 4],
    /// Resumo de quem transmite (quadros enviados, pulados pelo codec, descartados), para a janela.
    pub(crate) tx_info: Mutex<String>,
    /// Motivo de a transmissão não ter começado, para a janela.
    pub(crate) note: Mutex<String>,
}

impl ScreenState {
    fn set_image(&self, who: [u8; 32], img: Option<RawFrame>) {
        let mut m = self.images.lock().unwrap();
        match img {
            Some(img) => {
                m.insert(who, (self.seq.fetch_add(1, Ordering::Relaxed), img));
            }
            None => {
                m.remove(&who);
            }
        }
    }

    /// Linha de diagnóstico da janela: lado que recebe e lado que envia.
    pub(crate) fn diagnostics(&self) -> String {
        let n = |i: usize| self.rx[i].load(Ordering::Relaxed);
        let mut out = self.tx_info.lock().unwrap().clone();
        if n(0) + n(3) > 0 {
            if !out.is_empty() {
                out.push_str(" · ");
            }
            out.push_str(&format!("recebidos {} quadros, {} viraram imagem, {} com erro, {} perdidos no caminho", n(0), n(1), n(2), n(3)));
        }
        out
    }

    /// Fim da chamada: somem as imagens e a transmissão é interrompida.
    pub(crate) fn reset(&self) {
        for c in &self.rx {
            c.store(0, Ordering::Relaxed);
        }
        self.want_share.store(false, Ordering::Relaxed);
        self.images.lock().unwrap().clear();
    }
}

/// Trata um evento de tela recebido. Devolve `false` se o decodificador quebrou e o evento foi ignorado.
pub(crate) fn on_screen_event(state: &ScreenState, who: [u8; 32], dec: &mut Option<H264Decoder>, ev: ScreenEvent) -> bool {
    match ev {
        ScreenEvent::Stopped => {
            *dec = None;
            state.set_image(who, None);
            true
        }
        ScreenEvent::Lost(n) => {
            state.rx[3].store(n, Ordering::Relaxed);
            true
        }
        ScreenEvent::Frame(f) => {
            state.rx[0].fetch_add(1, Ordering::Relaxed);
            let d = match dec {
                Some(d) => d,
                None => match H264Decoder::new() {
                    Ok(d) => dec.insert(d),
                    Err(_) => return false,
                },
            };
            match d.decode(&f.data) {
                Ok(Some(img)) => {
                    state.rx[1].fetch_add(1, Ordering::Relaxed);
                    state.set_image(who, Some(img));
                    true
                }
                Ok(None) => true,
                // Quadro corrompido: recomeça no próximo quadro-chave.
                Err(_) => {
                    state.rx[2].fetch_add(1, Ordering::Relaxed);
                    *dec = None;
                    false
                }
            }
        }
    }
}

/// Abre a captura da tela: no Windows pelo GDI; no Unix pelo portal (PipeWire) no Wayland e pelo
/// X11 nos outros casos, e se a primeira falhar tenta a outra. Fora daqui só há o trait `ScreenSource`.
#[cfg(windows)]
async fn open_source(_fps: f32) -> Result<Box<dyn ScreenSource + Send>, String> {
    clam::capture::GdiScreen::open().map(|s| Box::new(s) as Box<dyn ScreenSource + Send>).map_err(|e| e.to_string())
}

#[cfg(unix)]
async fn open_source(fps: f32) -> Result<Box<dyn ScreenSource + Send>, String> {
    async fn portal(fps: f32) -> Result<Box<dyn ScreenSource + Send>, String> {
        clam::portal::PortalScreen::open_with(fps).await.map(|s| Box::new(s) as Box<dyn ScreenSource + Send>).map_err(|e| e.to_string())
    }
    fn x11() -> Result<Box<dyn ScreenSource + Send>, String> {
        clam::capture::X11Screen::open().map(|s| Box::new(s) as Box<dyn ScreenSource + Send>).map_err(|e| e.to_string())
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match portal(fps).await {
            Ok(s) => Ok(s),
            Err(e) => x11().map_err(|_| e),
        }
    } else {
        match x11() {
            Ok(s) => Ok(s),
            Err(e) => portal(fps).await.map_err(|_| e),
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
    let private = !st.open_path.load(Ordering::Relaxed);
    let slot = if private { SCREEN_SLOT } else { OPEN_SCREEN_SLOT };
    let bitrate = H264Encoder::bitrate_for(slot);
    let started = match open_source(fps_for(bitrate)).await {
        Ok(src) => H264Encoder::new(bitrate).map(|e| (src, e)).map_err(|e| e.to_string()),
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
    let opened = if wanted() { until_hangup(dialer.share_screen_keys_with(peer, &keys, Duration::from_secs(15), private), &shared.hangup).await } else { None };
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
    out.line(if private { "tela: transmitindo" } else { "tela: transmitindo sem privacidade (sem cover)" });
    let flag = shared.clone();
    // Capturar e codificar pesa: fora do executor.
    let share = tokio::task::spawn_blocking(move || {
        let every = Duration::from_secs_f32(1.0 / enc.fps());
        let mut next = std::time::Instant::now();
        let (mut pushed, mut skipped) = (0u64, 0u64);
        let (mut mark, mut mark_packets, mut rate) = (std::time::Instant::now(), 0u64, 0.0f64);
        while flag.screen.want_share.load(Ordering::Relaxed) && !flag.hangup.load(Ordering::Relaxed) {
            match pump(src.as_mut(), &mut enc, &share) {
                Ok(true) => pushed += 1,
                Ok(false) => skipped += 1,
                Err(e) => *flag.screen.note.lock().unwrap() = format!("codec: {e}"),
            }
            let t = share.stats();
            let span = mark.elapsed().as_secs_f64();
            if span >= 1.0 {
                rate = (t.packets_sent - mark_packets) as f64 / span;
                (mark, mark_packets) = (std::time::Instant::now(), t.packets_sent);
            }
            *flag.screen.tx_info.lock().unwrap() = format!(
                "enviados {pushed}, pulados {skipped} ({} pelo codificador, {} adiados com o circuito cheio, {} largados da camada alta), descartados na fila {}, saindo {rate:.0} pacotes/s, QP {}, imagem 1/{}, codificar leva {:.0} ms",
                enc.skipped, enc.held, enc.layer_dropped, t.frames_dropped, enc.qp(), enc.scale(), enc.encode_ms
            );
            next += every;
            std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
        }
        share
    })
    .await;
    st.sharing.store(false, Ordering::Relaxed);
    st.want_share.store(false, Ordering::Relaxed);
    st.tx_info.lock().unwrap().clear();
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

    const WHO: [u8; 32] = [7; 32];

    fn frame(w: usize, h: usize, rgb: [u8; 3]) -> RawFrame {
        RawFrame::new(w, h, [rgb[0], rgb[1], rgb[2], 255].repeat(w * h)).unwrap()
    }

    #[test]
    fn received_frames_become_the_current_image_until_the_peer_stops() {
        let st = ScreenState::default();
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let mut dec = None;
        let f = enc.encode(&frame(64, 48, [10, 200, 10]), false).unwrap().unwrap();
        assert!(on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: f.keyframe })));
        let (n, img) = st.images.lock().unwrap().get(&WHO).cloned().expect("imagem");
        assert_eq!((img.width, img.height), (64, 48));
        assert!(on_screen_event(&st, WHO, &mut dec, ScreenEvent::Stopped));
        assert!(st.images.lock().unwrap().is_empty());
        // Cada imagem nova leva um número maior: a janela sabe quando refazer a textura.
        let f = enc.encode(&frame(64, 48, [200, 10, 10]), true).unwrap().unwrap();
        on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: true }));
        assert!(st.images.lock().unwrap()[&WHO].0 > n);
    }

    #[test]
    fn garbage_frames_do_not_break_the_next_keyframe() {
        let st = ScreenState::default();
        let mut dec = None;
        let junk = ScreenFrame { data: vec![0xff; 300], keyframe: false };
        let _ = on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(junk));
        assert!(st.images.lock().unwrap().is_empty());
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let f = enc.encode(&frame(32, 32, [1, 2, 3]), false).unwrap().unwrap();
        assert!(on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: true })));
        assert!(st.images.lock().unwrap().contains_key(&WHO));
    }

    #[test]
    fn reset_clears_the_image_and_the_share_request() {
        let st = ScreenState::default();
        st.want_share.store(true, Ordering::Relaxed);
        st.set_image(WHO, Some(frame(2, 2, [0; 3])));
        st.reset();
        assert!(!st.want_share.load(Ordering::Relaxed));
        assert!(st.images.lock().unwrap().is_empty());
    }
}
