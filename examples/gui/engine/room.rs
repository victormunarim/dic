//! Sala de voz: uma chamada com cada pessoa que está na mesma sala, com as vozes misturadas num só alto-falante.
//!
//! A malha vem da presença do servidor (`Event::Mesh`): entre duas pessoas da sala quem tem a menor chave
//! disca, e a outra atende sozinha, sem tocar. Quem sai da sala (ou cai) deixa de estar na malha e sua
//! chamada é desligada; quem cai com a malha ainda valendo é religado pelo lado que discou.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::*;

/// Energia mínima (RMS) de um quadro recebido para a pessoa contar como falando.
const SPEAKING_RMS: f64 = 250.0;
/// Quadros de uma pessoa que esperam o alto-falante; passando disto, o excesso é descartado (a voz não atrasa).
const PEER_QUEUE: usize = 6;
/// Espera antes de tentar discar de novo para quem não atendeu.
const REDIAL_AFTER: Duration = Duration::from_secs(4);
/// Quantas pessoas assistem à minha tela ao mesmo tempo.
pub(crate) const MAX_VIEWERS: usize = 3;
/// Espera antes de repetir o pedido de assistir a quem ainda não mandou imagem.
const ASK_AGAIN: Duration = Duration::from_secs(5);
/// Um pedido repetido depois disto, com o circuito já aberto, é sinal de que ele não chegou: reabre.
const REOPEN_AFTER: Duration = Duration::from_secs(8);

/// Onde esta pessoa está e quem fala, para a janela.
#[derive(Default)]
pub(crate) struct RoomLive {
    pub(crate) at: Option<(clam::server::ServerId, clam::server::RoomId)>,
    /// Com quem a chamada está de pé.
    pub(crate) connected: HashSet<[u8; 32]>,
    /// Quando cada pessoa falou pela última vez.
    pub(crate) spoke: HashMap<[u8; 32], Instant>,
    /// De quem eu pedi para assistir à tela.
    pub(crate) watching: HashSet<[u8; 32]>,
}

impl RoomLive {
    /// A pessoa falou há pouco (o aro verde da janela).
    pub(crate) fn speaking(&self, key: &[u8; 32]) -> bool {
        self.spoke.get(key).is_some_and(|t| t.elapsed() < Duration::from_millis(500))
    }

    pub(crate) fn clear(&mut self) {
        *self = RoomLive::default();
    }
}

/// Peço (ou deixo de pedir) para assistir à tela de `who`, que está na minha sala. O pedido se repete
/// sozinho enquanto a imagem não chega.
pub(crate) fn follow(shared: &Shared, who: [u8; 32], on: bool) {
    let mut live = shared.room.lock().unwrap();
    let Some((server, _)) = live.at else { return };
    if on {
        live.watching.insert(who);
    } else {
        live.watching.remove(&who);
        shared.screen.set_image(who, None);
    }
    drop(live);
    if let Some(hub) = shared.servers.lock().unwrap().as_ref() {
        let _ = hub.try_act(|s, now| s.watch(&server, who, on, now));
    }
}

pub(crate) enum RoomCmd {
    /// Quem está na sala agora (sem esta pessoa).
    Mesh(Vec<[u8; 32]>),
    /// Chamada que chegou de alguém da malha, já atendida.
    Call([u8; 32], ActiveCall),
    /// Resultado de uma discagem.
    Dialed([u8; 32], Result<ActiveCall, String>),
    /// A chamada da geração `rev` com essa pessoa acabou.
    Ended([u8; 32], u64),
    /// Passo a transmitir a tela (ou paro).
    Share(bool),
    /// A captura ficou pronta (ou não abriu).
    Capture(Option<RoomCapture>),
    /// Alguém da sala pediu (ou deixou de pedir) para assistir à minha tela.
    Viewer([u8; 32], bool),
    /// O circuito de tela para esse espectador abriu (ou não); o número identifica o pedido.
    Opened([u8; 32], u64, Result<clam::phone::ScreenShare, String>),
    Leave,
}

struct Link {
    rev: u64,
    mic: mpsc::Sender<Vec<i16>>,
    pcm: mpsc::Receiver<Vec<i16>>,
    /// Pede as chaves de um circuito de tela novo a esta chamada.
    keys: mpsc::UnboundedSender<oneshot::Sender<clam::media::keys::CallKeys>>,
}

/// Quem assiste à minha tela e desde quando.
struct Viewer {
    token: u64,
    since: Instant,
}

/// Soma os quadros (cada pessoa já no seu volume), sem estourar o `i16`.
pub(crate) fn mix(frames: &[Vec<i16>]) -> Vec<i16> {
    let len = frames.iter().map(Vec::len).max().unwrap_or(0);
    (0..len).map(|i| frames.iter().filter_map(|f| f.get(i)).map(|x| *x as i32).sum::<i32>().clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect()
}

fn rms(f: &[i16]) -> f64 {
    if f.is_empty() {
        return 0.0;
    }
    (f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64).sqrt()
}

/// A chamada com uma pessoa: leva o microfone até ela e o que ela diz até a mistura.
#[allow(clippy::too_many_arguments)]
async fn link_loop(
    mut c: ActiveCall,
    key: [u8; 32],
    rev: u64,
    mut mic: mpsc::Receiver<Vec<i16>>,
    pcm: mpsc::Sender<Vec<i16>>,
    mut keys: mpsc::UnboundedReceiver<oneshot::Sender<clam::media::keys::CallKeys>>,
    room: mpsc::UnboundedSender<RoomCmd>,
    shared: Arc<Shared>,
) {
    let (mut screen, mut screen_open, mut decoder) = (c.take_screen(), true, None);
    let mut keys_open = true;
    let mut silence = Silence::new(tokio::time::Instant::now());
    let mut prev = c.stats().received;
    let mut check = interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            f = mic.recv() => match f {
                Some(f) => {
                    if c.send(f).await.is_err() {
                        break;
                    }
                }
                // A sala largou esta chamada.
                None => break,
            },
            f = c.next_pcm() => match f {
                Some(f) => {
                    let _ = pcm.try_send(f);
                }
                None => break,
            },
            ev = screen.recv(), if screen_open => match ev {
                Some(ev) => {
                    on_screen_event(&shared.screen, key, &mut decoder, ev);
                }
                None => screen_open = false,
            },
            r = keys.recv(), if keys_open => match r {
                Some(r) => {
                    let _ = r.send(c.next_share_keys());
                }
                None => keys_open = false,
            },
            _ = check.tick() => {
                let now = c.stats().received;
                if silence.lost(now > prev, tokio::time::Instant::now()) {
                    break;
                }
                prev = now;
            }
        }
    }
    c.hangup();
    shared.screen.set_image(key, None);
    let _ = room.send(RoomCmd::Ended(key, rev));
}

/// Para de transmitir a tela e avisa a sala.
fn stop_sharing(shared: &Shared) {
    shared.screen.want_share.store(false, Ordering::Relaxed);
    if let Some(hub) = shared.servers.lock().unwrap().as_ref() {
        hub.run(|s, now| s.set_sharing(false, now));
    }
}

/// Vive enquanto a pessoa estiver na sala. Devolve o microfone ao sistema ao sair.
pub(crate) async fn run_room(mut rx: mpsc::UnboundedReceiver<RoomCmd>, me_tx: mpsc::UnboundedSender<RoomCmd>, sound: Option<Sound>, me: [u8; 32], dialer: clam::node::Dialer, shared: Arc<Shared>, out: Sink) {
    let mut links: HashMap<[u8; 32], Link> = HashMap::new();
    let mut dialing: HashMap<[u8; 32], Instant> = HashMap::new();
    let mut mesh: Vec<[u8; 32]> = Vec::new();
    let mut revs = 0u64;
    // A minha tela: a captura (uma só), quem assiste e quando perguntei de novo a quem eu assisto.
    let mut capture: Option<RoomCapture> = None;
    let mut capturing = false;
    let mut viewers: HashMap<[u8; 32], Viewer> = HashMap::new();
    let mut asked: HashMap<[u8; 32], Instant> = HashMap::new();
    let mut tokens = 0u64;
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut n = 0usize;
    let mut sync = interval(Duration::from_secs(1));
    let add = |links: &mut HashMap<[u8; 32], Link>, revs: &mut u64, key: [u8; 32], c: ActiveCall| {
        *revs += 1;
        let (mic_tx, mic_rx) = mpsc::channel(4);
        let (pcm_tx, pcm_rx) = mpsc::channel(PEER_QUEUE);
        let (keys_tx, keys_rx) = mpsc::unbounded_channel();
        tokio::spawn(link_loop(c, key, *revs, mic_rx, pcm_tx, keys_rx, me_tx.clone(), shared.clone()));
        // A ligação antiga com a mesma pessoa cai junto com o `Link` que sai.
        links.insert(key, Link { rev: *revs, mic: mic_tx, pcm: pcm_rx, keys: keys_tx });
    };
    loop {
        tokio::select! {
            _ = tick.tick() => {
                for f in mic_frames(sound.as_ref(), 440.0, n, &shared) {
                    for l in links.values() {
                        let _ = l.mic.try_send(f.clone());
                    }
                }
                n += 1;
                let mut heard = Vec::new();
                for (key, l) in links.iter_mut() {
                    let mut last = None;
                    while let Ok(f) = l.pcm.try_recv() {
                        last = Some(f);
                        // Um quadro por tick: o que sobrou é atraso acumulado.
                        if l.pcm.len() <= 1 {
                            break;
                        }
                    }
                    if let Some(f) = last {
                        if rms(&f) > SPEAKING_RMS {
                            shared.room.lock().unwrap().spoke.insert(*key, Instant::now());
                        }
                        heard.push(f);
                    }
                }
                if !heard.is_empty() {
                    play_scaled(sound.as_ref(), mix(&heard), &shared);
                }
                if n.is_multiple_of(50) {
                    report_error(sound.as_ref(), &out);
                }
            }
            _ = sync.tick() => {
                // Quem saiu da malha é desligado; quem falta e tem a chave maior que a minha, eu disco.
                links.retain(|k, _| mesh.contains(k));
                for gone in viewers.keys().filter(|k| !links.contains_key(*k)).copied().collect::<Vec<_>>() {
                    viewers.remove(&gone);
                    if let Some(c) = &capture {
                        let _ = c.ctl.send(ShareCtl::Remove(gone));
                    }
                }
                // Quem eu assisto e ainda não mandou imagem recebe o pedido de novo (o quadro pode ter se perdido).
                let wanted: Vec<[u8; 32]> = shared.room.lock().unwrap().watching.iter().copied().collect();
                for who in wanted {
                    if !mesh.contains(&who) {
                        shared.room.lock().unwrap().watching.remove(&who);
                        asked.remove(&who);
                    } else if !shared.screen.images.lock().unwrap().contains_key(&who) && asked.get(&who).is_none_or(|t| t.elapsed() >= ASK_AGAIN) {
                        asked.insert(who, Instant::now());
                        follow(&shared, who, true);
                    }
                }
                for key in &mesh {
                    if links.contains_key(key) || me >= *key || dialing.get(key).is_some_and(|t| t.elapsed() < REDIAL_AFTER) {
                        continue;
                    }
                    dialing.insert(*key, Instant::now());
                    let (dialer, tx, key, opus) = (dialer.clone(), me_tx.clone(), *key, shared.opus.load(Ordering::Relaxed));
                    tokio::spawn(async move {
                        let codec = if opus { CodecId::Opus } else { CodecId::G722 };
                        let r = dialer.dial(key, codec.build(), Duration::from_secs(15)).await.map_err(|e| e.to_string());
                        let _ = tx.send(RoomCmd::Dialed(key, r));
                    });
                }
                shared.room.lock().unwrap().connected = links.keys().copied().collect();
            }
            cmd = rx.recv() => match cmd {
                Some(RoomCmd::Mesh(peers)) => mesh = peers,
                Some(RoomCmd::Call(key, c)) => add(&mut links, &mut revs, key, c),
                Some(RoomCmd::Dialed(key, Ok(c))) => {
                    dialing.remove(&key);
                    if mesh.contains(&key) {
                        add(&mut links, &mut revs, key, c);
                    }
                }
                Some(RoomCmd::Dialed(key, Err(e))) => {
                    out.line(format!("sala: não consegui ligar para {}: {e}", &hex(&key)[..8]));
                }
                Some(RoomCmd::Ended(key, rev)) => {
                    if links.get(&key).is_some_and(|l| l.rev == rev) {
                        links.remove(&key);
                        dialing.insert(key, Instant::now());
                    }
                }
                Some(RoomCmd::Share(true)) => {
                    if capture.is_none() && !capturing {
                        capturing = true;
                        let (sh, o, tx) = (shared.clone(), out.clone(), me_tx.clone());
                        tokio::spawn(async move {
                            let _ = tx.send(RoomCmd::Capture(start_room_capture(sh, o).await));
                        });
                    }
                }
                Some(RoomCmd::Share(false)) => {
                    capture = None;
                    viewers.clear();
                    shared.screen.want_share.store(false, Ordering::Relaxed);
                }
                Some(RoomCmd::Capture(c)) => {
                    capturing = false;
                    match c {
                        Some(c) if shared.screen.want_share.load(Ordering::Relaxed) => capture = Some(c),
                        _ => stop_sharing(&shared),
                    }
                }
                Some(RoomCmd::Viewer(who, false)) => {
                    viewers.remove(&who);
                    if let Some(c) = &capture {
                        let _ = c.ctl.send(ShareCtl::Remove(who));
                    }
                }
                Some(RoomCmd::Viewer(who, true)) => {
                    let Some(c) = &capture else { continue };
                    let fresh = viewers.get(&who).is_some_and(|v| v.since.elapsed() < REOPEN_AFTER);
                    if fresh || !mesh.contains(&who) || (viewers.len() >= MAX_VIEWERS && !viewers.contains_key(&who)) {
                        continue;
                    }
                    let Some(link) = links.get(&who) else { continue };
                    let (tx, rx) = oneshot::channel();
                    if link.keys.send(tx).is_err() {
                        continue;
                    }
                    tokens += 1;
                    viewers.insert(who, Viewer { token: tokens, since: Instant::now() });
                    let _ = c.ctl.send(ShareCtl::Remove(who));
                    let (dialer, me_tx, token, private, hangup) = (dialer.clone(), me_tx.clone(), tokens, c.private, shared.clone());
                    tokio::spawn(async move {
                        let r = match rx.await {
                            Ok(keys) => match until_hangup(dialer.share_screen_keys_with(who, &keys, Duration::from_secs(15), private), &hangup.hangup).await {
                                Some(r) => r.map_err(|e| e.to_string()),
                                None => Err("chamada encerrada".to_string()),
                            },
                            Err(_) => Err("a chamada com a pessoa acabou".to_string()),
                        };
                        let _ = me_tx.send(RoomCmd::Opened(who, token, r));
                    });
                }
                Some(RoomCmd::Opened(who, token, r)) => {
                    let current = viewers.get(&who).is_some_and(|v| v.token == token);
                    match (r, &capture) {
                        (Ok(share), Some(c)) if current => {
                            let _ = c.ctl.send(ShareCtl::Add(who, share));
                        }
                        (Err(e), _) if current => {
                            viewers.remove(&who);
                            out.line(format!("tela: não abriu o circuito para {}: {e}", &hex(&who)[..8]));
                        }
                        _ => {}
                    }
                }
                Some(RoomCmd::Leave) | None => break,
            },
        }
    }
    drop(links);
    drop(capture);
    stop_sharing(&shared);
    shared.screen.reset();
    shared.audio.set_level(0.0);
    shared.room.lock().unwrap().clear();
    out.line("saiu da sala");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_adds_voices_and_never_overflows() {
        assert_eq!(mix(&[vec![100, -100, 5], vec![1, 2]]), vec![101, -98, 5]);
        assert_eq!(mix(&[vec![30000], vec![30000]]), vec![i16::MAX]);
        assert_eq!(mix(&[vec![-30000], vec![-30000]]), vec![i16::MIN]);
        assert!(mix(&[]).is_empty());
    }

    #[test]
    fn only_loud_frames_count_as_speech() {
        assert!(rms(&[1000; 320]) > SPEAKING_RMS);
        assert!(rms(&[10; 320]) < SPEAKING_RMS);
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn speaking_expires() {
        let mut live = RoomLive::default();
        live.spoke.insert([1; 32], Instant::now());
        live.spoke.insert([2; 32], Instant::now() - Duration::from_secs(2));
        assert!(live.speaking(&[1; 32]));
        assert!(!live.speaking(&[2; 32]));
        assert!(!live.speaking(&[3; 32]));
    }
}
