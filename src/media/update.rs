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
//! - A **resposta** à apresentação achada na caixa segue dois caminhos independentes: contatos de confiança de quem
//!   responde (a mesma custódia, com a caixa do autor) e o *buraco de resposta* do item ([`reply_master`]), onde todo
//!   guardião que ainda segura o item lê e regrava a resposta na caixa do autor.
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
pub const TAG_CHAT: u8 = 13;
pub const TAG_FILE: u8 = 14;
pub const TAG_CUSTODY: u8 = 11;
/// Etiqueta do convite de servidor na caixa de correio (não é um quadro de rede: só abre o item da DHT e a custódia).
pub const TAG_MAIL_INVITE: u8 = 12;
/// Quanto tempo uma apresentação na caixa de correio vale, a contar do carimbo dela (7 dias).
pub const MAIL_TTL_MS: u64 = 7 * 24 * 3_600_000;
/// Itens da caixa de um dia: quem grava escolhe um pelo hash do quadro, e quem lê consulta todos. Mais de um evita
/// que duas apresentações para a mesma pessoa se atropelem no mesmo item da DHT.
pub const MAIL_SLOTS: usize = 4;
/// Pedaços da custódia de uma apresentação (chave da caixa + quadro de 200 bytes): veja [`seal_custody`].
pub const CUSTODY_PARTS: usize = 2;
/// Tamanho máximo de um item da caixa na DHT (o teto de um valor BEP44).
pub const MAIL_MAX: usize = 1000;
/// Pedaços da custódia de um convite de servidor: 32 B da chave da caixa e até [`MAIL_MAX`] B de item cabem em 8.
pub const CUSTODY_MAX_PARTS: usize = 8;
const CUSTODY_PLAIN_LEN: usize = PACKET_LEN - 1 - 32 - NONCE_LEN - 16;
/// Dados de cada pedaço da custódia: o que sobra do quadro depois de versão, `id` e índice.
pub const CUSTODY_DATA: usize = CUSTODY_PLAIN_LEN - 1 - 8 - 1;
/// Diferença máxima, em ms, entre o carimbo da remoção e o relógio de quem recebe (1 h): cobre relógios tortos e
/// limita o que um quadro gravado pode fazer depois.
pub const REMOVAL_WINDOW_MS: u64 = 3_600_000;
/// Maior texto de uma mensagem do bate-papo 1:1 (bytes UTF-8): o que sobra do quadro de 200 bytes depois de
/// versão, carimbo e tamanho. Texto maior o app divide em várias mensagens.
pub const MAX_CHAT: usize = 128;
/// Conteúdo da mensagem: versão, carimbo, tamanho e o texto preenchido com zeros.
const CHAT_PLAIN_LEN: usize = 1 + 8 + 1 + MAX_CHAT;
/// Maior arquivo que o bate-papo transfere (32 MiB): a 500 quadros por segundo são uns 9 minutos, e o receptor guarda só
/// uma tabela de pedaços (~34 KB) na memória.
pub const MAX_FILE: u64 = 32 << 20;
/// Bytes de arquivo em cada quadro `Chunk`: o corpo de 128 menos índice (4) e tamanho (1).
pub const FILE_CHUNK: usize = MAX_CHAT - 5;
/// Maior nome de arquivo na oferta (bytes UTF-8): o corpo de 128 menos tamanho (8) e comprimento (1), com folga.
pub const MAX_FILE_NAME: usize = 100;
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

/// Mensagem do bate-papo entre dois contatos: `id` é o carimbo (ms Unix) do remetente, único por conversa, e é o
/// que o ack repete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chat {
    pub id: u64,
    pub text: String,
}

/// Quadro de bate-papo de `my_static` para `peer`. Falha se o texto for vazio, passar de [`MAX_CHAT`] bytes ou tiver
/// caractere de controle.
pub fn seal_chat(my_static: &StaticSecret, peer: &PublicKey, chat: &Chat) -> Result<Frame, MediaError> {
    if chat.text.is_empty() || chat.text.len() > MAX_CHAT || chat.text.chars().any(char::is_control) {
        return Err(MediaError::Malformed);
    }
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    let plain: [u8; CHAT_PLAIN_LEN] = padded(&[&[VERSION], &chat.id.to_be_bytes(), &[chat.text.len() as u8], chat.text.as_bytes()]);
    Ok(seal_after(&cipher, TAG_CHAT, me.as_bytes(), &plain))
}

/// Abre `frame` como mensagem de bate-papo para `my_static`. Devolve a chave de quem mandou (provada) e a mensagem.
/// `None`: não é bate-papo para esta chave, ou o carimbo está a mais de [`REMOVAL_WINDOW_MS`] de `now_ms` (um quadro
/// gravado não volta depois).
pub fn open_chat(my_static: &StaticSecret, frame: &[u8], now_ms: u64) -> Option<([u8; 32], Chat)> {
    let mut head = Reader::new(frame);
    if frame.len() != PACKET_LEN || head.u8()? != TAG_CHAT {
        return None;
    }
    let author: [u8; 32] = head.array()?;
    let owner = PublicKey::from(author);
    let cipher = cipher(my_static, &owner, &owner)?;
    let plain = open_after(&cipher, TAG_CHAT, 32, frame, CHAT_PLAIN_LEN)?;
    let mut r = Reader::new(&plain);
    if r.u8()? != VERSION {
        return None;
    }
    let id = r.u64()?;
    let len = usize::from(r.u8()?);
    if len == 0 || len > MAX_CHAT || id.abs_diff(now_ms) > REMOVAL_WINDOW_MS {
        return None;
    }
    let text = String::from_utf8(r.take(len)?.to_vec()).ok().filter(|t| !t.chars().any(char::is_control))?;
    Some((author, Chat { id, text }))
}

/// Quadro da transferência de arquivo entre dois contatos. `id` (carimbo em ms do remetente) liga os quadros da
/// mesma transferência.
///
/// ```text
/// remetente                          destinatário
///   Offer(nome, tamanho)  ─────────────>   a pessoa decide
///                         <─────────────   Answer(aceita?)
///   Chunk(0), Chunk(1) …  ─────────────>   cada um confirmado (ack)
///   Cancel                <────────────>   qualquer lado desiste
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileFrame {
    Offer { id: u64, size: u64, name: String },
    Answer { id: u64, accept: bool },
    Chunk { id: u64, index: u32, data: Vec<u8> },
    Cancel { id: u64 },
}

impl FileFrame {
    pub fn id(&self) -> u64 {
        match self {
            Self::Offer { id, .. } | Self::Answer { id, .. } | Self::Chunk { id, .. } | Self::Cancel { id } => *id,
        }
    }

    /// O `seq` que o ack deste quadro repete: o `id`, ou `id + 1 + índice` no pedaço (assim cada pedaço tem o seu).
    pub fn ack_seq(&self) -> u64 {
        match self {
            Self::Chunk { id, index, .. } => file_chunk_ack(*id, *index),
            other => other.id(),
        }
    }
}

/// `seq` do ack do pedaço `index` da transferência `id`.
pub fn file_chunk_ack(id: u64, index: u32) -> u64 {
    id.wrapping_add(u64::from(index)).wrapping_add(1)
}

/// Quadro de arquivo de `my_static` para `peer`. Falha se algum campo passa do que cabe (nome, pedaço), se o nome é
/// vazio ou tem caractere de controle, ou se o tamanho é zero ou passa de [`MAX_FILE`].
pub fn seal_file(my_static: &StaticSecret, peer: &PublicKey, frame: &FileFrame) -> Result<Frame, MediaError> {
    let (kind, id, body): (u8, u64, Vec<u8>) = match frame {
        FileFrame::Offer { id, size, name } => {
            if *size == 0 || *size > MAX_FILE || !valid_file_name(name) {
                return Err(MediaError::Malformed);
            }
            (0, *id, [&size.to_be_bytes()[..], &[name.len() as u8], name.as_bytes()].concat())
        }
        FileFrame::Answer { id, accept } => (1, *id, vec![u8::from(*accept)]),
        FileFrame::Chunk { id, index, data } => {
            if data.is_empty() || data.len() > FILE_CHUNK {
                return Err(MediaError::Malformed);
            }
            (2, *id, [&index.to_be_bytes()[..], &[data.len() as u8], data].concat())
        }
        FileFrame::Cancel { id } => (3, *id, Vec::new()),
    };
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    let plain: [u8; CHAT_PLAIN_LEN] = padded(&[&[VERSION, kind], &id.to_be_bytes(), &body]);
    Ok(seal_after(&cipher, TAG_FILE, me.as_bytes(), &plain))
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_FILE_NAME && !name.chars().any(char::is_control)
}

/// Abre `frame` como quadro de arquivo para `my_static`. Devolve a chave de quem mandou (provada) e o quadro. `None`:
/// não é de arquivo para esta chave, ou a oferta tem carimbo a mais de [`REMOVAL_WINDOW_MS`] de `now_ms`, tamanho
/// fora de `1..=MAX_FILE` ou nome inválido.
pub fn open_file(my_static: &StaticSecret, frame: &[u8], now_ms: u64) -> Option<([u8; 32], FileFrame)> {
    let mut head = Reader::new(frame);
    if frame.len() != PACKET_LEN || head.u8()? != TAG_FILE {
        return None;
    }
    let author: [u8; 32] = head.array()?;
    let owner = PublicKey::from(author);
    let cipher = cipher(my_static, &owner, &owner)?;
    let plain = open_after(&cipher, TAG_FILE, 32, frame, CHAT_PLAIN_LEN)?;
    let mut r = Reader::new(&plain);
    if r.u8()? != VERSION {
        return None;
    }
    let kind = r.u8()?;
    let id = r.u64()?;
    let parsed = match kind {
        0 => {
            let size = r.u64()?;
            let len = usize::from(r.u8()?);
            let name = String::from_utf8(r.take(len)?.to_vec()).ok()?;
            if size == 0 || size > MAX_FILE || !valid_file_name(&name) || id.abs_diff(now_ms) > REMOVAL_WINDOW_MS {
                return None;
            }
            FileFrame::Offer { id, size, name }
        }
        1 => FileFrame::Answer { id, accept: match r.u8()? { 0 => false, 1 => true, _ => return None } },
        2 => {
            let index = r.u32()?;
            let len = usize::from(r.u8()?);
            if len == 0 || len > FILE_CHUNK {
                return None;
            }
            FileFrame::Chunk { id, index, data: r.take(len)?.to_vec() }
        }
        3 => FileFrame::Cancel { id },
        _ => return None,
    };
    Some((author, parsed))
}

/// Um pedaço da custódia: o `id` (o `seq` da apresentação) liga os pedaços do mesmo depósito.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustodyPart {
    pub id: u64,
    pub idx: u8,
    /// Quantos pedaços tem o depósito (de 1 a [`CUSTODY_MAX_PARTS`]).
    pub total: u8,
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

/// Chave do *buraco de resposta* do item `item` (o que foi lido da caixa de quem responde, ou guardado em custódia): um hash do
/// próprio item. Quem tem o item calcula onde, a cada dia, a resposta é deixada ([`mailbox_target`] com esta chave): o destinatário
/// grava lá e todo guardião que segura o item lê, sem que o destinatário precise saber quem são os guardiões nem ser
/// contato deles. O lugar não prova nada (qualquer um com o item o calcula): a resposta é fechada para o autor e só ele a abre.
pub fn reply_master(item: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, &Sha256::digest(item)).expand(b"clam/caixa/resposta/v1", &mut out).expect("32 bytes é válido");
    out
}

/// Qual item da caixa leva este quadro: o mesmo quadro cai sempre no mesmo item (os guardiões gravam o mesmo valor).
pub fn mail_slot(mail: &[u8]) -> usize {
    use sha2::Digest;
    Sha256::digest(mail).first().map_or(0, |b| usize::from(*b) % MAIL_SLOTS)
}

/// Quem assinou o quadro de apresentação `frame` (a chave vem em claro, sem abrir). `None`: não é um quadro de apresentação.
/// Serve ao guardião para saber se quem deixou o quadro é o próprio autor (depósito original) ou um guardião que repassou.
pub fn intro_author(frame: &[u8]) -> Option<[u8; 32]> {
    let mut head = Reader::new(frame);
    if frame.len() != PACKET_LEN || head.u8()? != TAG_INTRO {
        return None;
    }
    head.array()
}

/// O item da caixa para um convite de servidor: etiqueta, tamanho e o convite cru (cifrado só para o convidado, com
/// servidor, autor e convidado em claro no começo). `None` se passa de [`MAIL_MAX`] ou não tem o começo.
pub fn invite_mail_item(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < INVITE_AUTHOR_AT + 32 || raw.len() + 3 > MAIL_MAX {
        return None;
    }
    let len = u16::try_from(raw.len()).ok()?;
    Some([&[TAG_MAIL_INVITE][..], &len.to_be_bytes(), raw].concat())
}

/// O começo do convite cru de servidor: 16 B do servidor e depois a chave de quem convidou (veja `server::invite`).
const INVITE_AUTHOR_AT: usize = 16;

/// Quanto do começo de `bytes` é um item de caixa: 200 B de apresentação ou o convite com o tamanho. `None`: nenhum
/// dos dois (inclui o que passa de [`MAIL_MAX`]). Sobra (o enchimento do último pedaço de custódia) fica de fora.
pub fn mail_item(bytes: &[u8]) -> Option<&[u8]> {
    let len = match *bytes.first()? {
        TAG_INTRO => PACKET_LEN,
        TAG_MAIL_INVITE => {
            let mut r = Reader::new(bytes.get(1..)?);
            let raw = usize::from(r.u16()?);
            if raw < INVITE_AUTHOR_AT + 32 {
                return None;
            }
            raw.checked_add(3)?
        }
        _ => return None,
    };
    if len > MAIL_MAX {
        return None;
    }
    bytes.get(..len)
}

/// Quem assinou o item de caixa (a chave vem em claro): o autor da apresentação ou quem convidou.
pub fn mail_author(item: &[u8]) -> Option<[u8; 32]> {
    match *item.first()? {
        TAG_INTRO => intro_author(item),
        TAG_MAIL_INVITE => item.get(3 + INVITE_AUTHOR_AT..3 + INVITE_AUTHOR_AT + 32)?.try_into().ok(),
        _ => None,
    }
}

/// O que se acha na caixa de uma pessoa.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailItem {
    /// Apresentação de contato: quem a fez e o cartão.
    Intro([u8; 32], Intro),
    /// Convite de servidor cru (para `Invite::open`; o prazo é o da custódia, o item não leva carimbo).
    Invite(Vec<u8>),
}

/// Abre `value`, lido da caixa de `my_static`. Apresentação: como [`open_mail`]. Convite de servidor: só confere o
/// formato do item; abri-lo é com `server::invite::Invite::open`, que sabe se é para esta chave.
pub fn open_mail_item(my_static: &StaticSecret, value: &[u8], now_ms: u64) -> Option<MailItem> {
    match *value.first()? {
        TAG_INTRO => open_mail(my_static, value, now_ms).map(|(a, i)| MailItem::Intro(a, i)),
        TAG_MAIL_INVITE => {
            let item = mail_item(value).filter(|i| i.len() == value.len())?;
            item.get(3..).map(|raw| MailItem::Invite(raw.to_vec()))
        }
        _ => None,
    }
}

/// Abre `value`, lido da caixa de `my_static`, como apresentação ainda dentro do prazo. `None`: não é uma
/// apresentação para esta chave, ou o carimbo dela passa de [`MAIL_TTL_MS`] ou está mais de uma hora no futuro.
pub fn open_mail(my_static: &StaticSecret, value: &[u8], now_ms: u64) -> Option<([u8; 32], Intro)> {
    let (author, intro) = open_intro(my_static, value)?;
    let fresh = intro.seq <= now_ms.saturating_add(REMOVAL_WINDOW_MS) && now_ms.saturating_sub(intro.seq) <= MAIL_TTL_MS;
    fresh.then_some((author, intro))
}

/// Os quadros com que `my_static` deixa com `peer` (o guardião) o item de caixa `item` (veja [`mail_item`]) e a chave
/// da caixa do destinatário. `id` é o carimbo (ms): o `seq` da apresentação, ou a hora do convite. Uma apresentação
/// (232 B) leva [`CUSTODY_PARTS`] quadros; um convite, até [`CUSTODY_MAX_PARTS`].
pub fn seal_custody(my_static: &StaticSecret, peer: &PublicKey, id: u64, master: &[u8; 32], item: &[u8]) -> Result<Vec<Frame>, MediaError> {
    let me = PublicKey::from(my_static);
    let cipher = cipher(my_static, peer, &me).ok_or(MediaError::BadKey)?;
    if mail_item(item).is_none_or(|i| i.len() != item.len()) {
        return Err(MediaError::Malformed);
    }
    let payload = [&master[..], item].concat();
    let total = payload.len().div_ceil(CUSTODY_DATA);
    // Dois pedaços é o formato da primeira versão (o campo vale 0): quem só conhece esse continua a ler as apresentações.
    let code = if total == CUSTODY_PARTS { 0 } else { total as u8 };
    Ok(payload
        .chunks(CUSTODY_DATA)
        .enumerate()
        .map(|(idx, chunk)| {
            let data = padded::<CUSTODY_DATA>(&[chunk]);
            let plain: [u8; CUSTODY_PLAIN_LEN] = padded(&[&[VERSION], &id.to_be_bytes(), &[code << 4 | idx as u8], &data]);
            seal_after(&cipher, TAG_CUSTODY, me.as_bytes(), &plain)
        })
        .collect())
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
    let (id, at) = (r.u64()?, r.u8()?);
    let (code, idx) = (at >> 4, at & 0x0f);
    let total = if code == 0 { CUSTODY_PARTS as u8 } else { code };
    if usize::from(total) > CUSTODY_MAX_PARTS || idx >= total {
        return None;
    }
    Some((author, CustodyPart { id, idx, total, data: r.array()? }))
}

/// Junta os pedaços (em ordem) e devolve a chave da caixa e o item de caixa. `None`: faltou pedaço ou o que sobrou não
/// é um item.
pub fn join_custody(parts: &[[u8; CUSTODY_DATA]]) -> Option<([u8; 32], Vec<u8>)> {
    let all: Vec<u8> = parts.iter().flatten().copied().collect();
    let mut r = Reader::new(&all);
    let master = r.array()?;
    let item = mail_item(all.get(32..)?)?.to_vec();
    Some((master, item))
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
    fn chat_opens_only_for_the_recipient_and_only_while_fresh() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let chat = Chat { id: 1_000_000, text: "olá, tudo bem? ção".into() };
        let frame = seal_chat(&a, &pb, &chat).unwrap();
        assert_eq!(frame.len(), PACKET_LEN);
        assert_eq!(open_chat(&b, &frame, 1_000_500), Some((*pa.as_bytes(), chat.clone())));
        assert_eq!(open_chat(&a, &frame, 1_000_500), None, "o remetente não abre o próprio quadro");
        assert_eq!(open_chat(&StaticSecret::random_from_rng(OsRng), &frame, 1_000_500), None);
        assert_eq!(open_chat(&b, &frame, 1_000_000 + REMOVAL_WINDOW_MS + 1), None, "quadro velho");
        assert!(open_ack(&a, &pb, 1, &seal_ack(&b, &pa, 1).unwrap()));
        assert!(!frame.windows(3).any(|w| w == b"ol\xc3\xa1"), "o texto não aparece em claro");
    }

    #[test]
    fn file_frames_round_trip_only_for_the_recipient() {
        let (a, b) = ids();
        let (pa, pb) = (PublicKey::from(&a), PublicKey::from(&b));
        let frames = [
            FileFrame::Offer { id: 1_000_000, size: 4_000_000, name: "relatório ção.pdf".into() },
            FileFrame::Answer { id: 7, accept: true },
            FileFrame::Answer { id: 7, accept: false },
            FileFrame::Chunk { id: 7, index: 272_000, data: vec![0xAB; FILE_CHUNK] },
            FileFrame::Chunk { id: 7, index: 0, data: vec![1] },
            FileFrame::Cancel { id: 7 },
        ];
        for f in frames {
            let sealed = seal_file(&a, &pb, &f).unwrap();
            assert_eq!(sealed.len(), PACKET_LEN);
            assert_eq!(open_file(&b, &sealed, 1_000_500), Some((*pa.as_bytes(), f.clone())));
            assert_eq!(open_file(&a, &sealed, 1_000_500), None);
            assert_eq!(open_chat(&b, &sealed, 1_000_500), None, "não se confunde com texto");
            assert!(open_ack(&a, &pb, f.ack_seq(), &seal_ack(&b, &pa, f.ack_seq()).unwrap()));
        }
        let offer = seal_file(&a, &pb, &FileFrame::Offer { id: 1, size: 10, name: "a".into() }).unwrap();
        assert_eq!(open_file(&b, &offer, 1 + REMOVAL_WINDOW_MS + 1), None, "oferta velha");
    }

    #[test]
    fn file_frames_refuse_what_does_not_fit() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        let offer = |size, name: &str| FileFrame::Offer { id: 1, size, name: name.into() };
        assert!(seal_file(&a, &pb, &offer(1, "a.txt")).is_ok());
        assert!(seal_file(&a, &pb, &offer(MAX_FILE, &"n".repeat(MAX_FILE_NAME))).is_ok());
        for bad in [offer(0, "a"), offer(MAX_FILE + 1, "a"), offer(1, ""), offer(1, "a\nb"), offer(1, &"n".repeat(MAX_FILE_NAME + 1))] {
            assert!(seal_file(&a, &pb, &bad).is_err(), "{bad:?}");
        }
        for data in [vec![], vec![0; FILE_CHUNK + 1]] {
            assert!(seal_file(&a, &pb, &FileFrame::Chunk { id: 1, index: 0, data }).is_err());
        }
        // Cada pedaço da mesma transferência tem o seu ack.
        assert_ne!(file_chunk_ack(5, 0), file_chunk_ack(5, 1));
        assert_ne!(file_chunk_ack(5, 0), 5);
    }

    #[test]
    fn chat_refuses_empty_long_and_control_text() {
        let (a, b) = ids();
        let pb = PublicKey::from(&b);
        for text in [String::new(), "x".repeat(MAX_CHAT + 1), "a\nb".to_string()] {
            assert!(seal_chat(&a, &pb, &Chat { id: 1, text }).is_err());
        }
        assert!(seal_chat(&a, &pb, &Chat { id: 1, text: "x".repeat(MAX_CHAT) }).is_ok());
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
    fn the_author_of_an_introduction_shows_without_opening_it() {
        let (a, b) = ids();
        let frame = seal_intro(&a, &PublicKey::from(&b), &intro(3)).unwrap();
        assert_eq!(intro_author(&frame), Some(*PublicKey::from(&a).as_bytes()));
        assert_eq!(intro_author(&frame[..PACKET_LEN - 1]), None);
        let mut other = frame;
        other[0] = TAG_CUSTODY;
        assert_eq!(intro_author(&other), None);
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

    fn invite_raw(len: usize, author: [u8; 32]) -> Vec<u8> {
        [&[7u8; 16][..], &author, &vec![9u8; len - 48]].concat()
    }

    #[test]
    fn a_server_invite_travels_in_many_custody_parts_and_comes_back_whole() {
        let (a, keeper) = ids();
        let author = *PublicKey::from(&a).as_bytes();
        let master = [5u8; 32];
        for len in [80, 300, 700, MAIL_MAX - 3] {
            let item = invite_mail_item(&invite_raw(len, author)).unwrap();
            let frames = seal_custody(&a, &PublicKey::from(&keeper), 9, &master, &item).unwrap();
            assert_eq!(frames.len(), (32 + item.len()).div_ceil(CUSTODY_DATA));
            let mut parts = vec![[0u8; CUSTODY_DATA]; frames.len()];
            for (frame, slot) in frames.iter().zip(parts.iter_mut()) {
                let (_, part) = open_custody(&keeper, frame).unwrap();
                assert_eq!((part.id, usize::from(part.total)), (9, frames.len()));
                *slot = part.data;
            }
            assert_eq!(join_custody(&parts), Some((master, item.clone())), "len {len}");
            assert_eq!(mail_author(&item), Some(author));
            assert!(matches!(open_mail_item(&keeper, &item, 0), Some(MailItem::Invite(raw)) if raw == invite_raw(len, author)));
        }
        assert!(join_custody(&[[0u8; CUSTODY_DATA]]).is_none(), "faltou pedaço");
    }

    #[test]
    fn the_invite_item_has_limits() {
        assert!(invite_mail_item(&invite_raw(MAIL_MAX - 2, [1; 32])).is_none(), "passa do valor da DHT");
        assert!(invite_mail_item(&[0u8; 20]).is_none(), "sem o começo");
        let item = invite_mail_item(&invite_raw(100, [1; 32])).unwrap();
        assert!(mail_item(&item[..item.len() - 1]).is_none(), "cortado");
        assert_eq!(mail_item(&[item.clone(), vec![0; 40]].concat()), Some(&item[..]), "enchimento fica de fora");
        assert!(mail_item(&[3, 0, 0]).is_none());
        assert!(mail_author(&[TAG_MAIL_INVITE, 0]).is_none());
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
        assert_eq!(join_custody(&parts), Some((master, mail.to_vec())));
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
