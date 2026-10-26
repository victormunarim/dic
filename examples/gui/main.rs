//! Janela mínima para ligar e atender sem usar o terminal.
//! Código em `examples/gui/`: `conf` (formulário e arquivo), `engine` (nó e chamadas, sem janela), `ui` (egui).
//! Roteiro em `docs/CHAMADAS.md`, seção "Interface gráfica mínima".
//!
//! ```text
//! cargo run --release --features gui,audio --example gui
//! ```
//! Você informa a identidade e os contatos; "Ficar online" sobe relay e telefone; "Ligar" usa o código de
//! contato do outro (a chave e, na primeira vez, o relay dele). Chamadas de contatos da lista são atendidas
//! sozinhas. Sem `--features audio` (ou sem a opção marcada),
//! cada lado envia um tom (440 Hz quem liga, 660 Hz quem atende).
//! Os campos são gravados em `~/.clam-gui.conf`; a chave secreta vai para o chaveiro do sistema
//! (ou, se não houver um, para o arquivo, que só o dono lê).

mod conf;
mod engine;
mod servers;
mod theme;
mod ui;

use std::sync::Arc;

use eframe::egui;

use conf::Form;
use ui::{App, Tab};

fn main() -> eframe::Result {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]).with_min_inner_size([820.0, 560.0]).with_title("Clam"),
        ..Default::default()
    };
    eframe::run_native(
        "Clam",
        opts,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            let form = Form::load();
            let shared = Arc::<engine::Shared>::default();
            shared.audio.set_prefs(form.audio);
            Ok(Box::new(App {
                rt,
                form,
                log: Arc::default(),
                shared,
                running: None,
                done: Arc::default(),
                error: String::new(),
                tab: Tab::Home,
                was_in_call: false,
                audio_dirty: false,
                screen_tex: Default::default(),
                zoom: None,
                srv: Default::default(),
                #[cfg(feature = "audio")]
                devs: clam::audio::devices(),
                #[cfg(not(feature = "audio"))]
                devs: Default::default(),
            }))
        }),
    )
}

