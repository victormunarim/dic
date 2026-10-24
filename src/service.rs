//! Serviço de presença: publica o registro cifrado na época atual e consulta
//! contatos tolerando defasagem de relógio, misturando consultas falsas.

use ed25519_dalek::{SigningKey, VerifyingKey};
use rand_core::{OsRng, RngCore};

use crate::{
    DEFAULT_EPOCH_SECS, epoch_of,
    circle::{CircleSecret, TopicKind},
    pairing::PresenceKeys,
    presence::{self, PresenceError},
    store::RecordStore,
};

#[derive(Debug, Clone, Copy)]
pub struct PresenceConfig {
    pub epoch_secs: u64,
    /// Consultas falsas misturadas a cada busca real.
    pub decoys_per_lookup: usize,
}

impl Default for PresenceConfig {
    fn default() -> Self {
        Self { epoch_secs: DEFAULT_EPOCH_SECS, decoys_per_lookup: 3 }
    }
}

/// Publica os pontos de introdução sob o endereço da época atual.
/// Chamar de novo a cada período (e ao mudar os pontos de introdução).
pub fn publish(
    store: &dyn RecordStore,
    cfg: &PresenceConfig,
    keys: &PresenceKeys,
    signer: &SigningKey,
    now: u64,
    intro_points: &[Vec<u8>],
) -> Result<(), PresenceError> {
    let id = keys.lookup_id(epoch_of(now, cfg.epoch_secs));
    let record = presence::seal(keys, signer, &id, intro_points)?;
    store.put(id, record.to_vec());
    Ok(())
}

/// Busca a presença de um contato. Tenta a época atual, a anterior e a seguinte
/// (relógios dessincronizados). Cada busca vem embaralhada com consultas falsas.
pub fn lookup(
    store: &dyn RecordStore,
    cfg: &PresenceConfig,
    keys: &PresenceKeys,
    owner: &VerifyingKey,
    now: u64,
) -> Option<Vec<Vec<u8>>> {
    let epoch = epoch_of(now, cfg.epoch_secs);
    let mut candidates: Vec<Option<[u8; 32]>> = [Some(epoch), epoch.checked_sub(1), epoch.checked_add(1)]
        .into_iter()
        .flatten()
        .map(|e| Some(keys.lookup_id(e)))
        .collect();
    candidates.extend((0..cfg.decoys_per_lookup).map(|_| None));
    shuffle(&mut candidates);

    let mut found = None;
    for cand in candidates {
        match cand {
            Some(id) => {
                if found.is_some() {
                    continue;
                }
                if let Some(rec) = store.get(&id) {
                    found = presence::open(keys, owner, &id, &rec).ok();
                }
            }
            None => {
                store.get(&presence::decoy_lookup_id());
            }
        }
    }
    found
}

/// Tópicos GossipSub a assinar agora: época atual e a seguinte, para a troca
/// de período não derrubar a assinatura.
pub fn circle_topics(circle: &CircleSecret, cfg: &PresenceConfig, now: u64) -> Vec<String> {
    let epoch = epoch_of(now, cfg.epoch_secs);
    [TopicKind::Presence, TopicKind::CallAnnounce]
        .into_iter()
        .flat_map(|k| [circle.topic(k, epoch), circle.topic(k, epoch + 1)])
        .collect()
}

fn shuffle<T>(items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let j = (OsRng.next_u64() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use x25519_dalek::{PublicKey, StaticSecret};

    const T0: u64 = 10 * DEFAULT_EPOCH_SECS + 5;

    fn setup() -> (PresenceKeys, PresenceKeys, SigningKey) {
        let a = StaticSecret::random_from_rng(OsRng);
        let b = StaticSecret::random_from_rng(OsRng);
        (
            PresenceKeys::derive(&a, &PublicKey::from(&b)),
            PresenceKeys::derive(&b, &PublicKey::from(&a)),
            SigningKey::generate(&mut OsRng),
        )
    }

    fn intros() -> Vec<Vec<u8>> {
        vec![b"relay-1:4001".to_vec()]
    }

    #[test]
    fn contact_finds_presence() {
        let (ka, kb, sk) = setup();
        let (store, cfg) = (MemoryStore::new(), PresenceConfig::default());
        publish(&store, &cfg, &ka, &sk, T0, &intros()).unwrap();
        assert_eq!(lookup(&store, &cfg, &kb, &sk.verifying_key(), T0), Some(intros()));
    }

    #[test]
    fn tolerates_clock_skew_but_not_stale_records() {
        let (ka, kb, sk) = setup();
        let (store, cfg) = (MemoryStore::new(), PresenceConfig::default());
        publish(&store, &cfg, &ka, &sk, T0, &intros()).unwrap();
        let next = T0 + DEFAULT_EPOCH_SECS;
        assert_eq!(lookup(&store, &cfg, &kb, &sk.verifying_key(), next), Some(intros()));
        let far = T0 + 3 * DEFAULT_EPOCH_SECS;
        assert_eq!(lookup(&store, &cfg, &kb, &sk.verifying_key(), far), None);
    }

    #[test]
    fn address_rotates_between_epochs() {
        let (ka, _, sk) = setup();
        let (store, cfg) = (MemoryStore::new(), PresenceConfig::default());
        publish(&store, &cfg, &ka, &sk, T0, &intros()).unwrap();
        publish(&store, &cfg, &ka, &sk, T0 + DEFAULT_EPOCH_SECS, &intros()).unwrap();
        let keys = store.keys();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
    }

    #[test]
    fn stranger_finds_nothing() {
        let (ka, _, sk) = setup();
        let (other, _, _) = setup();
        let (store, cfg) = (MemoryStore::new(), PresenceConfig::default());
        publish(&store, &cfg, &ka, &sk, T0, &intros()).unwrap();
        assert_eq!(lookup(&store, &cfg, &other, &sk.verifying_key(), T0), None);
    }

    #[test]
    fn lookup_mixes_in_decoys() {
        let (_, kb, sk) = setup();
        let (store, cfg) = (MemoryStore::new(), PresenceConfig { decoys_per_lookup: 4, ..Default::default() });
        lookup(&store, &cfg, &kb, &sk.verifying_key(), T0);
        assert_eq!(store.observed_gets().len(), 3 + 4);
    }

    #[test]
    fn circle_topics_cover_current_and_next_epoch() {
        let c = CircleSecret::from_bytes([9; 32]);
        let cfg = PresenceConfig::default();
        let t = circle_topics(&c, &cfg, T0);
        assert_eq!(t.len(), 4);
        assert!(t.contains(&c.topic(TopicKind::Presence, 11)));
        // Na troca de período, a assinatura anterior continua válida.
        assert!(circle_topics(&c, &cfg, T0 + DEFAULT_EPOCH_SECS).contains(&t[1]));
    }
}
