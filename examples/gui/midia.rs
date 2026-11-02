//! Mídia do bate-papo: a imagem aparece em miniatura e abre em tela cheia, o vídeo vira um quadro com play que abre o
//! player do sistema, e todo arquivo entregue tem um ícone: baixar (o recebido vai para a pasta de baixados, sem
//! perguntar) ou, já baixado ou enviado por mim, mostrar na pasta. A decodificação roda fora da janela e só guarda uma
//! miniatura.

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

/// Mostra o arquivo no gerenciador de arquivos do sistema (selecionado, onde o sistema sabe fazê-lo; senão, a pasta
/// dele). Só por clique.
pub(crate) fn reveal(path: &Path) {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(format!("/select,{}", path.display()));
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg("-R").arg(path);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        c
    };
    if let Ok(mut child) = cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Quem manda na transferência: (contato, número da oferta). É a chave do arquivo recebido.
pub(crate) type Key = ([u8; 32], u64);

fn keep_id() -> egui::Id {
    egui::Id::new("clam-midia-baixar")
}

/// Pede para baixar o arquivo recebido `key`: a janela (`take_keeps`) leva o arquivo da pasta que se apaga para a
/// pasta de baixados, sem perguntar nada.
fn request_keep(ctx: &egui::Context, key: Key) {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Vec<Key>>(keep_id()).push(key));
    ctx.request_repaint();
}

/// Os pedidos de baixar feitos desde o último quadro.
pub(crate) fn take_keeps(ctx: &egui::Context) -> Vec<Key> {
    ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<Vec<Key>>(keep_id())))
}

/// O que o ícone do arquivo faz: recebido e ainda temporário, baixa; qualquer outro (baixado, ou enviado por mim) abre
/// a pasta dele no gerenciador de arquivos.
fn act(ctx: &egui::Context, key: Key, path: &Path) {
    if crate::arquivos::is_temporary(&crate::conf::received_dir(), path) {
        request_keep(ctx, key);
    } else {
        reveal(path);
    }
}

/// Função que desenha um ícone (`theme`).
type Icon = fn(&egui::Painter, Pos2, f32, egui::Color32);

/// Dica e ícone do botão do arquivo (ver `act`). A dica avisa no lugar que o recebido some ao fechar se não for baixado.
fn action_look(path: &Path) -> (&'static str, Icon) {
    if crate::arquivos::is_temporary(&crate::conf::received_dir(), path) {
        ("Baixar: guardar o arquivo na pasta de baixados. Sem baixar, ele é apagado quando o programa fecha", download_icon)
    } else {
        ("Mostrar no gerenciador de arquivos", folder_icon)
    }
}

fn full_id() -> egui::Id {
    egui::Id::new("clam-midia-cheia")
}

/// Abre a imagem em tela cheia (`overlay` a desenha).
fn open_full(ctx: &egui::Context, name: &str, path: &Path, key: Key) {
    ctx.data_mut(|d| d.insert_temp(full_id(), Some((name.to_string(), path.to_path_buf(), key))));
}

/// Mostra o que o arquivo entregue pede: imagem à vista, vídeo com play ou, para os demais, um cartão com o nome. Todos
/// têm o ícone de baixar ou de mostrar na pasta (`act`). Nada se desenha enquanto o arquivo não está no disco.
pub(crate) fn show(ui: &mut egui::Ui, x: &crate::arquivos::Xfer) {
    let Some(path) = x.media_path() else { return };
    let key = (x.peer, x.id);
    match kind(&x.name) {
        Some(Kind::Image) => image_block(ui, &x.name, path, key),
        Some(Kind::Video) => video_block(ui, &x.name, path, key),
        None => file_card(ui, &x.name, x.size, path, key),
    }
}

fn image_block(ui: &mut egui::Ui, name: &str, path: &Path, key: Key) {
    match &*slot(ui.ctx(), path, THUMB_PX) {
        Slot::Ready(tex) => {
            let s = tex.size_vec2();
            let size = s * (SHOWN / s.x.max(s.y).max(1.0)).min(1.0);
            let reply = ui.add(egui::Image::new(tex).fit_to_exact_size(size).corner_radius(12).sense(Sense::click()));
            reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Ver {name} em tela cheia")));
            let (rect, ctx) = (reply.rect, ui.ctx().clone());
            if reply.on_hover_text("Ver em tela cheia").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open_full(&ctx, name, path, key);
            }
            let (tip, icon) = action_look(path);
            if media_corner_button(ui, rect, 0, tip, icon).clicked() {
                act(&ctx, key, path);
            }
        }
        Slot::Loading => {
            ui.add(egui::Spinner::new().size(18.0).color(BLURPLE));
        }
        Slot::Failed => file_card(ui, name, 0, path, key),
    }
}

/// Quadro afundado com o triângulo de play desenhado (a fonte pode não ter o glifo).
fn video_block(ui: &mut egui::Ui, name: &str, path: &Path, key: Key) {
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
    let (tip, icon) = action_look(path);
    if media_corner_button(ui, rect, 0, tip, icon).clicked() {
        act(&ctx, key, path);
    }
}

/// Arquivo que não é mídia (ou imagem que não abriu): nome, tamanho e o ícone de baixar ou de mostrar na pasta.
fn file_card(ui: &mut egui::Ui, name: &str, size: u64, path: &Path, key: Key) {
    let ctx = ui.ctx().clone();
    sunken_frame(ui, 12, 10, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(name).color(pal().text));
            if size > 0 {
                ui.label(egui::RichText::new(size_text(size)).size(12.0).color(pal().muted));
            }
            let (rect, reply) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
            let ink = if reply.hovered() { pal().strong } else { pal().text };
            let (tip, icon) = action_look(path);
            icon(ui.painter(), rect.center(), 18.0, ink);
            reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
            if reply.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                act(&ctx, key, path);
            }
        });
    });
}

/// A imagem em tela cheia, por cima de tudo: Esc ou um clique fora dos botões fecham. Chamada uma vez por quadro.
pub(crate) fn overlay(ctx: &egui::Context) {
    let Some((_name, path, key)) = ctx.data(|d| d.get_temp::<Option<(String, PathBuf, Key)>>(full_id())).flatten() else { return };
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
        let (tip, icon) = action_look(&path);
        if media_corner_button(ui, screen, 1, tip, icon).clicked() {
            // Baixar muda o arquivo de lugar: fecha, e a conversa passa a mostrá-lo de onde ficou.
            close = crate::arquivos::is_temporary(&crate::conf::received_dir(), &path) || close;
            act(ctx, key, &path);
        }
        if back.clicked() {
            close = true;
        }
    });
    if close {
        ctx.data_mut(|d| d.insert_temp(full_id(), None::<(String, PathBuf, Key)>));
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
