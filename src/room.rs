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
    time::Duration,
};

use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::{
    directory,
    media::codec::G722,
    node::Node,
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

/// Liga para os membros que faltam e que são de quem tem a chave pública maior (o menor disca).
/// Devolve quantas chamadas completou.
pub async fn connect_missing(node: &Node, view: &RoomView, mesh: &Mesh, wait: Duration) -> usize {
    let me = node.public_key();
    let mut made = 0;
    for m in &view.members {
        if *m == me || me > *m || mesh.has(m) {
            continue;
        }
        if let Ok(call) = node.dial(*m, G722::new(), wait).await {
            mesh.add(*m, call);
            made += 1;
        }
    }
    made
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

type Queues = Arc<Mutex<HashMap<[u8; 32], VecDeque<Vec<i16>>>>>;

/// Chamadas ativas de uma sala: envia o microfone a todas e mistura o que chega.
#[derive(Default)]
pub struct Mesh {
    peers: Mutex<HashMap<[u8; 32], mpsc::Sender<Vec<i16>>>>,
    queues: Queues,
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

    /// Um quadro misturado (soma saturada) com o próximo de cada membro que tem algo. `None` se ninguém falou.
    pub fn mix(&self) -> Option<Vec<i16>> {
        let mut q = self.queues.lock().unwrap();
        let frames: Vec<Vec<i16>> = q.values_mut().filter_map(VecDeque::pop_front).collect();
        let len = frames.iter().map(Vec::len).max()?;
        let mut out = vec![0i32; len];
        for f in &frames {
            for (o, s) in out.iter_mut().zip(f) {
                *o += *s as i32;
            }
        }
        Some(out.into_iter().map(|x| x.clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect())
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
    fn mix_sums_with_saturation() {
        let mesh = Mesh::new();
        assert!(mesh.mix().is_none());
        {
            let mut q = mesh.queues.lock().unwrap();
            q.entry([1; 32]).or_default().push_back(vec![100, i16::MAX, -5]);
            q.entry([2; 32]).or_default().push_back(vec![23, 10, i16::MIN]);
        }
        assert_eq!(mesh.mix().unwrap(), vec![123, i16::MAX, i16::MIN]);
        assert!(mesh.mix().is_none());
    }
}
