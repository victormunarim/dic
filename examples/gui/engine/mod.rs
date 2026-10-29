//! Motor: nó e chamadas. Não conhece a janela; só recebe comandos e escreve no log.
//!
//! `sound` (áudio e controles), `screen` (tela compartilhada), `call` (conversa 1:1), `node` (laço do nó que recebe os comandos).

use clam::sync::LockExt;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use clam::media::codec::CodecId;
use clam::node::{Node, NodeConfig};
use clam::phone::ActiveCall;
use tokio::{
    sync::{mpsc, oneshot},
    time::{MissedTickBehavior, interval},
};

use crate::conf::*;

mod call;
mod node;
mod room;
mod screen;
mod sound;
mod voice;

pub(crate) use call::*;
pub(crate) use node::{member_name, run_node};
pub(crate) use room::*;
pub(crate) use screen::*;
pub(crate) use sound::*;
pub(crate) use voice::*;

pub(crate) type Log = Arc<Mutex<Vec<String>>>;

/// Escreve uma linha no log da janela e pede repintura.
#[derive(Clone)]
pub(crate) struct Sink {
    pub(crate) log: Log,
    /// Pede repintura da janela (vazio nos testes).
    pub(crate) repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Sink {
    pub(crate) fn line(&self, s: impl Into<String>) {
        let mut l = self.log.lock_unpoisoned();
        l.push(s.into());
        if l.len() > 500 {
            l.drain(..100);
        }
        drop(l);
        (self.repaint)();
    }
}

pub(crate) enum Cmd {
    /// Contato a ligar e, se o código dele veio com o relay (código direto), onde achá-lo.
    Dial([u8; 32], Option<clam::node::RelayInfo>),
    /// Contato novo (e o relay do código dele, se veio um código direto): atender e guardar o endereço.
    Contact([u8; 32], Option<clam::node::RelayInfo>),
    /// Contato removido pela pessoa: esquece e avisa o outro lado para ele também tirar (`Node::retire`).
    Retire([u8; 32]),
    /// Marca (ou desmarca) o contato como de confiança: a tela sem privacidade vai direto só para ele.
    Trust([u8; 32], bool),
    /// Liga o microfone ao alto-falante para a pessoa conferir som e nível.
    MicTest,
    /// Entra na sala de voz do servidor.
    JoinRoom(clam::server::ServerId, clam::server::RoomId),
    /// Sai da sala de voz.
    LeaveRoom,
    /// Transmitir (ou parar de transmitir) a tela para quem pedir, na sala.
    RoomShare(bool),
}

/// Estado compartilhado entre a janela e a tarefa do nó.
#[derive(Default)]
pub(crate) struct Shared {
    /// Há uma chamada em andamento (entrando ou saindo).
    pub(crate) in_call: AtomicBool,
    /// Pedido de desligar a chamada atual.
    pub(crate) hangup: AtomicBool,
    /// Com quem é a chamada 1:1 em andamento (vazio fora de chamada).
    pub(crate) peer: Mutex<Option<[u8; 32]>>,
    /// Qualidade da linha na chamada atual (vazio fora de chamada).
    pub(crate) link: Mutex<String>,
    /// Tela: o que a pessoa pediu, o que está rodando e o que chegou do outro lado.
    pub(crate) screen: ScreenState,
    /// Mudo, volume, ganho e nível do microfone.
    pub(crate) audio: Controls,
    /// Codec das próximas ligações que esta pessoa fizer: Opus em vez de G.722 (`Form::voice_opus`).
    pub(crate) opus: AtomicBool,
    /// Teste do microfone em andamento (a pessoa se ouve); desligar este sinalizador encerra o teste.
    pub(crate) mic_test: AtomicBool,
    /// Texto de estado do nó ("online em ...").
    pub(crate) status: Mutex<String>,
    /// Relay desta máquina como os outros o alcançam (vai no código de contato direto); vazio offline.
    pub(crate) relay: Mutex<Option<clam::node::RelayInfo>>,
    /// Quem adicionou esta pessoa e se apresentou; a janela põe cada um na lista de contatos.
    pub(crate) introduced: Mutex<Vec<clam::node::Introduced>>,
    /// Quem removeu esta pessoa; o nó já o esqueceu e a janela o tira da lista de contatos.
    pub(crate) removed: Mutex<Vec<[u8; 32]>>,
    /// Os servidores desta pessoa; vazio offline.
    pub(crate) servers: Mutex<Option<clam::server::net::ServerHub>>,
    /// Sala de voz em que está e quem fala.
    pub(crate) room: Mutex<RoomLive>,
}

pub(crate) struct Running {
    pub(crate) cmds: mpsc::UnboundedSender<Cmd>,
    pub(crate) stop: Arc<AtomicBool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> Sink {
        Sink { log: Arc::default(), repaint: Arc::new(|| {}) }
    }

    #[test]
    fn log_keeps_the_latest_lines() {
        let s = sink();
        for i in 0..650 {
            s.line(format!("linha {i}"));
        }
        let l = s.log.lock_unpoisoned();
        assert!(l.len() <= 500);
        assert_eq!(l.last().unwrap(), "linha 649");
    }
}
