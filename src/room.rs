//! Salas persistentes em malha (feature `net`).
//!
//! Uma sala é um segredo de 32 bytes que fica salvo na máquina de quem já entrou; quem o tem entra de
//! novo quando quiser. O diretório só guarda quem está presente (veja [`crate::directory::room_join`]):
//! a sala é identificada por uma chave Ed25519 derivada do segredo, e só quem a tem consegue entrar ou
//! listar. O primeiro membro vivo na lista é o anfitrião; se ele sai, o próximo assume.
//!
//! A voz continua de ponta a ponta entre cada par (malha completa): de cada par de membros, o de
//! chave pública menor liga para o outro. [`Mesh`] mantém as chamadas e mistura o que chega.

use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use ed25519_dalek::SigningKey;
use x25519_dalek::{PublicKey, StaticSecret};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::{
    directory,
    media::codec::G722,
    node::{Dialer, Node},
    phone::{ActiveCall, IncomingCall},
};

/// Quadros de um mesmo membro guardados à espera da mistura (200 ms): além disso o atraso só cresce.
const MAX_QUEUED_FRAMES: usize = 10;

/// Segredo novo para uma sala.
pub fn new_secret() -> [u8; 32] {
    let mut s = [0u8; 32];
    OsRng.fill_bytes(&mut s);
    s
}

/// Chave da sala, derivada do segredo. A pública identifica a sala no diretório; a secreta assina os pedidos.
pub fn room_key(secret: &[u8; 32]) -> SigningKey {
    let mut seed = [0u8; 32];
    Hkdf::<Sha256>::new(None, secret).expand(b"clam-room-key-v1", &mut seed).expect("32 bytes cabem no HKDF");
    SigningKey::from_bytes(&seed)
}

/// Chave que cifra o aviso do dono para um membro: DH entre as identidades dos dois (só eles a têm).
fn rekey_cipher(shared: &[u8; 32], old_room: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(Some(old_room), shared).expand(b"clam-room-rekey-v1", &mut k).expect("32 bytes cabem no HKDF");
    ChaCha20Poly1305::new(Key::from_slice(&k))
}

/// Aviso do dono `owner` para o membro `recipient`: o segredo novo, cifrado só para ele. Quem foi removido não
/// ganha aviso e, mesmo vendo o do diretório, não consegue abrir o dos outros. Tem [`directory::NOTICE_LEN`] bytes.
pub fn seal_rekey(owner: &StaticSecret, recipient: &[u8; 32], old_secret: &[u8; 32], new_secret: &[u8; 32]) -> Vec<u8> {
    let shared = owner.diffie_hellman(&PublicKey::from(*recipient));
    let old_room = room_key(old_secret).verifying_key().to_bytes();
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = rekey_cipher(shared.as_bytes(), &old_room)
        .encrypt(Nonce::from_slice(&nonce), &new_secret[..])
        .expect("cifrar 32 bytes não falha");
    [&nonce[..], &ct].concat()
}

/// Abre um aviso de [`seal_rekey`] com a identidade do membro. `None` se não for para ele ou for adulterado.
pub fn open_rekey(me: &StaticSecret, owner: &[u8; 32], old_secret: &[u8; 32], notice: &[u8]) -> Option<[u8; 32]> {
    if notice.len() != directory::NOTICE_LEN {
        return None;
    }
    let shared = me.diffie_hellman(&PublicKey::from(*owner));
    let old_room = room_key(old_secret).verifying_key().to_bytes();
    let pt = rekey_cipher(shared.as_bytes(), &old_room).decrypt(Nonce::from_slice(&notice[..12]), &notice[12..]).ok()?;
    pt.try_into().ok()
}

/// O dono troca o segredo da sala: gera um novo e deixa um aviso no diretório para cada membro de `keep`
/// (quem não estiver na lista fica de fora). Devolve o segredo novo; o dono entra nele com [`sync`].
pub async fn rotate(server: &str, owner: &StaticSecret, old_secret: &[u8; 32], keep: &[[u8; 32]]) -> io::Result<[u8; 32]> {
    let new = new_secret();
    let old_room = room_key(old_secret).verifying_key().to_bytes();
    for m in keep {
        directory::notice_put(server, owner, &old_room, m, &seal_rekey(owner, m, old_secret, &new)).await?;
    }
    Ok(new)
}

/// Membro: procura o aviso do dono para você e, se houver, devolve o segredo novo da sala.
pub async fn follow(server: &str, me: &StaticSecret, owner: &[u8; 32], old_secret: &[u8; 32]) -> io::Result<Option<[u8; 32]>> {
    let old_room = room_key(old_secret).verifying_key().to_bytes();
    let mine = PublicKey::from(me).to_bytes();
    let found = directory::notice_get(server, owner, &old_room, &mine).await?;
    Ok(found.and_then(|n| open_rekey(me, owner, old_secret, &n)))
}

/// Situação da sala depois de uma sincronização.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomView {
    /// Membros vivos, do anfitrião ao mais novo (inclui você).
    pub members: Vec<[u8; 32]>,
    pub host: [u8; 32],
    pub i_am_host: bool,
}

/// Entra (ou renova a presença) na sala e libera os outros membros para ligarem para você.
pub async fn sync(node: &Node, key: &SigningKey) -> io::Result<RoomView> {
    let me = node.public_key();
    directory::room_join(node.directory(), key, &me).await?;
    let members = directory::room_members(node.directory(), key).await?;
    node.allow(&members);
    let host = members.first().copied().unwrap_or(me);
    Ok(RoomView { host, i_am_host: host == me, members })
}

/// Sai da sala no diretório (os outros deixam de ver você sem esperar o prazo).
pub async fn leave(node: &Node, key: &SigningKey) -> io::Result<()> {
    directory::room_leave(node.directory(), key, &node.public_key()).await
}

/// Liga, em segundo plano e em paralelo, para os membros que faltam e que são de quem tem a chave pública
/// maior (o menor disca). Devolve quantas discagens iniciou; as chamadas completas entram em `mesh` sozinhas.
///
/// Nunca bloqueia: um membro ausente não atrasa os outros nem a sincronização. Cada membro tem uma discagem
/// por vez, e depois de uma falha só se tenta de novo após uma espera que cresce (2, 4, 8, 16 s). Por isso dá
/// para chamar a cada poucos segundos: uma chamada que cai volta logo, sem martelar o diretório.
pub fn connect_missing(dialer: &Dialer, view: &RoomView, mesh: &Mesh, wait: Duration) -> usize {
    let me = dialer.public_key();
    let mut started = 0;
    for m in &view.members {
        if *m == me || me > *m || mesh.has(m) || !mesh.begin_dial(m) {
            continue;
        }
        started += 1;
        let (dialer, mesh, m) = (dialer.clone(), mesh.clone(), *m);
        tokio::spawn(async move {
            match dialer.dial(m, G722::new(), wait).await {
                Ok(call) => {
                    mesh.add(m, call);
                    mesh.end_dial(&m, true);
                }
                Err(_) => mesh.end_dial(&m, false),
            }
        });
    }
    started
}

/// Atende um convite de um membro da sala e põe na malha. `false` se `inc` não é de um membro.
pub async fn accept_member(inc: IncomingCall, view: &RoomView, mesh: &Mesh) -> bool {
    let caller = *inc.caller().as_bytes();
    if !view.members.contains(&caller) {
        return false;
    }
    match inc.accept(G722::new()).await {
        Ok(call) => {
            mesh.add(caller, call);
            true
        }
        Err(_) => false,
    }
}

/// Espera inicial entre discagens a um membro que não atendeu; dobra a cada falha até 16 s.
const RETRY_BASE: Duration = Duration::from_secs(2);
/// Pico (de 32767) a partir do qual a mixagem reduz o ganho.
const LIMIT: f32 = 30000.0;

type Queues = Arc<Mutex<HashMap<[u8; 32], VecDeque<Vec<i16>>>>>;

/// Chamadas ativas de uma sala: envia o microfone a todas e mistura o que chega.
#[derive(Clone, Default)]
pub struct Mesh {
    peers: Arc<Mutex<HashMap<[u8; 32], mpsc::Sender<Vec<i16>>>>>,
    queues: Queues,
    /// Ganho do limitador da mixagem (1,0 = sem redução).
    gain: Arc<Mutex<f32>>,
    dials: Arc<Mutex<HashMap<[u8; 32], Dial>>>,
}

/// Estado da discagem a um membro: em andamento, ou esperando para tentar de novo depois de falhar.
struct Dial {
    busy: bool,
    failures: u32,
    retry_at: Instant,
}

impl Mesh {
    pub fn new() -> Self {
        Self::default()
    }

    /// Passa a cuidar de `call` com o membro `key`. Uma chamada anterior com a mesma chave é encerrada.
    pub fn add(&self, key: [u8; 32], mut call: ActiveCall) {
        let (tx, mut rx) = mpsc::channel::<Vec<i16>>(8);
        self.peers.lock().unwrap().insert(key, tx);
        let queues = self.queues.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = rx.recv() => match out {
                        Some(pcm) => if call.send(pcm).await.is_err() { break },
                        None => break,
                    },
                    inc = call.next_pcm() => match inc {
                        Some(pcm) => {
                            let mut q = queues.lock().unwrap();
                            let line = q.entry(key).or_default();
                            if line.len() >= MAX_QUEUED_FRAMES {
                                line.pop_front();
                            }
                            line.push_back(pcm);
                        }
                        None => break,
                    },
                }
            }
            queues.lock().unwrap().remove(&key);
            call.hangup();
        });
    }

    /// Reserva a discagem a `key`; `false` se já há uma em andamento ou se ainda é cedo para repetir.
    fn begin_dial(&self, key: &[u8; 32]) -> bool {
        let mut d = self.dials.lock().unwrap();
        let e = d.entry(*key).or_insert(Dial { busy: false, failures: 0, retry_at: Instant::now() });
        if e.busy || Instant::now() < e.retry_at {
            return false;
        }
        e.busy = true;
        true
    }

    fn end_dial(&self, key: &[u8; 32], ok: bool) {
        let mut d = self.dials.lock().unwrap();
        if ok {
            d.remove(key);
        } else if let Some(e) = d.get_mut(key) {
            e.busy = false;
            e.failures += 1;
            e.retry_at = Instant::now() + RETRY_BASE * (1 << e.failures.min(4).saturating_sub(1));
        }
    }

    /// Há chamada viva com `key`?
    pub fn has(&self, key: &[u8; 32]) -> bool {
        let mut p = self.peers.lock().unwrap();
        p.retain(|_, t| !t.is_closed());
        p.contains_key(key)
    }

    pub fn count(&self) -> usize {
        let mut p = self.peers.lock().unwrap();
        p.retain(|_, t| !t.is_closed());
        p.len()
    }

    /// Manda um quadro de voz a todos os membros conectados (quem está lento perde o quadro).
    pub fn send(&self, pcm: &[i16]) {
        for t in self.peers.lock().unwrap().values() {
            let _ = t.try_send(pcm.to_vec());
        }
    }

    /// Um quadro misturado com o próximo de cada membro que tem algo. `None` se ninguém falou.
    ///
    /// A soma passa por um limitador: se o pico passaria de [`LIMIT`], o ganho cai na hora para caber (sem
    /// cortar a onda) e volta devagar, em cerca de 200 ms, quando a soma baixa.
    pub fn mix(&self) -> Option<Vec<i16>> {
        let mut q = self.queues.lock().unwrap();
        let frames: Vec<Vec<i16>> = q.values_mut().filter_map(VecDeque::pop_front).collect();
        drop(q);
        let len = frames.iter().map(Vec::len).max()?;
        let mut sum = vec![0i32; len];
        for f in &frames {
            for (o, s) in sum.iter_mut().zip(f) {
                *o += *s as i32;
            }
        }
        let peak = sum.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0) as f32;
        let mut g = self.gain.lock().unwrap();
        let from = if *g == 0.0 { 1.0 } else { *g };
        let needed = if peak > LIMIT { LIMIT / peak } else { 1.0 };
        // Ataque instantâneo; solta por interpolação ao longo do quadro.
        let (start, end) = if needed < from { (needed, needed) } else { (from, from + (1.0 - from) * 0.1) };
        *g = end;
        let out = sum
            .into_iter()
            .enumerate()
            .map(|(i, x)| {
                let gain = start + (end - start) * i as f32 / len as f32;
                (x as f32 * gain).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
            })
            .collect();
        Some(out)
    }

    /// Desliga todas as chamadas.
    pub fn hangup(&self) {
        self.peers.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_key_is_deterministic_and_secret_dependent() {
        let (a, b) = (new_secret(), new_secret());
        assert_eq!(room_key(&a).to_bytes(), room_key(&a).to_bytes());
        assert_ne!(room_key(&a).verifying_key(), room_key(&b).verifying_key());
    }

    #[test]
    fn rekey_notice_opens_only_for_its_recipient() {
        let (owner, bob, eve) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let (old, new) = (new_secret(), new_secret());
        let (opub, bpub) = (PublicKey::from(&owner).to_bytes(), PublicKey::from(&bob).to_bytes());
        let n = seal_rekey(&owner, &bpub, &old, &new);
        assert_eq!(n.len(), directory::NOTICE_LEN);
        assert_eq!(open_rekey(&bob, &opub, &old, &n), Some(new));
        assert_eq!(open_rekey(&eve, &opub, &old, &n), None);
        assert_eq!(open_rekey(&bob, &opub, &new_secret(), &n), None);
    }

    #[test]
    fn mix_sums_quiet_voices_untouched() {
        let mesh = Mesh::new();
        assert!(mesh.mix().is_none());
        {
            let mut q = mesh.queues.lock().unwrap();
            q.entry([1; 32]).or_default().push_back(vec![100, 2000, -5]);
            q.entry([2; 32]).or_default().push_back(vec![23, 10, -4000]);
        }
        assert_eq!(mesh.mix().unwrap(), vec![123, 2010, -4005]);
        assert!(mesh.mix().is_none());
    }

    #[test]
    fn mix_limits_loud_sum_without_clipping_then_recovers() {
        let mesh = Mesh::new();
        let wave = |a: f64, n: usize| -> Vec<i16> { (0..320).map(|i| (a * (std::f64::consts::TAU * 440.0 * (n * 320 + i) as f64 / 16000.0).sin()) as i16).collect() };
        // Quatro vozes a 20000 somam 80000: sem limitador, achataria a onda.
        {
            let mut q = mesh.queues.lock().unwrap();
            for k in 0..4u8 {
                q.entry([k; 32]).or_default().push_back(wave(20000.0, 0));
            }
        }
        let out = mesh.mix().unwrap();
        let peak = out.iter().map(|x| x.unsigned_abs()).max().unwrap();
        assert!((29000..=30001).contains(&peak), "{peak}");
        // A forma da onda se preserva: razão constante com a soma ideal.
        let ideal: Vec<i32> = wave(20000.0, 0).iter().map(|x| *x as i32 * 4).collect();
        let k = ideal.iter().zip(&out).filter(|(i, _)| i.abs() > 40000).map(|(i, o)| *o as f64 / *i as f64);
        let k: Vec<f64> = k.collect();
        assert!(k.iter().all(|r| (r - k[0]).abs() < 0.01));
        // Voz baixa depois: o ganho volta a 1 em poucos quadros.
        let mut last = 0.0;
        for n in 1..40 {
            mesh.queues.lock().unwrap().entry([9; 32]).or_default().push_back(wave(1000.0, n));
            let o = mesh.mix().unwrap();
            last = o.iter().map(|x| x.unsigned_abs()).max().unwrap() as f64 / 1000.0;
        }
        assert!(last > 0.97, "{last}");
    }
}
