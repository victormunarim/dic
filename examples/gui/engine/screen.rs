//! Tela compartilhada numa chamada 1:1: captura e envio de quem transmite, decodificação de quem assiste.

use std::{collections::HashMap, sync::atomic::AtomicU64};

use super::*;
use clam::{
    node::Dialer,
    phone::ScreenEvent,
    video::{FPS, H264Decoder, H264Encoder, RawFrame, ScreenSource, pump, pump_all},
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
    /// Última imagem recebida de cada pessoa que transmite, com o número dela; a janela só refaz a
    /// textura quando o número muda.
    pub(crate) images: Mutex<HashMap<[u8; 32], (u64, RawFrame)>>,
    seq: AtomicU64,
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

    /// Só ficam imagens de quem está em `keep` (saiu da sala ou desligou).
    pub(crate) fn retain(&self, keep: impl Fn(&[u8; 32]) -> bool) {
        self.images.lock().unwrap().retain(|k, _| keep(k));
    }

    /// Fim da chamada ou da sala: somem as imagens e a transmissão é interrompida.
    pub(crate) fn reset(&self) {
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
                    state.set_image(who, Some(img));
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

/// Transmite a tela a todos os membros da sala: um circuito de tela por membro conectado, o mesmo
/// quadro codificado em todos. Membros que entram ganham o circuito sozinhos e um quadro-chave.
pub(crate) async fn room_share_task(dialer: Dialer, mesh: Mesh, shared: Arc<Shared>, out: Sink) {
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;
    let st = &shared.screen;
    let started = match open_source().await {
        Ok(src) => H264Encoder::new(H264Encoder::default_bitrate()).map(|e| (src, e)).map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    let (mut src, mut enc) = match started {
        Ok(x) => x,
        Err(e) => {
            out.line(format!("tela: não deu para capturar a tela ({e})"));
            *st.note.lock().unwrap() = format!("não deu para capturar a tela ({e})");
            st.want_share.store(false, Ordering::Relaxed);
            st.share_task.store(false, Ordering::Relaxed);
            return;
        }
    };
    out.line("tela: transmitindo para a sala");
    st.note.lock().unwrap().clear();
    let handle = tokio::runtime::Handle::current();
    let flag = shared.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let (tx, mut rx) = mpsc::unbounded_channel::<([u8; 32], Option<clam::phone::ScreenShare>)>();
        let mut shares: HashMap<[u8; 32], clam::phone::ScreenShare> = HashMap::new();
        let mut opening: HashSet<[u8; 32]> = HashSet::new();
        let mut retry: HashMap<[u8; 32], Instant> = HashMap::new();
        let every = Duration::from_secs_f32(1.0 / FPS);
        let mut next = Instant::now();
        while flag.screen.want_share.load(Ordering::Relaxed) && !flag.hangup.load(Ordering::Relaxed) {
            let keys = mesh.screen_keys();
            for (who, k) in &keys {
                if shares.contains_key(who) || opening.contains(who) || retry.get(who).is_some_and(|t| *t > Instant::now()) {
                    continue;
                }
                opening.insert(*who);
                let (dialer, tx, who, k) = (dialer.clone(), tx.clone(), *who, k.clone());
                handle.spawn(async move {
                    let r = dialer.share_screen_keys(who, &k, Duration::from_secs(15)).await.ok();
                    let _ = tx.send((who, r));
                });
            }
            let mut joined = false;
            while let Ok((who, r)) = rx.try_recv() {
                opening.remove(&who);
                match r {
                    Some(share) if keys.iter().any(|(k, _)| *k == who) => {
                        shares.insert(who, share);
                        joined = true;
                    }
                    Some(share) => {
                        handle.block_on(share.stop());
                    }
                    None => {
                        retry.insert(who, Instant::now() + Duration::from_secs(5));
                    }
                }
            }
            let gone: Vec<_> = shares.keys().filter(|k| !keys.iter().any(|(w, _)| w == *k)).copied().collect();
            for who in gone {
                if let Some(share) = shares.remove(&who) {
                    handle.block_on(share.stop());
                }
            }
            flag.screen.sharing.store(!shares.is_empty(), Ordering::Relaxed);
            let refs: Vec<_> = shares.values().collect();
            if let Err(e) = pump_all(src.as_mut(), &mut enc, &refs, joined) {
                *flag.screen.note.lock().unwrap() = format!("codec: {e}");
            }
            next += every;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        for (_, share) in shares {
            handle.block_on(share.stop());
        }
    })
    .await;
    st.sharing.store(false, Ordering::Relaxed);
    st.want_share.store(false, Ordering::Relaxed);
    out.line("tela: transmissão para a sala encerrada");
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

    #[test]
    fn retain_drops_images_of_people_who_left() {
        let st = ScreenState::default();
        st.set_image([1; 32], Some(frame(2, 2, [0; 3])));
        st.set_image([2; 32], Some(frame(2, 2, [0; 3])));
        st.retain(|k| *k == [2; 32]);
        let m = st.images.lock().unwrap();
        assert!(!m.contains_key(&[1; 32]) && m.contains_key(&[2; 32]));
    }
}
