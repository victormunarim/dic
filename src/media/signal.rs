//! Sinalização da chamada: convite, aceite, recusa. *Sans-IO*; cada mensagem é um quadro de
//! **200 bytes** (o mesmo tamanho do pacote de voz).
//!
//! ```text
//! chamador                                    chamado
//!   Invite: tag 1 ‖ e_i ‖ AEAD(s_i ‖ prova de autoria) ‖ AEAD(dica de relay)  ─>
//!                                           <─  Accept: tag 2 ‖ e_r      (ou Reject: tag 3)
//!   [pacotes de voz de 200 B nos dois sentidos, chaves de `CallKeys`]
//! ```
//!
//! - O convite cifra a identidade do chamador (`s_i`) com uma chave de `DH(e_i, s_r)`: o relay
//!   de saída, que vê estes quadros, **não** descobre quem liga. Vê só que é um convite.
//! - O chamado confere se `s_i` é um contato conhecido (quem chama decide isso: [`Incoming::caller`]).
//! - O convite prova quem chama: junto de `s_i` vai uma prova de autoria, derivada de `DH(s_i, s_r)`
//!   e amarrada à efêmera do convite ([`author_proof`]). Quem não tem a chave secreta de `s_i` não
//!   a calcula, então ninguém faz o telefone tocar em nome de um contato; [`Incoming::parse`] recusa
//!   o convite sem ela. Além disso, quem não tem essa chave não deriva as chaves da chamada
//!   ([`CallKeys`]), e a voz nunca autentica.
//! - O convite não depende do relógio de quem recebe: duas máquinas com a hora desencontrada
//!   ainda se ligam. A repetição de um convite gravado é
//!   barrada por quem recebe, que lembra as efêmeras já vistas ([`Incoming::nonce`]); sem
//!   essa memória (telefone reiniciado) o replay só faz o telefone tocar, não dá acesso à
//!   chamada. O lugar onde ia a hora de quem liga (que nenhum lado confere mais) leva a prova de autoria;
//!   por isso telefones de versão anterior, que conferiam a hora, não falam com os de agora.
//! - O convite pode levar uma **dica de relay** ([`RelayHint`]): o endereço e as chaves do relay de
//!   quem liga, num segundo bloco cifrado com a mesma chave (nonce diferente). Quem atende guarda a
//!   dica para ligar de volta. Telefones de versão anterior não olham esses
//!   bytes; sem dica o bloco é aleatório e [`Incoming::relay_hint`] devolve `None`. Como `s_i`, a dica
//!   não é autenticada: quem a recebe só a usa depois do endereço guardado, da DHT e das âncoras falharem, e só a
//!   promove a endereço guardado depois de uma chamada autenticada por ela.
//! - O convite diz o **codec** da chamada ([`Incoming::codec`]) nos 12 bytes finais: sorteio de 4 bytes
//!   ‖ 8 bytes de PRF da chave do convite. G.722, o padrão, não leva etiqueta (os bytes são aleatórios,
//!   como num telefone antigo); outro codec leva, e só quem tem a chave do convite a verifica.
//!   Telefone antigo não olha esses bytes e atende em G.722: ligar em outro codec exige os dois
//!   lados novos.

// `Incoming::parse` e as demais leituras do convite leem bytes que qualquer um pode ter montado: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::net::{IpAddr, SocketAddr};

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};

use super::{
    MediaError,
    codec::CodecId,
    keys::{CallKeys, Ephemeral},
    packet::PACKET_LEN,
};

const TAG_INVITE: u8 = 1;
const TAG_ACCEPT: u8 = 2;
const TAG_REJECT: u8 = 3;
const SEALED_LEN: usize = 32 + 8 + 16;
/// Dica de relay em claro: onion ‖ link ‖ família (4 ou 6) ‖ IP (16, completado com zeros) ‖ porta.
const HINT_LEN: usize = 32 + 32 + 1 + 16 + 2;
const HINT_SEALED_LEN: usize = HINT_LEN + 16;
const HINT_AT: usize = 33 + SEALED_LEN;
/// Onde começam os bytes finais do convite: aleatórios, ou a etiqueta do codec (veja [`codec_tag`]).
const TAIL_AT: usize = HINT_AT + HINT_SEALED_LEN;
/// Bytes da etiqueta do codec: um sorteio e o código de autenticação dele.
const TAG_SALT_LEN: usize = 4;
const TAG_MAC_LEN: usize = 8;
/// Bytes da prova de autoria do convite (veja [`author_proof`]).
const PROOF_LEN: usize = 8;

/// Onde chegar ao relay de quem liga: o que o chamado precisa para ligar de volta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayHint {
    pub addr: SocketAddr,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

impl RelayHint {
    fn encode(&self) -> [u8; HINT_LEN] {
        let mut b = [0u8; HINT_LEN];
        b[..32].copy_from_slice(&self.onion);
        b[32..64].copy_from_slice(&self.link);
        match self.addr.ip() {
            IpAddr::V4(ip) => {
                b[64] = 4;
                b[65..69].copy_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                b[64] = 6;
                b[65..81].copy_from_slice(&ip.octets());
            }
        }
        b[81..].copy_from_slice(&self.addr.port().to_be_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; HINT_LEN] = b.try_into().ok()?;
        let ip = match b[64] {
            4 => IpAddr::from(<[u8; 4]>::try_from(&b[65..69]).ok()?),
            6 => IpAddr::from(<[u8; 16]>::try_from(&b[65..81]).ok()?),
            _ => return None,
        };
        let port = u16::from_be_bytes([b[81], b[82]]);
        Some(Self { addr: SocketAddr::new(ip, port), onion: b[..32].try_into().ok()?, link: b[32..64].try_into().ok()? })
    }
}

/// Nonce do bloco da dica (o do bloco da identidade é zero; a chave é a mesma, então não podem coincidir).
fn hint_nonce() -> Nonce {
    let mut n = [0u8; 12];
    n[11] = 1;
    n.into()
}

/// Etiqueta que diz o codec nos bytes finais do convite: `sorteio ‖ PRF(chave do convite, sorteio ‖ codec)`.
///
/// Só quem tem a chave do convite verifica, e sem ela a etiqueta é indistinguível dos bytes
/// aleatórios que o convite de G.722 leva no lugar (por isso G.722, o padrão, não leva etiqueta e
/// continua igual ao de um telefone antigo). Telefone antigo ignora esses bytes e atende em G.722.
fn codec_tag(dh: &[u8; 32], e_i: &[u8; 32], s_r: &[u8; 32], salt: &[u8], codec: CodecId) -> [u8; TAG_MAC_LEN] {
    let mut info = e_i.to_vec();
    info.extend_from_slice(s_r);
    info.extend_from_slice(salt);
    info.push(codec.wire());
    let mut mac = [0u8; TAG_MAC_LEN];
    Hkdf::<Sha256>::new(Some(b"clam/v1/invite-codec"), dh).expand(&info, &mut mac).expect("8 bytes é válido");
    mac
}

/// Prova de que o dono de `s_i` fez o convite: `PRF(DH(s_i, s_r), e_i ‖ s_r)`. Só quem tem a chave secreta
/// de `s_i` (ou a de `s_r`) a calcula; o chamado a confere com a chave pública que o convite diz ser do chamador.
fn author_proof(dh: &[u8; 32], e_i: &[u8; 32], s_r: &[u8; 32]) -> [u8; PROOF_LEN] {
    let mut info = e_i.to_vec();
    info.extend_from_slice(s_r);
    let mut proof = [0u8; PROOF_LEN];
    Hkdf::<Sha256>::new(Some(b"clam/v1/invite-author"), dh).expand(&info, &mut proof).expect("8 bytes é válido");
    proof
}

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
    /// Monta o convite para `peer` (a chave do pareamento). `now_secs` não é mais usado (ficou na assinatura).
    pub fn invite(my_static: &StaticSecret, peer: &PublicKey, now_secs: u64) -> Result<(Self, Frame), MediaError> {
        Self::invite_with_hint(my_static, peer, now_secs, None)
    }

    /// Como [`Caller::invite`], levando junto o relay de quem liga (veja [`RelayHint`]).
    pub fn invite_with_hint(my_static: &StaticSecret, peer: &PublicKey, now_secs: u64, hint: Option<&RelayHint>) -> Result<(Self, Frame), MediaError> {
        Self::invite_codec(my_static, peer, now_secs, hint, CodecId::default())
    }

    /// Como [`Caller::invite_with_hint`], dizendo qual codec a chamada usa (o do chamado vem em
    /// [`Incoming::codec`]). Quem não lê a indicação (telefone antigo) atende em G.722.
    pub fn invite_codec(
        my_static: &StaticSecret,
        peer: &PublicKey,
        _now_secs: u64,
        hint: Option<&RelayHint>,
        codec: CodecId,
    ) -> Result<(Self, Frame), MediaError> {
        let eph = Ephemeral::generate();
        let dh = eph.diffie_hellman(peer)?;
        let cipher = invite_key(&dh, &eph.public(), peer.as_bytes());
        let author = my_static.diffie_hellman(peer);
        if !author.was_contributory() {
            return Err(MediaError::BadKey);
        }
        let mut plain = [0u8; 40];
        plain[..32].copy_from_slice(PublicKey::from(my_static).as_bytes());
        plain[32..].copy_from_slice(&author_proof(author.as_bytes(), &eph.public(), peer.as_bytes()));
        let sealed = cipher.encrypt(&Nonce::default(), plain.as_slice()).expect("cifrar em memória não falha");
        let mut f = [0u8; PACKET_LEN];
        f[0] = TAG_INVITE;
        f[1..33].copy_from_slice(&eph.public());
        f[33..HINT_AT].copy_from_slice(&sealed);
        // Sem dica, o bloco é aleatório: quem vê o quadro não distingue se há dica.
        match hint {
            Some(h) => {
                let sealed = cipher.encrypt(&hint_nonce(), h.encode().as_slice()).expect("cifrar em memória não falha");
                f[HINT_AT..HINT_AT + HINT_SEALED_LEN].copy_from_slice(&sealed);
            }
            None => OsRng.fill_bytes(&mut f[HINT_AT..HINT_AT + HINT_SEALED_LEN]),
        }
        OsRng.fill_bytes(&mut f[TAIL_AT..]);
        if codec != CodecId::default() {
            let (salt, mac) = f[TAIL_AT..TAIL_AT + TAG_SALT_LEN + TAG_MAC_LEN].split_at_mut(TAG_SALT_LEN);
            mac.copy_from_slice(&codec_tag(&dh, &eph.public(), peer.as_bytes(), salt, codec));
        }
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
    hint: Option<RelayHint>,
    codec: CodecId,
}

impl Incoming {
    /// Abre um quadro que pode ser um convite para `my_static`. Erro = não é um convite
    /// para esta chave: ignore o quadro. Não olha o relógio; use [`Incoming::nonce`] contra repetição.
    pub fn parse(my_static: &StaticSecret, frame: &[u8]) -> Result<Self, MediaError> {
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
        let cipher = invite_key(dh.as_bytes(), &eph, me.as_bytes());
        let plain = cipher.decrypt(&Nonce::default(), frame.get(33..HINT_AT).ok_or(MediaError::Size)?).map_err(|_| MediaError::Decrypt)?;
        let (caller, proof) = plain.split_at_checked(32).ok_or(MediaError::Malformed)?;
        let caller: [u8; 32] = caller.try_into().map_err(|_| MediaError::Malformed)?;
        let author = my_static.diffie_hellman(&PublicKey::from(caller));
        if !author.was_contributory() || !bool::from(author_proof(author.as_bytes(), &eph, me.as_bytes()).ct_eq(proof)) {
            return Err(MediaError::BadKey);
        }
        // Convite de versão anterior ou sem dica: o bloco não autentica e fica `None`.
        let hint = frame.get(HINT_AT..HINT_AT + HINT_SEALED_LEN).and_then(|sealed| cipher.decrypt(&hint_nonce(), sealed).ok()).and_then(|p| RelayHint::decode(&p));
        // Sem etiqueta que confira (telefone antigo, ou o padrão): G.722.
        let (salt, mac) = frame
            .get(TAIL_AT..TAIL_AT + TAG_SALT_LEN + TAG_MAC_LEN)
            .and_then(|tail| tail.split_at_checked(TAG_SALT_LEN))
            .ok_or(MediaError::Size)?;
        let codec = CodecId::ALL
            .into_iter()
            .filter(|c| *c != CodecId::default())
            .find(|c| bool::from(codec_tag(dh.as_bytes(), &eph, me.as_bytes(), salt, *c).ct_eq(mac)))
            .unwrap_or_default();
        Ok(Self { caller: PublicKey::from(caller), eph, hint, codec })
    }

    /// Codec que quem ligou escolheu. Não é uma negociação: quem atende usa este, ou recusa.
    pub fn codec(&self) -> CodecId {
        self.codec
    }

    /// Relay de quem ligou, se o convite o trouxe. Não é autenticado (veja o módulo): é só uma dica.
    pub fn relay_hint(&self) -> Option<&RelayHint> {
        self.hint.as_ref()
    }

    /// Efêmera do convite, única por convite: quem recebe a guarda para recusar o mesmo convite de novo.
    pub fn nonce(&self) -> [u8; 32] {
        self.eph
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
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
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
        let incoming = Incoming::parse(&b, &inv).unwrap();
        assert_eq!(incoming.caller().as_bytes(), PublicKey::from(&a).as_bytes());
        let (acc, kb) = incoming.accept(&b).unwrap();
        let Answer::Accepted(ka) = caller.answer(&a, &acc).unwrap() else { panic!("aceite esperado") };
        let (mut s, mut o) = (Sealer::new(&ka), Opener::new(&kb));
        assert_eq!(o.open(&s.seal(b"oi").unwrap()).unwrap().1, Opened::Voice(b"oi".to_vec()));
    }

    #[test]
    fn the_invite_does_not_depend_on_either_clock() {
        let (a, b) = ids();
        for sent in [0, 1000, u64::MAX / 2] {
            let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), sent).unwrap();
            assert_eq!(Incoming::parse(&b, &inv).unwrap().caller().as_bytes(), PublicKey::from(&a).as_bytes());
        }
    }

    #[test]
    fn each_invite_has_its_own_nonce() {
        let (a, b) = ids();
        let n = |_| Incoming::parse(&b, &Caller::invite(&a, &PublicKey::from(&b), 0).unwrap().1).unwrap().nonce();
        assert_ne!(n(0), n(1));
    }

    fn hint() -> RelayHint {
        RelayHint { addr: "192.168.15.10:4001".parse().unwrap(), onion: [7; 32], link: [9; 32] }
    }

    #[test]
    fn the_invite_carries_the_relay_of_the_caller() {
        let (a, b) = ids();
        for addr in ["192.168.15.10:4001", "[2001:db8::1]:65535"] {
            let h = RelayHint { addr: addr.parse().unwrap(), ..hint() };
            let (caller, inv) = Caller::invite_with_hint(&a, &PublicKey::from(&b), 0, Some(&h)).unwrap();
            let incoming = Incoming::parse(&b, &inv).unwrap();
            assert_eq!(incoming.relay_hint(), Some(&h));
            // A dica não atrapalha o resto do convite.
            let (acc, _) = incoming.accept(&b).unwrap();
            assert!(matches!(caller.answer(&a, &acc), Ok(Answer::Accepted(_))));
        }
    }

    #[test]
    fn an_invite_without_hint_or_from_an_old_phone_gives_none() {
        let (a, b) = ids();
        let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        assert_eq!(Incoming::parse(&b, &inv).unwrap().relay_hint(), None);
        // Telefone antigo: o que vem depois do bloco da identidade é zero.
        let mut old = inv;
        old[HINT_AT..].fill(0);
        let incoming = Incoming::parse(&b, &old).unwrap();
        assert_eq!((incoming.caller().as_bytes(), incoming.relay_hint()), (PublicKey::from(&a).as_bytes(), None));
    }

    #[test]
    fn a_tampered_hint_is_dropped_but_the_invite_still_works() {
        let (a, b) = ids();
        let (_, mut inv) = Caller::invite_with_hint(&a, &PublicKey::from(&b), 0, Some(&hint())).unwrap();
        inv[HINT_AT + 3] ^= 1;
        let incoming = Incoming::parse(&b, &inv).unwrap();
        assert_eq!(incoming.relay_hint(), None);
    }

    #[test]
    fn the_hint_stays_hidden_from_the_exit_relay() {
        let (a, b) = ids();
        let h = hint();
        let (_, inv) = Caller::invite_with_hint(&a, &PublicKey::from(&b), 0, Some(&h)).unwrap();
        assert!(!inv.windows(32).any(|w| w == h.onion || w == h.link));
        assert!(!inv.windows(4).any(|w| w == [192, 168, 15, 10]));
    }

    #[test]
    fn the_invite_carries_the_codec() {
        let (alice, bob) = ids();
        for id in CodecId::ALL {
            let (_, inv) = Caller::invite_codec(&alice, &PublicKey::from(&bob), 0, None, id).unwrap();
            assert_eq!(Incoming::parse(&bob, &inv).unwrap().codec(), id);
        }
    }

    #[test]
    fn an_invite_without_a_codec_is_g722() {
        let (alice, bob) = ids();
        let (_, inv) = Caller::invite(&alice, &PublicKey::from(&bob), 0).unwrap();
        assert_eq!(Incoming::parse(&bob, &inv).unwrap().codec(), CodecId::G722);
        // Telefone antigo: bytes finais zerados.
        let mut old = inv;
        old[TAIL_AT..].fill(0);
        assert_eq!(Incoming::parse(&bob, &old).unwrap().codec(), CodecId::G722);
    }

    #[test]
    fn a_tampered_codec_tag_falls_back_to_g722() {
        let (alice, bob) = ids();
        let (_, mut inv) = Caller::invite_codec(&alice, &PublicKey::from(&bob), 0, None, CodecId::Opus).unwrap();
        inv[TAIL_AT + 6] ^= 1;
        let incoming = Incoming::parse(&bob, &inv).unwrap();
        assert_eq!(incoming.codec(), CodecId::G722);
        assert!(incoming.accept(&bob).is_ok());
    }

    #[test]
    fn the_codec_stays_hidden_from_the_exit_relay() {
        let (alice, bob) = ids();
        // Sem a chave do convite, convite em Opus e em G.722 têm bytes finais igualmente aleatórios:
        // nenhum padrão fixo (aqui, só que não se repetem entre convites).
        let (_, a) = Caller::invite_codec(&alice, &PublicKey::from(&bob), 0, None, CodecId::Opus).unwrap();
        let (_, b) = Caller::invite_codec(&alice, &PublicKey::from(&bob), 0, None, CodecId::Opus).unwrap();
        assert_ne!(a[TAIL_AT..], b[TAIL_AT..]);
    }

    #[test]
    fn all_signals_have_the_packet_size() {
        let (a, b) = ids();
        let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let (acc, _) = Incoming::parse(&b, &inv).unwrap().accept(&b).unwrap();
        assert_eq!((inv.len(), acc.len()), (PACKET_LEN, PACKET_LEN));
    }

    #[test]
    fn reject_is_understood() {
        let (a, b) = ids();
        let (caller, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let rej = Incoming::parse(&b, &inv).unwrap().reject();
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
    fn other_recipients_and_tampered_invites_are_rejected() {
        let (a, b) = ids();
        let (_, inv) = Caller::invite(&a, &PublicKey::from(&b), 1000).unwrap();
        let other = StaticSecret::random_from_rng(OsRng);
        assert_eq!(Incoming::parse(&other, &inv).err(), Some(MediaError::Decrypt));
        let mut bad = inv;
        bad[40] ^= 1;
        assert!(Incoming::parse(&b, &bad).is_err());
        assert_eq!(Incoming::parse(&b, &inv[..100]).err(), Some(MediaError::Size));
        assert_eq!(Incoming::parse(&b, &[2u8; PACKET_LEN]).err(), Some(MediaError::Malformed));
    }

    #[test]
    fn an_invite_in_anothers_name_is_refused() {
        let (a, b) = ids();
        let x = StaticSecret::random_from_rng(OsRng);
        // `x` monta um convite com a identidade de `a` e a prova calculada com a própria chave.
        let eph = Ephemeral::generate();
        let dh = eph.diffie_hellman(&PublicKey::from(&b)).unwrap();
        let mut plain = [0u8; 40];
        plain[..32].copy_from_slice(PublicKey::from(&a).as_bytes());
        let own = x.diffie_hellman(&PublicKey::from(&b));
        plain[32..].copy_from_slice(&author_proof(own.as_bytes(), &eph.public(), PublicKey::from(&b).as_bytes()));
        let sealed = invite_key(&dh, &eph.public(), PublicKey::from(&b).as_bytes()).encrypt(&Nonce::default(), plain.as_slice()).unwrap();
        let mut f = [0u8; PACKET_LEN];
        f[0] = TAG_INVITE;
        f[1..33].copy_from_slice(&eph.public());
        f[33..HINT_AT].copy_from_slice(&sealed);
        OsRng.fill_bytes(&mut f[HINT_AT..]);
        assert_eq!(Incoming::parse(&b, &f).err(), Some(MediaError::BadKey));
        let (_, real) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        assert_eq!(Incoming::parse(&b, &real).unwrap().caller(), &PublicKey::from(&a));
    }

    #[test]
    fn caller_answering_with_another_static_key_gets_no_working_keys() {
        let (a, b) = ids();
        let other = StaticSecret::random_from_rng(OsRng);
        let (caller, inv) = Caller::invite(&a, &PublicKey::from(&b), 0).unwrap();
        let (acc, kb) = Incoming::parse(&b, &inv).unwrap().accept(&b).unwrap();
        // Quem tem o estado do chamador mas não a chave secreta `a` deriva chaves erradas.
        let Answer::Accepted(kx) = caller.answer(&other, &acc).unwrap() else { panic!() };
        let (mut s, mut o) = (Sealer::new(&kx), Opener::new(&kb));
        assert_eq!(o.open(&s.seal(b"x").unwrap()).err(), Some(MediaError::Decrypt));
    }
}
