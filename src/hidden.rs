//! Pontos de introdução e de encontro: os destinos de saída `intro:9` e `rv:9` (camada B de `docs/DOIS-RELAYS.md`).
//!
//! Para quem liga não aprender o IP de quem atende, quem atende não pode ter endereço publicado. O relay de saída
//! entrega estes dois destinos a serviços locais, no molde do gateway da DHT (`dht:9`) e da âncora (`anchor:9`):
//! cada conexão local é um circuito, e o quadro é `[u16 tamanho][pedaço de até 200 B]`, com a mensagem `u16 tamanho ‖
//! corpo` partida em pedaços para caber na célula de 256 B.
//!
//! - **Ponto de introdução (`intro:9`).** Quem atende (D) mantém um circuito de espera aqui e prova que tem a chave de
//!   serviço (`auth_key`, ed25519, nova por período e sem relação com a chave de contato) assinando um desafio. Quem
//!   liga (C) manda `Introduce(auth_key, blob)`; o ponto repassa o `blob` a D pelo circuito de espera **sem lê-lo**
//!   (é cifrado só para D). O ponto vê o relay de saída do circuito de D, nunca o IP de D.
//! - **Ponto de encontro (`rv:9`).** C deixa um `cookie` de 32 bytes aleatórios (`Establish`); D chega por outro
//!   circuito e manda `Join(cookie)`. O ponto confirma aos dois e passa a copiar os bytes de um circuito ao outro, sem
//!   ler nada. O `cookie` é de **uso único** e vence em [`RV_TTL`].
//!
//! Nenhum dos dois serviços sabe quem é contato de quem: a prova de autoria vai dentro do `blob` e quem confere é D,
//! que descarta introdução de chave fora da lista. Os serviços só limitam abuso (tabelas e pedidos por minuto).
//!
//! Tudo o que lê bytes da rede está em [`IntroRequest::parse`], [`RvRequest::parse`], [`IntroReply::parse`],
//! [`RvReply::parse`] e [`Messages::push`], os alvos de fuzz. As tabelas ([`IntroPoint`], [`RvPoint`]) são sans-IO: o
//! tempo entra como argumento e o "canal até o circuito" é um tipo qualquer.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, VerifyingKey};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};

use crate::{bytes::Reader, sync::LockExt};

/// Destino que o relay de saída mapeia para o ponto de introdução.
pub const INTRO_DEST: &str = "intro:9";

/// Destino que o relay de saída mapeia para o ponto de encontro.
pub const RV_DEST: &str = "rv:9";

/// Bytes por pedaço no circuito (cabem em uma célula), o mesmo do gateway.
pub const CHUNK: usize = 200;

/// Maior mensagem (corpo, sem o tamanho) nos dois sentidos.
pub const MAX_BODY: usize = 400;

/// Maior `blob` de uma introdução: o que sobra de [`MAX_BODY`] depois da etiqueta, da chave e do tamanho.
pub const MAX_BLOB: usize = MAX_BODY - 1 - 32 - 2;

/// Contexto da assinatura de posse da chave de serviço.
const PROOF_DOMAIN: &[u8] = b"clam-intro-establish-v1";

/// Quanto tempo um `cookie` espera por quem chega (D monta um circuito de 2 relays até o ponto de encontro).
pub const RV_TTL: Duration = Duration::from_secs(60);

/// Serviços (circuitos de espera) que um ponto de introdução aceita ao mesmo tempo.
pub const MAX_SERVICES: usize = 256;

/// `cookie`s esperando num ponto de encontro.
pub const MAX_WAITING: usize = 256;

/// Introduções por minuto, por chave de serviço (um D popular não vira alvo de inundação).
pub const INTRODUCE_PER_SERVICE: u32 = 20;

/// Pedidos por minuto somados no ponto (introdução ou encontro), contra quem queira só gastar o relay.
pub const REQUESTS_PER_MINUTE: u32 = 600;

/// Mensagens que o ponto de introdução empilha para um circuito de espera lento antes de recusar mais.
const PUSH_BACKLOG: usize = 8;

// ---------------------------------------------------------------------------
// Enquadramento
// ---------------------------------------------------------------------------

/// O pedaço ou a mensagem passa do limite: a conexão não serve mais.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadFrame;

/// Junta os pedaços `[u16 tamanho][bytes]` do circuito em mensagens `u16 tamanho ‖ corpo`.
#[derive(Default)]
pub struct Messages {
    acc: Vec<u8>,
}

impl Messages {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acrescenta um pedaço e devolve as mensagens completas, na ordem. Erro: pedaço acima de [`CHUNK`], mensagem de
    /// tamanho zero ou acima de [`MAX_BODY`].
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, BadFrame> {
        if chunk.len() > CHUNK {
            return Err(BadFrame);
        }
        self.acc.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(total) = Reader::new(&self.acc).u16().map(usize::from) {
            if total == 0 || total > MAX_BODY {
                return Err(BadFrame);
            }
            let end = total.saturating_add(2);
            if self.acc.len() < end {
                break;
            }
            let rest = self.acc.split_off(end);
            let body = self.acc.get(2..).map(<[u8]>::to_vec).unwrap_or_default();
            self.acc = rest;
            out.push(body);
        }
        Ok(out)
    }

    /// Bytes que sobraram de uma mensagem ainda incompleta (ou de uma seguinte já recebida).
    pub fn pending(&self) -> usize {
        self.acc.len()
    }
}

/// O corpo como vai no circuito: `u16 tamanho ‖ corpo`, partido em pedaços `[u16 tamanho][bytes]` de até [`CHUNK`].
#[must_use]
pub fn frame(body: &[u8]) -> Vec<u8> {
    let len = u16::try_from(body.len()).unwrap_or(u16::MAX);
    let mut message = len.to_be_bytes().to_vec();
    message.extend_from_slice(body);
    let mut wire = Vec::with_capacity(message.len() + 2 * message.len().div_ceil(CHUNK));
    for piece in message.chunks(CHUNK) {
        wire.extend_from_slice(&u16::try_from(piece.len()).unwrap_or(0).to_be_bytes());
        wire.extend_from_slice(piece);
    }
    wire
}

/// Lê um pedaço `[u16 tamanho][bytes]`. `None`: a conexão fechou, ou o pedaço passa de [`CHUNK`].
pub async fn read_chunk<R: AsyncRead + Unpin>(reader: &mut R) -> Option<Vec<u8>> {
    let mut len = [0u8; 2];
    reader.read_exact(&mut len).await.ok()?;
    let len = usize::from(u16::from_be_bytes(len));
    if len > CHUNK {
        return None;
    }
    let mut chunk = vec![0u8; len];
    reader.read_exact(&mut chunk).await.ok()?;
    Some(chunk)
}

/// Escreve uma mensagem, enquadrada por [`frame`].
pub async fn send<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> bool {
    writer.write_all(&frame(body)).await.is_ok()
}

// ---------------------------------------------------------------------------
// Ponto de introdução: mensagens
// ---------------------------------------------------------------------------

const TAG_ESTABLISH: u8 = 0x10;
const TAG_PROVE: u8 = 0x11;
const TAG_INTRODUCE: u8 = 0x12;
const TAG_CHALLENGE: u8 = 0x80;
const TAG_ESTABLISHED: u8 = 0x81;
const TAG_DELIVERED: u8 = 0x82;
const TAG_PUSH: u8 = 0x90;
const TAG_INTRO_REFUSED: u8 = 0x8f;

/// Pedido a `intro:9`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntroRequest {
    /// D quer receber introduções para `auth_key`; o ponto responde com um desafio.
    Establish { auth_key: [u8; 32] },
    /// D assina o desafio com a chave de serviço.
    Prove { signature: [u8; 64] },
    /// C pede que o `blob` (cifrado só para D) chegue a quem registrou `auth_key`.
    Introduce { auth_key: [u8; 32], blob: Vec<u8> },
}

impl IntroRequest {
    /// Lê um corpo de mensagem; `None` para tudo que não for exatamente um pedido válido.
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        let mut r = Reader::new(body);
        let request = match r.u8()? {
            TAG_ESTABLISH => Self::Establish { auth_key: r.array()? },
            TAG_PROVE => Self::Prove { signature: r.array()? },
            TAG_INTRODUCE => {
                let auth_key = r.array()?;
                let len = usize::from(r.u16()?);
                if len == 0 || len > MAX_BLOB {
                    return None;
                }
                Self::Introduce { auth_key, blob: r.take(len)?.to_vec() }
            }
            _ => return None,
        };
        r.is_empty().then_some(request)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Establish { auth_key } => [&[TAG_ESTABLISH][..], auth_key].concat(),
            Self::Prove { signature } => [&[TAG_PROVE][..], signature].concat(),
            Self::Introduce { auth_key, blob } => {
                let len = u16::try_from(blob.len()).unwrap_or(u16::MAX);
                [&[TAG_INTRODUCE][..], auth_key, &len.to_be_bytes(), blob].concat()
            }
        }
    }
}

/// Resposta ou aviso de `intro:9`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntroReply {
    /// Desafio para quem pediu `Establish`.
    Challenge([u8; 32]),
    /// A chave de serviço está registrada neste circuito.
    Established,
    /// A introdução foi entregue ao circuito de espera.
    Delivered,
    /// Para D, no circuito de espera: uma introdução chegou (o `blob` que C mandou, intacto).
    Push(Vec<u8>),
    /// Recusado: prova inválida, tabela cheia, serviço desconhecido ou limite de pedidos.
    Refused,
}

impl IntroReply {
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        let mut r = Reader::new(body);
        let reply = match r.u8()? {
            TAG_CHALLENGE => Self::Challenge(r.array()?),
            TAG_ESTABLISHED => Self::Established,
            TAG_DELIVERED => Self::Delivered,
            TAG_INTRO_REFUSED => Self::Refused,
            TAG_PUSH => {
                let len = usize::from(r.u16()?);
                if len == 0 || len > MAX_BLOB {
                    return None;
                }
                Self::Push(r.take(len)?.to_vec())
            }
            _ => return None,
        };
        r.is_empty().then_some(reply)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Challenge(nonce) => [&[TAG_CHALLENGE][..], nonce].concat(),
            Self::Established => vec![TAG_ESTABLISHED],
            Self::Delivered => vec![TAG_DELIVERED],
            Self::Refused => vec![TAG_INTRO_REFUSED],
            Self::Push(blob) => {
                let len = u16::try_from(blob.len()).unwrap_or(u16::MAX);
                [&[TAG_PUSH][..], &len.to_be_bytes(), blob].concat()
            }
        }
    }
}

/// O que se assina para provar a posse da chave de serviço: o contexto, o desafio e a própria chave. O desafio é
/// novo a cada pedido, então a assinatura não vale em outro circuito nem em outro ponto.
#[must_use]
pub fn proof_message(nonce: &[u8; 32], auth_key: &[u8; 32]) -> Vec<u8> {
    [PROOF_DOMAIN, nonce, auth_key].concat()
}

// ---------------------------------------------------------------------------
// Ponto de introdução: tabela (sans-IO)
// ---------------------------------------------------------------------------

/// Em que pé está uma conexão de `intro:9`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum IntroConn {
    /// Nada pedido ainda (ou só introduções, que não deixam estado).
    #[default]
    Fresh,
    /// `Establish` recebido; falta a assinatura do desafio `nonce`.
    Challenged { auth_key: [u8; 32], nonce: [u8; 32] },
    /// Prova aceita: esta conexão recebe as introduções de `auth_key`.
    Serving { auth_key: [u8; 32] },
}

/// O que o adaptador faz depois de [`IntroPoint::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntroAction<H> {
    /// Responde a quem pediu.
    Reply(IntroReply),
    /// Entrega o `blob` ao circuito de espera `H`; o adaptador responde `Delivered` se a entrega der certo e `Refused` se não.
    Forward(H, Vec<u8>),
}

/// Janela de um minuto com teto, sem relógio próprio.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self { start: now, count: 0 }
    }

    fn take(&mut self, now: Instant, limit: u32) -> bool {
        if now.saturating_duration_since(self.start) >= Duration::from_secs(60) {
            *self = Self::new(now);
        }
        self.count = self.count.saturating_add(1);
        self.count <= limit
    }
}

/// Tabela do ponto de introdução: chave de serviço → circuito de espera.
pub struct IntroPoint<H> {
    services: HashMap<[u8; 32], (u64, H, Window)>,
    total: Window,
    next_id: u64,
}

impl<H: Clone> Default for IntroPoint<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: Clone> IntroPoint<H> {
    pub fn new() -> Self {
        Self { services: HashMap::new(), total: Window::new(Instant::now()), next_id: 0 }
    }

    /// Circuitos de espera registrados agora.
    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Identificador novo para uma conexão (o que [`Self::leave`] usa para não derrubar o registro de outra).
    pub fn connection(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// Trata um pedido da conexão `conn` (veja [`Self::connection`]) em estado `state`. `nonce`: bytes aleatórios
    /// novos, usados só se o pedido for `Establish`. `handle`: como chegar a esta conexão, guardado se a prova passar.
    pub fn handle(&mut self, state: &mut IntroConn, conn: u64, handle: &H, request: IntroRequest, now: Instant, nonce: [u8; 32]) -> IntroAction<H> {
        if !self.total.take(now, REQUESTS_PER_MINUTE) {
            return IntroAction::Reply(IntroReply::Refused);
        }
        match request {
            IntroRequest::Establish { auth_key } => {
                if VerifyingKey::from_bytes(&auth_key).is_err() {
                    return IntroAction::Reply(IntroReply::Refused);
                }
                *state = IntroConn::Challenged { auth_key, nonce };
                IntroAction::Reply(IntroReply::Challenge(nonce))
            }
            IntroRequest::Prove { signature } => {
                let IntroConn::Challenged { auth_key, nonce } = *state else {
                    return IntroAction::Reply(IntroReply::Refused);
                };
                // O desafio vale uma vez: errar a prova obriga a recomeçar.
                *state = IntroConn::Fresh;
                let proven = VerifyingKey::from_bytes(&auth_key)
                    .is_ok_and(|key| key.verify_strict(&proof_message(&nonce, &auth_key), &Signature::from_bytes(&signature)).is_ok());
                let room = self.services.contains_key(&auth_key) || self.services.len() < MAX_SERVICES;
                if !proven || !room {
                    return IntroAction::Reply(IntroReply::Refused);
                }
                // Quem prova a posse da chave substitui o registro antigo (D refez o circuito de espera).
                self.services.insert(auth_key, (conn, handle.clone(), Window::new(now)));
                *state = IntroConn::Serving { auth_key };
                IntroAction::Reply(IntroReply::Established)
            }
            IntroRequest::Introduce { auth_key, blob } => {
                let Some((_, target, window)) = self.services.get_mut(&auth_key) else {
                    return IntroAction::Reply(IntroReply::Refused);
                };
                if window.take(now, INTRODUCE_PER_SERVICE) {
                    IntroAction::Forward(target.clone(), blob)
                } else {
                    IntroAction::Reply(IntroReply::Refused)
                }
            }
        }
    }

    /// A conexão `conn` acabou: tira o registro que ela tinha, se ainda for o dela.
    pub fn leave(&mut self, state: &IntroConn, conn: u64) {
        if let IntroConn::Serving { auth_key } = state
            && self.services.get(auth_key).is_some_and(|(owner, _, _)| *owner == conn)
        {
            self.services.remove(auth_key);
        }
    }
}

// ---------------------------------------------------------------------------
// Ponto de encontro: mensagens e tabela
// ---------------------------------------------------------------------------

const TAG_ESTABLISH_RV: u8 = 0x20;
const TAG_JOIN: u8 = 0x21;
const TAG_WAITING: u8 = 0xa0;
const TAG_JOINED: u8 = 0xa1;
const TAG_RV_REFUSED: u8 = 0xaf;

/// Pedido a `rv:9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvRequest {
    /// C deixa o `cookie` e espera D.
    Establish { cookie: [u8; 32] },
    /// D chega com o `cookie` que recebeu na introdução.
    Join { cookie: [u8; 32] },
}

impl RvRequest {
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        let mut r = Reader::new(body);
        let request = match r.u8()? {
            TAG_ESTABLISH_RV => Self::Establish { cookie: r.array()? },
            TAG_JOIN => Self::Join { cookie: r.array()? },
            _ => return None,
        };
        r.is_empty().then_some(request)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Establish { cookie } => [&[TAG_ESTABLISH_RV][..], cookie].concat(),
            Self::Join { cookie } => [&[TAG_JOIN][..], cookie].concat(),
        }
    }
}

/// Resposta de `rv:9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvReply {
    /// O `cookie` está guardado; falta D chegar.
    Waiting,
    /// Os dois lados chegaram: daqui em diante os bytes passam direto de um circuito ao outro.
    Joined,
    /// `cookie` repetido, desconhecido, vencido, tabela cheia ou limite de pedidos.
    Refused,
}

impl RvReply {
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        match body {
            [TAG_WAITING] => Some(Self::Waiting),
            [TAG_JOINED] => Some(Self::Joined),
            [TAG_RV_REFUSED] => Some(Self::Refused),
            _ => None,
        }
    }

    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        vec![match self {
            Self::Waiting => TAG_WAITING,
            Self::Joined => TAG_JOINED,
            Self::Refused => TAG_RV_REFUSED,
        }]
    }
}

/// Tabela do ponto de encontro: `cookie` → quem espera (`H`). Uso único e com prazo.
pub struct RvPoint<H> {
    waiting: HashMap<[u8; 32], (H, Instant)>,
    total: Window,
}

impl<H> Default for RvPoint<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H> RvPoint<H> {
    pub fn new() -> Self {
        Self { waiting: HashMap::new(), total: Window::new(Instant::now()) }
    }

    /// `cookie`s esperando agora (sem contar os vencidos que ainda não saíram).
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    fn expire(&mut self, now: Instant) {
        self.waiting.retain(|_, (_, at)| now.saturating_duration_since(*at) < RV_TTL);
    }

    /// Conta um pedido no teto por minuto; `false`: passou do limite.
    pub fn admit(&mut self, now: Instant) -> bool {
        self.total.take(now, REQUESTS_PER_MINUTE)
    }

    /// Guarda `cookie` com quem espera. `false`: o `cookie` já está na tabela (não se sobrescreve: quem o repete não
    /// toma o lugar de quem chegou primeiro) ou a tabela está cheia.
    pub fn establish(&mut self, cookie: [u8; 32], waiter: H, now: Instant) -> bool {
        self.expire(now);
        if self.waiting.contains_key(&cookie) || self.waiting.len() >= MAX_WAITING {
            return false;
        }
        self.waiting.insert(cookie, (waiter, now));
        true
    }

    /// Entrega quem esperava por `cookie` e tira o `cookie` da tabela: só um `Join` o usa.
    pub fn join(&mut self, cookie: &[u8; 32], now: Instant) -> Option<H> {
        self.expire(now);
        self.waiting.remove(cookie).map(|(waiter, _)| waiter)
    }

    /// Tira `cookie` (quem esperava desistiu).
    pub fn cancel(&mut self, cookie: &[u8; 32]) {
        self.waiting.remove(cookie);
    }
}

// ---------------------------------------------------------------------------
// Adaptadores tokio
// ---------------------------------------------------------------------------

fn random_32() -> [u8; 32] {
    use rand_core::RngCore;
    let mut bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes
}

type Pusher = mpsc::Sender<Vec<u8>>;

/// Atende os circuitos que o relay de saída encaminha a `listener` (veja [`INTRO_DEST`]).
pub async fn serve_intro(listener: TcpListener) {
    let point = Arc::new(Mutex::new(IntroPoint::<Pusher>::new()));
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(intro_conn(stream, point.clone()));
    }
}

/// Uma conexão é um circuito: um escritor único leva as respostas e as introduções empurradas, o leitor trata os pedidos.
async fn intro_conn(stream: TcpStream, point: Arc<Mutex<IntroPoint<Pusher>>>) {
    let (mut read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(PUSH_BACKLOG);
    let writer = tokio::spawn(async move {
        while let Some(wire) = rx.recv().await {
            if write.write_all(&wire).await.is_err() {
                break;
            }
        }
    });
    let conn = point.lock_unpoisoned().connection();
    let (mut state, mut messages) = (IntroConn::Fresh, Messages::new());
    'conn: while let Some(chunk) = read_chunk(&mut read).await {
        let Ok(bodies) = messages.push(&chunk) else { break };
        for body in bodies {
            let Some(request) = IntroRequest::parse(&body) else { break 'conn };
            let action = point.lock_unpoisoned().handle(&mut state, conn, &tx, request, Instant::now(), random_32());
            let reply = match action {
                IntroAction::Reply(reply) => reply,
                // `try_send`: um circuito de espera lento não segura quem está introduzindo, e o lock não vai a `await`.
                IntroAction::Forward(target, blob) => {
                    if target.try_send(frame(&IntroReply::Push(blob).encode())).is_ok() {
                        IntroReply::Delivered
                    } else {
                        IntroReply::Refused
                    }
                }
            };
            if tx.send(frame(&reply.encode())).await.is_err() {
                break 'conn;
            }
        }
    }
    point.lock_unpoisoned().leave(&state, conn);
    drop(tx);
    let _ = writer.await;
}

/// Atende os circuitos que o relay de saída encaminha a `listener` (veja [`RV_DEST`]).
pub async fn serve_rendezvous(listener: TcpListener) {
    let point = Arc::new(Mutex::new(RvPoint::<oneshot::Sender<TcpStream>>::new()));
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(rv_conn(stream, point.clone()));
    }
}

/// Uma conexão faz um único pedido. `Establish` espera o `Join` (ou o prazo); `Join` entrega o próprio circuito a quem
/// esperava. Quem esperava confirma aos dois e copia os bytes entre eles até um fechar.
async fn rv_conn(mut stream: TcpStream, point: Arc<Mutex<RvPoint<oneshot::Sender<TcpStream>>>>) {
    let mut messages = Messages::new();
    let request = loop {
        let Some(chunk) = read_chunk(&mut stream).await else { return };
        let Ok(mut bodies) = messages.push(&chunk) else { return };
        if let Some(body) = bodies.pop() {
            // Mais de uma mensagem, ou dados atrás do pedido: o protocolo manda esperar a confirmação.
            if !bodies.is_empty() || messages.pending() > 0 {
                return;
            }
            break RvRequest::parse(&body);
        }
    };
    let now = Instant::now();
    if !point.lock_unpoisoned().admit(now) {
        let _ = send(&mut stream, &RvReply::Refused.encode()).await;
        return;
    }
    match request {
        Some(RvRequest::Establish { cookie }) => {
            let (give, take) = oneshot::channel();
            if !point.lock_unpoisoned().establish(cookie, give, now) {
                let _ = send(&mut stream, &RvReply::Refused.encode()).await;
                return;
            }
            if !send(&mut stream, &RvReply::Waiting.encode()).await {
                point.lock_unpoisoned().cancel(&cookie);
                return;
            }
            let Ok(Ok(mut other)) = tokio::time::timeout(RV_TTL, take).await else {
                point.lock_unpoisoned().cancel(&cookie);
                return;
            };
            if send(&mut stream, &RvReply::Joined.encode()).await {
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut other).await;
            }
        }
        Some(RvRequest::Join { cookie }) => {
            let waiter = point.lock_unpoisoned().join(&cookie, now);
            match waiter {
                // Confirma a D antes de entregar o circuito: depois disso quem espera só copia bytes.
                Some(give) if send(&mut stream, &RvReply::Joined.encode()).await => {
                    let _ = give.send(stream);
                }
                _ => {
                    let _ = send(&mut stream, &RvReply::Refused.encode()).await;
                }
            }
        }
        None => {}
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> SigningKey {
        SigningKey::generate(&mut rand_core::OsRng)
    }

    fn establish(point: &mut IntroPoint<u32>, signer: &SigningKey, conn: u64, handle: u32, now: Instant) -> (IntroConn, IntroReply) {
        let auth_key = signer.verifying_key().to_bytes();
        let mut state = IntroConn::Fresh;
        let nonce = [7u8; 32];
        let IntroAction::Reply(IntroReply::Challenge(got)) = point.handle(&mut state, conn, &handle, IntroRequest::Establish { auth_key }, now, nonce) else {
            panic!("esperava um desafio");
        };
        assert_eq!(got, nonce);
        let signature = signer.sign(&proof_message(&nonce, &auth_key)).to_bytes();
        let IntroAction::Reply(reply) = point.handle(&mut state, conn, &handle, IntroRequest::Prove { signature }, now, [0; 32]) else {
            panic!("esperava uma resposta");
        };
        (state, reply)
    }

    #[test]
    fn framing_round_trips_across_chunks() {
        let body: Vec<u8> = (0..=255u8).cycle().take(MAX_BODY).collect();
        let wire = frame(&body);
        let mut messages = Messages::new();
        let mut got = Vec::new();
        let mut r = Reader::new(&wire);
        while let Some(len) = r.u16() {
            let chunk = r.take(usize::from(len)).expect("pedaço inteiro");
            got.extend(messages.push(chunk).expect("quadro válido"));
        }
        assert_eq!(got, vec![body]);
        assert_eq!(messages.pending(), 0);
    }

    #[test]
    fn framing_refuses_oversize_and_empty() {
        assert_eq!(Messages::new().push(&[0u8; CHUNK + 1]), Err(BadFrame));
        assert_eq!(Messages::new().push(&0u16.to_be_bytes()), Err(BadFrame));
        assert_eq!(Messages::new().push(&((MAX_BODY as u16) + 1).to_be_bytes()), Err(BadFrame));
    }

    #[test]
    fn two_messages_in_one_chunk_come_out_in_order() {
        let mut both = Vec::new();
        for body in [&[1u8, 2][..], &[3u8][..]] {
            both.extend_from_slice(&(body.len() as u16).to_be_bytes());
            both.extend_from_slice(body);
        }
        assert_eq!(Messages::new().push(&both), Ok(vec![vec![1, 2], vec![3]]));
    }

    #[test]
    fn requests_and_replies_round_trip_and_reject_trailing_bytes() {
        let requests = [
            IntroRequest::Establish { auth_key: [1; 32] },
            IntroRequest::Prove { signature: [2; 64] },
            IntroRequest::Introduce { auth_key: [3; 32], blob: vec![9; 50] },
        ];
        for request in requests {
            let bytes = request.encode();
            assert_eq!(IntroRequest::parse(&bytes), Some(request));
            assert_eq!(IntroRequest::parse(&[&bytes[..], &[0]].concat()), None, "bytes a mais");
            assert_eq!(IntroRequest::parse(bytes.split_last().expect("não vazio").1), None, "truncado");
        }
        for reply in [IntroReply::Challenge([4; 32]), IntroReply::Established, IntroReply::Delivered, IntroReply::Refused, IntroReply::Push(vec![5; 20])] {
            assert_eq!(IntroReply::parse(&reply.encode()), Some(reply));
        }
        for request in [RvRequest::Establish { cookie: [6; 32] }, RvRequest::Join { cookie: [7; 32] }] {
            assert_eq!(RvRequest::parse(&request.encode()), Some(request));
        }
        for reply in [RvReply::Waiting, RvReply::Joined, RvReply::Refused] {
            assert_eq!(RvReply::parse(&reply.encode()), Some(reply));
        }
    }

    #[test]
    fn an_introduction_blob_must_be_sized() {
        let empty = IntroRequest::Introduce { auth_key: [0; 32], blob: Vec::new() }.encode();
        assert_eq!(IntroRequest::parse(&empty), None);
        let big = IntroRequest::Introduce { auth_key: [0; 32], blob: vec![0; MAX_BLOB + 1] }.encode();
        assert_eq!(IntroRequest::parse(&big), None);
        let edge = IntroRequest::Introduce { auth_key: [0; 32], blob: vec![0; MAX_BLOB] }.encode();
        assert!(edge.len() <= MAX_BODY && IntroRequest::parse(&edge).is_some());
    }

    #[test]
    fn a_service_proves_the_key_and_receives_introductions() {
        let (mut point, signer, now) = (IntroPoint::<u32>::new(), key(), Instant::now());
        let (state, reply) = establish(&mut point, &signer, 1, 42, now);
        assert_eq!(reply, IntroReply::Established);
        assert!(matches!(state, IntroConn::Serving { .. }));
        let mut caller = IntroConn::Fresh;
        let request = IntroRequest::Introduce { auth_key: signer.verifying_key().to_bytes(), blob: vec![1, 2, 3] };
        assert_eq!(point.handle(&mut caller, 2, &0, request, now, [0; 32]), IntroAction::Forward(42, vec![1, 2, 3]));
    }

    #[test]
    fn a_wrong_proof_is_refused_and_the_challenge_is_spent() {
        let (mut point, signer, now) = (IntroPoint::<u32>::new(), key(), Instant::now());
        let auth_key = signer.verifying_key().to_bytes();
        let mut state = IntroConn::Fresh;
        point.handle(&mut state, 1, &1, IntroRequest::Establish { auth_key }, now, [9; 32]);
        // Assinou o desafio de outro circuito (replay).
        let replay = signer.sign(&proof_message(&[8; 32], &auth_key)).to_bytes();
        let action = point.handle(&mut state, 1, &1, IntroRequest::Prove { signature: replay }, now, [0; 32]);
        assert_eq!(action, IntroAction::Reply(IntroReply::Refused));
        assert!(point.is_empty());
        // A prova certa depois do erro também falha: o desafio vale uma vez.
        let right = signer.sign(&proof_message(&[9; 32], &auth_key)).to_bytes();
        assert_eq!(point.handle(&mut state, 1, &1, IntroRequest::Prove { signature: right }, now, [0; 32]), IntroAction::Reply(IntroReply::Refused));
    }

    #[test]
    fn someone_without_the_key_cannot_take_over_a_service() {
        let (mut point, owner, thief, now) = (IntroPoint::<u32>::new(), key(), key(), Instant::now());
        establish(&mut point, &owner, 1, 10, now);
        let auth_key = owner.verifying_key().to_bytes();
        let mut state = IntroConn::Fresh;
        point.handle(&mut state, 2, &20, IntroRequest::Establish { auth_key }, now, [5; 32]);
        let forged = thief.sign(&proof_message(&[5; 32], &auth_key)).to_bytes();
        assert_eq!(point.handle(&mut state, 2, &20, IntroRequest::Prove { signature: forged }, now, [0; 32]), IntroAction::Reply(IntroReply::Refused));
        let action = point.handle(&mut IntroConn::Fresh, 3, &0, IntroRequest::Introduce { auth_key, blob: vec![1] }, now, [0; 32]);
        assert_eq!(action, IntroAction::Forward(10, vec![1]), "o dono continua registrado");
    }

    #[test]
    fn a_proof_without_a_challenge_is_refused() {
        let mut point = IntroPoint::<u32>::new();
        let action = point.handle(&mut IntroConn::Fresh, 1, &1, IntroRequest::Prove { signature: [0; 64] }, Instant::now(), [0; 32]);
        assert_eq!(action, IntroAction::Reply(IntroReply::Refused));
    }

    #[test]
    fn an_unknown_service_is_refused() {
        let mut point = IntroPoint::<u32>::new();
        let action = point.handle(&mut IntroConn::Fresh, 1, &1, IntroRequest::Introduce { auth_key: [1; 32], blob: vec![1] }, Instant::now(), [0; 32]);
        assert_eq!(action, IntroAction::Reply(IntroReply::Refused));
    }

    #[test]
    fn a_reconnect_replaces_the_old_circuit_and_the_old_one_leaving_keeps_the_new() {
        let (mut point, signer, now) = (IntroPoint::<u32>::new(), key(), Instant::now());
        let (old, _) = establish(&mut point, &signer, 1, 10, now);
        let (new, _) = establish(&mut point, &signer, 2, 20, now);
        point.leave(&old, 1);
        let auth_key = signer.verifying_key().to_bytes();
        let action = point.handle(&mut IntroConn::Fresh, 3, &0, IntroRequest::Introduce { auth_key, blob: vec![1] }, now, [0; 32]);
        assert_eq!(action, IntroAction::Forward(20, vec![1]));
        point.leave(&new, 2);
        assert!(point.is_empty(), "quem registrou por último, ao sair, libera a chave");
    }

    #[test]
    fn introductions_per_service_are_limited_and_the_window_resets() {
        let (mut point, signer, now) = (IntroPoint::<u32>::new(), key(), Instant::now());
        establish(&mut point, &signer, 1, 1, now);
        let auth_key = signer.verifying_key().to_bytes();
        let ask = |point: &mut IntroPoint<u32>, at| point.handle(&mut IntroConn::Fresh, 9, &0, IntroRequest::Introduce { auth_key, blob: vec![1] }, at, [0; 32]);
        for _ in 0..INTRODUCE_PER_SERVICE {
            assert!(matches!(ask(&mut point, now), IntroAction::Forward(..)));
        }
        assert_eq!(ask(&mut point, now), IntroAction::Reply(IntroReply::Refused));
        assert!(matches!(ask(&mut point, now + Duration::from_secs(61)), IntroAction::Forward(..)));
    }

    #[test]
    fn the_service_table_is_bounded() {
        let (mut point, now) = (IntroPoint::<u32>::new(), Instant::now());
        for i in 0..MAX_SERVICES {
            let (_, reply) = establish(&mut point, &key(), i as u64, 0, now);
            assert_eq!(reply, IntroReply::Established, "serviço {i}");
        }
        let (_, reply) = establish(&mut point, &key(), 9999, 0, now);
        assert_eq!(reply, IntroReply::Refused);
    }

    #[test]
    fn a_cookie_is_single_use() {
        let (mut point, now) = (RvPoint::<&str>::new(), Instant::now());
        assert!(point.establish([1; 32], "c", now));
        assert_eq!(point.join(&[1; 32], now), Some("c"));
        assert_eq!(point.join(&[1; 32], now), None, "o segundo Join não acha nada");
        assert!(point.is_empty());
    }

    #[test]
    fn a_repeated_cookie_does_not_take_the_place_of_the_first() {
        let (mut point, now) = (RvPoint::<&str>::new(), Instant::now());
        assert!(point.establish([1; 32], "first", now));
        assert!(!point.establish([1; 32], "second", now));
        assert_eq!(point.join(&[1; 32], now), Some("first"));
    }

    #[test]
    fn an_unknown_cookie_joins_nothing() {
        let mut point = RvPoint::<&str>::new();
        assert_eq!(point.join(&[9; 32], Instant::now()), None);
    }

    #[test]
    fn a_cookie_expires() {
        let (mut point, now) = (RvPoint::<&str>::new(), Instant::now());
        point.establish([1; 32], "c", now);
        assert_eq!(point.join(&[1; 32], now + RV_TTL), None);
        assert!(point.is_empty());
        assert!(point.establish([1; 32], "again", now + RV_TTL), "vencido, o cookie pode voltar");
    }

    #[test]
    fn the_waiting_table_is_bounded_and_expiry_frees_room() {
        let (mut point, now) = (RvPoint::<u32>::new(), Instant::now());
        for i in 0..MAX_WAITING {
            let mut cookie = [0u8; 32];
            cookie[..8].copy_from_slice(&(i as u64).to_be_bytes());
            assert!(point.establish(cookie, 0, now));
        }
        assert!(!point.establish([0xff; 32], 0, now));
        assert!(point.establish([0xff; 32], 0, now + RV_TTL));
    }

    #[test]
    fn requests_per_minute_are_capped() {
        let (mut point, now) = (RvPoint::<u32>::new(), Instant::now());
        let admitted = (0..REQUESTS_PER_MINUTE + 10).filter(|_| point.admit(now)).count();
        assert_eq!(admitted as u32, REQUESTS_PER_MINUTE);
        assert!(point.admit(now + Duration::from_secs(61)));
    }

    // Adaptadores em loopback, com os mesmos quadros que o relay de saída entregaria.

    async fn ask(stream: &mut TcpStream, body: &[u8]) -> Vec<u8> {
        assert!(send(stream, body).await);
        next(stream).await
    }

    async fn next(stream: &mut TcpStream) -> Vec<u8> {
        let mut messages = Messages::new();
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(5), read_chunk(stream)).await.expect("prazo").expect("conexão aberta");
            if let Some(body) = messages.push(&chunk).expect("quadro válido").pop() {
                return body;
            }
        }
    }

    async fn local<F, Fut>(serve: F) -> std::net::SocketAddr
    where
        F: FnOnce(TcpListener) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("porta");
        let addr = listener.local_addr().expect("endereço");
        tokio::spawn(serve(listener));
        addr
    }

    #[tokio::test]
    async fn the_introduction_reaches_the_waiting_circuit_unread() {
        let addr = local(serve_intro).await;
        let signer = key();
        let auth_key = signer.verifying_key().to_bytes();
        let mut waiting = TcpStream::connect(addr).await.expect("conecta");
        let reply = ask(&mut waiting, &IntroRequest::Establish { auth_key }.encode()).await;
        let Some(IntroReply::Challenge(nonce)) = IntroReply::parse(&reply) else { panic!("sem desafio") };
        let signature = signer.sign(&proof_message(&nonce, &auth_key)).to_bytes();
        let reply = ask(&mut waiting, &IntroRequest::Prove { signature }.encode()).await;
        assert_eq!(IntroReply::parse(&reply), Some(IntroReply::Established));

        let mut caller = TcpStream::connect(addr).await.expect("conecta");
        let blob = vec![0xab; MAX_BLOB];
        let reply = ask(&mut caller, &IntroRequest::Introduce { auth_key, blob: blob.clone() }.encode()).await;
        assert_eq!(IntroReply::parse(&reply), Some(IntroReply::Delivered));
        assert_eq!(IntroReply::parse(&next(&mut waiting).await), Some(IntroReply::Push(blob)));

        let reply = ask(&mut caller, &IntroRequest::Introduce { auth_key: [1; 32], blob: vec![1] }.encode()).await;
        assert_eq!(IntroReply::parse(&reply), Some(IntroReply::Refused), "chave que ninguém registrou");
    }

    #[tokio::test]
    async fn the_service_goes_away_with_its_circuit() {
        let addr = local(serve_intro).await;
        let signer = key();
        let auth_key = signer.verifying_key().to_bytes();
        let mut waiting = TcpStream::connect(addr).await.expect("conecta");
        let Some(IntroReply::Challenge(nonce)) = IntroReply::parse(&ask(&mut waiting, &IntroRequest::Establish { auth_key }.encode()).await) else {
            panic!("sem desafio")
        };
        let signature = signer.sign(&proof_message(&nonce, &auth_key)).to_bytes();
        ask(&mut waiting, &IntroRequest::Prove { signature }.encode()).await;
        drop(waiting);
        let mut caller = TcpStream::connect(addr).await.expect("conecta");
        // Espera por condição, com prazo: o ponto percebe o fechamento de forma assíncrona.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let reply = ask(&mut caller, &IntroRequest::Introduce { auth_key, blob: vec![1] }.encode()).await;
            if IntroReply::parse(&reply) == Some(IntroReply::Refused) {
                break;
            }
            assert!(Instant::now() < deadline, "o registro não saiu depois que o circuito fechou");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn garbage_closes_the_connection() {
        let addr = local(serve_intro).await;
        let mut stream = TcpStream::connect(addr).await.expect("conecta");
        assert!(send(&mut stream, &[0xee, 1, 2]).await);
        let closed = tokio::time::timeout(Duration::from_secs(5), read_chunk(&mut stream)).await.expect("prazo");
        assert!(closed.is_none());
    }

    #[tokio::test]
    async fn the_rendezvous_joins_two_circuits_and_copies_bytes_both_ways() {
        let addr = local(serve_rendezvous).await;
        let cookie = [0x42; 32];
        let mut caller = TcpStream::connect(addr).await.expect("conecta");
        assert_eq!(RvReply::parse(&ask(&mut caller, &RvRequest::Establish { cookie }.encode()).await), Some(RvReply::Waiting));
        let mut callee = TcpStream::connect(addr).await.expect("conecta");
        assert_eq!(RvReply::parse(&ask(&mut callee, &RvRequest::Join { cookie }.encode()).await), Some(RvReply::Joined));
        assert_eq!(RvReply::parse(&next(&mut caller).await), Some(RvReply::Joined));
        caller.write_all(b"de C para D").await.expect("escreve");
        callee.write_all(b"de D para C").await.expect("escreve");
        let (mut at_callee, mut at_caller) = ([0u8; 11], [0u8; 11]);
        tokio::time::timeout(Duration::from_secs(5), callee.read_exact(&mut at_callee)).await.expect("prazo").expect("lê");
        tokio::time::timeout(Duration::from_secs(5), caller.read_exact(&mut at_caller)).await.expect("prazo").expect("lê");
        assert_eq!((&at_callee, &at_caller), (b"de C para D", b"de D para C"));
    }

    #[tokio::test]
    async fn a_join_with_an_unknown_or_used_cookie_is_refused() {
        let addr = local(serve_rendezvous).await;
        let mut stranger = TcpStream::connect(addr).await.expect("conecta");
        assert_eq!(RvReply::parse(&ask(&mut stranger, &RvRequest::Join { cookie: [1; 32] }.encode()).await), Some(RvReply::Refused));

        let cookie = [2; 32];
        let mut caller = TcpStream::connect(addr).await.expect("conecta");
        ask(&mut caller, &RvRequest::Establish { cookie }.encode()).await;
        let mut callee = TcpStream::connect(addr).await.expect("conecta");
        ask(&mut callee, &RvRequest::Join { cookie }.encode()).await;
        let mut late = TcpStream::connect(addr).await.expect("conecta");
        assert_eq!(RvReply::parse(&ask(&mut late, &RvRequest::Join { cookie }.encode()).await), Some(RvReply::Refused), "cookie de uso único");
    }

    #[tokio::test]
    async fn a_second_establish_with_the_same_cookie_is_refused() {
        let addr = local(serve_rendezvous).await;
        let cookie = [3; 32];
        let mut first = TcpStream::connect(addr).await.expect("conecta");
        assert_eq!(RvReply::parse(&ask(&mut first, &RvRequest::Establish { cookie }.encode()).await), Some(RvReply::Waiting));
        let mut second = TcpStream::connect(addr).await.expect("conecta");
        assert_eq!(RvReply::parse(&ask(&mut second, &RvRequest::Establish { cookie }.encode()).await), Some(RvReply::Refused));
    }
}
