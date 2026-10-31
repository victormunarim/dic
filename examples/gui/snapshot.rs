//! Telas da janela em PNG, sem display: o `egui` desenha para malhas de triângulos e aqui elas são rasterizadas
//! na CPU. Serve para quem revisa a interface (pessoa ou ferramenta) ver o resultado sem abrir a janela.
//!
//! ```text
//! CLAM_TELAS=docs/telas cargo test --features gui --example gui telas_em_png -- --ignored
//! ```
//! Não é teste de regressão pixel a pixel: fonte e arredondamento mudam de máquina para máquina. O teste comum só
//! garante que o desenho não ficou em branco; o que gera os arquivos é o `#[ignore]`, que mexe no tema global.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use eframe::egui::{self, epaint, Color32, Pos2, Rect, vec2};

use crate::conf::Form;
use crate::engine::Shared;
use crate::theme;
use crate::ui::{App, Tab};

/// Imagem RGBA (alfa pré-multiplicado não é usado: o fundo é opaco).
pub(crate) struct Frame {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) rgba: Vec<u8>,
}

impl Frame {
    fn filled(w: usize, h: usize, c: Color32) -> Self {
        Self { w, h, rgba: [c.r(), c.g(), c.b(), 255].repeat(w * h) }
    }

    /// Quantos pixels diferem da cor de `bg`: zero é tela em branco.
    pub(crate) fn drawn_pixels(&self, bg: Color32) -> usize {
        self.rgba.as_chunks::<4>().0.iter().filter(|p| p[..3] != [bg.r(), bg.g(), bg.b()]).count()
    }

    pub(crate) fn save(&self, path: &Path) {
        let file = std::fs::File::create(path).expect("criar o PNG");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), self.w as u32, self.h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().and_then(|mut w| w.write_image_data(&self.rgba)).expect("gravar o PNG");
    }
}

/// Texturas do `egui` (atlas de fontes e imagens) acumuladas de quadro em quadro.
#[derive(Default)]
struct Textures(HashMap<epaint::TextureId, (usize, usize, Vec<Color32>)>);

impl Textures {
    fn apply(&mut self, delta: &epaint::textures::TexturesDelta) {
        for (id, d) in &delta.set {
            let epaint::ImageData::Color(img) = &d.image;
            let [w, h] = img.size;
            match d.pos {
                None => {
                    self.0.insert(*id, (w, h, img.pixels.clone()));
                }
                Some([x0, y0]) => {
                    let Some((tw, _, px)) = self.0.get_mut(id) else { continue };
                    for row in 0..h {
                        let at = (y0 + row) * *tw + x0;
                        if let (Some(dst), Some(src)) = (px.get_mut(at..at + w), img.pixels.get(row * w..(row + 1) * w)) {
                            dst.copy_from_slice(src);
                        }
                    }
                }
            }
        }
        for id in &delta.free {
            self.0.remove(id);
        }
    }

    /// Texel mais próximo de `uv`; branco se a textura não existe.
    fn sample(&self, id: epaint::TextureId, uv: Pos2) -> [f32; 4] {
        let Some((w, h, px)) = self.0.get(&id) else { return [1.0; 4] };
        let x = ((uv.x * *w as f32) as usize).min(w.saturating_sub(1));
        let y = ((uv.y * *h as f32) as usize).min(h.saturating_sub(1));
        px.get(y * w + x).map_or([1.0; 4], |c| c.to_array().map(|v| f32::from(v) / 255.0))
    }
}

/// Rasteriza as malhas sobre `frame`. Mistura no espaço gamma, como o `egui_glow`: cor do vértice vezes o texel,
/// ambos com alfa pré-multiplicado, e `dst = src + dst * (1 - alfa)`.
fn raster(frame: &mut Frame, prims: &[epaint::ClippedPrimitive], tex: &Textures, ppp: f32) {
    let (fw, fh) = (frame.w as f32, frame.h as f32);
    for prim in prims {
        let epaint::Primitive::Mesh(mesh) = &prim.primitive else { continue };
        let clip = prim.clip_rect;
        let (cx0, cy0) = ((clip.min.x * ppp).max(0.0), (clip.min.y * ppp).max(0.0));
        let (cx1, cy1) = ((clip.max.x * ppp).min(fw), (clip.max.y * ppp).min(fh));
        for tri in mesh.indices.as_chunks::<3>().0 {
            let (Some(a), Some(b), Some(c)) = (
                mesh.vertices.get(tri[0] as usize),
                mesh.vertices.get(tri[1] as usize),
                mesh.vertices.get(tri[2] as usize),
            ) else {
                continue;
            };
            let (pa, pb, pc) = (a.pos * ppp, b.pos * ppp, c.pos * ppp);
            let area = (pb.x - pa.x) * (pc.y - pa.y) - (pb.y - pa.y) * (pc.x - pa.x);
            if area.abs() < 1e-6 {
                continue;
            }
            let x0 = pa.x.min(pb.x).min(pc.x).max(cx0).floor() as usize;
            let x1 = pa.x.max(pb.x).max(pc.x).min(cx1).ceil() as usize;
            let y0 = pa.y.min(pb.y).min(pc.y).max(cy0).floor() as usize;
            let y1 = pa.y.max(pb.y).max(pc.y).min(cy1).ceil() as usize;
            let (ca, cb, cc) = (a.color.to_array().map(f32::from), b.color.to_array().map(f32::from), c.color.to_array().map(f32::from));
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                    let w0 = ((pb.x - p.x) * (pc.y - p.y) - (pb.y - p.y) * (pc.x - p.x)) / area;
                    let w1 = ((pc.x - p.x) * (pa.y - p.y) - (pc.y - p.y) * (pa.x - p.x)) / area;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    let uv = Pos2::new(w0 * a.uv.x + w1 * b.uv.x + w2 * c.uv.x, w0 * a.uv.y + w1 * b.uv.y + w2 * c.uv.y);
                    let t = tex.sample(mesh.texture_id, uv);
                    let src: [f32; 4] = std::array::from_fn(|i| (w0 * ca[i] + w1 * cb[i] + w2 * cc[i]) / 255.0 * t[i]);
                    let Some(dst) = frame.rgba.get_mut((y * frame.w + x) * 4..(y * frame.w + x) * 4 + 4) else { continue };
                    let keep = 1.0 - src[3];
                    for i in 0..3 {
                        dst[i] = ((src[i] + f32::from(dst[i]) / 255.0 * keep).clamp(0.0, 1.0) * 255.0).round() as u8;
                    }
                }
            }
        }
    }
}

/// Roda `ui` por alguns quadros (o layout do `egui` assenta depois do segundo) e devolve o último desenhado.
fn render(size: egui::Vec2, bg: Color32, mut ui: impl FnMut(&egui::Context)) -> Frame {
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    let mut tex = Textures::default();
    let mut last = None;
    for i in 0..4 {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            time: Some(f64::from(i) * 0.1),
            ..Default::default()
        };
        let out = ctx.run(raw, &mut ui);
        tex.apply(&out.textures_delta);
        last = Some((ctx.tessellate(out.shapes, 1.0), ));
    }
    let mut frame = Frame::filled(size.x as usize, size.y as usize, bg);
    if let Some((prims,)) = last {
        raster(&mut frame, &prims, &tex, 1.0);
    }
    frame
}

/// O app como a janela o cria, sem rede, sem disco e sem pessoa conectada.
fn sample_app(tab: Tab) -> App {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("runtime");
    let form = Form {
        my_name: "Ana".into(),
        secret: "11".repeat(32),
        contacts: format!("Bruno {}\nCarla {}", "ab".repeat(32), "cd".repeat(32)),
        intros: true,
        ..Form::default()
    };
    App {
        rt,
        form,
        log: Arc::default(),
        shared: Arc::<Shared>::default(),
        running: None,
        done: Arc::default(),
        error: String::new(),
        tab,
        audio_dirty: false,
        screen_tex: Default::default(),
        zoom: None,
        trust_ask: None,
        chat_with: None,
        chat_draft: String::new(),
        srv: Default::default(),
        devs: Default::default(),
    }
}

fn shot(tab: Tab, size: egui::Vec2) -> Frame {
    let mut app = sample_app(tab);
    render(size, theme::pal().bg, |ctx| eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O rasterizador pinta: a tela de Configurações em tema escuro tem bem mais que o fundo.
    #[test]
    fn rasterizer_draws_something() {
        let frame = shot(Tab::Settings, vec2(1280.0, 800.0));
        let drawn = frame.drawn_pixels(theme::pal().bg);
        assert!(drawn > frame.w * frame.h / 50, "só {drawn} pixels fora do fundo");
    }

    /// Gera os PNGs de `CLAM_TELAS` (padrão `docs/telas`), escuro e claro. Mexe no tema global, por isso só roda
    /// pedido (`-- --ignored`) e nunca junto dos outros testes.
    #[test]
    #[ignore = "gera arquivos; veja o cabeçalho do módulo"]
    fn telas_em_png() {
        let dir = std::path::PathBuf::from(std::env::var("CLAM_TELAS").unwrap_or_else(|_| "docs/telas".into()));
        std::fs::create_dir_all(&dir).expect("criar a pasta");
        let tabs = [(Tab::Contacts, "chamada"), (Tab::Servers, "servidores"), (Tab::Video, "video"), (Tab::Audio, "audio"), (Tab::Settings, "configuracoes"), (Tab::Log, "atividade")];
        for light in [false, true] {
            theme::set_light(light);
            let theme_name = if light { "claro" } else { "escuro" };
            for (tab, name) in tabs {
                shot(tab, vec2(1280.0, 800.0)).save(&dir.join(format!("{name}-{theme_name}.png")));
            }
            shot(Tab::Settings, vec2(820.0, 560.0)).save(&dir.join(format!("configuracoes-estreita-{theme_name}.png")));
        }
        theme::set_light(false);
    }
}
