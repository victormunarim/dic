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

/// Onde esta pessoa está e quem fala, para a janela.
#[derive(Default)]
pub(crate) struct RoomLive {
    pub(crate) at: Option<(clam::server::ServerId, clam::server::RoomId)>,
    /// Com quem a chamada está de pé.
    pub(crate) connected: HashSet<[u8; 32]>,
    /// Quando cada pessoa falou pela última vez.
    pub(crate) spoke: HashMap<[u8; 32], Instant>,
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

pub(crate) enum RoomCmd {
    /// Quem está na sala agora (sem esta pessoa).
    Mesh(Vec<[u8; 32]>),
    /// Chamada que chegou de alguém da malha, já atendida.
    Call([u8; 32], ActiveCall),
    /// Resultado de uma discagem.
    Dialed([u8; 32], Result<ActiveCall, String>),
    /// A chamada da geração `rev` com essa pessoa acabou.
    Ended([u8; 32], u64),
    Leave,
}

struct Link {
    rev: u64,
    mic: mpsc::Sender<Vec<i16>>,
    pcm: mpsc::Receiver<Vec<i16>>,
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
async fn link_loop(mut c: ActiveCall, key: [u8; 32], rev: u64, mut mic: mpsc::Receiver<Vec<i16>>, pcm: mpsc::Sender<Vec<i16>>, room: mpsc::UnboundedSender<RoomCmd>) {
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
    let _ = room.send(RoomCmd::Ended(key, rev));
}

/// Vive enquanto a pessoa estiver na sala. Devolve o microfone ao sistema ao sair.
pub(crate) async fn run_room(mut rx: mpsc::UnboundedReceiver<RoomCmd>, me_tx: mpsc::UnboundedSender<RoomCmd>, sound: Option<Sound>, me: [u8; 32], dialer: clam::node::Dialer, shared: Arc<Shared>, out: Sink) {
    let mut links: HashMap<[u8; 32], Link> = HashMap::new();
    let mut dialing: HashMap<[u8; 32], Instant> = HashMap::new();
    let mut mesh: Vec<[u8; 32]> = Vec::new();
    let mut revs = 0u64;
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut n = 0usize;
    let mut sync = interval(Duration::from_secs(1));
    let add = |links: &mut HashMap<[u8; 32], Link>, revs: &mut u64, key: [u8; 32], c: ActiveCall| {
        *revs += 1;
        let (mic_tx, mic_rx) = mpsc::channel(4);
        let (pcm_tx, pcm_rx) = mpsc::channel(PEER_QUEUE);
        tokio::spawn(link_loop(c, key, *revs, mic_rx, pcm_tx, me_tx.clone()));
        // A ligação antiga com a mesma pessoa cai junto com o `Link` que sai.
        links.insert(key, Link { rev: *revs, mic: mic_tx, pcm: pcm_rx });
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
                Some(RoomCmd::Leave) | None => break,
            },
        }
    }
    drop(links);
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
