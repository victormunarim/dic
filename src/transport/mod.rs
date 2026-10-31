//! Transporte TCP dos relays e do cliente (feature `net`).
//!
//! Liga os motores *sans-IO* ([`RelayNode`], [`CircuitManager`]) a sockets TCP:
//!
//! - [`RelayServer`]: escuta conexões, repassa células entre links e abre as
//!   conexões que o relay pede (`Action::Connect`). O relay de saída (segundo
//!   salto) abre o fluxo até o destino **só** se a [`ExitPolicy`] permitir.
//! - [`CircuitClient`]: conecta aos primeiros relays sob demanda, dirige o
//!   gerente de circuitos (relógio, ping, failover) e expõe envio/eventos.
//!
//! # Formato no fio
//!
//! - **Links entre nós**: Noise `NK` (veja [`crate::link`]) com uma célula
//!   ([`Cell::WIRE_LEN`] = 261 bytes) por mensagem cifrada. O iniciador
//!   autentica o relay pela chave do descritor; quem observa a rede não vê ids
//!   de circuito nem comandos. Cada célula ainda é cifrada em camadas.
//! - **Fluxo do relay de saída ao destino**: quadros `[u16 BE tamanho][dados]`,
//!   para preservar as fronteiras dos pacotes de mídia sobre o TCP.
//!
//! # Limites
//!
//! - O link oculta conteúdo, ids e comandos, mas não o tamanho nem a
//!   temporização dos quadros por si só (todos têm o mesmo tamanho). O cover
//!   traffic (um quadro por intervalo, real ou `Padding`) cobre a temporização;
//!   veja [`DEFAULT_COVER`].
//! - O trecho entre o relay de saída e o destino é TCP puro: o conteúdo de
//!   mídia já vai cifrado ponta a ponta pela camada acima, não por este módulo.
//! - Fechar um link não derruba na hora os circuitos que passavam por ele; o
//!   relay os expira por ociosidade e o cliente os detecta por silêncio.
//! - Células inválidas fecham o link (violação de protocolo).
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior, interval, timeout},
};

use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::rand_core::OsRng;

use crate::link::{self, LinkReader, LinkWriter};
use crate::cover::CoverProfile;
use crate::reputation::{RelayStats, Reputation};
use crate::circuit::{
    Cell, Cmd,
    manager::{Candidate, CircuitManager, Event, Out, SendError, Status},
    relay::{Action, CircRef, LinkId, RelayNode},
};

/// Intervalo entre reaplicações da reputação à lista de candidatos.
pub const REPUTATION_REFRESH: Duration = Duration::from_secs(30);

/// Decide para onde o relay de saída pode abrir fluxos.
/// Devolve o endereço TCP real para o `dest` pedido, ou `None` para recusar.
/// A política padrão ([`deny_all`]) recusa tudo: sem ela o relay
/// seria um proxy aberto.
pub type ExitPolicy = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub fn deny_all() -> ExitPolicy {
    Arc::new(|_| None)
}

/// Permite somente os destinos da lista: `(nome pedido pelo cliente, endereço TCP)`.
pub fn allow_list(entries: Vec<(String, String)>) -> ExitPolicy {
    Arc::new(move |dest| entries.iter().find(|(n, _)| n == dest).map(|(_, a)| a.clone()))
}

#[derive(Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Conexões de entrada simultâneas. Padrão: 512.
    pub max_links: usize,
    /// Conexões de entrada simultâneas de uma mesma origem (um IPv4, ou um /64 de IPv6), para uma só não
    /// ocupar todas as vagas de `max_links`. Conexões desta máquina (loopback) não contam. Padrão: 32.
    pub max_links_per_ip: usize,
    /// Prazo para conectar ao próximo salto ou ao destino. Padrão: 3 s.
    pub connect_timeout: Duration,
    pub exit: ExitPolicy,
    /// Chave de link (Noise NK) do relay, a mesma que vai no descritor como `link_key`.
    /// `None` gera uma chave nova a cada início; para manter a identidade de link entre
    /// reinícios, guarde o segredo e passe-o aqui. É distinta da chave do circuito.
    pub link_secret: Option<StaticSecret>,
    /// Cover traffic: intervalo entre quadros nos links com padding. `None` desliga.
    /// O relay liga o padding em links que ele mesmo abre e responde com padding nos links
    /// de entrada cujo par o usa (veja [`DEFAULT_COVER`]). Padrão: `Some(DEFAULT_COVER)`.
    pub cover: Option<Duration>,
    /// Links com padding simultâneos (cada um custa banda constante nos dois sentidos).
    /// Sem teto, um par poderia forçar o relay a gastar banda em todos os links. Padrão: 64.
    pub max_cover_links: usize,
    /// Perfil de custo do cover; quando presente, substitui `cover` (usa o intervalo de link
    /// do perfil). Troca viva com [`RelayServer::set_profile`]. Padrão: `None`.
    pub profile: Option<CoverProfile>,
}

impl ServerConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self { listen, max_links: 512, max_links_per_ip: 32, connect_timeout: Duration::from_secs(3), exit: deny_all(), link_secret: None, cover: Some(DEFAULT_COVER), max_cover_links: 64, profile: None }
    }
}

/// Intervalo padrão do cover traffic: 100 quadros/s (279 bytes) por sentido em cada link.
pub const DEFAULT_COVER: Duration = Duration::from_millis(10);

/// Intervalo padrão do cover de ponta a ponta: 50 células/s por sentido (um quadro Opus de 20 ms).
pub const DEFAULT_CIRCUIT_COVER: Duration = Duration::from_millis(20);

const CHANNEL: usize = 1024;
const WRITE_QUEUE: usize = 256;
/// Células esperando acima das quais o escritor com cover manda mais algumas por intervalo.
const BACKLOG: usize = 4;
/// Células a mais por intervalo quando há fila acumulada. Com o cover de 10 ms dos relays, 1 + 32 por
/// intervalo escoam até 3300 células/s, mais que os 2000 pacotes/s da tela sem privacidade (com 5, o teto
/// era de 600 células/s e a fila do link enchia e descartava).
const MAX_EXTRA: usize = 32;

// ---------------------------------------------------------------------------
// Leitura/escrita de links
// ---------------------------------------------------------------------------

/// Lê células do link até erro (EOF, autenticação falha ou célula inválida).
/// Células `Padding` não chegam a `on_cell`: só avisam que o par usa cover traffic.
async fn read_cells<F, Fut>(mut rd: LinkReader, peer_cover: Arc<AtomicBool>, mut on_cell: F)
where
    F: FnMut(Cell) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    while let Ok(cell) = rd.read_cell().await {
        if cell.cmd == Cmd::Padding {
            peer_cover.store(true, Ordering::Relaxed);
            continue;
        }
        if !on_cell(cell).await {
            return;
        }
    }
}

/// Política de cover traffic de um link.
#[derive(Clone)]
struct Cover {
    /// Intervalo entre quadros; `None` desliga. Mudar o valor vale para o link já aberto.
    interval: watch::Receiver<Option<Duration>>,
    /// Liga o padding por conta própria (links que nós abrimos).
    initiate: bool,
    /// O par já mandou padding: responde na mesma moeda.
    peer: Arc<AtomicBool>,
    /// Teto de links com padding ativo (só nos relays).
    slots: Option<Arc<Semaphore>>,
}

impl Cover {
    fn off() -> Self {
        Self::fixed(None)
    }

    /// Intervalo que nunca muda (relays).
    fn fixed(interval: Option<Duration>) -> Self {
        Self { interval: watch::channel(interval).1, initiate: false, peer: Arc::new(AtomicBool::new(false)), slots: None }
    }
}

/// Espera uma mudança de intervalo; sem emissor, nunca acorda.
async fn interval_changed(rx: &mut watch::Receiver<Option<Duration>>) -> Option<Duration> {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
    *rx.borrow_and_update()
}

fn new_ticker(every: Duration) -> tokio::time::Interval {
    let mut t = interval(every);
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    t
}

/// Escreve as células da fila. Com cover ativo, emite exatamente um quadro por
/// `interval`: a célula seguinte da fila ou, se não houver, um `Padding`. Assim a taxa e a
/// temporização no fio não dependem de haver voz, silêncio ou controle.
async fn write_loop(mut wr: LinkWriter, mut rx: mpsc::Receiver<Cell>, mut cover: Cover) {
    let mut every = *cover.interval.borrow_and_update();
    let mut ticker = new_ticker(every.unwrap_or(Duration::from_secs(1)));
    // A vaga do teto fica retida enquanto o escritor existir.
    let mut _slot: Option<OwnedSemaphorePermit> = None;
    let mut active = false;
    loop {
        if every.is_none() {
            _slot = None;
            active = false;
            // Sem cover: repassa como chegar.
            tokio::select! {
                c = rx.recv() => match c {
                    Some(cell) => if wr.write_cell(&cell).await.is_err() { return; },
                    None => return,
                },
                new = interval_changed(&mut cover.interval) => {
                    every = new;
                    if let Some(d) = new { ticker = new_ticker(d); }
                }
            }
            continue;
        }
        if !active && (cover.initiate || cover.peer.load(Ordering::Relaxed)) {
            match &cover.slots {
                None => active = true,
                Some(s) => {
                    _slot = s.clone().try_acquire_owned().ok();
                    active = _slot.is_some();
                }
            }
        }
        if !active {
            // Sem cover ainda: repassa como chegar e reavalia a cada intervalo.
            tokio::select! {
                c = rx.recv() => match c {
                    Some(cell) => if wr.write_cell(&cell).await.is_err() { return; },
                    None => return,
                },
                _ = ticker.tick() => {}
                new = interval_changed(&mut cover.interval) => {
                    every = new;
                    if let Some(d) = new { ticker = new_ticker(d); }
                }
            }
            continue;
        }
        tokio::select! {
            _ = ticker.tick() => {}
            new = interval_changed(&mut cover.interval) => {
                every = new;
                if let Some(d) = new { ticker = new_ticker(d); }
                continue;
            }
        }
        let cell = match rx.try_recv() {
            Ok(c) => c,
            Err(mpsc::error::TryRecvError::Empty) => Cell::with_body(0, Cmd::Padding, &[]),
            Err(mpsc::error::TryRecvError::Disconnected) => return,
        };
        if wr.write_cell(&cell).await.is_err() {
            return;
        }
        // Há fila acumulada (um circuito mais rápido que o cover do link, como o da tela a 2 ms):
        // escoa mais algumas, senão a fila enche e o excesso é descartado.
        if rx.len() > BACKLOG {
            for _ in 0..MAX_EXTRA {
                let Ok(extra) = rx.try_recv() else { break };
                if wr.write_cell(&extra).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Disca `addr` e cifra o link, autenticando o relay por `key`.
///
/// Com `public_only`, só conecta a endereço público (resolvido aqui, e é a esse que conecta): um estranho que
/// pede um `Extend` não usa o relay para varrer a rede local, nem as portas desta máquina.
async fn dial(addr: String, key: [u8; 32], limit: Duration, public_only: bool) -> Option<(LinkReader, LinkWriter)> {
    // Rede privada e loopback só para o link local (LAN, testes); link-local (serviço de metadados de nuvem),
    // não especificado e multicast, para ninguém.
    let usable = |ip: &IpAddr| match ip.to_canonical() {
        IpAddr::V4(a) => !(a.is_unspecified() || a.is_link_local() || a.is_multicast() || a.is_broadcast()),
        IpAddr::V6(a) => !(a.is_unspecified() || a.is_multicast() || (a.segments()[0] & 0xffc0) == 0xfe80),
    };
    let targets = timeout(limit, tokio::net::lookup_host(addr)).await.ok()?.ok()?;
    let target = targets.into_iter().find(|a| if public_only { is_public(&a.ip()) } else { usable(&a.ip()) })?;
    let s = timeout(limit, TcpStream::connect(target)).await.ok()?.ok()?;
    // Sem isto o Nagle seguraria quadros pequenos de voz esperando o ACK do anterior.
    let _ = s.set_nodelay(true);
    link::connect(s, &key).await.ok()
}

/// `ip` é um endereço da internet pública (nem local, nem privado, nem reservado)?
pub fn is_public(ip: &IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(a) => {
            let o = a.octets();
            !(a.is_unspecified()
                || a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18))
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            !(a.is_unspecified()
                || a.is_loopback()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x0064 && s[1] == 0xff9b))
        }
    }
}

mod client;
mod server;

pub use client::{CircuitClient, ClientOptions};
pub use server::RelayServer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_global_addresses_are_public() {
        for a in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700::1111", "100.63.255.255", "172.32.0.1"] {
            assert!(is_public(&a.parse().unwrap()), "{a}");
        }
        for a in [
            "127.0.0.1", "0.0.0.0", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254", "100.64.0.1",
            "192.0.0.8", "198.18.0.1", "224.0.0.1", "255.255.255.255", "240.0.0.1", "203.0.113.5",
            "::1", "::", "fe80::1", "fc00::1", "fd12::1", "ff02::1", "2001:db8::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(&a.parse().unwrap()), "{a}");
        }
    }

    #[tokio::test]
    async fn a_stranger_cannot_make_the_relay_dial_this_machine() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accepted = tokio::spawn(async move { listener.accept().await.is_ok() });
        assert!(dial(addr.clone(), [0; 32], Duration::from_millis(500), true).await.is_none());
        assert!(dial(format!("localhost:{}", addr.rsplit(':').next().unwrap()), [0; 32], Duration::from_millis(500), true).await.is_none());
        // O pedido do dono do relay não tem essa restrição (o handshake falha só porque a chave é falsa).
        let _ = dial(addr, [0; 32], Duration::from_millis(500), false).await;
        assert!(accepted.await.unwrap(), "o dono chegou a conectar; o estranho não");
    }
}
