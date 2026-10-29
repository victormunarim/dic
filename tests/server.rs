//! Servidores e salas: vários motores trocando quadros por uma rede simulada (sem sockets).

use std::collections::{HashSet, VecDeque};

use clam::{
    media::{signal::Frame, update::Reach},
    rand_core::OsRng,
    server::{Event, MemberId, Output, Role, ServerError, ServerId, ServerView, Servers, engine::{HEARTBEAT, LEASE}, wire::{self, Msg}},
    x25519_dalek::StaticSecret,
};

struct Net {
    nodes: Vec<Servers>,
    secrets: Vec<StaticSecret>,
    down: HashSet<usize>,
    events: Vec<Vec<Event>>,
    now: u64,
    /// Descarta os `Gift` deste servidor (para testar o que acontece antes de o segredo certo chegar).
    block_gifts: Option<ServerId>,
}

impl Net {
    fn new(n: usize) -> Net {
        let secrets: Vec<StaticSecret> = (0..n).map(|_| StaticSecret::random_from_rng(OsRng)).collect();
        let mut net = Net { nodes: secrets.iter().map(|s| Servers::new(s.clone())).collect(), secrets, down: HashSet::new(), events: vec![Vec::new(); n], now: 1000, block_gifts: None };
        for i in 0..n {
            let reach = Reach { addr: format!("n{i}.exemplo:4001"), onion: [i as u8; 32], link: [i as u8 + 100; 32] };
            let out = net.nodes[i].set_reach(Some(reach), 1, net.now);
            net.run(i, out);
        }
        net
    }

    fn key(&self, i: usize) -> [u8; 32] {
        self.nodes[i].my_key()
    }

    fn id(&self, i: usize) -> MemberId {
        self.key(i)[..8].try_into().unwrap()
    }

    /// Entrega o que `from` mandou e tudo que isso provocar, até a rede ficar quieta.
    fn run(&mut self, from: usize, out: Output) {
        let mut queue: VecDeque<(usize, Frame)> = VecDeque::new();
        self.collect(from, out, &mut queue);
        let mut guard = 0;
        while let Some((to, frame)) = queue.pop_front() {
            guard += 1;
            assert!(guard < 20_000, "a rede não sossega");
            if self.down.contains(&to) {
                continue;
            }
            let contacts: Vec<[u8; 32]> = (0..self.nodes.len()).map(|j| self.key(j)).collect();
            let out = self.nodes[to].handle_with(&frame, self.now, &contacts);
            self.collect(to, out, &mut queue);
        }
    }

    fn collect(&mut self, from: usize, out: Output, queue: &mut VecDeque<(usize, Frame)>) {
        self.events[from].extend(out.events);
        for o in out.send {
            let to = (0..self.nodes.len()).find(|&j| self.key(j) == o.to).expect("destino existe");
            if let Some(server) = self.block_gifts
                && matches!(wire::open(&self.secrets[to], &self.key(from), &server, &o.frame), Some(Msg::Gift { .. }))
            {
                continue;
            }
            queue.push_back((to, o.frame));
        }
    }

    fn advance(&mut self, secs: u64) {
        for _ in 0..secs {
            self.now += 1;
            for i in 0..self.nodes.len() {
                if self.down.contains(&i) {
                    continue;
                }
                let out = self.nodes[i].tick(self.now);
                self.run(i, out);
            }
        }
    }

    fn view(&self, i: usize, server: &ServerId) -> ServerView {
        self.nodes[i].view(server, self.now).expect("servidor")
    }

    /// `a` cria o servidor e convida `others` (cada um convidado por quem veio antes).
    fn group(n: usize) -> (Net, ServerId) {
        let mut net = Net::new(n);
        let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
        net.run(0, out);
        for i in 1..n {
            let inviter = i - 1;
            let (key, now) = (net.key(i), net.now);
            let text = net.nodes[inviter].invite(&server, key, now).unwrap();
            let (_, out) = net.nodes[i].accept(&text, net.now).unwrap();
            net.run(i, out);
            net.advance(1);
        }
        (net, server)
    }
}

fn room_named(v: &ServerView, name: &str) -> [u8; 16] {
    v.rooms.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("sala {name}")).id
}

#[test]
fn invited_members_join_and_everyone_sees_the_same_server() {
    let (net, server) = Net::group(4);
    let reference = net.view(0, &server);
    assert_eq!(reference.members.len(), 4);
    assert_eq!(reference.my_role, Some(Role::Owner));
    for i in 1..4 {
        let v = net.view(i, &server);
        assert!(!v.joining, "nó {i} entrou");
        assert_eq!(v.name, "Amigos");
        assert_eq!(v.my_role, Some(Role::Member));
        assert_eq!(v.members.iter().map(|m| m.id).collect::<HashSet<_>>(), reference.members.iter().map(|m| m.id).collect::<HashSet<_>>());
        assert!(net.events[i].iter().any(|e| matches!(e, Event::Joined(s) if *s == server)));
    }
}

#[test]
fn an_invite_only_works_for_its_invitee_and_only_once() {
    let mut net = Net::new(3);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    let (k1, now) = (net.key(1), net.now);
    let text = net.nodes[0].invite(&server, k1, now).unwrap();
    assert_eq!(net.nodes[2].accept(&text, net.now).err(), Some(ServerError::NotForYou));
    let (_, out) = net.nodes[1].accept(&text, net.now).unwrap();
    net.run(1, out);
    assert_eq!(net.nodes[1].accept(&text, net.now).err(), Some(ServerError::AlreadyIn));
    assert_eq!(net.nodes[0].invite(&server, k1, now).err(), Some(ServerError::AlreadyIn));
}

#[test]
fn presence_shows_who_is_online_and_in_which_room() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    for i in 0..3 {
        assert!(net.view(i, &server).members.iter().all(|m| m.online), "todos online para o nó {i}");
    }
    let (room, out) = net.nodes[2].create_room(&server, "Jogos", net.now).unwrap();
    net.run(2, out);
    assert_eq!(room_named(&net.view(0, &server), "Jogos"), room, "a sala chega a quem não entrou");
    let out = net.nodes[1].join_room(&server, Some(room), net.now).unwrap();
    net.run(1, out);
    for i in 0..3 {
        let v = net.view(i, &server);
        let r = v.rooms.iter().find(|r| r.id == room).unwrap();
        assert_eq!(r.people, vec![net.id(1)], "nó {i} vê o nó 1 na sala");
    }
    let out = net.nodes[1].join_room(&server, None, net.now).unwrap();
    net.run(1, out);
    assert!(net.view(0, &server).rooms[0].people.is_empty());
}

#[test]
fn the_mesh_event_lists_who_shares_my_room() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    let (room, out) = net.nodes[0].create_room(&server, "Voz", net.now).unwrap();
    net.run(0, out);
    for i in [0, 1] {
        let out = net.nodes[i].join_room(&server, Some(room), net.now).unwrap();
        net.run(i, out);
    }
    let last = net.events[0].iter().rev().find_map(|e| if let Event::Mesh { peers, room, .. } = e { Some((peers.clone(), *room)) } else { None }).unwrap();
    assert_eq!(last, (vec![net.key(1)], Some(room)));
    let out = net.nodes[0].join_room(&server, None, net.now).unwrap();
    net.run(0, out);
    let last = net.events[0].iter().rev().find_map(|e| if let Event::Mesh { peers, room, .. } = e { Some((peers.clone(), *room)) } else { None }).unwrap();
    assert_eq!(last, (vec![], None));
}

#[test]
fn a_screen_is_only_served_to_those_in_the_room_who_ask() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    let (room, out) = net.nodes[0].create_room(&server, "Voz", net.now).unwrap();
    net.run(0, out);
    for i in [0, 1] {
        let out = net.nodes[i].join_room(&server, Some(room), net.now).unwrap();
        net.run(i, out);
    }
    // Quem não transmite ignora o pedido.
    let out = { let (k, now) = (net.key(0), net.now); net.nodes[1].watch(&server, k, true, now) }.unwrap();
    net.run(1, out);
    assert!(!net.events[0].iter().any(|e| matches!(e, Event::Watch { .. })));
    let out = net.nodes[0].set_sharing(true, net.now);
    net.run(0, out);
    assert!(net.view(1, &server).members.iter().find(|m| m.key == net.key(0)).unwrap().sharing);
    assert!(net.view(0, &server).members.iter().find(|m| m.is_me).unwrap().sharing);
    let out = { let (k, now) = (net.key(0), net.now); net.nodes[1].watch(&server, k, true, now) }.unwrap();
    net.run(1, out);
    assert!(net.events[0].contains(&Event::Watch { server, from: net.key(1), on: true }));
    // Quem está fora da sala nem consegue pedir.
    assert_eq!({ let (k, now) = (net.key(0), net.now); net.nodes[2].watch(&server, k, true, now) }.err(), Some(ServerError::UnknownMember));
    // Parar de transmitir some da lista; sair da sala também.
    let out = net.nodes[0].set_sharing(false, net.now);
    net.run(0, out);
    assert!(!net.view(1, &server).members.iter().find(|m| m.key == net.key(0)).unwrap().sharing);
}

#[test]
fn someone_who_goes_silent_leaves_the_room_when_the_lease_runs_out() {
    let (mut net, server) = Net::group(2);
    net.advance(2);
    let (room, out) = net.nodes[0].create_room(&server, "Voz", net.now).unwrap();
    net.run(0, out);
    let out = net.nodes[1].join_room(&server, Some(room), net.now).unwrap();
    net.run(1, out);
    assert_eq!(net.view(0, &server).rooms[0].people.len(), 1);
    net.down.insert(1);
    net.advance(LEASE + 2);
    let v = net.view(0, &server);
    assert!(v.rooms[0].people.is_empty());
    assert!(!v.members.iter().find(|m| m.id == net.id(1)).unwrap().online);
    net.down.clear();
    net.advance(HEARTBEAT * 5);
    assert!(net.view(0, &server).members.iter().all(|m| m.online), "voltou a aparecer");
}

#[test]
fn a_member_who_was_away_catches_up_with_what_changed() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    net.down.insert(2);
    let (_, out) = net.nodes[0].create_room(&server, "Um", net.now).unwrap();
    net.run(0, out);
    let (_, out) = net.nodes[1].create_room(&server, "Dois", net.now).unwrap();
    net.run(1, out);
    assert!(net.view(2, &server).rooms.is_empty());
    net.down.clear();
    let out = net.nodes[2].online(net.now);
    net.run(2, out);
    net.advance(HEARTBEAT);
    let names: HashSet<String> = net.view(2, &server).rooms.into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["Um".to_string(), "Dois".to_string()].into());
}

#[test]
fn any_member_creates_rooms_but_only_managers_remove_people() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    let target = net.id(2);
    assert_eq!(net.nodes[1].remove_member(&server, target, net.now).err(), Some(ServerError::Forbidden));
    let (room, out) = net.nodes[1].create_room(&server, "Do membro", net.now).unwrap();
    net.run(1, out);
    assert_eq!(net.nodes[2].delete_room(&server, room, net.now).err(), Some(ServerError::Forbidden));
    let out = net.nodes[0].remove_member(&server, target, net.now).unwrap();
    net.run(0, out);
    net.advance(2);
    let v = net.view(1, &server);
    assert_eq!(v.members.len(), 2);
    assert_eq!(v.epoch, 1, "o segredo trocou");
    assert_eq!(net.view(0, &server).epoch, 1);
    assert_eq!(net.view(2, &server).epoch, 0, "quem saiu não recebe o segredo novo");
}

#[test]
fn deleting_the_room_you_are_in_fails_until_you_leave_it() {
    let (mut net, server) = Net::group(2);
    net.advance(2);
    let (room, out) = net.nodes[0].create_room(&server, "Sala", net.now).unwrap();
    net.run(0, out);
    let out = net.nodes[0].join_room(&server, Some(room), net.now).unwrap();
    net.run(0, out);
    assert_eq!(net.nodes[0].delete_room(&server, room, net.now).err(), Some(ServerError::InRoom));
    let out = net.nodes[0].join_room(&server, None, net.now).unwrap();
    net.run(0, out);
    assert!(net.nodes[0].delete_room(&server, room, net.now).is_ok());
}

#[test]
fn a_fake_gift_from_a_plain_member_does_not_replace_the_server_secret() {
    let (mut net, server) = Net::group(4);
    net.advance(2);
    net.down.insert(3);
    net.block_gifts = Some(server);
    let target = net.id(2);
    let out = net.nodes[0].remove_member(&server, target, net.now).unwrap();
    net.run(0, out);
    net.down.clear();
    let out = net.nodes[3].online(net.now);
    net.run(3, out);
    net.advance(3);
    assert_eq!(net.view(3, &server).epoch, 0, "ainda sem o segredo novo");
    // Um membro comum entrega um segredo qualquer, para a época certa.
    let fake = wire::seal(&net.secrets[1], &net.key(3), &server, &Msg::Gift { epoch: 1, secret: [9; 32] }).unwrap();
    let contacts: Vec<[u8; 32]> = (0..4).map(|j| net.key(j)).collect();
    net.nodes[3].handle_with(&fake, net.now, &contacts);
    assert_eq!(net.view(3, &server).epoch, 0, "o segredo falso não vale");
    // O verdadeiro ainda entra.
    net.block_gifts = None;
    net.advance(HEARTBEAT * 2);
    assert_eq!(net.view(3, &server).epoch, 1);
}

#[test]
fn a_removed_member_learns_it_if_it_syncs_before_the_rekey_but_never_sees_new_rooms() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    let (t, now) = (net.id(2), net.now);
    let out = net.nodes[0].remove_member(&server, t, now).unwrap();
    net.run(0, out);
    let (_, out) = net.nodes[0].create_room(&server, "Secreta", net.now).unwrap();
    net.run(0, out);
    net.advance(HEARTBEAT * 3);
    assert!(net.view(1, &server).rooms.iter().any(|r| r.name == "Secreta"));
    assert!(net.view(2, &server).rooms.iter().all(|r| r.name != "Secreta"));
}

#[test]
fn display_names_are_set_once_and_shared() {
    let (mut net, server) = Net::group(2);
    let out = net.nodes[1].set_name("  Bia\u{7}  ", net.now);
    net.run(1, out);
    net.advance(2);
    let m = net.view(0, &server).members.into_iter().find(|m| m.id == net.id(1)).unwrap();
    assert_eq!(m.name, "Bia");
}

#[test]
fn invisible_members_do_not_show_up_but_still_see_others() {
    let (mut net, server) = Net::group(2);
    net.advance(2);
    let out = net.nodes[1].set_visible(false, net.now);
    net.run(1, out);
    net.advance(LEASE + HEARTBEAT * 2);
    assert!(!net.view(0, &server).members.iter().find(|m| m.id == net.id(1)).unwrap().online);
    let out = net.nodes[1].set_visible(true, net.now);
    net.run(1, out);
    net.advance(2);
    assert!(net.view(0, &server).members.iter().find(|m| m.id == net.id(1)).unwrap().online);
}

#[test]
fn rooms_stop_at_eight_people() {
    let (mut net, server) = Net::group(9);
    net.advance(3);
    let (room, out) = net.nodes[0].create_room(&server, "Cheia", net.now).unwrap();
    net.run(0, out);
    for i in 0..8 {
        let out = net.nodes[i].join_room(&server, Some(room), net.now).unwrap();
        net.run(i, out);
    }
    assert_eq!(net.nodes[8].join_room(&server, Some(room), net.now).err(), Some(ServerError::Full));
    assert_eq!(net.view(8, &server).rooms[0].people.len(), 8);
}

#[test]
fn the_state_survives_a_restart() {
    let (mut net, server) = Net::group(2);
    let (room, out) = net.nodes[1].create_room(&server, "Fica", net.now).unwrap();
    net.run(1, out);
    let saved = net.nodes[1].export();
    assert!(!saved.contains("Amigos"), "o nome vai em hexadecimal, não em claro");
    let back = Servers::import(net.secrets[1].clone(), &saved);
    let v = back.view(&server, net.now).unwrap();
    assert_eq!(v.name, "Amigos");
    assert_eq!(v.members.len(), 2);
    assert_eq!(v.rooms.iter().map(|r| r.id).collect::<Vec<_>>(), vec![room]);
    assert_eq!(back.export(), saved);
    let other = Servers::import(net.secrets[0].clone(), "servidor|lixo\nop|zz\npar|1\n");
    assert!(other.is_empty());
}

#[test]
fn frames_from_outsiders_and_junk_are_ignored() {
    let (mut net, server) = Net::group(2);
    let mut stranger = Net::new(1);
    let (other, out) = stranger.nodes[0].create("Outro", stranger.now).unwrap();
    stranger.run(0, out);
    let text = stranger.nodes[0].invite(&other, net.key(1), stranger.now).unwrap();
    let _ = text;
    let mut seed = 7u32;
    for _ in 0..3000 {
        let mut f = [0u8; 200];
        for x in f.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *x = (seed >> 24) as u8;
        }
        f[0] = 8;
        f[1..5].copy_from_slice(&server[..4]);
        f[5..13].copy_from_slice(&net.key(1)[..8]);
        let out = net.nodes[0].handle(&f, net.now);
        assert!(out.is_empty());
    }
    assert!(net.nodes[0].handle(&[1, 2, 3], net.now).is_empty());
    assert_eq!(net.view(0, &server).members.len(), 2);
}

#[test]
fn an_invite_reaches_a_contact_over_the_network_and_is_accepted_there() {
    let mut net = Net::new(3);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    let (key, now) = (net.key(2), net.now);
    let out = net.nodes[0].invite_send(&server, key, now).unwrap();
    net.run(0, out);
    let pending = net.nodes[2].pending_invites();
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].name.as_str(), pending[0].from), ("Amigos", net.key(0)));
    assert!(net.events[2].iter().any(|e| matches!(e, Event::Invitation { .. })));
    let (_, out) = net.nodes[2].accept(&pending[0].text, net.now).unwrap();
    net.run(2, out);
    net.advance(2);
    assert!(net.nodes[2].pending_invites().is_empty());
    assert_eq!(net.view(2, &server).members.len(), 2);
    assert!(!net.view(2, &server).joining);
}

#[test]
fn an_invite_from_a_stranger_is_ignored_and_a_declined_one_does_not_come_back() {
    let mut net = Net::new(2);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    let (key, now) = (net.key(1), net.now);
    let out = net.nodes[0].invite_send(&server, key, now).unwrap();
    for o in &out.send {
        let r = net.nodes[1].handle_with(&o.frame, net.now, &[]);
        assert!(r.ack.is_none() && r.events.is_empty(), "quem não é contato não é atendido");
    }
    assert!(net.nodes[1].pending_invites().is_empty());
    net.run(0, out);
    net.nodes[1].decline_invite(&server);
    assert!(net.nodes[1].pending_invites().is_empty());
    net.advance(61);
    assert!(net.nodes[1].pending_invites().is_empty(), "recusado não volta pelo reenvio");
}

#[test]
fn an_invite_to_someone_offline_is_delivered_again_until_it_arrives() {
    let mut net = Net::new(2);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    net.down.insert(1);
    let (key, now) = (net.key(1), net.now);
    let out = net.nodes[0].invite_send(&server, key, now).unwrap();
    net.run(0, out);
    net.advance(30);
    net.down.remove(&1);
    assert!(net.nodes[1].pending_invites().is_empty());
    net.advance(40);
    assert_eq!(net.nodes[1].pending_invites().len(), 1, "o reenvio entregou");
}

#[test]
fn the_delivery_of_an_invite_survives_the_inviter_restarting() {
    let mut net = Net::new(2);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    net.down.insert(1);
    let (key, now) = (net.key(1), net.now);
    let out = net.nodes[0].invite_send(&server, key, now).unwrap();
    net.run(0, out);
    // Quem convidou fecha e abre de novo: o reenvio continua de onde parou.
    let saved = net.nodes[0].export();
    net.nodes[0] = Servers::import(net.secrets[0].clone(), &saved);
    let reach = Reach { addr: "n0.exemplo:4001".into(), onion: [0; 32], link: [100; 32] };
    let out = net.nodes[0].set_reach(Some(reach), 2, net.now);
    net.run(0, out);
    net.down.remove(&1);
    assert!(net.nodes[1].pending_invites().is_empty());
    net.advance(70);
    assert_eq!(net.nodes[1].pending_invites().len(), 1, "o reenvio sobreviveu ao reinício");
}

#[test]
fn a_received_invite_survives_the_invitee_restarting() {
    let mut net = Net::new(2);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    let (key, now) = (net.key(1), net.now);
    let out = net.nodes[0].invite_send(&server, key, now).unwrap();
    net.run(0, out);
    let before = net.nodes[1].pending_invites();
    assert_eq!(before.len(), 1);
    let saved = net.nodes[1].export();
    net.nodes[1] = Servers::import(net.secrets[1].clone(), &saved);
    assert_eq!(net.nodes[1].pending_invites(), before);
    let reach = Reach { addr: "n1.exemplo:4001".into(), onion: [1; 32], link: [101; 32] };
    let out = net.nodes[1].set_reach(Some(reach), 2, net.now);
    net.run(1, out);
    let (_, out) = net.nodes[1].accept(&before[0].text, net.now).unwrap();
    net.run(1, out);
    net.advance(2);
    assert!(net.nodes[1].pending_invites().is_empty());
    assert_eq!(net.view(1, &server).members.len(), 2);
}

#[test]
fn a_saved_invite_that_is_not_for_me_or_from_someone_else_is_dropped() {
    let mut net = Net::new(3);
    let (server, out) = net.nodes[0].create("Amigos", net.now).unwrap();
    net.run(0, out);
    let (key, now) = (net.key(1), net.now);
    let out = net.nodes[0].invite_send(&server, key, now).unwrap();
    net.run(0, out);
    let saved = net.nodes[1].export();
    // Outra identidade não abre o texto; um remetente trocado não confere.
    assert!(Servers::import(net.secrets[2].clone(), &saved).pending_invites().is_empty());
    let forged = saved.replace(&net.key(0).iter().map(|b| format!("{b:02x}")).collect::<String>(), &"00".repeat(32));
    assert!(Servers::import(net.secrets[1].clone(), &forged).pending_invites().is_empty());
}

#[test]
fn room_chat_reaches_only_the_room_and_is_forgotten_when_it_empties() {
    let (mut net, server) = Net::group(3);
    net.advance(2);
    let (room, out) = net.nodes[0].create_room(&server, "Voz", net.now).unwrap();
    net.run(0, out);
    for i in [0, 1] {
        let out = net.nodes[i].join_room(&server, Some(room), net.now).unwrap();
        net.run(i, out);
    }
    let out = net.nodes[0].say(&server, "  oi, pessoal\n ", net.now).unwrap();
    net.run(0, out);
    let lines = |net: &Net, i: usize| net.view(i, &server).rooms[0].chat.iter().map(|l| (l.from, l.text.clone())).collect::<Vec<_>>();
    assert_eq!(lines(&net, 0), vec![(net.id(0), "oi, pessoal".to_string())]);
    assert_eq!(lines(&net, 1), vec![(net.id(0), "oi, pessoal".to_string())]);
    assert!(lines(&net, 2).is_empty(), "quem está fora da sala não recebe");
    // Quem está fora não consegue falar; texto vazio não manda nada.
    assert_eq!(net.nodes[2].say(&server, "psiu", net.now).err(), Some(ServerError::UnknownMember));
    assert!(net.nodes[0].say(&server, "   ", net.now).unwrap().is_empty());
    // A sala só esquece a conversa quando fica sem ninguém.
    let out = net.nodes[0].join_room(&server, None, net.now).unwrap();
    net.run(0, out);
    assert_eq!(lines(&net, 1).len(), 1, "ainda tem gente");
    let out = net.nodes[1].join_room(&server, None, net.now).unwrap();
    net.run(1, out);
    for i in 0..3 {
        assert!(lines(&net, i).is_empty(), "nó {i} esqueceu");
    }
}
