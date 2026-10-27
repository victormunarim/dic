//! Quadros de servidor: 200 bytes, cifrados par a par.
//!
//! ```text
//! tag 8 ‖ servidor(4) ‖ remetente(8) ‖ tamanho(1) ‖ nonce(12) ‖ AEAD(tipo ‖ corpo) ‖ bytes aleatórios
//! ```
//!
//! A chave é `HKDF(DH(minha secreta, pública do outro), "clam/servidor/quadro/v1" ‖ server_id)`: só os dois
//! a calculam, e quem recebe sabe de quem é o quadro pelo remetente (os 8 primeiros bytes da chave dele) e
//! prova a cada quadro que a chave bate. Os primeiros bytes do servidor e do remetente só ajudam a achar a
//! chave certa; vão dentro do circuito, então só quem atende o telefone os vê.

// Tudo aqui lê quadros que qualquer um pode mandar: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use super::{MemberId, RoomId, ServerId, op::Op};
use crate::media::{packet::PACKET_LEN, signal::Frame, update::{Reach, valid_addr}};

pub const TAG_SERVER: u8 = 8;
/// Servidor fictício dos quadros de convite: quem recebe ainda não conhece o servidor, então a chave do quadro
/// usa este no lugar (o convite em si vai selado à parte, veja [`super::invite`]).
pub const INVITE_SERVER: ServerId = [0; 16];
const NONCE_LEN: usize = 12;
const HEAD: usize = 1 + 4 + 8 + 1;
/// Bytes que cabem no conteúdo de um quadro (tipo e corpo).
pub const BUDGET: usize = PACKET_LEN - HEAD - NONCE_LEN - 16;
/// Bytes do convite que cabem num quadro de pedaço de convite.
pub const INVITE_CHUNK: usize = BUDGET - 7;
/// Pedaços que um convite pode ter.
pub const INVITE_CHUNKS: usize = 8;
/// Autores que cabem num quadro de vetor.
pub const VECTOR_PER_FRAME: usize = (BUDGET - 4) / 10;
/// Maior endereço (`host:porta`) que o quadro de endereço leva.
pub const MAX_ADDR: usize = crate::media::update::MAX_ADDR;

/// O que um membro conta de si ao dizer que está online.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub room: Option<RoomId>,
    /// Resumo do registro (veja [`super::Registry::fingerprint`]).
    pub fp: [u8; 8],
    pub count: u16,
    /// Época do segredo que este lado tem.
    pub epoch: u16,
    /// Está transmitindo a tela para quem pedir (só vale dentro de uma sala).
    pub sharing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Hello(Hello),
    HelloAck(Hello),
    /// Até onde este lado tem as operações de cada autor; uma página de `pages`.
    Vector { page: u8, pages: u8, entries: Vec<(MemberId, u16)> },
    Op(Op),
    /// Endereço do próprio remetente.
    MyAddr { seq: u64, reach: Reach },
    /// Quem recebeu um convite pede para entrar, com a chave de verificação das suas assinaturas e o seu relay.
    Join { vk: [u8; 32], reach: Reach },
    /// O convite foi aceito; as operações vêm em seguida.
    Joined,
    /// O segredo novo do servidor, depois de um `Rekey`.
    Gift { epoch: u16, secret: [u8; 32] },
    /// Recebi o quadro cujo nonce começa por estes bytes (veja [`nonce_tag`]).
    Ack([u8; 8]),
    /// Quem está na mesma sala que o destinatário pede (`true`) ou deixa de pedir (`false`) para assistir à tela dele.
    Watch(bool),
    /// Um pedaço (`idx` de `total`) do convite `id`, que o convidante entrega ao contato sem ele precisar de outro canal.
    InviteChunk { id: [u8; 4], idx: u8, total: u8, data: Vec<u8> },
}

fn cipher(me: &StaticSecret, peer: &[u8; 32], server: &ServerId) -> Option<ChaCha20Poly1305> {
    let dh = me.diffie_hellman(&PublicKey::from(*peer));
    if !dh.was_contributory() {
        return None;
    }
    let mut info = b"clam/servidor/quadro/v1".to_vec();
    info.extend_from_slice(server);
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(None, dh.as_bytes()).expand(&info, &mut k).expect("32 bytes é válido");
    Some(ChaCha20Poly1305::new(Key::from_slice(&k)))
}

fn put_reach(v: &mut Vec<u8>, reach: &Reach) {
    v.push(reach.addr.len() as u8);
    v.extend_from_slice(reach.addr.as_bytes());
    v.extend_from_slice(&reach.onion);
    v.extend_from_slice(&reach.link);
}

fn take_reach(b: &[u8]) -> Option<Reach> {
    let (&n, rest) = b.split_first()?;
    let n = n as usize;
    if n == 0 || n > MAX_ADDR || rest.len() != n + 64 {
        return None;
    }
    let (addr, keys) = rest.split_at_checked(n)?;
    let (onion, link) = keys.split_at_checked(32)?;
    Some(Reach { addr: String::from_utf8(addr.to_vec()).ok().filter(|a| valid_addr(a))?, onion: onion.try_into().ok()?, link: link.try_into().ok()? })
}

/// Conteúdo de `msg` (tipo e corpo), antes de cifrar. Público para os alvos de fuzz de `fuzz/`.
pub fn encode(msg: &Msg) -> Vec<u8> {
    let mut v = Vec::new();
    match msg {
        Msg::Hello(h) | Msg::HelloAck(h) => {
            v.push(if matches!(msg, Msg::Hello(_)) { 1 } else { 2 });
            v.push(u8::from(h.room.is_some()) | u8::from(h.sharing) << 1);
            if let Some(r) = h.room {
                v.extend_from_slice(&r);
            }
            v.extend_from_slice(&h.fp);
            v.extend_from_slice(&h.count.to_be_bytes());
            v.extend_from_slice(&h.epoch.to_be_bytes());
        }
        Msg::Vector { page, pages, entries } => {
            v.extend_from_slice(&[3, *page, *pages, entries.len() as u8]);
            for (a, n) in entries {
                v.extend_from_slice(a);
                v.extend_from_slice(&n.to_be_bytes());
            }
        }
        Msg::Op(op) => {
            v.push(4);
            v.extend_from_slice(&op.encode());
        }
        Msg::MyAddr { seq, reach } => {
            v.push(5);
            v.extend_from_slice(&seq.to_be_bytes());
            put_reach(&mut v, reach);
        }
        Msg::Join { vk, reach } => {
            v.push(6);
            v.extend_from_slice(vk);
            put_reach(&mut v, reach);
        }
        Msg::Joined => v.push(7),
        Msg::Gift { epoch, secret } => {
            v.push(8);
            v.extend_from_slice(&epoch.to_be_bytes());
            v.extend_from_slice(secret);
        }
        Msg::Ack(tag) => {
            v.push(9);
            v.extend_from_slice(tag);
        }
        Msg::Watch(on) => v.extend_from_slice(&[10, u8::from(*on)]),
        Msg::InviteChunk { id, idx, total, data } => {
            v.push(11);
            v.extend_from_slice(id);
            v.extend_from_slice(&[*idx, *total]);
            v.extend_from_slice(data);
        }
    }
    v
}

/// Lê o conteúdo de um quadro já decifrado; `None` se o tipo ou o corpo não fazem sentido.
pub fn decode(b: &[u8]) -> Option<Msg> {
    let (&kind, b) = b.split_first()?;
    Some(match kind {
        1 | 2 => {
            let (&flags, rest) = b.split_first()?;
            if flags > 3 {
                return None;
            }
            let sharing = flags & 2 != 0;
            let (room, rest) = if flags & 1 == 1 {
                let (room, rest) = rest.split_at_checked(16)?;
                (Some(<RoomId>::try_from(room).ok()?), rest)
            } else {
                (None, rest)
            };
            if rest.len() != 12 {
                return None;
            }
            let h = Hello {
                room,
                fp: rest.get(..8)?.try_into().ok()?,
                count: u16::from_be_bytes(rest.get(8..10)?.try_into().ok()?),
                epoch: u16::from_be_bytes(rest.get(10..)?.try_into().ok()?),
                sharing,
            };
            if kind == 1 { Msg::Hello(h) } else { Msg::HelloAck(h) }
        }
        3 => {
            let [page, pages, n, rest @ ..] = b else { return None };
            if rest.len() != *n as usize * 10 || *n as usize > VECTOR_PER_FRAME || *pages == 0 || page >= pages {
                return None;
            }
            let entries = rest
                .as_chunks::<10>()
                .0
                .iter()
                .map(|c| {
                    let (author, n) = c.split_first_chunk::<8>()?;
                    Some((*author, u16::from_be_bytes(n.try_into().ok()?)))
                })
                .collect::<Option<Vec<_>>>()?;
            Msg::Vector { page: *page, pages: *pages, entries }
        }
        4 => Msg::Op(Op::decode(b)?),
        5 => Msg::MyAddr { seq: u64::from_be_bytes(b.get(..8)?.try_into().ok()?), reach: take_reach(b.get(8..)?)? },
        6 if b.len() > 32 => {
            let (vk, reach) = b.split_at_checked(32)?;
            Msg::Join { vk: vk.try_into().ok()?, reach: take_reach(reach)? }
        }
        7 if b.is_empty() => Msg::Joined,
        8 if b.len() == 34 => {
            let (epoch, secret) = b.split_at_checked(2)?;
            Msg::Gift { epoch: u16::from_be_bytes(epoch.try_into().ok()?), secret: secret.try_into().ok()? }
        }
        9 if b.len() == 8 => Msg::Ack(b.try_into().ok()?),
        10 => match b {
            [on @ (0 | 1)] => Msg::Watch(*on == 1),
            _ => return None,
        },
        11 => {
            let [i0, i1, i2, i3, idx, total, data @ ..] = b else { return None };
            if data.is_empty() || data.len() > INVITE_CHUNK || *total as usize > INVITE_CHUNKS || idx >= total {
                return None;
            }
            Msg::InviteChunk { id: [*i0, *i1, *i2, *i3], idx: *idx, total: *total, data: data.to_vec() }
        }
        _ => return None,
    })
}

/// Identifica um quadro para quem o confirma: os primeiros bytes do nonce.
pub fn nonce_tag(frame: &[u8]) -> Option<[u8; 8]> {
    frame.get(HEAD..HEAD + 8)?.try_into().ok()
}

/// Quadro de `msg` de `me` para `peer`, ou `None` se a chave do outro não serve ou a mensagem não cabe.
pub fn seal(me: &StaticSecret, peer: &[u8; 32], server: &ServerId, msg: &Msg) -> Option<Frame> {
    let plain = encode(msg);
    if plain.len() > BUDGET {
        return None;
    }
    let cipher = cipher(me, peer, server)?;
    let mut f = [0u8; PACKET_LEN];
    f[0] = TAG_SERVER;
    f[1..5].copy_from_slice(&server[..4]);
    f[5..13].copy_from_slice(&PublicKey::from(me).as_bytes()[..8]);
    f[13] = plain.len() as u8;
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    f[HEAD..HEAD + NONCE_LEN].copy_from_slice(&nonce);
    let sealed = cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &f[..HEAD] }).ok()?;
    let (used, padding) = f.get_mut(HEAD + NONCE_LEN..)?.split_at_mut_checked(sealed.len())?;
    used.copy_from_slice(&sealed);
    OsRng.fill_bytes(padding);
    Some(f)
}

/// De que servidor (4 primeiros bytes) e de quem (8 primeiros bytes da chave) é o quadro.
pub fn head(frame: &[u8]) -> Option<([u8; 4], MemberId)> {
    if frame.len() != PACKET_LEN || *frame.first()? != TAG_SERVER {
        return None;
    }
    Some((frame.get(1..5)?.try_into().ok()?, frame.get(5..13)?.try_into().ok()?))
}

/// Abre o quadro que `peer` mandou a `me`; `None` se não é dele ou foi adulterado.
pub fn open(me: &StaticSecret, peer: &[u8; 32], server: &ServerId, frame: &[u8]) -> Option<Msg> {
    let (srv, from) = head(frame)?;
    let len = *frame.get(13)? as usize;
    if len > BUDGET || srv[..] != server[..4] || from[..] != peer[..8] {
        return None;
    }
    let cipher = cipher(me, peer, server)?;
    let (aad, rest) = frame.split_at_checked(HEAD)?;
    let (nonce, rest) = rest.split_at_checked(NONCE_LEN)?;
    let sealed = rest.get(..len + 16)?;
    let plain = cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad }).ok()?;
    decode(&plain)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::server::op::{Action, Op};
    use ed25519_dalek::SigningKey;

    fn pair() -> ((StaticSecret, [u8; 32]), (StaticSecret, [u8; 32])) {
        let a = StaticSecret::random_from_rng(OsRng);
        let b = StaticSecret::random_from_rng(OsRng);
        let (pa, pb) = (*PublicKey::from(&a).as_bytes(), *PublicKey::from(&b).as_bytes());
        ((a, pa), (b, pb))
    }

    fn samples() -> Vec<Msg> {
        let hello = Hello { room: Some([3; 16]), fp: [1; 8], count: 9, epoch: 4, sharing: true };
        let op = Op::sign(&[7; 16], &SigningKey::from_bytes(&[1; 32]), [2; 8], 3, 4, Action::AddMember { key: [5; 32], vk: [6; 32] });
        vec![
            Msg::Hello(hello),
            Msg::HelloAck(Hello { room: None, ..hello }),
            Msg::Vector { page: 1, pages: 3, entries: (0..VECTOR_PER_FRAME as u8).map(|i| ([i; 8], 1000 + i as u16)).collect() },
            Msg::Op(op),
            Msg::MyAddr { seq: u64::MAX, reach: Reach { addr: format!("{}:4001", "a".repeat(MAX_ADDR - 5)), onion: [8; 32], link: [9; 32] } },
            Msg::Join { vk: [4; 32], reach: Reach { addr: format!("{}:4001", "a".repeat(MAX_ADDR - 5)), onion: [8; 32], link: [9; 32] } },
            Msg::Joined,
            Msg::Gift { epoch: 2, secret: [7; 32] },
            Msg::Ack([3; 8]),
            Msg::Watch(true),
            Msg::Watch(false),
            Msg::InviteChunk { id: [1, 2, 3, 4], idx: 2, total: 5, data: vec![9; INVITE_CHUNK] },
        ]
    }

    #[test]
    fn every_message_round_trips_between_the_pair_and_always_fits() {
        let ((a, pa), (b, pb)) = pair();
        for msg in samples() {
            let f = seal(&a, &pb, &[7; 16], &msg).expect("cabe");
            assert_eq!(head(&f), Some(([7; 4], pa[..8].try_into().unwrap())));
            assert_eq!(open(&b, &pa, &[7; 16], &f), Some(msg));
        }
    }

    #[test]
    fn only_the_pair_and_the_right_server_open_a_frame() {
        let ((a, pa), (b, pb)) = pair();
        let (c, _) = (StaticSecret::random_from_rng(OsRng), ());
        let f = seal(&a, &pb, &[7; 16], &Msg::Joined).unwrap();
        assert!(open(&c, &pa, &[7; 16], &f).is_none(), "terceiro");
        assert!(open(&b, &pa, &[8; 16], &f).is_none(), "outro servidor");
        assert!(open(&b, &pb, &[7; 16], &f).is_none(), "remetente trocado");
        for i in 0..crate::media::packet::PACKET_LEN {
            let mut bad = f;
            bad[i] ^= 1;
            assert!(open(&b, &pa, &[7; 16], &bad).is_none() || i >= HEAD + NONCE_LEN + 1 + 16, "byte {i}");
        }
    }

    #[test]
    fn the_frame_does_not_repeat_between_sends() {
        let ((a, _), (_, pb)) = pair();
        let msg = Msg::Joined;
        assert_ne!(seal(&a, &pb, &[7; 16], &msg), seal(&a, &pb, &[7; 16], &msg));
    }

    #[test]
    fn junk_never_panics() {
        let ((_, pa), (b, _)) = pair();
        let mut seed = 1u32;
        for _ in 0..2000 {
            let mut f = [0u8; 200];
            for x in f.iter_mut() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *x = (seed >> 24) as u8;
            }
            f[0] = TAG_SERVER;
            let _ = open(&b, &pa, &[7; 16], &f);
            let _ = head(&f);
            let _ = decode(&f[1..(f[13] as usize % 150).max(1) + 1]);
        }
    }
}
