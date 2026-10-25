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
//! Formato: quadros `[u16 BE tamanho][dados]`. O servidor abre com `E (32) ‖ hora Unix em s (8, BE) ‖ IP visto (texto)`. A hora sincroniza o relógio dos convites ([`crate::clock`]).

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
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
    time::{interval, timeout},
};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::circuit::manager::Candidate;

mod client;
mod limit;
mod punch;
mod state;

pub use client::*;
pub use limit::RateLimit;
pub use punch::{PunchListener, punch_go, punch_prep, punch_register};
pub use state::SAVE_EVERY;
use limit::*;
use state::*;
use punch::*;

/// Validade de uma entrada sem republicar.
pub const TTL: Duration = Duration::from_secs(120);
/// Intervalo em que o nó republica.
pub const REPUBLISH: Duration = Duration::from_secs(40);
/// Ouvintes de furo de NAT simultâneos no servidor.
pub const MAX_LISTENERS: usize = 1024;
/// Intervalo mínimo entre testes de alcançabilidade da mesma chave.
const REACH_COOLDOWN: Duration = Duration::from_secs(20);
/// Tamanho de um aviso de troca de segredo: nonce (12) ‖ segredo novo cifrado (32) ‖ etiqueta (16).
pub const NOTICE_LEN: usize = 12 + 32 + 16;
/// Quanto tempo um aviso espera quem estava desconectado.
const NOTICE_TTL: Duration = Duration::from_secs(3 * 24 * 3600);
const MAX_NOTICES: usize = 8192;
/// Entradas simultâneas no servidor.
pub const MAX_ENTRIES: usize = 4096;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = 512;
const MAX_REQUESTS: usize = 16;
const CONTEXT: &[u8] = b"clam-dir-v1";
/// Sem nada do servidor por tanto tempo (ele manda um ping a cada [`REPUBLISH`]), o nó reconecta.
const CONTROL_IDLE: Duration = Duration::from_secs(100);

/// Nome do destino que o relay de saída mapeia para o telefone de `key`.
pub fn dest_for(key: &[u8; 32]) -> String {
    let hex: String = key[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}:9")
}

const REQ_PUBLISH: u8 = 1;
const REQ_LOOKUP: u8 = 2;
const REQ_SAMPLE: u8 = 3;
const REQ_PUNCH_LISTEN: u8 = 9;
const REQ_REACH: u8 = 12;
const REQ_NOTICE_PUT: u8 = 13;
const REQ_NOTICE_GET: u8 = 14;
const REQ_PUNCH_PREP: u8 = 10;
const REQ_PUNCH_GO: u8 = 11;
const CTL_PUNCH: u8 = 3;
const REQ_ROOM_JOIN: u8 = 6;
const REQ_ROOM_LEAVE: u8 = 7;
const REQ_ROOM_LIST: u8 = 8;
const CTL_PING: u8 = 2;
const RES_OK: u8 = 0x80;
const RES_ERR: u8 = 0x81;
const RES_FOUND: u8 = 0x82;
const RES_NONE: u8 = 0x83;
const RES_LIST: u8 = 0x84;
const RES_MEMBERS: u8 = 0x85;
/// Contexto das assinaturas de sala (a chave da sala assina `contexto ‖ E ‖ pedido`; `E` impede repetição).
const ROOM_CONTEXT: &[u8] = b"clam-room-v1";
/// Membros por sala e salas no servidor.
pub const MAX_ROOM_MEMBERS: usize = 8;
const MAX_ROOMS: usize = 1024;

/// Como alcançar um relay: endereço TCP e as duas chaves públicas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayInfo {
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

impl RelayInfo {
    pub fn candidate(&self) -> Candidate {
        Candidate { addr: self.addr.clone(), public: self.onion, link_public: self.link, score: 1.0 }
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

/// Copia um trecho de tamanho já conferido pelo chamador para um array.
fn take<const N: usize>(b: &[u8]) -> [u8; N] {
    b.try_into().expect("tamanho conferido antes")
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

struct Member {
    key: [u8; 32],
    joined: Instant,
    seen: Instant,
}

/// Salas: chave pública da sala → membros vivos, do mais antigo (o anfitrião) ao mais novo.
type Rooms = Arc<Mutex<HashMap<[u8; 32], Vec<Member>>>>;

struct Token {
    addr: Option<SocketAddr>,
    key: Option<[u8; 32]>,
    at: Instant,
}

struct Listener {
    id: u64,
    token: [u8; 16],
    udp: Option<SocketAddr>,
    push: mpsc::Sender<SocketAddr>,
}

/// Furo de NAT: o servidor só descobre o endereço UDP de cada ponta (como um STUN) e avisa o ouvinte
/// de quem vai chamar. A voz nunca passa por aqui.
#[derive(Default)]
struct PunchState {
    tokens: Mutex<HashMap<[u8; 16], Token>>,
    listeners: Mutex<HashMap<[u8; 32], Listener>>,
    probed: Mutex<HashMap<[u8; 32], Instant>>,
}

/// Avisos de troca de segredo de sala: (dono, sala antiga, destinatário) → aviso cifrado.
type Notices = Arc<Mutex<HashMap<[u8; 96], (Vec<u8>, Instant)>>>;

/// Validade de uma ficha UDP sem reaparecer.
const TOKEN_TTL: Duration = Duration::from_secs(120);
const MAX_TOKENS: usize = 8192;

/// Servidor de diretório em memória. Encerra ao ser descartado.
pub struct DirectoryServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
    udp_task: JoinHandle<()>,
    saver: Option<JoinHandle<()>>,
}

impl DirectoryServer {
    pub async fn spawn(listen: SocketAddr) -> io::Result<Self> {
        Self::spawn_limited(listen, RateLimit::default()).await
    }

    /// Como [`spawn`](Self::spawn), com outro [`RateLimit`] por IP.
    pub async fn spawn_limited(listen: SocketAddr, limit: RateLimit) -> io::Result<Self> {
        Self::spawn_with(listen, limit, None).await
    }

    /// Como [`spawn_limited`](Self::spawn_limited), mas guarda o estado (publicações e salas) em `state`:
    /// lê ao subir e regrava a cada [`SAVE_EVERY`]. Quem republica em até 40 s reaparece sem o diretório
    /// ter ficado vazio, e as salas mantêm os membros e a ordem (o anfitrião) depois de um reinício.
    pub async fn spawn_with(listen: SocketAddr, limit: RateLimit, state: Option<std::path::PathBuf>) -> io::Result<Self> {
        let listener = TcpListener::bind(listen).await?;
        let addr = listener.local_addr()?;
        let table: Table = Arc::default();
        let rooms: Rooms = Arc::default();
        if let Some(path) = &state
            && let Ok(bytes) = std::fs::read(path)
        {
            load_state(&bytes, &mut table.lock().unwrap(), &mut rooms.lock().unwrap());
        }
        let saver = state.map(|path| {
            let (table, rooms) = (table.clone(), rooms.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(SAVE_EVERY).await;
                    let bytes = dump_state(&table.lock().unwrap(), &rooms.lock().unwrap());
                    // Grava num arquivo ao lado e renomeia: uma queda no meio não deixa estado pela metade.
                    let tmp = path.with_extension("tmp");
                    if std::fs::write(&tmp, bytes).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
            })
        });
        let punch: Arc<PunchState> = Arc::default();
        let notices: Notices = Arc::default();
        let limiter = Arc::new(Limiter::new(limit));
        let udp = Arc::new(UdpSocket::bind(addr).await?);
        let udp_task = tokio::spawn(udp_loop(udp, punch.clone()));
        let task = tokio::spawn(async move {
            loop {
                let Ok((s, peer)) = listener.accept().await else { continue };
                if !limiter.allow(peer.ip(), 1.0) {
                    continue;
                }
                let (table, rooms, punch, notices, limiter) = (table.clone(), rooms.clone(), punch.clone(), notices.clone(), limiter.clone());
                tokio::spawn(async move {
                    let _ = serve(s, peer, table, rooms, punch, notices, limiter).await;
                });
            }
        });
        Ok(Self { addr, task, udp_task, saver })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for DirectoryServer {
    fn drop(&mut self) {
        self.task.abort();
        self.udp_task.abort();
        if let Some(t) = &self.saver {
            t.abort();
        }
    }
}

async fn serve(mut s: TcpStream, peer: SocketAddr, table: Table, rooms: Rooms, punch: Arc<PunchState>, notices: Notices, limiter: Arc<Limiter>) -> io::Result<()> {
    // Estática (e não `EphemeralSecret`) porque a chave pública do cliente só chega no pedido de publicar.
    let eph = StaticSecret::random_from_rng(OsRng);
    let e_pub = PublicKey::from(&eph);
    let mut hello = e_pub.as_bytes().to_vec();
    hello.extend_from_slice(&crate::clock::now_secs().to_be_bytes());
    hello.extend_from_slice(peer.ip().to_string().as_bytes());
    write_frame(&mut s, &hello).await?;
    for _ in 0..MAX_REQUESTS {
        let req = match read_frame(&mut s).await {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        if !limiter.allow(peer.ip(), cost(req.first())) {
            write_frame(&mut s, &[&[RES_ERR][..], b"muitos pedidos"].concat()).await?;
            return Ok(());
        }
        match req.first() {
            Some(&REQ_NOTICE_PUT | &REQ_NOTICE_GET) => {
                let reply = notice_request(&req, &eph, e_pub.as_bytes(), &notices);
                write_frame(&mut s, &reply).await?;
                continue;
            }
            Some(&REQ_REACH) => {
                let reply = reach(&req, &eph, e_pub.as_bytes(), &table, &punch).await;
                write_frame(&mut s, &reply).await?;
                continue;
            }
            Some(&REQ_PUNCH_LISTEN) => return punch_listen(s, &eph, e_pub.as_bytes(), &req, &punch).await,
            Some(&REQ_PUNCH_PREP | &REQ_PUNCH_GO) => {
                let reply = punch_request(&req, &punch);
                write_frame(&mut s, &reply).await?;
                continue;
            }
            _ => {}
        }
        let reply = handle(&req, &eph, e_pub.as_bytes(), peer, &table, &rooms);
        write_frame(&mut s, &reply).await?;
    }
    Ok(())
}

fn handle(req: &[u8], eph: &StaticSecret, e_pub: &[u8; 32], peer: SocketAddr, table: &Table, rooms: &Rooms) -> Vec<u8> {
    let err = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    let Some((&kind, body)) = req.split_first() else { return err("vazio") };
    match kind {
        REQ_PUBLISH => {
            // pub(32) onion(32) link(32) porta(2) addr_len(1) addr mac(32)
            if body.len() < 32 * 3 + 2 + 1 + 32 {
                return err("curto");
            }
            let (data, tag) = body.split_at(body.len() - 32);
            let pubkey: [u8; 32] = take(&data[..32]);
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
            let info = RelayInfo { addr: addr.clone(), onion: take(&data[32..64]), link: take(&data[64..96]) };
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
        REQ_ROOM_JOIN | REQ_ROOM_LEAVE | REQ_ROOM_LIST => room_request(kind, body, e_pub, rooms),
        _ => err("pedido desconhecido"),
    }
}

/// Avisos de troca de segredo. `PUT` (só o dono, com a prova de posse da chave dele): `dono ‖ sala ‖ destino ‖
/// aviso ‖ HMAC`. `GET` (aberto, o aviso é cifrado): `dono ‖ sala ‖ destino`. Um aviso por (dono, sala, destino).
fn notice_request(req: &[u8], eph: &StaticSecret, e_pub: &[u8; 32], notices: &Notices) -> Vec<u8> {
    let err = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    let slot = |b: &[u8]| -> [u8; 96] { take(&b[..96]) };
    let mut all = notices.lock().unwrap();
    if req[0] == REQ_NOTICE_GET {
        if req.len() != 1 + 96 {
            return err("tamanho");
        }
        all.retain(|_, (_, at)| at.elapsed() < NOTICE_TTL);
        return match all.get(&slot(&req[1..])) {
            Some((blob, _)) => [&[RES_FOUND][..], blob].concat(),
            None => vec![RES_NONE],
        };
    }
    if req.len() != 1 + 96 + NOTICE_LEN + 32 {
        return err("tamanho");
    }
    let owner: [u8; 32] = take(&req[1..33]);
    let shared = eph.diffie_hellman(&PublicKey::from(owner));
    let split = req.len() - 32;
    if !shared.was_contributory() || mac(shared.as_bytes(), e_pub, &req[..split]).verify_slice(&req[split..]).is_err() {
        return err("prova de posse inválida");
    }
    all.retain(|_, (_, at)| at.elapsed() < NOTICE_TTL);
    let key = slot(&req[1..]);
    if all.len() >= MAX_NOTICES && !all.contains_key(&key) {
        return err("servidor cheio");
    }
    all.insert(key, (req[97..split].to_vec(), Instant::now()));
    vec![RES_OK]
}

/// Entrar, sair ou listar uma sala. Corpo: `sala (32) ‖ [membro (32)] ‖ assinatura da sala (64)`.
fn room_request(kind: u8, body: &[u8], e_pub: &[u8; 32], rooms: &Rooms) -> Vec<u8> {
    let err = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    let want = if kind == REQ_ROOM_LIST { 32 + 64 } else { 32 + 32 + 64 };
    if body.len() != want {
        return err("tamanho");
    }
    let (data, sig) = body.split_at(body.len() - 64);
    let room: [u8; 32] = take(&data[..32]);
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&room) else { return err("sala inválida") };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else { return err("assinatura inválida") };
    let msg = [ROOM_CONTEXT, e_pub, &[kind], data].concat();
    if ed25519_dalek::Verifier::verify(&vk, &msg, &sig).is_err() {
        return err("sem o segredo da sala");
    }
    let mut all = rooms.lock().unwrap();
    for m in all.values_mut() {
        m.retain(|x| x.seen.elapsed() < TTL);
    }
    all.retain(|_, m| !m.is_empty());
    match kind {
        REQ_ROOM_JOIN => {
            let key: [u8; 32] = take(&data[32..]);
            if !all.contains_key(&room) && all.len() >= MAX_ROOMS {
                return err("servidor cheio de salas");
            }
            let list = all.entry(room).or_default();
            if let Some(m) = list.iter_mut().find(|m| m.key == key) {
                m.seen = Instant::now();
            } else if list.len() >= MAX_ROOM_MEMBERS {
                return err("sala cheia");
            } else {
                list.push(Member { key, joined: Instant::now(), seen: Instant::now() });
            }
            vec![RES_OK]
        }
        REQ_ROOM_LEAVE => {
            if let Some(list) = all.get_mut(&room) {
                list.retain(|m| m.key[..] != data[32..]);
            }
            vec![RES_OK]
        }
        _ => {
            let mut list: Vec<&Member> = all.get(&room).map(|l| l.iter().collect()).unwrap_or_default();
            list.sort_by_key(|m| m.joined);
            let mut out = vec![RES_MEMBERS, list.len() as u8];
            for m in list {
                out.extend_from_slice(&m.key);
            }
            out
        }
    }
}
