//! O registro de operações de um servidor e o estado que sai dele.
//!
//! Cada membro guarda todas as operações. O estado é a dobra delas na ordem `(lamport, autor, seq)`: uma
//! operação só vale se, naquele ponto da ordem, o autor era membro, a assinatura confere e a permissão existe.
//! Chegou uma operação mais antiga que outras já guardadas, o estado é refeito do começo, então todos
//! convergem para o mesmo estado ao terem as mesmas operações, em qualquer ordem de chegada.
//!
//! O registro tem teto, e o teto não é de ninguém só: quem já foi membro tem a assinatura conferida na entrada
//! (uma operação falsa com o nome de outro não gasta a cota dele nem toma o lugar da verdadeira), cada autor
//! guarda até [`MAX_OPS_PER_AUTHOR`], quem nunca foi membro até [`MAX_STRANGER_OPS`] no total, e as últimas
//! [`OWNER_RESERVE`] vagas só o dono ocupa, para ele remover e trocar a chave mesmo com o resto cheio.
//!
//! O relógio `lamport` não pode saltar: uma operação cujo `lamport` passa de um acima do maior já visto na
//! ordem é ignorada (volta a valer quando as que faltam chegam). Sem isso um membro assinaria `u32::MAX` e
//! ninguém mais conseguiria fazer a próxima operação.

use std::collections::{BTreeMap, HashMap};

use ed25519_dalek::{SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use super::{
    MAX_MEMBERS, MAX_ROOMS, MemberId, RoomId, ServerId, member_id,
    op::{Action, Op, Role},
};

/// Operações guardadas por servidor; passando disto, as novas são recusadas.
pub const MAX_OPS: usize = 4096;
/// Operações de um mesmo autor (menos o dono); passando disto, as novas dele são recusadas. Um membro sozinho
/// não enche o registro.
pub const MAX_OPS_PER_AUTHOR: usize = 512;
/// Operações de autores que nunca foram membros aqui (ainda não convidados, ou lixo) e cuja assinatura, por
/// isso, não dá para conferir. Passando disto, as novas são recusadas (voltam quando o autor for conhecido).
pub const MAX_STRANGER_OPS: usize = 64;
/// Das `MAX_OPS` vagas, as últimas só o dono ocupa.
pub const OWNER_RESERVE: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub key: [u8; 32],
    pub vk: [u8; 32],
    pub role: Role,
    pub name: String,
    pub added_by: MemberId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    pub name: String,
    pub creator: MemberId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub members: BTreeMap<MemberId, Member>,
    pub rooms: BTreeMap<RoomId, Room>,
    pub epoch: u16,
    /// `secret_commit` do segredo da época atual (zerado antes do primeiro `Rekey`, ou num `Rekey` antigo).
    pub commit: [u8; 32],
}

impl State {
    pub fn role_of(&self, id: &MemberId) -> Option<Role> {
        self.members.get(id).map(|m| m.role)
    }
}

/// O que aconteceu ao inserir uma operação.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Added,
    /// Já estava guardada.
    Known,
    /// Não entra: registro cheio, ou conflita com outra operação do mesmo autor e `seq`.
    Refused,
}

struct Entry {
    op: Op,
    /// Resultado da conferência da assinatura com a chave de verificação que valia na hora.
    checked: Option<([u8; 32], bool)>,
}

pub struct Registry {
    server: ServerId,
    owner_vk: [u8; 32],
    ops: BTreeMap<(u32, MemberId, u32), Entry>,
    by_seq: HashMap<(MemberId, u32), (u32, MemberId, u32)>,
    state: State,
    /// Maior `lamport` que conta, depois da última dobra: o relógio de onde sai a próxima operação.
    clock: u32,
    /// Chave de verificação de quem já foi membro (a última com que entrou), mesmo removido depois.
    vks: HashMap<MemberId, [u8; 32]>,
}

impl Registry {
    /// Registro vazio de `server`; `owner_vk` é a chave de verificação do dono, que o convite traz e que só
    /// vale para a primeira operação (o dono se juntando).
    pub fn new(server: ServerId, owner_vk: [u8; 32]) -> Self {
        Self { server, owner_vk, ops: BTreeMap::new(), by_seq: HashMap::new(), state: State::default(), clock: 0, vks: HashMap::new() }
    }

    pub fn server(&self) -> &ServerId {
        &self.server
    }

    pub fn owner_vk(&self) -> &[u8; 32] {
        &self.owner_vk
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        self.ops.values().map(|e| &e.op)
    }

    /// Guarda uma operação vinda de fora (ou feita aqui) e refaz o estado.
    pub fn insert(&mut self, op: Op) -> Insert {
        let key = op.order();
        // De quem já foi membro a assinatura se confere agora: sem isso qualquer um escreveria com o nome de outro
        // para gastar a cota dele, ou para tomar o lugar da operação verdadeira de mesmo autor e `seq`.
        let known = self.vks.get(&op.author).copied();
        if let Some(vk) = &known
            && !op.verify(&self.server, vk)
        {
            return Insert::Refused;
        }
        if let Some(old) = self.by_seq.get(&(op.author, op.seq)).copied() {
            if old == key && self.ops.get(&old).is_some_and(|e| e.op == op) {
                return Insert::Known;
            }
            // Duas operações diferentes do mesmo autor e `seq` (quem fez isso fez de propósito): fica a menor
            // na ordem dos bytes, igual para todos.
            let keep_new = op.encode() < self.ops[&old].op.encode();
            if !keep_new {
                return Insert::Refused;
            }
            self.ops.remove(&old);
        } else if !self.has_room(&op.author, known.is_some()) {
            return Insert::Refused;
        }
        self.by_seq.insert((op.author, op.seq), key);
        self.ops.insert(key, Entry { checked: known.map(|vk| (vk, true)), op });
        self.refold();
        Insert::Added
    }

    /// Cabe mais uma operação de `author`? `known`: ele já foi membro (a assinatura foi conferida).
    fn has_room(&self, author: &MemberId, known: bool) -> bool {
        let owner = self.vks.get(author) == Some(&self.owner_vk);
        if self.ops.len() >= MAX_OPS || (!owner && self.ops.len() >= MAX_OPS - OWNER_RESERVE) {
            return false;
        }
        if !owner && self.ops.values().filter(|e| e.op.author == *author).count() >= MAX_OPS_PER_AUTHOR {
            return false;
        }
        known || self.ops.values().filter(|e| !self.vks.contains_key(&e.op.author)).count() < MAX_STRANGER_OPS
    }

    /// Operações de `author` com `seq` maior que `after`, em ordem de `seq`.
    pub fn ops_after(&self, author: &MemberId, after: u32) -> Vec<&Op> {
        let mut v: Vec<&Op> = self.ops().filter(|o| o.author == *author && o.seq > after).collect();
        v.sort_by_key(|o| o.seq);
        v
    }

    /// Para cada autor, até onde as operações dele estão completas aqui (1..=n sem buraco). Em ordem de autor.
    pub fn vector(&self) -> Vec<(MemberId, u32)> {
        let mut seqs: BTreeMap<MemberId, Vec<u32>> = BTreeMap::new();
        for o in self.ops() {
            seqs.entry(o.author).or_default().push(o.seq);
        }
        seqs.into_iter()
            .map(|(a, mut s)| {
                s.sort_unstable();
                let mut have = 0;
                for x in s {
                    if x == have + 1 {
                        have = x;
                    }
                }
                (a, have)
            })
            .collect()
    }

    /// Resumo curto do que o registro tem: dois registros com o mesmo resumo têm as mesmas operações.
    pub fn fingerprint(&self) -> [u8; 8] {
        let mut h = Sha256::new();
        h.update(self.server);
        for o in self.ops() {
            h.update(o.encode());
        }
        h.finalize()[..8].try_into().expect("8 bytes")
    }

    /// `(lamport, seq)` da próxima operação de `author`.
    pub fn next_for(&self, author: &MemberId) -> (u32, u32) {
        let lamport = self.clock.saturating_add(1);
        let seq = self.ops().filter(|o| o.author == *author).map(|o| o.seq).max().unwrap_or(0).saturating_add(1);
        (lamport, seq)
    }

    /// Faz, assina e guarda uma operação de `author`, se ele puder fazê-la agora.
    pub fn make(&mut self, key: &SigningKey, author: MemberId, action: Action) -> Option<Op> {
        let (lamport, seq) = self.next_for(&author);
        let op = Op::sign(&self.server, key, author, lamport, seq, action);
        let mut probe = self.state.clone();
        if !apply(&mut probe, &self.owner_vk, &op, &mut |_| true) {
            return None;
        }
        (self.insert(op.clone()) == Insert::Added).then_some(op)
    }

    /// Dá para `author` fazer `action` agora?
    pub fn allows(&self, author: &MemberId, action: &Action) -> bool {
        let (lamport, seq) = self.next_for(author);
        let op = Op { author: *author, lamport, seq, action: action.clone(), sig: [0; 64] };
        apply(&mut self.state.clone(), &self.owner_vk, &op, &mut |_| true)
    }

    fn refold(&mut self) {
        let mut state = State::default();
        let mut vks = HashMap::new();
        let mut clock = 0u32;
        let (server, owner_vk) = (self.server, self.owner_vk);
        for e in self.ops.values_mut() {
            let op = &e.op;
            if op.lamport > clock.saturating_add(1) {
                continue;
            }
            clock = clock.max(op.lamport);
            let checked = &mut e.checked;
            let applied = apply(&mut state, &owner_vk, op, &mut |vk| match checked {
                Some((k, ok)) if k == vk => *ok,
                _ => {
                    let ok = op.verify(&server, vk);
                    *checked = Some((*vk, ok));
                    ok
                }
            });
            if applied && let Action::AddMember { key, vk } = &op.action {
                vks.insert(member_id(key), *vk);
            }
        }
        self.vks = vks;
        self.state = state;
        self.clock = clock;
    }
}

/// Aplica `op` a `state` se ela vale; devolve se valeu. `sig_ok` confere a assinatura com uma chave de verificação.
fn apply(state: &mut State, owner_vk: &[u8; 32], op: &Op, sig_ok: &mut dyn FnMut(&[u8; 32]) -> bool) -> bool {
    let genesis = state.members.is_empty();
    if genesis {
        // Só vale a entrada do dono, com a chave de verificação que o convite fixou.
        let Action::AddMember { key, vk } = &op.action else { return false };
        if member_id(key) != op.author || vk != owner_vk || !sig_ok(vk) {
            return false;
        }
        state.members.insert(op.author, Member { key: *key, vk: *vk, role: Role::Owner, name: String::new(), added_by: op.author });
        return true;
    }
    let Some(author) = state.members.get(&op.author) else { return false };
    let (role, avk) = (author.role, author.vk);
    if !sig_ok(&avk) {
        return false;
    }
    match &op.action {
        Action::AddMember { key, vk } => {
            let id = member_id(key);
            if state.members.contains_key(&id) || state.members.len() >= MAX_MEMBERS || VerifyingKey::from_bytes(vk).is_err() {
                return false;
            }
            state.members.insert(id, Member { key: *key, vk: *vk, role: Role::Member, name: String::new(), added_by: op.author });
        }
        Action::RemoveMember { target } => {
            let Some(t) = state.members.get(target) else { return false };
            let allowed = match t.role {
                Role::Owner => false,
                Role::Admin => role == Role::Owner || *target == op.author,
                Role::Member => role.manages() || *target == op.author,
            };
            if !allowed {
                return false;
            }
            state.members.remove(target);
        }
        Action::SetRole { target, role: new } => {
            let Some(t) = state.members.get_mut(target) else { return false };
            if role != Role::Owner || t.role == Role::Owner || *new == Role::Owner {
                return false;
            }
            t.role = *new;
        }
        Action::CreateRoom { room, name } => {
            if state.rooms.contains_key(room) || state.rooms.len() >= MAX_ROOMS {
                return false;
            }
            state.rooms.insert(*room, Room { name: name.clone(), creator: op.author });
        }
        Action::RenameRoom { room, name } => {
            let Some(r) = state.rooms.get_mut(room) else { return false };
            if !(role.manages() || r.creator == op.author) {
                return false;
            }
            r.name = name.clone();
        }
        Action::DeleteRoom { room } => {
            let Some(r) = state.rooms.get(room) else { return false };
            if !(role.manages() || r.creator == op.author) {
                return false;
            }
            state.rooms.remove(room);
        }
        Action::Rekey { epoch, commit } => {
            if !role.manages() || *epoch != state.epoch.wrapping_add(1) {
                return false;
            }
            state.epoch = *epoch;
            state.commit = *commit;
        }
        Action::SetName { name } => {
            if let Some(m) = state.members.get_mut(&op.author) {
                m.name = name.clone();
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Person {
        sk: SigningKey,
        key: [u8; 32],
        id: MemberId,
    }

    fn person(n: u8) -> Person {
        let sk = SigningKey::from_bytes(&[n; 32]);
        let key = [n; 32];
        Person { sk, key, id: member_id(&key) }
    }

    fn vk(p: &Person) -> [u8; 32] {
        p.sk.verifying_key().to_bytes()
    }

    const SID: ServerId = [42; 16];

    /// Registro do dono `a` já com `b` como membro.
    fn started(a: &Person, b: &Person) -> Registry {
        let mut r = Registry::new(SID, vk(a));
        r.make(&a.sk, a.id, Action::AddMember { key: a.key, vk: vk(a) }).unwrap();
        r.make(&a.sk, a.id, Action::AddMember { key: b.key, vk: vk(b) }).unwrap();
        r
    }

    #[test]
    fn the_owner_joins_first_and_invites_others() {
        let (a, b) = (person(1), person(2));
        let r = started(&a, &b);
        assert_eq!(r.state().members.len(), 2);
        assert_eq!(r.state().role_of(&a.id), Some(Role::Owner));
        assert_eq!(r.state().role_of(&b.id), Some(Role::Member));
        assert_eq!(r.state().members[&b.id].added_by, a.id);
    }

    #[test]
    fn only_the_pinned_owner_key_can_start_a_server() {
        let (a, b) = (person(1), person(2));
        let mut r = Registry::new(SID, vk(&a));
        assert!(r.make(&b.sk, b.id, Action::AddMember { key: b.key, vk: vk(&b) }).is_none(), "outro não funda");
        assert!(r.state().members.is_empty());
    }

    #[test]
    fn any_member_creates_rooms_and_invites_but_only_managers_remove() {
        let (a, b, c, d) = (person(1), person(2), person(3), person(4));
        let mut r = started(&a, &b);
        assert!(r.make(&b.sk, b.id, Action::AddMember { key: c.key, vk: vk(&c) }).is_some(), "membro convida");
        assert!(r.make(&b.sk, b.id, Action::CreateRoom { room: [1; 16], name: "geral".into() }).is_some());
        assert!(r.make(&c.sk, c.id, Action::CreateRoom { room: [2; 16], name: "jogos".into() }).is_some());
        assert!(r.make(&c.sk, c.id, Action::RemoveMember { target: b.id }).is_none(), "membro não remove outro");
        assert!(r.make(&c.sk, c.id, Action::DeleteRoom { room: [1; 16] }).is_none(), "nem apaga sala alheia");
        assert!(r.make(&b.sk, b.id, Action::DeleteRoom { room: [1; 16] }).is_some(), "criador apaga a sua");
        assert!(r.make(&d.sk, d.id, Action::CreateRoom { room: [3; 16], name: "x".into() }).is_none(), "estranho não faz nada");
        assert!(r.make(&a.sk, a.id, Action::RemoveMember { target: c.id }).is_some(), "dono remove");
        assert!(!r.state().members.contains_key(&c.id));
        assert!(r.state().rooms.contains_key(&[2; 16]), "a sala de quem saiu continua");
    }

    #[test]
    fn roles_follow_the_owner_and_the_owner_cannot_be_removed() {
        let (a, b, c) = (person(1), person(2), person(3));
        let mut r = started(&a, &b);
        r.make(&a.sk, a.id, Action::AddMember { key: c.key, vk: vk(&c) }).unwrap();
        assert!(r.make(&b.sk, b.id, Action::SetRole { target: c.id, role: Role::Admin }).is_none());
        assert!(r.make(&a.sk, a.id, Action::SetRole { target: b.id, role: Role::Admin }).is_some());
        assert!(r.make(&b.sk, b.id, Action::RemoveMember { target: c.id }).is_some(), "admin remove membro");
        assert!(r.make(&b.sk, b.id, Action::RemoveMember { target: a.id }).is_none(), "ninguém remove o dono");
        assert!(r.make(&a.sk, a.id, Action::RemoveMember { target: a.id }).is_none(), "nem ele mesmo");
        assert!(r.make(&a.sk, a.id, Action::SetRole { target: b.id, role: Role::Owner }).is_none());
        assert!(r.make(&b.sk, b.id, Action::RemoveMember { target: b.id }).is_some(), "admin pode sair");
    }

    #[test]
    fn rekey_only_goes_one_epoch_at_a_time_and_names_are_self_service() {
        let (a, b) = (person(1), person(2));
        let mut r = started(&a, &b);
        assert!(r.make(&b.sk, b.id, Action::Rekey { epoch: 1, commit: [1; 32] }).is_none());
        assert!(r.make(&a.sk, a.id, Action::Rekey { epoch: 2, commit: [2; 32] }).is_none());
        assert!(r.make(&a.sk, a.id, Action::Rekey { epoch: 1, commit: [1; 32] }).is_some());
        assert_eq!(r.state().epoch, 1);
        r.make(&b.sk, b.id, Action::SetName { name: "Bia".into() }).unwrap();
        assert_eq!(r.state().members[&b.id].name, "Bia");
    }

    #[test]
    fn full_servers_and_rooms_refuse_more() {
        let a = person(1);
        let mut r = Registry::new(SID, vk(&a));
        r.make(&a.sk, a.id, Action::AddMember { key: a.key, vk: vk(&a) }).unwrap();
        for n in 0..MAX_ROOMS {
            let mut room = [0u8; 16];
            room[0] = n as u8;
            assert!(r.make(&a.sk, a.id, Action::CreateRoom { room, name: format!("s{n}") }).is_some());
        }
        assert!(r.make(&a.sk, a.id, Action::CreateRoom { room: [200; 16], name: "demais".into() }).is_none());
        for n in 2..=MAX_MEMBERS as u8 {
            let p = person(n);
            assert!(r.make(&a.sk, a.id, Action::AddMember { key: p.key, vk: vk(&p) }).is_some());
        }
        let extra = person(100);
        assert!(r.make(&a.sk, a.id, Action::AddMember { key: extra.key, vk: vk(&extra) }).is_none());
    }

    #[test]
    fn every_replica_converges_whatever_the_arrival_order() {
        let (a, b, c) = (person(1), person(2), person(3));
        let mut src = started(&a, &b);
        src.make(&b.sk, b.id, Action::AddMember { key: c.key, vk: vk(&c) }).unwrap();
        src.make(&c.sk, c.id, Action::CreateRoom { room: [1; 16], name: "geral".into() }).unwrap();
        src.make(&a.sk, a.id, Action::RemoveMember { target: b.id }).unwrap();
        src.make(&c.sk, c.id, Action::SetName { name: "Cadu".into() }).unwrap();
        let ops: Vec<Op> = src.ops().cloned().collect();
        for shift in 0..ops.len() {
            let mut r = Registry::new(SID, vk(&a));
            let mut order = ops.clone();
            order.rotate_left(shift);
            order.reverse();
            for op in order {
                r.insert(op);
            }
            assert_eq!(r.state(), src.state(), "ordem {shift}");
            assert_eq!(r.fingerprint(), src.fingerprint());
        }
    }

    #[test]
    fn forged_and_misattributed_operations_have_no_effect() {
        let (a, b, e) = (person(1), person(2), person(5));
        let mut r = started(&a, &b);
        let before = r.state().clone();
        // Assinada por uma chave que não é a do autor declarado.
        let forged = Op::sign(&SID, &e.sk, a.id, 3, 99, Action::RemoveMember { target: b.id });
        assert_eq!(r.insert(forged), Insert::Refused, "autor conhecido: a assinatura é conferida na entrada");
        assert_eq!(r.state(), &before);
        // Assinatura de outro servidor.
        let other = Op::sign(&[7; 16], &a.sk, a.id, 3, 100, Action::RemoveMember { target: b.id });
        r.insert(other);
        assert_eq!(r.state(), &before);
        // O estranho não vira membro por ter feito a operação antes de ser convidado.
        let early = Op::sign(&SID, &e.sk, e.id, 1, 1, Action::CreateRoom { room: [9; 16], name: "x".into() });
        assert_eq!(r.insert(early), Insert::Added, "guarda: pode ser de quem o convite ainda não chegou");
        assert!(r.state().rooms.is_empty());
    }

    #[test]
    fn operations_of_a_removed_member_stop_counting() {
        let (a, b) = (person(1), person(2));
        let mut r = started(&a, &b);
        r.make(&a.sk, a.id, Action::RemoveMember { target: b.id }).unwrap();
        let late = Op::sign(&SID, &b.sk, b.id, 4, 1, Action::CreateRoom { room: [1; 16], name: "x".into() });
        r.insert(late);
        assert!(r.state().rooms.is_empty());
        r.make(&a.sk, a.id, Action::AddMember { key: b.key, vk: vk(&b) }).unwrap();
        assert!(r.state().members.contains_key(&b.id), "convidado de novo");
        assert!(r.state().rooms.is_empty(), "o que fez removido não ressuscita");
    }

    #[test]
    fn a_member_cannot_jump_the_clock_to_freeze_the_server() {
        let (a, b) = (person(1), person(2));
        let mut r = started(&a, &b);
        let jump = Op::sign(&SID, &b.sk, b.id, u32::MAX, 1, Action::CreateRoom { room: [1; 16], name: "x".into() });
        assert_eq!(r.insert(jump.clone()), Insert::Added, "guarda, mas não conta");
        assert!(r.state().rooms.is_empty());
        assert_eq!(r.next_for(&a.id), (3, 3), "o relógio não andou");
        assert!(r.make(&a.sk, a.id, Action::CreateRoom { room: [2; 16], name: "y".into() }).is_some());
        // Um salto de mais de um também é ignorado, e vale quando a que faltava chega.
        let skip = Op::sign(&SID, &b.sk, b.id, 5, 3, Action::CreateRoom { room: [3; 16], name: "z".into() });
        r.insert(skip);
        assert!(!r.state().rooms.contains_key(&[3; 16]));
        let fill = Op::sign(&SID, &b.sk, b.id, 4, 2, Action::SetName { name: "Bia".into() });
        r.insert(fill);
        assert!(r.state().rooms.contains_key(&[3; 16]), "com o buraco preenchido a operação conta");
        // Todo mundo chega ao mesmo estado.
        let mut other = Registry::new(SID, vk(&a));
        let mut all: Vec<Op> = r.ops().cloned().collect();
        all.reverse();
        all.push(jump);
        for op in all {
            other.insert(op);
        }
        assert_eq!(other.state(), r.state());
    }

    #[test]
    fn vector_and_fingerprint_describe_what_is_held() {
        let (a, b) = (person(1), person(2));
        let r = started(&a, &b);
        assert_eq!(r.vector(), vec![(a.id, 2)]);
        let mut gap = Registry::new(SID, vk(&a));
        let ops: Vec<Op> = r.ops().cloned().collect();
        gap.insert(ops[1].clone());
        assert_eq!(gap.vector(), vec![(a.id, 0)], "buraco: a segunda sem a primeira não conta");
        assert_ne!(gap.fingerprint(), r.fingerprint());
        assert_eq!(r.ops_after(&a.id, 1).len(), 1);
        gap.insert(ops[0].clone());
        assert_eq!(gap.fingerprint(), r.fingerprint());
    }

    #[test]
    fn two_different_operations_with_the_same_author_and_seq_resolve_the_same_everywhere() {
        let (a, b) = (person(1), person(2));
        let base = started(&a, &b);
        let x = Op::sign(&SID, &b.sk, b.id, 3, 1, Action::CreateRoom { room: [1; 16], name: "um".into() });
        let y = Op::sign(&SID, &b.sk, b.id, 3, 1, Action::CreateRoom { room: [2; 16], name: "dois".into() });
        let build = |first: &Op, second: &Op| {
            let mut r = Registry::new(SID, vk(&a));
            for op in base.ops() {
                r.insert(op.clone());
            }
            r.insert(first.clone());
            r.insert(second.clone());
            r
        };
        let (r1, r2) = (build(&x, &y), build(&y, &x));
        assert_eq!(r1.state(), r2.state());
        assert_eq!(r1.fingerprint(), r2.fingerprint());
        assert_eq!(r1.state().rooms.len(), 1);
    }
    /// `n` operações de `p`, cada uma trocando o nome dele; devolve quantas o registro aceitou.
    fn rename_many(r: &mut Registry, p: &Person, n: usize) -> usize {
        (0..n).filter(|i| r.make(&p.sk, p.id, Action::SetName { name: format!("n{i}") }).is_some()).count()
    }

    #[test]
    fn one_member_cannot_fill_the_registry() {
        let (a, b) = (person(1), person(2));
        let mut r = started(&a, &b);
        let mine = r.ops().filter(|o| o.author == b.id).count();
        assert_eq!(rename_many(&mut r, &b, MAX_OPS_PER_AUTHOR + 50), MAX_OPS_PER_AUTHOR - mine);
        assert!(r.make(&b.sk, b.id, Action::SetName { name: "mais".into() }).is_none(), "passou da cota");
        assert!(r.make(&a.sk, a.id, Action::CreateRoom { room: [1; 16], name: "geral".into() }).is_some(), "os outros seguem");
    }

    #[test]
    fn members_in_collusion_cannot_take_the_owners_reserve() {
        let a = person(1);
        let mut r = Registry::new(SID, vk(&a));
        r.make(&a.sk, a.id, Action::AddMember { key: a.key, vk: vk(&a) }).unwrap();
        let crowd: Vec<Person> = (2..=10).map(person).collect();
        for p in &crowd {
            r.make(&a.sk, a.id, Action::AddMember { key: p.key, vk: vk(p) }).unwrap();
        }
        for p in &crowd {
            rename_many(&mut r, p, MAX_OPS_PER_AUTHOR);
        }
        assert_eq!(r.len(), MAX_OPS - OWNER_RESERVE, "o conluio para onde a reserva começa");
        assert!(r.make(&crowd[0].sk, crowd[0].id, Action::SetName { name: "x".into() }).is_none());
        let target = crowd[0].id;
        assert!(r.make(&a.sk, a.id, Action::RemoveMember { target }).is_some(), "o dono ainda remove");
        assert!(r.make(&a.sk, a.id, Action::Rekey { epoch: 1, commit: [1; 32] }).is_some(), "e troca a chave");
    }

    #[test]
    fn a_forged_operation_cannot_spend_anothers_quota_or_replace_a_real_one() {
        let (a, b, e) = (person(1), person(2), person(5));
        let mut r = started(&a, &b);
        let owner_ops = r.ops().filter(|o| o.author == a.id).count();
        // Com o nome do dono, assinadas por quem não é ele: nenhuma entra, nem para ocupar `seq` nem para trocar a verdadeira.
        for seq in 1..=(MAX_OPS_PER_AUTHOR as u32 + 10) {
            let forged = Op::sign(&SID, &e.sk, a.id, 2, seq, Action::SetName { name: "x".into() });
            assert_eq!(r.insert(forged), Insert::Refused);
        }
        assert_eq!(r.ops().filter(|o| o.author == a.id).count(), owner_ops);
        assert!(r.make(&a.sk, a.id, Action::CreateRoom { room: [1; 16], name: "geral".into() }).is_some());
    }

    #[test]
    fn strangers_get_a_small_shared_pool() {
        let (a, b) = (person(1), person(2));
        let mut r = started(&a, &b);
        for n in 0..MAX_STRANGER_OPS as u8 {
            let junk = person(100 + n);
            let op = Op::sign(&SID, &junk.sk, junk.id, 1, 1, Action::SetName { name: "x".into() });
            assert_eq!(r.insert(op), Insert::Added, "{n}");
        }
        let more = person(200);
        let op = Op::sign(&SID, &more.sk, more.id, 1, 1, Action::SetName { name: "x".into() });
        assert_eq!(r.insert(op), Insert::Refused, "o poço dos estranhos encheu");
        assert!(r.make(&b.sk, b.id, Action::SetName { name: "Bia".into() }).is_some(), "membro não depende dele");
    }
}
