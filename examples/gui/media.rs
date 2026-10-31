//! Abas Vídeo e Áudio: preferências da transmissão de tela, dispositivos e tratamento da voz.

use std::sync::atomic::Ordering;

use eframe::egui::{self, CornerRadius};

use crate::{conf::*, engine::*, theme::*, ui::*};

impl App {
    /// Preferências da transmissão de tela: valem da próxima transmissão em diante, na chamada e nas salas.
    pub(crate) fn video_tab(&mut self, ui: &mut egui::Ui) {
        let st = &self.shared.screen;
        let busy = st.want_share.load(Ordering::Relaxed) || st.sharing.load(Ordering::Relaxed);
        card(ui, "Transmissão de tela", "Valem a partir da próxima transmissão e ficam salvas.", |ui| {
            if busy {
                note(ui, "Há uma transmissão em andamento: pare de compartilhar para mudar.");
            }
            ui.add_enabled_ui(!busy, |ui| {
                let changed = ui
                    .check(&mut self.form.screen_private, "Privacidade na transmissão da tela")
                    .on_hover_text("Ligada, a tela passa por um relay no meio do caminho e sai num ritmo constante, com tráfego de enchimento: quem observa a rede não vê quando a imagem se mexe. Desligada, só sai o que há para enviar; o ritmo máximo dobra e a imagem fica mais nítida e fluida, mas quem observa a rede vê quando a imagem se mexe. Para os contatos de confiança (aba Contatos) a tela ainda vai direto ao relay deles, sem relay no meio, e eles veem o seu IP; para os demais segue por 2 relays, que escondem o seu IP. O vídeo segue cifrado nos dois casos. O contato de confiança precisa de uma versão que aceite a tela direta. Vale na próxima transmissão.")
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
                    .check(&mut self.form.screen_hardware, "Codificar a tela pela placa de vídeo (experimental)")
                    .on_hover_text("Usa a GPU (VA-API, NVENC ou Quick Sync, via GStreamer) em vez do processador. Precisa de gstreamer1.0-tools e gstreamer1.0-plugins-bad com o driver da GPU; sem isso, ou se ela parar, o programa volta sozinho ao openh264. Menos CPU, mas sem o controle fino de nitidez por quadro, e a imagem pode demorar um pouco mais a reagir. Não funciona no Windows. Vale na próxima transmissão.")
                    .changed()
                {
                    self.form.save();
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------------------------------
// Áudio
// ---------------------------------------------------------------------------------------------------

impl App {
    /// Dispositivos, mudo, volume, sensibilidade e tratamento da voz, medidor do microfone e teste do microfone.
    pub(crate) fn audio_tab(&mut self, ui: &mut egui::Ui, offline: bool, in_call: bool, testing: bool) {
        let shared = self.shared.clone();
        let a = &shared.audio;
        let mut muted = a.muted.load(Ordering::Relaxed);
        let before = a.prefs();
        let mut p = before;
        let level = a.level();
        let speaking = a.speaking() && !muted;

        split(ui, |ui, side| {
            if side == 0 {
                card(ui, "Dispositivos", "", |ui| {
                    ui.add_enabled(offline, egui::Checkbox::new(&mut self.form.use_audio, "Usar microfone e alto-falante"))
                        .on_hover_text("Desmarcado, cada lado manda só um tom de teste. Vale ao ficar online.");
                    #[cfg(feature = "audio")]
                    devices_ui(ui, &mut self.form, &mut self.devs);
                });
                card(ui, "Volume e microfone", "", |ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(120.0);
                    if ui.check(&mut muted, "Mudo (o outro lado não ouve você)").changed() {
                        a.muted.store(muted, Ordering::Relaxed);
                    }
                    ui.label("Volume do alto-falante");
                    ui.add(egui::Slider::new(&mut p.volume, AudioPrefs::VOLUME));
                    ui.label("Ganho do microfone");
                    ui.add(egui::Slider::new(&mut p.mic_gain, AudioPrefs::MIC_GAIN));
                    ui.label("Nível do microfone");
                    // Medidor: a linha marca o limiar de sensibilidade; o texto diz se a voz passa dele.
                    let meter = ui.add(
                        egui::ProgressBar::new(level)
                            .desired_width(ui.available_width())
                            .desired_height(26.0)
                            .corner_radius(CornerRadius::same(6))
                            .fill(if speaking { pal().green } else { BLURPLE })
                            .text(if speaking { "voz" } else { "" }),
                    );
                    let x = meter.rect.left() + meter.rect.width() * voice_meter(p.gate_db);
                    ui.painter().vline(x, meter.rect.y_range(), egui::Stroke::new(2.5_f32, pal().strong));
                    ui.label("Sensibilidade (limiar da voz)");
                    ui.add(egui::Slider::new(&mut p.gate_db, AudioPrefs::GATE_DB).suffix(" dB"))
                        .on_hover_text("A linha laranja no medidor. Quanto mais para a esquerda, mais sensível: sons baixos já contam como voz.");
                    if in_call || testing {
                        note(ui, "Fale e veja o medidor: a linha laranja deve ficar abaixo da sua voz e acima do ruído.");
                    }
                    if testing {
                        if button(ui, true, "Parar o teste", pal().red).clicked() {
                            self.shared.mic_test.store(false, Ordering::Relaxed);
                        }
                    } else if button(ui, !in_call && !offline, "Testar o microfone", BLURPLE).clicked()
                        && let Some(r) = &self.running
                    {
                        let _ = r.cmds.send(Cmd::MicTest);
                    }
                    if offline {
                        note(ui, "Fique online para testar o microfone.");
                    }
                });
            } else {
                card(ui, "Tratamento da voz", "O que o programa faz com o som antes de mandá-lo.", |ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(120.0);
                    ui.check(&mut p.denoise, "Remover ruído (rede neural)")
                        .on_hover_text("Tira teclado, ventilador, estática e vozes ao fundo (RNNoise). Com ela ligada, a porta e o indicador \"voz\" só contam o que a rede reconhece como fala. Adiciona ~10 ms de atraso.");
                    ui.check(&mut p.echo_cancel, "Cancelar eco do alto-falante")
                        .on_hover_text("Tira do microfone o que saiu pelo alto-falante, para o outro lado não se ouvir de volta. Leva alguns segundos de conversa para aprender o ambiente. Com fones não é preciso.");
                    ui.check(&mut p.echo_suppress, "Abaixar o microfone enquanto só há eco")
                        .on_hover_text("Reforço para o eco que o cancelador deixa passar: com o outro lado falando, o microfone cai 18 dB enquanto estiver mais de 20 dB abaixo do que sai pelo alto-falante. Quem fala baixinho por cima do outro pode ser cortado; com fones, desligue.");
                    if let Some(erle) = a.echo_erle().filter(|_| p.echo_cancel && (in_call || testing)) {
                        note(ui, format!("Eco cancelado: {erle:.0} dB"));
                    }
                    ui.check(&mut p.gate, "Silenciar o que não é voz")
                        .on_hover_text("Quando o microfone fica abaixo do limiar, o som enviado é reduzido. Ajuste a sensibilidade até a linha ficar logo acima do ruído.");
                    ui.add_enabled_ui(p.gate, |ui| {
                        ui.label("Redução do ruído");
                        ui.add(egui::Slider::new(&mut p.gate_depth_db, AudioPrefs::GATE_DEPTH_DB).suffix(" dB"));
                    });
                    ui.check(&mut p.high_pass, "Cortar graves (ventilador, mesa, sopro)");
                    ui.check(&mut p.agc, "Volume automático (nivela voz baixa e alta)")
                        .on_hover_text("Reforça quem fala baixo e contém quem fala alto. Não reforça o ruído de fundo.");
                    ui.add_enabled_ui(!in_call, |ui| {
                        let mut opus = self.form.voice_opus;
                        if ui
                            .check(&mut opus, "Voz em Opus (metade da banda do G.722)")
                            .on_hover_text("Vale para as chamadas que você inicia; ao atender, vale o que o outro escolheu. O Opus usa 32 kbps contra 64 kbps do G.722 e esconde melhor a perda de pacotes, mas só funciona se o outro lado também for desta versão; contra um telefone antigo, desmarque. Vale na próxima ligação.")
                            .changed()
                        {
                            self.form.voice_opus = opus;
                            self.form.save();
                        }
                    });
                    self.shared.opus.store(self.form.voice_opus, Ordering::Relaxed);
                    if button(ui, true, "Restaurar padrões do áudio", pal().bg).clicked() {
                        p = AudioPrefs::default();
                    }
                });
            }
        });

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
    }
}
