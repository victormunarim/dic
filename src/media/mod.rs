//! Camada de mídia para chamada de voz 1:1: cifra ponta a ponta, pacote de tamanho fixo e
//! buffer de jitter. É *sans-IO*: quem chama entrega o tempo (ms) e os bytes.
//!
//! ```text
//! microfone ─> Codec::encode ─> VoiceCall::send ─> [200 B] ─> CircuitClient::send
//! alto-falante <─ Codec::decode <─ VoiceCall::playout <─ jitter <─ VoiceCall::receive <─ Event::Data
//! ```
//!
//! - [`keys`]: handshake autenticado (efêmeras + chaves estáticas do pareamento) e chaves por direção.
//! - [`packet`]: pacote de **200 bytes sempre** (cabe nos 203 de uma célula de 2 saltos), AEAD,
//!   janela anti-replay. O tamanho do quadro de voz não aparece nem no fio entre o relay de
//!   saída e o destino.
//! - [`jitter`]: reordena, detecta perda e adapta o atraso de reprodução.
//! - [`codec`]: trait do codec; vêm o G.722, o Opus (Rust puro, 16 kHz) e o G.711 µ-law. O convite diz qual a chamada usa.
//! - [`signal`]: convite, aceite e recusa, em quadros de 200 B (a identidade de quem liga vai cifrada).
//! - [`update`]: aviso de endereço novo de um contato para outro (quadros de 200 B).
//! - [`screen`]: vídeo da tela em fragmentos de 200 B num circuito próprio, com taxa fixa.
//! - [`VoiceCall`]: junta tudo.
//!
//! Sem DTX: o cover do circuito já manda uma célula a cada 20 ms, então calar não economiza banda
//! e o silêncio fica igual à fala para quem observa a rede.

pub mod codec;
pub mod jitter;
pub mod keys;
pub mod packet;
pub mod screen;
pub mod signal;
pub mod update;

mod call;
pub use call::{CallStats, VoiceCall};

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("pacote com tamanho diferente de {}", packet::PACKET_LEN)]
    Size,
    #[error("autenticação do pacote falhou")]
    Decrypt,
    #[error("pacote repetido ou antigo demais")]
    Replay,
    #[error("quadro codificado de {0} bytes passa de {max}", max = packet::MAX_PAYLOAD)]
    TooLarge(usize),
    #[error("pacote autenticado mas malformado")]
    Malformed,
    #[error("chave pública inválida (ponto de ordem baixa)")]
    BadKey,
    #[error("PCM com {got} amostras; o codec espera {want}")]
    FrameLen { got: usize, want: usize },
    #[error("contador de pacotes esgotado")]
    CounterExhausted,
}
