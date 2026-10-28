//! Aparência: paleta escura de inspiração no Discord, tamanhos generosos e os pequenos componentes
//! (cartão, botão grande, avatar, ponto de estado) que as abas compartilham.

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Stroke, TextStyle, Ui, vec2};

/// Fundo da área de conteúdo.
pub(crate) const BG: Color32 = Color32::from_rgb(0x31, 0x33, 0x38);
/// Barra lateral e cartões.
pub(crate) const PANEL: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x31);
/// Fundo mais fundo: campos de texto, caixas de registro, painel do usuário.
pub(crate) const DEEP: Color32 = Color32::from_rgb(0x1e, 0x1f, 0x22);
pub(crate) const SELECTED: Color32 = Color32::from_rgb(0x40, 0x44, 0x4b);
pub(crate) const TEXT: Color32 = Color32::from_rgb(0xdb, 0xde, 0xe1);
pub(crate) const MUTED: Color32 = Color32::from_rgb(0x94, 0x9b, 0xa4);
pub(crate) const BLURPLE: Color32 = Color32::from_rgb(0x58, 0x65, 0xf2);
pub(crate) const GREEN: Color32 = Color32::from_rgb(0x23, 0xa5, 0x5a);
pub(crate) const RED: Color32 = Color32::from_rgb(0xda, 0x37, 0x3c);
pub(crate) const YELLOW: Color32 = Color32::from_rgb(0xf0, 0xb2, 0x32);
pub(crate) const GREY_BUTTON: Color32 = Color32::from_rgb(0x4e, 0x50, 0x58);

pub(crate) fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0_f32, DEEP);
    v.extreme_bg_color = DEEP;
    v.faint_bg_color = PANEL;
    v.code_bg_color = DEEP;
    v.hyperlink_color = Color32::from_rgb(0x00, 0xa8, 0xfc);
    v.selection.bg_fill = BLURPLE;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    let radius = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x3f, 0x41, 0x47));
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = radius;
    w.inactive.bg_fill = GREY_BUTTON;
    w.inactive.weak_bg_fill = GREY_BUTTON;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = Color32::from_rgb(0x6d, 0x6f, 0x78);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x5d, 0x5f, 0x68);
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.hovered.corner_radius = radius;
    w.active.bg_fill = BLURPLE;
    w.active.weak_bg_fill = Color32::from_rgb(0x48, 0x55, 0xdb);
    w.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.active.corner_radius = radius;
    w.open.bg_fill = SELECTED;
    w.open.weak_bg_fill = SELECTED;
    w.open.corner_radius = radius;
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(26.0));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(16.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(16.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(13.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(14.5));
        s.spacing.item_spacing = vec2(10.0, 10.0);
        s.spacing.button_padding = vec2(16.0, 9.0);
        s.spacing.interact_size = vec2(44.0, 38.0);
        s.spacing.icon_width = 24.0;
        s.spacing.icon_width_inner = 14.0;
        s.spacing.icon_spacing = 10.0;
        s.spacing.slider_width = 260.0;
        s.spacing.combo_width = 280.0;
        s.spacing.text_edit_width = 320.0;
        s.spacing.scroll.bar_width = 10.0;
        s.spacing.scroll.floating = false;
    });
}

/// Cartão da área de conteúdo: título, subtítulo opcional e o corpo, ocupando a largura da coluna.
pub(crate) fn card(ui: &mut Ui, title: &str, subtitle: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(20)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(18.0).color(Color32::WHITE));
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).color(MUTED));
        }
        ui.add_space(6.0);
        add(ui);
    });
    ui.add_space(14.0);
}

/// Cabeçalho da página: título grande e uma linha de explicação.
pub(crate) fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).heading().strong().color(Color32::WHITE));
    ui.label(RichText::new(subtitle).color(MUTED));
    ui.add_space(14.0);
}

/// Botão grande e cheio de cor (as ações principais).
pub(crate) fn big_button(ui: &mut Ui, text: &str, fill: Color32) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).strong().size(17.0).color(Color32::WHITE))
            .fill(fill)
            .corner_radius(CornerRadius::same(8))
            .min_size(vec2(180.0, 46.0)),
    )
}

/// Botão cheio de tamanho normal, que pode ficar desligado.
pub(crate) fn button(ui: &mut Ui, enabled: bool, text: &str, fill: Color32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(Color32::WHITE)).fill(fill).corner_radius(CornerRadius::same(6)).min_size(vec2(110.0, 38.0)),
    )
}

/// Botão quadrado só com ícone; o nome da ação aparece ao passar o mouse.
pub(crate) fn icon_button(ui: &mut Ui, enabled: bool, icon: &str, tip: &str, fill: Color32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(icon).size(18.0).color(Color32::WHITE)).fill(fill).corner_radius(CornerRadius::same(6)).min_size(vec2(42.0, 38.0)),
    )
    .on_hover_text(tip)
}

/// Campo de texto de uma linha, alto e largo.
pub(crate) fn field<'t>(text: &'t mut String, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).hint_text(hint).margin(Margin::symmetric(12, 10)).desired_width(f32::INFINITY)
}

/// Uma explicação em letra menor e apagada.
pub(crate) fn note(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(14.0).color(MUTED));
}

/// Cor estável de um nome, para o avatar.
fn hue(name: &str) -> Color32 {
    const PALETTE: [Color32; 6] = [
        Color32::from_rgb(0x58, 0x65, 0xf2),
        Color32::from_rgb(0x23, 0xa5, 0x5a),
        Color32::from_rgb(0xeb, 0x45, 0x9e),
        Color32::from_rgb(0xf0, 0xb2, 0x32),
        Color32::from_rgb(0x00, 0xa8, 0xfc),
        Color32::from_rgb(0xed, 0x42, 0x45),
    ];
    PALETTE[name.bytes().fold(7usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % PALETTE.len()]
}

/// Círculo colorido com a inicial do nome; `dot` põe um ponto de estado no canto. A resposta aceita clique.
pub(crate) fn avatar(ui: &mut Ui, name: &str, size: f32, dot: Option<Color32>) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let p = ui.painter();
    p.circle_filled(rect.center(), size / 2.0, hue(name));
    let initial = name.trim().chars().next().map_or('?', |c| c.to_uppercase().next().unwrap_or(c)).to_string();
    p.text(rect.center(), egui::Align2::CENTER_CENTER, initial, FontId::proportional(size * 0.46), Color32::WHITE);
    if let Some(color) = dot {
        let r = size * 0.17;
        let at = rect.right_bottom() - vec2(r * 0.9, r * 0.9);
        p.circle_filled(at, r + 2.5, PANEL);
        p.circle_filled(at, r, color);
    }
    reply
}

/// Ponto de estado seguido do texto.
pub(crate) fn status_dot(ui: &mut Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 6.0, color);
        ui.label(RichText::new(text).strong().size(18.0).color(Color32::WHITE));
    });
}

/// Símbolo de ligar/desligar (arco aberto em cima e um traço), desenhado à mão para não depender de fonte.
pub(crate) fn power_icon(ui: &Ui, rect: egui::Rect, color: Color32) {
    let (c, r) = (rect.center(), rect.width() * 0.13);
    let stroke = egui::Stroke::new(rect.width() * 0.04, color);
    // Do ângulo de -60° ao de 240° (em torno do topo), passando por baixo.
    let arc: Vec<egui::Pos2> = (0..=24)
        .map(|i| {
            let a = (-60.0f32 + 300.0 * i as f32 / 24.0).to_radians();
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    ui.painter().add(egui::Shape::line(arc, stroke));
    ui.painter().line_segment([c - vec2(0.0, r * 1.25), c - vec2(0.0, r * 0.1)], stroke);
}
