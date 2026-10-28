//! Pacote de voz de **200 bytes sempre**: `seq (4, AAD) ‖ AEAD(tipo ‖ len ‖ quadro ‖ zeros)`.
//!
//! 200 B cabem nos 203 de uma célula de 2 saltos. Quadro curto ou longo, fala ou silêncio:
//! o pacote tem o mesmo tamanho. A chave é por direção e por chamada, então o `seq` serve
//! de nonce sem risco de reuso.

// O pacote de mídia chega da rede: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};

use super::{MediaError, keys::CallKeys};
use crate::bytes::Reader;

/// Tamanho do pacote no fio.
pub const PACKET_LEN: usize = 200;
const SEQ_LEN: usize = 4;
const TAG_LEN: usize = 16;
/// Texto claro cifrado: `tipo (1) ‖ len (1) ‖ quadro ‖ zeros`.
const PLAIN_LEN: usize = PACKET_LEN - SEQ_LEN - TAG_LEN;
/// Maior quadro codificado que cabe no pacote.
pub const MAX_PAYLOAD: usize = PLAIN_LEN - 2;

const KIND_VOICE: u8 = 0;
const KIND_HANGUP: u8 = 1;

/// O que um pacote autenticado carrega.
#[derive(Debug, PartialEq, Eq)]
pub enum Opened {
    Voice(Vec<u8>),
    /// O outro lado desligou. Vem autenticado: ninguém de fora derruba a chamada.
    Hangup,
}
/// Quantos pacotes atrás do mais novo ainda se aceitam fora de ordem.
pub const REPLAY_WINDOW: u32 = 128;

fn nonce(seq: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[8..].copy_from_slice(&seq.to_be_bytes());
    Nonce::from(n)
}

/// Cifra pacotes de saída.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    seq: u32,
}

impl Sealer {
    pub fn new(keys: &CallKeys) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&keys.send)), seq: 0 }
    }

    /// Próximo número de sequência (o do pacote que `seal` vai gerar).
    pub fn next_seq(&self) -> u32 {
        self.seq
    }

    pub fn seal(&mut self, frame: &[u8]) -> Result<[u8; PACKET_LEN], MediaError> {
        self.seal_kind(KIND_VOICE, frame)
    }

    /// Pacote de desligar: mesmo tamanho e mesma cifra de um de voz.
    pub fn seal_hangup(&mut self) -> Result<[u8; PACKET_LEN], MediaError> {
        self.seal_kind(KIND_HANGUP, &[])
    }

    fn seal_kind(&mut self, kind: u8, frame: &[u8]) -> Result<[u8; PACKET_LEN], MediaError> {
        if frame.len() > MAX_PAYLOAD {
            return Err(MediaError::TooLarge(frame.len()));
        }
        let seq = self.seq;
        let next = seq.checked_add(1).ok_or(MediaError::CounterExhausted)?;
        let mut plain = [0u8; PLAIN_LEN];
        plain[0] = kind;
        plain[1] = frame.len() as u8;
        plain.get_mut(2..2 + frame.len()).ok_or(MediaError::TooLarge(frame.len()))?.copy_from_slice(frame);
        let aad = seq.to_be_bytes();
        let ct = self
            .cipher
            .encrypt(&nonce(seq), Payload { msg: &plain, aad: &aad })
            .expect("cifrar em memória não falha");
        self.seq = next;
        let mut out = [0u8; PACKET_LEN];
        out[..SEQ_LEN].copy_from_slice(&aad);
        out[SEQ_LEN..].copy_from_slice(&ct);
        Ok(out)
    }
}

/// Decifra pacotes de entrada e rejeita repetidos.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    /// Maior `seq` autenticado até agora.
    top: Option<u32>,
    /// Bit `i` = `top - i` já foi visto.
    seen: u128,
}

impl Opener {
    pub fn new(keys: &CallKeys) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&keys.recv)), top: None, seen: 0 }
    }

    fn is_replay(&self, seq: u32) -> bool {
        match self.top {
            None => false,
            Some(top) if seq > top => false,
            Some(top) => {
                let age = top - seq;
                age >= REPLAY_WINDOW || self.seen >> age & 1 == 1
            }
        }
    }

    fn mark(&mut self, seq: u32) {
        match self.top {
            Some(top) if seq <= top => self.seen |= 1 << (top - seq),
            Some(top) => {
                let shift = seq - top;
                self.seen = if shift >= 128 { 1 } else { self.seen << shift | 1 };
                self.top = Some(seq);
            }
            None => {
                self.seen = 1;
                self.top = Some(seq);
            }
        }
    }

    /// Autentica e decifra. Só marca o `seq` como visto depois de autenticar, para que um
    /// pacote forjado não queime números.
    pub fn open(&mut self, packet: &[u8]) -> Result<(u32, Opened), MediaError> {
        let packet: &[u8; PACKET_LEN] = packet.try_into().map_err(|_| MediaError::Size)?;
        let (aad, body) = packet.split_at(SEQ_LEN);
        let seq = u32::from_be_bytes(aad.try_into().map_err(|_| MediaError::Size)?);
        if self.is_replay(seq) {
            return Err(MediaError::Replay);
        }
        let plain = self.cipher.decrypt(&nonce(seq), Payload { msg: body, aad }).map_err(|_| MediaError::Decrypt)?;
        self.mark(seq);
        let mut r = Reader::new(&plain);
        let (Some(kind), Some(len)) = (r.u8(), r.u8()) else { return Err(MediaError::Malformed) };
        if usize::from(len) > MAX_PAYLOAD {
            return Err(MediaError::Malformed);
        }
        match kind {
            KIND_VOICE => Ok((seq, Opened::Voice(r.take(len.into()).ok_or(MediaError::Malformed)?.to_vec()))),
            KIND_HANGUP => Ok((seq, Opened::Hangup)),
            _ => Err(MediaError::Malformed),
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::media::keys::{CallKeys, Ephemeral};
    use rand_core::OsRng;
    use x25519_dalek::{PublicKey, StaticSecret};

    fn pair() -> ((Sealer, Opener), (Sealer, Opener)) {
        let (a, b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
        let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap();
        let kb = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).unwrap();
        ((Sealer::new(&ka), Opener::new(&ka)), (Sealer::new(&kb), Opener::new(&kb)))
    }

    #[test]
    fn packet_is_always_200_bytes() {
        let ((mut s, _), _) = pair();
        for n in [0, 1, 80, 160, MAX_PAYLOAD] {
            assert_eq!(s.seal(&vec![7; n]).unwrap().len(), PACKET_LEN);
        }
        assert_eq!(s.seal(&[0; MAX_PAYLOAD + 1]).err(), Some(MediaError::TooLarge(MAX_PAYLOAD + 1)));
    }

    #[test]
    fn round_trip_both_directions() {
        let ((mut sa, _), (_, mut ob)) = pair();
        let p = sa.seal(b"oi").unwrap();
        assert_eq!(ob.open(&p).unwrap(), (0, Opened::Voice(b"oi".to_vec())));
        // Chaves de outra chamada não abrem este pacote.
        let (_, (_, mut other)) = pair();
        assert_eq!(other.open(&sa.seal(b"x").unwrap()).err(), Some(MediaError::Decrypt));
    }

    #[test]
    fn rejects_replay_and_old_packets() {
        let ((mut s, _), (_, mut o)) = pair();
        let p: Vec<_> = (0..300).map(|_| s.seal(b"a").unwrap()).collect();
        o.open(&p[5]).unwrap();
        assert_eq!(o.open(&p[5]).err(), Some(MediaError::Replay));
        o.open(&p[3]).unwrap(); // fora de ordem, dentro da janela
        o.open(&p[299]).unwrap();
        assert_eq!(o.open(&p[3]).err(), Some(MediaError::Replay));
        assert_eq!(o.open(&p[100]).err(), Some(MediaError::Replay)); // atrás da janela
        o.open(&p[250]).unwrap();
    }

    #[test]
    fn hangup_looks_like_voice_and_is_authenticated() {
        let ((mut s, _), (_, mut o)) = pair();
        let h = s.seal_hangup().unwrap();
        assert_eq!(h.len(), PACKET_LEN);
        let mut bad = h;
        bad[10] ^= 1;
        assert!(o.open(&bad).is_err());
        assert_eq!(o.open(&h).unwrap().1, Opened::Hangup);
    }

    #[test]
    fn tampering_is_rejected_and_does_not_burn_the_seq() {
        let ((mut s, _), (_, mut o)) = pair();
        let good = s.seal(b"voz").unwrap();
        for i in [0, 3, 4, 100, PACKET_LEN - 1] {
            let mut bad = good;
            bad[i] ^= 1;
            assert!(o.open(&bad).is_err(), "byte {i}");
        }
        assert_eq!(o.open(&good).unwrap().1, Opened::Voice(b"voz".to_vec()));
        assert_eq!(o.open(&good[..199]).err(), Some(MediaError::Size));
    }
}
