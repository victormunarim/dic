//! Sinalização da chamada: convite, aceite, recusa. *Sans-IO*; cada mensagem é um quadro de
//! **200 bytes** (o mesmo tamanho do pacote de voz).
//!
//! ```text
//! chamador                                    chamado
//!   Invite: tag 1 ‖ e_i ‖ AEAD(s_i ‖ hora)  ─>
//!                                           <─  Accept: tag 2 ‖ e_r      (ou Reject: tag 3)
//!   [pacotes de voz de 200 B nos dois sentidos, chaves de `CallKeys`]
//! ```
//!
//! - O convite cifra a identidade do chamador (`s_i`) com uma chave de `DH(e_i, s_r)`: o relay
//!   de saída, que vê estes quadros, **não** descobre quem liga. Vê só que é um convite.
//! - O chamado confere se `s_i` é um contato conhecido (quem chama decide isso: [`Incoming::caller`]).
//! - O convite não prova quem chama (qualquer um pode escrever um `s_i` alheio), mas quem não
//!   tem a chave secreta de `s_i` não deriva as chaves da chamada ([`CallKeys`]): o aceite
//!   vai para o vazio e a voz nunca autentica.
//! - A hora no convite (segundos Unix) limita a repetição de convites gravados a [`INVITE_TTL_SECS`].
//!   Dentro desse prazo o replay só faz o telefone tocar; não dá acesso à chamada.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use super::{
    MediaError,
    keys::{CallKeys, Ephemeral},
    packet::PACKET_LEN,
};

/// Idade máxima aceita para um convite.
pub const INVITE_TTL_SECS: u64 = 60;

const TAG_INVITE: u8 = 1;
const TAG_ACCEPT: u8 = 2;
const TAG_REJECT: u8 = 3;
const SEALED_LEN: usize = 32 + 8 + 16;

/// Um quadro de sinalização, pronto para ir ao circuito.
pub type Frame = [u8; PACKET_LEN];

fn invite_key(dh: &[u8; 32], e_i: &[u8; 32], s_r: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut info = e_i.to_vec();
    info.extend_from_slice(s_r);
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(Some(b"clam/v1/invite"), dh).expand(&info, &mut k).expect("32 bytes é válido");
    ChaCha20Poly1305::new(Key::from_slice(&k))
}

/// Quem liga: guarda a efêmera até a resposta.
pub struct Caller {
    eph: Ephemeral,
    peer: PublicKey,
}

/// Resposta do chamado.
pub enum Answer {
    Accepted(CallKeys),
    Rejected,
}

impl Caller {
    /// Monta o convite para `peer` (a chave do pareamento). `now_secs`: relógio Unix.
    pub fn invite(my_static: &StaticSecret, peer: &PublicKey, now_secs: u64) -> Result<(Self, Frame), MediaError> {
        let eph = Ephemeral::generate();
        let dh = eph.diffie_hellman(peer)?;
        let cipher = invite_key(&dh, &eph.public(), peer.as_bytes());
        let mut plain = [0u8; 40];
        plain[..32].copy_from_slice(PublicKey::from(my_static).as_bytes());
        plain[32..].copy_from_slice(&now_secs.to_be_bytes());
        let sealed = cipher.encrypt(&Nonce::default(), plain.as_slice()).expect("cifrar em memória não falha");
        let mut f = [0u8; PACKET_LEN];
        f[0] = TAG_INVITE;
        f[1..33].copy_from_slice(&eph.public());
        f[33..33 + SEALED_LEN].copy_from_slice(&sealed);
        Ok((Self { eph, peer: *peer }, f))
    }

    /// Interpreta a resposta. Aceite de quem não é o chamado deriva chaves erradas; o primeiro
    /// pacote de voz falha na autenticação.
    pub fn answer(&self, my_static: &StaticSecret, frame: &[u8]) -> Result<Answer, MediaError> {
        let frame: &Frame = frame.try_into().map_err(|_| MediaError::Size)?;
        match frame[0] {
            TAG_REJECT => Ok(Answer::Rejected),
            TAG_ACCEPT => {
                let e_r: [u8; 32] = frame[1..33].try_into().expect("32 bytes");
                CallKeys::initiator(my_static, &self.eph, &self.peer, &e_r).map(Answer::Accepted)
            }
            _ => Err(MediaError::Malformed),
        }
    }
}

/// Convite recebido, ainda sem resposta.
pub struct Incoming {
    caller: PublicKey,
    eph: [u8; 32],
}

impl Incoming {
    /// Abre um quadro que pode ser um convite para `my_static`. Erro = não é um convite
    /// para esta chave (ou é velho demais): ignore o quadro.
    pub fn parse(my_static: &StaticSecret, now_secs: u64, frame: &[u8]) -> Result<Self, MediaError> {
        let frame: &Frame = frame.try_into().map_err(|_| MediaError::Size)?;
        if frame[0] != TAG_INVITE {
            return Err(MediaError::Malformed);
        }
        let eph: [u8; 32] = frame[1..33].try_into().expect("32 bytes");
        let dh = my_static.diffie_hellman(&PublicKey::from(eph));
        if !dh.was_contributory() {
            return Err(MediaError::BadKey);
        }
        let me = PublicKey::from(my_static);
        let plain = invite_key(dh.as_bytes(), &eph, me.as_bytes())
            .decrypt(&Nonce::default(), &frame[33..33 + SEALED_LEN])
            .map_err(|_| MediaError::Decrypt)?;
        let sent = u64::from_be_bytes(plain[32..40].try_into().expect("8 bytes"));
        if now_secs.abs_diff(sent) > INVITE_TTL_SECS {
            return Err(MediaError::Stale);
        }
        let caller: [u8; 32] = plain[..32].try_into().expect("32 bytes");
        Ok(Self { caller: PublicKey::from(caller), eph })
    }

    /// Chave estática que o chamador diz ter. Compare com a lista de contatos antes de aceitar.
    pub fn caller(&self) -> &PublicKey {
        &self.caller
    }

    /// Aceita: devolve o quadro de aceite e as chaves da chamada.
    pub fn accept(self, my_static: &StaticSecret) -> Result<(Frame, CallKeys), MediaError> {
        let mine = Ephemeral::generate();
        let keys = CallKeys::responder(my_static, &mine, &self.caller, &self.eph)?;
        let mut f = [0u8; PACKET_LEN];
        f[0] = TAG_ACCEPT;
        f[1..33].copy_from_slice(&mine.public());
        Ok((f, keys))
    }

    /// Recusa: não deriva chave nenhuma.
    pub fn reject(self) -> Frame {
        let mut f = [0u8; PACKET_LEN];
        f[0] = TAG_REJECT;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;
    use crate::media::packet::{Opened, Opener, Sealer};

    fn ids() -> (StaticSecret, StaticSecret) {
        (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng))
    }

    #[test]
    fn invite_accept_then_voice() {
        let (a, b) = ids();
        let (caller, inv) = Caller::invite(&a, &PublicKey::from(&b), 1000).unwrap();
        let incoming = Incoming::parse(&b, 1010, &inv).unwrap();
        assert_eq!(incoming.caller().as_bytes(), PublicKey::from(&a).as_bytes());
        let (acc, kb) = incoming.accept(&b).unwrap();
        let Answer::Accepted(ka) = caller.answer(&a, &acc).unwrap() else { panic!("aceite esperado") };
        let (mut s, mut o) = (Sealer::new(&ka), Opener::new(&kb));
        assert_eq!(o.open(&s.seal(b"oi").unwrap()).unwrap().1, Opened::Voice(b"oi".to_vec()));
    }

    #[test]
    fn all_signals_have_the_packet_size() {
        let (a, b) = ids();
        let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let (acc, _) = Incoming::parse(&b, 0, &inv).unwrap().accept(&b).unwrap();
        assert_eq!((inv.len(), acc.len()), (PACKET_LEN, PACKET_LEN));
    }

    #[test]
    fn reject_is_understood() {
        let (a, b) = ids();
        let (caller, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let rej = Incoming::parse(&b, 0, &inv).unwrap().reject();
        assert!(matches!(caller.answer(&a, &rej), Ok(Answer::Rejected)));
    }

    #[test]
    fn invite_hides_the_caller_identity() {
        let (a, b) = ids();
        let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let id = PublicKey::from(&a);
        assert!(!inv.windows(32).any(|w| w == id.as_bytes()));
    }

    #[test]
    fn other_recipients_stale_and_tampered_invites_are_rejected() {
        let (a, b) = ids();
        let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), 1000).unwrap();
        let other = StaticSecret::random_from_rng(OsRng);
        assert_eq!(Incoming::parse(&other, 1000, &inv).err(), Some(MediaError::Decrypt));
        assert_eq!(Incoming::parse(&b, 1000 + INVITE_TTL_SECS + 1, &inv).err(), Some(MediaError::Stale));
        let mut bad = inv;
        bad[40] ^= 1;
        assert!(Incoming::parse(&b, 1000, &bad).is_err());
        assert_eq!(Incoming::parse(&b, 1000, &inv[..100]).err(), Some(MediaError::Size));
        assert_eq!(Incoming::parse(&b, 1000, &[2u8; PACKET_LEN]).err(), Some(MediaError::Malformed));
    }

    #[test]
    fn caller_answering_with_another_static_key_gets_no_working_keys() {
        let (a, b) = ids();
        let other = StaticSecret::random_from_rng(OsRng);
        let (caller, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let (acc, kb) = Incoming::parse(&b, 0, &inv).unwrap().accept(&b).unwrap();
        // Quem tem o estado do chamador mas não a chave secreta `a` deriva chaves erradas.
        let Answer::Accepted(kx) = caller.answer(&other, &acc).unwrap() else { panic!() };
        let (mut s, mut o) = (Sealer::new(&kx), Opener::new(&kb));
        assert_eq!(o.open(&s.seal(b"x").unwrap()).err(), Some(MediaError::Decrypt));
    }
}
