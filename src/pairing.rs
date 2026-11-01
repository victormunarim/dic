//! Pareamento: segredo X25519 compartilhado → chave de cifra e semente de busca (HKDF).

use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

const SALT: &[u8] = b"clam/v1/pairing";
const INFO_ENC: &[u8] = b"presence-enc";
const INFO_LOOKUP: &[u8] = b"presence-lookup-seed";
const INFO_EPOCH: &[u8] = b"presence-lookup-epoch";
const INFO_DHT_SEED: &[u8] = b"clam/presenca/v2/dht-assina";
const INFO_DHT_SALT: &[u8] = b"clam/presenca/v2/dht-sal";
const INFO_OWNER: &[u8] = b"clam/presenca/v2/dono";

/// Chaves de presença compartilhadas entre dois contatos.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PresenceKeys {
    pub(crate) enc_key: [u8; 32],
    lookup_seed: [u8; 32],
}

impl PresenceKeys {
    /// Deriva as chaves a partir do segredo do dispositivo e da chave pública do contato.
    /// Simétrico: os dois lados chegam às mesmas chaves.
    pub fn derive(my_secret: &StaticSecret, their_public: &PublicKey) -> Self {
        let shared = my_secret.diffie_hellman(their_public);
        // Salt independe da ordem dos lados; o segredo ECDH já é simétrico.
        let hk = Hkdf::<Sha256>::new(Some(SALT), shared.as_bytes());
        let mut enc_key = [0u8; 32];
        let mut lookup_seed = [0u8; 32];
        hk.expand(INFO_ENC, &mut enc_key).expect("32 bytes é válido");
        hk.expand(INFO_LOOKUP, &mut lookup_seed).expect("32 bytes é válido");
        Self { enc_key, lookup_seed }
    }

    /// Identificador de busca na DHT para um período; gira a cada época.
    pub fn lookup_id(&self, epoch: u64) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(SALT), &self.lookup_seed);
        let mut info = [0u8; INFO_EPOCH.len() + 8];
        info[..INFO_EPOCH.len()].copy_from_slice(INFO_EPOCH);
        info[INFO_EPOCH.len()..].copy_from_slice(&epoch.to_be_bytes());
        let mut out = [0u8; 32];
        hk.expand(&info, &mut out).expect("32 bytes é válido");
        out
    }

    /// Onde o registro de presença da época fica na DHT Mainline: a semente da chave de assinatura do item e o sal.
    /// Os dois lados do par derivam o mesmo alvo, e só eles (vem do segredo do par, não de dado público); a época
    /// entra na derivação, então o alvo muda sozinho e uma época não se liga à outra.
    pub fn dht_target(&self, epoch: u64) -> ([u8; 32], [u8; 32]) {
        let id = self.lookup_id(epoch);
        let derive = |info: &[u8]| {
            let mut out = [0u8; 32];
            Hkdf::<Sha256>::new(Some(SALT), &id).expand(info, &mut out).expect("32 bytes é válido");
            out
        };
        (derive(INFO_DHT_SEED), derive(INFO_DHT_SALT))
    }

    /// A chave que assina o registro de presença de `owner` neste par. Sai do segredo do par, não de uma chave
    /// guardada: os dois lados chegam à mesma chave de verificação sem trocar nada além das chaves de contato.
    /// Por isso a assinatura prova só que o registro é de alguém do par (o mesmo que a cifra já prova); não separa
    /// `owner` do outro lado.
    pub fn owner_signing_key(&self, owner: &[u8; 32]) -> SigningKey {
        let mut info = INFO_OWNER.to_vec();
        info.extend_from_slice(owner);
        let mut seed = [0u8; 32];
        Hkdf::<Sha256>::new(Some(SALT), &self.lookup_seed).expand(&info, &mut seed).expect("32 bytes é válido");
        let key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn pair() -> (PresenceKeys, PresenceKeys) {
        let a = StaticSecret::random_from_rng(OsRng);
        let b = StaticSecret::random_from_rng(OsRng);
        (
            PresenceKeys::derive(&a, &PublicKey::from(&b)),
            PresenceKeys::derive(&b, &PublicKey::from(&a)),
        )
    }

    #[test]
    fn both_sides_agree() {
        let (ka, kb) = pair();
        assert_eq!(ka.enc_key, kb.enc_key);
        assert_eq!(ka.lookup_id(7), kb.lookup_id(7));
    }

    #[test]
    fn lookup_id_rotates_and_is_pair_specific() {
        let (ka, _) = pair();
        let (kc, _) = pair();
        assert_ne!(ka.lookup_id(1), ka.lookup_id(2));
        assert_ne!(ka.lookup_id(1), kc.lookup_id(1));
    }

    #[test]
    fn dht_target_and_owner_key_are_shared_by_the_pair_only() {
        let (ka, kb) = pair();
        let (kc, _) = pair();
        assert_eq!(ka.dht_target(5), kb.dht_target(5));
        assert_ne!(ka.dht_target(5), ka.dht_target(6));
        assert_ne!(ka.dht_target(5), kc.dht_target(5));
        let (seed, salt) = ka.dht_target(5);
        assert_ne!(seed, salt);
        assert_ne!(seed, ka.lookup_id(5), "o alvo não repete o identificador que vai de dado associado");
        let owner = [7u8; 32];
        assert_eq!(ka.owner_signing_key(&owner).verifying_key(), kb.owner_signing_key(&owner).verifying_key());
        assert_ne!(ka.owner_signing_key(&owner).verifying_key(), ka.owner_signing_key(&[8u8; 32]).verifying_key());
        assert_ne!(ka.owner_signing_key(&owner).verifying_key(), kc.owner_signing_key(&owner).verifying_key());
    }

    #[test]
    fn enc_key_differs_from_lookup_seed() {
        let (ka, _) = pair();
        assert_ne!(ka.enc_key, ka.lookup_seed);
    }
}
