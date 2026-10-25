//! Lado do cliente: monta o circuito salto a salto e envia/recebe dados.
//!
//! Uso típico (cada método devolve a célula a enviar ao **primeiro** relay):
//! 1. [`ClientCircuit::create`] com a chave pública do relay A;
//! 2. ao receber [`Event::HopReady`]`(0)`, [`ClientCircuit::extend`] para o relay B;
//! 3. ao receber [`Event::HopReady`]`(1)`, [`ClientCircuit::begin`] com o destino;
//! 4. [`ClientCircuit::send_data`] / [`ClientCircuit::ping`] à vontade.

use super::{
    CELL_LEN, Cell, CircuitError, Cmd, MAX_HOPS, RelayCell, RelayCmd,
    crypto::{ClientHandshake, LayerReceiver, LayerSender},
    link_len, max_data, pad_to_cell, relay_cell_len,
};

struct Hop {
    send: LayerSender,
    recv: LayerReceiver,
}

enum Pending {
    Create(ClientHandshake),
    Extend(ClientHandshake),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// O salto de índice dado terminou o handshake.
    HopReady(usize),
    /// Dados vindos do destino.
    Data(Vec<u8>),
    /// Resposta a um [`ClientCircuit::ping`], com o salto que respondeu.
    Pong { hop: usize, nonce: u64 },
    /// Preenchimento vindo do salto de saída (sinal de vida, sem conteúdo).
    Padding,
    /// O destino encerrou o fluxo.
    Ended,
    /// Um relay derrubou o circuito: monte outro.
    Destroyed,
}

pub struct ClientCircuit {
    circ: u32,
    hops: Vec<Hop>,
    pending: Option<Pending>,
}

impl ClientCircuit {
    /// Começa o circuito com o relay A. Devolve a célula `Create` para ele.
    pub fn create(circ: u32, relay_public: &[u8; 32]) -> (Self, Cell) {
        let hs = ClientHandshake::new(relay_public);
        let mut body = [0u8; 33];
        body[..32].copy_from_slice(&hs.public());
        let cell = Cell::with_body(circ, Cmd::Create, &body); // posição 0
        (Self { circ, hops: Vec::new(), pending: Some(Pending::Create(hs)) }, cell)
    }

    pub fn circ_id(&self) -> u32 {
        self.circ
    }

    /// Saltos prontos (0 a [`MAX_HOPS`]).
    pub fn hops(&self) -> usize {
        self.hops.len()
    }

    /// Pede ao último salto que se conecte a `addr` e vire o próximo salto.
    /// `relay_public` é a chave do circuito do próximo relay; `link_public`, a chave com que
    /// o salto atual autentica o link até ele (Noise NK).
    pub fn extend(&mut self, addr: &str, relay_public: &[u8; 32], link_public: &[u8; 32]) -> Result<Cell, CircuitError> {
        if self.hops.is_empty() || self.pending.is_some() {
            return Err(CircuitError::Protocol("extend fora de hora"));
        }
        if self.hops.len() >= MAX_HOPS {
            return Err(CircuitError::Protocol("circuito já está no tamanho máximo"));
        }
        let hs = ClientHandshake::new(relay_public);
        let mut data = hs.public().to_vec();
        data.extend_from_slice(link_public);
        data.extend_from_slice(addr.as_bytes());
        let cell = self.build(self.hops.len() - 1, RelayCell::new(RelayCmd::Extend, data))?;
        self.pending = Some(Pending::Extend(hs));
        Ok(cell)
    }

    /// Pede à saída que abra o fluxo para `dest`.
    pub fn begin(&mut self, dest: &str) -> Result<Cell, CircuitError> {
        self.send_last(RelayCmd::Begin, dest.as_bytes().to_vec())
    }

    /// Envia dados à saída. Máximo: [`max_data`]`(1)` bytes com 2 saltos.
    pub fn send_data(&mut self, payload: &[u8]) -> Result<Cell, CircuitError> {
        self.send_last(RelayCmd::Data, payload.to_vec())
    }

    /// Célula de preenchimento para o salto de saída (cover de ponta a ponta).
    pub fn cover(&mut self) -> Result<Cell, CircuitError> {
        self.send_last(RelayCmd::Drop, Vec::new())
    }

    pub fn end(&mut self) -> Result<Cell, CircuitError> {
        self.send_last(RelayCmd::End, Vec::new())
    }

    /// Ping ao salto `hop`; a resposta chega como [`Event::Pong`]. Mede a
    /// latência até aquele salto e serve de sinal de vida do circuito.
    pub fn ping(&mut self, hop: usize, nonce: u64) -> Result<Cell, CircuitError> {
        if hop >= self.hops.len() {
            return Err(CircuitError::Protocol("salto inexistente"));
        }
        self.build(hop, RelayCell::new(RelayCmd::Ping, nonce.to_be_bytes().to_vec()))
    }

    fn send_last(&mut self, cmd: RelayCmd, data: Vec<u8>) -> Result<Cell, CircuitError> {
        let last = self.hops.len().checked_sub(1).ok_or(CircuitError::Protocol("circuito vazio"))?;
        self.build(last, RelayCell::new(cmd, data))
    }

    /// Cifra para o salto `target` e embrulha nas camadas dos anteriores.
    fn build(&mut self, target: usize, rc: RelayCell) -> Result<Cell, CircuitError> {
        if rc.data.len() > max_data(target) {
            return Err(CircuitError::Oversize);
        }
        let mut pt = vec![1u8];
        pt.extend_from_slice(&rc.encode(relay_cell_len(target))?);
        let mut sealed = self.hops[target].send.seal(&pt)?;
        for j in (0..target).rev() {
            let mut pt = vec![0u8];
            pt.extend_from_slice(&sealed);
            sealed = self.hops[j].send.seal(&pt)?;
        }
        debug_assert_eq!(sealed.len(), CELL_LEN);
        Ok(Cell { circ: self.circ, cmd: Cmd::Relay, body: pad_to_cell(&sealed) })
    }

    /// Processa uma célula recebida do primeiro relay.
    pub fn handle_cell(&mut self, cell: Cell) -> Result<Event, CircuitError> {
        if cell.circ != self.circ {
            return Err(CircuitError::UnknownCircuit);
        }
        match cell.cmd {
            Cmd::Destroy => Ok(Event::Destroyed),
            Cmd::Created => match self.pending.take() {
                Some(Pending::Create(hs)) if self.hops.is_empty() => {
                    self.add_hop(hs, &cell.body[..32], &cell.body[32..64])?;
                    Ok(Event::HopReady(0))
                }
                other => {
                    self.pending = other;
                    Err(CircuitError::Protocol("Created inesperado"))
                }
            },
            Cmd::Relay => self.peel(&cell.body),
            Cmd::Create => Err(CircuitError::Protocol("Create inesperado")),
            Cmd::Padding => Err(CircuitError::Protocol("padding fora do link")),
        }
    }

    fn add_hop(&mut self, hs: ClientHandshake, y: &[u8], tag: &[u8]) -> Result<(), CircuitError> {
        let keys = hs.finish(y.try_into().unwrap(), tag.try_into().unwrap())?;
        let (send, recv) = keys.client_layers();
        self.hops.push(Hop { send, recv });
        Ok(())
    }

    /// Retira camadas até achar o salto que originou a célula.
    fn peel(&mut self, body: &[u8; CELL_LEN]) -> Result<Event, CircuitError> {
        let mut data = body[..link_len(0)].to_vec();
        for j in 0..self.hops.len() {
            let pt = self.hops[j].recv.open(&data)?;
            match pt[0] {
                0 => data = pt[1..].to_vec(),
                1 => return self.on_relay_cell(j, RelayCell::decode(&pt[1..])?),
                _ => return Err(CircuitError::Protocol("flag de camada inválida")),
            }
        }
        Err(CircuitError::Protocol("nenhum salto reconheceu a célula"))
    }

    fn on_relay_cell(&mut self, hop: usize, rc: RelayCell) -> Result<Event, CircuitError> {
        match rc.cmd {
            RelayCmd::Extended => {
                let Some(Pending::Extend(hs)) = self.pending.take() else {
                    return Err(CircuitError::Protocol("Extended inesperado"));
                };
                if hop + 1 != self.hops.len() || rc.data.len() != 64 {
                    return Err(CircuitError::Protocol("Extended malformado"));
                }
                self.add_hop(hs, &rc.data[..32], &rc.data[32..])?;
                Ok(Event::HopReady(self.hops.len() - 1))
            }
            RelayCmd::Data => Ok(Event::Data(rc.data)),
            RelayCmd::Pong => {
                let nonce = u64::from_be_bytes(
                    rc.data.as_slice().try_into().map_err(|_| CircuitError::Protocol("Pong malformado"))?,
                );
                Ok(Event::Pong { hop, nonce })
            }
            RelayCmd::End => Ok(Event::Ended),
            RelayCmd::Drop => Ok(Event::Padding),
            _ => Err(CircuitError::Protocol("comando inesperado do relay")),
        }
    }
}
