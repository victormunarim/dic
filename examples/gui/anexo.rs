//! Anexos em espera: o clipe e o arquivo solto na janela só escolhem o arquivo; ele sai quando a pessoa clica em Enviar
//! (ou Enter). Até lá aparece como uma etiqueta acima do campo, com X para desistir. O estado mora na memória do egui
//! (por conversa), então o diálogo de arquivos, que roda em outra thread, pode entregar o caminho sem passar pelo `App`.

use std::{hash::Hash, path::PathBuf};

use eframe::egui::{self, RichText};

use crate::{arquivos::size_text, theme::*};

type Pending = Vec<(PathBuf, u64)>;

fn id(key: impl Hash) -> egui::Id {
    egui::Id::new(("anexos-em-espera", key))
}

/// Põe o arquivo na espera. Ignora o que não é arquivo legível e o que já está na lista.
pub(crate) fn stage(ctx: &egui::Context, key: impl Hash, path: PathBuf) {
    let Some(size) = std::fs::metadata(&path).ok().filter(std::fs::Metadata::is_file).map(|m| m.len()) else { return };
    ctx.data_mut(|d| {
        let list = d.get_temp_mut_or_default::<Pending>(id(key));
        if !list.iter().any(|(p, _)| *p == path) {
            list.push((path, size));
        }
    });
    ctx.request_repaint();
}

/// Quantos arquivos esperam.
pub(crate) fn count(ctx: &egui::Context, key: impl Hash) -> usize {
    ctx.data(|d| d.get_temp::<Pending>(id(key)).map_or(0, |l| l.len()))
}

/// Retira os arquivos da espera, para enviar.
pub(crate) fn take(ctx: &egui::Context, key: impl Hash) -> Vec<PathBuf> {
    ctx.data_mut(|d| d.remove_temp::<Pending>(id(key))).unwrap_or_default().into_iter().map(|(p, _)| p).collect()
}

/// As etiquetas dos arquivos em espera, cada uma com o X que a tira da lista.
pub(crate) fn chips(ui: &mut egui::Ui, key: impl Hash + Copy) {
    let list = ui.ctx().data(|d| d.get_temp::<Pending>(id(key))).unwrap_or_default();
    if list.is_empty() {
        return;
    }
    let mut drop_at = None;
    ui.horizontal_wrapped(|ui| {
        for (i, (path, size)) in list.iter().enumerate() {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            sunken_frame(ui, 10, 4, |ui| {
                ui.label(RichText::new(name).color(pal().text));
                ui.label(RichText::new(size_text(*size)).size(12.0).color(pal().muted));
                if close_button(ui, "Tirar o anexo").clicked() {
                    drop_at = Some(i);
                }
            });
        }
    });
    if let Some(i) = drop_at {
        ui.ctx().data_mut(|d| {
            let l = d.get_temp_mut_or_default::<Pending>(id(key));
            if i < l.len() {
                l.remove(i);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_wait_until_taken_and_ignore_repeats_and_non_files() {
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!("clam-anexo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"oi").unwrap();
        stage(&ctx, "k", file.clone());
        stage(&ctx, "k", file.clone());
        stage(&ctx, "k", dir.clone());
        stage(&ctx, "k", dir.join("nao-existe"));
        assert_eq!(count(&ctx, "k"), 1);
        assert_eq!(count(&ctx, "outra"), 0, "cada conversa tem a sua espera");
        assert_eq!(take(&ctx, "k"), vec![file]);
        assert_eq!(count(&ctx, "k"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
