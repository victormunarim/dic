//! Miniaturas das imagens e vídeos do bate-papo: a imagem aparece na conversa, o vídeo vira um quadro com o botão de
//! play que abre o player do sistema. A decodificação roda fora da janela e só guarda uma miniatura pequena.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use clam::sync::LockExt;
use eframe::egui::{self, ColorImage, Pos2, Sense, Shape, TextureHandle, vec2};

use crate::theme::*;

/// Lado maior da miniatura guardada (px): folga para telas de alta densidade sem guardar a foto inteira.
const THUMB_PX: u32 = 480;
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

/// Miniaturas já decodificadas (ou em decodificação), por caminho. Mora na memória do egui, sem campo no `App`.
#[derive(Default)]
struct Previews {
    map: Mutex<HashMap<PathBuf, Arc<Slot>>>,
}

fn previews(ctx: &egui::Context) -> Arc<Previews> {
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with::<Arc<Previews>>(egui::Id::new("clam-previews"), Arc::default).clone())
}

/// Decodifica a imagem e devolve a miniatura. Erro: arquivo ilegível, formato fora da lista ou grande demais.
fn load_thumb(path: &Path) -> Result<ColorImage, String> {
    let mut reader = image::ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let thumb = img.thumbnail(THUMB_PX, THUMB_PX).to_rgba8();
    let size = [thumb.width() as usize, thumb.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()))
}

fn slot(ctx: &egui::Context, path: &Path) -> Arc<Slot> {
    let store = previews(ctx);
    let mut map = store.map.lock_unpoisoned();
    if let Some(s) = map.get(path) {
        return s.clone();
    }
    let loading = Arc::new(Slot::Loading);
    map.insert(path.to_path_buf(), loading.clone());
    drop(map);
    let (ctx, path) = (ctx.clone(), path.to_path_buf());
    std::thread::spawn(move || {
        let done = match load_thumb(&path) {
            Ok(img) => Slot::Ready(ctx.load_texture(format!("midia:{}", path.display()), img, egui::TextureOptions::LINEAR)),
            Err(_) => Slot::Failed,
        };
        previews(&ctx).map.lock_unpoisoned().insert(path, Arc::new(done));
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

/// Mostra a imagem na conversa, ou o quadro de vídeo com o play; clicar abre no sistema. Nada para outros tipos.
pub(crate) fn show(ui: &mut egui::Ui, name: &str, path: &Path) {
    match kind(name) {
        Some(Kind::Image) => image_block(ui, name, path),
        Some(Kind::Video) => video_block(ui, name, path),
        None => {}
    }
}

fn image_block(ui: &mut egui::Ui, name: &str, path: &Path) {
    match &*slot(ui.ctx(), path) {
        Slot::Ready(tex) => {
            let s = tex.size_vec2();
            let size = s * (SHOWN / s.x.max(s.y).max(1.0)).min(1.0);
            let reply = ui.add(egui::Image::new(tex).fit_to_exact_size(size).corner_radius(12).sense(Sense::click()));
            reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Abrir {name}")));
            if reply.on_hover_text("Abrir no visualizador do sistema").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open_external(path);
            }
        }
        Slot::Loading => {
            ui.add(egui::Spinner::new().size(18.0).color(BLURPLE));
        }
        Slot::Failed => {
            ui.label(egui::RichText::new("Não consegui mostrar esta imagem.").size(12.0).color(pal().muted));
        }
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
    if reply.on_hover_text("Reproduzir no player do sistema").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        open_external(path);
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
        let t = load_thumb(&file).unwrap();
        assert_eq!(t.size, [480, 240]);
        std::fs::write(dir.join("ruim.png"), b"isto nao e uma imagem").unwrap();
        assert!(load_thumb(&dir.join("ruim.png")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
