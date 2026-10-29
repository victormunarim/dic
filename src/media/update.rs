//! Atualização de endereço: um contato avisa o novo relay dele. *Sans-IO*; cada
//! mensagem é um quadro de **200 bytes**, como a sinalização da chamada.
//!
//! ```text
//! dono do endereço                                 contato
//!   Update: tag 4 ‖ nonce ‖ AEAD(registro de endereço)  ─>
//!                                       <─  Ack: tag 5 ‖ nonce ‖ AEAD(seq)
//! ```
//!
//! - O registro leva versão, `seq` (só cresce), o endereço do relay e as chaves dele (`onion`, `link`).
//! - A chave da cifra sai de `HKDF(DH(minha secreta, pública do outro), "clam/encontro/cifra/v1" ‖ pública do
//!   dono do conteúdo)`. Inclui o dono, então o registro de A para B e o de B para A usam chaves
//!   diferentes. Só os dois do par a calculam; um terceiro não lê nem forja o registro.
//! - O quadro não diz de quem é: quem recebe tenta a chave de cada contato ([`open_update`]). Quem não é
//!   contato não consegue abrir, e o telefone descarta o quadro em silêncio, como faz com um convite alheio.
//! - O ack prova que o contato abriu o registro (usa a chave do sentido contrário), e é o que tira o
//!   registro da fila de saída.
//! - O mesmo registro, sem o quadro em volta, é o que o ponto de encontro guarda na DHT
//!   ([`seal_record`], [`open_record`]); a chave de assinatura e o sal do item vêm de [`rendezvous`].
//! - A **apresentação** (tag 7) é o aviso de "adicionei você": quem adiciona um contato entrega o próprio
//!   cartão (nome e o relay dele) para o outro ter o contato na lista também. O
//!   quadro leva a chave pública de quem apresenta em claro (o destinatário ainda não o conhece, então não
//!   tem como tentar chave por chave), e o conteúdo vai cifrado com `DH(apresentador, destinatário)`: só
//!   quem tem a chave secreta da identidade que se diz autora consegue formar um quadro que abre. O
//!   destinatário confirma com o mesmo ack (tag 5) sobre o `seq` da apresentação ([`seal_intro`], [`open_intro`]).
//! - A **remoção** (tag 10) é o aviso de "removi você": quem tira um contato avisa o outro, que o tira também.
//!   Mesmo formato da apresentação (autor em claro, conteúdo cifrado com `DH`), com um carimbo de tempo no lugar
//!   do cartão; o ack (tag 5) repete o carimbo. Quem recebe só aceita carimbo dentro de [`REMOVAL_WINDOW_MS`],
//!   para que um quadro gravado não apague, dias depois, um contato readicionado ([`seal_removal`], [`open_removal`]).
//! - A **caixa de correio** (tags 11 e o valor na DHT) deixa a apresentação chegar a quem está offline sem que
//!   ninguém no meio saiba quem é o destinatário. Quem se apresenta entrega o quadro da apresentação, fechado só
//!   para o destinatário, a até 10 contatos de confiança (*custódia*: dois quadros de 200 bytes cifrados com
//!   `DH(autor, guardião)`, com o quadro da apresentação e uma *chave de caixa*, [`mailbox_master`]). A chave de caixa
//!   é um hash da chave do destinatário: o guardião a usa para derivar, a cada dia, onde gravar o quadro na
//!   DHT ([`mailbox_target`]) e o grava de hora em hora, mas não volta dela à chave do destinatário. O destinatário
//!   calcula os mesmos alvos, lê e abre com [`open_mail`]. Nada disso vai a um servidor.
//! - Repetir um registro gravado só devolve ao contato um endereço que ele já teve; quem recebe descarta
//!   registro com `seq` menor ou igual ao último que viu.

// Os registros, avisos e apresentações chegam de fora e qualquer um pode ter montado o quadro: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::bytes::Reader;

use super::{MediaError, packet::PACKET_LEN, signal::Frame};

pub const TAG_UPDATE: u8 = 4;
pub const TAG_ACK: u8 = 5;
pub const TAG_INTRO: u8 = 7;
pub const TAG_REMOVAL: u8 = 10;
pub const TAG_CUSTODY: u8 = 11;
/// Quanto tempo uma apresentação na caixa de correio vale, a contar do carimbo dela (7 dias).
pub const MAIL_TTL_MS: u64 = 7 * 24 * 3_600_000;
/// Itens da caixa de um dia: quem grava escolhe um pelo hash do quadro, e quem lê consulta todos. Mais de um evita
/// que duas apresentações para a mesma pessoa se atropelem no mesmo item da DHT.
pub const MAIL_SLOTS: usize = 4;
/// Pedaços da custódia (veja [`seal_custody`]).
pub const CUSTODY_PARTS: usize = 2;
const CUSTODY_PLAIN_LEN: usize = PACKET_LEN - 1 - 32 - NONCE_LEN - 16;
/// Dados de cada pedaço da custódia: o que sobra do quadro depois de versão, `id` e índice.
pub const CUSTODY_DATA: usize = CUSTODY_PLAIN_LEN - 1 - 8 - 1;
/// Diferença máxima, em ms, entre o carimbo da remoção e o relógio de quem recebe (1 h): cobre relógios tortos e
/// limita o que um quadro gravado pode fazer depois.
pub const REMOVAL_WINDOW_MS: u64 = 3_600_000;
/// Só separa o domínio da cifra do registro guardado na DHT (entra como dado associado, não vai a nenhum quadro).
const TAG_RECORD: u8 = 6;
/// Tamanho máximo do endereço (`host:porta`) no registro, o que cabe no quadro.
pub const MAX_ADDR: usize = 60;
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const PLAIN_LEN: usize = 1 + 8 + 1 + MAX_ADDR + 32 + 32;
/// Tamanho do valor de um registro guardado fora do circuito (DHT, âncora): `nonce ‖ AEAD`.
pub const RECORD_LEN: usize = NONCE_LEN + PLAIN_LEN + 16;
/// Parte fixa da apresentação: versão, `seq`, tipo, tamanhos, chaves do relay.
const INTRO_HEAD: usize = 1 + 8 + 1 + 1 + 1 + 32 + 32;
/// O quadro da apresentação cabe em [`PACKET_LEN`]: tag, autor, nonce, conteúdo, autenticação.
const INTRO_PLAIN_LEN: usize = PACKET_LEN - 1 - 32 - NONCE_LEN - 16;
/// Bytes para o endereço e o nome juntos na apresentação.
pub const INTRO_TEXT: usize = INTRO_PLAIN_LEN - INTRO_HEAD;
/// Maior nome que a apresentação leva (o nome perde para o endereço quando falta espaço).
pub const MAX_NAME: usize = 32;

/// O que o dono do endereço conta ao contato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressRecord {
    /// Cresce a cada registro novo do mesmo dono.
    pub seq: u64,
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

fn cipher(my: &StaticSecret, peer: &PublicKey, owner: &PublicKey) -> Option<ChaCha20Poly1305> {
    let dh = my.diffie_hellman(peer);
    if !dh.was_contributory() {
        return None;
    }
    let mut info = b"clam/encontro/cifra/v1".to_vec();
    info.extend_from_slice(owner.as_bytes());
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(None, dh.as_bytes()).expand(&info, &mut k).expect("32 bytes é válido");
    Some(ChaCha20Poly1305::new(Key::from_slice(&k)))
}

/// `nonce ‖ AEAD(plain)`, com nonce aleatório.
fn seal_bytes(cipher: &ChaCha20Poly1305, tag: u8, plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: &[tag] }).expect("cifrar em memória não falha"));
    out
}

fn seal(cipher: &ChaCha20Poly1305, tag: u8, plain: &[u8]) -> Frame {
    seal_after(cipher, tag, &[], plain)
}

/// Quadro `tag ‖ head ‖ nonce ‖ AEAD(plain)`, completado com bytes aleatórios até [`PACKET_LEN`].
fn seal_after(cipher: &ChaCha20Poly1305, tag: u8, head: &[u8], plain: &[u8]) -> Frame {
    let sealed = seal_bytes(cipher, tag, plain);
    let prefix = [&[tag][..], head, &sealed].concat();
    let mut f = [0u8; PACKET_LEN];
    OsRng.fill_bytes(&mut f);
    // Os tamanhos são fixos por construção; se um dia passarem de `PACKET_LEN`, o quadro sai truncado e não abre.
    if let Some(dst) = f.get_mut(..prefix.len()) {
        dst.copy_from_slice(&prefix);
    }
    f
}

/// `parts` emendados e completados com zeros até `N` bytes.
fn padded<const N: usize>(parts: &[&[u8]]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut at = 0;
    for part in parts {
        if let Some(dst) = out.get_mut(at..at + part.len()) {
            dst.copy_from_slice(part);
        }
        at += part.len();
    }
    out
}

fn open(cipher: &ChaCha20Poly1305, tag: u8, frame: &[u8], plain_len: usize) -> Option<Vec<u8>> {
    open_after(cipher, tag, 0, frame, plain_len)
}

/// Abre o quadro `tag ‖ head (head_len bytes) ‖ nonce ‖ AEAD(plain)`.
fn open_after(cipher: &ChaCha20Poly1305, tag: u8, head_len: usize, frame: &[u8], plain_len: usize) -> Option<Vec<u8>> {
    let frame: &Frame = frame.try_into().ok()?;
    let mut r = Reader::new(frame);
    if r.u8()? != tag {
        return None;
    }
    r.take(head_len)?;
    let nonce = r.take(NONCE_LEN)?;
    let sealed = r.take(plain_len + 16)?;
    cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad: &[tag] }).ok()
}

fn encode(record: &AddressRecord) -> Result<[u8; PLAIN_LEN], MediaError> {
    if record.addr.is_empty() || record.addr.len() > MAX_ADDR {
        return Err(MediaError::Malformed);
    }
    let addr = padded::<MAX_ADDR>(&[record.addr.as_bytes()]);
    Ok(padded(&[&[VERSION], &record.seq.to_be_bytes(), &[record.addr.len() as u8], &addr, &record.onion, &record.link]))
}

/// Quadro com o registro de `my_static` para `peer`.
pub fn seal_update(my_static: &StaticSecret, peer: &PublicKey, record: &AddressRecord) -> Result<Frame, MediaError> {
    let plain = encode(record)?;
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal(&cipher, TAG_UPDATE, &plain))
}

/// O registro de `my_static` para `peer` como valor de um item da DHT (`nonce ‖ AEAD`, 162 B).
pub fn seal_record(my_static: &StaticSecret, peer: &PublicKey, record: &AddressRecord) -> Result<Vec<u8>, MediaError> {
    let plain = encode(record)?;
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal_bytes(&cipher, TAG_RECORD, &plain))
}

/// Abre o valor de um item da DHT que `peer` guardou para `my_static`. `None`: não abre com a chave do par.
pub fn open_record(my_static: &StaticSecret, peer: &PublicKey, value: &[u8]) -> Option<AddressRecord> {
    if value.len() != NONCE_LEN + PLAIN_LEN + 16 {
        return None;
    }
    let cipher = cipher(my_static, peer, peer)?;
    let (nonce, body) = value.split_at_checked(NONCE_LEN)?;
    let plain = cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: body, aad: &[TAG_RECORD] }).ok()?;
    parse(&plain)
}

/// Onde `owner` guarda o endereço dele para o par `(my_static, peer)` no dia `period` (`unix / 86400`): a semente da
/// chave de assinatura ed25519 e o sal do item. Os dois lados do par derivam o mesmo par de valores; o dia
/// entra nas duas derivações, então o alvo na DHT muda sozinho a cada dia e um dia não se liga ao outro.
pub fn rendezvous(my_static: &StaticSecret, peer: &PublicKey, owner: &PublicKey, period: u64) -> Option<([u8; 32], [u8; 32])> {
    let dh = my_static.diffie_hellman(peer);
    if !dh.was_contributory() {
        return None;
    }
    let derive = |label: &[u8]| {
        let mut info = label.to_vec();
        info.extend_from_slice(owner.as_bytes());
        info.extend_from_slice(&period.to_be_bytes());
        let mut out = [0u8; 32];
        Hkdf::<Sha256>::new(None, dh.as_bytes()).expand(&info, &mut out).expect("32 bytes é válido");
        out
    };
    Some((derive(b"clam/encontro/assina/v1"), derive(b"clam/encontro/sal/v1")))
}

/// Só separa o domínio da cifra do registro de servidor (dado associado).
const TAG_SERVER_RECORD: u8 = 9;

fn server_derive(secret: &[u8; 32], label: &[u8], server: &[u8; 16], owner: &[u8; 32], period: Option<u64>) -> [u8; 32] {
    let mut info = label.to_vec();
    info.extend_from_slice(server);
    info.extend_from_slice(owner);
    if let Some(p) = period {
        info.extend_from_slice(&p.to_be_bytes());
    }
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, secret).expand(&info, &mut out).expect("32 bytes é válido");
    out
}

/// Onde `owner` guarda o endereço dele para os membros de um servidor, no dia `period`: semente de assinatura e
/// sal do item, derivados do segredo do servidor. Um registro por membro, não um por par: qualquer membro, só
/// sabendo a chave do outro, acha o item. Quem tem o segredo também deriva a semente de `owner` e pode gravar
/// no lugar dele; por isso o registro é só uma dica de relay, e a chamada ainda confere a identidade do outro lado.
pub fn server_rendezvous(secret: &[u8; 32], server: &[u8; 16], owner: &[u8; 32], period: u64) -> ([u8; 32], [u8; 32]) {
    (server_derive(secret, b"clam/servidor/encontro/assina/v1", server, owner, Some(period)), server_derive(secret, b"clam/servidor/encontro/sal/v1", server, owner, Some(period)))
}

fn server_cipher(secret: &[u8; 32], server: &[u8; 16], owner: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(&server_derive(secret, b"clam/servidor/encontro/cifra/v1", server, owner, None)))
}

/// O registro de `owner` como valor de um item da DHT, cifrado com o segredo do servidor.
pub fn seal_server_record(secret: &[u8; 32], server: &[u8; 16], owner: &[u8; 32], record: &AddressRecord) -> Result<Vec<u8>, MediaError> {
    Ok(seal_bytes(&server_cipher(secret, server, owner), TAG_SERVER_RECORD, &encode(record)?))
}

/// Abre o valor que `owner` guardou para os membros do servidor. `None`: não abre com este segredo (outra época) ou é lixo.
pub fn open_server_record(secret: &[u8; 32], server: &[u8; 16], owner: &[u8; 32], value: &[u8]) -> Option<AddressRecord> {
    if value.len() != NONCE_LEN + PLAIN_LEN + 16 {
        return None;
    }
    let (nonce, body) = value.split_at_checked(NONCE_LEN)?;
    let plain = server_cipher(secret, server, owner).decrypt(Nonce::from_slice(nonce), Payload { msg: body, aad: &[TAG_SERVER_RECORD] }).ok()?;
    parse(&plain)
}

/// Abre `frame` como atualização de algum de `contacts`. `None`: não é uma atualização para esta chave, ou
/// de nenhum contato; ignore o quadro.
pub fn open_update(my_static: &StaticSecret, contacts: impl IntoIterator<Item = [u8; 32]>, frame: &[u8]) -> Option<([u8; 32], AddressRecord)> {
    if frame.first() != Some(&TAG_UPDATE) {
        return None;
    }
    for contact in contacts {
        let owner = PublicKey::from(contact);
        let Some(cipher) = cipher(my_static, &owner, &owner) else { continue };
        let Some(plain) = open(&cipher, TAG_UPDATE, frame, PLAIN_LEN) else { continue };
        let record = parse(&plain)?;
        return Some((contact, record));
    }
    None
}

/// `true` se `addr` é um `host:porta` que vale a pena discar: só ASCII visível, porta de 1 a 65535 e, se o
/// host é um IP, que não seja o não especificado, multicast nem link-local (onde fica, por exemplo, o
/// serviço de metadados de nuvem). Endereços vêm de contatos, convites e da DHT: quem os escreve não é de
/// confiança, e o programa conecta neles sozinho.
pub fn valid_addr(addr: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    if addr.is_empty() || addr.len() > MAX_ADDR || !addr.bytes().all(|b| b.is_ascii_graphic()) {
        return false;
    }
    let Some((host, port)) = addr.rsplit_once(':') else { return false };
    if host.is_empty() || port.parse::<u16>().is_ok_and(|p| p == 0) || port.parse::<u16>().is_err() {
        return false;
    }
    match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(ip) => match ip.to_canonical() {
            IpAddr::V4(a) => !(a.is_unspecified() || a.is_multicast() || a.is_link_local() || a == Ipv4Addr::BROADCAST),
            IpAddr::V6(a) => !(a.is_unspecified() || a.is_multicast() || (a.segments()[0] & 0xffc0) == 0xfe80),
        },
        Err(_) => true,
    }
}

fn parse(plain: &[u8]) -> Option<AddressRecord> {
    if plain.len() != PLAIN_LEN {
        return None;
    }
    let mut r = Reader::new(plain);
    if r.u8()? != VERSION {
        return None;
    }
    let seq = r.u64()?;
    let n = usize::from(r.u8()?);
    if n == 0 || n > MAX_ADDR {
        return None;
    }
    let field = r.take(MAX_ADDR)?;
    let addr = String::from_utf8(field.get(..n)?.to_vec()).ok().filter(|a| valid_addr(a))?;
    Some(AddressRecord { seq, addr, onion: r.array()?, link: r.array()? })
}

/// Como o apresentador pode ser alcançado por quem o adiciona: o relay dele (o código dele é `chave#addr#onion#link`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach {
    pub addr: String,
    pub onion: [u8; 32],
    pub link: [u8; 32],
}

/// O cartão que quem adiciona um contato entrega a ele.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intro {
    /// Distingue uma apresentação da outra; o ack repete este número.
    pub seq: u64,
    /// Como o apresentador quer ser chamado (até [`MAX_NAME`] bytes; corta no fim se faltar espaço).
    pub name: String,
    pub reach: Reach,
}

/// Quadro de apresentação de `my_static` para `peer`. Falha se o endereço for vazio ou passar de [`MAX_ADDR`].
pub fn seal_intro(my_static: &StaticSecret, peer: &PublicKey, intro: &Intro) -> Result<Frame, MediaError> {
    let Reach { addr, onion, link } = &intro.reach;
    if addr.is_empty() || addr.len() > MAX_ADDR {
        return Err(MediaError::Malformed);
    }
    let name = clip(intro.name.trim(), MAX_NAME.min(INTRO_TEXT - addr.len()));
    // Byte 9 é reservado (zero).
    let plain: [u8; INTRO_PLAIN_LEN] =
        padded(&[&[VERSION], &intro.seq.to_be_bytes(), &[0, addr.len() as u8, name.len() as u8], onion, link, addr.as_bytes(), name.as_bytes()]);
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal_after(&cipher, TAG_INTRO, me.as_bytes(), &plain))
}

/// O começo de `s` com até `max` bytes, sem cortar um caractere ao meio.
fn clip(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

/// Abre `frame` como apresentação para `my_static`. Devolve a chave de quem apresentou (provada: só quem tem a
/// chave secreta dela monta um quadro que abre) e o cartão. `None`: não é uma apresentação para esta chave.
pub fn open_intro(my_static: &StaticSecret, frame: &[u8]) -> Option<([u8; 32], Intro)> {
    let mut head = Reader::new(frame);
    if frame.len() != PACKET_LEN || head.u8()? != TAG_INTRO {
        return None;
    }
    let author: [u8; 32] = head.array()?;
    let owner = PublicKey::from(author);
    let cipher = cipher(my_static, &owner, &owner)?;
    let plain = open_after(&cipher, TAG_INTRO, 32, frame, INTRO_PLAIN_LEN)?;
    let mut r = Reader::new(&plain);
    if r.u8()? != VERSION {
        return None;
    }
    let seq = r.u64()?;
    r.u8()?;
    let (addr_len, name_len) = (usize::from(r.u8()?), usize::from(r.u8()?));
    if addr_len == 0 || addr_len > MAX_ADDR || addr_len + name_len > INTRO_TEXT {
        return None;
    }
    let (onion, link) = (r.array()?, r.array()?);
    let (addr, name) = r.take(addr_len + name_len)?.split_at_checked(addr_len)?;
    let addr = String::from_utf8(addr.to_vec()).ok()?;
    let name = String::from_utf8(name.to_vec()).ok().filter(|n| !n.chars().any(char::is_control))?;
    if !valid_addr(&addr) {
        return None;
    }
    Some((author, Intro { seq, name, reach: Reach { addr, onion, link } }))
}

/// Quadro de remoção de `my_static` para `peer`, com o carimbo `stamp_ms` (milissegundos Unix de agora).
pub fn seal_removal(my_static: &StaticSecret, peer: &PublicKey, stamp_ms: u64) -> Result<Frame, MediaError> {
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    let plain: [u8; 9] = padded(&[&[VERSION], &stamp_ms.to_be_bytes()]);
    Ok(seal_after(&cipher, TAG_REMOVAL, me.as_bytes(), &plain))
}

/// Abre `frame` como remoção para `my_static`. Devolve a chave de quem removeu (provada) e o carimbo. `None`: não
/// é remoção para esta chave, ou o carimbo está a mais de [`REMOVAL_WINDOW_MS`] de `now_ms`.
pub fn open_removal(my_static: &StaticSecret, frame: &[u8], now_ms: u64) -> Option<([u8; 32], u64)> {
    let mut head = Reader::new(frame);
    if frame.len() != PACKET_LEN || head.u8()? != TAG_REMOVAL {
        return None;
    }
    let author: [u8; 32] = head.array()?;
    let owner = PublicKey::from(author);
    let cipher = cipher(my_static, &owner, &owner)?;
    let plain = open_after(&cipher, TAG_REMOVAL, 32, frame, 9)?;
    let mut r = Reader::new(&plain);
    if r.u8()? != VERSION {
        return None;
    }
    let stamp = r.u64()?;
    (stamp.abs_diff(now_ms) <= REMOVAL_WINDOW_MS).then_some((author, stamp))
}

/// Um pedaço da custódia: o `id` (o `seq` da apresentação) liga os pedaços do mesmo depósito.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustodyPart {
    pub id: u64,
    pub idx: u8,
    pub data: [u8; CUSTODY_DATA],
}

/// Chave da caixa de `recipient`: um hash da chave pública dele. Quem a tem calcula onde a caixa fica a cada dia
/// ([`mailbox_target`]), mas não volta dela à chave; quem sabe a chave de `recipient` a calcula também.
pub fn mailbox_master(recipient: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, recipient).expand(b"clam/caixa/mestre/v1", &mut out).expect("32 bytes é válido");
    out
}

/// Onde a caixa guarda o item `slot` no dia `period`: semente da chave de assinatura e sal. Muda a cada dia e a cada
/// item; qualquer um com a chave da caixa assina, e só o destinatário abre o que está lá.
pub fn mailbox_target(master: &[u8; 32], period: u64, slot: usize) -> ([u8; 32], [u8; 32]) {
    let derive = |label: &[u8]| {
        let mut info = label.to_vec();
        info.extend_from_slice(&period.to_be_bytes());
        info.extend_from_slice(&(slot as u64).to_be_bytes());
        let mut out = [0u8; 32];
        Hkdf::<Sha256>::new(None, master).expand(&info, &mut out).expect("32 bytes é válido");
        out
    };
    (derive(b"clam/caixa/assina/v1"), derive(b"clam/caixa/sal/v1"))
}

/// Qual item da caixa leva este quadro: o mesmo quadro cai sempre no mesmo item (os guardiões gravam o mesmo valor).
pub fn mail_slot(mail: &[u8]) -> usize {
    use sha2::Digest;
    Sha256::digest(mail).first().map_or(0, |b| usize::from(*b) % MAIL_SLOTS)
}

/// Abre `value`, lido da caixa de `my_static`, como apresentação ainda dentro do prazo. `None`: não é uma
/// apresentação para esta chave, ou o carimbo dela passa de [`MAIL_TTL_MS`] ou está mais de uma hora no futuro.
pub fn open_mail(my_static: &StaticSecret, value: &[u8], now_ms: u64) -> Option<([u8; 32], Intro)> {
    let (author, intro) = open_intro(my_static, value)?;
    let fresh = intro.seq <= now_ms.saturating_add(REMOVAL_WINDOW_MS) && now_ms.saturating_sub(intro.seq) <= MAIL_TTL_MS;
    fresh.then_some((author, intro))
}

/// Os dois quadros com que `my_static` deixa com `peer` (o guardião) a apresentação `mail` (um quadro de
/// [`seal_intro`]) e a chave da caixa do destinatário. `id` é o `seq` da apresentação.
pub fn seal_custody(my_static: &StaticSecret, peer: &PublicKey, id: u64, master: &[u8; 32], mail: &Frame) -> Result<[Frame; CUSTODY_PARTS], MediaError> {
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    let payload = [&master[..], &mail[..]].concat();
    let mut frames = [[0u8; PACKET_LEN]; CUSTODY_PARTS];
    for (idx, (frame, chunk)) in frames.iter_mut().zip(payload.chunks(CUSTODY_DATA)).enumerate() {
        let data = padded::<CUSTODY_DATA>(&[chunk]);
        let plain: [u8; CUSTODY_PLAIN_LEN] = padded(&[&[VERSION], &id.to_be_bytes(), &[idx as u8], &data]);
        *frame = seal_after(&cipher, TAG_CUSTODY, me.as_bytes(), &plain);
    }
    Ok(frames)
}

/// Abre `frame` como pedaço de custódia para `my_static`. Devolve a chave de quem deixou (provada, como na
/// apresentação) e o pedaço. `None`: não é uma custódia para esta chave.
pub fn open_custody(my_static: &StaticSecret, frame: &[u8]) -> Option<([u8; 32], CustodyPart)> {
    let mut head = Reader::new(frame);
    if frame.len() != PACKET_LEN || head.u8()? != TAG_CUSTODY {
        return None;
    }
    let author: [u8; 32] = head.array()?;
    let owner = PublicKey::from(author);
    let cipher = cipher(my_static, &owner, &owner)?;
    let plain = open_after(&cipher, TAG_CUSTODY, 32, frame, CUSTODY_PLAIN_LEN)?;
    let mut r = Reader::new(&plain);
    if r.u8()? != VERSION {
        return None;
    }
    let (id, idx) = (r.u64()?, r.u8()?);
    if usize::from(idx) >= CUSTODY_PARTS {
        return None;
    }
    Some((author, CustodyPart { id, idx, data: r.array()? }))
}

/// Junta os pedaços (em ordem) e devolve a chave da caixa e o quadro da apresentação.
pub fn join_custody(parts: &[[u8; CUSTODY_DATA]; CUSTODY_PARTS]) -> Option<([u8; 32], Frame)> {
    let all: Vec<u8> = parts.iter().flatten().copied().collect();
    let mut r = Reader::new(&all);
    let master = r.array()?;
    let mail: Frame = r.array()?;
    Some((master, mail))
}

/// Confirmação de `seq` de `my_static` para `peer`, a quem mandou a atualização.
pub fn seal_ack(my_static: &StaticSecret, peer: &PublicKey, seq: u64) -> Result<Frame, MediaError> {
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    Ok(seal(&cipher, TAG_ACK, &seq.to_be_bytes()))
}

/// `true` se `frame` é a confirmação de `peer` para o registro `seq`.
pub fn open_ack(my_static: &StaticSecret, peer: &PublicKey, seq: u64, frame: &[u8]) -> bool {
    cipher(my_static, peer, peer)
        .and_then(|c| open(&c, TAG_ACK, frame, 8))
        .is_some_and(|p| p == seq.to_be_bytes())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    #[test]
    fn a_server_record_opens_only_with_the_servers_secret_for_that_owner() {
        let (secret, server, owner) = ([1u8; 32], [2u8; 16], [3u8; 32]);
        let record = AddressRecord { seq: 7, addr: "1.2.3.4:4001".into(), onion: [4; 32], link: [5; 32] };
        let value = seal_server_record(&secret, &server, &owner, &record).unwrap();
        assert_eq!(open_server_record(&secret, &server, &owner, &value), Some(record));
        assert_eq!(open_server_record(&[9; 32], &server, &owner, &value), None, "outro segredo (outra época)");
        assert_eq!(open_server_record(&secret, &[9; 16], &owner, &value), None, "outro servidor");
        assert_eq!(open_server_record(&secret, &server, &[9; 32], &value), None, "outro dono");
        assert_eq!(open_server_record(&secret, &server, &owner, &value[1..]), None);
    }

    #[test]
    fn the_server_slot_is_per_owner_and_per_day() {
        let (secret, server) = ([1u8; 32], [2u8; 16]);
        let base = server_rendezvous(&secret, &server, &[3; 32], 100);
        assert_eq!(base, server_rendezvous(&secret, &server, &[3; 32], 100));
        assert_ne!(base, server_rendezvous(&secret, &server, &[4; 32], 100));
        assert_ne!(base, server_rendezvous(&secret, &server, &[3; 32], 101));
        assert_ne!(base, server_rendezvous(&[9; 32], &server, &[3; 32], 100));
        assert_ne!(base.0, base.1);
    }

    use super::*;

    fn ids() -> (StaticSecret, StaticSecret) {
        (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng))
    }

    fn record(seq: u64) -> AddressRecord {
        AddressRecord { seq, addr: "casa.exemplo.com:4001".into(), onion: [7; 32], link: [9; 32] }
    }

    #[test]
    fn the_contact_opens_what_the_owner_sealed() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let frame = seal_update(&a, &pb, &record(5)).unwrap();
        let (from, rec) = open_update(&b, [[1; 32], *pa.as_bytes()], &frame).unwrap();
        assert_eq!(from, *pa.as_bytes());
        assert_eq!(rec, record(5));
    }

    #[test]
    fn the_frame_is_always_200_bytes_and_does_not_show_the_address() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        let mut short = record(1);
        short.addr = "1.2.3.4:5".into();
        let f1 = seal_update(&a, &pb, &short).unwrap();
        let f2 = seal_update(&a, &pb, &record(1)).unwrap();
        assert_eq!((f1.len(), f2.len()), (PACKET_LEN, PACKET_LEN));
        assert!(!f2.windows(5).any(|w| w == b"casa."));
    }

    #[test]
    fn a_stranger_cannot_open_or_forge_it() {
        let (a, b) = ids();
        let c = StaticSecret::random_from_rng(OsRng);
        let (pa, pb, pc) = (PublicKey::from(&a), PublicKey::from(&b), PublicKey::from(&c));
        let frame = seal_update(&a, &pb, &record(1)).unwrap();
        // Quem não é contato de A, ou não tem a chave de B, não abre.
        assert!(open_update(&b, [*pc.as_bytes()], &frame).is_none());
        assert!(open_update(&c, [*pa.as_bytes()], &frame).is_none());
        // C diz ser A: não deriva a chave do par A-B.
        let forged = seal_update(&c, &pb, &record(2)).unwrap();
        assert!(open_update(&b, [*pa.as_bytes()], &forged).is_none());
    }

    #[test]
    fn a_flipped_byte_breaks_authentication() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let mut frame = seal_update(&a, &pb, &record(1)).unwrap();
        frame[20] ^= 1;
        assert!(open_update(&b, [*pa.as_bytes()], &frame).is_none());
    }

    #[test]
    fn the_key_depends_on_who_owns_the_address() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        // B não consegue passar por A: o registro "de A" cifrado por B usa a chave do dono B.
        let by_b = seal_update(&b, &pa, &record(1)).unwrap();
        assert!(open_update(&b, [*pa.as_bytes()], &by_b).is_none());
        assert!(open_update(&a, [*pb.as_bytes()], &by_b).is_some());
    }

    #[test]
    fn the_ack_confirms_only_its_own_seq() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let ack = seal_ack(&b, &pa, 7).unwrap();
        assert!(open_ack(&a, &pb, 7, &ack));
        assert!(!open_ack(&a, &pb, 8, &ack));
        assert!(!open_ack(&b, &pa, 7, &ack));
    }

    #[test]
    fn the_stored_record_opens_only_with_the_pair_key() {
        let (a, b) = ids();
        let c = StaticSecret::random_from_rng(OsRng);
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let value = seal_record(&a, &pb, &record(3)).unwrap();
        assert_eq!(value.len(), 162);
        assert_eq!(open_record(&b, &pa, &value), Some(record(3)));
        assert!(open_record(&c, &pa, &value).is_none());
        assert!(open_record(&a, &pb, &value).is_none(), "o dono não é o par");
        // Quadro de atualização e item guardado não se confundem.
        let frame = seal_update(&a, &pb, &record(3)).unwrap();
        assert!(open_record(&b, &pa, &frame[1..163]).is_none());
    }

    #[test]
    fn both_sides_derive_the_same_place_and_it_moves_every_day() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let by_owner = rendezvous(&a, &pb, &pa, 100).unwrap();
        assert_eq!(rendezvous(&b, &pa, &pa, 100).unwrap(), by_owner);
        assert_ne!(rendezvous(&a, &pb, &pa, 101).unwrap(), by_owner);
        // Um sentido por item: o endereço de B para A fica em outro lugar.
        assert_ne!(rendezvous(&a, &pb, &pb, 100).unwrap(), by_owner);
        let stranger = StaticSecret::random_from_rng(OsRng);
        assert_ne!(rendezvous(&stranger, &pb, &pa, 100).unwrap(), by_owner);
    }

    fn intro(seq: u64) -> Intro {
        Intro { seq, name: "Ana".into(), reach: Reach { addr: "203.0.113.7:4001".into(), onion: [3; 32], link: [4; 32] } }
    }

    #[test]
    fn the_introduction_proves_who_sent_it_and_reads_only_for_the_recipient() {
        let (a, b) = ids();
        let c = StaticSecret::random_from_rng(OsRng);
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let frame = seal_intro(&a, &pb, &intro(9)).unwrap();
        assert_eq!(frame.len(), PACKET_LEN);
        assert_eq!(open_intro(&b, &frame), Some((*pa.as_bytes(), intro(9))));
        // Outro destinatário não abre.
        assert!(open_intro(&c, &frame).is_none());
        // C não passa por A: troca a chave de autor no quadro e a cifra deixa de abrir.
        let mut forged = seal_intro(&c, &pb, &intro(9)).unwrap();
        forged[1..33].copy_from_slice(pa.as_bytes());
        assert!(open_intro(&b, &forged).is_none());
        // Um byte mudado quebra a autenticação.
        let mut flipped = frame;
        flipped[100] ^= 1;
        assert!(open_intro(&b, &flipped).is_none());
        // Atualização de endereço e apresentação não se confundem.
        let update = seal_update(&a, &pb, &record(1)).unwrap();
        assert!(open_intro(&b, &update).is_none());
        assert!(open_update(&b, [*pa.as_bytes()], &frame).is_none());
    }

    #[test]
    fn the_introduction_does_not_show_the_address_or_the_name() {
        let (a, b) = ids();
        let frame = seal_intro(&a, &PublicKey::from(&b), &intro(1)).unwrap();
        assert!(!frame.windows(3).any(|w| w == b"Ana") && !frame.windows(8).any(|w| w == b"203.0.113"));
    }

    #[test]
    fn only_dialable_addresses_and_clean_names_come_out_of_an_introduction() {
        for ok in ["203.0.113.7:4001", "casa.exemplo.com:4001", "[2001:db8::1]:4001", "127.0.0.1:4001", "192.168.0.2:4001"] {
            assert!(valid_addr(ok), "{ok}");
        }
        for bad in ["", "sem-porta", ":4001", "host:0", "host:70000", "host:abc", "a b:4001", "h\n:4001", "169.254.169.254:80", "0.0.0.0:4001", "224.0.0.1:4001", "[fe80::1]:4001", "[::]:4001", "é.exemplo:4001"] {
            assert!(!valid_addr(bad), "{bad:?}");
        }
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        for (name, addr) in [("Ana\u{7}", "203.0.113.7:4001"), ("Ana", "169.254.169.254:80")] {
            let bad = Intro { name: name.into(), reach: Reach { addr: addr.into(), onion: [3; 32], link: [4; 32] }, ..intro(1) };
            let frame = seal_intro(&a, &pb, &bad).unwrap();
            assert!(open_intro(&b, &frame).is_none(), "{name:?} {addr}");
        }
    }

    #[test]
    fn the_introduction_trims_the_name_to_fit() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        let long = Intro { seq: 2, name: "é".repeat(40), reach: Reach { addr: "casa.exemplo.com:4001".into(), onion: [5; 32], link: [6; 32] } };
        let (_, got) = open_intro(&b, &seal_intro(&a, &pb, &long).unwrap()).unwrap();
        assert_eq!(got.reach, long.reach);
        assert!(got.name.len() <= MAX_NAME && got.name.chars().all(|c| c == 'é') && !got.name.is_empty());
        // Endereço no limite: o nome encolhe para caber, o endereço não perde nada.
        let tight = Intro { seq: 3, name: "x".repeat(MAX_NAME), reach: Reach { addr: format!("{}:4001", "h".repeat(MAX_ADDR - 5)), onion: [1; 32], link: [2; 32] } };
        let (_, got) = open_intro(&b, &seal_intro(&a, &pb, &tight).unwrap()).unwrap();
        assert_eq!(got.reach, tight.reach);
        assert_eq!(got.name.len(), INTRO_TEXT - MAX_ADDR);
        for addr in ["", &"x".repeat(MAX_ADDR + 1)] {
            let bad = Intro { reach: Reach { addr: addr.to_string(), onion: [3; 32], link: [4; 32] }, ..intro(1) };
            assert_eq!(seal_intro(&a, &pb, &bad), Err(MediaError::Malformed));
        }
    }

    #[test]
    fn the_ack_of_an_introduction_confirms_its_seq() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let ack = seal_ack(&b, &pa, 9).unwrap();
        assert!(open_ack(&a, &pb, 9, &ack));
        assert!(!open_ack(&a, &pb, 10, &ack));
    }

    #[test]
    fn address_limits_are_enforced() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        for addr in ["", &"x".repeat(MAX_ADDR + 1)] {
            let r = AddressRecord { addr: addr.to_string(), ..record(1) };
            assert_eq!(seal_update(&a, &pb, &r), Err(MediaError::Malformed));
        }
        let r = AddressRecord { addr: "x".repeat(MAX_ADDR), ..record(1) };
        assert!(seal_update(&a, &pb, &r).is_ok());
    }

    #[test]
    fn the_removal_proves_who_sent_it_and_is_fresh() {
        let (a, b) = ids();
        let (a_pk, b_pk) = (PublicKey::from(&a), PublicKey::from(&b));
        let frame = seal_removal(&a, &b_pk, 10_000_000).unwrap();
        assert_eq!(frame.len(), PACKET_LEN);
        assert_eq!(open_removal(&b, &frame, 10_000_000), Some((a_pk.to_bytes(), 10_000_000)));
        assert_eq!(open_removal(&b, &frame, 10_000_000 + REMOVAL_WINDOW_MS), Some((a_pk.to_bytes(), 10_000_000)));
        assert_eq!(open_removal(&b, &frame, 10_000_000 + REMOVAL_WINDOW_MS + 1), None, "quadro velho");
        assert_eq!(open_removal(&b, &frame, 10_000_000 - REMOVAL_WINDOW_MS - 1), None, "quadro do futuro");
        let (c, _) = ids();
        assert_eq!(open_removal(&c, &frame, 10_000_000), None, "só o destinatário abre");
        assert_eq!(open_intro(&b, &frame), None);
    }

    #[test]
    fn the_removal_cannot_be_forged_with_someone_elses_key() {
        let (a, b) = ids();
        let (c, _) = ids();
        let mut frame = seal_removal(&c, &PublicKey::from(&b), 5).unwrap();
        // Troca o autor por `a`: a chave do par muda e a autenticação falha.
        frame[1..33].copy_from_slice(PublicKey::from(&a).as_bytes());
        assert_eq!(open_removal(&b, &frame, 5), None);
    }

    #[test]
    fn the_ack_of_a_removal_confirms_its_stamp() {
        let (a, b) = ids();
        let (a_pk, b_pk) = (PublicKey::from(&a), PublicKey::from(&b));
        let ack = seal_ack(&b, &a_pk, 77).unwrap();
        assert!(open_ack(&a, &b_pk, 77, &ack));
        assert!(!open_ack(&a, &b_pk, 78, &ack));
    }

    #[test]
    fn the_custody_carries_the_mail_to_the_keeper_who_cannot_read_it() {
        let (a, keeper) = ids();
        let b = StaticSecret::random_from_rng(OsRng);
        let (pk, pb) = (PublicKey::from(&keeper), PublicKey::from(&b));
        let mail = seal_intro(&a, &pb, &intro(5)).unwrap();
        let master = mailbox_master(pb.as_bytes());
        let frames = seal_custody(&a, &pk, 5, &master, &mail).unwrap();
        let mut parts = [[0u8; CUSTODY_DATA]; CUSTODY_PARTS];
        for (frame, slot) in frames.iter().zip(parts.iter_mut()) {
            assert_eq!(frame.len(), PACKET_LEN);
            let (author, part) = open_custody(&keeper, frame).unwrap();
            assert_eq!((author, part.id), (*PublicKey::from(&a).as_bytes(), 5));
            *slot = part.data;
            assert!(open_custody(&b, frame).is_none(), "só o guardião abre");
        }
        assert_eq!(join_custody(&parts), Some((master, mail)));
        assert!(open_intro(&keeper, &mail).is_none(), "o guardião não lê a apresentação");
        let mut bad = frames[0];
        bad[100] ^= 1;
        assert!(open_custody(&keeper, &bad).is_none());
    }

    #[test]
    fn the_mailbox_moves_every_day_and_slot_and_only_the_recipient_opens_it() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        let master = mailbox_master(pb.as_bytes());
        assert_ne!(master, mailbox_master(PublicKey::from(&a).as_bytes()));
        let base = mailbox_target(&master, 10, 0);
        assert_eq!(base, mailbox_target(&master, 10, 0));
        assert_ne!(base, mailbox_target(&master, 11, 0));
        assert_ne!(base, mailbox_target(&master, 10, 1));
        assert_ne!(base.0, base.1);
        let mail = seal_intro(&a, &pb, &intro(1_000_000)).unwrap();
        assert!(mail_slot(&mail) < MAIL_SLOTS);
        assert_eq!(mail_slot(&mail), mail_slot(&mail));
        assert!(open_mail(&b, &mail, 1_000_000).is_some());
        assert!(open_mail(&a, &mail, 1_000_000).is_none(), "outra chave");
        assert!(open_mail(&b, &mail, 1_000_000 + MAIL_TTL_MS).is_some());
        assert!(open_mail(&b, &mail, 1_000_001 + MAIL_TTL_MS).is_none(), "passou de uma semana");
        let future = seal_intro(&a, &pb, &intro(10_000_000 + REMOVAL_WINDOW_MS + 1)).unwrap();
        assert!(open_mail(&b, &future, 10_000_000).is_none(), "carimbo no futuro");
    }
}
