//! Diretório de primeiro contato (feature `net`).
//!
//! Só serve para duas pessoas se acharem na primeira vez: cada nó publica "esta chave pública
//! atende neste relay" e quem quer ligar consulta. A chamada em si não passa pelo diretório,
//! vai pelos relays, de ponta a ponta.
//!
//! - **Publicar** exige provar a posse da chave secreta, sem assinaturas: o servidor manda uma
//!   chave efêmera `E` a cada conexão e o cliente devolve `HMAC(DH(secreta, E), pedido)`. O servidor
//!   calcula o mesmo DH com `DH(e, pública)`. Quem não tem a secreta não consegue sobrescrever a entrada.
//! - **Consultar** é aberto. O diretório vê quem publica e quem consulta (IP e chave pública);
//!   não vê o conteúdo das chamadas. É o custo de ter primeiro contato por servidor.
//! - As entradas expiram em [`TTL`]; o nó republica a cada [`REPUBLISH`].
//!
//! - **Hospedar** (`REQ_HOST`) é para quem está atrás de NAT e não recebe conexões. O servidor, se
//!   configurado com um relay ([`HostConfig`]), publica a entrada apontando para o relay *dele* e mantém
//!   uma conexão de controle aberta com o nó. Quando o relay do servidor, como último salto, recebe um
//!   circuito para esse nó, o servidor avisa pelo controle; o nó abre uma conexão **de saída** de volta
//!   (`REQ_ATTACH` + ficha de 16 bytes) e o servidor emenda as duas. Nenhuma porta precisa ser aberta.
//!   O servidor passa a ver o IP do nó hospedado e o tráfego (já cifrado de ponta a ponta) dele.
//!
//! Formato: quadros `[u16 BE tamanho][dados]`. O servidor abre com `E (32) ‖ IP visto (texto)`.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{interval, sleep, timeout},
};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::circuit::{
    manager::Candidate,
    relay::{RelayConfig, RelayNode},
};
use crate::transport::{RelayServer, ServerConfig};

/// Validade de uma entrada sem republicar.
pub const TTL: Duration = Duration::from_secs(120);
/// Intervalo em que o nó republica.
pub const REPUBLISH: Duration = Duration::from_secs(40);
/// Entradas simultâneas no servidor.
pub const MAX_ENTRIES: usize = 4096;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = 512;
const MAX_REQUESTS: usize = 16;
const CONTEXT: &[u8] = b"clam-dir-v1";
/// Nós hospedados simultâneos no servidor.
pub const MAX_HOSTED: usize = 1024;
/// Quanto tempo uma conexão do relay espera o nó hospedado se anexar.
const PENDING_TTL: Duration = Duration::from_secs(10);
const MAX_PENDING: usize = 256;
/// Sem nada do servidor por tanto tempo (ele manda um ping a cada [`REPUBLISH`]), o nó reconecta.
const CONTROL_IDLE: Duration = Duration::from_secs(100);
const RECONNECT: Duration = Duration::from_secs(5);

/// Nome do destino que o relay de saída mapeia para o telefone de `key`.
pub fn dest_for(key: &[u8; 32]) -> String {
    let hex: String = key[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}:9")
}

const REQ_PUBLISH: u8 = 1;
const REQ_LOOKUP: u8 = 2;
const REQ_SAMPLE: u8 = 3;
const REQ_HOST: u8 = 4;
const REQ_ATTACH: u8 = 5;
const CTL_OPEN: u8 = 1;
const CTL_PING: u8 = 2;
const RES_OK: u8 = 0x80;
const RES_ERR: u8 = 0x81;
const RES_FOUND: u8 = 0x82;
const RES_NONE: u8 = 0x83;
const RES_LIST: u8 = 0x84;

/// Como alcançar um relay: endereço TCP e as duas chaves públicas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayInfo {
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

impl RelayInfo {
    pub fn candidate(&self) -> Candidate {
        Candidate { addr: self.addr.clone(), public: self.onion.into(), link_public: self.link, score: 1.0 }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.addr.len() as u8);
        out.extend_from_slice(self.addr.as_bytes());
        out.extend_from_slice(&self.onion);
        out.extend_from_slice(&self.link);
    }

    fn decode(b: &[u8]) -> Option<(Self, &[u8])> {
        let (&n, b) = b.split_first()?;
        let n = n as usize;
        if b.len() < n + 64 {
            return None;
        }
        let addr = String::from_utf8(b[..n].to_vec()).ok()?;
        let onion = b[n..n + 32].try_into().ok()?;
        let link = b[n + 32..n + 64].try_into().ok()?;
        Some((Self { addr, onion, link }, &b[n + 64..]))
    }
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    timeout(IO_TIMEOUT, r.read_exact(&mut len)).await.map_err(|_| io::ErrorKind::TimedOut)??;
    let n = u16::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "quadro grande demais"));
    }
    let mut buf = vec![0u8; n];
    timeout(IO_TIMEOUT, r.read_exact(&mut buf)).await.map_err(|_| io::ErrorKind::TimedOut)??;
    Ok(buf)
}

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) -> io::Result<()> {
    let mut f = (data.len() as u16).to_be_bytes().to_vec();
    f.extend_from_slice(data);
    timeout(IO_TIMEOUT, w.write_all(&f)).await.map_err(|_| io::ErrorKind::TimedOut)?
}

fn mac(key: &[u8; 32], hello_e: &[u8], body: &[u8]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC aceita qualquer tamanho de chave");
    m.update(CONTEXT);
    m.update(hello_e);
    m.update(body);
    m
}

struct Entry {
    info: RelayInfo,
    seen: Instant,
}

type Table = Arc<Mutex<HashMap<[u8; 32], Entry>>>;

/// Servidor de diretório em memória. Encerra ao ser descartado.
pub struct DirectoryServer {
    addr: SocketAddr,
    relay_addr: Option<SocketAddr>,
    task: JoinHandle<()>,
    _relay: Option<RelayServer>,
}

/// Relay do próprio servidor, para quem está atrás de NAT (veja `REQ_HOST`).
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Onde o relay escuta.
    pub relay_listen: SocketAddr,
    /// `ip:porta` que os outros usam para chegar ao relay. `None`: o IP de `relay_listen` e a porta efetiva
    /// (serve para testes locais; em produção informe o IP público).
    pub relay_public: Option<String>,
    /// Escreve em stderr cada nó hospedado, circuito recebido e conexão emendada (para diagnóstico).
    pub verbose: bool,
}

struct Host {
    info: RelayInfo,
    verbose: bool,
    routes: Arc<Mutex<HashMap<String, SocketAddr>>>,
    pending: Mutex<HashMap<[u8; 16], (TcpStream, Instant)>>,
}

impl DirectoryServer {
    pub async fn spawn(listen: SocketAddr) -> io::Result<Self> {
        Self::spawn_with(listen, None).await
    }

    /// Como [`spawn`](Self::spawn); com `host`, sobe também um relay para nós atrás de NAT.
    pub async fn spawn_with(listen: SocketAddr, host: Option<HostConfig>) -> io::Result<Self> {
        let listener = TcpListener::bind(listen).await?;
        let addr = listener.local_addr()?;
        let table: Table = Arc::default();
        let (host, relay) = match host {
            None => (None, None),
            Some(cfg) => {
                let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
                let onion = node.public_key();
                let routes: Arc<Mutex<HashMap<String, SocketAddr>>> = Arc::default();
                let mut scfg = ServerConfig::new(cfg.relay_listen);
                let r = routes.clone();
                scfg.exit = Arc::new(move |d: &str| r.lock().unwrap().get(d).map(|a| a.to_string()));
                let relay = RelayServer::spawn(node, scfg).await?;
                let public = cfg.relay_public.unwrap_or_else(|| format!("{}:{}", cfg.relay_listen.ip(), relay.local_addr().port()));
                let info = RelayInfo { addr: public, onion, link: relay.link_public_key() };
                (Some(Arc::new(Host { info, verbose: cfg.verbose, routes, pending: Mutex::default() })), Some(relay))
            }
        };
        let relay_addr = relay.as_ref().map(RelayServer::local_addr);
        let task = tokio::spawn(async move {
            loop {
                let Ok((s, peer)) = listener.accept().await else { continue };
                let (table, host) = (table.clone(), host.clone());
                tokio::spawn(async move {
                    let _ = serve(s, peer, table, host).await;
                });
            }
        });
        Ok(Self { addr, relay_addr, task, _relay: relay })
    }

    /// Endereço do relay hospedado, se houver.
    pub fn relay_addr(&self) -> Option<SocketAddr> {
        self.relay_addr
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for DirectoryServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(mut s: TcpStream, peer: SocketAddr, table: Table, host: Option<Arc<Host>>) -> io::Result<()> {
    // Estática (e não `EphemeralSecret`) porque a chave pública do cliente só chega no pedido de publicar.
    let eph = StaticSecret::random_from_rng(OsRng);
    let e_pub = PublicKey::from(&eph);
    let mut hello = e_pub.as_bytes().to_vec();
    hello.extend_from_slice(peer.ip().to_string().as_bytes());
    write_frame(&mut s, &hello).await?;
    for _ in 0..MAX_REQUESTS {
        let req = match read_frame(&mut s).await {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        match req.first() {
            Some(&REQ_HOST) => return host_control(s, peer, &eph, e_pub.as_bytes(), &req, &table, host).await,
            Some(&REQ_ATTACH) => return attach(s, &req, host).await,
            _ => {}
        }
        let reply = handle(&req, &eph, e_pub.as_bytes(), peer, &table);
        write_frame(&mut s, &reply).await?;
    }
    Ok(())
}

/// Conexão de controle de um nó hospedado: registra a rota, publica a entrada e avisa a cada circuito novo.
async fn host_control(mut s: TcpStream, peer: SocketAddr, eph: &StaticSecret, e_pub: &[u8; 32], req: &[u8], table: &Table, host: Option<Arc<Host>>) -> io::Result<()> {
    let refuse = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    let Some(host) = host else { return write_frame(&mut s, &refuse("servidor sem relay hospedado")).await };
    if req.len() != 1 + 32 + 32 {
        return write_frame(&mut s, &refuse("tamanho")).await;
    }
    let pubkey: [u8; 32] = req[1..33].try_into().unwrap();
    let shared = eph.diffie_hellman(&PublicKey::from(pubkey));
    if !shared.was_contributory() || mac(shared.as_bytes(), e_pub, &req[..33]).verify_slice(&req[33..]).is_err() {
        return write_frame(&mut s, &refuse("prova de posse inválida")).await;
    }
    let key = dest_for(&pubkey);
    let fwd = TcpListener::bind("127.0.0.1:0").await?;
    let faddr = fwd.local_addr()?;
    let full = {
        let mut r = host.routes.lock().unwrap();
        let full = r.len() >= MAX_HOSTED && !r.contains_key(&key);
        if !full {
            r.insert(key.clone(), faddr);
        }
        full
    };
    if full {
        return write_frame(&mut s, &refuse("servidor cheio")).await;
    }
    let set_entry = || {
        let mut t = table.lock().unwrap();
        t.retain(|_, e| e.seen.elapsed() < TTL);
        t.insert(pubkey, Entry { info: host.info.clone(), seen: Instant::now() });
    };
    set_entry();
    if host.verbose {
        eprintln!("[servidor] {key} hospedado, conexão de {peer}");
    }
    write_frame(&mut s, &[&[RES_OK][..], host.info.addr.as_bytes()].concat()).await?;
    let mut tick = interval(REPUBLISH);
    tick.tick().await;
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            a = fwd.accept() => {
                let Ok((c, _)) = a else { continue };
                let mut tok = [0u8; 16];
                OsRng.fill_bytes(&mut tok);
                {
                    let mut p = host.pending.lock().unwrap();
                    p.retain(|_, (_, t)| t.elapsed() < PENDING_TTL);
                    if p.len() >= MAX_PENDING {
                        continue;
                    }
                    p.insert(tok, (c, Instant::now()));
                }
                if host.verbose {
                    eprintln!("[servidor] circuito para {key}: avisando o nó");
                }
                if write_frame(&mut s, &[&[CTL_OPEN][..], &tok].concat()).await.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {
                set_entry();
                if write_frame(&mut s, &[CTL_PING]).await.is_err() {
                    break;
                }
            }
            // O nó não manda nada pelo controle: dado ou fim de conexão encerram.
            r = s.read(&mut probe) => {
                if !matches!(r, Ok(n) if n > 0) {
                    break;
                }
            }
        }
    }
    if host.verbose {
        eprintln!("[servidor] {key} saiu");
    }
    // Só desfaz se a rota ainda é desta conexão (o nó pode ter reconectado).
    let mut r = host.routes.lock().unwrap();
    if r.get(&key) == Some(&faddr) {
        r.remove(&key);
        table.lock().unwrap().remove(&pubkey);
    }
    drop(r);
    Ok(())
}

/// O nó hospedado devolve a ficha por uma conexão de saída; emenda com a que o relay abriu.
async fn attach(mut s: TcpStream, req: &[u8], host: Option<Arc<Host>>) -> io::Result<()> {
    let (Some(host), Ok(tok)) = (host, <[u8; 16]>::try_from(&req[1..])) else { return Ok(()) };
    let taken = host.pending.lock().unwrap().remove(&tok);
    let Some((mut c, _)) = taken.filter(|(_, t)| t.elapsed() < PENDING_TTL) else {
        if host.verbose {
            eprintln!("[servidor] ficha de anexo desconhecida ou vencida");
        }
        return Ok(());
    };
    if host.verbose {
        eprintln!("[servidor] nó anexou; emendando");
    }
    let _ = copy_bidirectional(&mut s, &mut c).await;
    Ok(())
}

fn handle(req: &[u8], eph: &StaticSecret, e_pub: &[u8; 32], peer: SocketAddr, table: &Table) -> Vec<u8> {
    let err = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    let Some((&kind, body)) = req.split_first() else { return err("vazio") };
    match kind {
        REQ_PUBLISH => {
            // pub(32) onion(32) link(32) porta(2) addr_len(1) addr mac(32)
            if body.len() < 32 * 3 + 2 + 1 + 32 {
                return err("curto");
            }
            let (data, tag) = body.split_at(body.len() - 32);
            let pubkey: [u8; 32] = data[..32].try_into().unwrap();
            let shared = eph.diffie_hellman(&PublicKey::from(pubkey));
            if !shared.was_contributory() {
                return err("chave inválida");
            }
            if mac(shared.as_bytes(), e_pub, &req[..req.len() - 32]).verify_slice(tag).is_err() {
                return err("prova de posse inválida");
            }
            let port = u16::from_be_bytes([data[96], data[97]]);
            let alen = data[98] as usize;
            if data.len() != 99 + alen {
                return err("tamanho");
            }
            let addr = if alen == 0 {
                SocketAddr::new(peer.ip(), port).to_string()
            } else {
                match std::str::from_utf8(&data[99..]) {
                    Ok(a) if a.parse::<SocketAddr>().is_ok() => a.to_string(),
                    _ => return err("endereço inválido (use ip:porta)"),
                }
            };
            let info = RelayInfo { addr: addr.clone(), onion: data[32..64].try_into().unwrap(), link: data[64..96].try_into().unwrap() };
            let mut t = table.lock().unwrap();
            t.retain(|_, e| e.seen.elapsed() < TTL);
            if t.len() >= MAX_ENTRIES && !t.contains_key(&pubkey) {
                return err("diretório cheio");
            }
            t.insert(pubkey, Entry { info, seen: Instant::now() });
            [&[RES_OK][..], addr.as_bytes()].concat()
        }
        REQ_LOOKUP => {
            let Ok(key) = <[u8; 32]>::try_from(body) else { return err("tamanho") };
            let t = table.lock().unwrap();
            match t.get(&key).filter(|e| e.seen.elapsed() < TTL) {
                Some(e) => {
                    let mut out = vec![RES_FOUND];
                    e.info.encode(&mut out);
                    out
                }
                None => vec![RES_NONE],
            }
        }
        REQ_SAMPLE => {
            // n(1) k(1) k×pub(32)
            let (Some(&n), Some(&k)) = (body.first(), body.get(1)) else { return err("curto") };
            let k = k as usize;
            if body.len() != 2 + 32 * k {
                return err("tamanho");
            }
            let skip: Vec<&[u8]> = body[2..].chunks(32).collect();
            let t = table.lock().unwrap();
            let mut pool: Vec<&RelayInfo> =
                t.iter().filter(|(key, e)| e.seen.elapsed() < TTL && !skip.contains(&&key[..])).map(|(_, e)| &e.info).collect();
            let mut out = vec![RES_LIST, 0];
            for _ in 0..(n as usize).min(8).min(pool.len()) {
                let i = OsRng.next_u32() as usize % pool.len();
                pool.swap_remove(i).encode(&mut out);
                out[1] += 1;
            }
            out
        }
        _ => err("pedido desconhecido"),
    }
}

/// Conexão de cliente com o diretório: lê o `hello` e já responde às provas de posse.
struct Session {
    s: TcpStream,
    e_pub: [u8; 32],
    seen_ip: String,
}

async fn connect(server: &str) -> io::Result<Session> {
    let mut s = timeout(IO_TIMEOUT, TcpStream::connect(server)).await.map_err(|_| io::ErrorKind::TimedOut)??;
    let hello = read_frame(&mut s).await?;
    if hello.len() < 33 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hello inválido"));
    }
    Ok(Session { e_pub: hello[..32].try_into().unwrap(), seen_ip: String::from_utf8_lossy(&hello[32..]).into(), s })
}

async fn ask(sess: &mut Session, req: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(&mut sess.s, req).await?;
    read_frame(&mut sess.s).await
}

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

/// Publica (ou renova) a entrada de `secret` no diretório. `addr`: endereço público `ip:porta` do relay,
/// se o IP que o servidor enxerga não serve (NAT, VPN); `None` usa o IP visto pelo servidor e `relay_port`.
/// Devolve o endereço publicado.
pub async fn publish(server: &str, secret: &StaticSecret, relay_port: u16, onion: [u8; 32], link: [u8; 32], addr: Option<&str>) -> io::Result<String> {
    let mut sess = connect(server).await?;
    let me = PublicKey::from(secret);
    let addr = addr.unwrap_or("");
    if addr.len() > 60 {
        return Err(bad("endereço longo demais"));
    }
    let mut req = vec![REQ_PUBLISH];
    req.extend_from_slice(me.as_bytes());
    req.extend_from_slice(&onion);
    req.extend_from_slice(&link);
    req.extend_from_slice(&relay_port.to_be_bytes());
    req.push(addr.len() as u8);
    req.extend_from_slice(addr.as_bytes());
    let shared = secret.diffie_hellman(&PublicKey::from(sess.e_pub));
    req.extend_from_slice(&mac(shared.as_bytes(), &sess.e_pub, &req).finalize().into_bytes());
    let reply = ask(&mut sess, &req).await?;
    match reply.split_first() {
        Some((&RES_OK, a)) => Ok(String::from_utf8_lossy(a).into()),
        Some((&RES_ERR, m)) => Err(bad(&format!("diretório recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Procura quem atende em `peer`.
pub async fn lookup(server: &str, peer: &[u8; 32]) -> io::Result<Option<RelayInfo>> {
    let mut sess = connect(server).await?;
    let reply = ask(&mut sess, &[&[REQ_LOOKUP][..], peer].concat()).await?;
    match reply.split_first() {
        Some((&RES_FOUND, b)) => Ok(Some(RelayInfo::decode(b).ok_or_else(|| bad("entrada inválida"))?.0)),
        Some((&RES_NONE, _)) => Ok(None),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Até `n` relays sorteados, fora das chaves de `exclude` (no máximo 8 de cada vez).
pub async fn sample(server: &str, n: u8, exclude: &[[u8; 32]]) -> io::Result<Vec<RelayInfo>> {
    let mut sess = connect(server).await?;
    let mut req = vec![REQ_SAMPLE, n, exclude.len() as u8];
    for k in exclude {
        req.extend_from_slice(k);
    }
    let reply = ask(&mut sess, &req).await?;
    let Some((&RES_LIST, b)) = reply.split_first() else { return Err(bad("resposta inesperada")) };
    let (&count, mut rest) = b.split_first().ok_or_else(|| bad("curto"))?;
    let mut out = vec![];
    for _ in 0..count {
        let (info, r) = RelayInfo::decode(rest).ok_or_else(|| bad("entrada inválida"))?;
        out.push(info);
        rest = r;
    }
    Ok(out)
}

/// IP que o servidor enxerga para esta máquina (útil para mostrar ao usuário).
pub async fn seen_ip(server: &str) -> io::Result<String> {
    Ok(connect(server).await?.seen_ip)
}

/// Registra `secret` como hospedado no servidor e mantém a conexão de controle: a cada circuito novo
/// para você, abre uma conexão de saída até o servidor e a emenda com o telefone em `phone`.
/// Reconecta sozinho se o servidor cair. Devolve o endereço do relay do servidor (o publicado) e a tarefa;
/// `errors` guarda o último erro de reconexão (`None` = ligado).
pub async fn host(server: &str, secret: &StaticSecret, phone: SocketAddr, errors: Arc<Mutex<Option<String>>>) -> io::Result<(String, JoinHandle<()>)> {
    let (first, addr) = register(server, secret).await?;
    let (server, secret) = (server.to_string(), secret.clone());
    let task = tokio::spawn(async move {
        let mut sess = Some(first);
        loop {
            if let Some(mut ss) = sess.take() {
                let e = run_control(&mut ss.s, &server, phone).await.err();
                *errors.lock().unwrap() = Some(e.map_or_else(|| "servidor encerrou o controle".into(), |e| e.to_string()));
            }
            sleep(RECONNECT).await;
            match register(&server, &secret).await {
                Ok((ss, _)) => {
                    *errors.lock().unwrap() = None;
                    sess = Some(ss);
                }
                Err(e) => *errors.lock().unwrap() = Some(e.to_string()),
            }
        }
    });
    Ok((addr, task))
}

async fn register(server: &str, secret: &StaticSecret) -> io::Result<(Session, String)> {
    let mut sess = connect(server).await?;
    let mut req = vec![REQ_HOST];
    req.extend_from_slice(PublicKey::from(secret).as_bytes());
    let shared = secret.diffie_hellman(&PublicKey::from(sess.e_pub));
    req.extend_from_slice(&mac(shared.as_bytes(), &sess.e_pub, &req).finalize().into_bytes());
    let reply = ask(&mut sess, &req).await?;
    match reply.split_first() {
        Some((&RES_OK, a)) => Ok((sess, String::from_utf8_lossy(a).into())),
        Some((&RES_ERR, m)) => Err(bad(&format!("servidor recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

async fn run_control(s: &mut TcpStream, server: &str, phone: SocketAddr) -> io::Result<()> {
    loop {
        let f = timeout(CONTROL_IDLE, read_frame_idle(s)).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        if let (Some(&CTL_OPEN), Ok(tok)) = (f.first(), <[u8; 16]>::try_from(f.get(1..).unwrap_or(&[]))) {
            let server = server.to_string();
            tokio::spawn(async move {
                let _ = splice_back(&server, tok, phone).await;
            });
        } else if f.first() != Some(&CTL_PING) {
            return Err(bad("mensagem de controle desconhecida"));
        }
    }
}

async fn splice_back(server: &str, tok: [u8; 16], phone: SocketAddr) -> io::Result<()> {
    let mut sess = connect(server).await?;
    write_frame(&mut sess.s, &[&[REQ_ATTACH][..], &tok].concat()).await?;
    let mut local = timeout(IO_TIMEOUT, TcpStream::connect(phone)).await.map_err(|_| io::ErrorKind::TimedOut)??;
    copy_bidirectional(&mut sess.s, &mut local).await.map(|_| ())
}

/// Lê um quadro sem prazo (o controle fica parado até haver chamada).
async fn read_frame_idle<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}
