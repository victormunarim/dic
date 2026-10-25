//! Laço do nó: atende, disca e executa os comandos da janela.

use super::*;

/// Roda o nó até `stop`: atende contatos sozinho e disca quando a janela pede.
pub(crate) async fn run_node(form: Form, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Shared>, stop: Arc<AtomicBool>, out: Sink) -> Result<(), String> {
    let secret = form.identity()?;
    let ident = secret.clone();
    // Só contatos são atendidos sozinhos; apelidos servem apenas para exibir nomes.
    let contacts: Vec<_> = parse_contacts(&form.contacts);
    let names = form.names();
    let mut cfg = NodeConfig::new(secret, form.directory_addr(), contacts.iter().map(|(_, k)| *k).collect());
    (cfg.known, cfg.hints) = load_known();
    // Códigos diretos na lista de contatos trazem o relay deles: dica para ligar sem diretório.
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
    out.line(format!("{} endereço(s) de contatos guardados em {} ({} deles só vieram de convites)", cfg.known.len() + cfg.hints.len(), known_path().display(), cfg.hints.len()));
    if cfg.directory.trim().is_empty() {
        out.line("sem diretório: só ligo e recebo por código de contato direto (veja \"Meu código de contato\")");
    } else {
        out.line("conectando ao diretório...");
    }
    let mut node = Node::start(cfg).await.map_err(|e| format!("não ficou online: {e}"))?;
    *shared.status.lock().unwrap() = format!("online em {}", node.published_addr());
    *shared.relay.lock().unwrap() = node.relay_info();
    out.line(format!("online; os outros chegam em {}", node.published_addr()));
    if let Some(n) = node.network_note() {
        out.line(n);
    }
    let dialer = node.dialer();
    let mut saver = KnownSaver::new(&node.known(), &node.hints());
    let (dialed_tx, mut dialed_rx) = mpsc::unbounded_channel::<([u8; 32], u32, Option<Result<ActiveCall, clam::node::DialError>>)>();
    let (redial_tx, mut redial_rx) = mpsc::unbounded_channel::<([u8; 32], u32)>();
    // Discar demora (furo de NAT, endereço guardado, espera de resposta): numa tarefa, para a janela seguir
    // atendendo comandos; "Desligar" cancela.
    let spawn_dial = |peer: [u8; 32], attempt: u32, delay: Duration| {
        let (dialer, sh, tx, out) = (dialer.clone(), shared.clone(), dialed_tx.clone(), out.clone());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            out.line(match dialer.stored_address(&peer) {
                Some(a) => format!("ligando: tento antes o endereço guardado do contato ({a}); o diretório só entra se ele não responder"),
                None if dialer.has_directory() => "ligando: não há endereço guardado deste contato; vou consultar o diretório".to_string(),
                None => "ligando: não há endereço deste contato e não há diretório; precisa do código de contato direto dele".to_string(),
            });
            let r = until_hangup(dialer.dial(peer, G722::new(), Duration::from_secs(15)), &sh.hangup).await;
            let _ = tx.send((peer, attempt, r));
        });
    };
    let mut room: Option<RoomRun> = None;
    let mut room_tick = interval(Duration::from_millis(20));
    room_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            inc = node.next_incoming() => {
                let Some(inc) = inc else { break };
                if let Some(r) = &room
                    && let Some(view) = &r.view
                    && view.members.contains(inc.caller().as_bytes())
                {
                    if room::accept_member(inc, view, &r.mesh).await {
                        out.line("alguém da sala entrou na chamada");
                    }
                    continue;
                }
                let name = member_name(&names, inc.caller().as_bytes());
                if shared.in_call.swap(true, Ordering::Relaxed) {
                    out.line(format!("{name} ligou, mas já há uma chamada; recusada"));
                    inc.reject().await;
                    continue;
                }
                out.line(format!("chamada de {name}"));
                shared.hangup.store(false, Ordering::Relaxed);
                let caller = *inc.caller().as_bytes();
                match inc.accept(G722::new()).await {
                    Ok(c) => start_talk(c, 660.0, &shared, &out, &form, None, Peer { key: caller, dialer: dialer.clone() }),
                    Err(e) => {
                        out.line(format!("não atendeu: {e}"));
                        shared.in_call.store(false, Ordering::Relaxed);
                    }
                }
            }
            Some((peer, attempt, r)) = dialed_rx.recv() => {
                saver.sync(&node.known(), &node.hints(), &out);
                match (dial_step(attempt, r.as_ref().map(|r| r.is_ok())), r) {
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
                    // Discar demora (furo de NAT, endereço guardado, espera de resposta): numa tarefa, para a
                    // janela seguir atendendo comandos; "Desligar" cancela a ligação.
                    spawn_dial(peer, 0, Duration::ZERO);
                }
                Some(Cmd::Join(mut secret, owner, name)) => {
                    if shared.in_call.swap(true, Ordering::Relaxed) {
                        out.line("saia da chamada atual antes de entrar numa sala");
                        continue;
                    }
                    out.line(format!("entrando na sala {name}..."));
                    let me = node.public_key();
                    // Se o dono já trocou o segredo (alguém foi removido), segue a troca antes de entrar.
                    if let Some(o) = owner.filter(|o| *o != me) {
                        let new = follow_chain(node.directory(), &ident, &o, secret, &shared).await;
                        if new != secret {
                            out.line(format!("sala {name}: o dono trocou o segredo; usando o novo"));
                            secret = new;
                        }
                    }
                    shared.owner.store(owner == Some(me), Ordering::Relaxed);
                    shared.in_room.store(true, Ordering::Relaxed);
                    let mesh = Mesh::new();
                    *shared.mesh.lock().unwrap() = Some(mesh.clone());
                    *shared.me.lock().unwrap() = node.public_key();
                    room = Some(RoomRun {
                        name,
                        secret,
                        owner,
                        key: room::room_key(&secret),
                        mesh,
                        view: None,
                        sound: open_sound(form.use_audio, &form, &out),
                        next_sync: tokio::time::Instant::now(),
                        next_dial: tokio::time::Instant::now(),
                        shown: (0, 0, [0; 32]),
                        n: 0,
                        decoders: Default::default(),
                    });
                }
                Some(Cmd::Remove(gone)) => {
                    let Some(r) = room.as_mut() else { continue };
                    let (Some(view), true) = (r.view.as_ref(), r.owner == Some(node.public_key())) else {
                        out.line("só o dono da sala remove membros");
                        continue;
                    };
                    let keep: Vec<[u8; 32]> = view.members.iter().copied().filter(|m| *m != gone && *m != node.public_key()).collect();
                    match room::rotate(node.directory(), &ident, &r.secret, &keep).await {
                        Ok(new) => {
                            shared.rekeyed.lock().unwrap().push((r.secret, new));
                            r.secret = new;
                            r.key = room::room_key(&new);
                            r.mesh.retain(&keep);
                            r.view = None;
                            r.next_sync = tokio::time::Instant::now();
                            out.line(format!("{} foi removido da sala {}; o segredo mudou e só quem ficou recebe o novo", member_name(&names, &gone), r.name));
                        }
                        Err(e) => out.line(format!("não deu para remover: {e}")),
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
                Some(Cmd::Leave) => {
                    if let Some(r) = room.take() {
                        leave_room(&node, r, &shared, &out).await;
                    }
                }
                None => break,
            },
            _ = room_tick.tick(), if room.is_some() => {
                let Some(r) = room.as_mut() else { continue };
                for f in mic_frames(r.sound.as_ref(), 440.0, r.n, &shared) {
                    r.mesh.send(&f);
                }
                r.n += 1;
                if r.n.is_multiple_of(50) {
                    report_error(r.sound.as_ref(), &out);
                }
                if let Some(f) = r.mesh.mix() {
                    play_scaled(r.sound.as_ref(), f, &shared);
                }
                for (who, ev) in r.mesh.take_screen_events() {
                    if !on_screen_event(&shared.screen, who, r.decoders.entry(who).or_default(), ev) {
                        out.line(format!("tela de {}: quadro inválido; esperando o próximo quadro-chave", member_name(&names, &who)));
                    }
                }
                if r.n.is_multiple_of(50) {
                    shared.screen.retain(|k| r.mesh.has(k));
                    r.decoders.retain(|k, _| r.mesh.has(k));
                    if shared.screen.want_share.load(Ordering::Relaxed) && !shared.screen.share_task.swap(true, Ordering::Relaxed) {
                        tokio::spawn(room_share_task(dialer.clone(), r.mesh.clone(), shared.clone(), out.clone()));
                    }
                }
                // Discagem em segundo plano e em paralelo; repete a cada 2 s, o que religa sozinho quem caiu.
                if tokio::time::Instant::now() >= r.next_dial {
                    r.next_dial = tokio::time::Instant::now() + Duration::from_secs(2);
                    if let Some(view) = &r.view {
                        room::connect_missing(&dialer, view, &r.mesh, Duration::from_secs(8));
                    }
                }
                if tokio::time::Instant::now() >= r.next_sync {
                    r.next_sync = tokio::time::Instant::now() + Duration::from_secs(10);
                    // Membro: o dono pode ter trocado o segredo para tirar alguém; passa para o novo.
                    if let Some(o) = r.owner.filter(|o| *o != node.public_key()) {
                        let new = follow_chain(node.directory(), &ident, &o, r.secret, &shared).await;
                        if new != r.secret {
                            out.line(format!("sala {}: o dono trocou o segredo; acompanhando", r.name));
                            r.secret = new;
                            r.key = room::room_key(&new);
                        }
                    }
                    match room::sync(&node, &r.key).await {
                        Ok(view) => {
                            room::connect_missing(&dialer, &view, &r.mesh, Duration::from_secs(8));
                            let shown = (view.members.len(), r.mesh.count(), view.host);
                            if shown != r.shown {
                                r.shown = shown;
                                let host = if view.i_am_host { "você".to_string() } else { member_name(&names, &view.host) };
                                let text = format!("sala {}: {} pessoa(s), {} conectada(s); anfitrião: {host}", r.name, shown.0, shown.1);
                                out.line(text.clone());
                                *shared.room.lock().unwrap() = text;
                            }
                            // Quem saiu da lista (removido pelo dono) perde também a chamada.
                            r.mesh.retain(&view.members);
                            *shared.members.lock().unwrap() = view.members.clone();
                            r.view = Some(view);
                        }
                        Err(e) => out.line(format!("sala {}: diretório: {e}", r.name)),
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        if let Some(n) = node.take_notice() {
            out.line(n);
        }
        // Os endereços mudam sozinhos (renovação com o diretório, convites): grava quando mudam, não só
        // depois de uma ligação, para um fechamento brusco não perder o que foi aprendido.
        if saver.due() {
            saver.sync(&node.known(), &node.hints(), &out);
        }
    }
    saver.sync(&node.known(), &node.hints(), &out);
    if let Some(r) = room.take() {
        leave_room(&node, r, &shared, &out).await;
    }
    shared.hangup.store(true, Ordering::Relaxed);
    Ok(())
}

/// Mantém `.clam-gui.known` igual aos endereços de contatos que o nó conhece, avisando de erro de gravação
/// uma vez por erro diferente (não a cada tentativa).
struct KnownSaver {
    saved: (Stored, Stored),
    last_error: Option<String>,
    next_check: tokio::time::Instant,
}

impl KnownSaver {
    const EVERY: Duration = Duration::from_secs(5);

    /// `known` e `hints` são o que já está no arquivo (lido na partida): só grava quando algo for diferente disso.
    fn new(known: &Stored, hints: &Stored) -> Self {
        Self { saved: (Self::sorted(known), Self::sorted(hints)), last_error: None, next_check: tokio::time::Instant::now() + Self::EVERY }
    }

    fn sorted(list: &Stored) -> Stored {
        let mut v = list.clone();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    fn due(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        if now < self.next_check {
            return false;
        }
        self.next_check = now + Self::EVERY;
        true
    }

    fn sync(&mut self, known: &Stored, hints: &Stored, out: &Sink) {
        let now = (Self::sorted(known), Self::sorted(hints));
        if now == self.saved && self.last_error.is_none() {
            return;
        }
        match save_known(&now.0, &now.1) {
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
