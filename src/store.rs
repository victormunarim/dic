//! Abstração do armazenamento distribuído (DHT). O transporte real (libp2p)
//! implementa `RecordStore`; `MemoryStore` serve a testes e simulações.

use crate::sync::LockExt;
use std::{collections::HashMap, sync::Mutex};

pub trait RecordStore {
    fn put(&self, key: [u8; 32], value: Vec<u8>);
    fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>>;
}

/// Controle de assinatura de tópicos (GossipSub). Separado de [`RecordStore`]
/// porque nem todo armazenamento tem pub/sub.
pub trait TopicControl {
    fn subscribe(&self, topic: &str);
    fn unsubscribe(&self, topic: &str);
}

#[derive(Default)]
pub struct MemoryStore {
    records: Mutex<HashMap<[u8; 32], Vec<u8>>>,
    gets: Mutex<Vec<[u8; 32]>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Chaves consultadas até agora, na ordem; é o que um observador da DHT veria.
    pub fn observed_gets(&self) -> Vec<[u8; 32]> {
        self.gets.lock_unpoisoned().clone()
    }

    pub fn keys(&self) -> Vec<[u8; 32]> {
        self.records.lock_unpoisoned().keys().copied().collect()
    }
}

impl RecordStore for MemoryStore {
    fn put(&self, key: [u8; 32], value: Vec<u8>) {
        self.records.lock_unpoisoned().insert(key, value);
    }

    fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        self.gets.lock_unpoisoned().push(*key);
        self.records.lock_unpoisoned().get(key).cloned()
    }
}
