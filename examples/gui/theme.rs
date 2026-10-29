//! Aparência: neomorfismo (soft UI) escuro. Uma cor base só; o relevo vem de duas sombras opostas (uma clara
//! no canto superior esquerdo, uma escura no inferior direito). Três estados: elevado (`raised`), afundado
//! (`sunken`) e plano. As telas não pintam sombra: usam `card`, `button`, `icon_button`, `raised_frame` e
//! `sunken_frame` daqui, de modo que mudar o visual é mudar este arquivo.

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Shadow, Shape, Stroke, TextStyle, Ui, vec2};

/// Cor base: fundo da área de conteúdo e das superfícies elevadas (a mesma, é o que dá o efeito).
pub(crate) const BG: Color32 = Color32::from_rgb(0x2b, 0x2e, 0x35);
/// Barras laterais e cartões: o mesmo material do fundo; quem separa é o relevo.
pub(crate) const PANEL: Color32 = BG;
/// Superfície afundada: campos de texto, caixas de registro, painel do usuário.
pub(crate) const DEEP: Color32 = Color32::from_rgb(0x24, 0x27, 0x2d);
/// Item selecionado/ativo (afundado, um pouco mais claro que o fundo afundado).
pub(crate) const SELECTED: Color32 = Color32::from_rgb(0x33, 0x37, 0x40);
/// Sombra clara (luz vinda de cima à esquerda) e sombra escura (embaixo à direita).
const LIGHT: Color32 = Color32::from_rgb(0x38, 0x3c, 0x45);
const DARK: Color32 = Color32::from_rgb(0x1d, 0x1f, 0x24);
pub(crate) const TEXT: Color32 = Color32::from_rgb(0xdd, 0xe0, 0xe6);
pub(crate) const MUTED: Color32 = Color32::from_rgb(0x9a, 0xa1, 0xad);
/// O único acento: ação principal, estado ativo, aro de quem fala.
pub(crate) const BLURPLE: Color32 = Color32::from_rgb(0x6a, 0x77, 0xf5);
/// Só para "ao vivo/ok".
pub(crate) const GREEN: Color32 = Color32::from_rgb(0x2f, 0xb0, 0x68);
/// Só para o destrutivo.
pub(crate) const RED: Color32 = Color32::from_rgb(0xe0, 0x4a, 0x4f);
pub(crate) const YELLOW: Color32 = Color32::from_rgb(0xf0, 0xb2, 0x32);
/// Botão neutro: a própria base, elevada.
pub(crate) const GREY_BUTTON: Color32 = BG;

/// Raios de canto, um por categoria.
const R_CARD: u8 = 18;
const R_BUTTON: u8 = 12;

/// Sombras de um relevo de `depth` px: a clara sobe para a esquerda, a escura desce para a direita.
fn shadows(depth: i8) -> (Shadow, Shadow) {
    let blur = (depth as u8).saturating_mul(2);
    (
        Shadow { offset: [-depth, -depth], blur, spread: 0, color: LIGHT },
        Shadow { offset: [depth, depth], blur, spread: 0, color: DARK },
    )
}

/// Pinta `rect` elevado: as duas sombras por baixo e a superfície por cima.
pub(crate) fn raised(painter: &egui::Painter, rect: Rect, radius: u8, fill: Color32, depth: i8) {
    let (light, dark) = shadows(depth);
    painter.add(light.as_shape(rect, radius));
    painter.add(dark.as_shape(rect, radius));
    painter.rect_filled(rect, CornerRadius::same(radius), fill);
}

/// Formas de `rect` afundado: a superfície com a borda de cima/esquerda escura e a de baixo/direita clara,
/// como se a luz batesse numa cavidade.
fn sunken_shapes(rect: Rect, radius: u8, fill: Color32) -> Vec<Shape> {
    let r = f32::from(radius);
    let (w, inset) = (2.0_f32, 1.0_f32);
    let (l, t, rt, b) = (rect.left() + inset, rect.top() + inset, rect.right() - inset, rect.bottom() - inset);
    vec![
        Shape::rect_filled(rect, CornerRadius::same(radius), fill),
        Shape::line(vec![Pos2::new(l, b - r), Pos2::new(l, t + r * 0.4), Pos2::new(l + r * 0.4, t), Pos2::new(rt - r, t)], Stroke::new(w, DARK)),
        Shape::line(vec![Pos2::new(rt, t + r), Pos2::new(rt, b - r * 0.4), Pos2::new(rt - r * 0.4, b), Pos2::new(l + r, b)], Stroke::new(w, LIGHT)),
    ]
}

/// Pinta `rect` afundado.
pub(crate) fn sunken(painter: &egui::Painter, rect: Rect, radius: u8, fill: Color32) {
    painter.extend(sunken_shapes(rect, radius, fill));
}

pub(crate) fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = PANEL;
    // Contorno fino nos menus e janelas: sem ele o menu se mistura ao fundo do painel.
    v.window_stroke = Stroke::new(1.0_f32, MUTED);
    v.extreme_bg_color = DEEP;
    v.faint_bg_color = PANEL;
    v.code_bg_color = DEEP;
    v.hyperlink_color = Color32::from_rgb(0x00, 0xa8, 0xfc);
    v.selection.bg_fill = BLURPLE;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.window_corner_radius = CornerRadius::same(R_CARD);
    v.menu_corner_radius = CornerRadius::same(R_BUTTON);
    v.window_shadow = shadows(6).1;
    v.popup_shadow = shadows(4).1;
    let radius = CornerRadius::same(R_BUTTON);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, DARK);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = radius;
    w.inactive.bg_fill = GREY_BUTTON;
    w.inactive.weak_bg_fill = GREY_BUTTON;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = Color32::from_rgb(0x35, 0x39, 0x42);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x35, 0x39, 0x42);
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.hovered.corner_radius = radius;
    w.active.bg_fill = BLURPLE;
    w.active.weak_bg_fill = Color32::from_rgb(0x5a, 0x67, 0xe0);
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

/// Moldura elevada (soft UI) com o miolo em `add`. A sombra clara fica numa vaga reservada antes da moldura
/// para ir por baixo dela; a escura é a sombra da própria moldura.
pub(crate) fn raised_frame<R>(ui: &mut Ui, radius: u8, margin: i8, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let (light, dark) = shadows(5);
    let slot = ui.painter().add(Shape::Noop);
    let out = egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(radius)).inner_margin(Margin::same(margin)).shadow(dark).show(ui, add);
    ui.painter().set(slot, light.as_shape(out.response.rect, radius));
    out
}

/// Moldura afundada, para campos e caixas de registro. O fundo vai numa vaga reservada para ficar por baixo
/// do miolo, já que o tamanho só se conhece depois de desenhá-lo.
pub(crate) fn sunken_frame<R>(ui: &mut Ui, radius: u8, margin: i8, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let slot = ui.painter().add(Shape::Noop);
    let out = egui::Frame::new().inner_margin(Margin::same(margin)).show(ui, add);
    ui.painter().set(slot, Shape::Vec(sunken_shapes(out.response.rect, radius, DEEP)));
    out
}

/// Cartão da área de conteúdo: título, subtítulo opcional e o corpo, ocupando a largura da coluna.
pub(crate) fn card(ui: &mut Ui, title: &str, subtitle: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(4.0);
    raised_frame(ui, R_CARD, 22, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(18.0).color(Color32::WHITE));
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).color(MUTED));
        }
        ui.add_space(6.0);
        add(ui);
    });
    ui.add_space(18.0);
}

/// Cabeçalho da página: título grande e uma linha de explicação.
pub(crate) fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).heading().strong().color(Color32::WHITE));
    ui.label(RichText::new(subtitle).color(MUTED));
    ui.add_space(14.0);
}

/// Botão macio: elevado em repouso, afundado ao apertar, plano e apagado quando desligado. A cor de `fill`
/// é a da superfície (a base, para os neutros; o acento ou o semântico para ação principal e destrutiva).
fn soft_button(ui: &mut Ui, enabled: bool, text: RichText, fill: Color32, min: egui::Vec2) -> egui::Response {
    let galley = egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, TextStyle::Button);
    let size = (galley.size() + ui.spacing().button_padding * 2.0).max(min);
    let (rect, reply) = ui.allocate_exact_size(size, if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let pressed = enabled && reply.is_pointer_button_down_on();
        let surface = if !enabled { fill.gamma_multiply(0.55) } else if reply.hovered() && !pressed { fill.lerp_to_gamma(Color32::WHITE, 0.06) } else { fill };
        if !enabled {
            painter.rect_filled(rect, CornerRadius::same(R_BUTTON), surface);
        } else if pressed {
            sunken(painter, rect, R_BUTTON, surface.gamma_multiply(0.9));
        } else {
            raised(painter, rect, R_BUTTON, surface, 4);
        }
        let color = if enabled { galley.job.sections.first().map_or(Color32::WHITE, |s| s.format.color) } else { MUTED };
        let at = egui::Align2::CENTER_CENTER.align_size_within_rect(galley.size(), rect).min + if pressed { vec2(0.0, 1.0) } else { egui::Vec2::ZERO };
        painter.galley(at, galley, color);
    }
    reply
}

/// Botão grande e cheio de cor (as ações principais).
pub(crate) fn big_button(ui: &mut Ui, text: &str, fill: Color32) -> egui::Response {
    soft_button(ui, true, RichText::new(text).strong().size(17.0).color(Color32::WHITE), fill, vec2(180.0, 48.0))
}

/// Botão de tamanho normal, que pode ficar desligado.
pub(crate) fn button(ui: &mut Ui, enabled: bool, text: &str, fill: Color32) -> egui::Response {
    soft_button(ui, enabled, RichText::new(text).strong().color(Color32::WHITE), fill, vec2(110.0, 40.0))
}

/// Botão quadrado só com ícone; o nome da ação aparece ao passar o mouse.
pub(crate) fn icon_button(ui: &mut Ui, enabled: bool, icon: &str, tip: &str, fill: Color32) -> egui::Response {
    soft_button(ui, enabled, RichText::new(icon).size(18.0).color(Color32::WHITE), fill, vec2(44.0, 40.0)).on_hover_text(tip)
}

/// Caixa de marcar com a caixinha visível: no tema neomórfico o `checkbox` padrão some no fundo (mesma cor
/// base, sem contorno). Aqui a caixinha fica afundada (`DEEP`) e com contorno `MUTED`.
pub(crate) trait CheckExt {
    fn check(&mut self, checked: &mut bool, text: impl Into<egui::WidgetText>) -> egui::Response;
}

impl CheckExt for egui::Ui {
    fn check(&mut self, checked: &mut bool, text: impl Into<egui::WidgetText>) -> egui::Response {
        self.scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_fill = DEEP;
            w.inactive.bg_stroke = Stroke::new(1.5_f32, MUTED);
            w.hovered.bg_fill = DEEP;
            w.hovered.bg_stroke = Stroke::new(1.5_f32, TEXT);
            ui.checkbox(checked, text)
        })
        .inner
    }
}

/// Campo de texto de uma linha, alto e largo.
pub(crate) fn field<'t>(text: &'t mut String, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).hint_text(hint).margin(Margin::symmetric(12, 10)).desired_width(f32::INFINITY)
}

/// Sinal de mais desenhado (duas barras), porque a fonte pode não ter o glifo e mostrar um quadrado.
pub(crate) fn draw_plus(painter: &egui::Painter, center: Pos2, half: f32, color: Color32) {
    let stroke = Stroke::new(half * 0.28, color);
    painter.line_segment([center - vec2(half, 0.0), center + vec2(half, 0.0)], stroke);
    painter.line_segment([center - vec2(0.0, half), center + vec2(0.0, half)], stroke);
}

/// Botão só com o sinal de mais, sem moldura. A dica diz o que ele cria.
pub(crate) fn plus_button(ui: &mut Ui, tip: &str) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(34.0, 34.0), egui::Sense::click());
    draw_plus(ui.painter(), rect.center(), 8.0, if reply.hovered() { Color32::WHITE } else { TEXT });
    reply.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Três pontos um acima do outro (menu de ações), desenhados: a fonte pode não ter o glifo "⋮". Quem chama
/// abre o menu com `egui::Popup::menu(&resposta)`.
pub(crate) fn dots_button(ui: &mut Ui) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(28.0, 34.0), egui::Sense::click());
    let ink = if reply.hovered() { Color32::WHITE } else { MUTED };
    for dy in [-6.0, 0.0, 6.0] {
        ui.painter().circle_filled(rect.center() + vec2(0.0, dy), 2.0, ink);
    }
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Escudo desenhado (confiança): cheio e verde quando ligado, só o contorno quando desligado.
pub(crate) fn shield_button(ui: &mut Ui, on: bool) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(28.0, 34.0), egui::Sense::click());
    let c = rect.center();
    let pts = [vec2(-7.0, -8.0), vec2(7.0, -8.0), vec2(7.0, 1.0), vec2(0.0, 9.0), vec2(-7.0, 1.0)].map(|d| c + d).to_vec();
    if on {
        ui.painter().add(egui::Shape::convex_polygon(pts, GREEN, Stroke::NONE));
        // Visto no meio: o estado ligado não depende só da cor.
        let ink = Stroke::new(1.8_f32, DEEP);
        ui.painter().line_segment([c + vec2(-3.5, 0.0), c + vec2(-1.0, 3.0)], ink);
        ui.painter().line_segment([c + vec2(-1.0, 3.0), c + vec2(3.5, -3.0)], ink);
    } else {
        let ink = if reply.hovered() { Color32::WHITE } else { MUTED };
        ui.painter().add(egui::Shape::convex_polygon(pts, Color32::TRANSPARENT, Stroke::new(1.6_f32, ink)));
    }
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Lixeira desenhada (remover): tampa, alça e corpo; vermelha ao passar o mouse.
pub(crate) fn trash_button(ui: &mut Ui) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(28.0, 34.0), egui::Sense::click());
    let c = rect.center();
    let ink = Stroke::new(1.6_f32, if reply.hovered() { RED } else { MUTED });
    let p = ui.painter();
    p.line_segment([c + vec2(-7.0, -5.0), c + vec2(7.0, -5.0)], ink);
    p.line_segment([c + vec2(-2.5, -5.0), c + vec2(-2.5, -8.0)], ink);
    p.line_segment([c + vec2(2.5, -5.0), c + vec2(2.5, -8.0)], ink);
    p.line_segment([c + vec2(-2.5, -8.0), c + vec2(2.5, -8.0)], ink);
    p.rect_stroke(egui::Rect::from_min_max(c + vec2(-5.0, -5.0), c + vec2(5.0, 8.0)), CornerRadius::same(2), ink, egui::StrokeKind::Middle);
    p.line_segment([c + vec2(-1.8, -2.0), c + vec2(-1.8, 5.0)], ink);
    p.line_segment([c + vec2(1.8, -2.0), c + vec2(1.8, 5.0)], ink);
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Porta com seta para fora (sair), desenhada; vermelha ao passar o mouse. Quem chama decide a confirmação.
pub(crate) fn exit_button(ui: &mut Ui) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(28.0, 34.0), egui::Sense::click());
    let c = rect.center();
    let ink = Stroke::new(1.6_f32, if reply.hovered() { RED } else { MUTED });
    let p = ui.painter();
    // Batente aberto à direita, para a seta sair por ali.
    p.line_segment([c + vec2(1.0, -8.0), c + vec2(-6.0, -8.0)], ink);
    p.line_segment([c + vec2(-6.0, -8.0), c + vec2(-6.0, 8.0)], ink);
    p.line_segment([c + vec2(-6.0, 8.0), c + vec2(1.0, 8.0)], ink);
    p.line_segment([c + vec2(-2.0, 0.0), c + vec2(8.0, 0.0)], ink);
    p.line_segment([c + vec2(4.0, -4.0), c + vec2(8.0, 0.0)], ink);
    p.line_segment([c + vec2(4.0, 4.0), c + vec2(8.0, 0.0)], ink);
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Círculo com um xis (sair da sala), desenhado: simples e legível em tamanho pequeno. Vermelho ao passar o
/// mouse; quem chama decide o que acontece ao clicar.
pub(crate) fn hangup_button(ui: &mut Ui) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(32.0, 34.0), egui::Sense::click());
    let c = rect.center();
    let ink = Stroke::new(1.8_f32, if reply.hovered() { RED } else { MUTED });
    let p = ui.painter();
    p.circle_stroke(c, 9.0, ink);
    p.line_segment([c + vec2(-3.5, -3.5), c + vec2(3.5, 3.5)], ink);
    p.line_segment([c + vec2(-3.5, 3.5), c + vec2(3.5, -3.5)], ink);
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Monitor desenhado, na `color` dada; `filled` preenche a tela. Base de [`screen_button`] e do ícone de quem transmite.
pub(crate) fn screen_icon(ui: &mut Ui, color: Color32, filled: bool, sense: egui::Sense) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(32.0, 34.0), sense);
    let c = rect.center();
    let ink = Stroke::new(1.7_f32, color);
    let p = ui.painter();
    let screen = egui::Rect::from_center_size(c + vec2(0.0, -2.0), vec2(20.0, 14.0));
    p.rect_stroke(screen, CornerRadius::same(2), ink, egui::StrokeKind::Middle);
    if filled {
        p.rect_filled(screen.shrink(3.5), CornerRadius::same(1), color);
    }
    p.line_segment([c + vec2(0.0, 5.0), c + vec2(0.0, 9.0)], ink);
    p.line_segment([c + vec2(-5.0, 9.0), c + vec2(5.0, 9.0)], ink);
    reply
}

/// Monitor desenhado (compartilhar a tela): vermelho e preenchido enquanto transmite, cinza em repouso.
pub(crate) fn screen_button(ui: &mut Ui, sharing: bool) -> egui::Response {
    let hovered = ui.rect_contains_pointer(egui::Rect::from_min_size(ui.cursor().min, vec2(32.0, 34.0)));
    let color = if sharing { RED } else if hovered { Color32::WHITE } else { MUTED };
    screen_icon(ui, color, sharing, egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)
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
