//! Janela (egui): desenha o formulário e fala com o motor.

use std::{sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
}, time::Duration};

use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};
use eframe::egui;
use tokio::sync::mpsc;

use crate::{conf::*, engine::*};


pub(crate) struct App {
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) form: Form,
    pub(crate) log: Log,
    pub(crate) shared: Arc<Shared>,
    pub(crate) running: Option<Running>,
    pub(crate) done: Arc<AtomicBool>,
    pub(crate) error: String,
    /// Ajustes de áudio mudaram e ainda não foram salvos (salva ao soltar o controle).
    pub(crate) audio_dirty: bool,
    /// Microfones e alto-falantes que o sistema listou (atualizável na janela).
    #[cfg_attr(not(feature = "audio"), allow(dead_code))]
    pub(crate) devs: (Vec<String>, Vec<String>),
    /// Textura da tela de quem transmite para você e o número da imagem que ela mostra.
    /// Tela de quem está aberta numa janela própria, em tamanho grande.
    pub(crate) zoom: Option<[u8; 32]>,
    pub(crate) screen_tex: std::collections::HashMap<[u8; 32], (u64, egui::TextureHandle)>,
}

impl App {
    /// Escolha de microfone e alto-falante (vazio = padrão do sistema); vale na próxima chamada.
    #[cfg(feature = "audio")]
    fn devices_ui(&mut self, ui: &mut egui::Ui, offline: bool) {
        let _ = offline;
        let pick = |ui: &mut egui::Ui, label: &str, cur: &mut String, list: &[String]| {
            ui.horizontal(|ui| {
                ui.label(label);
                let shown = if cur.is_empty() { "padrão do sistema".to_string() } else { cur.clone() };
                egui::ComboBox::from_id_salt(label).selected_text(shown).width(320.0).show_ui(ui, |ui| {
                    ui.selectable_value(cur, String::new(), "padrão do sistema");
                    for d in list {
                        ui.selectable_value(cur, d.clone(), d);
                    }
                });
            });
        };
        let (ins, outs) = self.devs.clone();
        pick(ui, "Microfone", &mut self.form.input_dev, &ins);
        pick(ui, "Alto-falante", &mut self.form.output_dev, &outs);
        ui.weak(format!("Sistema de áudio: {}", clam::audio::host_name()));
        if ui.small_button("Atualizar lista de dispositivos").clicked() {
            self.devs = clam::audio::devices();
        }
    }

    pub(crate) fn go_online(&mut self, ctx: &egui::Context) {
        self.error.clear();
        if let Err(e) = self.form.identity() {
            self.error = e;
            return;
        }
        if let Err(e) = self.form.save_checked() {
            self.error = format!("não deu para gravar a configuração em {}: {e}; os contatos não ficam salvos", conf_path().display());
        }
        self.log.lock().unwrap().clear();
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let out = Sink {
            log: self.log.clone(),
            repaint: {
                let ctx = ctx.clone();
                Arc::new(move || ctx.request_repaint())
            },
        };
        let (form, shared, stop2, done2) = (self.form.clone(), self.shared.clone(), stop.clone(), done.clone());
        self.rt.spawn(async move {
            if let Err(e) = run_node(form, rx, shared, stop2, out.clone()).await {
                out.line(format!("ERRO: {e}"));
            }
            out.line("offline");
            done2.store(true, Ordering::Relaxed);
            (out.repaint)();
        });
        self.running = Some(Running { cmds: tx, stop });
        self.done = done;
    }
}

pub(crate) fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title).strong().size(15.0));
        ui.add_space(4.0);
        add(ui);
    });
    ui.add_space(8.0);
}

impl App {
    /// Mudo, volume, sensibilidade e tratamento da voz, medidor do microfone e teste do microfone.
    fn audio_ui(&mut self, ui: &mut egui::Ui, in_call: bool, testing: bool) {
        let a = &self.shared.audio;
        let mut muted = a.muted.load(Ordering::Relaxed);
        if ui.checkbox(&mut muted, "Mudo (o outro lado não ouve você)").changed() {
            a.muted.store(muted, Ordering::Relaxed);
        }
        let before = a.prefs();
        let mut p = before;
        ui.add(egui::Slider::new(&mut p.volume, AudioPrefs::VOLUME).text("Volume do alto-falante"));
        ui.add(egui::Slider::new(&mut p.mic_gain, AudioPrefs::MIC_GAIN).text("Ganho do microfone"));

        // Medidor: a linha marca o limiar de sensibilidade; o texto diz se a voz passa dele.
        let level = a.level();
        let speaking = a.speaking() && !muted;
        let meter = ui.add(egui::ProgressBar::new(level).desired_width(260.0).text(if speaking { "Microfone: voz" } else { "Microfone" }));
        let x = meter.rect.left() + meter.rect.width() * voice_meter(p.gate_db);
        ui.painter().vline(x, meter.rect.y_range(), egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(230, 160, 40)));

        ui.add(egui::Slider::new(&mut p.gate_db, AudioPrefs::GATE_DB).suffix(" dB").text("Sensibilidade (limiar da voz)"))
            .on_hover_text("A linha laranja no medidor. Quanto mais para a esquerda, mais sensível: sons baixos já contam como voz.");
        ui.checkbox(&mut p.denoise, "Remover ruído (rede neural)")
            .on_hover_text("Tira teclado, ventilador, estática e vozes ao fundo (RNNoise). Com ela ligada, a porta e o indicador \"voz\" só contam o que a rede reconhece como fala. Adiciona ~10 ms de atraso.");
        ui.checkbox(&mut p.echo_cancel, "Cancelar eco do alto-falante")
            .on_hover_text("Tira do microfone o que saiu pelo alto-falante, para o outro lado não se ouvir de volta. Leva alguns segundos de conversa para aprender o ambiente. Com fones não é preciso.");
        ui.checkbox(&mut p.echo_suppress, "Abaixar o microfone enquanto só há eco")
            .on_hover_text("Reforço para o eco que o cancelador deixa passar: com o outro lado falando, o microfone cai 18 dB enquanto estiver mais de 20 dB abaixo do que sai pelo alto-falante. Quem fala baixinho por cima do outro pode ser cortado; com fones, desligue.");
        if let Some(erle) = a.echo_erle().filter(|_| p.echo_cancel && (in_call || testing)) {
            ui.weak(format!("Eco cancelado: {erle:.0} dB"));
        }
        ui.checkbox(&mut p.gate, "Silenciar o que não é voz")
            .on_hover_text("Quando o microfone fica abaixo do limiar, o som enviado é reduzido. Ajuste a sensibilidade até a linha ficar logo acima do ruído.");
        ui.add_enabled(p.gate, egui::Slider::new(&mut p.gate_depth_db, AudioPrefs::GATE_DEPTH_DB).suffix(" dB").text("Redução do ruído"));
        ui.checkbox(&mut p.high_pass, "Cortar graves (ventilador, mesa, sopro)");
        ui.checkbox(&mut p.agc, "Volume automático (nivela voz baixa e alta)")
            .on_hover_text("Reforça quem fala baixo e contém quem fala alto. Não reforça o ruído de fundo.");
        ui.add_enabled_ui(!in_call, |ui| {
            let mut opus = self.form.voice_opus;
            if ui
                .checkbox(&mut opus, "Voz em Opus (metade da banda do G.722)")
                .on_hover_text("Vale para as chamadas que você inicia; ao atender, vale o que o outro escolheu. O Opus usa 32 kbps contra 64 kbps do G.722 e esconde melhor a perda de pacotes, mas só funciona se o outro lado também for desta versão; contra um telefone antigo, desmarque. Vale na próxima ligação.")
                .changed()
            {
                self.form.voice_opus = opus;
                self.form.save();
            }
        });
        self.shared.opus.store(self.form.voice_opus, Ordering::Relaxed);
        if ui.small_button("Restaurar padrões do áudio").clicked() {
            p = AudioPrefs::default();
        }

        if p != before {
            a.set_prefs(p);
            self.audio_dirty = true;
        }
        // Salva ao soltar o controle, não a cada quadro do arrasto.
        if self.audio_dirty && !ui.input(|i| i.pointer.any_down()) {
            self.audio_dirty = false;
            self.form.audio = p;
            self.form.save();
        }

        if in_call || testing {
            ui.weak("Fale e veja o medidor: a linha laranja deve ficar abaixo da sua voz e acima do ruído.");
        }
        if testing {
            if ui.button("Parar o teste").clicked() {
                self.shared.mic_test.store(false, Ordering::Relaxed);
            }
        } else if ui.add_enabled(!in_call, egui::Button::new("Testar o microfone")).clicked()
            && let Some(r) = &self.running
        {
            let _ = r.cmds.send(Cmd::MicTest);
        }
    }

    pub(crate) fn contacts_ui(&mut self, ui: &mut egui::Ui, online: bool, in_call: bool) {
        // Quem adicionou esta pessoa se apresentou (o nó já o atende): entra na lista, também na tela dele.
        let introduced: Vec<_> = std::mem::take(&mut *self.shared.introduced.lock().unwrap());
        let mut added = false;
        for i in introduced {
            if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == i.peer) {
                if !self.form.contacts.trim().is_empty() {
                    self.form.contacts.push('\n');
                }
                self.form.contacts.push_str(&intro_line(&i.name, &i.peer, &i.reach));
                added = true;
            }
        }
        if added {
            self.form.save();
        }
        let contacts = parse_contacts(&self.form.contacts);
        if contacts.is_empty() {
            ui.weak("Nenhum contato ainda. Peça o código de contato de um amigo e adicione abaixo.");
        }
        let mut remove = None;
        for (i, (name, key)) in contacts.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(name).size(14.0));
                ui.weak(&hex(key)[..8]);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remover").clicked() {
                        remove = Some(i);
                    }
                    if ui.add_enabled(online && !in_call, egui::Button::new("📞 Ligar")).clicked()
                        && let Some(r) = &self.running
                    {
                        self.error.clear();
                        let _ = r.cmds.send(Cmd::Dial(*key, self.form.relay_for(key)));
                    }
                });
            });
        }
        if let Some(i) = remove {
            let key = contacts[i].1;
            self.form.contacts = remove_contact(&self.form.contacts, i);
            // O endereço guardado do contato vai junto (a menos que a mesma chave ainda esteja na lista).
            if !parse_contacts(&self.form.contacts).iter().any(|(_, k)| *k == key) {
                match &self.running {
                    Some(r) => {
                        let _ = r.cmds.send(Cmd::Forget(key));
                    }
                    None => {
                        if let Err(e) = forget_known(&key).and_then(|()| mark_pending(key, false)) {
                            self.error = format!("não deu para tirar o endereço do contato de {}: {e}", known_path().display());
                        }
                    }
                }
            }
            self.form.save();
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.form.new_name).hint_text("Nome").desired_width(110.0));
            ui.add(egui::TextEdit::singleline(&mut self.form.new_code).hint_text("Código de contato").desired_width(260.0));
            if ui.button("Adicionar contato").clicked() {
                if split_code(&self.form.new_code).is_none() {
                    self.error = "Esse código de contato não parece certo (são 64 letras e números, e o que vem depois deles tem de estar inteiro).".into();
                } else {
                    let name: String = self.form.new_name.chars().filter(|c| !matches!(c, ';' | '\n')).collect();
                    if let Some((key, relay)) = split_direct(&self.form.new_code) {
                        match (&self.running, relay) {
                            (Some(r), relay) => {
                                let _ = r.cmds.send(Cmd::Contact(key, relay));
                            }
                            (None, relay) => {
                                // Com o nó parado: o endereço entra como dica e a apresentação fica na fila do próximo início.
                                if let Err(e) = relay.map_or(Ok(()), |r| remember_hint(key, r)).and_then(|()| mark_pending(key, true)) {
                                    self.error = format!("não deu para guardar o contato em {}: {e}", known_path().display());
                                }
                            }
                        }
                    }
                    let line = format!("{} {}", name.trim(), self.form.new_code.trim());
                    if !self.form.contacts.trim().is_empty() {
                        self.form.contacts.push('\n');
                    }
                    self.form.contacts.push_str(line.trim());
                    self.form.new_name.clear();
                    self.form.new_code.clear();
                    self.error.clear();
                    self.form.save();
                }
            }
        });
    }
}

impl App {
    /// Compartilhar a própria tela e ver a dos outros, na chamada .
    fn screen_ui(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let (wanted, sharing) = (st.want_share.load(Ordering::Relaxed), st.sharing.load(Ordering::Relaxed));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let label = if wanted { "⏹ Parar de compartilhar a tela" } else { "🖥 Compartilhar minha tela" };
            if ui.button(label).clicked() {
                st.want_share.store(!wanted, Ordering::Relaxed);
            }
            if sharing {
                ui.colored_label(egui::Color32::LIGHT_GREEN, "● sua tela está sendo vista");
            } else if wanted {
                ui.weak("ligando o circuito de vídeo...");
            }
        });
        if !wanted && !sharing {
            let note = st.note.lock().unwrap().clone();
            if !note.is_empty() {
                ui.colored_label(egui::Color32::LIGHT_RED, note);
            }
        }
        // A escolha vale a partir da próxima transmissão; fica salva para não precisar mudar de novo.
        st.open_path.store(!self.form.screen_private, Ordering::Relaxed);
        st.sharp.store(self.form.screen_sharp, Ordering::Relaxed);
        st.hardware.store(self.form.screen_hardware, Ordering::Relaxed);
        ui.add_enabled_ui(!wanted && !sharing, |ui| {
            let changed = ui
                .checkbox(&mut self.form.screen_private, "Privacidade na transmissão da tela")
                .on_hover_text("Ligada, a tela sai num ritmo constante, com tráfego de enchimento, e quem observa a rede não vê quando a imagem se mexe. Desligada, só sai o que há para enviar e o ritmo máximo dobra: a imagem fica mais nítida e fluida, mas o vídeo segue cifrado. Vale na próxima transmissão.")
                .changed();
            if changed {
                self.form.save();
            }
            ui.horizontal(|ui| {
                ui.label("Priorizar:");
                let mut changed = ui
                    .radio_value(&mut self.form.screen_sharp, false, "Fluidez")
                    .on_hover_text("Mais quadros por segundo (o dobro): bom para jogo e vídeo. A imagem fica um pouco mais grosseira quando se mexe.")
                    .changed();
                changed |= ui
                    .radio_value(&mut self.form.screen_sharp, true, "Nitidez")
                    .on_hover_text("Metade dos quadros por segundo, cada um com o dobro de bits: bom para texto, código e slides. O movimento fica menos suave.")
                    .changed();
                if changed {
                    self.form.save();
                }
            });
            if ui
                .checkbox(&mut self.form.screen_hardware, "Codificar a tela pela placa de vídeo (experimental)")
                .on_hover_text("Usa a GPU (VA-API, NVENC ou Quick Sync, via GStreamer) em vez do processador. Precisa de gstreamer1.0-tools e gstreamer1.0-plugins-bad com o driver da GPU; sem isso, ou se ela parar, o programa volta sozinho ao openh264. Menos CPU, mas sem o controle fino de nitidez por quadro, e a imagem pode demorar um pouco mais a reagir. Não funciona no Windows. Vale na próxima transmissão.")
                .changed()
            {
                self.form.save();
            }
        });
        let diag = st.diagnostics();
        if !diag.is_empty() {
            ui.weak(diag);
        }
        let names = self.form.names();
        let mut shown: Vec<([u8; 32], u64, egui::ColorImage)> = Vec::new();
        {
            let images = st.images.lock().unwrap();
            self.screen_tex.retain(|k, _| images.contains_key(k));
            for (who, (seq, img)) in images.iter() {
                if self.screen_tex.get(who).is_none_or(|(n, _)| n != seq) {
                    shown.push((*who, *seq, egui::ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.rgba)));
                }
            }
        }
        for (who, seq, color) in shown {
            // Atualiza a textura que já existe: criar uma nova na GPU a cada quadro (15 por segundo, em
            // 1080p) enche a memória de vídeo de texturas à espera de serem liberadas.
            match self.screen_tex.get_mut(&who) {
                Some((n, tex)) => {
                    tex.set(color, egui::TextureOptions::LINEAR);
                    *n = seq;
                }
                None => {
                    let tex = ui.ctx().load_texture(format!("tela-{}", who[0]), color, egui::TextureOptions::LINEAR);
                    self.screen_tex.insert(who, (seq, tex));
                }
            }
        }
        let mut order: Vec<_> = self.screen_tex.iter().collect();
        order.sort_by_key(|(k, _)| **k);
        for (who, (_, tex)) in order {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(format!("Tela de {}:", member_name(&names, who)));
                if ui.button("🔍 Ampliar").on_hover_text("Abre a tela numa janela maior").clicked() {
                    self.zoom = Some(*who);
                }
            });
            // Ampliada, a miniatura sai: a imagem só aparece na janela ampliada (e não é desenhada duas vezes).
            if self.zoom != Some(*who) {
                ui.add(egui::Image::new(tex).max_width(ui.available_width()).shrink_to_fit());
            }
        }
    }
}

thread_local! {
    /// Fim do último quadro desenhado, para notar quando a janela fica parada.
    static LAST_FRAME_END: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

impl App {
    /// Registra no log da janela uma lentidão da própria janela, no máximo uma vez por segundo.
    fn note_slow(&mut self, line: String) {
        thread_local!(static LAST_NOTE: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) });
        let due = LAST_NOTE.with(|l| l.get()).is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due {
            LAST_NOTE.with(|l| l.set(Some(std::time::Instant::now())));
            self.log.lock().unwrap().push(line);
        }
    }
}

impl App {
    /// Janela própria com a tela de `zoom`, ajustada ao tamanho da janela; fecha sozinha se a tela some.
    fn zoom_window(&mut self, ctx: &egui::Context) {
        let Some(who) = self.zoom else { return };
        let Some((_, tex)) = self.screen_tex.get(&who) else {
            self.zoom = None;
            return;
        };
        let title = format!("Tela de {}", member_name(&self.form.names(), &who));
        let size = tex.size_vec2();
        // Janela flutuante dentro da janela do app: um segundo viewport (outra janela do sistema com
        // contexto GL próprio) travava a janela principal no Wayland, que esperava a troca de buffers das duas.
        let screen = ctx.screen_rect().size();
        let fit = (screen.x * 0.9 / size.x).min(screen.y * 0.9 / size.y).min(1.0);
        let mut open = true;
        egui::Window::new(title)
            .id(egui::Id::new("tela-ampliada"))
            .open(&mut open)
            .resizable(true)
            .default_size(size * fit)
            .show(ctx, |ui| {
                ui.add(egui::Image::new(tex).fit_to_exact_size(ui.available_size()).maintain_aspect_ratio(true));
            });
        if !open {
            self.zoom = None;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let began = std::time::Instant::now();
        let watching = !self.shared.screen.images.lock().unwrap().is_empty();
        if let Some(end) = LAST_FRAME_END.with(|l| l.get()) {
            // Vendo tela a janela repinta a cada 16 ms: bem mais que isso sem atualizar é a janela travada fora da
            // nossa conta (troca de buffers, eventos do sistema).
            let gap = began.duration_since(end);
            if watching && gap > Duration::from_millis(400) {
                self.note_slow(format!("janela: {} ms sem atualizar entre dois quadros", gap.as_millis()));
            }
        }
        if self.done.swap(false, Ordering::Relaxed) {
            self.running = None;
            self.shared.in_call.store(false, Ordering::Relaxed);
            self.shared.status.lock().unwrap().clear();
            *self.shared.relay.lock().unwrap() = None;
        }
        self.zoom_window(ctx);
        let offline = self.running.is_none();
        let in_call = self.shared.in_call.load(Ordering::Relaxed);
        let status = self.shared.status.lock().unwrap().clone();
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Clam");
                    ui.weak("chamadas de voz entre pessoas, sem servidor no meio");
                });
                ui.add_space(6.0);

                card(ui, "Conexão", |ui| {
                    ui.horizontal(|ui| {
                        let (color, text) = if offline {
                            (egui::Color32::GRAY, "Offline".to_string())
                        } else if status.is_empty() {
                            (egui::Color32::YELLOW, "Conectando...".to_string())
                        } else {
                            (egui::Color32::LIGHT_GREEN, "Online".to_string())
                        };
                        ui.colored_label(color, format!("● {text}"));
                        if !status.is_empty() {
                            ui.weak(&status);
                        }
                    });
                    ui.weak(format!(
                        "Configuração: {} ({})",
                        conf_path().display(),
                        if self.form.loaded { "lida ao abrir" } else { "não encontrada ao abrir; será criada" }
                    ));
                    ui.add_enabled(offline, egui::Checkbox::new(&mut self.form.use_audio, "Usar microfone e alto-falante"));
                    #[cfg(feature = "audio")]
                    self.devices_ui(ui, offline);
                    ui.add_space(4.0);
                    if offline {
                        if ui.add_sized([160.0, 30.0], egui::Button::new("Ficar online")).clicked() {
                            self.go_online(ctx);
                        }
                    } else if ui.add_enabled(!in_call, egui::Button::new("Ficar offline")).clicked()
                        && let Some(r) = &self.running
                    {
                        r.stop.store(true, Ordering::Relaxed);
                    }
                    if !self.error.is_empty() {
                        ui.colored_label(egui::Color32::LIGHT_RED, &self.error);
                    }
                });

                let testing = self.shared.mic_test.load(Ordering::Relaxed);
                if in_call && !testing {
                    card(ui, "Em chamada", |ui| {
                        ui.label("Conversando agora.");
                        let link = self.shared.link.lock().unwrap().clone();
                        if !link.is_empty() {
                            ui.label(format!("Linha: {link}"));
                        }
                        if ui.add_sized([160.0, 30.0], egui::Button::new("Desligar")).clicked() {
                            self.shared.hangup.store(true, Ordering::Relaxed);
                        }
                        self.screen_ui(ui);
                    });
                }

                if !offline && self.form.use_audio {
                    card(ui, "Áudio", |ui| self.audio_ui(ui, in_call, testing));
                }

                card(ui, "Meu código de contato", |ui| {
                    ui.weak("Envie este código a quem vai ligar para você.");
                    let relay = self.shared.relay.lock().unwrap().clone();
                    match self.form.my_code(relay.as_ref()) {
                        Some(code) => {
                            ui.horizontal(|ui| {
                                ui.monospace(if code.len() > 36 { format!("{}...", &code[..36]) } else { code.clone() });
                                if ui.button("Copiar").clicked() {
                                    ui.ctx().copy_text(code);
                                }
                            });
                        }
                        None => {
                            ui.weak("(identidade inválida: veja Configurações)");
                        }
                    }
                });

                card(ui, "Contatos", |ui| self.contacts_ui(ui, !offline, in_call));

                card(ui, "Ligar para um código", |ui| {
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !offline && !in_call,
                            egui::TextEdit::singleline(&mut self.form.peer).hint_text("cole o código de contato ou o nome de um contato").desired_width(380.0),
                        );
                        if ui.add_enabled(!offline && !in_call, egui::Button::new("📞 Ligar")).clicked() {
                            match self.form.target() {
                                Ok(k) => {
                                    self.error.clear();
                                    self.form.save();
                                    if let Some(r) = &self.running {
                                        let _ = r.cmds.send(Cmd::Dial(k, self.form.relay_for(&k)));
                                    }
                                }
                                Err(e) => self.error = e,
                            }
                        }
                    });
                    if offline {
                        ui.weak("Fique online primeiro.");
                    }
                });

                egui::CollapsingHeader::new("Configurações avançadas").show(ui, |ui| {
                    ui.add_enabled_ui(offline, |ui| {
                        ui.label("Identidade (chave secreta, 64 hex). Não compartilhe com ninguém.");
                        ui.add(egui::TextEdit::singleline(&mut self.form.secret).password(true).desired_width(f32::INFINITY));
                        if ui.button("Gerar nova identidade").on_hover_text("O seu código de contato muda e os outros precisarão do novo.").clicked() {
                            self.form.secret = hex(&StaticSecret::random_from_rng(OsRng).to_bytes());
                        }
                        ui.separator();
                        ui.label("Endereço público ip:porta (só se o IP que os outros enxergam não serve, como atrás de NAT; a porta precisa estar aberta no roteador; pode ser um nome):");
                        ui.text_edit_singleline(&mut self.form.public_addr);
                        ui.separator();
                        // A DHT vem ligada: a caixa é para desligá-la.
                        let mut no_dht = !self.form.use_dht;
                        let off = ui
                            .checkbox(&mut no_dht, "Não usar a DHT (BitTorrent) para reencontrar contatos")
                            .on_hover_text("Quando você e um contato mudam de endereço ao mesmo tempo, os dois se acham por ela, sem servidor. O conteúdo vai cifrado só para o contato, mas quem opera nós da DHT vê o seu IP. Com a DHT ligada, seu nó também atende a DHT para circuitos de outros nós.");
                        if off.changed() {
                            self.form.use_dht = !no_dht;
                        }
                        ui.add_enabled_ui(self.form.use_dht, |ui| {
                            ui.checkbox(&mut self.form.dht_circuit, "Falar com a DHT pelos relays dos contatos (esconde o seu IP)")
                                .on_hover_text("Usa um circuito de dois relays de contatos: o primeiro vê o seu IP mas não o pedido, o segundo vê o pedido mas não o seu IP. Precisa de ao menos dois contatos com endereço guardado; sem eles a DHT não é usada.");
                        });
                        ui.separator();
                        ui.label("Seu nome (aparece na lista de contatos de quem você adiciona):");
                        ui.add(egui::TextEdit::singleline(&mut self.form.my_name).char_limit(clam::media::update::MAX_NAME).desired_width(240.0));
                        ui.checkbox(&mut self.form.intros, "Aceitar quem me adiciona como contato")
                            .on_hover_text("Quando alguém adiciona o seu código, ele manda uma apresentação cifrada e passa a aparecer na sua lista, para os dois se ligarem. Desligado, só entra na lista quem você adicionar. Só aceita de quem já tem o seu endereço; no máximo 20 por minuto.");
                        ui.separator();
                        // Servir de âncora vem ligado: a caixa é para não servir.
                        let mut no_anchor = !self.form.anchor;
                        let off = ui
                            .checkbox(&mut no_anchor, "Não servir de âncora para os registros de endereço dos outros")
                            .on_hover_text("Guarda, em memória, o endereço cifrado de quem usa este computador como âncora, para os contatos dele o acharem. Não lê o conteúdo nem forja item. Precisa de um endereço que os outros alcancem; quem a usa vê o IP do relay dela.");
                        if off.changed() {
                            self.form.anchor = !no_anchor;
                        }
                        ui.label("Âncoras que você usa (uma por linha: ip:porta#chave-do-circuito#chave-do-link):");
                        ui.add(egui::TextEdit::multiline(&mut self.form.anchors).desired_rows(2).desired_width(f32::INFINITY));
                    });
                    if self.form.anchor
                        && let Some(r) = self.shared.relay.lock().unwrap().clone()
                    {
                        ui.horizontal(|ui| {
                            ui.label("Código da sua âncora:");
                            ui.monospace(format!("{}...", &anchor_code(&r)[..24]));
                            if ui.button("Copiar").clicked() {
                                ui.ctx().copy_text(anchor_code(&r));
                            }
                        });
                    }
                });

                egui::CollapsingHeader::new("Registro de atividade").default_open(true).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).show(ui, |ui| {
                        for l in self.log.lock().unwrap().iter() {
                            ui.monospace(l);
                        }
                    });
                });
            });
        });
        // Com tela chegando a janela repinta a cada ~16 ms; a 300 ms o vídeo parecia ter 3 quadros/s.
        ctx.request_repaint_after(Duration::from_millis(if watching { 16 } else { 300 }));
        let took = began.elapsed();
        if took > Duration::from_millis(100) {
            self.note_slow(format!("janela: um quadro levou {} ms para desenhar", took.as_millis()));
        }
        LAST_FRAME_END.with(|l| l.set(Some(std::time::Instant::now())));
    }
}
