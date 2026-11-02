//! Janela mínima para ligar e atender sem usar o terminal.
//! Código em `examples/gui/`: `conf` (formulário e arquivo), `engine` (nó e chamadas, sem janela), `ui` (egui).
//! Roteiro em `docs/CHAMADAS.md`, seção "Interface gráfica mínima".
//!
//! ```text
//! cargo run --release --features gui,audio --example gui
//! ```
//! Você informa a identidade e os contatos; o programa tenta ficar online ao abrir (dá para desligar na aba Configurações) e "Ficar online" sobe relay e telefone; "Ligar" usa o código de
//! contato do outro (a chave e, na primeira vez, o relay dele). Chamadas de contatos da lista são atendidas
//! sozinhas. Sem `--features audio` (ou sem a opção marcada),
//! cada lado envia um tom (440 Hz quem liga, 660 Hz quem atende).
//! A aba Servidores (`servers`) cria e entra em servidores e salas de voz (`docs/SERVIDORES.md`); eles ficam em
//! `~/.clam-gui.servers`.
//! Os campos são gravados em `~/.clam-gui.conf`; a chave secreta vai para o chaveiro do sistema
//! (ou, se não houver um, para o arquivo, que só o dono lê).

mod conf;
mod engine;
mod call;
mod arquivos;
mod atalhos;
mod chats;
mod anexo;
mod conversa;
mod midia;
mod contacts;
mod media;
mod settings;
mod servers;
#[cfg(test)]
mod snapshot;
mod theme;
mod ui;

use std::sync::Arc;

use eframe::egui;

use conf::Form;
use ui::{App, Tab};

fn main() -> eframe::Result {
    // Fechamento brusco (queda de energia, kill, pane) não deixa arquivo recebido para trás: o que não foi baixado
    // some na abertura seguinte, antes de qualquer transferência nova.
    arquivos::sweep(&conf::received_dir());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]).with_min_inner_size([820.0, 560.0]).with_title("Clam"),
        ..Default::default()
    };
    eframe::run_native(
        "Clam",
        opts,
        Box::new(|cc| {
            let form = Form::load();
            theme::set_light(form.light.unwrap_or_else(|| cc.egui_ctx.system_theme() == Some(egui::Theme::Light)));
            theme::apply(&cc.egui_ctx);
            let shared = Arc::<engine::Shared>::default();
            shared.audio.set_prefs(form.audio);
            *clam::sync::LockExt::lock_unpoisoned(&shared.chats) = chats::ChatBook::load();
            // Os arquivos que a pessoa baixou antes voltam à conversa, no lugar onde ficaram.
            if let Ok(text) = std::fs::read_to_string(conf::kept_path()) {
                clam::sync::LockExt::lock_unpoisoned(&shared.transfers).load_kept(&text);
            }
            let mut app = App {
                rt,
                form,
                log: Arc::default(),
                shared,
                running: None,
                done: Arc::default(),
                error: String::new(),
                tab: Tab::Contacts,
                audio_dirty: false,
                screen_tex: Default::default(),
        screen_rate: Default::default(),
                zoom: None,
                chat_with: None,
                chat_draft: String::new(),
                global_keys: crate::atalhos::Global::enabled(),
                srv: Default::default(),
                #[cfg(feature = "audio")]
                devs: clam::audio::devices(),
                #[cfg(not(feature = "audio"))]
                devs: Default::default(),
            };
            if app.form.auto_online {
                app.go_online(&cc.egui_ctx);
            }
            Ok(Box::new(app))
        }),
    )
}

