//! Convite para um servidor: um texto que só o convidado abre.
//!
//! ```text
//! clam-servidor:  hex( servidor(16) ‖ quem convida(32) ‖ convidado(32) ‖ nonce(12) ‖ AEAD(conteúdo) )
//! ```
//!
//! O conteúdo (segredo do servidor, época, chave de verificação do dono, nome e dicas de relay) vai
//! cifrado com `HKDF(DH(quem convida, convidado), "clam/servidor/convite/v1" ‖ servidor)`: quem pega o
//! texto no caminho não lê o segredo, e quem recebe o convite errado não consegue abri-lo.

// `Invite::open` lê um texto que qualquer um pode ter montado: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use super::{MAX_NAME, ServerError, ServerId, valid_name};
use crate::media::update::{MAX_ADDR, Reach, valid_addr};

pub const PREFIX: &str = "clam-servidor:";
const NONCE_LEN: usize = 12;
const HEAD: usize = 16 + 32 + 32;
/// Dicas de relay de outros membros que o convite leva, além de quem convida.
pub const MAX_HINTS: usize = 2;

/// Como alcançar um membro: a chave dele e o relay dele.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub key: [u8; 32],
    pub reach: Reach,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub server: ServerId,
    pub name: String,
    pub epoch: u16,
    pub secret: [u8; 32],
    pub owner_vk: [u8; 32],
    /// Quem pode usar o convite.
    pub invitee: [u8; 32],
    /// Quem convidou, com o relay dele: é a quem o convidado manda o `Join`.
    pub inviter: Hint,
    pub hints: Vec<Hint>,
}

fn cipher(me: &StaticSecret, peer: &[u8; 32], server: &ServerId) -> Option<ChaCha20Poly1305> {
    let dh = me.diffie_hellman(&PublicKey::from(*peer));
    if !dh.was_contributory() {
        return None;
    }
    let mut info = b"clam/servidor/convite/v1".to_vec();
    info.extend_from_slice(server);
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(None, dh.as_bytes()).expand(&info, &mut k).expect("32 bytes é válido");
    Some(ChaCha20Poly1305::new(Key::from_slice(&k)))
}

fn put_reach(v: &mut Vec<u8>, r: &Reach) {
    v.push(r.addr.len() as u8);
    v.extend_from_slice(r.addr.as_bytes());
    v.extend_from_slice(&r.onion);
    v.extend_from_slice(&r.link);
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, rest) = b.split_at_checked(n)?;
    *b = rest;
    Some(head)
}

fn take_u8(b: &mut &[u8]) -> Option<u8> {
    take(b, 1)?.first().copied()
}

fn take_reach(b: &mut &[u8]) -> Option<Reach> {
    let n = take_u8(b)? as usize;
    if n == 0 || n > MAX_ADDR {
        return None;
    }
    let addr = String::from_utf8(take(b, n)?.to_vec()).ok().filter(|a| valid_addr(a))?;
    Some(Reach { addr, onion: take(b, 32)?.try_into().ok()?, link: take(b, 32)?.try_into().ok()? })
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Invite {
    /// O texto do convite, cifrado de `inviter_secret` (dono de `self.inviter.key`) para `self.invitee`.
    pub fn seal(&self, inviter_secret: &StaticSecret) -> Option<String> {
        if PublicKey::from(inviter_secret).as_bytes() != &self.inviter.key || self.hints.len() > MAX_HINTS || !valid_name(&self.name) {
            return None;
        }
        let mut plain = Vec::new();
        plain.extend_from_slice(&self.epoch.to_be_bytes());
        plain.extend_from_slice(&self.secret);
        plain.extend_from_slice(&self.owner_vk);
        plain.push(self.name.len() as u8);
        plain.extend_from_slice(self.name.as_bytes());
        put_reach(&mut plain, &self.inviter.reach);
        plain.push(self.hints.len() as u8);
        for h in &self.hints {
            plain.extend_from_slice(&h.key);
            put_reach(&mut plain, &h.reach);
        }
        let mut raw = Vec::new();
        raw.extend_from_slice(&self.server);
        raw.extend_from_slice(&self.inviter.key);
        raw.extend_from_slice(&self.invitee);
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        raw.extend_from_slice(&nonce);
        let sealed = cipher(inviter_secret, &self.invitee, &self.server)?
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: raw.get(..HEAD)? })
            .ok()?;
        raw.extend_from_slice(&sealed);
        Some(format!("{PREFIX}{}", hex(&raw)))
    }

    /// Abre o convite com a identidade de quem o recebeu.
    pub fn open(text: &str, me: &StaticSecret) -> Result<Invite, ServerError> {
        let raw = unhex(text.trim().strip_prefix(PREFIX).ok_or(ServerError::BadInvite)?).ok_or(ServerError::BadInvite)?;
        if raw.len() < HEAD + NONCE_LEN + 16 {
            return Err(ServerError::BadInvite);
        }
        let (aad, rest) = raw.split_at_checked(HEAD).ok_or(ServerError::BadInvite)?;
        let (nonce, sealed) = rest.split_at_checked(NONCE_LEN).ok_or(ServerError::BadInvite)?;
        let head = || {
            let mut h = aad;
            Some((take(&mut h, 16)?.try_into().ok()?, take(&mut h, 32)?.try_into().ok()?, take(&mut h, 32)?.try_into().ok()?))
        };
        let (server, inviter, invitee): (ServerId, [u8; 32], [u8; 32]) = head().ok_or(ServerError::BadInvite)?;
        if PublicKey::from(me).as_bytes() != &invitee {
            return Err(ServerError::NotForYou);
        }
        let plain = cipher(me, &inviter, &server)
            .and_then(|c| c.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad }).ok())
            .ok_or(ServerError::BadInvite)?;
        Self::parse(server, inviter, invitee, &plain).ok_or(ServerError::BadInvite)
    }

    fn parse(server: ServerId, inviter: [u8; 32], invitee: [u8; 32], mut b: &[u8]) -> Option<Invite> {
        let b = &mut b;
        let epoch = u16::from_be_bytes(take(b, 2)?.try_into().ok()?);
        let secret = take(b, 32)?.try_into().ok()?;
        let owner_vk = take(b, 32)?.try_into().ok()?;
        let n = take_u8(b)? as usize;
        if n > MAX_NAME {
            return None;
        }
        let name = String::from_utf8(take(b, n)?.to_vec()).ok().filter(|n| valid_name(n))?;
        let inviter = Hint { key: inviter, reach: take_reach(b)? };
        let count = take_u8(b)? as usize;
        if count > MAX_HINTS {
            return None;
        }
        let mut hints = Vec::new();
        for _ in 0..count {
            let key = take(b, 32)?.try_into().ok()?;
            hints.push(Hint { key, reach: take_reach(b)? });
        }
        b.is_empty().then_some(Invite { server, name, epoch, secret, owner_vk, invitee, inviter, hints })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn reach(n: u8) -> Reach {
        Reach { addr: format!("relay{n}.exemplo:4001"), onion: [n; 32], link: [n + 1; 32] }
    }

    fn sample(inviter: &StaticSecret, invitee: &StaticSecret) -> Invite {
        Invite {
            server: [1; 16],
            name: "Amigos".into(),
            epoch: 2,
            secret: [3; 32],
            owner_vk: [4; 32],
            invitee: *PublicKey::from(invitee).as_bytes(),
            inviter: Hint { key: *PublicKey::from(inviter).as_bytes(), reach: reach(5) },
            hints: vec![Hint { key: [6; 32], reach: reach(6) }, Hint { key: [7; 32], reach: reach(7) }],
        }
    }

    #[test]
    fn only_the_invitee_opens_the_invite() {
        let (a, b, c) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let inv = sample(&a, &b);
        let text = inv.seal(&a).expect("sela");
        assert!(text.starts_with(PREFIX));
        assert_eq!(Invite::open(&text, &b), Ok(inv));
        assert_eq!(Invite::open(&text, &c), Err(ServerError::NotForYou));
        assert!(!text.contains(&hex(&[3; 32])), "o segredo não aparece em claro");
    }

    #[test]
    fn only_the_inviter_can_seal_it() {
        let (a, b, c) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        assert!(sample(&a, &b).seal(&c).is_none());
        let mut many = sample(&a, &b);
        many.hints.push(many.hints[0].clone());
        assert!(many.seal(&a).is_none());
    }

    #[test]
    fn tampered_or_malformed_text_is_refused() {
        let (a, b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let text = sample(&a, &b).seal(&a).unwrap();
        for i in (PREFIX.len()..text.len()).step_by(7) {
            let mut bad = text.clone().into_bytes();
            bad[i] = if bad[i] == b'0' { b'1' } else { b'0' };
            let bad = String::from_utf8(bad).unwrap();
            assert!(Invite::open(&bad, &b).is_err(), "posição {i}");
        }
        for junk in ["", "oi", "clam-servidor:", "clam-servidor:zz", &text[..text.len() - 2], &format!("{text}00")] {
            assert!(Invite::open(junk, &b).is_err());
        }
        assert!(Invite::open(&format!("  {text}\n"), &b).is_ok(), "espaços em volta não atrapalham");
    }
}
