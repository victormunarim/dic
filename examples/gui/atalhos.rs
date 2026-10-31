//! Atalhos de teclado que a pessoa configura (mudo e fone): texto na configuração, leitura, captura na janela.

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

/// Mudo do microfone.
pub(crate) const MUTE_DEFAULT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::M);
/// Fone: microfone e áudio que se escuta, juntos.
pub(crate) const DEAFEN_DEFAULT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::D);

/// Texto gravado na configuração: `cmd+shift+D` (`cmd` é Ctrl, ou Cmd no macOS).
pub(crate) fn to_text(s: &KeyboardShortcut) -> String {
    let m = s.modifiers;
    let mut out = String::new();
    for (on, name) in [(m.command || m.ctrl || m.mac_cmd, "cmd"), (m.alt, "alt"), (m.shift, "shift")] {
        if on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(s.logical_key.name());
    out
}

/// Lê o texto gravado. Vazio, ilegível ou que não passa em [`allowed`] volta a `None`: quem chama usa o padrão.
pub(crate) fn from_text(text: &str) -> Option<KeyboardShortcut> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    for part in text.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "cmd" => modifiers = modifiers.plus(Modifiers::COMMAND),
            "alt" => modifiers = modifiers.plus(Modifiers::ALT),
            "shift" => modifiers = modifiers.plus(Modifiers::SHIFT),
            _ => key = Key::from_name(part.trim()),
        }
    }
    let s = KeyboardShortcut::new(modifiers, key?);
    allowed(&s).then_some(s)
}

/// O atalho vale se não rouba a digitação: precisa de Ctrl/Alt/Cmd (Shift sozinho não basta) ou ser uma tecla F, e
/// Ctrl+C, V, X, A, Z, Y ficam com a edição de texto.
pub(crate) fn allowed(s: &KeyboardShortcut) -> bool {
    let m = s.modifiers;
    let f_key = matches!(s.logical_key, Key::F1 | Key::F2 | Key::F3 | Key::F4 | Key::F5 | Key::F6 | Key::F7 | Key::F8 | Key::F9 | Key::F10 | Key::F11 | Key::F12);
    let held = m.command || m.ctrl || m.mac_cmd || m.alt;
    let editing = matches!(s.logical_key, Key::C | Key::V | Key::X | Key::A | Key::Z | Key::Y) && !m.alt;
    (held || f_key) && !(editing && held) && s.logical_key != Key::Escape
}

/// Atalho da configuração, ou o padrão se o texto está vazio ou inválido.
pub(crate) fn resolve(text: &str, default: KeyboardShortcut) -> KeyboardShortcut {
    from_text(text).unwrap_or(default)
}

/// Combinação que os atalhos globais entendem, ou `None` (tecla de seta, Enter etc.: só valem na janela).
pub(crate) fn to_combo(s: &KeyboardShortcut) -> Option<clam::hotkey::Combo> {
    let m = s.modifiers;
    Some(clam::hotkey::Combo { ctrl: m.command || m.ctrl, alt: m.alt, shift: m.shift, key: clam::hotkey::Key::from_name(s.logical_key.name())? })
}

/// Atalhos que valem com a janela em segundo plano. Só liga com `enabled` (o `main`; os testes e as capturas não
/// tomam teclas do sistema) e refaz o registro quando as teclas mudam.
#[derive(Default)]
pub(crate) struct Global {
    pub(crate) enabled: bool,
    registered: Vec<clam::hotkey::Combo>,
    handle: Option<clam::hotkey::Handle>,
}

/// Quem os atalhos acionam.
const MUTE_ID: u32 = 1;
const DEAFEN_ID: u32 = 2;

impl Global {
    /// Atalhos globais ligados (o programa de verdade).
    pub(crate) fn enabled() -> Self {
        Global { enabled: true, ..Global::default() }
    }

    /// Garante que o sistema tem as teclas atuais. Barato: só compara com o que já está registrado.
    pub(crate) fn sync(&mut self, ctx: &egui::Context, shared: &std::sync::Arc<crate::engine::Shared>, mute: &KeyboardShortcut, deafen: &KeyboardShortcut) {
        if !self.enabled {
            return;
        }
        let want: Vec<_> = [mute, deafen].into_iter().filter_map(to_combo).collect();
        if want.len() == 2 && self.handle.is_some() && self.registered == want {
            return;
        }
        self.handle = None;
        self.registered = want.clone();
        if want.len() != 2 {
            return; // uma das teclas não existe fora da janela: vale só nela
        }
        let (shared, ctx) = (shared.clone(), ctx.clone());
        let bindings = vec![
            clam::hotkey::Binding { id: MUTE_ID, combo: want[0], label: "Silenciar o microfone".into() },
            clam::hotkey::Binding { id: DEAFEN_ID, combo: want[1], label: "Desligar microfone e som".into() },
        ];
        self.handle = Some(clam::hotkey::register(
            bindings,
            std::sync::Arc::new(move |id| {
                match id {
                    MUTE_ID => shared.audio.toggle_muted(),
                    DEAFEN_ID => shared.audio.toggle_deafened(),
                    _ => return,
                }
                ctx.request_repaint();
            }),
        ));
    }

    /// O sistema já entrega as teclas: a janela não as trata (senão uma tecla mudaria o estado duas vezes).
    pub(crate) fn active(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.status.active())
    }

    /// Por que não valem em segundo plano, para a dica; vazio se valem.
    pub(crate) fn note(&self) -> String {
        match &self.handle {
            Some(h) => h.status.note(),
            None if self.enabled => "Esta tecla só existe na janela (use uma letra, algarismo ou tecla F).".into(),
            None => String::new(),
        }
    }
}

fn capturing_id() -> egui::Id {
    egui::Id::new("atalho_capturando")
}

/// Há um campo esperando a tecla: os atalhos da janela se calam, para a tecla escolhida não disparar nada.
pub(crate) fn capturing(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<egui::Id>(capturing_id())).is_some()
}

/// Lê a próxima combinação digitada, se for válida. Esc cancela (devolve `Some(None)`).
fn read_press(ctx: &egui::Context) -> Option<Option<KeyboardShortcut>> {
    ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Key { key: Key::Escape, pressed: true, .. } => Some(None),
            egui::Event::Key { key, pressed: true, modifiers, repeat: false, .. } => {
                let s = KeyboardShortcut::new(*modifiers, *key);
                allowed(&s).then_some(Some(s))
            }
            _ => None,
        })
    })
}

/// Botão que mostra o atalho e, clicado, espera a próxima combinação. Devolve o novo atalho escolhido.
pub(crate) fn field(ui: &mut egui::Ui, id: &str, current: &KeyboardShortcut, taken: &[KeyboardShortcut]) -> Option<KeyboardShortcut> {
    let me = egui::Id::new(("atalho", id));
    let waiting = ui.ctx().data(|d| d.get_temp::<egui::Id>(capturing_id())) == Some(me);
    let label = if waiting { "Digite a combinação… (Esc cancela)".to_string() } else { ui.ctx().format_shortcut(current) };
    let tip = "Clique e digite a nova combinação. Precisa de Ctrl, Alt ou Cmd (ou uma tecla F). Vale com a janela em primeiro plano.";
    if crate::theme::button(ui, true, &label, crate::theme::pal().bg).on_hover_text(tip).clicked() {
        ui.ctx().data_mut(|d| d.insert_temp(capturing_id(), me));
        return None;
    }
    if !waiting {
        return None;
    }
    match read_press(ui.ctx()) {
        None => None,
        Some(pick) => {
            ui.ctx().data_mut(|d| d.remove::<egui::Id>(capturing_id()));
            pick.filter(|s| !taken.iter().any(|t| t.logical_key == s.logical_key && t.modifiers.matches_logically(s.modifiers)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_and_bad_text_falls_back() {
        for s in [MUTE_DEFAULT, DEAFEN_DEFAULT, KeyboardShortcut::new(Modifiers::ALT, Key::F5), KeyboardShortcut::new(Modifiers::NONE, Key::F9)] {
            assert_eq!(from_text(&to_text(&s)), Some(s), "{s:?}");
        }
        assert_eq!(resolve("", MUTE_DEFAULT), MUTE_DEFAULT);
        assert_eq!(resolve("lixo+++", DEAFEN_DEFAULT), DEAFEN_DEFAULT);
        assert_eq!(resolve("cmd+C", MUTE_DEFAULT), MUTE_DEFAULT, "Ctrl+C é da edição");
        assert_eq!(resolve("shift+K", MUTE_DEFAULT), MUTE_DEFAULT, "Shift sozinho rouba a digitação");
    }

    #[test]
    fn defaults_are_valid_and_differ() {
        assert!(allowed(&MUTE_DEFAULT) && allowed(&DEAFEN_DEFAULT));
        assert_ne!(MUTE_DEFAULT, DEAFEN_DEFAULT);
    }
}
