//! Circuito de 2 saltos (modo privado): você → relay A → relay B → destino.
//!
//! Tudo aqui é *sans-IO*: as máquinas de estado recebem células e devolvem
//! ações; quem move bytes (libp2p, UDP, simulador) fica por fora.
//!
//! - [`crypto`]: handshake X25519 por salto (estilo ntor) e camadas AEAD.
//! - [`client`]: [`client::ClientCircuit`], do lado de quem origina a chamada.
//! - [`relay`]: [`relay::RelayNode`], o relay voluntário, com limites de banda.
//!
//! Formato: todas as células têm tamanho fixo ([`CELL_LEN`]). Cada salto
//! retira uma camada de [`LAYER_OVERHEAD`] bytes e devolve a célula completada
//! com bytes aleatórios, então o tamanho no fio não revela a posição do salto.
//! O relay A só conhece o seu IP e o de B; B só conhece o de A e o destino.
//! O conteúdo da mídia continua cifrado de ponta a ponta (MLS) por cima.

pub mod client;
pub mod crypto;
pub mod manager;
pub mod relay;

use rand_core::{OsRng, RngCore};
use thiserror::Error;

/// Tamanho fixo do corpo de toda célula. Com 2 saltos sobram 203 bytes de
/// dados por célula (um quadro Opus de 20 ms cabe). Troque a constante para
/// ajustar a relação entre banda e tamanho máximo de pacote.
pub const CELL_LEN: usize = 256;
/// Número máximo de relays no circuito.
pub const MAX_HOPS: usize = 2;
/// Contador (8) + tag AEAD (16) + flag (1) acrescentados por camada.
pub const LAYER_OVERHEAD: usize = 25;
const RELAY_HEADER: usize = 3;

/// Bytes úteis de uma célula depois de `pos` camadas retiradas.
pub const fn link_len(pos: usize) -> usize {
    CELL_LEN - LAYER_OVERHEAD * pos
}

/// Tamanho do conteúdo (comando + dados) endereçado ao salto `pos`.
pub const fn relay_cell_len(pos: usize) -> usize {
    link_len(pos) - LAYER_OVERHEAD
}

/// Máximo de bytes de dados que cabem em uma célula endereçada ao salto `pos`.
pub const fn max_data(pos: usize) -> usize {
    relay_cell_len(pos) - RELAY_HEADER
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CircuitError {
    #[error("handshake inválido (ponto fraco ou autenticação do relay falhou)")]
    Handshake,
    #[error("falha ao decifrar a camada")]
    Decrypt,
    #[error("célula repetida ou fora da janela")]
    Replay,
    #[error("contador de nonce esgotado: refaça o circuito")]
    CounterExhausted,
    #[error("dados grandes demais para a célula")]
    Oversize,
    #[error("violação de protocolo: {0}")]
    Protocol(&'static str),
    #[error("célula de outro circuito")]
    UnknownCircuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cmd {
    Create = 1,
    Created = 2,
    Relay = 3,
    Destroy = 4,
    /// Preenchimento de link (cover traffic): o transporte o descarta, nunca chega a um circuito.
    Padding = 5,
}

impl Cmd {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Cmd::Create,
            2 => Cmd::Created,
            3 => Cmd::Relay,
            4 => Cmd::Destroy,
            5 => Cmd::Padding,
            _ => return None,
        })
    }
}

/// Célula de tamanho fixo trocada entre dois nós vizinhos.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    pub circ: u32,
    pub cmd: Cmd,
    pub body: [u8; CELL_LEN],
}

impl Cell {
    pub const WIRE_LEN: usize = 5 + CELL_LEN;

    /// Monta a célula com `content` no início e bytes aleatórios no resto.
    pub fn with_body(circ: u32, cmd: Cmd, content: &[u8]) -> Self {
        debug_assert!(content.len() <= CELL_LEN);
        let mut body = [0u8; CELL_LEN];
        body[..content.len()].copy_from_slice(content);
        OsRng.fill_bytes(&mut body[content.len()..]);
        Self { circ, cmd, body }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::WIRE_LEN);
        out.extend_from_slice(&self.circ.to_be_bytes());
        out.push(self.cmd as u8);
        out.extend_from_slice(&self.body);
        out
    }

    /// Lê o que veio do link: o tamanho é fixo e qualquer outro é protocolo violado.
    #[deny(clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn from_bytes(buf: &[u8]) -> Result<Self, CircuitError> {
        let size = || CircuitError::Protocol("tamanho de célula inválido");
        if buf.len() != Self::WIRE_LEN {
            return Err(size());
        }
        let (circ, rest) = buf.split_first_chunk::<4>().ok_or_else(size)?;
        let (&cmd, body) = rest.split_first().ok_or_else(size)?;
        let cmd = Cmd::from_u8(cmd).ok_or(CircuitError::Protocol("comando desconhecido"))?;
        let body: [u8; CELL_LEN] = body.try_into().map_err(|_| size())?;
        Ok(Self { circ: u32::from_be_bytes(*circ), cmd, body })
    }
}

/// Completa `sealed` até [`CELL_LEN`] com bytes aleatórios.
pub(crate) fn pad_to_cell(sealed: &[u8]) -> [u8; CELL_LEN] {
    debug_assert!(sealed.len() <= CELL_LEN);
    let mut body = [0u8; CELL_LEN];
    body[..sealed.len()].copy_from_slice(sealed);
    OsRng.fill_bytes(&mut body[sealed.len()..]);
    body
}

/// Comandos dentro de uma célula endereçada a um salto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayCmd {
    /// Cliente → salto: estenda o circuito (`x2` de 32 bytes + endereço do próximo relay).
    Extend = 1,
    /// Salto → cliente: resposta do handshake do próximo relay (`y` + `tag`).
    Extended = 2,
    /// Cliente → saída: abra o fluxo para o destino (endereço em texto).
    Begin = 3,
    Data = 4,
    Ping = 5,
    Pong = 6,
    /// Fim do fluxo.
    End = 7,
    /// Preenchimento de circuito (cover de ponta a ponta): o salto de saída o descarta e,
    /// no sentido contrário, responde com `Data` da fila ou com outro `Drop`.
    Drop = 8,
}

impl RelayCmd {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => RelayCmd::Extend,
            2 => RelayCmd::Extended,
            3 => RelayCmd::Begin,
            4 => RelayCmd::Data,
            5 => RelayCmd::Ping,
            6 => RelayCmd::Pong,
            7 => RelayCmd::End,
            8 => RelayCmd::Drop,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCell {
    pub cmd: RelayCmd,
    pub data: Vec<u8>,
}

impl RelayCell {
    pub fn new(cmd: RelayCmd, data: Vec<u8>) -> Self {
        Self { cmd, data }
    }

    /// `[cmd:1][len:2][dados][zeros]` com exatamente `total` bytes.
    pub fn encode(&self, total: usize) -> Result<Vec<u8>, CircuitError> {
        if self.data.len() + RELAY_HEADER > total {
            return Err(CircuitError::Oversize);
        }
        let mut buf = vec![0u8; total];
        buf[0] = self.cmd as u8;
        buf[1..3].copy_from_slice(&(self.data.len() as u16).to_be_bytes());
        buf[RELAY_HEADER..RELAY_HEADER + self.data.len()].copy_from_slice(&self.data);
        Ok(buf)
    }

    #[deny(clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn decode(buf: &[u8]) -> Result<Self, CircuitError> {
        // `RELAY_HEADER` é este cabeçalho: comando (1) e tamanho (2).
        let [cmd, len0, len1, rest @ ..] = buf else {
            return Err(CircuitError::Protocol("célula de relay curta"));
        };
        let cmd = RelayCmd::from_u8(*cmd).ok_or(CircuitError::Protocol("comando de relay desconhecido"))?;
        let len = u16::from_be_bytes([*len0, *len1]) as usize;
        let data = rest.get(..len).ok_or(CircuitError::Protocol("tamanho de relay inválido"))?;
        Ok(Self { cmd, data: data.to_vec() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_consistent() {
        assert_eq!(link_len(0), CELL_LEN);
        assert_eq!(link_len(1), link_len(0) - LAYER_OVERHEAD);
        assert_eq!(max_data(1), 203);
        assert!(max_data(1) >= 200, "quadro Opus de 20 ms precisa caber");
    }

    #[test]
    fn cell_wire_roundtrip() {
        let c = Cell::with_body(7, Cmd::Relay, b"oi");
        assert_eq!(Cell::from_bytes(&c.to_bytes()).unwrap(), c);
        assert!(Cell::from_bytes(&[0; 3]).is_err());
        let mut bad = c.to_bytes();
        bad[4] = 99;
        assert!(Cell::from_bytes(&bad).is_err());
    }

    #[test]
    fn relay_cell_roundtrip_and_limits() {
        let rc = RelayCell::new(RelayCmd::Data, b"abc".to_vec());
        let enc = rc.encode(relay_cell_len(1)).unwrap();
        assert_eq!(enc.len(), relay_cell_len(1));
        assert_eq!(RelayCell::decode(&enc).unwrap(), rc);
        let big = RelayCell::new(RelayCmd::Data, vec![0; max_data(1) + 1]);
        assert_eq!(big.encode(relay_cell_len(1)), Err(CircuitError::Oversize));
        assert!(RelayCell::new(RelayCmd::Data, vec![0; max_data(1)]).encode(relay_cell_len(1)).is_ok());
    }
}
