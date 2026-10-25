//! Chaves da chamada: handshake autenticado de 3 DH, no estilo Noise `KK`.
//!
//! Os dois já se conhecem (pareamento) e trocam só uma chave efêmera cada, dentro do convite
//! e da resposta da chamada. Misturam-se:
//!
//! - `DH(e_i, e_r)`: sigilo adiante (nada do que vem depois decifra uma chamada passada);
//! - `DH(s_i, e_r)`: só o dono de `s_i` chega a ele (autentica o chamador);
//! - `DH(e_i, s_r)`: só o dono de `s_r` chega a ele (autentica o chamado).
//!
//! Quem não possui a chave estática esperada deriva chaves diferentes, e o primeiro pacote
//! falha na autenticação. Não há confirmação explícita: o primeiro pacote válido é a
//! confirmação. A transcrição (as quatro chaves públicas) entra no HKDF.

use hkdf::Hkdf;
use rand_core::OsRng;
use sha2::Sha256;
use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::MediaError;

const SALT: &[u8] = b"clam/v1/call";

/// Par efêmero de uma chamada. Descarte-o assim que as chaves saírem.
pub struct Ephemeral {
    secret: StaticSecret,
    public: PublicKey,
}

impl Ephemeral {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub(crate) fn diffie_hellman(&self, peer: &PublicKey) -> Result<[u8; 32], MediaError> {
        checked(&self.secret, peer).map(|d| *d.as_bytes())
    }

    /// O que vai para o outro lado.
    pub fn public(&self) -> [u8; 32] {
        self.public.to_bytes()
    }
}

/// Chaves AEAD das duas direções, já do ponto de vista de quem as derivou.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct CallKeys {
    pub(crate) send: [u8; 32],
    pub(crate) recv: [u8; 32],
}

fn checked(secret: &StaticSecret, public: &PublicKey) -> Result<SharedSecret, MediaError> {
    let dh = secret.diffie_hellman(public);
    // Pontos de ordem baixa dão um segredo previsível (tudo zero).
    dh.was_contributory().then_some(dh).ok_or(MediaError::BadKey)
}

fn derive(dh: [SharedSecret; 3], s_i: &[u8; 32], e_i: &[u8; 32], s_r: &[u8; 32], e_r: &[u8; 32], initiator: bool) -> CallKeys {
    let mut ikm = Vec::with_capacity(96);
    for d in &dh {
        ikm.extend_from_slice(d.as_bytes());
    }
    let mut info = Vec::with_capacity(128);
    for part in [s_i, e_i, s_r, e_r] {
        info.extend_from_slice(part);
    }
    let hk = Hkdf::<Sha256>::new(Some(SALT), &ikm);
    ikm.zeroize();
    let (mut i2r, mut r2i) = ([0u8; 32], [0u8; 32]);
    let mut label = b"i2r".to_vec();
    label.extend_from_slice(&info);
    hk.expand(&label, &mut i2r).expect("32 bytes é válido");
    label[..3].copy_from_slice(b"r2i");
    hk.expand(&label, &mut r2i).expect("32 bytes é válido");
    if initiator { CallKeys { send: i2r, recv: r2i } } else { CallKeys { send: r2i, recv: i2r } }
}

impl CallKeys {
    /// Chaves independentes para outro fluxo da mesma chamada (ex.: a tela, que anda noutro
    /// circuito). Os dois lados passam o mesmo `label` e obtêm chaves espelhadas; o `seq` de cada
    /// fluxo recomeça sem risco de reusar nonce.
    pub fn sub(&self, label: &[u8]) -> CallKeys {
        let expand = |key: &[u8; 32]| {
            let mut out = [0u8; 32];
            Hkdf::<Sha256>::new(Some(b"clam/v1/sub"), key).expand(label, &mut out).expect("32 bytes é válido");
            out
        };
        CallKeys { send: expand(&self.send), recv: expand(&self.recv) }
    }

    /// Chamador: `my_static` é a chave do dispositivo; `peer_static`, a do contato (pareamento);
    /// `my_eph` é a efêmera enviada no convite; `peer_eph`, a que veio na resposta.
    pub fn initiator(my_static: &StaticSecret, my_eph: &Ephemeral, peer_static: &PublicKey, peer_eph: &[u8; 32]) -> Result<Self, MediaError> {
        let peer_eph = PublicKey::from(*peer_eph);
        let dh = [
            checked(&my_eph.secret, &peer_eph)?,
            checked(my_static, &peer_eph)?,
            checked(&my_eph.secret, peer_static)?,
        ];
        let me = PublicKey::from(my_static);
        Ok(derive(dh, me.as_bytes(), my_eph.public.as_bytes(), peer_static.as_bytes(), peer_eph.as_bytes(), true))
    }

    /// Chamado: o espelho do chamador. `peer_eph` é a efêmera do convite.
    pub fn responder(my_static: &StaticSecret, my_eph: &Ephemeral, peer_static: &PublicKey, peer_eph: &[u8; 32]) -> Result<Self, MediaError> {
        let peer_eph = PublicKey::from(*peer_eph);
        let dh = [
            checked(&my_eph.secret, &peer_eph)?,
            checked(&my_eph.secret, peer_static)?,
            checked(my_static, &peer_eph)?,
        ];
        let me = PublicKey::from(my_static);
        Ok(derive(dh, peer_static.as_bytes(), peer_eph.as_bytes(), me.as_bytes(), my_eph.public.as_bytes(), false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (StaticSecret, StaticSecret) {
        (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng))
    }

    #[test]
    fn both_sides_derive_mirrored_keys() {
        let (a, b) = pair();
        let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
        let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap();
        let kb = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).unwrap();
        assert_eq!(ka.send, kb.recv);
        assert_eq!(ka.recv, kb.send);
        assert_ne!(ka.send, ka.recv);
    }

    #[test]
    fn sub_keys_mirror_and_differ_per_label() {
        let (a, b) = pair();
        let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
        let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap();
        let kb = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).unwrap();
        let (sa, sb) = (ka.sub(b"screen"), kb.sub(b"screen"));
        assert_eq!(sa.send, sb.recv);
        assert_eq!(sa.recv, sb.send);
        assert_ne!(sa.send, ka.send);
        assert_ne!(sa.send, ka.sub(b"other").send);
    }

    #[test]
    fn each_call_gets_fresh_keys() {
        let (a, b) = pair();
        let run = || {
            let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
            CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap().send
        };
        assert_ne!(run(), run());
    }

    #[test]
    fn impostor_without_the_static_key_derives_other_keys() {
        let (a, b) = pair();
        let impostor = StaticSecret::random_from_rng(OsRng);
        let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
        let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap();
        // Alguém que fala como B mas tem outra chave estática.
        let kx = CallKeys::responder(&impostor, &eb, &PublicKey::from(&a), &ea.public()).unwrap();
        assert_ne!(ka.send, kx.recv);
        assert_ne!(ka.recv, kx.send);
    }

    #[test]
    fn low_order_points_are_rejected() {
        let (a, b) = pair();
        let ea = Ephemeral::generate();
        assert_eq!(CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &[0u8; 32]).err(), Some(MediaError::BadKey));
        let mut one = [0u8; 32];
        one[0] = 1;
        assert_eq!(CallKeys::responder(&b, &ea, &PublicKey::from(&a), &one).err(), Some(MediaError::BadKey));
    }
}
