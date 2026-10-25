//! Atualização de endereço: um contato avisa o novo relay dele, sem diretório. *Sans-IO*; cada
//! mensagem é um quadro de **200 bytes**, como a sinalização da chamada.
//!
//! ```text
//! dono do endereço                                 contato
//!   Update: tag 4 ‖ nonce ‖ AEAD(registro de endereço)  ─>
//!                                       <─  Ack: tag 5 ‖ nonce ‖ AEAD(seq)
//! ```
//!
//! - O registro leva versão, `seq` (só cresce), o endereço do relay e as chaves dele (`onion`, `link`).
//! - A chave da cifra sai de `HKDF(DH(minha secreta, pública do outro), "clam/encontro/cifra/v1" ‖ pública do
//!   dono do conteúdo)`. Inclui o dono, então o registro de A para B e o de B para A usam chaves
//!   diferentes. Só os dois do par a calculam; um terceiro não lê nem forja o registro.
//! - O quadro não diz de quem é: quem recebe tenta a chave de cada contato ([`open_update`]). Quem não é
//!   contato não consegue abrir, e o telefone descarta o quadro em silêncio, como faz com um convite alheio.
//! - O ack prova que o contato abriu o registro (usa a chave do sentido contrário), e é o que tira o
//!   registro da fila de saída.
//! - Repetir um registro gravado só devolve ao contato um endereço que ele já teve; quem recebe descarta
//!   registro com `seq` menor ou igual ao último que viu.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use super::{MediaError, packet::PACKET_LEN, signal::Frame};

pub const TAG_UPDATE: u8 = 4;
pub const TAG_ACK: u8 = 5;
/// Tamanho máximo do endereço (`host:porta`) no registro, o mesmo que o diretório aceita.
pub const MAX_ADDR: usize = 60;
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const PLAIN_LEN: usize = 1 + 8 + 1 + MAX_ADDR + 32 + 32;

/// O que o dono do endereço conta ao contato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressRecord {
    /// Cresce a cada registro novo do mesmo dono.
    pub seq: u64,
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

fn cipher(my: &StaticSecret, peer: &PublicKey, owner: &PublicKey) -> Option<ChaCha20Poly1305> {
    let dh = my.diffie_hellman(peer);
    if !dh.was_contributory() {
        return None;
    }
    let mut info = b"clam/encontro/cifra/v1".to_vec();
    info.extend_from_slice(owner.as_bytes());
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(None, dh.as_bytes()).expand(&info, &mut k).expect("32 bytes é válido");
    Some(ChaCha20Poly1305::new(Key::from_slice(&k)))
}

fn seal(cipher: &ChaCha20Poly1305, tag: u8, plain: &[u8]) -> Frame {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let sealed = cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: &[tag] }).expect("cifrar em memória não falha");
    let mut f = [0u8; PACKET_LEN];
    f[0] = tag;
    f[1..1 + NONCE_LEN].copy_from_slice(&nonce);
    f[1 + NONCE_LEN..1 + NONCE_LEN + sealed.len()].copy_from_slice(&sealed);
    OsRng.fill_bytes(&mut f[1 + NONCE_LEN + sealed.len()..]);
    f
}

fn open(cipher: &ChaCha20Poly1305, tag: u8, frame: &[u8], plain_len: usize) -> Option<Vec<u8>> {
    let frame: &Frame = frame.try_into().ok()?;
    if frame[0] != tag {
        return None;
    }
    let sealed = &frame[1 + NONCE_LEN..1 + NONCE_LEN + plain_len + 16];
    cipher.decrypt(Nonce::from_slice(&frame[1..1 + NONCE_LEN]), Payload { msg: sealed, aad: &[tag] }).ok()
}

/// Quadro com o registro de `my_static` para `peer`.
pub fn seal_update(my_static: &StaticSecret, peer: &PublicKey, record: &AddressRecord) -> Result<Frame, MediaError> {
    if record.addr.is_empty() || record.addr.len() > MAX_ADDR {
        return Err(MediaError::Malformed);
    }
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    let mut plain = [0u8; PLAIN_LEN];
    plain[0] = VERSION;
    plain[1..9].copy_from_slice(&record.seq.to_be_bytes());
    plain[9] = record.addr.len() as u8;
    plain[10..10 + record.addr.len()].copy_from_slice(record.addr.as_bytes());
    plain[10 + MAX_ADDR..42 + MAX_ADDR].copy_from_slice(&record.onion);
    plain[42 + MAX_ADDR..].copy_from_slice(&record.link);
    Ok(seal(&cipher, TAG_UPDATE, &plain))
}

/// Abre `frame` como atualização de algum de `contacts`. `None`: não é uma atualização para esta chave, ou
/// de nenhum contato; ignore o quadro.
pub fn open_update(my_static: &StaticSecret, contacts: impl IntoIterator<Item = [u8; 32]>, frame: &[u8]) -> Option<([u8; 32], AddressRecord)> {
    if frame.first() != Some(&TAG_UPDATE) {
        return None;
    }
    for contact in contacts {
        let owner = PublicKey::from(contact);
        let Some(cipher) = cipher(my_static, &owner, &owner) else { continue };
        let Some(plain) = open(&cipher, TAG_UPDATE, frame, PLAIN_LEN) else { continue };
        let record = parse(&plain)?;
        return Some((contact, record));
    }
    None
}

fn parse(plain: &[u8]) -> Option<AddressRecord> {
    if plain.len() != PLAIN_LEN || plain[0] != VERSION {
        return None;
    }
    let n = plain[9] as usize;
    if n == 0 || n > MAX_ADDR {
        return None;
    }
    Some(AddressRecord {
        seq: u64::from_be_bytes(plain[1..9].try_into().ok()?),
        addr: String::from_utf8(plain[10..10 + n].to_vec()).ok()?,
        onion: plain[10 + MAX_ADDR..42 + MAX_ADDR].try_into().ok()?,
        link: plain[42 + MAX_ADDR..].try_into().ok()?,
    })
}

/// Confirmação de `seq` de `my_static` para `peer`, a quem mandou a atualização.
pub fn seal_ack(my_static: &StaticSecret, peer: &PublicKey, seq: u64) -> Result<Frame, MediaError> {
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal(&cipher, TAG_ACK, &seq.to_be_bytes()))
}

/// `true` se `frame` é a confirmação de `peer` para o registro `seq`.
pub fn open_ack(my_static: &StaticSecret, peer: &PublicKey, seq: u64, frame: &[u8]) -> bool {
    cipher(my_static, peer, peer)
        .and_then(|c| open(&c, TAG_ACK, frame, 8))
        .is_some_and(|p| p == seq.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (StaticSecret, StaticSecret) {
        (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng))
    }

    fn record(seq: u64) -> AddressRecord {
        AddressRecord { seq, addr: "casa.exemplo.com:4001".into(), onion: [7; 32], link: [9; 32] }
    }

    #[test]
    fn the_contact_opens_what_the_owner_sealed() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let frame = seal_update(&a, &pb, &record(5)).unwrap();
        let (from, rec) = open_update(&b, [[1; 32], *pa.as_bytes()], &frame).unwrap();
        assert_eq!(from, *pa.as_bytes());
        assert_eq!(rec, record(5));
    }

    #[test]
    fn the_frame_is_always_200_bytes_and_does_not_show_the_address() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        let mut short = record(1);
        short.addr = "1.2.3.4:5".into();
        let f1 = seal_update(&a, &pb, &short).unwrap();
        let f2 = seal_update(&a, &pb, &record(1)).unwrap();
        assert_eq!((f1.len(), f2.len()), (PACKET_LEN, PACKET_LEN));
        assert!(!f2.windows(5).any(|w| w == b"casa."));
    }

    #[test]
    fn a_stranger_cannot_open_or_forge_it() {
        let (a, b) = ids();
        let c = StaticSecret::random_from_rng(OsRng);
        let (pa, pb, pc) = (PublicKey::from(&a), PublicKey::from(&b), PublicKey::from(&c));
        let frame = seal_update(&a, &pb, &record(1)).unwrap();
        // Quem não é contato de A, ou não tem a chave de B, não abre.
        assert!(open_update(&b, [*pc.as_bytes()], &frame).is_none());
        assert!(open_update(&c, [*pa.as_bytes()], &frame).is_none());
        // C diz ser A: não deriva a chave do par A-B.
        let forged = seal_update(&c, &pb, &record(2)).unwrap();
        assert!(open_update(&b, [*pa.as_bytes()], &forged).is_none());
    }

    #[test]
    fn a_flipped_byte_breaks_authentication() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let mut frame = seal_update(&a, &pb, &record(1)).unwrap();
        frame[20] ^= 1;
        assert!(open_update(&b, [*pa.as_bytes()], &frame).is_none());
    }

    #[test]
    fn the_key_depends_on_who_owns_the_address() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        // B não consegue passar por A: o registro "de A" cifrado por B usa a chave do dono B.
        let by_b = seal_update(&b, &pa, &record(1)).unwrap();
        assert!(open_update(&b, [*pa.as_bytes()], &by_b).is_none());
        assert!(open_update(&a, [*pb.as_bytes()], &by_b).is_some());
    }

    #[test]
    fn the_ack_confirms_only_its_own_seq() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let ack = seal_ack(&b, &pa, 7).unwrap();
        assert!(open_ack(&a, &pb, 7, &ack));
        assert!(!open_ack(&a, &pb, 8, &ack));
        assert!(!open_ack(&b, &pa, 7, &ack));
    }

    #[test]
    fn address_limits_are_enforced() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        for addr in ["", &"x".repeat(MAX_ADDR + 1)] {
            let r = AddressRecord { addr: addr.to_string(), ..record(1) };
            assert_eq!(seal_update(&a, &pb, &r), Err(MediaError::Malformed));
        }
        let r = AddressRecord { addr: "x".repeat(MAX_ADDR), ..record(1) };
        assert!(seal_update(&a, &pb, &r).is_ok());
    }
}
