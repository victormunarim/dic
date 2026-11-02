//! Mídia do bate-papo: a imagem aparece em miniatura e abre em tela cheia, o vídeo vira um quadro com play que abre o
//! player do sistema, e todo arquivo entregue tem o ícone de baixar (salvar uma cópia onde a pessoa escolher). A
//! decodificação roda fora da janela e só guarda uma miniatura.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use clam::sync::LockExt;
use eframe::egui::{self, ColorImage, Pos2, Sense, Shape, TextureHandle, vec2};

use crate::{arquivos::size_text, theme::*};

/// Lado maior da miniatura guardada (px): folga para telas de alta densidade sem guardar a foto inteira.
const THUMB_PX: u32 = 480;
/// Lado maior da imagem em tela cheia (px).
const FULL_PX: u32 = 2400;
/// Lado maior da miniatura na conversa (pontos).
const SHOWN: f32 = 240.0;
/// Memória que a decodificação aceita alocar: uma imagem hostil pequena em disco não pode pedir gigabytes.
const MAX_ALLOC: u64 = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Image,
    Video,
}

/// O tipo de mídia pela extensão do nome; `None` para o que não se mostra (nunca se abre arquivo de outro tipo).
pub(crate) fn kind(name: &str) -> Option<Kind> {
    let ext = Path::new(name).extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => Some(Kind::Image),
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "m4v" => Some(Kind::Video),
        _ => None,
    }
}

enum Slot {
    Loading,
    Ready(TextureHandle),
    Failed,
}

/// Texturas já decodificadas (ou em decodificação), por caminho e tamanho. Mora na memória do egui, sem campo no `App`.
#[derive(Default)]
struct Previews {
    map: Mutex<HashMap<(PathBuf, u32), Arc<Slot>>>,
}

fn previews(ctx: &egui::Context) -> Arc<Previews> {
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with::<Arc<Previews>>(egui::Id::new("clam-previews"), Arc::default).clone())
}

/// Decodifica a imagem e devolve uma versão de no máximo `px` de lado. Erro: arquivo ilegível, formato fora da lista ou
/// grande demais.
fn load_thumb(path: &Path, px: u32) -> Result<ColorImage, String> {
    let mut reader = image::ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    // `thumbnail` também amplia; imagem pequena fica no tamanho que tem.
    let thumb = if img.width() > px || img.height() > px { img.thumbnail(px, px).to_rgba8() } else { img.to_rgba8() };
    let size = [thumb.width() as usize, thumb.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()))
}

fn slot(ctx: &egui::Context, path: &Path, px: u32) -> Arc<Slot> {
    let store = previews(ctx);
    let key = (path.to_path_buf(), px);
    let mut map = store.map.lock_unpoisoned();
    if let Some(s) = map.get(&key) {
        return s.clone();
    }
    let loading = Arc::new(Slot::Loading);
    map.insert(key.clone(), loading.clone());
    drop(map);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let done = match load_thumb(&key.0, px) {
            Ok(img) => Slot::Ready(ctx.load_texture(format!("midia:{px}:{}", key.0.display()), img, egui::TextureOptions::LINEAR)),
            Err(_) => Slot::Failed,
        };
        previews(&ctx).map.lock_unpoisoned().insert(key, Arc::new(done));
        ctx.request_repaint();
    });
    loading
}

/// Abre o arquivo no visualizador ou player padrão do sistema. Só mídia conhecida (`kind`), e só por clique.
fn open_external(path: &Path) {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(path);
    if let Ok(mut child) = cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        // Recolhe o processo para não deixar zumbi.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Baixar: pergunta onde salvar (o diálogo do sistema bloqueia, então roda fora da janela). Arquivo recebido mora na
/// pasta que se apaga ao fechar (`conf::received_dir`): baixar o **leva** para o lugar escolhido, e é isso que o livra
/// do apagamento; a janela passa a mostrá-lo de lá (`moved`). Arquivo que eu enviei é o original da pessoa, que nunca se
/// move nem se apaga: baixar faz uma cópia, como sempre.
pub(crate) fn save_as(ctx: &egui::Context, name: &str, path: &Path) {
    let (name, path, ctx) = (name.to_string(), path.to_path_buf(), ctx.clone());
    std::thread::spawn(move || {
        let temp = crate::arquivos::is_temporary(&crate::conf::received_dir(), &path);
        let title = if temp { "Guardar o arquivo" } else { "Salvar uma cópia" };
        if let Some(dest) = rfd::FileDialog::new().set_title(title).set_file_name(&name).save_file()
            && dest != path
        {
            if crate::arquivos::is_temporary(&crate::conf::received_dir(), &dest) {
                eprintln!("escolha um lugar fora de {}: o que está lá é apagado ao fechar", crate::conf::received_dir().display());
            } else if temp {
                match crate::arquivos::keep(&path, &dest) {
                    Ok(()) => ctx.data_mut(|d| d.get_temp_mut_or_default::<Moved>(moved_id()).push((path, dest))),
                    Err(e) => eprintln!("não consegui guardar {}: {e}", dest.display()),
                }
            } else if let Err(e) = std::fs::copy(&path, &dest) {
                eprintln!("não consegui salvar {}: {e}", dest.display());
            }
        }
        ctx.request_repaint();
    });
}

/// Arquivos recebidos que a pessoa já baixou nesta sessão: (onde estavam, onde ficaram). A transferência guarda o
/// caminho antigo; a janela troca pelo novo ao desenhar.
type Moved = Vec<(PathBuf, PathBuf)>;

fn moved_id() -> egui::Id {
    egui::Id::new("clam-midia-baixados")
}

/// Onde o arquivo está agora, se a pessoa o baixou (e talvez o moveu outra vez: segue a cadeia).
fn current(ctx: &egui::Context, path: &Path) -> PathBuf {
    let moved = ctx.data(|d| d.get_temp::<Moved>(moved_id())).unwrap_or_default();
    let mut at = path.to_path_buf();
    for (from, to) in &moved {
        if *from == at {
            at = to.clone();
        }
    }
    at
}

/// A dica do ícone de baixar: avisa no lugar que o arquivo recebido some ao fechar se não for baixado.
fn download_tip(path: &Path) -> &'static str {
    if crate::arquivos::is_temporary(&crate::conf::received_dir(), path) {
        "Baixar: guardar o arquivo onde você escolher. Sem baixar, ele é apagado quando o programa fecha"
    } else {
        "Baixar: salvar uma cópia onde você escolher"
    }
}

fn full_id() -> egui::Id {
    egui::Id::new("clam-midia-cheia")
}

/// Abre a imagem em tela cheia (`overlay` a desenha).
fn open_full(ctx: &egui::Context, name: &str, path: &Path) {
    ctx.data_mut(|d| d.insert_temp(full_id(), Some((name.to_string(), path.to_path_buf()))));
}

/// Mostra o que o arquivo entregue pede: imagem à vista, vídeo com play ou, para os demais, um cartão com o nome. Todos
/// têm o ícone de baixar no canto.
pub(crate) fn show(ui: &mut egui::Ui, name: &str, size: u64, path: &Path) {
    let path = &current(ui.ctx(), path);
    match kind(name) {
        Some(Kind::Image) => image_block(ui, name, path),
        Some(Kind::Video) => video_block(ui, name, path),
        None => file_card(ui, name, size, path),
    }
}

fn image_block(ui: &mut egui::Ui, name: &str, path: &Path) {
    match &*slot(ui.ctx(), path, THUMB_PX) {
        Slot::Ready(tex) => {
            let s = tex.size_vec2();
            let size = s * (SHOWN / s.x.max(s.y).max(1.0)).min(1.0);
            let reply = ui.add(egui::Image::new(tex).fit_to_exact_size(size).corner_radius(12).sense(Sense::click()));
            reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Ver {name} em tela cheia")));
            let (rect, ctx) = (reply.rect, ui.ctx().clone());
            if reply.on_hover_text("Ver em tela cheia").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open_full(&ctx, name, path);
            }
            if media_corner_button(ui, rect, 0, download_tip(path), download_icon).clicked() {
                save_as(&ctx, name, path);
            }
        }
        Slot::Loading => {
            ui.add(egui::Spinner::new().size(18.0).color(BLURPLE));
        }
        Slot::Failed => file_card(ui, name, 0, path),
    }
}

/// Quadro afundado com o triângulo de play desenhado (a fonte pode não ter o glifo).
fn video_block(ui: &mut egui::Ui, name: &str, path: &Path) {
    let (rect, reply) = ui.allocate_exact_size(vec2(SHOWN, SHOWN * 9.0 / 16.0), Sense::click());
    let p = ui.painter();
    sunken(p, rect, 12, pal().deep);
    let c = rect.center();
    let ink = if reply.hovered() { pal().strong } else { pal().text };
    p.circle_stroke(c, 24.0, egui::Stroke::new(2.0_f32, ink));
    let tri: Vec<Pos2> = [vec2(-7.0, -12.0), vec2(-7.0, 12.0), vec2(13.0, 0.0)].iter().map(|d| c + *d).collect();
    p.add(Shape::convex_polygon(tri, ink, egui::Stroke::NONE));
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Reproduzir {name}")));
    let ctx = ui.ctx().clone();
    if reply.on_hover_text("Reproduzir em tela cheia no player do sistema").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        open_external(path);
    }
    if media_corner_button(ui, rect, 0, download_tip(path), download_icon).clicked() {
        save_as(&ctx, name, path);
    }
}

/// Arquivo que não é mídia (ou imagem que não abriu): nome, tamanho e o ícone de baixar.
fn file_card(ui: &mut egui::Ui, name: &str, size: u64, path: &Path) {
    let ctx = ui.ctx().clone();
    sunken_frame(ui, 12, 10, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(name).color(pal().text));
            if size > 0 {
                ui.label(egui::RichText::new(size_text(size)).size(12.0).color(pal().muted));
            }
            let (rect, reply) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
            let ink = if reply.hovered() { pal().strong } else { pal().text };
            download_icon(ui.painter(), rect.center(), 18.0, ink);
            reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Baixar"));
            if reply.on_hover_text(download_tip(path)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                save_as(&ctx, name, path);
            }
        });
    });
}

/// A imagem em tela cheia, por cima de tudo: Esc ou um clique fora dos botões fecham. Chamada uma vez por quadro.
pub(crate) fn overlay(ctx: &egui::Context) {
    let Some((name, path)) = ctx.data(|d| d.get_temp::<Option<(String, PathBuf)>>(full_id())).flatten() else { return };
    let path = current(ctx, &path);
    let screen = ctx.screen_rect();
    let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    egui::Area::new(full_id().with("area")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        ui.set_clip_rect(screen);
        let back = ui.allocate_rect(screen, Sense::click());
        ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(235));
        match &*slot(ctx, &path, FULL_PX) {
            Slot::Ready(tex) => {
                let s = tex.size_vec2();
                let room = screen.shrink(32.0).size();
                let size = s * (room.x / s.x).min(room.y / s.y).clamp(0.01, 1.0);
                let rect = egui::Rect::from_center_size(screen.center(), size);
                ui.painter().image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
            Slot::Loading => {
                ui.put(egui::Rect::from_center_size(screen.center(), vec2(32.0, 32.0)), egui::Spinner::new().size(32.0).color(BLURPLE));
            }
            Slot::Failed => {
                ui.painter().text(screen.center(), egui::Align2::CENTER_CENTER, "Não consegui mostrar esta imagem.", egui::FontId::proportional(16.0), egui::Color32::WHITE);
            }
        }
        // Os botões ficam no canto da tela, não da imagem.
        if media_corner_button(ui, screen, 0, "Fechar", close_icon).clicked() {
            close = true;
        }
        if media_corner_button(ui, screen, 1, download_tip(&path), download_icon).clicked() {
            save_as(ctx, &name, &path);
        }
        if back.clicked() {
            close = true;
        }
    });
    if close {
        ctx.data_mut(|d| d.insert_temp(full_id(), None::<(String, PathBuf)>));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_media_is_recognized_by_extension() {
        assert_eq!(kind("foto.PNG"), Some(Kind::Image));
        assert_eq!(kind("a.b.jpeg"), Some(Kind::Image));
        assert_eq!(kind("filme.mp4"), Some(Kind::Video));
        assert_eq!(kind("script.sh"), None);
        assert_eq!(kind("programa.exe"), None);
        assert_eq!(kind("semextensao"), None);
    }

    #[test]
    fn thumbnails_shrink_big_images_and_keep_the_ratio() {
        let dir = std::env::temp_dir().join(format!("clam-midia-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("g.png");
        image::RgbaImage::from_pixel(1200, 600, image::Rgba([10, 20, 30, 255])).save(&file).unwrap();
        assert_eq!(load_thumb(&file, THUMB_PX).unwrap().size, [480, 240]);
        assert_eq!(load_thumb(&file, FULL_PX).unwrap().size, [1200, 600], "não amplia");
        std::fs::write(dir.join("ruim.png"), b"isto nao e uma imagem").unwrap();
        assert!(load_thumb(&dir.join("ruim.png"), THUMB_PX).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
