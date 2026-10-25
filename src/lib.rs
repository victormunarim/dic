//! # clam
//!
//! Núcleo de privacidade da rede de chamadas P2P: modo privado por padrão e
//! presença cifrada. Veja o `README.md` e a pasta `docs/` para o guia completo.
//!
//! | Módulo | Papel |
//! | --- | --- |
//! | [`pairing`] | Pareamento X25519 → chaves de presença (HKDF) |
//! | [`presence`] | Registro de presença cifrado, assinado e de tamanho fixo |
//! | [`service`] | Publicar, buscar (com consultas falsas) e tópicos de círculo |
//! | [`store`] | Trait [`store::RecordStore`] que o transporte (DHT) implementa |
//! | [`circle`] | Tópicos derivados do segredo do grupo |
//! | [`intro`] | Escolha dos pontos de introdução (relays semente, saúde, diversidade) |
//! | [`circuit`] | Circuito de 2 saltos: handshake por salto, camadas AEAD, relay e cliente |
//! | [`media`] | Chamada de voz: E2E, pacote fixo de 200 B, buffer de jitter, codec |
//! | `audio` | Microfone e alto-falante (8 kHz mono) via `cpal` (feature `audio`) |
//! | `clock` | Hora do diretório para os convites, sem depender do relógio local (feature `net`) |
//! | `upnp` | Abre a porta do relay no roteador (feature `net`) |
//! | `timer` | Temporizadores de 1 ms no Windows (feature `net`) |
//! | `directory` | Diretório de primeiro contato: publicar e achar o relay de uma chave (feature `net`) |
//! | `node` | `Node`: relay + telefone + publicação no diretório; liga só com a chave do outro (feature `net`) |
//! | `room` | Salas persistentes em malha: segredo salvo, anfitrião pelo diretório, mixagem (feature `net`) |
//! | `phone` | Chamado (`Phone`) e chamador (`call`) sobre o circuito (feature `net`) |
//! | [`path`] | Modos de caminho: 2 saltos (padrão), 1 salto, direto |
//!
//! ```
//! use clam::{
//!     ed25519_dalek::SigningKey,
//!     pairing::PresenceKeys,
//!     rand_core::OsRng,
//!     service::{self, PresenceConfig},
//!     store::MemoryStore,
//!     x25519_dalek::{PublicKey, StaticSecret},
//! };
//!
//! let a = StaticSecret::random_from_rng(OsRng);
//! let b = StaticSecret::random_from_rng(OsRng);
//! let signer = SigningKey::generate(&mut OsRng);
//! let (ka, kb) = (
//!     PresenceKeys::derive(&a, &PublicKey::from(&b)),
//!     PresenceKeys::derive(&b, &PublicKey::from(&a)),
//! );
//!
//! let (store, cfg, now) = (MemoryStore::new(), PresenceConfig::default(), 1_795_000_000);
//! let intros = vec![b"relay.example:4001".to_vec()];
//! service::publish(&store, &cfg, &ka, &signer, now, &intros).unwrap();
//! assert_eq!(service::lookup(&store, &cfg, &kb, &signer.verifying_key(), now), Some(intros));
//! ```

pub use ed25519_dalek;
pub use rand_core;
pub use x25519_dalek;

#[cfg(feature = "audio")]
pub mod audio;
pub mod circle;
pub mod cover;
pub mod circuit;
pub mod descriptor;
#[cfg(feature = "net")]
pub mod discovery;
pub mod intro;
pub mod media;
#[cfg(feature = "net")]
pub mod link;
pub mod pairing;
#[cfg(feature = "net")]
pub mod net;
pub mod path;
#[cfg(feature = "net")]
pub mod clock;
#[cfg(feature = "net")]
pub mod directory;
#[cfg(feature = "net")]
pub mod node;
#[cfg(feature = "net")]
pub mod phone;
#[cfg(feature = "net")]
pub mod punch;
#[cfg(feature = "net")]
pub mod timer;
#[cfg(feature = "video")]
pub mod video;
#[cfg(feature = "capture")]
pub mod capture;
#[cfg(all(feature = "portal", unix))]
pub mod portal;
#[cfg(feature = "net")]
pub mod upnp;
#[cfg(feature = "net")]
pub mod room;
#[cfg(feature = "net")]
pub mod runner;
pub mod presence;
pub mod reputation;
pub mod service;
pub mod store;
#[cfg(feature = "net")]
pub mod transport;

/// Duração padrão de um período de rotação do endereço de presença.
pub const DEFAULT_EPOCH_SECS: u64 = 3600;

/// Período de rotação a que um instante (segundos Unix) pertence.
pub fn epoch_of(unix_secs: u64, epoch_secs: u64) -> u64 {
    unix_secs / epoch_secs
}
