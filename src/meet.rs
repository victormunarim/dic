//! Introdução e encontro às cegas: quem liga (C) alcança quem atende (D) sem saber o IP de D, e D sem saber o de C
//! (camada B de `docs/DOIS-RELAYS.md`, etapa 3). Usa os serviços `intro:9` e `rv:9` dos relays ([`crate::hidden`]).
//!
//! ```text
//!   D ══ circuito de espera ══> I (ponto de introdução)        D guarda só a auth_key e o relay I
//!   C ══ circuito ══> M (encontro)   Establish(cookie)         C escolhe M e um cookie novo
//!   C ══ circuito ══> I             Introduce(auth_key, blob)  blob = [M, cookie] cifrado só para D
//!   I ── Push(blob) ──> D                                      I não lê o blob e não vê o IP de D
//!   D ══ circuito ══> M             Join(cookie)               M liga os dois circuitos
//!   C <═════════ chamada normal, cifrada de ponta a ponta ═════════> D
//! ```
//!
//! Todo circuito aqui é de 2 saltos com o primeiro salto num relay de terceiro (`pool`): quem fala com I ou M é o
//! relay de entrada, nunca C nem D. Sem relay de terceiro o circuito não monta; não existe queda para o caminho que
//! mostra o IP.
//!
//! - **Blob** ([`seal_meeting`], [`open_meeting`]): efêmera X25519 do remetente → chave de D, então só D abre; o
//!   ponto de introdução não aprende quem liga. Dentro vão a chave de C, o carimbo, o `cookie` e o relay de encontro,
//!   mais uma prova de autoria (HKDF do segredo estático C↔D, comparada em tempo constante): quem não tem a chave
//!   privada de C não forja uma introdução em nome dele. Tamanho fixo ([`MEETING_LEN`]).
//! - **Quem decide é D.** Os pontos não conhecem a lista de contatos. D descarta, em silêncio, introdução de chave
//!   fora da lista, de carimbo velho ou de `cookie` repetido, e limita quantos encontros faz ao mesmo tempo.
//! - Depois do `Joined` o circuito de C é o mesmo que a chamada usa hoje: [`meet`] devolve o [`CircuitClient`] pronto
//!   para `call_linked`. Do lado de D, [`HiddenService`] liga o circuito ao telefone local como o relay de saída faria
//!   (um quadro por pedaço), então o telefone não muda.
//!
//! Só [`open_meeting`] lê bytes da rede neste módulo (alvo de fuzz `meet_blob`); o resto fala por
//! [`crate::hidden`], que já tem os seus.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::{
    collections::{HashSet, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::{Signer, SigningKey};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Semaphore, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    bytes::Reader,
    circuit::manager::{Candidate, CircuitManager, Event, ManagerConfig},
    hidden::{self, CHUNK, INTRO_DEST, IntroReply, IntroRequest, Messages, RV_DEST, RvReply, RvRequest},
    phone::Contacts,
    presence::IntroPointV2,
    sync::LockExt,
    transport::{CircuitClient, ClientOptions},
};

const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
/// Maior endereço de relay no blob (o mesmo limite do aviso de endereço novo).
pub const MAX_ADDR: usize = 60;
const PROOF_LEN: usize = 16;
const PLAIN_LEN: usize = 32 + 8 + 32 + 32 + 32 + 1 + MAX_ADDR + PROOF_LEN;
/// Tamanho do blob de introdução: sempre o mesmo, qualquer que seja o endereço do relay de encontro.
pub const MEETING_LEN: usize = 1 + 32 + NONCE_LEN + PLAIN_LEN + 16;
/// Quanto o carimbo do blob pode diferir do relógio de D (um blob gravado não volta depois).
pub const MEETING_WINDOW_MS: u64 = 120_000;

/// Prazo para um circuito ficar pronto.
const READY_WAIT: Duration = Duration::from_secs(10);
/// Prazo para uma resposta de ponto (`Challenge`, `Established`, `Delivered`, `Waiting`).
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// Quanto C espera D chegar ao encontro depois de entregue a introdução (o ponto descarta o `cookie` em [`hidden::RV_TTL`]).
const JOIN_WAIT: Duration = Duration::from_secs(30);
/// Quanto [`HiddenService::start`] espera os pontos de introdução responderem.
const START_WAIT: Duration = Duration::from_secs(20);
/// Maior espera entre duas tentativas de refazer um circuito de espera.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Encontros que D faz ao mesmo tempo; a introdução que passa disso é descartada.
const MAX_MEETINGS: usize = 4;
/// Quantos `cookie`s recentes D lembra para recusar introdução repetida.
const SEEN_COOKIES: usize = 256;
/// Maior quadro que o bridge aceita do telefone.
const MAX_FRAME: usize = 1200;

#[derive(Debug, thiserror::Error)]
pub enum MeetError {
    #[error("não há relay de terceiro para o primeiro salto: nunca se cai para um caminho que mostre o IP")]
    NoPool,
    #[error("o circuito até o ponto não ficou pronto")]
    NoCircuit,
    #[error("o ponto não respondeu")]
    NoReply,
    #[error("o ponto recusou")]
    Refused,
    #[error("resposta fora do protocolo")]
    Protocol,
    #[error("a introdução não chegou ao encontro a tempo")]
    NoMeeting,
    #[error("endereço de relay fora do limite do blob")]
    BadRelay,
    #[error("chave inválida")]
    BadKey,
    #[error("nenhum ponto de introdução respondeu")]
    NoIntroPoint,
}

// ---------------------------------------------------------------------------
// Blob de introdução (sans-IO)
// ---------------------------------------------------------------------------

/// O que C conta a D dentro do blob.
#[derive(Clone, Debug, PartialEq)]
pub struct Meeting {
    /// Chave de contato de quem liga, **provada** pelo blob.
    pub caller: [u8; 32],
    /// Relay de encontro: onde C espera por D.
    pub rv: Candidate,
    /// Segredo de uso único que liga os dois circuitos no encontro.
    pub cookie: [u8; 32],
}

fn kdf<const N: usize>(salt: &[u8], secret: &[u8], info: &[u8]) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    Hkdf::<Sha256>::new(Some(salt), secret).expand(info, &mut out).ok()?;
    Some(out)
}

/// A prova de autoria: só quem tem uma das duas chaves privadas, C ou D, calcula o segredo estático C↔D.
fn proof(shared: &[u8; 32], eph: &[u8; 32], cookie: &[u8; 32], stamp: u64, to: &[u8; 32]) -> Option<[u8; PROOF_LEN]> {
    let mut info = b"clam/meet/prova/v1".to_vec();
    for part in [&eph[..], cookie, &stamp.to_be_bytes(), to] {
        info.extend_from_slice(part);
    }
    kdf(b"clam/meet/prova/v1", shared, &info)
}

fn cipher(shared: &[u8; 32], to: &[u8; 32], eph: &[u8; 32]) -> Option<ChaCha20Poly1305> {
    let mut info = b"clam/meet/cifra/v1".to_vec();
    info.extend_from_slice(to);
    info.extend_from_slice(eph);
    let key: [u8; 32] = kdf(b"clam/meet/cifra/v1", shared, &info)?;
    Some(ChaCha20Poly1305::new(Key::from_slice(&key)))
}

fn dh(secret: &StaticSecret, public: &PublicKey) -> Option<[u8; 32]> {
    let shared = secret.diffie_hellman(public);
    shared.was_contributory().then(|| *shared.as_bytes())
}

/// O blob de `caller` para `callee`: o relay de encontro `rv` e o `cookie`. `now_ms` é o relógio de C.
///
/// # Errors
///
/// [`MeetError::BadRelay`] se o endereço de `rv` é vazio ou passa de [`MAX_ADDR`]; [`MeetError::BadKey`] se a chave de
/// `callee` não serve para o acordo X25519.
pub fn seal_meeting(caller: &StaticSecret, callee: &PublicKey, rv: &Candidate, cookie: [u8; 32], now_ms: u64) -> Result<Vec<u8>, MeetError> {
    let addr = rv.addr.as_bytes();
    if addr.is_empty() || addr.len() > MAX_ADDR {
        return Err(MeetError::BadRelay);
    }
    let eph = StaticSecret::random_from_rng(OsRng);
    let eph_pub = *PublicKey::from(&eph).as_bytes();
    let to = *callee.as_bytes();
    let (outer, inner) = (dh(&eph, callee).ok_or(MeetError::BadKey)?, dh(caller, callee).ok_or(MeetError::BadKey)?);
    let tag = proof(&inner, &eph_pub, &cookie, now_ms, &to).ok_or(MeetError::BadKey)?;
    let mut padded = [0u8; MAX_ADDR];
    padded.get_mut(..addr.len()).ok_or(MeetError::BadRelay)?.copy_from_slice(addr);
    let mut plain = Vec::with_capacity(PLAIN_LEN);
    for part in [PublicKey::from(caller).as_bytes().as_slice(), &now_ms.to_be_bytes(), &cookie, &rv.public, &rv.link_public, &[addr.len() as u8], &padded, &tag] {
        plain.extend_from_slice(part);
    }
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let aead = cipher(&outer, &to, &eph_pub).ok_or(MeetError::BadKey)?;
    let sealed = aead.encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &[VERSION] }).map_err(|_| MeetError::BadKey)?;
    let mut blob = Vec::with_capacity(MEETING_LEN);
    blob.push(VERSION);
    blob.extend_from_slice(&eph_pub);
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&sealed);
    Ok(blob)
}

/// Abre `blob` como introdução para `callee`. `None`: não é para esta chave, está adulterado, o carimbo está a mais
/// de [`MEETING_WINDOW_MS`] de `now_ms` ou a prova de autoria não fecha. Quem chama ainda confere se `caller` é
/// contato e se o `cookie` já foi visto.
#[must_use]
pub fn open_meeting(callee: &StaticSecret, blob: &[u8], now_ms: u64) -> Option<Meeting> {
    if blob.len() != MEETING_LEN {
        return None;
    }
    let mut r = Reader::new(blob);
    if r.u8()? != VERSION {
        return None;
    }
    let eph_pub: [u8; 32] = r.array()?;
    let nonce = r.take(NONCE_LEN)?;
    let sealed = r.take(PLAIN_LEN + 16)?;
    let to = *PublicKey::from(callee).as_bytes();
    let outer = dh(callee, &PublicKey::from(eph_pub))?;
    let plain = cipher(&outer, &to, &eph_pub)?.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad: &[VERSION] }).ok()?;
    let mut p = Reader::new(&plain);
    let caller: [u8; 32] = p.array()?;
    let stamp = p.u64()?;
    let cookie: [u8; 32] = p.array()?;
    let (public, link_public): ([u8; 32], [u8; 32]) = (p.array()?, p.array()?);
    let len = usize::from(p.u8()?);
    let padded = p.take(MAX_ADDR)?;
    let tag: [u8; PROOF_LEN] = p.array()?;
    if len == 0 || len > MAX_ADDR || stamp.abs_diff(now_ms) > MEETING_WINDOW_MS {
        return None;
    }
    let addr = String::from_utf8(padded.get(..len)?.to_vec()).ok()?;
    let inner = dh(callee, &PublicKey::from(caller))?;
    let expected = proof(&inner, &eph_pub, &cookie, stamp, &to)?;
    if !bool::from(expected.ct_eq(&tag)) {
        return None;
    }
    Some(Meeting { caller, rv: Candidate { addr, public, link_public, score: 1.0 }, cookie })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `cookie`s recentes de introduções já atendidas, para recusar a repetição de um blob gravado.
#[derive(Default)]
struct Seen {
    set: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl Seen {
    /// `true` na primeira vez que vê `cookie`.
    fn first_time(&mut self, cookie: [u8; 32]) -> bool {
        if !self.set.insert(cookie) {
            return false;
        }
        self.order.push_back(cookie);
        if self.order.len() > SEEN_COOKIES
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Conversa com um ponto, por um circuito de 2 saltos
// ---------------------------------------------------------------------------

/// Um circuito de 2 saltos até `exit`, com o primeiro salto num relay de `pool` (nunca o próprio `exit`) e o destino
/// `dest` no relay de saída. Sem relay de terceiro, falha em vez de montar um circuito direto.
fn circuit(pool: &[Candidate], exit: &Candidate, dest: &str, options: ClientOptions) -> Result<CircuitClient, MeetError> {
    let mut relays: Vec<Candidate> = pool.iter().filter(|c| c.addr != exit.addr).cloned().collect();
    if relays.is_empty() {
        return Err(MeetError::NoPool);
    }
    relays.push(exit.clone());
    let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), ..Default::default() };
    Ok(CircuitClient::spawn_with(CircuitManager::new(cfg, dest, relays), Duration::from_secs(3), options))
}

/// Circuito mais o acumulador das mensagens `u16 tamanho ‖ corpo` que chegam em pedaços.
struct Talk {
    client: CircuitClient,
    inbox: Messages,
    ready: VecDeque<Vec<u8>>,
}

impl Talk {
    async fn open(pool: &[Candidate], exit: &Candidate, dest: &str, options: ClientOptions) -> Result<Self, MeetError> {
        let mut client = circuit(pool, exit, dest, options)?;
        let up = timeout(READY_WAIT, async {
            while let Some(event) = client.next_event().await {
                if matches!(event, Event::Ready { .. }) {
                    return true;
                }
            }
            false
        })
        .await;
        if up != Ok(true) {
            client.stop();
            return Err(MeetError::NoCircuit);
        }
        Ok(Self { client, inbox: Messages::new(), ready: VecDeque::new() })
    }

    async fn say(&self, body: &[u8]) -> Result<(), MeetError> {
        let len = u16::try_from(body.len()).map_err(|_| MeetError::Protocol)?;
        let mut message = len.to_be_bytes().to_vec();
        message.extend_from_slice(body);
        for piece in message.chunks(CHUNK) {
            self.client.send(piece).await.map_err(|_| MeetError::NoCircuit)?;
        }
        Ok(())
    }

    /// A próxima mensagem inteira. `Err(NoCircuit)` quando o circuito ativo cai (a conexão com o ponto morre junto:
    /// o registro e o `cookie` não valem num circuito novo).
    async fn hear(&mut self, wait: Duration) -> Result<Vec<u8>, MeetError> {
        timeout(wait, async {
            loop {
                if let Some(message) = self.ready.pop_front() {
                    return Ok(message);
                }
                match self.client.next_event().await {
                    Some(Event::Data(chunk)) => {
                        self.ready.extend(self.inbox.push(&chunk).map_err(|_| MeetError::Protocol)?);
                    }
                    Some(Event::NoCircuit | Event::FailedOver { .. }) | None => return Err(MeetError::NoCircuit),
                    Some(_) => {}
                }
            }
        })
        .await
        .map_err(|_| MeetError::NoReply)?
    }

    /// Nada sobrou entre mensagens: o que vier depois é a conversa que passa pelo encontro, não resposta de ponto.
    fn clean(&self) -> bool {
        self.ready.is_empty() && self.inbox.pending() == 0
    }
}

// ---------------------------------------------------------------------------
// Quem liga
// ---------------------------------------------------------------------------

/// Onde D espera: um ponto de introdução e a chave de serviço que D provou ter lá. Fica na presença de D (etapa 4).
#[derive(Clone, Debug, PartialEq)]
pub struct IntroTarget {
    pub relay: Candidate,
    pub auth_key: [u8; 32],
}

/// O que D publica na presença v2 ([`crate::presence::seal_v2`]) é este ponto, sem a pontuação (que é de quem escolhe).
impl From<&IntroTarget> for IntroPointV2 {
    fn from(t: &IntroTarget) -> Self {
        Self { addr: t.relay.addr.clone(), public: t.relay.public, link_public: t.relay.link_public, auth_key: t.auth_key }
    }
}

/// O ponto que C lê da presença de D. A pontuação inicial é neutra: quem liga não tem reputação do relay de D.
impl From<&IntroPointV2> for IntroTarget {
    fn from(p: &IntroPointV2) -> Self {
        Self { relay: Candidate { addr: p.addr.clone(), public: p.public, link_public: p.link_public, score: 1.0 }, auth_key: p.auth_key }
    }
}

/// C chega a D: espera em `rv`, avisa D por `via` e devolve o circuito já no encontro, pronto para `call_linked`.
/// Tudo passa pelo primeiro salto de `pool`; D não aparece em lugar nenhum além do blob que só ele abre.
///
/// # Errors
///
/// [`MeetError::NoPool`] sem relay de terceiro; [`MeetError::NoCircuit`], [`MeetError::NoReply`] ou
/// [`MeetError::Refused`] quando um ponto não atende; [`MeetError::NoMeeting`] se D não chegou ao encontro.
pub async fn meet(secret: &StaticSecret, callee: &PublicKey, via: &IntroTarget, rv: &Candidate, pool: &[Candidate]) -> Result<CircuitClient, MeetError> {
    meet_with(secret, callee, via, rv, pool, ClientOptions::default()).await
}

/// Como [`meet`], com as opções (cover, perfil) do circuito do encontro, que é o que volta e carrega a tela ou o arquivo.
/// A conversa com o ponto de introdução usa sempre as opções padrão.
///
/// # Errors
///
/// Como [`meet`].
pub async fn meet_with(secret: &StaticSecret, callee: &PublicKey, via: &IntroTarget, rv: &Candidate, pool: &[Candidate], options: ClientOptions) -> Result<CircuitClient, MeetError> {
    let mut cookie = [0u8; 32];
    OsRng.fill_bytes(&mut cookie);
    let blob = seal_meeting(secret, callee, rv, cookie, now_ms())?;

    let mut waiting = Talk::open(pool, rv, RV_DEST, options).await?;
    waiting.say(&RvRequest::Establish { cookie }.encode()).await?;
    expect_waiting(&waiting.hear(REPLY_WAIT).await?)?;

    let mut intro = Talk::open(pool, &via.relay, INTRO_DEST, ClientOptions::default()).await?;
    intro.say(&IntroRequest::Introduce { auth_key: via.auth_key, blob }.encode()).await?;
    let reply = IntroReply::parse(&intro.hear(REPLY_WAIT).await?);
    intro.client.stop();
    if reply != Some(IntroReply::Delivered) {
        return Err(MeetError::Refused);
    }

    let joined = waiting.hear(JOIN_WAIT).await.map_err(|e| if matches!(e, MeetError::NoReply) { MeetError::NoMeeting } else { e })?;
    if RvReply::parse(&joined) != Some(RvReply::Joined) || !waiting.clean() {
        return Err(MeetError::Protocol);
    }
    Ok(waiting.client)
}

fn expect_waiting(body: &[u8]) -> Result<(), MeetError> {
    match RvReply::parse(body) {
        Some(RvReply::Waiting) => Ok(()),
        Some(RvReply::Refused) => Err(MeetError::Refused),
        _ => Err(MeetError::Protocol),
    }
}

// ---------------------------------------------------------------------------
// Quem atende
// ---------------------------------------------------------------------------

/// O que [`HiddenService::start`] precisa.
pub struct ServiceConfig {
    /// Chave de contato de D: abre os blobs e vale para a chamada.
    pub secret: StaticSecret,
    /// O telefone local de D, para onde o encontro é ligado.
    pub phone: SocketAddr,
    /// Contatos de D (lista viva): introdução de outra chave é descartada.
    pub contacts: Contacts,
    /// Relays de terceiros para o primeiro salto (sem o relay do próprio D).
    pub pool: Vec<Candidate>,
    /// Relays onde D espera. Uma `auth_key` nova para cada um.
    pub intros: Vec<Candidate>,
}

struct Shared {
    secret: StaticSecret,
    phone: SocketAddr,
    contacts: Contacts,
    pool: Vec<Candidate>,
    seen: Mutex<Seen>,
    slots: Arc<Semaphore>,
}

/// D à espera: mantém um circuito em cada ponto de introdução e atende cada introdução com um encontro. Largar o
/// valor derruba os circuitos.
pub struct HiddenService {
    points: Vec<IntroTarget>,
    tasks: Vec<JoinHandle<()>>,
}

impl HiddenService {
    /// Registra D nos pontos de `cfg.intros` e fica esperando. Devolve quando os pontos responderam (ou depois de
    /// [`START_WAIT`]); os que não responderam são deixados de lado.
    ///
    /// # Errors
    ///
    /// [`MeetError::NoPool`] sem relay de terceiro; [`MeetError::NoIntroPoint`] se nenhum ponto registrou D.
    pub async fn start(cfg: ServiceConfig) -> Result<Self, MeetError> {
        let ServiceConfig { secret, phone, contacts, pool, intros } = cfg;
        if pool.is_empty() {
            return Err(MeetError::NoPool);
        }
        let shared = Arc::new(Shared { secret, phone, contacts, pool, seen: Mutex::default(), slots: Arc::new(Semaphore::new(MAX_MEETINGS)) });
        let (up_tx, mut up_rx) = mpsc::channel::<usize>(intros.len().max(1));
        let mut candidates = Vec::new();
        for (index, relay) in intros.into_iter().enumerate() {
            let signer = SigningKey::generate(&mut OsRng);
            let target = IntroTarget { relay, auth_key: signer.verifying_key().to_bytes() };
            let task = tokio::spawn(keep_waiting(shared.clone(), target.clone(), signer, index, up_tx.clone()));
            candidates.push((target, task));
        }
        drop(up_tx);
        let mut up = HashSet::new();
        let _ = timeout(START_WAIT, async {
            while up.len() < candidates.len() {
                match up_rx.recv().await {
                    Some(index) => up.insert(index),
                    None => break,
                };
            }
        })
        .await;
        let (mut points, mut tasks) = (Vec::new(), Vec::new());
        for (index, (target, task)) in candidates.into_iter().enumerate() {
            if up.contains(&index) {
                points.push(target);
                tasks.push(task);
            } else {
                task.abort();
            }
        }
        if points.is_empty() {
            return Err(MeetError::NoIntroPoint);
        }
        Ok(Self { points, tasks })
    }

    /// Onde D está esperando agora: é o que vai na presença de D (etapa 4).
    #[must_use]
    pub fn points(&self) -> &[IntroTarget] {
        &self.points
    }
}

impl Drop for HiddenService {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Mantém D registrado em `target`: refaz o circuito de espera, com espera crescente, quando ele cai.
async fn keep_waiting(shared: Arc<Shared>, target: IntroTarget, signer: SigningKey, index: usize, up: mpsc::Sender<usize>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if wait_at(&shared, &target, &signer, index, &up).await {
            backoff = Duration::from_secs(1);
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Um circuito de espera, do registro até cair. `true` se chegou a registrar.
async fn wait_at(shared: &Arc<Shared>, target: &IntroTarget, signer: &SigningKey, index: usize, up: &mpsc::Sender<usize>) -> bool {
    let Ok(mut talk) = Talk::open(&shared.pool, &target.relay, INTRO_DEST, ClientOptions::default()).await else { return false };
    if register(&mut talk, target, signer).await.is_err() {
        talk.client.stop();
        return false;
    }
    let _ = up.try_send(index);
    // Espera sem prazo: a introdução chega quando alguém liga. Circuito caído encerra (`NoCircuit`) e o registro
    // no ponto morre com a conexão.
    while let Ok(message) = talk.hear(Duration::from_secs(24 * 3600)).await {
        match IntroReply::parse(&message) {
            Some(IntroReply::Push(blob)) => {
                tokio::spawn(attend(shared.clone(), blob));
            }
            _ => break,
        }
    }
    talk.client.stop();
    true
}

async fn register(talk: &mut Talk, target: &IntroTarget, signer: &SigningKey) -> Result<(), MeetError> {
    talk.say(&IntroRequest::Establish { auth_key: target.auth_key }.encode()).await?;
    let Some(IntroReply::Challenge(nonce)) = IntroReply::parse(&talk.hear(REPLY_WAIT).await?) else { return Err(MeetError::Protocol) };
    let signature = signer.sign(&hidden::proof_message(&nonce, &target.auth_key)).to_bytes();
    talk.say(&IntroRequest::Prove { signature }.encode()).await?;
    match IntroReply::parse(&talk.hear(REPLY_WAIT).await?) {
        Some(IntroReply::Established) => Ok(()),
        Some(IntroReply::Refused) => Err(MeetError::Refused),
        _ => Err(MeetError::Protocol),
    }
}

/// Uma introdução: confere, vai ao encontro e liga o circuito ao telefone. Tudo o que falha é descartado em silêncio;
/// quem ligou não aprende nada sobre D pelo motivo.
async fn attend(shared: Arc<Shared>, blob: Vec<u8>) {
    let Some(meeting) = open_meeting(&shared.secret, &blob, now_ms()) else { return };
    if !shared.contacts.lock_unpoisoned().contains(&meeting.caller) || !shared.seen.lock_unpoisoned().first_time(meeting.cookie) {
        return;
    }
    let Ok(_slot) = shared.slots.clone().try_acquire_owned() else { return };
    let Ok(mut talk) = Talk::open(&shared.pool, &meeting.rv, RV_DEST, ClientOptions::default()).await else { return };
    if talk.say(&RvRequest::Join { cookie: meeting.cookie }.encode()).await.is_err() {
        return;
    }
    let joined = talk.hear(REPLY_WAIT).await.ok().and_then(|m| RvReply::parse(&m));
    if joined == Some(RvReply::Joined) && talk.clean() {
        bridge(talk.client, shared.phone).await;
    }
}

/// Liga o circuito ao telefone local como o relay de saída faria: um quadro `[u16 tamanho][bytes]` por pedaço, nos
/// dois sentidos. O telefone não sabe que a chamada veio por um encontro.
async fn bridge(mut client: CircuitClient, phone: SocketAddr) {
    let Ok(tcp) = TcpStream::connect(phone).await else { return };
    let _ = tcp.set_nodelay(true);
    let (mut rd, mut wr) = tcp.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let reader = tokio::spawn(async move {
        let mut len = [0u8; 2];
        while rd.read_exact(&mut len).await.is_ok() {
            let n = usize::from(u16::from_be_bytes(len));
            if n > MAX_FRAME {
                return;
            }
            let mut frame = vec![0u8; n];
            if rd.read_exact(&mut frame).await.is_err() || tx.send(frame).await.is_err() {
                return;
            }
        }
    });
    loop {
        tokio::select! {
            event = client.next_event() => match event {
                Some(Event::Data(chunk)) => {
                    let Ok(n) = u16::try_from(chunk.len()) else { break };
                    let mut frame = n.to_be_bytes().to_vec();
                    frame.extend_from_slice(&chunk);
                    if wr.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                Some(Event::NoCircuit | Event::FailedOver { .. }) | None => break,
                Some(_) => {}
            },
            frame = rx.recv() => match frame {
                Some(frame) => {
                    if client.send(&frame).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    reader.abort();
    client.stop();
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn relay() -> Candidate {
        Candidate { addr: "203.0.113.7:4001".to_string(), public: [7; 32], link_public: [8; 32], score: 1.0 }
    }

    fn keys() -> (StaticSecret, StaticSecret) {
        (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng))
    }

    #[test]
    fn the_blob_opens_only_for_the_callee_and_proves_the_caller() {
        let (c, d) = keys();
        let blob = seal_meeting(&c, &PublicKey::from(&d), &relay(), [3; 32], 1_000).unwrap();
        assert_eq!(blob.len(), MEETING_LEN);
        const { assert!(MEETING_LEN <= hidden::MAX_BLOB) };
        let m = open_meeting(&d, &blob, 1_000).unwrap();
        assert_eq!((m.caller, m.cookie, m.rv), (*PublicKey::from(&c).as_bytes(), [3; 32], relay()));
        let (_, other) = keys();
        assert!(open_meeting(&other, &blob, 1_000).is_none(), "só D abre");
    }

    #[test]
    fn the_size_does_not_depend_on_the_address() {
        let (c, d) = keys();
        let short = Candidate { addr: "a:1".into(), ..relay() };
        let long = Candidate { addr: "x".repeat(MAX_ADDR), ..relay() };
        let a = seal_meeting(&c, &PublicKey::from(&d), &short, [1; 32], 5).unwrap();
        let b = seal_meeting(&c, &PublicKey::from(&d), &long, [1; 32], 5).unwrap();
        assert_eq!(a.len(), b.len());
        assert_eq!(open_meeting(&d, &a, 5).unwrap().rv.addr, "a:1");
        assert!(seal_meeting(&c, &PublicKey::from(&d), &Candidate { addr: "x".repeat(MAX_ADDR + 1), ..relay() }, [1; 32], 5).is_err());
        assert!(seal_meeting(&c, &PublicKey::from(&d), &Candidate { addr: String::new(), ..relay() }, [1; 32], 5).is_err());
    }

    #[test]
    fn two_blobs_for_the_same_meeting_look_unrelated() {
        let (c, d) = keys();
        let a = seal_meeting(&c, &PublicKey::from(&d), &relay(), [1; 32], 5).unwrap();
        let b = seal_meeting(&c, &PublicKey::from(&d), &relay(), [1; 32], 5).unwrap();
        assert_ne!(a.get(1..33), b.get(1..33), "efêmera nova a cada blob");
        assert_ne!(a.get(45..), b.get(45..));
    }

    #[test]
    fn a_forged_author_a_stale_stamp_and_a_changed_byte_are_refused() {
        let (c, d) = keys();
        let callee = PublicKey::from(&d);
        let blob = seal_meeting(&c, &callee, &relay(), [3; 32], 1_000_000).unwrap();
        assert!(open_meeting(&d, &blob, 1_000_000 + MEETING_WINDOW_MS + 1).is_none(), "carimbo velho");
        assert!(open_meeting(&d, &blob, 1_000_000 - MEETING_WINDOW_MS - 1).is_none(), "carimbo do futuro");
        for at in [0, 1, 40, MEETING_LEN / 2, MEETING_LEN - 1] {
            let mut bad = blob.clone();
            bad[at] ^= 1;
            assert!(open_meeting(&d, &bad, 1_000_000).is_none(), "byte {at} trocado");
        }
        assert!(open_meeting(&d, &blob[..MEETING_LEN - 1], 1_000_000).is_none());

        // Um impostor (que não tem a chave privada de C) monta o blob com a chave pública de C no lugar da dele:
        // a prova usa o segredo estático, então não fecha.
        let impostor = StaticSecret::random_from_rng(OsRng);
        let eph = StaticSecret::random_from_rng(OsRng);
        let (eph_pub, to) = (*PublicKey::from(&eph).as_bytes(), *callee.as_bytes());
        let outer = dh(&eph, &callee).unwrap();
        let tag = proof(&dh(&impostor, &callee).unwrap(), &eph_pub, &[9; 32], 1_000_000, &to).unwrap();
        let mut plain = Vec::new();
        for part in [PublicKey::from(&c).as_bytes().as_slice(), &1_000_000u64.to_be_bytes(), &[9; 32], &[7; 32], &[8; 32], &[3], b"a:1", &[0; MAX_ADDR - 3], &tag] {
            plain.extend_from_slice(part);
        }
        let nonce = [4u8; NONCE_LEN];
        let sealed = cipher(&outer, &to, &eph_pub).unwrap().encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &[VERSION] }).unwrap();
        let forged = [&[VERSION][..], &eph_pub, &nonce, &sealed].concat();
        assert_eq!(forged.len(), MEETING_LEN);
        assert!(open_meeting(&d, &forged, 1_000_000).is_none(), "autoria forjada");
    }

    #[test]
    fn a_repeated_cookie_is_seen_once() {
        let mut seen = Seen::default();
        assert!(seen.first_time([1; 32]));
        assert!(!seen.first_time([1; 32]));
        for i in 0..=SEEN_COOKIES as u16 {
            let mut cookie = [9u8; 32];
            cookie[..2].copy_from_slice(&i.to_be_bytes());
            seen.first_time(cookie);
        }
        assert!(seen.set.len() <= SEEN_COOKIES && seen.order.len() == seen.set.len());
    }

    #[test]
    fn an_intro_target_survives_the_presence_record() {
        let target = IntroTarget { relay: relay(), auth_key: [5; 32] };
        let point = IntroPointV2::from(&target);
        assert_eq!((point.addr.as_str(), point.auth_key), ("203.0.113.7:4001", [5; 32]));
        assert_eq!(IntroTarget::from(&point), target);
    }

    #[tokio::test]
    async fn without_a_third_party_relay_nothing_is_built() {
        let exit = relay();
        let alone = vec![exit.clone()];
        assert!(matches!(circuit(&alone, &exit, RV_DEST, ClientOptions::default()), Err(MeetError::NoPool)), "só o próprio destino não é terceiro");
        assert!(matches!(circuit(&[], &exit, RV_DEST, ClientOptions::default()), Err(MeetError::NoPool)));
        let (c, d) = keys();
        let via = IntroTarget { relay: exit.clone(), auth_key: [1; 32] };
        assert!(matches!(meet(&c, &PublicKey::from(&d), &via, &exit, &[]).await, Err(MeetError::NoPool)));
    }
}
