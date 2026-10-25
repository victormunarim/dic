//! Motor: nó, chamadas e salas. Não conhece a janela; só recebe comandos e escreve no log.
//!
//! `sound` (áudio e controles), `screen` (tela compartilhada), `call` (conversa 1:1), `rooms` (salas), `node` (laço do nó que recebe os comandos).

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use clam::media::codec::G722;
use clam::node::{Node, NodeConfig};
use clam::phone::ActiveCall;
use clam::room::{self, Mesh, RoomView};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};

use clam::x25519_dalek::StaticSecret;

use crate::conf::*;

mod call;
mod node;
mod rooms;
mod screen;
mod sound;
mod voice;

pub(crate) use call::*;
pub(crate) use node::run_node;
pub(crate) use rooms::*;
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
        let mut l = self.log.lock().unwrap();
        l.push(s.into());
        if l.len() > 500 {
            l.drain(..100);
        }
        drop(l);
        (self.repaint)();
    }
}

pub(crate) enum Cmd {
    /// Contato a ligar e, se o código dele veio com o relay (código direto), onde achá-lo sem diretório.
    Dial([u8; 32], Option<clam::directory::RelayInfo>),
    /// Contato novo (e o relay do código dele, se veio um código direto): atender e guardar o endereço.
    Contact([u8; 32], Option<clam::directory::RelayInfo>),
    /// Contato removido: parar de atender e esquecer o endereço guardado.
    Forget([u8; 32]),
    /// Segredo, dono da sala (se conhecido) e nome.
    Join([u8; 32], Option<[u8; 32]>, String),
    /// O dono tira um membro: troca o segredo e avisa só quem fica.
    Remove([u8; 32]),
    Leave,
    /// Liga o microfone ao alto-falante para a pessoa conferir som e nível.
    MicTest,
}

/// Estado compartilhado entre a janela e a tarefa do nó.
#[derive(Default)]
pub(crate) struct Shared {
    /// Há uma chamada em andamento (entrando ou saindo).
    pub(crate) in_call: AtomicBool,
    /// Pedido de desligar a chamada atual.
    pub(crate) hangup: AtomicBool,
    /// Qualidade da linha na chamada atual (vazio fora de chamada).
    pub(crate) link: Mutex<String>,
    /// Tela: o que a pessoa pediu, o que está rodando e o que chegou do outro lado.
    pub(crate) screen: ScreenState,
    /// Mudo, volume, ganho e nível do microfone.
    pub(crate) audio: Controls,
    /// Teste do microfone em andamento (a pessoa se ouve); desligar este sinalizador encerra o teste.
    pub(crate) mic_test: AtomicBool,
    /// Texto de estado do nó ("online em ...").
    pub(crate) status: Mutex<String>,
    /// Relay desta máquina como os outros o alcançam (vai no código de contato direto); vazio offline.
    pub(crate) relay: Mutex<Option<clam::directory::RelayInfo>>,
    /// Está numa sala (as chamadas da sala não usam `in_call`, mas ele fica ligado).
    pub(crate) in_room: AtomicBool,
    /// Resumo da sala atual.
    pub(crate) room: Mutex<String>,
    /// Sala atual: a malha (volume e quem fala) e os membros vivos, você incluído.
    pub(crate) mesh: Mutex<Option<Mesh>>,
    pub(crate) members: Mutex<Vec<[u8; 32]>>,
    pub(crate) me: Mutex<[u8; 32]>,
    /// Sou o dono da sala atual (posso remover membros).
    pub(crate) owner: AtomicBool,
    /// Segredos de sala trocados pelo dono, `(antigo, novo)`, para a janela atualizar o que está salvo.
    pub(crate) rekeyed: Mutex<Vec<([u8; 32], [u8; 32])>>,
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
        let l = s.log.lock().unwrap();
        assert!(l.len() <= 500);
        assert_eq!(l.last().unwrap(), "linha 649");
    }
}
