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
    let plain = encode(intro_points)?;
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.enc_key));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: lookup_id })
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
    if record.len() != RECORD_LEN {
        return Err(PresenceError::BadLength);
    }
    let body_end = RECORD_LEN - SIG_LEN;
    let sig = Signature::from_slice(&record[body_end..]).map_err(|_| PresenceError::BadSignature)?;
    owner
        .verify(&signed_message(lookup_id, &record[..body_end]), &sig)
        .map_err(|_| PresenceError::BadSignature)?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.enc_key));
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&record[..NONCE_LEN]),
            Payload { msg: &record[NONCE_LEN..body_end], aad: lookup_id },
        )
        .map_err(|_| PresenceError::Decrypt)?;
    decode(&plain)
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

    #[test]
    fn decoys_have_real_shape() {
        assert_eq!(decoy_record().len(), RECORD_LEN);
        assert_ne!(decoy_lookup_id(), decoy_lookup_id());
    }
}
