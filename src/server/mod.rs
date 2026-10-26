//! Servidores e salas: grupos de pessoas com salas de voz, sem servidor de verdade (veja `docs/SERVIDORES.md`).
//!
//! O estado de um servidor (membros, papéis, salas) é um **registro de operações assinadas** que cada membro
//! guarda inteiro ([`registry`]). A presença (quem está online e em qual sala) e a troca de operações andam em
//! quadros de 200 bytes cifrados par a par ([`wire`]). O [`engine::Servers`] junta tudo sem tocar a rede
//! (*sans-IO*): o [`Node`](crate::node::Node) entrega e recebe os quadros.

pub mod engine;
pub mod invite;
pub mod op;
pub mod registry;
pub mod wire;

use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::StaticSecret;

pub use op::{Action, Op, Role};
pub use engine::{Event, MemberView, Outbound, Output, RoomView, ServerView, Servers};
pub use registry::{Registry, State};

/// Identificador do servidor (aleatório, só circula em convites e dentro dos quadros).
pub type ServerId = [u8; 16];
/// Identificador de uma sala.
pub type RoomId = [u8; 16];
/// Os 8 primeiros bytes da chave pública de um membro: é como as operações e os quadros dizem quem é.
pub type MemberId = [u8; 8];

/// Membros por servidor.
pub const MAX_MEMBERS: usize = 50;
/// Salas por servidor.
pub const MAX_ROOMS: usize = 32;
/// Pessoas por sala (a malha de chamadas cresce com o quadrado).
pub const MAX_ROOM_PEOPLE: usize = 8;
/// Bytes do nome de um servidor, de uma sala ou de um membro.
pub const MAX_NAME: usize = 32;

pub fn member_id(key: &[u8; 32]) -> MemberId {
    key[..8].try_into().expect("8 bytes")
}

/// Chave de assinatura das operações, derivada da identidade: quem tem a chave secreta da identidade é
/// quem assina; a chave de verificação viaja no `AddMember` de cada membro.
pub fn signing_key(secret: &StaticSecret) -> ed25519_dalek::SigningKey {
    let mut seed = [0u8; 32];
    Hkdf::<Sha256>::new(None, &secret.to_bytes()).expand(b"clam/servidor/assinatura/v1", &mut seed).expect("32 bytes é válido");
    ed25519_dalek::SigningKey::from_bytes(&seed)
}

/// Nome aceito para servidor, sala ou membro: 1 a [`MAX_NAME`] bytes, sem caracteres de controle.
pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= MAX_NAME && !name.chars().any(char::is_control)
}

/// Ajusta um texto digitado para virar nome: tira o que não pode, junta espaços e corta em [`MAX_NAME`] bytes.
pub fn clean_name(raw: &str) -> String {
    let joined = raw.chars().filter(|c| !c.is_control()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for c in joined.chars() {
        if out.len() + c.len_utf8() > MAX_NAME {
            break;
        }
        out.push(c);
    }
    out
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServerError {
    #[error("servidor desconhecido")]
    UnknownServer,
    #[error("você não é membro deste servidor")]
    NotMember,
    #[error("sem permissão para isso")]
    Forbidden,
    #[error("nome inválido (1 a {MAX_NAME} bytes, sem caracteres de controle)")]
    BadName,
    #[error("convite inválido")]
    BadInvite,
    #[error("este convite é de outra pessoa")]
    NotForYou,
    #[error("você já está neste servidor")]
    AlreadyIn,
    #[error("limite atingido")]
    Full,
    #[error("sala desconhecida")]
    UnknownRoom,
    #[error("membro desconhecido")]
    UnknownMember,
    #[error("o seu endereço ainda não é conhecido (espere o relay ficar pronto)")]
    NoAddress,
}
