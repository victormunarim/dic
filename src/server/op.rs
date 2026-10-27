//! Operações do registro: o que muda o estado de um servidor, assinada por quem a fez.
//!
//! ```text
//! tipo(1) ‖ autor(8) ‖ lamport(4) ‖ seq(4) ‖ tamanho do corpo(1) ‖ corpo ‖ assinatura ed25519(64)
//! ```
//!
//! A assinatura cobre o `server_id`, que não viaja na operação (o quadro já diz de que servidor ela é).
//! `seq` conta as operações de cada autor (1, 2, 3...) e `lamport` ordena as de autores diferentes.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use super::{MAX_NAME, MemberId, RoomId, ServerId, valid_name};

/// Maior corpo de uma operação: cabe, com o cabeçalho e a assinatura, num quadro de servidor.
pub const MAX_BODY: usize = 75;
const HEAD: usize = 1 + 8 + 4 + 4 + 1;
const SIG: usize = 64;
/// Maior operação codificada.
pub const MAX_OP_LEN: usize = HEAD + MAX_BODY + SIG;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    fn code(self) -> u8 {
        match self {
            Role::Owner => 0,
            Role::Admin => 1,
            Role::Member => 2,
        }
    }

    pub(crate) fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => Role::Owner,
            1 => Role::Admin,
            2 => Role::Member,
            _ => return None,
        })
    }

    /// Dono e admins gerenciam os outros.
    pub fn manages(self) -> bool {
        self != Role::Member
    }
}

/// O que a operação faz.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Quem assina convida `key` (com a chave de verificação dele). A primeira do registro é o dono se juntando.
    AddMember { key: [u8; 32], vk: [u8; 32] },
    RemoveMember { target: MemberId },
    SetRole { target: MemberId, role: Role },
    CreateRoom { room: RoomId, name: String },
    RenameRoom { room: RoomId, name: String },
    DeleteRoom { room: RoomId },
    /// O segredo do servidor passa para a época `epoch` (a entrega do segredo novo é à parte, par a par).
    /// `commit` é o [`secret_commit`](super::secret_commit) do segredo novo; zerado, é um `Rekey` do formato antigo.
    Rekey { epoch: u16, commit: [u8; 32] },
    /// Quem assina diz como quer ser chamado.
    SetName { name: String },
}

impl Action {
    fn code(&self) -> u8 {
        match self {
            Action::AddMember { .. } => 1,
            Action::RemoveMember { .. } => 2,
            Action::SetRole { .. } => 3,
            Action::CreateRoom { .. } => 4,
            Action::RenameRoom { .. } => 5,
            Action::DeleteRoom { .. } => 6,
            Action::Rekey { .. } => 7,
            Action::SetName { .. } => 8,
        }
    }

    fn body(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Action::AddMember { key, vk } => {
                b.extend_from_slice(key);
                b.extend_from_slice(vk);
            }
            Action::RemoveMember { target } => b.extend_from_slice(target),
            Action::SetRole { target, role } => {
                b.extend_from_slice(target);
                b.push(role.code());
            }
            Action::CreateRoom { room, name } | Action::RenameRoom { room, name } => {
                b.extend_from_slice(room);
                b.extend_from_slice(name.as_bytes());
            }
            Action::DeleteRoom { room } => b.extend_from_slice(room),
            Action::Rekey { epoch, commit } => {
                b.extend_from_slice(&epoch.to_be_bytes());
                if *commit != [0; 32] {
                    b.extend_from_slice(commit);
                }
            }
            Action::SetName { name } => b.extend_from_slice(name.as_bytes()),
        }
        b
    }

    fn parse(code: u8, b: &[u8]) -> Option<Self> {
        let name = |raw: &[u8]| String::from_utf8(raw.to_vec()).ok().filter(|n| valid_name(n));
        Some(match code {
            1 if b.len() == 64 => Action::AddMember { key: b[..32].try_into().ok()?, vk: b[32..].try_into().ok()? },
            2 if b.len() == 8 => Action::RemoveMember { target: b.try_into().ok()? },
            3 if b.len() == 9 => Action::SetRole { target: b[..8].try_into().ok()?, role: Role::from_code(b[8])? },
            4 | 5 if b.len() > 16 && b.len() <= 16 + MAX_NAME => {
                let (room, name) = (b[..16].try_into().ok()?, name(&b[16..])?);
                if code == 4 { Action::CreateRoom { room, name } } else { Action::RenameRoom { room, name } }
            }
            6 if b.len() == 16 => Action::DeleteRoom { room: b.try_into().ok()? },
            7 if b.len() == 2 || b.len() == 34 => Action::Rekey {
                epoch: u16::from_be_bytes(b[..2].try_into().ok()?),
                commit: if b.len() == 34 { b[2..].try_into().ok()? } else { [0; 32] },
            },
            8 if !b.is_empty() && b.len() <= MAX_NAME => Action::SetName { name: name(b)? },
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub author: MemberId,
    pub lamport: u32,
    pub seq: u32,
    pub action: Action,
    pub sig: [u8; SIG],
}

impl Op {
    /// Posição na ordem em que as operações valem.
    pub fn order(&self) -> (u32, MemberId, u32) {
        (self.lamport, self.author, self.seq)
    }

    fn unsigned(&self) -> Vec<u8> {
        let body = self.action.body();
        let mut v = Vec::with_capacity(HEAD + body.len());
        v.push(self.action.code());
        v.extend_from_slice(&self.author);
        v.extend_from_slice(&self.lamport.to_be_bytes());
        v.extend_from_slice(&self.seq.to_be_bytes());
        v.push(body.len() as u8);
        v.extend_from_slice(&body);
        v
    }

    fn message(&self, server: &ServerId) -> Vec<u8> {
        let mut m = b"clam/servidor/op/v1".to_vec();
        m.extend_from_slice(server);
        m.extend_from_slice(&self.unsigned());
        m
    }

    /// Cria e assina uma operação.
    pub fn sign(server: &ServerId, key: &SigningKey, author: MemberId, lamport: u32, seq: u32, action: Action) -> Self {
        let mut op = Op { author, lamport, seq, action, sig: [0; SIG] };
        op.sig = key.sign(&op.message(server)).to_bytes();
        op
    }

    pub fn verify(&self, server: &ServerId, vk: &[u8; 32]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(vk) else { return false };
        vk.verify(&self.message(server), &Signature::from_bytes(&self.sig)).is_ok()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = self.unsigned();
        v.extend_from_slice(&self.sig);
        v
    }

    /// Lê uma operação inteira (sem conferir a assinatura: isso depende de quem é o autor).
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < HEAD + SIG {
            return None;
        }
        let body_len = b[HEAD - 1] as usize;
        if body_len > MAX_BODY || b.len() != HEAD + body_len + SIG {
            return None;
        }
        let action = Action::parse(b[0], &b[HEAD..HEAD + body_len])?;
        Some(Op {
            author: b[1..9].try_into().ok()?,
            lamport: u32::from_be_bytes(b[9..13].try_into().ok()?),
            seq: u32::from_be_bytes(b[13..17].try_into().ok()?),
            action,
            sig: b[HEAD + body_len..].try_into().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn sample() -> Vec<Action> {
        vec![
            Action::AddMember { key: [1; 32], vk: [2; 32] },
            Action::RemoveMember { target: [3; 8] },
            Action::SetRole { target: [3; 8], role: Role::Admin },
            Action::CreateRoom { room: [4; 16], name: "geral".into() },
            Action::RenameRoom { room: [4; 16], name: "papo".into() },
            Action::DeleteRoom { room: [4; 16] },
            Action::Rekey { epoch: 3, commit: [8; 32] },
            Action::Rekey { epoch: 4, commit: [0; 32] },
            Action::SetName { name: "Ana".into() },
        ]
    }

    #[test]
    fn every_action_round_trips_and_fits_the_frame() {
        for a in sample() {
            let op = Op::sign(&[9; 16], &key(), [5; 8], 7, 2, a);
            let bytes = op.encode();
            assert!(bytes.len() <= MAX_OP_LEN);
            assert_eq!(Op::decode(&bytes), Some(op));
        }
        let long = Action::CreateRoom { room: [0; 16], name: "x".repeat(MAX_NAME) };
        assert!(Op::sign(&[9; 16], &key(), [5; 8], 1, 1, long).encode().len() <= MAX_OP_LEN);
    }

    #[test]
    fn the_signature_covers_the_server_and_every_field() {
        let vk = key().verifying_key().to_bytes();
        let op = Op::sign(&[9; 16], &key(), [5; 8], 7, 2, Action::DeleteRoom { room: [4; 16] });
        assert!(op.verify(&[9; 16], &vk));
        assert!(!op.verify(&[8; 16], &vk), "outro servidor");
        for edit in [|o: &mut Op| o.lamport += 1, |o: &mut Op| o.seq += 1, |o: &mut Op| o.author[0] ^= 1, |o: &mut Op| o.action = Action::DeleteRoom { room: [5; 16] }] {
            let mut bad = op.clone();
            edit(&mut bad);
            assert!(!bad.verify(&[9; 16], &vk));
        }
    }

    #[test]
    fn malformed_bytes_are_refused() {
        let op = Op::sign(&[9; 16], &key(), [5; 8], 1, 1, Action::SetName { name: "Ana".into() });
        let good = op.encode();
        assert!(Op::decode(&good[..good.len() - 1]).is_none());
        let mut longer = good.clone();
        longer.push(0);
        assert!(Op::decode(&longer).is_none());
        let mut kind = good.clone();
        kind[0] = 99;
        assert!(Op::decode(&kind).is_none());
        let mut role = Op::sign(&[9; 16], &key(), [5; 8], 1, 1, Action::SetRole { target: [1; 8], role: Role::Admin }).encode();
        role[HEAD + 8] = 9;
        assert!(Op::decode(&role).is_none());
        let mut ctl = good;
        ctl[HEAD] = 0x07;
        assert!(Op::decode(&ctl).is_none(), "nome com caractere de controle");
        for n in 0..256 {
            let junk: Vec<u8> = (0..n).map(|i| (i * 31 + n) as u8).collect();
            let _ = Op::decode(&junk);
        }
    }
}
