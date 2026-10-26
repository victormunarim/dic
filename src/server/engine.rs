//! O motor dos servidores: junta registro, convites e presença sem tocar a rede (*sans-IO*).
//!
//! Quem usa chama os métodos com a hora (`now`, em segundos, sempre crescente) e recebe um [`Output`]:
//! quadros a entregar ([`Outbound`]) e coisas a mostrar ou fazer ([`Event`]). Os quadros recebidos entram
//! por [`Servers::handle`]. O [`Node`](crate::node::Node) entrega os quadros e chama [`Servers::tick`].
//!
//! Tempo da presença: cada membro online manda `Hello` a cada [`HEARTBEAT`] segundos; quem ouve vale por
//! [`LEASE`] segundos. Quem está offline é sondado a cada [`PROBE`] segundos.

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use rand_core::{OsRng, RngCore};
use x25519_dalek::{PublicKey, StaticSecret};

use super::{
    MAX_ROOM_PEOPLE, MemberId, RoomId, ServerError, ServerId, clean_name, member_id,
    invite::{Hint, Invite, MAX_HINTS, hex, unhex},
    op::{Action, Op, Role},
    registry::{Insert, Registry},
    signing_key, valid_name,
    wire::{self, Hello, Msg, VECTOR_PER_FRAME},
};
use crate::media::{signal::Frame, update::Reach};

/// De quanto em quanto tempo um membro online renova a presença.
pub const HEARTBEAT: u64 = 15;
/// Quanto tempo a presença vale sem ser renovada.
pub const LEASE: u64 = 40;
/// De quanto em quanto tempo se sonda quem parece offline.
pub const PROBE: u64 = 60;
/// Intervalo mínimo entre dois vetores ao mesmo membro.
const VECTOR_GAP: u64 = 5;
/// Intervalo mínimo entre dois presentes (segredo novo) ao mesmo membro.
const GIFT_GAP: u64 = 10;
/// Operações mandadas por rodada de sincronização a um membro.
const OPS_PER_ROUND: usize = 64;

/// Um quadro a entregar a `to`; `via` é o último relay que ele anunciou, para quem ainda não o conhece.
#[derive(Clone, Debug)]
pub struct Outbound {
    pub server: ServerId,
    pub to: [u8; 32],
    pub via: Option<Reach>,
    pub frame: Frame,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Algo do servidor mudou (membros, salas, presença): redesenhe.
    Changed(ServerId),
    /// Entrei no servidor (o convite foi aceito e as operações chegaram).
    Joined(ServerId),
    /// Fui removido do servidor, que deixou de existir aqui.
    Removed(ServerId),
    /// Um membro anunciou o relay dele: o nó pode passar a usá-lo.
    Address { key: [u8; 32], reach: Reach },
    /// Quem está na mesma sala que eu mudou (vazio se não estou em sala): é a malha da chamada.
    Mesh { server: ServerId, room: Option<RoomId>, peers: Vec<[u8; 32]> },
    Notice(String),
}

#[derive(Default, Debug)]
pub struct Output {
    pub send: Vec<Outbound>,
    pub events: Vec<Event>,
    /// Confirmação do quadro que acabou de chegar, para voltar pela mesma conexão.
    pub ack: Option<Frame>,
}

impl Output {
    fn merge(&mut self, other: Output) {
        self.send.extend(other.send);
        self.events.extend(other.events);
    }

    pub fn is_empty(&self) -> bool {
        self.send.is_empty() && self.events.is_empty()
    }
}

struct Me {
    secret: StaticSecret,
    key: [u8; 32],
    id: MemberId,
    sk: SigningKey,
}

struct Peer {
    key: [u8; 32],
    /// A presença vale até aqui; 0 se nunca ouvi.
    until: u64,
    room: Option<RoomId>,
    reach: Option<(u64, Reach)>,
    last_hello: Option<u64>,
    last_vector: Option<u64>,
    last_gift: Option<u64>,
    pages: BTreeMap<u8, Vec<(MemberId, u16)>>,
    pages_total: u8,
}

impl Peer {
    fn new(key: [u8; 32]) -> Self {
        Peer { key, until: 0, room: None, reach: None, last_hello: None, last_vector: None, last_gift: None, pages: BTreeMap::new(), pages_total: 0 }
    }

    fn online(&self, now: u64) -> bool {
        self.until > now
    }
}

struct Server {
    id: ServerId,
    name: String,
    epoch: u16,
    secret: [u8; 32],
    reg: Registry,
    peers: BTreeMap<MemberId, Peer>,
    /// Quem eu convidei e ainda não entrou.
    invited: BTreeMap<MemberId, [u8; 32]>,
    /// Convite aceito, esperando entrar: a quem mandar o `Join`.
    joining: Option<Hint>,
    last_join: Option<u64>,
    /// Já fui membro (para saber quando sou removido).
    member: bool,
    room: Option<RoomId>,
    mesh: (Option<RoomId>, Vec<[u8; 32]>),
}

impl Server {
    fn peer(&mut self, key: [u8; 32]) -> &mut Peer {
        self.peers.entry(member_id(&key)).or_insert_with(|| Peer::new(key))
    }

    /// Garante um `Peer` para cada membro (menos eu) e tira os que sumiram do registro e não são dicas nem convidados.
    fn sync_peers(&mut self, me: &Me) {
        let members: Vec<[u8; 32]> = self.reg.state().members.values().map(|m| m.key).filter(|k| *k != me.key).collect();
        for k in members {
            self.peer(k);
        }
        let (state, invited, joining) = (self.reg.state(), &self.invited, &self.joining);
        let keep = |id: &MemberId, p: &Peer| state.members.contains_key(id) || invited.contains_key(id) || joining.as_ref().is_some_and(|h| h.key == p.key) || p.reach.is_some() && joining.is_some();
        self.peers.retain(|id, p| keep(id, p));
    }

    fn hello(&self) -> Hello {
        Hello { room: self.room, fp: self.reg.fingerprint(), count: self.reg.len().min(u16::MAX as usize) as u16, epoch: self.epoch }
    }

    fn is_member(&self, id: &MemberId) -> bool {
        self.reg.state().members.contains_key(id)
    }

    fn people_in(&self, room: &RoomId, now: u64) -> usize {
        self.peers.iter().filter(|(id, p)| self.is_member(id) && p.online(now) && p.room.as_ref() == Some(room)).count() + usize::from(self.room.as_ref() == Some(room))
    }

    fn mesh_now(&self, now: u64) -> (Option<RoomId>, Vec<[u8; 32]>) {
        let Some(room) = self.room else { return (None, Vec::new()) };
        let mut peers: Vec<[u8; 32]> = self.peers.iter().filter(|(id, p)| self.is_member(id) && p.online(now) && p.room == Some(room)).map(|(_, p)| p.key).collect();
        peers.sort();
        (Some(room), peers)
    }
}

pub struct Servers {
    me: Me,
    visible: bool,
    name: String,
    reach: Option<(u64, Reach)>,
    servers: BTreeMap<ServerId, Server>,
}

fn seal_to(me: &Me, s: &Server, key: &[u8; 32], msg: &Msg) -> Option<Outbound> {
    let frame = wire::seal(&me.secret, key, &s.id, msg)?;
    Some(Outbound { server: s.id, to: *key, via: s.peers.get(&member_id(key)).and_then(|p| p.reach.as_ref().map(|r| r.1.clone())), frame })
}

fn vector_msgs(reg: &Registry) -> Vec<Msg> {
    let v: Vec<(MemberId, u16)> = reg.vector().into_iter().map(|(a, n)| (a, n.min(u16::MAX as u32) as u16)).collect();
    let pages = v.len().div_ceil(VECTOR_PER_FRAME).max(1);
    (0..pages).map(|i| Msg::Vector { page: i as u8, pages: pages as u8, entries: v.iter().skip(i * VECTOR_PER_FRAME).take(VECTOR_PER_FRAME).copied().collect() }).collect()
}

impl Servers {
    pub fn new(secret: StaticSecret) -> Self {
        let key = *PublicKey::from(&secret).as_bytes();
        let sk = signing_key(&secret);
        Servers { me: Me { id: member_id(&key), key, secret, sk }, visible: true, name: String::new(), reach: None, servers: BTreeMap::new() }
    }

    pub fn my_key(&self) -> [u8; 32] {
        self.me.key
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// `reply` é a confirmação de `sent` que o destinatário mandou.
    pub fn is_ack(&self, sent: &Outbound, reply: &[u8]) -> bool {
        matches!(wire::open(&self.me.secret, &sent.to, &sent.server, reply), Some(Msg::Ack(t)) if Some(t) == wire::nonce_tag(&sent.frame))
    }

    /// Todos os servidores com a época e o segredo atuais (para o reencontro).
    pub fn secrets(&self) -> Vec<(ServerId, u16, [u8; 32])> {
        self.servers.values().filter(|s| s.member).map(|s| (s.id, s.epoch, s.secret)).collect()
    }

    /// Chaves de todos os membros de todos os servidores (menos eu).
    pub fn all_members(&self) -> Vec<[u8; 32]> {
        let mut v: Vec<[u8; 32]> = self.servers.values().flat_map(|s| s.reg.state().members.values().map(|m| m.key)).filter(|k| *k != self.me.key).collect();
        v.sort();
        v.dedup();
        v
    }

    // ---- criar, convidar, entrar -------------------------------------------------------------------------

    pub fn create(&mut self, name: &str, now: u64) -> Result<(ServerId, Output), ServerError> {
        let name = clean_name(name);
        if !valid_name(&name) {
            return Err(ServerError::BadName);
        }
        let mut id = [0u8; 16];
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut id);
        OsRng.fill_bytes(&mut secret);
        let owner_vk = self.me.sk.verifying_key().to_bytes();
        let mut s = Server {
            id,
            name,
            epoch: 0,
            secret,
            reg: Registry::new(id, owner_vk),
            peers: BTreeMap::new(),
            invited: BTreeMap::new(),
            joining: None,
            last_join: None,
            member: false,
            room: None,
            mesh: (None, Vec::new()),
        };
        s.reg.make(&self.me.sk, self.me.id, Action::AddMember { key: self.me.key, vk: owner_vk }).ok_or(ServerError::Forbidden)?;
        s.member = true;
        self.servers.insert(id, s);
        let mut out = Output::default();
        out.events.push(Event::Changed(id));
        out.merge(self.announce_name(&id, now));
        Ok((id, self.finish(now, out)))
    }

    /// Texto de convite para `invitee`, que precisa ser levado a ele por fora (ou pelo contato).
    pub fn invite(&mut self, server: &ServerId, invitee: [u8; 32], now: u64) -> Result<String, ServerError> {
        let reach = self.reach.as_ref().map(|r| r.1.clone()).ok_or(ServerError::NoAddress)?;
        let s = self.servers.get_mut(server).ok_or(ServerError::UnknownServer)?;
        if !s.is_member(&self.me.id) {
            return Err(ServerError::NotMember);
        }
        let id = member_id(&invitee);
        if s.is_member(&id) {
            return Err(ServerError::AlreadyIn);
        }
        if s.reg.state().members.len() + s.invited.len() >= super::MAX_MEMBERS {
            return Err(ServerError::Full);
        }
        let hints = s
            .peers
            .values()
            .filter(|p| s.reg.state().members.contains_key(&member_id(&p.key)) && p.online(now) && p.key != invitee)
            .filter_map(|p| p.reach.as_ref().map(|r| Hint { key: p.key, reach: r.1.clone() }))
            .take(MAX_HINTS)
            .collect();
        let invite = Invite {
            server: s.id,
            name: s.name.clone(),
            epoch: s.epoch,
            secret: s.secret,
            owner_vk: *s.reg.owner_vk(),
            invitee,
            inviter: Hint { key: self.me.key, reach },
            hints,
        };
        let text = invite.seal(&self.me.secret).ok_or(ServerError::BadInvite)?;
        s.invited.insert(id, invitee);
        s.peer(invitee);
        Ok(text)
    }

    /// Aceita um convite: o servidor aparece na lista e o `Join` segue para quem convidou.
    pub fn accept(&mut self, text: &str, now: u64) -> Result<(ServerId, Output), ServerError> {
        let reach = self.reach.as_ref().map(|r| r.1.clone()).ok_or(ServerError::NoAddress)?;
        let inv = Invite::open(text, &self.me.secret)?;
        if self.servers.contains_key(&inv.server) {
            return Err(ServerError::AlreadyIn);
        }
        let mut s = Server {
            id: inv.server,
            name: inv.name,
            epoch: inv.epoch,
            secret: inv.secret,
            reg: Registry::new(inv.server, inv.owner_vk),
            peers: BTreeMap::new(),
            invited: BTreeMap::new(),
            joining: Some(inv.inviter.clone()),
            last_join: Some(now),
            member: false,
            room: None,
            mesh: (None, Vec::new()),
        };
        let mut out = Output::default();
        for h in std::iter::once(&inv.inviter).chain(inv.hints.iter()) {
            s.peer(h.key).reach = Some((0, h.reach.clone()));
            out.events.push(Event::Address { key: h.key, reach: h.reach.clone() });
        }
        out.send.extend(seal_to(&self.me, &s, &inv.inviter.key, &Msg::Join { vk: self.me.sk.verifying_key().to_bytes(), reach }));
        let id = s.id;
        self.servers.insert(id, s);
        out.events.push(Event::Changed(id));
        Ok((id, self.finish(now, out)))
    }

    /// Esquece o servidor aqui. Sem trocar o segredo, quem sai continua com o que já viu.
    pub fn leave(&mut self, server: &ServerId, now: u64) -> Result<Output, ServerError> {
        let s = self.servers.remove(server).ok_or(ServerError::UnknownServer)?;
        let mut out = Output::default();
        if s.mesh.0.is_some() {
            out.events.push(Event::Mesh { server: s.id, room: None, peers: Vec::new() });
        }
        out.events.push(Event::Removed(s.id));
        let _ = now;
        Ok(out)
    }

    // ---- ações sobre o registro --------------------------------------------------------------------------

    fn act(&mut self, server: &ServerId, action: Action, now: u64) -> Result<(Option<Op>, Output), ServerError> {
        let s = self.servers.get_mut(server).ok_or(ServerError::UnknownServer)?;
        if !s.is_member(&self.me.id) {
            return Err(ServerError::NotMember);
        }
        let op = s.reg.make(&self.me.sk, self.me.id, action).ok_or(ServerError::Forbidden)?;
        s.sync_peers(&self.me);
        let mut out = Output::default();
        out.events.push(Event::Changed(*server));
        let s = &self.servers[server];
        for p in s.peers.values().filter(|p| p.online(now) && s.is_member(&member_id(&p.key))) {
            out.send.extend(seal_to(&self.me, s, &p.key, &Msg::Op(op.clone())));
        }
        Ok((Some(op), out))
    }

    pub fn create_room(&mut self, server: &ServerId, name: &str, now: u64) -> Result<(RoomId, Output), ServerError> {
        let name = clean_name(name);
        if !valid_name(&name) {
            return Err(ServerError::BadName);
        }
        let mut room = [0u8; 16];
        OsRng.fill_bytes(&mut room);
        let (_, out) = self.act(server, Action::CreateRoom { room, name }, now)?;
        Ok((room, self.finish(now, out)))
    }

    pub fn rename_room(&mut self, server: &ServerId, room: RoomId, name: &str, now: u64) -> Result<Output, ServerError> {
        let name = clean_name(name);
        if !valid_name(&name) {
            return Err(ServerError::BadName);
        }
        let (_, out) = self.act(server, Action::RenameRoom { room, name }, now)?;
        Ok(self.finish(now, out))
    }

    pub fn delete_room(&mut self, server: &ServerId, room: RoomId, now: u64) -> Result<Output, ServerError> {
        let (_, out) = self.act(server, Action::DeleteRoom { room }, now)?;
        Ok(self.finish(now, out))
    }

    pub fn set_role(&mut self, server: &ServerId, target: MemberId, role: Role, now: u64) -> Result<Output, ServerError> {
        let (_, out) = self.act(server, Action::SetRole { target, role }, now)?;
        Ok(self.finish(now, out))
    }

    /// Remove `target` e troca o segredo do servidor para os que ficam.
    pub fn remove_member(&mut self, server: &ServerId, target: MemberId, now: u64) -> Result<Output, ServerError> {
        let (_, mut out) = self.act(server, Action::RemoveMember { target }, now)?;
        let epoch = self.servers[server].reg.state().epoch + 1;
        if let Ok((_, more)) = self.act(server, Action::Rekey { epoch }, now) {
            out.merge(more);
            let s = self.servers.get_mut(server).expect("existe");
            s.epoch = epoch;
            OsRng.fill_bytes(&mut s.secret);
            let s = &self.servers[server];
            for p in s.peers.values().filter(|p| p.online(now) && s.is_member(&member_id(&p.key))) {
                out.send.extend(seal_to(&self.me, s, &p.key, &Msg::Gift { epoch, secret: s.secret }));
            }
        }
        Ok(self.finish(now, out))
    }

    /// Como quero ser chamado, em todos os servidores.
    pub fn set_name(&mut self, name: &str, now: u64) -> Output {
        self.name = clean_name(name);
        let ids: Vec<ServerId> = self.servers.keys().copied().collect();
        let mut out = Output::default();
        for id in ids {
            out.merge(self.announce_name(&id, now));
        }
        self.finish(now, out)
    }

    fn announce_name(&mut self, server: &ServerId, now: u64) -> Output {
        if !valid_name(&self.name) {
            return Output::default();
        }
        let current = self.servers.get(server).and_then(|s| s.reg.state().members.get(&self.me.id)).map(|m| m.name.clone());
        if current.is_none() || current.as_deref() == Some(self.name.as_str()) {
            return Output::default();
        }
        self.act(server, Action::SetName { name: self.name.clone() }, now).map(|(_, o)| o).unwrap_or_default()
    }

    // ---- presença ---------------------------------------------------------------------------------------

    pub fn set_visible(&mut self, visible: bool, now: u64) -> Output {
        if self.visible == visible {
            return Output::default();
        }
        self.visible = visible;
        let mut out = Output::default();
        if visible {
            out.merge(self.hello_all(now, true));
        }
        for s in self.servers.values() {
            out.events.push(Event::Changed(s.id));
        }
        self.finish(now, out)
    }

    /// Entra em `room` (ou sai de qualquer sala com `None`).
    pub fn join_room(&mut self, server: &ServerId, room: Option<RoomId>, now: u64) -> Result<Output, ServerError> {
        let s = self.servers.get_mut(server).ok_or(ServerError::UnknownServer)?;
        if !s.is_member(&self.me.id) {
            return Err(ServerError::NotMember);
        }
        if let Some(r) = room {
            if !s.reg.state().rooms.contains_key(&r) {
                return Err(ServerError::UnknownRoom);
            }
            if s.room != Some(r) && s.people_in(&r, now) >= MAX_ROOM_PEOPLE {
                return Err(ServerError::Full);
            }
        }
        // Uma sala por vez, em qualquer servidor.
        for other in self.servers.values_mut().filter(|o| o.id != *server) {
            other.room = None;
        }
        let s = self.servers.get_mut(server).expect("existe");
        s.room = room;
        let mut out = Output::default();
        out.events.push(Event::Changed(*server));
        out.merge(self.hello_all(now, true));
        Ok(self.finish(now, out))
    }

    /// `Hello` a quem está online (`only_online`) ou a todos os membros.
    fn hello_all(&mut self, now: u64, only_online: bool) -> Output {
        let mut out = Output::default();
        if !self.visible {
            return out;
        }
        for s in self.servers.values_mut().filter(|s| s.member) {
            let hello = s.hello();
            let keys: Vec<[u8; 32]> = s.peers.values().filter(|p| s.reg.state().members.contains_key(&member_id(&p.key)) && (!only_online || p.online(now))).map(|p| p.key).collect();
            for k in keys {
                out.send.extend(seal_to(&self.me, s, &k, &Msg::Hello(hello)));
                s.peer(k).last_hello = Some(now);
            }
        }
        out
    }

    /// Fiquei online (ou abri o servidor): avisa todos os membros.
    pub fn online(&mut self, now: u64) -> Output {
        let out = self.hello_all(now, false);
        self.finish(now, out)
    }

    /// A janela abriu o servidor: renova a presença com todos.
    pub fn open(&mut self, server: &ServerId, now: u64) -> Output {
        let mut out = Output::default();
        if !self.visible {
            return out;
        }
        if let Some(s) = self.servers.get_mut(server).filter(|s| s.member) {
            let hello = s.hello();
            let keys: Vec<[u8; 32]> = s.peers.values().filter(|p| s.reg.state().members.contains_key(&member_id(&p.key))).map(|p| p.key).collect();
            for k in keys {
                out.send.extend(seal_to(&self.me, s, &k, &Msg::Hello(hello)));
                s.peer(k).last_hello = Some(now);
            }
        }
        self.finish(now, out)
    }

    /// O meu relay mudou (ou ficou pronto): conta aos membros online.
    pub fn set_reach(&mut self, reach: Option<Reach>, seq: u64, now: u64) -> Output {
        let mut out = Output::default();
        let Some(reach) = reach else {
            self.reach = None;
            return out;
        };
        self.reach = Some((seq, reach.clone()));
        for s in self.servers.values().filter(|s| s.member) {
            for p in s.peers.values().filter(|p| p.online(now) && s.is_member(&member_id(&p.key))) {
                out.send.extend(seal_to(&self.me, s, &p.key, &Msg::MyAddr { seq, reach: reach.clone() }));
            }
        }
        // Um convite aceito antes de ter endereço sai agora.
        let vk = self.me.sk.verifying_key().to_bytes();
        for s in self.servers.values_mut() {
            if let Some(h) = s.joining.clone().filter(|_| s.last_join.is_none()) {
                out.send.extend(seal_to(&self.me, s, &h.key, &Msg::Join { vk, reach: reach.clone() }));
                s.last_join = Some(now);
            }
        }
        self.finish(now, out)
    }

    /// Relógio: renova a presença, sonda quem está offline, repete o `Join` e solta o que expirou.
    pub fn tick(&mut self, now: u64) -> Output {
        let mut out = Output::default();
        let vk = self.me.sk.verifying_key().to_bytes();
        let reach = self.reach.as_ref().map(|r| r.1.clone());
        let visible = self.visible;
        for s in self.servers.values_mut() {
            if let Some(h) = s.joining.clone() {
                if let Some(reach) = reach.clone().filter(|_| s.last_join.is_none_or(|t| now >= t + PROBE)) {
                    out.send.extend(seal_to(&self.me, s, &h.key, &Msg::Join { vk, reach }));
                    s.last_join = Some(now);
                }
                continue;
            }
            if !visible || !s.member {
                continue;
            }
            let hello = s.hello();
            let due: Vec<[u8; 32]> = s
                .peers
                .values()
                .filter(|p| s.reg.state().members.contains_key(&member_id(&p.key)))
                .filter(|p| p.last_hello.is_none_or(|t| now >= t + if p.online(now) { HEARTBEAT } else { PROBE }))
                .map(|p| p.key)
                .collect();
            for k in due {
                out.send.extend(seal_to(&self.me, s, &k, &Msg::Hello(hello)));
                s.peer(k).last_hello = Some(now);
            }
        }
        self.finish(now, out)
    }

    // ---- quadros recebidos ------------------------------------------------------------------------------

    /// Trata um quadro de servidor. Lixo, adulterado ou de quem não é daqui some sem resposta.
    pub fn handle(&mut self, frame: &[u8], now: u64) -> Output {
        let mut out = Output::default();
        let Some((short, sender)) = wire::head(frame) else { return out };
        let ids: Vec<ServerId> = self.servers.keys().filter(|id| id[..4] == short).copied().collect();
        for sid in ids {
            let Some(key) = self.servers[&sid].peers.get(&sender).map(|p| p.key) else { continue };
            let Some(msg) = wire::open(&self.me.secret, &key, &sid, frame) else { continue };
            if !matches!(msg, Msg::Ack(_)) {
                out.ack = wire::nonce_tag(frame).and_then(|t| wire::seal(&self.me.secret, &key, &sid, &Msg::Ack(t)));
            }
            out.merge(self.on_msg(sid, key, msg, now));
            break;
        }
        self.finish(now, out)
    }

    fn on_msg(&mut self, sid: ServerId, key: [u8; 32], msg: Msg, now: u64) -> Output {
        let mut out = Output::default();
        let id = member_id(&key);
        let me = &self.me;
        let visible = self.visible;
        let my_reach = self.reach.clone();
        let s = self.servers.get_mut(&sid).expect("existe");
        let is_member = s.is_member(&id);
        match msg {
            Msg::Hello(h) | Msg::HelloAck(h) if !is_member => {
                let _ = h;
            }
            msg @ (Msg::Hello(_) | Msg::HelloAck(_)) => {
                let (h, reply) = match msg {
                    Msg::Hello(h) => (h, true),
                    Msg::HelloAck(h) => (h, false),
                    _ => unreachable!(),
                };
                if !visible {
                    return out;
                }
                let p = s.peer(key);
                let changed = !p.online(now) || p.room != h.room;
                p.until = now + LEASE;
                p.room = h.room;
                if changed {
                    out.events.push(Event::Changed(sid));
                }
                if reply {
                    out.send.extend(seal_to(me, s, &key, &Msg::HelloAck(s.hello())));
                }
                if h.fp != s.reg.fingerprint() && s.peer(key).last_vector.is_none_or(|t| now >= t + VECTOR_GAP) {
                    s.peer(key).last_vector = Some(now);
                    for m in vector_msgs(&s.reg) {
                        out.send.extend(seal_to(me, s, &key, &m));
                    }
                }
                if h.epoch < s.epoch && s.epoch == s.reg.state().epoch && s.peer(key).last_gift.is_none_or(|t| now >= t + GIFT_GAP) {
                    s.peer(key).last_gift = Some(now);
                    out.send.extend(seal_to(me, s, &key, &Msg::Gift { epoch: s.epoch, secret: s.secret }));
                }
                // A conversa também me diz onde ele está, se eu ainda não sei.
                if s.peer(key).reach.is_none() {
                    if let Some((seq, r)) = my_reach {
                        out.send.extend(seal_to(me, s, &key, &Msg::MyAddr { seq, reach: r }));
                    }
                }
            }
            Msg::Vector { page, pages, entries } => {
                let p = s.peer(key);
                if p.pages_total != pages || page == 0 {
                    p.pages.clear();
                    p.pages_total = pages;
                }
                p.pages.insert(page, entries);
                if p.pages.len() as u8 == pages {
                    let theirs: BTreeMap<MemberId, u16> = std::mem::take(&mut p.pages).into_values().flatten().collect();
                    let mut missing: Vec<&Op> = Vec::new();
                    for (author, _) in s.reg.vector() {
                        missing.extend(s.reg.ops_after(&author, theirs.get(&author).copied().unwrap_or(0) as u32));
                    }
                    missing.sort_by_key(|o| o.order());
                    for op in missing.into_iter().take(OPS_PER_ROUND) {
                        out.send.extend(seal_to(me, s, &key, &Msg::Op(op.clone())));
                    }
                }
            }
            Msg::Op(op) => {
                if is_member || s.joining.is_some() || s.invited.contains_key(&id) {
                    if s.reg.insert(op) == Insert::Added {
                        s.sync_peers(me);
                        out.events.push(Event::Changed(sid));
                    }
                }
            }
            Msg::MyAddr { seq, reach } => {
                if is_member || s.joining.as_ref().is_some_and(|h| h.key == key) {
                    let p = s.peer(key);
                    if p.reach.as_ref().is_none_or(|(old, _)| seq > *old) {
                        p.reach = Some((seq, reach.clone()));
                        out.events.push(Event::Address { key, reach });
                    }
                }
            }
            Msg::Join { vk, reach } => {
                let mine = s.invited.get(&id) == Some(&key) || s.reg.state().members.get(&id).is_some_and(|m| m.added_by == me.id);
                if !mine || !s.is_member(&me.id) {
                    return out;
                }
                s.peer(key).reach = Some((0, reach.clone()));
                out.events.push(Event::Address { key, reach });
                if !s.is_member(&id) {
                    let Some(added) = s.reg.make(&me.sk, me.id, Action::AddMember { key, vk }) else { return out };
                    s.sync_peers(me);
                    let others: Vec<[u8; 32]> = s.peers.values().filter(|p| p.online(now) && p.key != key && s.is_member(&member_id(&p.key))).map(|p| p.key).collect();
                    for k in others {
                        out.send.extend(seal_to(me, s, &k, &Msg::Op(added.clone())));
                    }
                }
                s.invited.remove(&id);
                out.send.extend(seal_to(me, s, &key, &Msg::Joined));
                let mut ops: Vec<&Op> = s.reg.ops().collect();
                ops.sort_by_key(|o| o.order());
                for op in ops.into_iter().take(OPS_PER_ROUND * 3) {
                    out.send.extend(seal_to(me, s, &key, &Msg::Op(op.clone())));
                }
                out.events.push(Event::Changed(sid));
            }
            Msg::Joined => {
                // Nada a fazer: o que me torna membro são as operações.
            }
            Msg::Ack(_) => {}
            Msg::Gift { epoch, secret } => {
                if is_member && epoch == s.reg.state().epoch && epoch > s.epoch {
                    s.epoch = epoch;
                    s.secret = secret;
                    out.events.push(Event::Changed(sid));
                }
            }
        }
        out
    }

    /// Fecha toda operação pública: confere se entrei ou fui removido e anuncia mudança da malha.
    fn finish(&mut self, now: u64, mut out: Output) -> Output {
        let ids: Vec<ServerId> = self.servers.keys().copied().collect();
        let mut announce = Vec::new();
        for id in ids {
            let s = self.servers.get_mut(&id).expect("existe");
            let in_registry = s.is_member(&self.me.id);
            if in_registry && !s.member {
                s.member = true;
                s.joining = None;
                out.events.push(Event::Joined(id));
                announce.push(id);
            } else if !in_registry && s.member {
                self.servers.remove(&id);
                out.events.push(Event::Removed(id));
                continue;
            }
            if let Some(r) = s.room {
                if !s.reg.state().rooms.contains_key(&r) {
                    s.room = None;
                }
            }
            let mesh = s.mesh_now(now);
            if mesh != s.mesh {
                out.events.push(Event::Mesh { server: id, room: mesh.0, peers: mesh.1.clone() });
                s.mesh = mesh;
            }
        }
        for id in announce {
            out.merge(self.announce_name(&id, now));
            let hello = self.hello_all(now, false);
            out.merge(hello);
        }
        out
    }

    // ---- visão para a janela ----------------------------------------------------------------------------

    pub fn view(&self, server: &ServerId, now: u64) -> Option<ServerView> {
        let s = self.servers.get(server)?;
        let st = s.reg.state();
        let mut members: Vec<MemberView> = st
            .members
            .iter()
            .map(|(id, m)| {
                let is_me = *id == self.me.id;
                let p = s.peers.get(id);
                MemberView {
                    id: *id,
                    key: m.key,
                    name: m.name.clone(),
                    role: m.role,
                    is_me,
                    online: is_me && self.visible || p.is_some_and(|p| p.online(now)),
                    room: if is_me { s.room.filter(|_| self.visible) } else { p.filter(|p| p.online(now)).and_then(|p| p.room) },
                }
            })
            .collect();
        members.sort_by(|a, b| b.online.cmp(&a.online).then(a.role.cmp(&b.role)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then(a.id.cmp(&b.id)));
        let rooms = st
            .rooms
            .iter()
            .map(|(id, r)| RoomView { id: *id, name: r.name.clone(), creator: r.creator, people: members.iter().filter(|m| m.room == Some(*id)).map(|m| m.id).collect() })
            .collect();
        Some(ServerView {
            id: s.id,
            name: s.name.clone(),
            me: self.me.id,
            my_role: st.role_of(&self.me.id),
            joining: s.joining.is_some() && !s.member,
            epoch: s.epoch,
            my_room: s.room,
            members,
            rooms,
        })
    }

    pub fn views(&self, now: u64) -> Vec<ServerView> {
        let mut v: Vec<ServerView> = self.servers.keys().filter_map(|id| self.view(id, now)).collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        v
    }

    // ---- disco ------------------------------------------------------------------------------------------

    /// Texto para guardar em disco (contém o segredo de cada servidor: o arquivo deve ser só do usuário).
    pub fn export(&self) -> String {
        let mut t = String::new();
        for s in self.servers.values() {
            let joining = s.joining.as_ref().map_or("-".to_string(), |h| format!("{}:{}:{}:{}", hex(&h.key), hex(h.reach.addr.as_bytes()), hex(&h.reach.onion), hex(&h.reach.link)));
            t.push_str(&format!("servidor|{}|{}|{}|{}|{}|{}\n", hex(&s.id), s.epoch, hex(&s.secret), hex(s.reg.owner_vk()), hex(s.name.as_bytes()), joining));
            for op in s.reg.ops() {
                t.push_str(&format!("op|{}\n", hex(&op.encode())));
            }
            for k in s.invited.values() {
                t.push_str(&format!("convite|{}\n", hex(k)));
            }
            for p in s.peers.values() {
                if let Some((seq, r)) = &p.reach {
                    t.push_str(&format!("par|{}|{}|{}|{}|{}\n", hex(&p.key), seq, hex(r.addr.as_bytes()), hex(&r.onion), hex(&r.link)));
                }
            }
        }
        t
    }

    /// Lê o que [`export`](Self::export) escreveu; linhas inválidas são ignoradas.
    pub fn import(secret: StaticSecret, text: &str) -> Self {
        let mut me = Servers::new(secret);
        let mut cur: Option<Server> = None;
        let mut done = Vec::new();
        let reach_of = |addr: &str, onion: &str, link: &str| -> Option<Reach> {
            Some(Reach { addr: String::from_utf8(unhex(addr)?).ok()?, onion: unhex(onion)?.try_into().ok()?, link: unhex(link)?.try_into().ok()? })
        };
        for line in text.lines() {
            let f: Vec<&str> = line.split('|').collect();
            match f.as_slice() {
                ["servidor", id, epoch, secret, owner_vk, name, joining] => {
                    done.extend(cur.take());
                    let parsed = (|| {
                        let id: ServerId = unhex(id)?.try_into().ok()?;
                        let joining = if *joining == "-" {
                            None
                        } else {
                            let j: Vec<&str> = joining.split(':').collect();
                            let [key, addr, onion, link] = j.as_slice() else { return None };
                            Some(Hint { key: unhex(key)?.try_into().ok()?, reach: reach_of(addr, onion, link)? })
                        };
                        Some(Server {
                            id,
                            name: String::from_utf8(unhex(name)?).ok().filter(|n| valid_name(n))?,
                            epoch: epoch.parse().ok()?,
                            secret: unhex(secret)?.try_into().ok()?,
                            reg: Registry::new(id, unhex(owner_vk)?.try_into().ok()?),
                            peers: BTreeMap::new(),
                            invited: BTreeMap::new(),
                            joining,
                            last_join: None,
                            member: false,
                            room: None,
                            mesh: (None, Vec::new()),
                        })
                    })();
                    cur = parsed;
                }
                ["op", op] => {
                    if let (Some(s), Some(op)) = (cur.as_mut(), unhex(op).and_then(|b| Op::decode(&b))) {
                        s.reg.insert(op);
                    }
                }
                ["convite", key] => {
                    if let (Some(s), Some(k)) = (cur.as_mut(), unhex(key).and_then(|b| <[u8; 32]>::try_from(b).ok())) {
                        s.invited.insert(member_id(&k), k);
                    }
                }
                ["par", key, seq, addr, onion, link] => {
                    if let (Some(s), Some(k), Ok(seq), Some(r)) = (cur.as_mut(), unhex(key).and_then(|b| <[u8; 32]>::try_from(b).ok()), seq.parse::<u64>(), reach_of(addr, onion, link)) {
                        s.peer(k).reach = Some((seq, r));
                    }
                }
                _ => {}
            }
        }
        done.extend(cur.take());
        for mut s in done {
            s.member = s.reg.state().members.contains_key(&me.me.id);
            if s.member {
                s.joining = None;
            }
            let invited: Vec<[u8; 32]> = s.invited.values().copied().collect();
            for k in invited {
                s.peer(k);
            }
            s.sync_peers(&me.me);
            me.servers.insert(s.id, s);
        }
        me
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberView {
    pub id: MemberId,
    pub key: [u8; 32],
    /// Nome que o membro escolheu (vazio se não escolheu).
    pub name: String,
    pub role: Role,
    pub is_me: bool,
    pub online: bool,
    pub room: Option<RoomId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomView {
    pub id: RoomId,
    pub name: String,
    pub creator: MemberId,
    /// Quem está na sala agora, online primeiro na ordem da lista de membros.
    pub people: Vec<MemberId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerView {
    pub id: ServerId,
    pub name: String,
    pub me: MemberId,
    pub my_role: Option<Role>,
    /// Convite aceito, mas o convidante ainda não respondeu.
    pub joining: bool,
    pub epoch: u16,
    pub my_room: Option<RoomId>,
    /// Online primeiro, depois por papel e nome.
    pub members: Vec<MemberView>,
    pub rooms: Vec<RoomView>,
}
