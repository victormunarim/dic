//! Janela mínima para ligar e atender sem usar o terminal.
//! Código em `examples/gui/`: `conf` (formulário e arquivo), `engine` (nó, chamadas e salas, sem janela), `ui` (egui).
//! Roteiro no README, seção "Interface gráfica mínima".
//!
//! ```text
//! cargo run --release --features gui,audio --example gui
//! ```
//! Você informa a identidade, o endereço do diretório e os contatos; "Ficar online" sobe relay e
//! telefone e publica no diretório; "Ligar" só precisa da chave pública do outro. Chamadas de
//! contatos da lista são atendidas sozinhas. Sem `--features audio` (ou sem a opção marcada),
//! cada lado envia um tom (440 Hz quem liga, 660 Hz quem atende).
//! Os campos são gravados em `~/.clam-gui.conf`; a chave secreta vai para o chaveiro do sistema
//! (ou, se não houver um, para o arquivo, que só o dono lê).

mod conf;
mod engine;
mod ui;

use std::sync::Arc;

use eframe::egui;

use conf::Form;
use ui::App;

fn main() -> eframe::Result {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 720.0]).with_title("Clam"),
        ..Default::default()
    };
    eframe::run_native(
        "Clam",
        opts,
        Box::new(|_| {
            Ok(Box::new(App {
                rt,
                form: Form::load(),
                log: Arc::default(),
                shared: Arc::default(),
                running: None,
                done: Arc::default(),
                error: String::new(),
                #[cfg(feature = "audio")]
                devs: clam::audio::devices(),
                #[cfg(not(feature = "audio"))]
                devs: Default::default(),
            }))
        }),
    )
}

