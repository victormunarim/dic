//! Ponto de encontro na DHT (feature `net`): onde dois contatos que mudaram de endereço se reencontram.
//!
//! Cada dono guarda o próprio registro de endereço, cifrado para o contato ([`crate::media::update::seal_record`]),
//! num item mutável da Mainline DHT do BitTorrent (BEP 44). O alvo do item vem do par de chaves e do dia
//! ([`crate::media::update::rendezvous`]), então só os dois lados do par sabem onde procurar, e o alvo
//! muda sozinho a cada dia. Nenhum servidor participa.
//!
//! Quem guarda o item na DHT vê o IP de quem escreve e o de quem lê; o conteúdo só abre com a chave do par.
//! Itens somem da DHT em cerca de 2 h sem renovação, então o depósito só existe enquanto o dono está online.
//!
//! # Pelo circuito
//!
//! Falando direto com a DHT, quem guarda o item vê o IP de quem escreve e o de quem lê. Com
//! [`DhtOptions::circuit`], o nó pede a um relay de saída de um circuito de 2 saltos que fale com a DHT por ele
//! (o *gateway*, [`GATEWAY_DEST`]): o primeiro salto vê o IP mas não o pedido, a saída vê o pedido mas não o IP, e a
//! DHT só vê a saída. Todo nó com a DHT ligada serve de gateway para os outros.
//!
//! # Âncoras
//!
//! Para quem a DHT está bloqueada ou não basta, um nó com endereço estável pode guardar os registros dos outros:
//! a *âncora* ([`Anchor`], `NodeConfig::anchor`). Voluntário e federado: cada pessoa
//! escolhe em quais âncoras confia para estar no ar, e quem opera uma não precisa ser ninguém especial. O dono grava o
//! mesmo item assinado da DHT (mesmo alvo, mesma cifra) em cada âncora que ele usa (as réplicas), e quem liga lê as
//! mesmas três datas. A âncora fala o protocolo do gateway pelo destino [`ANCHOR_DEST`]; só guarda, não consulta nada
//! fora, e não lê o conteúdo nem forja item (confere a assinatura de cada gravação).
//!
//! O gateway não é um proxy aberto: só entende dois pedidos, ler o alvo `(chave, sal)` e gravar um item **já
//! assinado** por quem pediu (a semente de assinatura nunca sai do nó), com valor pequeno e limite de pedidos por
//! minuto. O gateway não lê o valor (é cifrado para o contato) e não consegue forjar um item.
//!
//! Formato: o pedido e a resposta são `u16 BE tamanho ‖ corpo`, em pedaços de [`CHUNK`] bytes, um por célula do circuito.
//! Pedidos: `1 ‖ chave (32) ‖ sal (32)` (ler) e `2 ‖ chave (32) ‖ assinatura (64) ‖ seq (8) ‖ sal (32) ‖ valor` (gravar).
//! Respostas: `1 ‖ valor` ou `1` (achou/gravou) e `0` (não achou/falhou).

// A DHT e as âncoras entregam bytes de desconhecidos: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use crate::{bytes::Reader, sync::LockExt};
use std::{
    collections::HashMap,
    io,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use mainline::{Dht, MutableItem, SigningKey, async_dht::AsyncDht};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

use crate::{circuit::manager::Event, media::update::RECORD_LEN, transport::CircuitClient};

/// Destino que o relay de saída mapeia para o gateway da DHT (veja o módulo).
pub const GATEWAY_DEST: &str = "dht:9";

/// Destino que o relay de saída mapeia para a âncora (veja o módulo).
pub const ANCHOR_DEST: &str = "anchor:9";

/// Bytes de um pedido ou resposta por célula do circuito (cabem em `circuit::max_data(1)` = 203).
pub const CHUNK: usize = 200;

/// Maior pedido aceito pelo gateway (corpo, sem o tamanho).
const MAX_REQUEST: usize = 400;

/// Maior resposta aceita de um gateway (corpo, sem o tamanho).
const MAX_REPLY: usize = 1100;

/// Pedidos que um gateway atende por minuto, somando todos os circuitos.
const GATEWAY_PER_MINUTE: u32 = 60;

/// Pedidos que uma âncora atende por minuto: ela serve a todos os contatos de quem a usa.
const ANCHOR_PER_MINUTE: u32 = 600;

/// Pedidos que um gateway atende ao mesmo tempo.
const GATEWAY_PARALLEL: usize = 4;

/// Itens que uma âncora guarda; passando disto sai o mais antigo.
pub const ANCHOR_ITEMS: usize = 2048;

/// Quanto tempo uma âncora guarda um item sem gravação nova (o dono grava a cada hora).
pub const ANCHOR_KEEP: Duration = Duration::from_secs(3 * 24 * 3600);

/// Quanto esperar a resposta de um gateway a uma leitura e a uma gravação.
const REMOTE_GET_WAIT: Duration = Duration::from_secs(15);
const REMOTE_PUT_WAIT: Duration = Duration::from_secs(30);

const REQ_GET: u8 = 1;
const REQ_PUT: u8 = 2;

/// Quanto esperar a DHT gravar um item.
const PUT_WAIT: Duration = Duration::from_secs(20);

/// Quanto esperar a DHT devolver os itens de um alvo.
const GET_WAIT: Duration = Duration::from_secs(10);

/// Como o nó entra na DHT.
#[derive(Clone, Debug, Default)]
pub struct DhtOptions {
    /// Nós iniciais (`host:porta`). Vazio: os nós públicos da Mainline.
    pub bootstrap: Vec<String>,
    /// Endereço local do socket UDP da DHT. `None`: qualquer interface (nos testes, loopback).
    pub bind: Option<Ipv4Addr>,
    /// Gravar e ler pelos relays de dois contatos (veja o módulo) em vez de falar com a DHT do próprio IP.
    /// Precisa de ao menos dois relays de contatos; sem eles a DHT não é usada, nunca cai para o acesso direto.
    pub circuit: bool,
}

/// Item mutável já assinado, pronto para ir à DHT sem a semente de quem assinou.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub key: [u8; 32],
    pub signature: [u8; 64],
    pub seq: i64,
    pub salt: [u8; 32],
    pub value: Vec<u8>,
}

/// Chave pública ed25519 da semente.
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    signer(seed).verifying_key().to_bytes()
}

/// Assina `value` com a chave da semente para o alvo `(chave, salt)`.
pub fn sign(seed: &[u8; 32], salt: &[u8; 32], value: &[u8], seq: i64) -> Signed {
    let item = MutableItem::new(signer(seed), value, seq, Some(salt));
    Signed { key: *item.key(), signature: *item.signature(), seq, salt: *salt, value: value.to_vec() }
}

impl Signed {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![REQ_PUT];
        out.extend_from_slice(&self.key);
        out.extend_from_slice(&self.signature);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&self.value);
        out
    }

    /// A assinatura confere com a chave, o sal e o valor (a mesma conta que a DHT faz).
    pub fn verify(&self) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(&self.key) else { return false };
        let mut signable = format!("4:salt{}:", self.salt.len()).into_bytes();
        signable.extend_from_slice(&self.salt);
        signable.extend(format!("3:seqi{}e1:v{}:", self.seq, self.value.len()).into_bytes());
        signable.extend_from_slice(&self.value);
        key.verify(&signable, &Signature::from_bytes(&self.signature)).is_ok()
    }

    fn decode(body: &[u8]) -> Option<Self> {
        let mut r = Reader::new(body);
        if r.u8()? != REQ_PUT {
            return None;
        }
        let (key, signature) = (r.array()?, r.array()?);
        let seq = i64::from_be_bytes(r.array()?);
        let salt = r.array()?;
        // Sem valor não há o que guardar.
        let value = r.rest();
        if value.is_empty() {
            return None;
        }
        Some(Self { key, signature, seq, salt, value: value.to_vec() })
    }
}

/// Cliente da DHT: grava e lê os itens do ponto de encontro.
pub struct Rendezvous {
    dht: AsyncDht,
}

fn signer(seed: &[u8; 32]) -> SigningKey {
    SigningKey::from_bytes(seed)
}

impl Rendezvous {
    /// Entra na DHT. Resolver os nós iniciais pode bloquear, então roda fora do executor.
    pub async fn start(options: &DhtOptions) -> io::Result<Self> {
        let options = options.clone();
        let dht = tokio::task::spawn_blocking(move || {
            let mut builder = Dht::builder();
            if !options.bootstrap.is_empty() {
                builder.bootstrap(&options.bootstrap);
            }
            if let Some(bind) = options.bind {
                builder.bind_address(bind);
            }
            builder.build()
        })
        .await
        .map_err(io::Error::other)??;
        Ok(Self { dht: dht.as_async() })
    }

    /// Grava `value` no alvo `(chave derivada de seed, salt)`. `seq` tem de crescer a cada gravação nova
    /// do mesmo alvo, porque a DHT recusa o mesmo `seq` com outro valor.
    pub async fn publish(&self, seed: &[u8; 32], salt: &[u8; 32], value: &[u8], seq: i64) -> Result<(), String> {
        self.put(&sign(seed, salt, value, seq)).await
    }

    /// Grava um item que outro nó assinou.
    pub async fn put(&self, signed: &Signed) -> Result<(), String> {
        let item = MutableItem::new_signed_unchecked(signed.key, signed.signature, &signed.value, signed.seq, Some(&signed.salt));
        match tokio::time::timeout(PUT_WAIT, self.dht.put_mutable(item, None)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("a DHT não respondeu a tempo".into()),
        }
    }

    /// Valor mais recente guardado no alvo `(chave derivada de seed, salt)`, se houver.
    pub async fn lookup(&self, seed: &[u8; 32], salt: &[u8; 32]) -> Option<Vec<u8>> {
        self.get(&public_key(seed), salt).await
    }

    /// Valor mais recente guardado no alvo `(key, salt)`, se houver.
    pub async fn get(&self, key: &[u8; 32], salt: &[u8; 32]) -> Option<Vec<u8>> {
        let item = tokio::time::timeout(GET_WAIT, self.dht.get_mutable_most_recent(key, Some(salt))).await.ok()??;
        Some(item.value().to_vec())
    }
}

// ---------------------------------------------------------------------------
// Gateway e âncora (lado da saída)
// ---------------------------------------------------------------------------

/// Onde o gateway lê e grava: a DHT ([`Rendezvous`]) ou o depósito de uma âncora ([`Anchor`]).
pub trait Store: Send + Sync + 'static {
    fn read(&self, key: &[u8; 32], salt: &[u8; 32]) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send;
    /// `true`: o item foi guardado.
    fn write(&self, item: &Signed) -> impl std::future::Future<Output = bool> + Send;
}

impl Store for Rendezvous {
    async fn read(&self, key: &[u8; 32], salt: &[u8; 32]) -> Option<Vec<u8>> {
        self.get(key, salt).await
    }

    async fn write(&self, item: &Signed) -> bool {
        self.put(item).await.is_ok()
    }
}

/// Depósito de uma âncora: guarda em memória, por alvo, o item de `seq` mais alto que recebeu.
///
/// Só aceita o que prova ser de quem tem a chave do alvo (assinatura), no tamanho de um registro cifrado, e só
/// substitui por `seq` maior. Passando de [`ANCHOR_ITEMS`] sai o expirado ou, na falta, o mais antigo; quem
/// encher a âncora de itens falsos pode empurrar os verdadeiros para fora, e o dono repõe o seu a cada hora.
#[derive(Default)]
pub struct Anchor {
    items: Mutex<Stored>,
}

/// Por alvo `(chave, sal)`: o item e quando chegou.
type Stored = HashMap<([u8; 32], [u8; 32]), (Signed, Instant)>;

impl Anchor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Itens guardados agora (sem contar os expirados que ainda não saíram).
    pub fn len(&self) -> usize {
        self.items.lock_unpoisoned().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn store(&self, item: &Signed, now: Instant) -> bool {
        if item.value.len() != RECORD_LEN || !item.verify() {
            return false;
        }
        let mut items = self.items.lock_unpoisoned();
        let target = (item.key, item.salt);
        if items.get(&target).is_some_and(|(old, at)| at.elapsed() < ANCHOR_KEEP && old.seq >= item.seq) {
            return false;
        }
        if !items.contains_key(&target) && items.len() >= ANCHOR_ITEMS {
            items.retain(|_, (_, at)| at.elapsed() < ANCHOR_KEEP);
            if items.len() >= ANCHOR_ITEMS
                && let Some(oldest) = items.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| *k)
            {
                items.remove(&oldest);
            }
        }
        items.insert(target, (item.clone(), now));
        true
    }
}

impl Store for Anchor {
    async fn read(&self, key: &[u8; 32], salt: &[u8; 32]) -> Option<Vec<u8>> {
        let items = self.items.lock_unpoisoned();
        items.get(&(*key, *salt)).filter(|(_, at)| at.elapsed() < ANCHOR_KEEP).map(|(item, _)| item.value.clone())
    }

    async fn write(&self, item: &Signed) -> bool {
        self.store(item, Instant::now())
    }
}

/// Teto de pedidos por minuto, somado entre os circuitos.
struct Budget {
    window: Mutex<(Instant, u32)>,
    per_minute: u32,
}

impl Budget {
    fn new(per_minute: u32) -> Self {
        Self { window: Mutex::new((Instant::now(), 0)), per_minute }
    }

    fn take(&self) -> bool {
        let mut b = self.window.lock_unpoisoned();
        if b.0.elapsed() >= Duration::from_secs(60) {
            *b = (Instant::now(), 0);
        }
        b.1 += 1;
        b.1 <= self.per_minute
    }
}

/// Atende os circuitos que o relay de saída encaminha a `listener` (veja [`GATEWAY_DEST`]) com a DHT de `dht`.
pub async fn serve_gateway(listener: TcpListener, dht: Arc<Rendezvous>) {
    serve(listener, dht, GATEWAY_PER_MINUTE).await;
}

/// Atende os circuitos que o relay de saída encaminha a `listener` (veja [`ANCHOR_DEST`]) com o depósito de `anchor`.
pub async fn serve_anchor(listener: TcpListener, anchor: Arc<Anchor>) {
    serve(listener, anchor, ANCHOR_PER_MINUTE).await;
}

async fn serve<S: Store>(listener: TcpListener, store: Arc<S>, per_minute: u32) {
    let budget = Arc::new(Budget::new(per_minute));
    let parallel = Arc::new(Semaphore::new(GATEWAY_PARALLEL));
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(gateway_conn(stream, store.clone(), budget.clone(), parallel.clone()));
    }
}

/// Uma conexão é um circuito: lê quadros `[u16 tamanho][pedaço]`, junta os pedaços em pedidos e responde na ordem.
async fn gateway_conn<S: Store>(mut stream: TcpStream, store: Arc<S>, budget: Arc<Budget>, parallel: Arc<Semaphore>) {
    let mut acc: Vec<u8> = Vec::new();
    let mut len = [0u8; 2];
    while stream.read_exact(&mut len).await.is_ok() {
        let mut chunk = vec![0u8; u16::from_be_bytes(len) as usize];
        if stream.read_exact(&mut chunk).await.is_err() {
            return;
        }
        acc.extend_from_slice(&chunk);
        while acc.len() >= 2 {
            let total = Reader::new(&acc).u16().map_or(0, usize::from);
            if total == 0 || total > MAX_REQUEST {
                return;
            }
            if acc.len() < 2 + total {
                break;
            }
            let body: Vec<u8> = acc.drain(..2 + total).skip(2).collect();
            let reply = match parallel.acquire().await {
                Ok(_slot) if budget.take() => answer(&*store, &body).await,
                _ => vec![0],
            };
            let mut message = (reply.len() as u16).to_be_bytes().to_vec();
            message.extend_from_slice(&reply);
            for piece in message.chunks(CHUNK) {
                let mut framed = (piece.len() as u16).to_be_bytes().to_vec();
                framed.extend_from_slice(piece);
                if stream.write_all(&framed).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn answer<S: Store>(store: &S, body: &[u8]) -> Vec<u8> {
    match body.first() {
        Some(&REQ_GET) if body.len() == 1 + 32 + 32 => {
            let mut r = Reader::new(body);
            r.u8();
            let (Some(key), Some(salt)) = (r.array(), r.array()) else { return vec![0] };
            match store.read(&key, &salt).await {
                Some(value) => [&[1u8][..], &value].concat(),
                None => vec![0],
            }
        }
        Some(&REQ_PUT) => match Signed::decode(body) {
            Some(signed) if store.write(&signed).await => vec![1],
            _ => vec![0],
        },
        _ => vec![0],
    }
}

// ---------------------------------------------------------------------------
// Gateway (lado de quem pede)
// ---------------------------------------------------------------------------

/// O gateway não respondeu a tempo, ou respondeu fora do formato: a sessão não serve mais.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayLost;

/// Sessão com um gateway por um circuito já pronto até a saída.
pub struct Remote {
    client: CircuitClient,
}

impl Remote {
    pub fn new(client: CircuitClient) -> Self {
        Self { client }
    }

    /// Um pedido e a resposta. `None`: o circuito não respondeu a tempo ou a resposta veio fora do formato.
    async fn call(&mut self, body: &[u8], wait: Duration) -> Option<Vec<u8>> {
        let mut message = (body.len() as u16).to_be_bytes().to_vec();
        message.extend_from_slice(body);
        for piece in message.chunks(CHUNK) {
            self.client.send(piece).await.ok()?;
        }
        tokio::time::timeout(wait, async {
            let mut acc: Vec<u8> = Vec::new();
            loop {
                if let Event::Data(d) = self.client.next_event().await? {
                    acc.extend_from_slice(&d);
                    if let Some(total) = Reader::new(&acc).u16().map(usize::from) {
                        if total == 0 || total > MAX_REPLY {
                            return None;
                        }
                        if let Some(reply) = acc.get(2..2 + total) {
                            return Some(reply.to_vec());
                        }
                    }
                }
            }
        })
        .await
        .ok()?
    }

    /// Lê o alvo `(key, salt)`. `Err`: o gateway não respondeu (a sessão não serve mais); `Ok(None)`: não há item.
    pub async fn get(&mut self, key: &[u8; 32], salt: &[u8; 32]) -> Result<Option<Vec<u8>>, GatewayLost> {
        let mut body = vec![REQ_GET];
        body.extend_from_slice(key);
        body.extend_from_slice(salt);
        match self.call(&body, REMOTE_GET_WAIT).await.as_deref() {
            Some([1, value @ ..]) => Ok(Some(value.to_vec())),
            Some([0]) => Ok(None),
            _ => Err(GatewayLost),
        }
    }

    /// Grava `signed`. `Err`: o gateway não respondeu; `Ok(false)`: respondeu que não gravou.
    pub async fn put(&mut self, signed: &Signed) -> Result<bool, GatewayLost> {
        match self.call(&signed.encode(), REMOTE_PUT_WAIT).await.as_deref() {
            Some([1]) => Ok(true),
            Some([0]) => Ok(false),
            _ => Err(GatewayLost),
        }
    }

    pub fn stop(self) {
        self.client.stop();
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_item_survives_the_wire_and_carries_no_seed() {
        let seed = [7u8; 32];
        let signed = sign(&seed, &[9; 32], &[5; 162], 1234);
        assert_eq!(signed.key, public_key(&seed));
        let body = signed.encode();
        assert!(body.len() <= MAX_REQUEST, "cabe no limite do gateway: {}", body.len());
        assert!(!body.windows(32).any(|w| w == seed), "a semente não vai no pedido");
        assert_eq!(Signed::decode(&body), Some(signed));
        assert!(Signed::decode(&body[..100]).is_none());
        assert!(Signed::decode(&[REQ_GET, 0]).is_none());
    }

    #[test]
    fn the_signature_is_the_one_the_dht_checks() {
        let signed = sign(&[7; 32], &[9; 32], b"valor", 5);
        let again = MutableItem::new(signer(&[7; 32]), b"valor", 5, Some(&[9; 32]));
        assert_eq!(&signed.signature, again.signature());
    }

    #[test]
    fn the_budget_runs_out_within_a_minute() {
        let budget = Budget::new(GATEWAY_PER_MINUTE);
        assert!((0..GATEWAY_PER_MINUTE).all(|_| budget.take()));
        assert!(!budget.take());
    }

    fn record(secret: &[u8; 32], seq: i64) -> Signed {
        sign(secret, &[9; 32], &[3; RECORD_LEN], seq)
    }

    #[test]
    fn a_signed_item_verifies_and_a_changed_one_does_not() {
        let item = record(&[7; 32], 5);
        assert!(item.verify());
        assert!(!Signed { seq: 6, ..item.clone() }.verify());
        assert!(!Signed { value: vec![4; RECORD_LEN], ..item }.verify());
        assert!(!Signed { key: [8; 32], ..item }.verify());
    }

    #[test]
    fn the_anchor_keeps_only_signed_records_and_only_newer_ones() {
        let anchor = Anchor::new();
        let now = Instant::now();
        assert!(anchor.store(&record(&[7; 32], 5), now));
        assert!(!anchor.store(&record(&[7; 32], 5), now), "mesmo seq");
        assert!(!anchor.store(&record(&[7; 32], 4), now), "seq menor");
        assert!(anchor.store(&record(&[7; 32], 6), now));
        let forged = Signed { value: vec![4; RECORD_LEN], ..record(&[7; 32], 7) };
        assert!(!anchor.store(&forged, now), "assinatura não confere");
        let short = sign(&[7; 32], &[9; 32], b"curto", 9);
        assert!(!anchor.store(&short, now), "tamanho de registro");
        assert_eq!(anchor.len(), 1);
    }

    #[test]
    fn a_full_anchor_drops_the_oldest_item() {
        let anchor = Anchor::new();
        for i in 0..ANCHOR_ITEMS {
            let mut secret = [1; 32];
            secret[..8].copy_from_slice(&(i as u64).to_be_bytes());
            assert!(anchor.store(&record(&secret, 1), Instant::now()));
        }
        assert_eq!(anchor.len(), ANCHOR_ITEMS);
        assert!(anchor.store(&record(&[2; 32], 1), Instant::now()));
        assert_eq!(anchor.len(), ANCHOR_ITEMS);
    }
}
