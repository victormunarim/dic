//! Laço do nó: atende, disca e executa os comandos da janela.

use clam::sync::LockExt;
use super::*;

/// Roda o nó até `stop`: atende contatos sozinho e disca quando a janela pede.
pub(crate) async fn run_node(form: Form, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Shared>, stop: Arc<AtomicBool>, out: Sink) -> Result<(), String> {
    let secret = form.identity()?;
        // Só contatos são atendidos sozinhos; apelidos servem apenas para exibir nomes.
    let contacts: Vec<_> = parse_contacts(&form.contacts);
    let names = form.names();
    let mut cfg = NodeConfig::new(secret, contacts.iter().map(|(_, k)| *k).collect());
    let book = load_known();
    (cfg.known, cfg.hints, cfg.history) = (book.known, book.hints, book.history);
    // Códigos diretos na lista de contatos trazem o relay deles: dica para ligar sem ter ligado antes.
    for (k, r) in contact_relays(&form.contacts) {
        cfg.hints.retain(|(h, _)| *h != k);
        cfg.hints.push((k, r));
    }
    let (relay_keys, keys_error) = load_relay_keys();
    cfg.relay_keys = Some(relay_keys);
    if let Some(e) = keys_error {
        out.line(format!("não consegui gravar as chaves do relay em {}: {e}; os contatos perdem o endereço guardado de você a cada vez que o programa reabre", relay_keys_path().display()));
    }
    if !form.public_addr.trim().is_empty() {
        cfg.public_addr = Some(form.public_addr.trim().to_string());
    }
    cfg.name = form.my_name.trim().to_string();
    cfg.introductions = form.intros;
    cfg.anchor = form.anchor;
    cfg.anchors = parse_anchors(&form.anchors);
    cfg.servers = load_servers();
    cfg.custody = load_custody();
    cfg.tombstones = load_tombs();
    if form.use_dht {
        cfg.dht = Some(clam::rendezvous::DhtOptions { circuit: form.dht_circuit, ..Default::default() });
        if form.dht_circuit {
            out.line("DHT ligada por circuito: o endereço é guardado e lido pelos relays de dois contatos, sem expor o seu IP à DHT (sem dois relays de contatos a DHT fica parada)");
        } else {
            out.line("DHT ligada: o endereço é guardado, cifrado para cada contato, na DHT do BitTorrent (quem opera nós da DHT vê o seu IP)");
        }
    }
    out.line(format!("{} endereço(s) de contatos guardados em {} ({} deles só vieram de convites)", cfg.known.len() + cfg.hints.len(), known_path().display(), cfg.hints.len()));
    let mut node = Node::start(cfg).await.map_err(|e| format!("não ficou online: {e}"))?;
    *shared.status.lock_unpoisoned() = format!("online em {}", node.published_addr());
    *shared.relay.lock_unpoisoned() = node.relay_info();
    out.line(format!("online; os outros chegam em {}", node.published_addr()));
    if let Some(n) = node.network_note() {
        out.line(n);
    }
    // Apresentações que ficaram sem confirmação da última vez (só de quem ainda é contato).
    let pending = load_pending();
    for (_, key) in &contacts {
        if pending.contains(key) {
            node.introduce(*key);
        }
    }
    // Avisos de remoção que ficaram sem confirmação (só de quem não voltou a ser contato).
    for key in load_removals() {
        if contacts.iter().any(|(_, k)| *k == key) {
            continue;
        }
        node.retire(key);
    }
    let mut pending_saved = pending_now(&node);
    let mut removals_saved = node.pending_removals();
    removals_saved.sort();
    let dialer = node.dialer();
    let mut receiving = super::files::Receiving::default();
    // Só vale quem ainda é contato (o `Form` já descarta o resto).
    for key in form.trusted_keys() {
        dialer.set_trusted(key, true);
    }
    tokio::spawn(probe_contacts(dialer.clone(), shared.clone(), stop.clone()));
    let hub = node.servers();
    if !form.my_name.trim().is_empty() {
        hub.run(|s, now| s.set_name(form.my_name.trim(), now));
    }
    *shared.servers.lock_unpoisoned() = Some(hub.clone());
    let mut servers_saved = hub.read(|s, _| s.export());
    let mut custody_saved = node.custody_export();
    let mut tombs_saved = sorted(node.tombstones());
    let mut servers_check = tokio::time::Instant::now();
    // A sala de voz em que estou: o canal da tarefa e quem está na malha (esses são atendidos sem tocar).
    let mut room: Option<(mpsc::UnboundedSender<RoomCmd>, clam::server::ServerId)> = None;
    let mut mesh: Vec<[u8; 32]> = Vec::new();
    let mut saver = KnownSaver::new(&book_of(&node));
    let (dialed_tx, mut dialed_rx) = mpsc::unbounded_channel::<([u8; 32], u32, Option<Result<ActiveCall, clam::node::DialError>>)>();
    // Chamada que a pessoa atendeu (ou não) depois de tocar: o toque espera numa tarefa, para o nó seguir vivo.
    let (answered_tx, mut answered_rx) = mpsc::unbounded_channel::<([u8; 32], Result<ActiveCall, String>)>();
    let (redial_tx, mut redial_rx) = mpsc::unbounded_channel::<([u8; 32], u32)>();
    // Discar demora (endereço guardado, espera de resposta): numa tarefa, para a janela seguir
    // atendendo comandos; "Desligar" cancela.
    let spawn_dial = |peer: [u8; 32], attempt: u32, delay: Duration| {
        let (dialer, sh, tx, out) = (dialer.clone(), shared.clone(), dialed_tx.clone(), out.clone());
        *shared.ringing.lock_unpoisoned() = Some(peer);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            out.line(match dialer.stored_address(&peer) {
                Some(a) => format!("ligando: tento o endereço guardado do contato ({a})"),
                None => "ligando: não há endereço guardado deste contato; tento a DHT e as âncoras, se ligadas".to_string(),
            });
            let codec = if sh.opus.load(Ordering::Relaxed) { CodecId::Opus } else { CodecId::G722 };
            let r = until_hangup(dialer.dial(peer, codec.build(), Duration::from_secs(15)), &sh.hangup).await;
            let _ = tx.send((peer, attempt, r));
        });
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            inc = node.next_incoming() => {
                let Some(inc) = inc else { break };
                let name = member_name(&names, inc.caller().as_bytes());
                if let Some((tx, _)) = &room {
                    // Na sala só a malha é atendida, sem tocar: é a pessoa que entrou na mesma sala.
                    if mesh.contains(inc.caller().as_bytes()) {
                        let caller = *inc.caller().as_bytes();
                        let codec = inc.codec();
                        match inc.accept(codec.build()).await {
                            Ok(c) => {
                                let _ = tx.send(RoomCmd::Call(caller, c));
                            }
                            Err(e) => out.line(format!("sala: não atendeu {name}: {e}")),
                        }
                    } else {
                        inc.reject().await;
                    }
                    continue;
                }
                if shared.in_call.swap(true, Ordering::Relaxed) {
                    out.line(format!("{name} ligou, mas já há uma chamada; recusada"));
                    inc.reject().await;
                    continue;
                }
                out.line(format!("chamada de {name}"));
                let caller = *inc.caller().as_bytes();
                // Quem liga escolhe o codec; telefone antigo não diz e fica em G.722.
                let codec = inc.codec();
                out.line(format!("codec da chamada: {codec:?}"));
                // Só conecta quando a pessoa atende: a chamada toca até ela aceitar, recusar ou passar `RING_SECS`.
                shared.answer.store(ANSWER_NONE, Ordering::Relaxed);
                *shared.incoming.lock_unpoisoned() = Some(caller);
                let (sh, tx, out) = (shared.clone(), answered_tx.clone(), out.clone());
                tokio::spawn(async move {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(RING_SECS);
                    let answer = loop {
                        let a = sh.answer.load(Ordering::Relaxed);
                        if a != ANSWER_NONE || tokio::time::Instant::now() >= deadline {
                            break a;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    };
                    *sh.incoming.lock_unpoisoned() = None;
                    sh.answer.store(ANSWER_NONE, Ordering::Relaxed);
                    let result = if answer == ANSWER_ACCEPT {
                        inc.accept(codec.build()).await.map_err(|e| e.to_string())
                    } else {
                        inc.reject().await;
                        Err(if answer == ANSWER_REJECT { "recusada".to_string() } else { "ninguém atendeu".to_string() })
                    };
                    out.line(format!("chamada recebida: {}", if result.is_ok() { "atendida" } else { "não atendida" }));
                    let _ = tx.send((caller, result));
                });
            }
            Some((caller, r)) = answered_rx.recv() => {
                match r {
                    Ok(c) => {
                        shared.hangup.store(false, Ordering::Relaxed);
                        start_talk(c, 660.0, &shared, &out, &form, None, Peer { key: caller, dialer: dialer.clone() });
                    }
                    Err(e) => {
                        out.line(format!("não atendeu: {e}"));
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                }
            }
            Some((peer, attempt, r)) = dialed_rx.recv() => {
                saver.sync(&book_of(&node), &out);
                let step = dial_step(attempt, r.as_ref().map(|r| r.is_ok()));
                // Entre uma tentativa e a próxima a janela segue "chamando"; em qualquer outro desfecho, acabou.
                if !matches!(step, DialStep::Retry) {
                    *shared.ringing.lock_unpoisoned() = None;
                }
                match (step, r) {
                    (DialStep::Talk, Some(Ok(c))) => {
                        out.line(if attempt > 0 { "religada" } else { "atendida" });
                        start_talk(c, 440.0, &shared, &out, &form, Some((redial_tx.clone(), peer)), Peer { key: peer, dialer: dialer.clone() });
                    }
                    (DialStep::Retry, Some(Err(e))) => {
                        out.line(format!("não religou ({attempt}/{REDIAL_TRIES}): {e}"));
                        let tx = redial_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            let _ = tx.send((peer, attempt + 1));
                        });
                    }
                    (_, Some(Err(e))) => {
                        out.line(format!("não completou: {e}"));
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                    _ => {
                        out.line("ligação cancelada");
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                }
            }
            Some((peer, attempt)) = redial_rx.recv() => {
                if shared.hangup.load(Ordering::Relaxed) {
                    out.line("ligação cancelada");
                    *shared.ringing.lock_unpoisoned() = None;
                    shared.in_call.store(false, Ordering::Relaxed);
                } else {
                    out.line(format!("religando ({attempt}/{REDIAL_TRIES})..."));
                    spawn_dial(peer, attempt, Duration::ZERO);
                }
            }
            cmd = rx.recv() => match cmd {
                Some(Cmd::Dial(peer, relay)) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("já há uma chamada em andamento");
                        continue;
                    }
                    shared.hangup.store(false, Ordering::Relaxed);
                    if let Some(r) = relay {
                        node.add_hint(peer, r);
                    }
                    out.line("ligando...");
                    // Discar demora (endereço guardado, espera de resposta): numa tarefa, para a
                    // janela seguir atendendo comandos; "Desligar" cancela a ligação.
                    spawn_dial(peer, 0, Duration::ZERO);
                }
                Some(Cmd::Contact(key, relay)) => {
                    node.add_contact(key, relay);
                    // A pessoa adicionada também passa a ter este contato (veja `Node::introduce`).
                    node.introduce(key);
                    save_pending_if_changed(&node, &mut pending_saved, &out);
                    save_removals_if_changed(&node, &mut removals_saved, &out);
                    saver.sync(&book_of(&node), &out);
                }
                Some(Cmd::Trust(key, on)) => dialer.set_trusted(key, on),
                Some(Cmd::Chat(peer, text)) => {
                    let parts = crate::chats::split_text(&text, clam::media::update::MAX_CHAT);
                    let sends: Vec<(u64, String)> = parts.into_iter().map(|t| (dialer.next_chat_id(), t)).collect();
                    {
                        let mut book = shared.chats.lock_unpoisoned();
                        for (id, t) in &sends {
                            book.push_mine(peer, *id, t.clone());
                        }
                    }
                    // Em fila única, para as partes de um texto longo chegarem na ordem.
                    let (sh, d, o) = (shared.clone(), dialer.clone(), out.clone());
                    tokio::spawn(async move {
                        for (id, t) in sends {
                            let ok = d.send_chat(peer, id, &t, Duration::from_secs(8)).await;
                            sh.chats.lock_unpoisoned().set_state(&peer, id, if ok { crate::chats::State::Sent } else { crate::chats::State::Failed });
                            (o.repaint)();
                        }
                    });
                }
                Some(Cmd::SendFile(peer, path)) => super::files::start_send(&shared, &dialer, &out, peer, path),
                Some(Cmd::FileCancel(peer, id, mine)) => super::files::cancel(&shared, &dialer, &out, &mut receiving, peer, id, mine),
                Some(Cmd::Retire(key)) => {
                    node.retire(key);
                    save_pending_if_changed(&node, &mut pending_saved, &out);
                    save_removals_if_changed(&node, &mut removals_saved, &out);
                    saver.sync(&book_of(&node), &out);
                }
                Some(Cmd::JoinRoom(server, room_id)) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("encerre a chamada antes de entrar na sala");
                        continue;
                    }
                    shared.hangup.store(false, Ordering::Relaxed);
                    // Marca a sala antes de abrir o áudio: a janela não pode ver "em chamada" sem "em sala" e
                    // abrir a aba Chamada.
                    shared.room.lock_unpoisoned().at = Some((server, room_id));
                    if let Err(e) = hub.try_act(|s, now| s.join_room(&server, Some(room_id), now)) {
                        out.line(format!("não entrei na sala: {e}"));
                        shared.room.lock_unpoisoned().clear();
                        shared.in_call.store(false, Ordering::Relaxed);
                        continue;
                    }
                    let sound = open_sound(form.use_audio, &form, &out);
                    let (tx, rrx) = mpsc::unbounded_channel();
                    let (sh, o, d, me) = (shared.clone(), out.clone(), dialer.clone(), hub.my_key());
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        run_room(rrx, tx2, sound, me, d, sh.clone(), o).await;
                        sh.in_call.store(false, Ordering::Relaxed);
                    });
                    room = Some((tx, server));
                    out.line("entrou na sala");
                }
                Some(Cmd::LeaveRoom) => {
                    if let Some((tx, server)) = room.take() {
                        let _ = tx.send(RoomCmd::Leave);
                        let _ = hub.try_act(|s, now| s.join_room(&server, None, now));
                        node.set_room_peers(&[]);
                        mesh.clear();
                    }
                }
                Some(Cmd::RoomShare(on)) => {
                    if let Some((tx, _)) = &room {
                        shared.screen.want_share.store(on, Ordering::Relaxed);
                        hub.run(|s, now| s.set_sharing(on, now));
                        let _ = tx.send(RoomCmd::Share(on));
                    }
                }
                Some(Cmd::MicTest) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("encerre a chamada antes de testar o microfone");
                        continue;
                    }
                    match open_sound(true, &form, &out) {
                        Some(sound) => {
                            shared.mic_test.store(true, Ordering::Relaxed);
                            out.line("teste do microfone: você se ouve com um pequeno atraso; use fones, senão o microfone capta o alto-falante e o teste faz microfonia");
                            tokio::spawn(mic_test(sound, shared.clone(), stop.clone(), out.clone()));
                        }
                        None => shared.in_call.store(false, Ordering::Relaxed),
                    }
                }
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        for e in hub.take_events() {
            use clam::server::Event;
            match e {
                Event::Mesh { peers, .. } => {
                    node.set_room_peers(&peers);
                    if let Some((tx, _)) = &room {
                        let _ = tx.send(RoomCmd::Mesh(peers.clone()));
                    }
                    mesh = peers;
                }
                Event::Watch { from, on, .. } => {
                    if let Some((tx, _)) = &room {
                        let _ = tx.send(RoomCmd::Viewer(from, on));
                    }
                }
                Event::Joined(_) => out.line("entrou no servidor"),
                Event::Removed(id) => {
                    out.line("você foi removido de um servidor");
                    if room.as_ref().is_some_and(|(_, s)| *s == id) {
                        if let Some((tx, _)) = room.take() {
                            let _ = tx.send(RoomCmd::Leave);
                        }
                        node.set_room_peers(&[]);
                        mesh.clear();
                    }
                }
                Event::Notice(n) => out.line(n),
                Event::Invitation { .. } => out.line("um contato convidou você para um servidor (aba Servidores)"),
                Event::Changed(_) | Event::Address { .. } => {}
            }
            (out.repaint)();
        }
        if servers_check.elapsed() >= Duration::from_secs(5) {
            servers_check = tokio::time::Instant::now();
            let now = hub.read(|s, _| s.export());
            if now != servers_saved {
                match save_servers(&now) {
                    Ok(()) => servers_saved = now,
                    Err(e) => out.line(format!("não consegui gravar os servidores em {}: {e}", servers_path().display())),
                }
            }
        }
        if let Some(n) = node.take_notice() {
            out.line(n);
        }
        let got = node.take_chats();
        if !got.is_empty() {
            let mut book = shared.chats.lock_unpoisoned();
            for (peer, c) in got {
                book.push_received(peer, c.id, c.text);
            }
            drop(book);
            (out.repaint)();
        }
        let files = node.take_files();
        if !files.is_empty() {
            // Contato não precisa de aceite: o arquivo começa a chegar e a pessoa pode cancelar.
            for (peer, id) in super::files::on_frames(&shared, &mut receiving, files) {
                super::files::answer(&shared, &dialer, &out, &mut receiving, peer, id, true);
            }
            (out.repaint)();
        }
        save_chats_if_dirty(&shared, &out);
        let introduced = node.take_introductions();
        if !introduced.is_empty() {
            for i in &introduced {
                let r = &i.reach;
                // Quem já era contato e se apresenta é a aceitação do convite que esta pessoa deixou (pela caixa, se estava offline).
                let accepted = node.is_contact(&i.peer);
                node.add_contact(i.peer, Some(clam::node::RelayInfo { addr: r.addr.clone(), onion: r.onion, link: r.link }));
                let who = if i.name.is_empty() { "alguém" } else { &i.name };
                if accepted {
                    out.line(format!("{who} ({}) aceitou o seu convite", &hex(&i.peer)[..8]));
                } else {
                    out.line(format!("{who} ({}) adicionou você e se apresentou: já está nos seus contatos", &hex(&i.peer)[..8]));
                }
            }
            shared.introduced.lock_unpoisoned().extend(introduced);
            saver.sync(&book_of(&node), &out);
        }
        let removed = node.take_removals();
        if !removed.is_empty() {
            for key in &removed {
                node.forget(key);
                out.line(format!("{} ({}) removeu você: saiu dos seus contatos", member_name(&contacts, key), &hex(key)[..8]));
            }
            shared.removed.lock_unpoisoned().extend(removed);
            save_pending_if_changed(&node, &mut pending_saved, &out);
            saver.sync(&book_of(&node), &out);
        }
        // Os endereços mudam sozinhos (renovação na DHT, convites): grava quando mudam, não só
        // depois de uma ligação, para um fechamento brusco não perder o que foi aprendido.
        if saver.due() {
            saver.sync(&book_of(&node), &out);
            save_pending_if_changed(&node, &mut pending_saved, &out);
            save_removals_if_changed(&node, &mut removals_saved, &out);
            let tombs = sorted(node.tombstones());
            if tombs != tombs_saved && save_tombs(&tombs).is_ok() {
                tombs_saved = tombs;
            }
            let held = node.custody_export();
            if held != custody_saved {
                match save_custody(&held) {
                    Ok(()) => custody_saved = held,
                    Err(e) => out.line(format!("não consegui gravar as apresentações em custódia em {}: {e}", custody_path().display())),
                }
            }
        }
    }
    save_chats_if_dirty(&shared, &out);
    saver.sync(&book_of(&node), &out);
    let _ = save_custody(&node.custody_export());
    let _ = save_tombs(&node.tombstones());
    let _ = save_servers(&hub.read(|s, _| s.export()));
    if let Some((tx, _)) = room.take() {
        let _ = tx.send(RoomCmd::Leave);
    }
    *shared.servers.lock_unpoisoned() = None;
    shared.hangup.store(true, Ordering::Relaxed);
    Ok(())
}

/// Grava `.clam-gui.chats` quando o histórico mudou.
fn save_chats_if_dirty(shared: &Shared, out: &Sink) {
    let Some(text) = shared.chats.lock_unpoisoned().take_dirty() else { return };
    if let Err(e) = crate::chats::save_chats(&text) {
        out.line(format!("não consegui gravar o bate-papo em {}: {e}", crate::chats::chats_path().display()));
    }
}

/// Nome do contato, ou os 8 primeiros caracteres da chave.
pub(crate) fn member_name(contacts: &[(String, [u8; 32])], key: &[u8; 32]) -> String {
    contacts.iter().find(|(_, k)| k == key).map(|(n, _)| n.clone()).unwrap_or_else(|| hex(key)[..8].to_string())
}

/// Contatos a quem a apresentação ainda não foi confirmada, em ordem estável.
fn pending_now(node: &Node) -> Vec<[u8; 32]> {
    let mut keys = node.pending_introductions();
    keys.sort();
    keys
}

/// Grava `.clam-gui.intro` quando a fila de apresentações pendentes mudou (entregue, adicionada ou removida).
fn save_pending_if_changed(node: &Node, saved: &mut Vec<[u8; 32]>, out: &Sink) {
    let now = pending_now(node);
    if now == *saved {
        return;
    }
    if let Err(e) = save_pending(&now) {
        out.line(format!("não consegui gravar as apresentações pendentes em {}: {e}", pending_path().display()));
    }
    *saved = now;
}

/// Grava `.clam-gui.removed` quando a fila de avisos de remoção pendentes mudou.
fn save_removals_if_changed(node: &Node, saved: &mut Vec<[u8; 32]>, out: &Sink) {
    let mut now = node.pending_removals();
    now.sort();
    if now == *saved {
        return;
    }
    if let Err(e) = save_removals(&now) {
        out.line(format!("não consegui gravar as remoções pendentes em {}: {e}", removals_path().display()));
    }
    *saved = now;
}

/// O que o nó sabe dos endereços dos contatos, no formato do arquivo.
fn book_of(node: &Node) -> Book {
    Book { known: node.known(), hints: node.hints(), history: node.history() }
}

/// Mantém `.clam-gui.known` igual aos endereços de contatos que o nó conhece, avisando de erro de gravação
/// uma vez por erro diferente (não a cada tentativa).
struct KnownSaver {
    saved: Book,
    last_error: Option<String>,
    next_check: tokio::time::Instant,
}

impl KnownSaver {
    const EVERY: Duration = Duration::from_secs(5);

    /// `book` é o que já está no arquivo (lido na partida): só grava quando algo for diferente disso.
    fn new(book: &Book) -> Self {
        Self { saved: book.sorted(), last_error: None, next_check: tokio::time::Instant::now() + Self::EVERY }
    }

    fn due(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        if now < self.next_check {
            return false;
        }
        self.next_check = now + Self::EVERY;
        true
    }

    fn sync(&mut self, book: &Book, out: &Sink) {
        let now = book.sorted();
        if now == self.saved && self.last_error.is_none() {
            return;
        }
        match save_known(&now) {
            Ok(()) => {
                self.saved = now;
                self.last_error = None;
            }
            Err(e) => {
                let text = format!("não consegui gravar os endereços dos contatos em {}: {e}", known_path().display());
                if self.last_error.as_ref() != Some(&text) {
                    out.line(text.clone());
                }
                self.last_error = Some(text);
            }
        }
    }
}

/// Ordem estável para comparar o que mudou desde a última gravação (o mapa do nó não tem ordem).
fn sorted(mut tombs: Vec<([u8; 32], u64)>) -> Vec<([u8; 32], u64)> {
    tombs.sort_unstable();
    tombs
}

/// Quanto vale uma sondagem de presença antes de repetir.
const PROBE_EVERY: Duration = Duration::from_secs(60);

/// Sonda os contatos da lista, um por vez (cada sondagem monta um circuito), do mais antigo ao mais novo; o
/// resultado vai a `Shared::presence` e a lista de contatos bloqueia o 📞 de quem não responde. Contato em
/// chamada conta como online sem sondar.
async fn probe_contacts(dialer: clam::node::Dialer, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let mut checked: std::collections::HashMap<[u8; 32], tokio::time::Instant> = Default::default();
    while !stop.load(Ordering::Relaxed) {
        let watch = shared.watch.lock_unpoisoned().clone();
        let now = tokio::time::Instant::now();
        let busy = *shared.peer.lock_unpoisoned();
        let due = watch.iter().filter(|k| checked.get(*k).is_none_or(|t| now.duration_since(*t) >= PROBE_EVERY)).min_by_key(|k| checked.get(*k).copied());
        match due {
            Some(key) if Some(*key) == busy => {
                checked.insert(*key, now);
                shared.presence.lock_unpoisoned().insert(*key, true);
            }
            Some(key) => {
                let up = dialer.is_online(key).await;
                checked.insert(*key, tokio::time::Instant::now());
                shared.presence.lock_unpoisoned().insert(*key, up);
            }
            None => tokio::time::sleep(Duration::from_secs(1)).await,
        }
        checked.retain(|k, _| watch.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_name_prefers_contact_names() {
        let contacts = vec![("ana".to_string(), [1u8; 32])];
        assert_eq!(member_name(&contacts, &[1; 32]), "ana");
        assert_eq!(member_name(&contacts, &[2; 32]), "02020202");
    }
}
