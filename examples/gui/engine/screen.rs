//! Tela compartilhada numa chamada 1:1: captura e envio de quem transmite, decodificação de quem assiste.

use clam::sync::LockExt;
use std::{collections::HashMap, sync::atomic::AtomicU64};

use super::*;
use clam::{
    node::Dialer,
    phone::{OPEN_SCREEN_SLOT, SCREEN_SLOT, ScreenEvent, ScreenShare},
    video::{H264Decoder, H264Encoder, Priority, RawFrame, ScreenSource, pump, pump_all},
};

/// Estado da tela compartilhada, visto pela janela e pela chamada.
#[derive(Default)]
pub(crate) struct ScreenState {
    /// A pessoa quer transmitir a própria tela.
    pub(crate) want_share: AtomicBool,
    /// Há uma tarefa de transmissão viva (ligando ou enviando).
    pub(crate) share_task: AtomicBool,
    /// A pessoa abriu mão da privacidade da tela (direto ao contato, sem cover, o dobro do ritmo); falso é o padrão. A janela
    /// copia a escolha da configuração a cada quadro.
    pub(crate) open_path: AtomicBool,
    /// A pessoa prefere nitidez a fluidez (metade dos quadros por segundo); a janela copia a escolha da
    /// configuração a cada quadro, e ela vale a partir da próxima transmissão.
    pub(crate) sharp: AtomicBool,
    /// A pessoa pediu codificação por hardware; a janela copia a escolha da configuração a cada quadro.
    pub(crate) hardware: AtomicBool,
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
    pub(crate) fn set_image(&self, who: [u8; 32], img: Option<RawFrame>) {
        let mut m = self.images.lock_unpoisoned();
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
        let mut out = self.tx_info.lock_unpoisoned().clone();
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
        self.images.lock_unpoisoned().clear();
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

/// Abre a captura e o codificador conforme as escolhas da janela. Devolve também se a transmissão é privada.
async fn start_capture(shared: &Arc<Shared>, out: &Sink) -> Result<(Box<dyn ScreenSource + Send>, H264Encoder, bool), String> {
    let st = &shared.screen;
    let private = !st.open_path.load(Ordering::Relaxed);
    let slot = if private { SCREEN_SLOT } else { OPEN_SCREEN_SLOT };
    let bitrate = H264Encoder::bitrate_for(slot);
    let priority = if st.sharp.load(Ordering::Relaxed) { Priority::Sharpness } else { Priority::Smoothness };
    let started = match open_source(priority.fps(bitrate)).await {
        Ok(src) => H264Encoder::with_priority(bitrate, priority).map(|e| (src, e)).map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    let (src, mut enc) = started.map_err(|e| format!("não deu para capturar a tela ({e})"))?;
    if st.hardware.load(Ordering::Relaxed) {
        // `detect` roda processos: fora do executor.
        match tokio::task::spawn_blocking(clam::hwenc::detect).await.ok().flatten() {
            Some(name) if enc.set_hardware(Some(name)) => out.line(format!("tela: codificando por hardware ({name})")),
            _ => out.line("tela: sem codificador por hardware neste computador (precisa de gstreamer1.0-tools e gstreamer1.0-plugins-bad com o driver da GPU), usando o openh264"),
        }
    }
    Ok((src, enc, private))
}

/// Liga o circuito de tela e envia quadros enquanto a pessoa quiser e a chamada durar.
pub(crate) async fn share_task(dialer: Dialer, peer: [u8; 32], keys: clam::media::keys::CallKeys, shared: Arc<Shared>, out: Sink) {
    let st = &shared.screen;
    let fail = |why: String| {
        out.line(format!("tela: {why}"));
        *st.note.lock_unpoisoned() = why;
        st.want_share.store(false, Ordering::Relaxed);
    };
    st.note.lock_unpoisoned().clear();
    let wanted = || st.want_share.load(Ordering::Relaxed) && !shared.hangup.load(Ordering::Relaxed);
    let (mut src, mut enc, private) = match start_capture(&shared, &out).await {
        Ok(x) => x,
        Err(e) => {
            fail(e);
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
    out.line(if private { "tela: transmitindo" } else { "tela: transmitindo sem privacidade (direto ao contato, sem cover)" });
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
                Err(e) => *flag.screen.note.lock_unpoisoned() = format!("codec: {e}"),
            }
            let t = share.stats();
            let span = mark.elapsed().as_secs_f64();
            if span >= 1.0 {
                rate = (t.packets_sent - mark_packets) as f64 / span;
                (mark, mark_packets) = (std::time::Instant::now(), t.packets_sent);
            }
            *flag.screen.tx_info.lock_unpoisoned() = format!(
                "enviados {pushed}, pulados {skipped} ({} pelo codificador, {} adiados com o circuito cheio, {} largados da camada alta), descartados na fila {}, saindo {rate:.0} pacotes/s, taxa {} kbps, QP {}, imagem 1/{}, codificar leva {:.0} ms, codificador {}",
                enc.skipped, enc.held, enc.layer_dropped, t.frames_dropped, enc.bitrate() / 1000, enc.qp(), enc.scale(), enc.encode_ms,
                match (enc.hardware(), &enc.hw_note) {
                    (Some(name), _) => format!("hardware ({name})"),
                    (None, Some(why)) => format!("openh264 (o hardware parou: {why})"),
                    (None, None) => "openh264".to_string(),
                }
            );
            next += every;
            std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
        }
        share
    })
    .await;
    st.sharing.store(false, Ordering::Relaxed);
    st.want_share.store(false, Ordering::Relaxed);
    st.tx_info.lock_unpoisoned().clear();
    if let Ok(share) = share {
        share.stop().await;
    }
    out.line("tela: transmissão encerrada");
    st.share_task.store(false, Ordering::Relaxed);
}

/// O que a captura da sala recebe: um circuito novo ou o fim de um.
pub(crate) enum ShareCtl {
    Add([u8; 32], ScreenShare),
    Remove([u8; 32]),
}

/// A captura da tela na sala: um codificador só, o mesmo quadro vai a todos os espectadores. Largar o valor
/// encerra a captura e os circuitos.
pub(crate) struct RoomCapture {
    pub(crate) ctl: std::sync::mpsc::Sender<ShareCtl>,
    /// Com privacidade os circuitos de tela abrem com 2 saltos e cover; sem ela, direto ao espectador e sem cover (o
    /// ritmo do codificador foi escolhido para isso).
    pub(crate) private: bool,
}

/// Abre a captura e deixa uma thread à espera dos espectadores: só captura quando há ao menos um.
pub(crate) async fn start_room_capture(shared: Arc<Shared>, out: Sink) -> Option<RoomCapture> {
    let st = &shared.screen;
    st.note.lock_unpoisoned().clear();
    let (mut src, mut enc, private) = match start_capture(&shared, &out).await {
        Ok(x) => x,
        Err(e) => {
            out.line(format!("tela: {e}"));
            *st.note.lock_unpoisoned() = e;
            return None;
        }
    };
    let (ctl, rx) = std::sync::mpsc::channel::<ShareCtl>();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        use std::sync::mpsc::TryRecvError;
        let mut shares: HashMap<[u8; 32], ScreenShare> = HashMap::new();
        let every = Duration::from_secs_f32(1.0 / enc.fps());
        let mut next = std::time::Instant::now();
        let (mut pushed, mut skipped, mut force_key) = (0u64, 0u64, false);
        'run: while shared.screen.want_share.load(Ordering::Relaxed) && !shared.hangup.load(Ordering::Relaxed) {
            loop {
                match rx.try_recv() {
                    Ok(ShareCtl::Add(who, share)) => {
                        if let Some(old) = shares.insert(who, share) {
                            handle.block_on(old.stop());
                        }
                        force_key = true;
                    }
                    Ok(ShareCtl::Remove(who)) => {
                        if let Some(old) = shares.remove(&who) {
                            handle.block_on(old.stop());
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'run,
                }
            }
            shared.screen.sharing.store(!shares.is_empty(), Ordering::Relaxed);
            if shares.is_empty() {
                std::thread::sleep(Duration::from_millis(50));
                next = std::time::Instant::now();
                continue;
            }
            let refs: Vec<&ScreenShare> = shares.values().collect();
            match pump_all(src.as_mut(), &mut enc, &refs, std::mem::take(&mut force_key)) {
                Ok(0) => skipped += 1,
                Ok(_) => pushed += 1,
                Err(e) => *shared.screen.note.lock_unpoisoned() = format!("codec: {e}"),
            }
            *shared.screen.tx_info.lock_unpoisoned() = format!(
                "para {} pessoas: enviados {pushed}, pulados {skipped} ({} pelo codificador, {} adiados com o circuito cheio), taxa {} kbps, QP {}, imagem 1/{}, codificar leva {:.0} ms",
                shares.len(), enc.skipped, enc.held, enc.bitrate() / 1000, enc.qp(), enc.scale(), enc.encode_ms
            );
            next += every;
            std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
        }
        for (_, share) in shares {
            handle.block_on(share.stop());
        }
        shared.screen.sharing.store(false, Ordering::Relaxed);
        shared.screen.tx_info.lock_unpoisoned().clear();
    });
    Some(RoomCapture { ctl, private })
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
        let (n, img) = st.images.lock_unpoisoned().get(&WHO).cloned().expect("imagem");
        assert_eq!((img.width, img.height), (64, 48));
        assert!(on_screen_event(&st, WHO, &mut dec, ScreenEvent::Stopped));
        assert!(st.images.lock_unpoisoned().is_empty());
        // Cada imagem nova leva um número maior: a janela sabe quando refazer a textura.
        let f = enc.encode(&frame(64, 48, [200, 10, 10]), true).unwrap().unwrap();
        on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: true }));
        assert!(st.images.lock_unpoisoned()[&WHO].0 > n);
    }

    #[test]
    fn garbage_frames_do_not_break_the_next_keyframe() {
        let st = ScreenState::default();
        let mut dec = None;
        let junk = ScreenFrame { data: vec![0xff; 300], keyframe: false };
        let _ = on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(junk));
        assert!(st.images.lock_unpoisoned().is_empty());
        let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
        let f = enc.encode(&frame(32, 32, [1, 2, 3]), false).unwrap().unwrap();
        assert!(on_screen_event(&st, WHO, &mut dec, ScreenEvent::Frame(ScreenFrame { data: f.data, keyframe: true })));
        assert!(st.images.lock_unpoisoned().contains_key(&WHO));
    }

    #[test]
    fn reset_clears_the_image_and_the_share_request() {
        let st = ScreenState::default();
        st.want_share.store(true, Ordering::Relaxed);
        st.set_image(WHO, Some(frame(2, 2, [0; 3])));
        st.reset();
        assert!(!st.want_share.load(Ordering::Relaxed));
        assert!(st.images.lock_unpoisoned().is_empty());
    }
}
