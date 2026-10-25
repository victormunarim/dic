//! Furo de NAT por UDP (feature `net`): chamar quem não tem porta aberta.
//!
//! Atrás de um NAT (ou CGNAT) ninguém consegue abrir conexão para você, mas o roteador deixa passar o que
//! parece resposta a algo que você enviou. Então as duas pontas enviam pacotes uma à outra *ao mesmo tempo*,
//! cada uma abrindo seu lado do roteador:
//!
//! 1. Cada nó usa **um** socket UDP. Ele fala com o diretório (que responde de onde o pacote veio, como um
//!    STUN) e repete isso a cada [`KEEPALIVE`] para manter o mapeamento do roteador aberto.
//! 2. Quem aceita chamadas mantém uma conexão de controle com o diretório. Quem chama pede o furo: o diretório
//!    avisa o ouvinte do endereço UDP de quem chama e devolve a quem chama o do ouvinte.
//! 3. Os dois sondam o endereço do outro até um responder; daí em diante os datagramas passam direto. O convite
//!    e a voz (já cifrados de ponta a ponta) seguem por esse caminho, sem circuito e sem o servidor.
//!
//! Não funciona quando os dois lados estão atrás de NAT **simétrico** (o roteador escolhe uma porta externa
//! diferente para cada destino): aí o endereço que o diretório viu não é o que o outro lado alcançaria.
//!
//! Datagramas entre as pontas: `0` sonda, `3` resposta à sonda, `1 ‖ dados` pacote da chamada.

use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    net::UdpSocket,
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use x25519_dalek::StaticSecret;

use crate::{directory, phone::DatagramPort};

const PROBE: u8 = 0;
const DATA: u8 = 1;
const PROBE_ACK: u8 = 3;
/// Como o diretório: `1 ‖ ficha` pede para ele anotar o endereço; ele responde `2 ‖ ficha ‖ endereço`.
const TOKEN: u8 = 1;
const TOKEN_ACK: u8 = 2;

/// Intervalo entre repetições do endereço ao diretório (mantém o mapeamento do NAT, em geral 30 s ou mais).
pub const KEEPALIVE: Duration = Duration::from_secs(15);
const PROBE_EVERY: Duration = Duration::from_millis(150);
/// Quanto tempo um endereço avisado pelo diretório vale para receber pacotes.
const EXPECT_TTL: Duration = Duration::from_secs(30);
const MAX_ROUTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum PunchError {
    #[error("diretório: {0}")]
    Directory(#[from] io::Error),
    #[error("o UDP não chega ao diretório (firewall ou rede bloqueia UDP)")]
    NoUdp,
    #[error("essa chave não aceita chamadas por furo de NAT agora")]
    NotListening,
    #[error("o furo de NAT não abriu (NAT simétrico dos dois lados, ou o outro lado não respondeu)")]
    Timeout,
}

/// Canal de pacotes com um par depois do furo: o que entra em `tx` sai como datagrama; `rx` traz os que chegam.
pub struct Chan {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub rx: mpsc::Receiver<Vec<u8>>,
}

struct Shared {
    sock: UdpSocket,
    server_udp: SocketAddr,
    port: DatagramPort,
    routes: Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>,
    expected: Mutex<HashMap<SocketAddr, Instant>>,
    opened: Mutex<HashSet<SocketAddr>>,
    acked: Mutex<HashSet<[u8; 16]>>,
    bell: Notify,
}

impl Shared {
    fn is_expected(&self, a: &SocketAddr) -> bool {
        self.expected.lock().unwrap().get(a).is_some_and(|t| t.elapsed() < EXPECT_TTL)
    }

    fn expect(&self, a: SocketAddr) {
        let mut e = self.expected.lock().unwrap();
        e.retain(|_, t| t.elapsed() < EXPECT_TTL);
        e.insert(a, Instant::now());
        self.opened.lock().unwrap().remove(&a);
    }

    /// Cria a rota de um par: datagramas que chegam dele vão para o `Receiver`; o que entra no `Sender` sai para ele.
    fn route(self: &Arc<Self>, peer: SocketAddr) -> Chan {
        let (in_tx, in_rx) = mpsc::channel(256);
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
        {
            let mut r = self.routes.lock().unwrap();
            r.retain(|_, t| !t.is_closed());
            if r.len() < MAX_ROUTES {
                r.insert(peer, in_tx);
            }
        }
        let sh = self.clone();
        tokio::spawn(async move {
            while let Some(d) = out_rx.recv().await {
                let _ = sh.sock.send_to(&[&[DATA][..], &d].concat(), peer).await;
            }
        });
        Chan { tx: out_tx, rx: in_rx }
    }

    async fn on_data(self: &Arc<Self>, src: SocketAddr, data: &[u8]) {
        let existing = self.routes.lock().unwrap().get(&src).cloned();
        if let Some(t) = existing {
            if t.try_send(data.to_vec()).is_err() && t.is_closed() {
                self.routes.lock().unwrap().remove(&src);
            }
            return;
        }
        // Par novo: só se o diretório avisou deste endereço (quem chama).
        if !self.is_expected(&src) {
            return;
        }
        let Chan { tx, mut rx } = self.route(src);
        let sh = self.clone();
        let first = data.to_vec();
        tokio::spawn(async move {
            let (w_tx, mut w_rx) = mpsc::channel::<Vec<u8>>(256);
            let Some(conn) = sh.port.open(w_tx).await else { return };
            // O que o telefone responde sai para o par.
            let out = tokio::spawn(async move {
                while let Some(d) = w_rx.recv().await {
                    if tx.send(d).await.is_err() {
                        break;
                    }
                }
            });
            if conn.deliver(first).await {
                while let Some(d) = rx.recv().await {
                    if !conn.deliver(d).await {
                        break;
                    }
                }
            }
            out.abort();
            conn.close().await;
        });
    }
}

/// Socket UDP do nó, com o furo de NAT para receber e fazer chamadas diretas.
pub struct Punch {
    shared: Arc<Shared>,
    server: String,
    tasks: Vec<JoinHandle<()>>,
}

impl Punch {
    /// Anuncia o UDP ao diretório e passa a aceitar chamadores que ele avisar. Erra se o UDP não chegar ao diretório.
    pub async fn start(server: &str, secret: &StaticSecret, port: DatagramPort) -> Result<Self, PunchError> {
        let server_udp = tokio::net::lookup_host(server).await?.next().ok_or(PunchError::NoUdp)?;
        let bind: SocketAddr = if server_udp.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
        let sock = UdpSocket::bind(bind).await?;
        let shared = Arc::new(Shared {
            sock,
            server_udp,
            port,
            routes: Mutex::default(),
            expected: Mutex::default(),
            opened: Mutex::default(),
            acked: Mutex::default(),
            bell: Notify::new(),
        });
        let mut tasks = vec![tokio::spawn(demux(shared.clone()))];
        let mut listener = directory::punch_register(server, secret).await?;
        if !announce(&shared, &listener.token).await {
            for t in &tasks {
                t.abort();
            }
            return Err(PunchError::NoUdp);
        }
        let token = Arc::new(Mutex::new(listener.token));
        {
            let (sh, token) = (shared.clone(), token.clone());
            tasks.push(tokio::spawn(async move {
                loop {
                    sleep(KEEPALIVE).await;
                    let t = *token.lock().unwrap();
                    let _ = sh.sock.send_to(&[&[TOKEN][..], &t].concat(), sh.server_udp).await;
                }
            }));
        }
        {
            let (sh, server, secret) = (shared.clone(), server.to_string(), secret.clone());
            tasks.push(tokio::spawn(async move {
                loop {
                    while let Ok(caller) = listener.next().await {
                        sh.expect(caller);
                        tokio::spawn(probe_until_open(sh.clone(), caller, Duration::from_secs(8)));
                    }
                    // Conexão de controle caiu: registra de novo (o diretório pode ter reiniciado).
                    loop {
                        sleep(Duration::from_secs(5)).await;
                        if let Ok(l) = directory::punch_register(&server, &secret).await {
                            *token.lock().unwrap() = l.token;
                            let _ = sh.sock.send_to(&[&[TOKEN][..], &l.token].concat(), sh.server_udp).await;
                            listener = l;
                            break;
                        }
                    }
                }
            }));
        }
        Ok(Self { shared, server: server.to_string(), tasks })
    }

    /// Abre um caminho UDP direto até `peer`. Erra rápido se `peer` não aceita furos.
    pub async fn dial(&self, peer: &[u8; 32], wait: Duration) -> Result<Chan, PunchError> {
        let sh = &self.shared;
        let token = directory::punch_prep(&self.server).await?;
        if !announce(sh, &token).await {
            return Err(PunchError::NoUdp);
        }
        let target = directory::punch_go(&self.server, &token, peer).await?.ok_or(PunchError::NotListening)?;
        sh.expect(target);
        let chan = sh.route(target);
        if probe_until_open(sh.clone(), target, wait).await {
            Ok(chan)
        } else {
            sh.routes.lock().unwrap().remove(&target);
            Err(PunchError::Timeout)
        }
    }
}

impl Drop for Punch {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Faz o diretório anotar o endereço externo desta ficha; espera a confirmação.
async fn announce(sh: &Arc<Shared>, token: &[u8; 16]) -> bool {
    for _ in 0..4 {
        let _ = sh.sock.send_to(&[&[TOKEN][..], token].concat(), sh.server_udp).await;
        let _ = timeout(Duration::from_millis(600), async {
            while !sh.acked.lock().unwrap().contains(token) {
                sh.bell.notified().await;
            }
        })
        .await;
        if sh.acked.lock().unwrap().contains(token) {
            return true;
        }
    }
    false
}

/// Sonda `peer` até ele responder (ou `wait` passar). `true` se o caminho abriu.
async fn probe_until_open(sh: Arc<Shared>, peer: SocketAddr, wait: Duration) -> bool {
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if sh.opened.lock().unwrap().contains(&peer) {
            return true;
        }
        let _ = sh.sock.send_to(&[PROBE], peer).await;
        let _ = timeout(PROBE_EVERY, sh.bell.notified()).await;
    }
    sh.opened.lock().unwrap().contains(&peer)
}

async fn demux(sh: Arc<Shared>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, src)) = sh.sock.recv_from(&mut buf).await else { continue };
        if n == 0 {
            continue;
        }
        if src == sh.server_udp {
            if buf[0] == TOKEN_ACK && n >= 17 {
                sh.acked.lock().unwrap().insert(buf[1..17].try_into().unwrap());
                sh.bell.notify_waiters();
            }
            continue;
        }
        match buf[0] {
            PROBE if sh.is_expected(&src) => {
                let _ = sh.sock.send_to(&[PROBE_ACK], src).await;
                sh.opened.lock().unwrap().insert(src);
                sh.bell.notify_waiters();
            }
            PROBE_ACK if sh.is_expected(&src) => {
                sh.opened.lock().unwrap().insert(src);
                sh.bell.notify_waiters();
            }
            DATA => sh.on_data(src, &buf[1..n]).await,
            _ => {}
        }
    }
}
