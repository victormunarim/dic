//! # clam
//!
//! Núcleo de privacidade da rede de chamadas P2P: modo privado por padrão e
//! presença cifrada. Veja o `README.md` e a pasta `docs/` para o guia completo.
//!
//! | Módulo | Papel |
//! | --- | --- |
//! | [`pairing`] | Pareamento X25519 → chaves de presença (HKDF) |
//! | [`presence`] | Registro de presença cifrado, assinado e de tamanho fixo |
//! | [`service`] | Publicar, buscar (com consultas falsas) e tópicos de círculo (camada de biblioteca: o `Node` não a usa) |
//! | [`store`] | Trait [`store::RecordStore`] que o transporte (DHT) implementa |
//! | [`circle`] | Tópicos derivados do segredo do grupo |
//! | [`intro`] | Escolha dos pontos de introdução (relays semente, saúde, diversidade) |
//! | [`circuit`] | Circuito de 2 saltos: handshake por salto, camadas AEAD, relay e cliente |
//! | [`media`] | Chamada de voz: E2E, pacote fixo de 200 B, buffer de jitter, codec |
//! | `audio` | Microfone e alto-falante (8 kHz mono) via `cpal` (feature `audio`) |
//! | `upnp` | Abre a porta do relay no roteador (feature `net`) |
//! | `timer` | Temporizadores de 1 ms no Windows (feature `net`) |
//! | `node` | `Node`: relay + telefone + fila de endereços; liga só com a chave do outro e o relay dele (feature `net`) |
//! | `rendezvous` | Ponto de encontro na DHT Mainline (BEP 44) e âncoras, para reencontrar um contato que mudou de endereço (feature `net`) |
//! | `video` | Codificação H.264 da tela: controle de taxa, qualidade e NACK (feature `video`) |
//! | `scroll` | Mensagem de rolagem da tela: desloca a imagem em vez de recodificá-la (feature `video`) |
//! | `hwenc` | Codificador por hardware opcional, via `gst-launch` (feature `video`, Linux) |
//! | `capture`, `portal` | Captura da tela: X11 e GDI; portal do desktop no Wayland (features `capture`, `portal`) |
//! | `phone` | Chamado (`Phone`) e chamador (`call`) sobre o circuito (feature `net`) |
//! | [`path`] | Modos de caminho: 2 saltos (padrão), direto |
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
pub(crate) mod bytes;
pub mod circle;
pub mod cover;
pub mod circuit;
pub mod descriptor;
#[cfg(feature = "net")]
pub mod discovery;
pub mod intro;
pub mod mailbox;
pub mod media;
#[cfg(feature = "net")]
pub mod link;
pub mod pairing;
#[cfg(feature = "net")]
pub mod net;
pub mod path;
pub mod sync;
#[cfg(feature = "net")]
#[cfg(feature = "net")]
pub mod node;
#[cfg(feature = "net")]
pub mod phone;
#[cfg(feature = "net")]
pub mod rendezvous;
#[cfg(feature = "net")]
pub mod timer;
#[cfg(feature = "video")]
pub mod hwenc;
#[cfg(feature = "video")]
pub mod prefetch;
#[cfg(feature = "video")]
pub mod scroll;
#[cfg(feature = "video")]
pub mod video;
#[cfg(feature = "capture")]
pub mod capture;
#[cfg(all(feature = "portal", unix))]
pub mod portal;
#[cfg(feature = "net")]
pub mod upnp;
#[cfg(feature = "net")]
pub mod runner;
pub mod presence;
pub mod reputation;
pub mod server;
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
