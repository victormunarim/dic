//! Atualização de endereço: um contato avisa o novo relay dele. *Sans-IO*; cada
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
//! - O mesmo registro, sem o quadro em volta, é o que o ponto de encontro guarda na DHT
//!   ([`seal_record`], [`open_record`]); a chave de assinatura e o sal do item vêm de [`rendezvous`].
//! - A **apresentação** (tag 7) é o aviso de "adicionei você": quem adiciona um contato entrega o próprio
//!   cartão (nome e o relay dele) para o outro ter o contato na lista também. O
//!   quadro leva a chave pública de quem apresenta em claro (o destinatário ainda não o conhece, então não
//!   tem como tentar chave por chave), e o conteúdo vai cifrado com `DH(apresentador, destinatário)`: só
//!   quem tem a chave secreta da identidade que se diz autora consegue formar um quadro que abre. O
//!   destinatário confirma com o mesmo ack (tag 5) sobre o `seq` da apresentação ([`seal_intro`], [`open_intro`]).
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
pub const TAG_INTRO: u8 = 7;
/// Só separa o domínio da cifra do registro guardado na DHT (entra como dado associado, não vai a nenhum quadro).
const TAG_RECORD: u8 = 6;
/// Tamanho máximo do endereço (`host:porta`) no registro, o que cabe no quadro.
pub const MAX_ADDR: usize = 60;
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const PLAIN_LEN: usize = 1 + 8 + 1 + MAX_ADDR + 32 + 32;
/// Tamanho do valor de um registro guardado fora do circuito (DHT, âncora): `nonce ‖ AEAD`.
pub const RECORD_LEN: usize = NONCE_LEN + PLAIN_LEN + 16;
/// Parte fixa da apresentação: versão, `seq`, tipo, tamanhos, chaves do relay.
const INTRO_HEAD: usize = 1 + 8 + 1 + 1 + 1 + 32 + 32;
/// O quadro da apresentação cabe em [`PACKET_LEN`]: tag, autor, nonce, conteúdo, autenticação.
const INTRO_PLAIN_LEN: usize = PACKET_LEN - 1 - 32 - NONCE_LEN - 16;
/// Bytes para o endereço e o nome juntos na apresentação.
pub const INTRO_TEXT: usize = INTRO_PLAIN_LEN - INTRO_HEAD;
/// Maior nome que a apresentação leva (o nome perde para o endereço quando falta espaço).
pub const MAX_NAME: usize = 32;

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

/// `nonce ‖ AEAD(plain)`, com nonce aleatório.
fn seal_bytes(cipher: &ChaCha20Poly1305, tag: u8, plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: &[tag] }).expect("cifrar em memória não falha"));
    out
}

fn seal(cipher: &ChaCha20Poly1305, tag: u8, plain: &[u8]) -> Frame {
    seal_after(cipher, tag, &[], plain)
}

/// Quadro `tag ‖ head ‖ nonce ‖ AEAD(plain)`, completado com bytes aleatórios até [`PACKET_LEN`].
fn seal_after(cipher: &ChaCha20Poly1305, tag: u8, head: &[u8], plain: &[u8]) -> Frame {
    let sealed = seal_bytes(cipher, tag, plain);
    let mut f = [0u8; PACKET_LEN];
    f[0] = tag;
    f[1..1 + head.len()].copy_from_slice(head);
    let at = 1 + head.len();
    f[at..at + sealed.len()].copy_from_slice(&sealed);
    OsRng.fill_bytes(&mut f[at + sealed.len()..]);
    f
}

fn open(cipher: &ChaCha20Poly1305, tag: u8, frame: &[u8], plain_len: usize) -> Option<Vec<u8>> {
    open_after(cipher, tag, 0, frame, plain_len)
}

/// Abre o quadro `tag ‖ head (head_len bytes) ‖ nonce ‖ AEAD(plain)`.
fn open_after(cipher: &ChaCha20Poly1305, tag: u8, head_len: usize, frame: &[u8], plain_len: usize) -> Option<Vec<u8>> {
    let frame: &Frame = frame.try_into().ok()?;
    if frame[0] != tag {
        return None;
    }
    let at = 1 + head_len;
    let sealed = &frame[at + NONCE_LEN..at + NONCE_LEN + plain_len + 16];
    cipher.decrypt(Nonce::from_slice(&frame[at..at + NONCE_LEN]), Payload { msg: sealed, aad: &[tag] }).ok()
}

fn encode(record: &AddressRecord) -> Result<[u8; PLAIN_LEN], MediaError> {
    if record.addr.is_empty() || record.addr.len() > MAX_ADDR {
        return Err(MediaError::Malformed);
    }
    let mut plain = [0u8; PLAIN_LEN];
    plain[0] = VERSION;
    plain[1..9].copy_from_slice(&record.seq.to_be_bytes());
    plain[9] = record.addr.len() as u8;
    plain[10..10 + record.addr.len()].copy_from_slice(record.addr.as_bytes());
    plain[10 + MAX_ADDR..42 + MAX_ADDR].copy_from_slice(&record.onion);
    plain[42 + MAX_ADDR..].copy_from_slice(&record.link);
    Ok(plain)
}

/// Quadro com o registro de `my_static` para `peer`.
pub fn seal_update(my_static: &StaticSecret, peer: &PublicKey, record: &AddressRecord) -> Result<Frame, MediaError> {
    let plain = encode(record)?;
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal(&cipher, TAG_UPDATE, &plain))
}

/// O registro de `my_static` para `peer` como valor de um item da DHT (`nonce ‖ AEAD`, 162 B).
pub fn seal_record(my_static: &StaticSecret, peer: &PublicKey, record: &AddressRecord) -> Result<Vec<u8>, MediaError> {
    let plain = encode(record)?;
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal_bytes(&cipher, TAG_RECORD, &plain))
}

/// Abre o valor de um item da DHT que `peer` guardou para `my_static`. `None`: não abre com a chave do par.
pub fn open_record(my_static: &StaticSecret, peer: &PublicKey, value: &[u8]) -> Option<AddressRecord> {
    if value.len() != NONCE_LEN + PLAIN_LEN + 16 {
        return None;
    }
    let cipher = cipher(my_static, peer, peer)?;
    let plain = cipher.decrypt(Nonce::from_slice(&value[..NONCE_LEN]), Payload { msg: &value[NONCE_LEN..], aad: &[TAG_RECORD] }).ok()?;
    parse(&plain)
}

/// Onde `owner` guarda o endereço dele para o par `(my_static, peer)` no dia `period` (`unix / 86400`): a semente da
/// chave de assinatura ed25519 e o sal do item. Os dois lados do par derivam o mesmo par de valores; o dia
/// entra nas duas derivações, então o alvo na DHT muda sozinho a cada dia e um dia não se liga ao outro.
pub fn rendezvous(my_static: &StaticSecret, peer: &PublicKey, owner: &PublicKey, period: u64) -> Option<([u8; 32], [u8; 32])> {
    let dh = my_static.diffie_hellman(peer);
    if !dh.was_contributory() {
        return None;
    }
    let derive = |label: &[u8]| {
        let mut info = label.to_vec();
        info.extend_from_slice(owner.as_bytes());
        info.extend_from_slice(&period.to_be_bytes());
        let mut out = [0u8; 32];
        Hkdf::<Sha256>::new(None, dh.as_bytes()).expand(&info, &mut out).expect("32 bytes é válido");
        out
    };
    Some((derive(b"clam/encontro/assina/v1"), derive(b"clam/encontro/sal/v1")))
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

/// Como o apresentador pode ser alcançado por quem o adiciona: o relay dele (o código dele é `chave#addr#onion#link`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach {
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

/// O cartão que quem adiciona um contato entrega a ele.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intro {
    /// Distingue uma apresentação da outra; o ack repete este número.
    pub seq: u64,
    /// Como o apresentador quer ser chamado (até [`MAX_NAME`] bytes; corta no fim se faltar espaço).
    pub name: String,
    pub reach: Reach,
}

/// Quadro de apresentação de `my_static` para `peer`. Falha se o endereço for vazio ou passar de [`MAX_ADDR`].
pub fn seal_intro(my_static: &StaticSecret, peer: &PublicKey, intro: &Intro) -> Result<Frame, MediaError> {
    let Reach { addr, onion, link } = &intro.reach;
    if addr.is_empty() || addr.len() > MAX_ADDR {
        return Err(MediaError::Malformed);
    }
    let name = clip(intro.name.trim(), MAX_NAME.min(INTRO_TEXT - addr.len()));
    let mut plain = [0u8; INTRO_PLAIN_LEN];
    plain[0] = VERSION;
    plain[1..9].copy_from_slice(&intro.seq.to_be_bytes());
    plain[10] = addr.len() as u8;
    plain[11] = name.len() as u8;
    plain[12..44].copy_from_slice(onion);
    plain[44..76].copy_from_slice(link);
    plain[INTRO_HEAD..INTRO_HEAD + addr.len()].copy_from_slice(addr.as_bytes());
    plain[INTRO_HEAD + addr.len()..INTRO_HEAD + addr.len() + name.len()].copy_from_slice(name.as_bytes());
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal_after(&cipher, TAG_INTRO, me.as_bytes(), &plain))
}

/// O começo de `s` com até `max` bytes, sem cortar um caractere ao meio.
fn clip(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Abre `frame` como apresentação para `my_static`. Devolve a chave de quem apresentou (provada: só quem tem a
/// chave secreta dela monta um quadro que abre) e o cartão. `None`: não é uma apresentação para esta chave.
pub fn open_intro(my_static: &StaticSecret, frame: &[u8]) -> Option<([u8; 32], Intro)> {
    if frame.len() != PACKET_LEN || frame[0] != TAG_INTRO {
        return None;
    }
    let author: [u8; 32] = frame[1..33].try_into().ok()?;
    let owner = PublicKey::from(author);
    let cipher = cipher(my_static, &owner, &owner)?;
    let plain = open_after(&cipher, TAG_INTRO, 32, frame, INTRO_PLAIN_LEN)?;
    if plain[0] != VERSION {
        return None;
    }
    let (addr_len, name_len) = (plain[10] as usize, plain[11] as usize);
    if addr_len == 0 || addr_len > MAX_ADDR || addr_len + name_len > INTRO_TEXT {
        return None;
    }
    let text = &plain[INTRO_HEAD..INTRO_HEAD + addr_len + name_len];
    let addr = String::from_utf8(text[..addr_len].to_vec()).ok()?;
    let name = String::from_utf8(text[addr_len..].to_vec()).ok()?;
    let reach = Reach { addr, onion: plain[12..44].try_into().ok()?, link: plain[44..76].try_into().ok()? };
    Some((author, Intro { seq: u64::from_be_bytes(plain[1..9].try_into().ok()?), name, reach }))
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
    fn the_stored_record_opens_only_with_the_pair_key() {
        let (a, b) = ids();
        let c = StaticSecret::random_from_rng(OsRng);
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let value = seal_record(&a, &pb, &record(3)).unwrap();
        assert_eq!(value.len(), 162);
        assert_eq!(open_record(&b, &pa, &value), Some(record(3)));
        assert!(open_record(&c, &pa, &value).is_none());
        assert!(open_record(&a, &pb, &value).is_none(), "o dono não é o par");
        // Quadro de atualização e item guardado não se confundem.
        let frame = seal_update(&a, &pb, &record(3)).unwrap();
        assert!(open_record(&b, &pa, &frame[1..163]).is_none());
    }

    #[test]
    fn both_sides_derive_the_same_place_and_it_moves_every_day() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let by_owner = rendezvous(&a, &pb, &pa, 100).unwrap();
        assert_eq!(rendezvous(&b, &pa, &pa, 100).unwrap(), by_owner);
        assert_ne!(rendezvous(&a, &pb, &pa, 101).unwrap(), by_owner);
        // Um sentido por item: o endereço de B para A fica em outro lugar.
        assert_ne!(rendezvous(&a, &pb, &pb, 100).unwrap(), by_owner);
        let stranger = StaticSecret::random_from_rng(OsRng);
        assert_ne!(rendezvous(&stranger, &pb, &pa, 100).unwrap(), by_owner);
    }

    fn intro(seq: u64) -> Intro {
        Intro { seq, name: "Ana".into(), reach: Reach { addr: "203.0.113.7:4001".into(), onion: [3; 32], link: [4; 32] } }
    }

    #[test]
    fn the_introduction_proves_who_sent_it_and_reads_only_for_the_recipient() {
        let (a, b) = ids();
        let c = StaticSecret::random_from_rng(OsRng);
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let frame = seal_intro(&a, &pb, &intro(9)).unwrap();
        assert_eq!(frame.len(), PACKET_LEN);
        assert_eq!(open_intro(&b, &frame), Some((*pa.as_bytes(), intro(9))));
        // Outro destinatário não abre.
        assert!(open_intro(&c, &frame).is_none());
        // C não passa por A: troca a chave de autor no quadro e a cifra deixa de abrir.
        let mut forged = seal_intro(&c, &pb, &intro(9)).unwrap();
        forged[1..33].copy_from_slice(pa.as_bytes());
        assert!(open_intro(&b, &forged).is_none());
        // Um byte mudado quebra a autenticação.
        let mut flipped = frame;
        flipped[100] ^= 1;
        assert!(open_intro(&b, &flipped).is_none());
        // Atualização de endereço e apresentação não se confundem.
        let update = seal_update(&a, &pb, &record(1)).unwrap();
        assert!(open_intro(&b, &update).is_none());
        assert!(open_update(&b, [*pa.as_bytes()], &frame).is_none());
    }

    #[test]
    fn the_introduction_does_not_show_the_address_or_the_name() {
        let (a, b) = ids();
        let frame = seal_intro(&a, &PublicKey::from(&b), &intro(1)).unwrap();
        assert!(!frame.windows(3).any(|w| w == b"Ana") && !frame.windows(8).any(|w| w == b"203.0.113"));
    }

    #[test]
    fn the_introduction_trims_the_name_to_fit() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        let long = Intro { seq: 2, name: "é".repeat(40), reach: Reach { addr: "casa.exemplo.com:4001".into(), onion: [5; 32], link: [6; 32] } };
        let (_, got) = open_intro(&b, &seal_intro(&a, &pb, &long).unwrap()).unwrap();
        assert_eq!(got.reach, long.reach);
        assert!(got.name.len() <= MAX_NAME && got.name.chars().all(|c| c == 'é') && !got.name.is_empty());
        // Endereço no limite: o nome encolhe para caber, o endereço não perde nada.
        let tight = Intro { seq: 3, name: "x".repeat(MAX_NAME), reach: Reach { addr: "h".repeat(MAX_ADDR), onion: [1; 32], link: [2; 32] } };
        let (_, got) = open_intro(&b, &seal_intro(&a, &pb, &tight).unwrap()).unwrap();
        assert_eq!(got.reach, tight.reach);
        assert_eq!(got.name.len(), INTRO_TEXT - MAX_ADDR);
        for addr in ["", &"x".repeat(MAX_ADDR + 1)] {
            let bad = Intro { reach: Reach { addr: addr.to_string(), onion: [3; 32], link: [4; 32] }, ..intro(1) };
            assert_eq!(seal_intro(&a, &pb, &bad), Err(MediaError::Malformed));
        }
    }

    #[test]
    fn the_ack_of_an_introduction_confirms_its_seq() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let ack = seal_ack(&b, &pa, 9).unwrap();
        assert!(open_ack(&a, &pb, 9, &ack));
        assert!(!open_ack(&a, &pb, 10, &ack));
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
