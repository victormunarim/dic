//! Registro de presença: cifrado, de tamanho fixo e assinado.
//! Contém só pontos de introdução (endereços de relays), nunca o IP do dono.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use thiserror::Error;

use crate::pairing::PresenceKeys;

pub const MAX_INTRO_POINTS: usize = 8;
pub const MAX_INTRO_LEN: usize = 64;
/// Tamanho fixo do texto claro (com padding): 1 + 8 * (1 + 64) = 521 → 576.
const PLAINTEXT_LEN: usize = 576;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const SIG_LEN: usize = 64;
/// Tamanho fixo de todo registro publicado, qualquer que seja o conteúdo.
pub const RECORD_LEN: usize = NONCE_LEN + PLAINTEXT_LEN + TAG_LEN + SIG_LEN;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PresenceError {
    #[error("pontos de introdução demais (máx. {MAX_INTRO_POINTS})")]
    TooManyIntroPoints,
    #[error("ponto de introdução vazio ou maior que {MAX_INTRO_LEN} bytes")]
    BadIntroPoint,
    #[error("registro com tamanho inválido")]
    BadLength,
    #[error("assinatura inválida")]
    BadSignature,
    #[error("falha ao decifrar (chave, endereço ou época errados)")]
    Decrypt,
    #[error("conteúdo malformado")]
    Malformed,
}

fn encode(intro_points: &[Vec<u8>]) -> Result<[u8; PLAINTEXT_LEN], PresenceError> {
    if intro_points.len() > MAX_INTRO_POINTS {
        return Err(PresenceError::TooManyIntroPoints);
    }
    let mut buf = [0u8; PLAINTEXT_LEN];
    buf[0] = intro_points.len() as u8;
    let mut at = 1;
    for p in intro_points {
        if p.is_empty() || p.len() > MAX_INTRO_LEN {
            return Err(PresenceError::BadIntroPoint);
        }
        buf[at] = p.len() as u8;
        buf[at + 1..at + 1 + p.len()].copy_from_slice(p);
        at += 1 + p.len();
    }
    Ok(buf)
}

fn decode(buf: &[u8]) -> Result<Vec<Vec<u8>>, PresenceError> {
    let count = *buf.first().ok_or(PresenceError::Malformed)? as usize;
    if count > MAX_INTRO_POINTS {
        return Err(PresenceError::Malformed);
    }
    let mut at = 1;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let len = *buf.get(at).ok_or(PresenceError::Malformed)? as usize;
        if len == 0 || len > MAX_INTRO_LEN {
            return Err(PresenceError::Malformed);
        }
        let end = at + 1 + len;
        out.push(buf.get(at + 1..end).ok_or(PresenceError::Malformed)?.to_vec());
        at = end;
    }
    // O restante deve ser padding zerado.
    if buf[at..].iter().any(|&b| b != 0) {
        return Err(PresenceError::Malformed);
    }
    Ok(out)
}

/// Valida os pontos de introdução sem cifrar nada (mesmas regras de [`seal`]).
pub fn validate_intro_points(intro_points: &[Vec<u8>]) -> Result<(), PresenceError> {
    encode(intro_points).map(|_| ())
}

/// Cria o registro publicado sob `lookup_id`. O endereço entra como AAD, então
/// o registro não pode ser movido para outro endereço/época.
pub fn seal(
    keys: &PresenceKeys,
    signer: &SigningKey,
    lookup_id: &[u8; 32],
    intro_points: &[Vec<u8>],
) -> Result<[u8; RECORD_LEN], PresenceError> {
    seal_plain(keys, signer, lookup_id, &encode(intro_points)?)
}

/// Cifra e assina um texto claro já montado (v1 ou v2): o envelope é o mesmo, então o tamanho é o mesmo.
fn seal_plain(
    keys: &PresenceKeys,
    signer: &SigningKey,
    lookup_id: &[u8; 32],
    plain: &[u8; PLAINTEXT_LEN],
) -> Result<[u8; RECORD_LEN], PresenceError> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.enc_key));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: lookup_id })
        .map_err(|_| PresenceError::Decrypt)?;

    let mut rec = [0u8; RECORD_LEN];
    rec[..NONCE_LEN].copy_from_slice(&nonce);
    rec[NONCE_LEN..NONCE_LEN + ct.len()].copy_from_slice(&ct);
    let body_end = NONCE_LEN + ct.len();
    let sig = signer.sign(&signed_message(lookup_id, &rec[..body_end]));
    rec[body_end..].copy_from_slice(&sig.to_bytes());
    Ok(rec)
}

/// Verifica a assinatura do dono e decifra os pontos de introdução.
pub fn open(
    keys: &PresenceKeys,
    owner: &VerifyingKey,
    lookup_id: &[u8; 32],
    record: &[u8],
) -> Result<Vec<Vec<u8>>, PresenceError> {
    decode(&open_plain(keys, owner, lookup_id, record)?)
}

/// Confere o tamanho e a assinatura e decifra o texto claro (v1 ou v2).
fn open_plain(
    keys: &PresenceKeys,
    owner: &VerifyingKey,
    lookup_id: &[u8; 32],
    record: &[u8],
) -> Result<Vec<u8>, PresenceError> {
    if record.len() != RECORD_LEN {
        return Err(PresenceError::BadLength);
    }
    let body_end = RECORD_LEN - SIG_LEN;
    let sig = Signature::from_slice(&record[body_end..]).map_err(|_| PresenceError::BadSignature)?;
    owner
        .verify(&signed_message(lookup_id, &record[..body_end]), &sig)
        .map_err(|_| PresenceError::BadSignature)?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.enc_key));
    cipher
        .decrypt(
            Nonce::from_slice(&record[..NONCE_LEN]),
            Payload { msg: &record[NONCE_LEN..body_end], aad: lookup_id },
        )
        .map_err(|_| PresenceError::Decrypt)
}

fn signed_message(lookup_id: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(32 + body.len());
    m.extend_from_slice(lookup_id);
    m.extend_from_slice(body);
    m
}

/// Consulta falsa: identificador aleatório indistinguível de um real, para
/// esconder quem o usuário procura.
pub fn decoy_lookup_id() -> [u8; 32] {
    let mut id = [0u8; 32];
    OsRng.fill_bytes(&mut id);
    id
}

/// Registro falso de tamanho fixo, para publicações/respostas de cobertura.
pub fn decoy_record() -> [u8; RECORD_LEN] {
    let mut r = [0u8; RECORD_LEN];
    OsRng.fill_bytes(&mut r);
    r
}

pub use v2::{IntroPointV2, MAX_ADDR_V2, MAX_POINTS_V2, decode_points_v2, encode_points_v2, open_v2, seal_v2};

/// Registro de presença versão 2 (etapa 4 de `docs/DOIS-RELAYS.md`): os pontos de introdução são **tipados** (relay
/// com as duas chaves e a `auth_key` que o dono registrou lá), não bytes opacos, e o registro continua com o mesmo
/// tamanho e o mesmo envelope de [`seal`]. O primeiro byte do texto claro é a marca [`V2_MARK`], maior que o
/// máximo de pontos da v1: um leitor v1 recusa o registro como malformado (contato antigo não o abre), e a v2 recusa
/// o texto da v1.
mod v2 {
    #![deny(clippy::indexing_slicing, clippy::unwrap_used)]

    use ed25519_dalek::{SigningKey, VerifyingKey};

    use super::{PLAINTEXT_LEN, PresenceError, RECORD_LEN, open_plain, seal_plain};
    use crate::pairing::PresenceKeys;

    /// Marca da versão 2 no primeiro byte do texto claro.
    const V2_MARK: u8 = 0xA2;
    /// Pontos de introdução por registro (o desenho pede 3, em grupos diferentes).
    pub const MAX_POINTS_V2: usize = 3;
    /// Maior endereço de relay (o mesmo limite do blob de introdução).
    pub const MAX_ADDR_V2: usize = 60;
    /// `len` do endereço, endereço preenchido, chave do circuito, chave do link e `auth_key`.
    const POINT_LEN: usize = 1 + MAX_ADDR_V2 + 32 + 32 + 32;
    const _: () = assert!(2 + MAX_POINTS_V2 * POINT_LEN <= PLAINTEXT_LEN);

    /// Onde o dono espera: um relay de introdução e a chave de serviço que ele provou ter lá.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct IntroPointV2 {
        pub addr: String,
        pub public: [u8; 32],
        pub link_public: [u8; 32],
        pub auth_key: [u8; 32],
    }

    /// Monta o texto claro da v2, com preenchimento zerado até o tamanho fixo.
    ///
    /// # Errors
    ///
    /// [`PresenceError::TooManyIntroPoints`] acima de [`MAX_POINTS_V2`]; [`PresenceError::BadIntroPoint`] para endereço
    /// vazio ou maior que [`MAX_ADDR_V2`].
    pub fn encode_points_v2(points: &[IntroPointV2]) -> Result<[u8; PLAINTEXT_LEN], PresenceError> {
        if points.len() > MAX_POINTS_V2 {
            return Err(PresenceError::TooManyIntroPoints);
        }
        let mut plain = Vec::with_capacity(PLAINTEXT_LEN);
        plain.push(V2_MARK);
        plain.push(u8::try_from(points.len()).map_err(|_| PresenceError::TooManyIntroPoints)?);
        for p in points {
            let addr = p.addr.as_bytes();
            if addr.is_empty() || addr.len() > MAX_ADDR_V2 {
                return Err(PresenceError::BadIntroPoint);
            }
            plain.push(u8::try_from(addr.len()).map_err(|_| PresenceError::BadIntroPoint)?);
            plain.extend_from_slice(addr);
            plain.resize(plain.len() + (MAX_ADDR_V2 - addr.len()), 0);
            for key in [&p.public, &p.link_public, &p.auth_key] {
                plain.extend_from_slice(key);
            }
        }
        plain.resize(PLAINTEXT_LEN, 0);
        plain.try_into().map_err(|_| PresenceError::Malformed)
    }

    /// Lê o texto claro da v2. Estrito: marca, contagem, endereço em UTF-8 sem sobra e todo o resto zerado, para que
    /// dois textos diferentes nunca valham o mesmo registro.
    ///
    /// # Errors
    ///
    /// [`PresenceError::Malformed`] para qualquer desvio.
    pub fn decode_points_v2(plain: &[u8]) -> Result<Vec<IntroPointV2>, PresenceError> {
        let bad = PresenceError::Malformed;
        if plain.len() != PLAINTEXT_LEN || plain.first() != Some(&V2_MARK) {
            return Err(bad);
        }
        let count = usize::from(*plain.get(1).ok_or(PresenceError::Malformed)?);
        if count > MAX_POINTS_V2 {
            return Err(bad);
        }
        let mut at = 2;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let raw = plain.get(at..at + POINT_LEN).ok_or(PresenceError::Malformed)?;
            at += POINT_LEN;
            let (len, rest) = raw.split_first().ok_or(PresenceError::Malformed)?;
            let len = usize::from(*len);
            let (addr, rest) = rest.split_at_checked(MAX_ADDR_V2).ok_or(PresenceError::Malformed)?;
            let (used, pad) = addr.split_at_checked(len).ok_or(PresenceError::Malformed)?;
            if len == 0 || pad.iter().any(|&b| b != 0) {
                return Err(bad);
            }
            let addr = String::from_utf8(used.to_vec()).map_err(|_| PresenceError::Malformed)?;
            let (public, rest) = rest.split_first_chunk::<32>().ok_or(PresenceError::Malformed)?;
            let (link_public, rest) = rest.split_first_chunk::<32>().ok_or(PresenceError::Malformed)?;
            let (auth_key, _) = rest.split_first_chunk::<32>().ok_or(PresenceError::Malformed)?;
            out.push(IntroPointV2 { addr, public: *public, link_public: *link_public, auth_key: *auth_key });
        }
        if plain.get(at..).is_none_or(|tail| tail.iter().any(|&b| b != 0)) {
            return Err(bad);
        }
        Ok(out)
    }

    /// Cria o registro v2 sob `lookup_id`: o mesmo envelope, assinatura e endereço como dado associado da [`super::seal`].
    ///
    /// # Errors
    ///
    /// Como [`encode_points_v2`].
    pub fn seal_v2(
        keys: &PresenceKeys,
        signer: &SigningKey,
        lookup_id: &[u8; 32],
        points: &[IntroPointV2],
    ) -> Result<[u8; RECORD_LEN], PresenceError> {
        seal_plain(keys, signer, lookup_id, &encode_points_v2(points)?)
    }

    /// Abre um registro v2.
    ///
    /// # Errors
    ///
    /// Como [`super::open`]; mais [`PresenceError::Malformed`] se o texto claro não é v2 (um registro v1 cai aqui).
    pub fn open_v2(
        keys: &PresenceKeys,
        owner: &VerifyingKey,
        lookup_id: &[u8; 32],
        record: &[u8],
    ) -> Result<Vec<IntroPointV2>, PresenceError> {
        decode_points_v2(&open_plain(keys, owner, lookup_id, record)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::{PublicKey, StaticSecret};

    struct Fixture {
        keys_a: PresenceKeys,
        keys_b: PresenceKeys,
        signer: SigningKey,
    }

    fn fixture() -> Fixture {
        let a = StaticSecret::random_from_rng(OsRng);
        let b = StaticSecret::random_from_rng(OsRng);
        Fixture {
            keys_a: PresenceKeys::derive(&a, &PublicKey::from(&b)),
            keys_b: PresenceKeys::derive(&b, &PublicKey::from(&a)),
            signer: SigningKey::generate(&mut OsRng),
        }
    }

    fn intros() -> Vec<Vec<u8>> {
        vec![b"relay-1.example:4001".to_vec(), b"relay-2.example:4001".to_vec()]
    }

    #[test]
    fn roundtrip() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        let rec = seal(&f.keys_a, &f.signer, &id, &intros()).unwrap();
        let got = open(&f.keys_b, &f.signer.verifying_key(), &id, &rec).unwrap();
        assert_eq!(got, intros());
    }

    #[test]
    fn size_is_constant() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        let small = seal(&f.keys_a, &f.signer, &id, &[]).unwrap();
        let big = seal(&f.keys_a, &f.signer, &id, &vec![vec![7u8; MAX_INTRO_LEN]; MAX_INTRO_POINTS]).unwrap();
        assert_eq!(small.len(), big.len());
        assert_eq!(small.len(), RECORD_LEN);
    }

    #[test]
    fn stranger_cannot_open() {
        let f = fixture();
        let other = fixture();
        let id = f.keys_a.lookup_id(1);
        let rec = seal(&f.keys_a, &f.signer, &id, &intros()).unwrap();
        assert_eq!(
            open(&other.keys_a, &f.signer.verifying_key(), &id, &rec),
            Err(PresenceError::Decrypt)
        );
    }

    #[test]
    fn rejects_wrong_signer_tamper_and_moved_address() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        let rec = seal(&f.keys_a, &f.signer, &id, &intros()).unwrap();

        let imposter = SigningKey::generate(&mut OsRng);
        assert_eq!(open(&f.keys_b, &imposter.verifying_key(), &id, &rec), Err(PresenceError::BadSignature));

        let mut bad = rec;
        bad[20] ^= 1;
        assert_eq!(open(&f.keys_b, &f.signer.verifying_key(), &id, &bad), Err(PresenceError::BadSignature));

        let next = f.keys_a.lookup_id(2);
        assert_eq!(open(&f.keys_b, &f.signer.verifying_key(), &next, &rec), Err(PresenceError::BadSignature));

        assert_eq!(open(&f.keys_b, &f.signer.verifying_key(), &id, &rec[..10]), Err(PresenceError::BadLength));
    }

    #[test]
    fn validates_intro_points() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        assert_eq!(
            seal(&f.keys_a, &f.signer, &id, &vec![b"x".to_vec(); MAX_INTRO_POINTS + 1]),
            Err(PresenceError::TooManyIntroPoints)
        );
        assert_eq!(seal(&f.keys_a, &f.signer, &id, &[vec![]]), Err(PresenceError::BadIntroPoint));
        assert_eq!(
            seal(&f.keys_a, &f.signer, &id, &[vec![1; MAX_INTRO_LEN + 1]]),
            Err(PresenceError::BadIntroPoint)
        );
    }

    fn points() -> Vec<IntroPointV2> {
        (1..=MAX_POINTS_V2 as u8)
            .map(|i| IntroPointV2 { addr: format!("203.0.113.{i}:4001"), public: [i; 32], link_public: [i + 10; 32], auth_key: [i + 20; 32] })
            .collect()
    }

    #[test]
    fn v2_roundtrip_keeps_the_fixed_size() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        let full = seal_v2(&f.keys_a, &f.signer, &id, &points()).unwrap();
        let empty = seal_v2(&f.keys_a, &f.signer, &id, &[]).unwrap();
        assert_eq!((full.len(), empty.len()), (RECORD_LEN, RECORD_LEN));
        assert_eq!(open_v2(&f.keys_b, &f.signer.verifying_key(), &id, &full).unwrap(), points());
        assert_eq!(open_v2(&f.keys_b, &f.signer.verifying_key(), &id, &empty).unwrap(), vec![]);
        let longest = IntroPointV2 { addr: "x".repeat(MAX_ADDR_V2), ..points().remove(0) };
        assert!(seal_v2(&f.keys_a, &f.signer, &id, &[longest]).is_ok());
    }

    #[test]
    fn v2_and_v1_do_not_open_each_other() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        let v2 = seal_v2(&f.keys_a, &f.signer, &id, &points()).unwrap();
        let v1 = seal(&f.keys_a, &f.signer, &id, &intros()).unwrap();
        assert_eq!(open(&f.keys_b, &f.signer.verifying_key(), &id, &v2), Err(PresenceError::Malformed), "contato antigo não abre a v2");
        assert_eq!(open_v2(&f.keys_b, &f.signer.verifying_key(), &id, &v1), Err(PresenceError::Malformed));
    }

    #[test]
    fn v2_keeps_the_envelope_guarantees() {
        let f = fixture();
        let other = fixture();
        let id = f.keys_a.lookup_id(1);
        let rec = seal_v2(&f.keys_a, &f.signer, &id, &points()).unwrap();
        assert_eq!(open_v2(&other.keys_a, &f.signer.verifying_key(), &id, &rec), Err(PresenceError::Decrypt));
        let imposter = SigningKey::generate(&mut OsRng);
        assert_eq!(open_v2(&f.keys_b, &imposter.verifying_key(), &id, &rec), Err(PresenceError::BadSignature));
        assert_eq!(open_v2(&f.keys_b, &f.signer.verifying_key(), &f.keys_a.lookup_id(2), &rec), Err(PresenceError::BadSignature));
    }

    #[test]
    fn v2_validates_and_decodes_strictly() {
        let f = fixture();
        let id = f.keys_a.lookup_id(1);
        let many = vec![points().remove(0); MAX_POINTS_V2 + 1];
        assert_eq!(seal_v2(&f.keys_a, &f.signer, &id, &many), Err(PresenceError::TooManyIntroPoints));
        for addr in [String::new(), "x".repeat(MAX_ADDR_V2 + 1)] {
            let bad = IntroPointV2 { addr, ..points().remove(0) };
            assert_eq!(seal_v2(&f.keys_a, &f.signer, &id, &[bad]), Err(PresenceError::BadIntroPoint));
        }
        // Sobra no preenchimento, endereço inválido e contagem acima do limite são recusados.
        let good = encode_points_v2(&points()).unwrap();
        assert_eq!(decode_points_v2(&good).unwrap(), points());
        let mut dirty = good;
        dirty[PLAINTEXT_LEN - 1] = 1;
        assert_eq!(decode_points_v2(&dirty), Err(PresenceError::Malformed));
        let mut utf = good;
        utf[3] = 0xFF;
        assert_eq!(decode_points_v2(&utf), Err(PresenceError::Malformed));
        let mut count = good;
        count[1] = MAX_POINTS_V2 as u8 + 1;
        assert_eq!(decode_points_v2(&count), Err(PresenceError::Malformed));
        assert_eq!(decode_points_v2(&good[..10]), Err(PresenceError::Malformed));
    }

    #[test]
    fn decoys_have_real_shape() {
        assert_eq!(decoy_record().len(), RECORD_LEN);
        assert_ne!(decoy_lookup_id(), decoy_lookup_id());
    }
}
