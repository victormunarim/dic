//! Aparência: neomorfismo (soft UI), escuro ou claro. Uma cor base só; o relevo vem de duas sombras opostas (uma clara
//! no canto superior esquerdo, uma escura no inferior direito). Três estados: elevado (`raised`), afundado
//! (`sunken`) e plano. As telas não pintam sombra: usam `card`, `button`, `icon_button`, `raised_frame` e
//! `sunken_frame` daqui, de modo que mudar o visual é mudar este arquivo.

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Shadow, Shape, Stroke, TextStyle, Ui, vec2};

/// O único acento: ação principal, estado ativo, aro de quem fala (igual nos dois temas).
pub(crate) const BLURPLE: Color32 = Color32::from_rgb(0x53, 0x60, 0xea);

/// As cores que mudam entre o tema escuro e o claro. O resto (acento, raios) é igual nos dois.
pub(crate) struct Palette {
    /// Cor base: fundo da área de conteúdo, barras e superfícies elevadas (a mesma, é o que dá o efeito).
    pub bg: Color32,
    /// Superfície afundada: campos de texto, caixas de registro, painel do usuário.
    pub deep: Color32,
    /// Item selecionado/ativo.
    pub selected: Color32,
    /// Item sob o ponteiro.
    pub hover: Color32,
    /// Sombra clara (luz vinda de cima à esquerda) e sombra escura (embaixo à direita).
    pub light: Color32,
    pub dark: Color32,
    /// Linha fina e discreta entre as seções da janela.
    pub divider: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// Texto de destaque (títulos, nomes): o extremo de contraste do tema.
    pub strong: Color32,
    /// Só para "ao vivo/ok".
    pub green: Color32,
    /// Só para o destrutivo.
    pub red: Color32,
    pub yellow: Color32,
    /// Texto de erro.
    pub error: Color32,
    /// Aro do foco de teclado: o acento claro o bastante para passar de 3:1 sobre a base em cada tema.
    pub focus: Color32,
}

const DARK_PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0x2b, 0x2e, 0x35),
    deep: Color32::from_rgb(0x24, 0x27, 0x2d),
    selected: Color32::from_rgb(0x33, 0x37, 0x40),
    hover: Color32::from_rgb(0x35, 0x39, 0x42),
    light: Color32::from_rgb(0x38, 0x3c, 0x45),
    dark: Color32::from_rgb(0x1d, 0x1f, 0x24),
    divider: Color32::from_rgb(0x4a, 0x4f, 0x5a),
    text: Color32::from_rgb(0xdd, 0xe0, 0xe6),
    muted: Color32::from_rgb(0x9a, 0xa1, 0xad),
    strong: Color32::WHITE,
    green: Color32::from_rgb(0x1a, 0x86, 0x50),
    red: Color32::from_rgb(0xcf, 0x3d, 0x42),
    yellow: Color32::from_rgb(0xf0, 0xb2, 0x32),
    error: Color32::from_rgb(0xf2, 0x7b, 0x7e),
    focus: Color32::from_rgb(0x8f, 0x99, 0xff),
};

/// Tema claro: mesma lógica de relevo, com a luz branca e a sombra azulada; texto escuro (AA sobre a base).
const LIGHT_PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0xe4, 0xe8, 0xee),
    deep: Color32::from_rgb(0xd6, 0xdb, 0xe4),
    selected: Color32::from_rgb(0xd9, 0xde, 0xe8),
    hover: Color32::from_rgb(0xd3, 0xd9, 0xe3),
    light: Color32::from_rgb(0xff, 0xff, 0xff),
    dark: Color32::from_rgb(0xa0, 0xa8, 0xb6),
    divider: Color32::from_rgb(0x9a, 0xa2, 0xb1),
    text: Color32::from_rgb(0x2a, 0x2e, 0x37),
    muted: Color32::from_rgb(0x55, 0x5d, 0x6b),
    strong: Color32::from_rgb(0x12, 0x14, 0x19),
    green: Color32::from_rgb(0x15, 0x72, 0x3e),
    red: Color32::from_rgb(0xc2, 0x34, 0x3a),
    yellow: Color32::from_rgb(0x8f, 0x5f, 0x00),
    error: Color32::from_rgb(0xb0, 0x24, 0x2a),
    focus: BLURPLE,
};

static LIGHT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A paleta do tema em uso.
pub(crate) fn pal() -> &'static Palette {
    if LIGHT_ON.load(std::sync::atomic::Ordering::Relaxed) { &LIGHT_PALETTE } else { &DARK_PALETTE }
}

/// Liga ou desliga o tema claro. Quem chama refaz o `apply` para o egui acompanhar.
pub(crate) fn set_light(on: bool) {
    LIGHT_ON.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// O tema claro está ligado?
pub(crate) fn is_light() -> bool {
    LIGHT_ON.load(std::sync::atomic::Ordering::Relaxed)
}

/// Raios de canto, um por categoria.
pub(crate) const R_CARD: u8 = 18;
const R_BUTTON: u8 = 12;

/// Sombras de um relevo de `depth` px: a clara sobe para a esquerda, a escura desce para a direita.
fn shadows(depth: i8) -> (Shadow, Shadow) {
    let blur = (depth as u8).saturating_mul(2);
    (
        Shadow { offset: [-depth, -depth], blur, spread: 0, color: pal().light },
        Shadow { offset: [depth, depth], blur, spread: 0, color: pal().dark },
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
        Shape::line(vec![Pos2::new(l, b - r), Pos2::new(l, t + r * 0.4), Pos2::new(l + r * 0.4, t), Pos2::new(rt - r, t)], Stroke::new(w, pal().dark)),
        Shape::line(vec![Pos2::new(rt, t + r), Pos2::new(rt, b - r * 0.4), Pos2::new(rt - r * 0.4, b), Pos2::new(l + r, b)], Stroke::new(w, pal().light)),
    ]
}

/// Pinta `rect` afundado.
pub(crate) fn sunken(painter: &egui::Painter, rect: Rect, radius: u8, fill: Color32) {
    painter.extend(sunken_shapes(rect, radius, fill));
}

pub(crate) fn apply(ctx: &egui::Context) {
    let mut v = if is_light() { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.override_text_color = Some(pal().text);
    v.panel_fill = pal().bg;
    v.window_fill = pal().bg;
    // Contorno fino nos menus e janelas: sem ele o menu se mistura ao fundo do painel.
    v.window_stroke = Stroke::new(1.0_f32, pal().muted);
    v.extreme_bg_color = pal().deep;
    v.faint_bg_color = pal().bg;
    v.code_bg_color = pal().deep;
    v.hyperlink_color = Color32::from_rgb(0x00, 0xa8, 0xfc);
    v.selection.bg_fill = BLURPLE;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.window_corner_radius = CornerRadius::same(R_CARD);
    v.menu_corner_radius = CornerRadius::same(R_BUTTON);
    v.window_shadow = shadows(6).1;
    v.popup_shadow = shadows(4).1;
    let radius = CornerRadius::same(R_BUTTON);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = pal().bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal().divider);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal().text);
    w.noninteractive.corner_radius = radius;
    w.inactive.bg_fill = pal().bg;
    w.inactive.weak_bg_fill = pal().bg;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, pal().text);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = pal().hover;
    w.hovered.weak_bg_fill = pal().hover;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5_f32, pal().strong);
    w.hovered.corner_radius = radius;
    w.active.bg_fill = BLURPLE;
    w.active.weak_bg_fill = Color32::from_rgb(0x5a, 0x67, 0xe0);
    w.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.active.corner_radius = radius;
    w.open.bg_fill = pal().selected;
    w.open.weak_bg_fill = pal().selected;
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
    let out = egui::Frame::new().fill(pal().bg).corner_radius(CornerRadius::same(radius)).inner_margin(Margin::same(margin)).shadow(dark).show(ui, add);
    ui.painter().set(slot, light.as_shape(out.response.rect, radius));
    out
}

/// Moldura afundada, para campos e caixas de registro. O fundo vai numa vaga reservada para ficar por baixo
/// do miolo, já que o tamanho só se conhece depois de desenhá-lo.
pub(crate) fn sunken_frame<R>(ui: &mut Ui, radius: u8, margin: i8, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let slot = ui.painter().add(Shape::Noop);
    let out = egui::Frame::new().inner_margin(Margin::same(margin)).show(ui, add);
    ui.painter().set(slot, Shape::Vec(sunken_shapes(out.response.rect, radius, pal().deep)));
    out
}

/// Cartão da área de conteúdo: título, subtítulo opcional e o corpo, ocupando a largura da coluna.
pub(crate) fn card(ui: &mut Ui, title: &str, subtitle: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(4.0);
    raised_frame(ui, R_CARD, 22, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(18.0).color(pal().strong));
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).color(pal().muted));
        }
        ui.add_space(6.0);
        add(ui);
    });
    ui.add_space(18.0);
}

/// Cabeçalho da página: título grande e uma linha de explicação.
pub(crate) fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).heading().strong().color(pal().strong));
    ui.label(RichText::new(subtitle).color(pal().muted));
    ui.add_space(14.0);
}

/// Fundo sob o ponteiro: a base vai para a cor de hover do tema; um preenchimento colorido escurece, porque
/// clarear tiraria contraste do texto branco que vai sobre ele.
pub(crate) fn hovered_fill(fill: Color32) -> Color32 {
    if fill == pal().bg { pal().hover } else { fill.lerp_to_gamma(Color32::BLACK, 0.08) }
}

/// Deslocamento de 1 px para baixo enquanto o ícone sem moldura está apertado: o "afundar" dos controles que
/// não têm relevo para afundar.
fn press_nudge(reply: &egui::Response) -> egui::Vec2 {
    if reply.is_pointer_button_down_on() { vec2(0.0, 1.0) } else { egui::Vec2::ZERO }
}

/// Aro de foco do teclado, o mesmo em todos os controles desenhados à mão (que não ganham o do egui). Só
/// aparece com o foco; o raio acompanha o do controle.
pub(crate) fn focus_ring(ui: &Ui, reply: &egui::Response, radius: u8) {
    if reply.has_focus() {
        ui.painter().rect_stroke(reply.rect.expand(2.0), CornerRadius::same(radius.saturating_add(2)), Stroke::new(2.0_f32, pal().focus), egui::StrokeKind::Middle);
    }
}

/// Botão macio: elevado em repouso, afundado ao apertar, plano e apagado quando desligado. A cor de `fill`
/// é a da superfície (a base, para os neutros; o acento ou o semântico para ação principal e destrutiva).
fn soft_button(ui: &mut Ui, enabled: bool, text: RichText, fill: Color32, min: egui::Vec2) -> egui::Response {
    let name = text.text().to_string();
    let galley = egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, TextStyle::Button);
    let size = (galley.size() + ui.spacing().button_padding * 2.0).max(min);
    let (reply, rect, pressed) = soft_surface(ui, enabled, fill, size);
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &name));
    if ui.is_rect_visible(rect) {
        let color = if enabled { galley.job.sections.first().map_or(Color32::WHITE, |s| s.format.color) } else { pal().muted };
        let at = egui::Align2::CENTER_CENTER.align_size_within_rect(galley.size(), rect).min + if pressed { vec2(0.0, 1.0) } else { egui::Vec2::ZERO };
        ui.painter().galley(at, galley, color);
    }
    reply
}

/// A superfície do botão macio (relevo, hover, apertado, desligado) sem o conteúdo. Devolve a resposta, o
/// retângulo e se está apertado, para quem desenha o rótulo ou o ícone por cima.
fn soft_surface(ui: &mut Ui, enabled: bool, fill: Color32, size: egui::Vec2) -> (egui::Response, Rect, bool) {
    // Desligado continua alcançável pelo Tab (sem clicar), senão o motivo só existiria para o mouse.
    let (rect, mut reply) = ui.allocate_exact_size(size, if enabled { egui::Sense::click() } else { egui::Sense::focusable_noninteractive() });
    if enabled {
        reply = reply.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    let pressed = enabled && reply.is_pointer_button_down_on();
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let surface = if !enabled { fill.gamma_multiply(0.55) } else if reply.hovered() && !pressed { hovered_fill(fill) } else { fill };
        if !enabled {
            painter.rect_filled(rect, CornerRadius::same(R_BUTTON), surface);
        } else if pressed {
            sunken(painter, rect, R_BUTTON, surface.gamma_multiply(0.9));
        } else {
            raised(painter, rect, R_BUTTON, surface, 4);
        }
    }
    focus_ring(ui, &reply, R_BUTTON);
    (reply, rect, pressed)
}

/// Botão grande e cheio de cor (as ações principais).
pub(crate) fn big_button(ui: &mut Ui, text: &str, fill: Color32) -> egui::Response {
    soft_button(ui, true, RichText::new(text).strong().size(17.0).color(ink_on(fill)), fill, vec2(180.0, 48.0))
}

/// Bloco recolhido com o texto longo de uma opção: a tela mostra só o título, e quem quer saber mais abre. O texto
/// não fica fixo ocupando a tela (privacidade e limites se explicam aqui, não em parágrafos soltos).
pub(crate) fn details(ui: &mut Ui, title: &str, text: &str) {
    egui::CollapsingHeader::new(RichText::new(title).size(13.0).color(pal().muted)).icon(chevron).show(ui, |ui| note(ui, text));
}

/// Seta do bloco recolhido, desenhada: aponta para a direita fechada e gira para baixo ao abrir (`openness` de 0 a 1).
/// A seta padrão do egui vem da fonte e destoa dos outros ícones do tema.
fn chevron(ui: &mut Ui, openness: f32, response: &egui::Response) {
    let ink = if response.hovered() { pal().strong } else { pal().muted };
    let c = response.rect.center();
    let a = openness * std::f32::consts::FRAC_PI_2;
    let (sin, cos) = a.sin_cos();
    let turn = |x: f32, y: f32| c + vec2(x * cos - y * sin, x * sin + y * cos);
    ui.painter().add(egui::Shape::convex_polygon(vec![turn(-2.5, -4.0), turn(3.5, 0.0), turn(-2.5, 4.0)], ink, Stroke::NONE));
}

/// Marca discreta ao lado de um campo que só vale quando o programa abrir de novo (relógio, com a explicação no
/// hover); sem ela a pessoa muda o campo e não vê efeito nenhum.
pub(crate) fn reopen_mark(ui: &mut Ui) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(24.0, 24.0), egui::Sense::hover());
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, REOPEN_TIP));
    clock_icon(ui.painter(), rect.center(), 16.0, if reply.hovered() { pal().strong } else { pal().muted });
    reply.on_hover_text(REOPEN_TIP)
}

const REOPEN_TIP: &str = "Vale quando o programa abrir de novo.";

/// Tinta do texto sobre um preenchimento: branco sobre o acento e os semânticos (escuros nos dois temas), e o texto do
/// tema sobre a base, que no tema claro é quase branca e apagaria o branco.
fn ink_on_with(p: &Palette, fill: Color32) -> Color32 {
    if fill == p.bg { p.text } else { Color32::WHITE }
}

fn ink_on(fill: Color32) -> Color32 {
    ink_on_with(pal(), fill)
}

/// Motivo de um botão desligado por `button`/`big_button`/`phone_button`: a resposta deles segue "habilitada" para o egui
/// (o `Ui` não está desligado), então `on_disabled_hover_text` nunca apareceria.
pub(crate) fn why_off(reply: egui::Response, enabled: bool, reason: &str) -> egui::Response {
    if enabled { reply } else { reply.on_hover_text(reason) }
}

/// Base de todo botão de ícone sem moldura: nome acessível e dica andam juntos (o leitor de tela não lê o hover), a
/// placa suave aparece sob o ponteiro, o ícone afunda 1 px ao apertar e o aro de foco e a mãozinha vêm de graça.
/// `ink` recebe se o ponteiro está em cima e devolve a tinta; em `Ui` desligado o ícone apaga e não reage. Botão novo
/// nasce daqui, para não esquecer nome, dica ou foco.
fn bare_button(ui: &mut Ui, size: egui::Vec2, name: &str, tip: &str, ink: impl FnOnce(bool) -> Color32, icon: f32, draw: impl FnOnce(&egui::Painter, Pos2, f32, Color32)) -> egui::Response {
    let enabled = ui.is_enabled();
    let (rect, reply) = ui.allocate_exact_size(size, egui::Sense::click());
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
    // Com o menu dele aberto o botão segue marcado: o ponteiro está no popup, e numa lista de ⋮ iguais é o que diz de
    // qual linha é o menu.
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&reply));
    let hovered = enabled && (reply.hovered() || open);
    if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(R_BARE), pal().hover);
    }
    let color = if enabled { ink(hovered) } else { pal().muted.gamma_multiply(0.5) };
    draw(ui.painter(), rect.center() + press_nudge(&reply), icon, color);
    focus_ring(ui, &reply, R_BARE);
    reply.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Raio da placa de hover e do aro de foco dos botões sem moldura.
const R_BARE: u8 = 8;

/// Botão de tamanho normal, que pode ficar desligado.
pub(crate) fn button(ui: &mut Ui, enabled: bool, text: &str, fill: Color32) -> egui::Response {
    soft_button(ui, enabled, RichText::new(text).strong().color(ink_on(fill)), fill, vec2(110.0, 40.0))
}

/// Botão quadrado só com ícone; o nome da ação aparece ao passar o mouse.
#[allow(dead_code)] // vocabulário do tema: os ícones agora são desenhados (`glyph_button`), mas o botão de texto continua útil
pub(crate) fn icon_button(ui: &mut Ui, enabled: bool, icon: &str, tip: &str, fill: Color32) -> egui::Response {
    soft_button(ui, enabled, RichText::new(icon).size(18.0).color(ink_on(fill)), fill, vec2(44.0, 40.0)).on_hover_text(tip)
}

/// Botão quadrado com o fone desenhado (ligar, atender); a dica diz a ação. Desenhado: a fonte pode não ter o emoji.
pub(crate) fn phone_button(ui: &mut Ui, enabled: bool, tip: &str, fill: Color32) -> egui::Response {
    let (reply, rect, pressed) = soft_surface(ui, enabled, fill, vec2(44.0, 40.0));
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));
    phone_icon(ui.painter(), rect.center() + if pressed { vec2(0.0, 1.0) } else { egui::Vec2::ZERO }, 22.0, if enabled { Color32::WHITE } else { pal().muted });
    reply.on_hover_text(tip)
}

/// Caixa de marcar com a caixinha visível: no tema neomórfico o `checkbox` padrão some no fundo (mesma cor
/// base, sem contorno). Aqui a caixinha fica afundada (`pal().deep`) e com contorno `pal().muted`.
pub(crate) trait CheckExt {
    fn check(&mut self, checked: &mut bool, text: impl Into<egui::WidgetText>) -> egui::Response;
}

impl CheckExt for egui::Ui {
    fn check(&mut self, checked: &mut bool, text: impl Into<egui::WidgetText>) -> egui::Response {
        self.scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_fill = pal().deep;
            w.inactive.bg_stroke = Stroke::new(1.5_f32, pal().muted);
            w.hovered.bg_fill = pal().deep;
            w.hovered.bg_stroke = Stroke::new(1.5_f32, pal().text);
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
    bare_button(ui, vec2(34.0, 34.0), tip, tip, |h| if h { pal().strong } else { pal().text }, 8.0, draw_plus)
}

/// Botão só com um X, sem moldura (fechar). Desenhado: a fonte pode não ter o glifo "✕".
pub(crate) fn close_button(ui: &mut Ui, tip: &str) -> egui::Response {
    bare_button(ui, vec2(34.0, 34.0), tip, tip, |h| if h { pal().strong } else { pal().text }, 22.0, close_icon)
}

/// Três pontos um acima do outro (menu de ações; a dica é sempre "Mais ações"), desenhados: a fonte pode não ter o glifo "⋮". Quem chama
/// abre o menu com `egui::Popup::menu(&resposta)`.
pub(crate) fn dots_button(ui: &mut Ui, name: &str) -> egui::Response {
    let dots = |p: &egui::Painter, c: Pos2, _: f32, k: Color32| {
        for dy in [-6.0, 0.0, 6.0] {
            p.circle_filled(c + vec2(0.0, dy), 2.0, k);
        }
    };
    bare_button(ui, vec2(34.0, 34.0), name, "Mais ações", |h| if h { pal().strong } else { pal().muted }, 0.0, dots)
}

/// Escudo (confiança), desenhado: cheio, na tinta forte e com visto, quando ligado; só o contorno quando desligado.
/// Não é verde: o verde é "ao vivo/ok" e a confiança expõe o IP, então a cor não pode tranquilizar. `size` é a altura.
pub(crate) fn shield_icon(painter: &egui::Painter, c: Pos2, size: f32, ink: Color32, on: bool) {
    let k = size / 18.0;
    let pts = [vec2(-7.0, -8.0), vec2(7.0, -8.0), vec2(7.0, 1.0), vec2(0.0, 9.0), vec2(-7.0, 1.0)].map(|d| c + d * k).to_vec();
    if on {
        painter.add(egui::Shape::convex_polygon(pts, pal().strong, Stroke::NONE));
        // Visto no meio: o estado ligado não depende só da cor.
        let check = Stroke::new(1.8_f32 * k, pal().bg);
        painter.line_segment([c + vec2(-3.5, 0.0) * k, c + vec2(-1.0, 3.0) * k], check);
        painter.line_segment([c + vec2(-1.0, 3.0) * k, c + vec2(3.5, -3.0) * k], check);
    } else {
        painter.add(egui::Shape::convex_polygon(pts, Color32::TRANSPARENT, Stroke::new(1.6_f32 * k, ink)));
    }
}

/// Lixeira (remover), desenhada: tampa, alça e corpo. `size` é a altura aproximada.
pub(crate) fn trash_icon(painter: &egui::Painter, c: Pos2, size: f32, color: Color32) {
    let k = size / 18.0;
    let ink = Stroke::new(1.6_f32 * k, color);
    let p = painter;
    p.line_segment([c + vec2(-7.0, -5.0) * k, c + vec2(7.0, -5.0) * k], ink);
    p.line_segment([c + vec2(-2.5, -5.0) * k, c + vec2(-2.5, -8.0) * k], ink);
    p.line_segment([c + vec2(2.5, -5.0) * k, c + vec2(2.5, -8.0) * k], ink);
    p.line_segment([c + vec2(-2.5, -8.0) * k, c + vec2(2.5, -8.0) * k], ink);
    p.rect_stroke(Rect::from_min_max(c + vec2(-5.0, -5.0) * k, c + vec2(5.0, 8.0) * k), CornerRadius::same(2), ink, egui::StrokeKind::Middle);
    p.line_segment([c + vec2(-1.8, -2.0) * k, c + vec2(-1.8, 5.0) * k], ink);
    p.line_segment([c + vec2(1.8, -2.0) * k, c + vec2(1.8, 5.0) * k], ink);
}

/// Botão da lixeira; vermelha ao passar o mouse.
pub(crate) fn trash_button(ui: &mut Ui, name: &str) -> egui::Response {
    bare_button(ui, vec2(34.0, 34.0), name, name, |h| if h { pal().red } else { pal().muted }, 18.0, trash_icon)
}

/// Porta com seta para fora (sair), desenhada: batente aberto à direita, para a seta sair por ali.
pub(crate) fn exit_icon(painter: &egui::Painter, c: Pos2, size: f32, color: Color32) {
    let k = size / 18.0;
    let ink = Stroke::new(1.6_f32, color);
    painter.line_segment([c + vec2(1.0, -8.0) * k, c + vec2(-6.0, -8.0) * k], ink);
    painter.line_segment([c + vec2(-6.0, -8.0) * k, c + vec2(-6.0, 8.0) * k], ink);
    painter.line_segment([c + vec2(-6.0, 8.0) * k, c + vec2(1.0, 8.0) * k], ink);
    painter.line_segment([c + vec2(-2.0, 0.0) * k, c + vec2(8.0, 0.0) * k], ink);
    painter.line_segment([c + vec2(4.0, -4.0) * k, c + vec2(8.0, 0.0) * k], ink);
    painter.line_segment([c + vec2(4.0, 4.0) * k, c + vec2(8.0, 0.0) * k], ink);
}

/// Botão da porta com seta (sair); vermelho ao passar o mouse. Quem chama decide a confirmação.
pub(crate) fn exit_button(ui: &mut Ui, name: &str) -> egui::Response {
    bare_button(ui, vec2(34.0, 34.0), name, name, |h| if h { pal().red } else { pal().muted }, 18.0, exit_icon)
}

/// Xis (cancelar, fechar), desenhado.
pub(crate) fn close_icon(painter: &egui::Painter, c: Pos2, size: f32, color: Color32) {
    let h = size * 0.28;
    let ink = Stroke::new(2.0_f32, color);
    painter.line_segment([c + vec2(-h, -h), c + vec2(h, h)], ink);
    painter.line_segment([c + vec2(-h, h), c + vec2(h, -h)], ink);
}

/// Fone deitado em botão vermelho e em relevo (desligar, recusar), do mesmo tamanho do de atender: sem moldura o vermelho
/// do tema não passa de 3:1 sobre a base escura, e um fone cinza some ao lado do verde. Branco sobre o vermelho já
/// vale 4,5:1 nos dois temas. Quem chama decide o que acontece ao clicar; `name` é o que o leitor de tela diz.
pub(crate) fn hangup_button(ui: &mut Ui, name: &str) -> egui::Response {
    let (reply, rect, pressed) = soft_surface(ui, true, pal().red, vec2(44.0, 40.0));
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
    hangup_icon(ui.painter(), rect.center() + if pressed { vec2(0.0, 1.0) } else { egui::Vec2::ZERO }, 22.0, Color32::WHITE);
    reply.on_hover_text(name)
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
    focus_ring(ui, &reply, 8);
    p.line_segment([c + vec2(0.0, 5.0), c + vec2(0.0, 9.0)], ink);
    p.line_segment([c + vec2(-5.0, 9.0), c + vec2(5.0, 9.0)], ink);
    reply
}

/// Monitor riscado (parar de assistir): é o monitor de [`screen_button`] com um traço, para não se confundir com
/// sair da sala nem desligar. O traço leva um vão da cor da base, para ler em tamanho pequeno.
pub(crate) fn stop_watching_button(ui: &mut Ui) -> egui::Response {
    let reply = screen_icon(ui, Color32::TRANSPARENT, false, egui::Sense::click());
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Parar de assistir"));
    let color = if reply.hovered() { pal().strong } else { pal().muted };
    let c = reply.rect.center();
    let ink = Stroke::new(1.7_f32, color);
    let p = ui.painter();
    let screen = Rect::from_center_size(c + vec2(0.0, -2.0), vec2(20.0, 14.0));
    p.rect_stroke(screen, CornerRadius::same(2), ink, egui::StrokeKind::Middle);
    p.line_segment([c + vec2(0.0, 5.0), c + vec2(0.0, 9.0)], ink);
    p.line_segment([c + vec2(-5.0, 9.0), c + vec2(5.0, 9.0)], ink);
    let (from, to) = (c + vec2(-11.0, 10.0), c + vec2(11.0, -12.0));
    p.line_segment([from, to], Stroke::new(5.0_f32, pal().bg));
    p.line_segment([from, to], Stroke::new(1.9_f32, color));
    reply.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Monitor desenhado (compartilhar a tela): vermelho e preenchido enquanto transmite, cinza em repouso.
pub(crate) fn screen_button(ui: &mut Ui, sharing: bool) -> egui::Response {
    let name = if sharing { "Parar de compartilhar a tela" } else { "Compartilhar a minha tela" };
    let hovered = ui.rect_contains_pointer(egui::Rect::from_min_size(ui.cursor().min, vec2(32.0, 34.0)));
    let color = if sharing { pal().red } else if hovered { pal().strong } else { pal().muted };
    let reply = screen_icon(ui, color, sharing, egui::Sense::click());
    reply.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
    reply.on_hover_text(name).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Uma explicação em letra menor e apagada.
pub(crate) fn note(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(14.0).color(pal().muted));
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
    avatar_with(ui, name, size, dot, egui::Sense::click())
}

/// `avatar` com o `Sense` à escolha: quem já tem um botão para a mesma ação tira o foco do avatar
/// (`focusable: false`) para o teclado não parar duas vezes.
pub(crate) fn avatar_with(ui: &mut Ui, name: &str, size: f32, dot: Option<Color32>, sense: egui::Sense) -> egui::Response {
    let (rect, reply) = ui.allocate_exact_size(vec2(size, size), sense);
    let p = ui.painter();
    p.circle_filled(rect.center(), size / 2.0, hue(name));
    let initial = name.trim().chars().next().map_or('?', |c| c.to_uppercase().next().unwrap_or(c)).to_string();
    p.text(rect.center(), egui::Align2::CENTER_CENTER, initial, FontId::proportional(size * 0.46), Color32::WHITE);
    if let Some(color) = dot {
        let r = size * 0.17;
        let at = rect.right_bottom() - vec2(r * 0.9, r * 0.9);
        p.circle_filled(at, r + 2.5, pal().bg);
        p.circle_filled(at, r, color);
    }
    reply
}

/// Bolinha com o número de mensagens novas no canto superior direito de `rect`; `ring` é a cor do fundo atrás do ícone.
/// Desenhada, sem depender de glifo. Zero não desenha nada.
pub(crate) fn count_badge(painter: &egui::Painter, rect: egui::Rect, n: usize, ring: Color32) {
    if n == 0 {
        return;
    }
    let text = if n > 99 { "99+".to_string() } else { n.to_string() };
    let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
    let w = (galley.size().x + 8.0).max(18.0);
    let badge = egui::Rect::from_center_size(rect.right_top() + vec2(-4.0, 4.0), vec2(w, 18.0));
    painter.rect_filled(badge.expand(2.0), CornerRadius::same(11), ring);
    painter.rect_filled(badge, CornerRadius::same(9), BLURPLE);
    painter.galley(badge.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// Ponto de estado seguido do texto.
pub(crate) fn status_dot(ui: &mut Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 6.0, color);
        ui.label(RichText::new(text).strong().size(18.0).color(pal().strong));
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

/// Sol (ou lua, no tema claro) desenhado à mão: o botão que troca o tema. O raio do disco e os raios de luz
/// seguem a largura de `rect`.
pub(crate) fn theme_icon(painter: &egui::Painter, rect: Rect, light: bool, color: Color32) {
    let (c, u) = (rect.center(), rect.width());
    if light {
        // Lua crescente em contorno: o arco de fora e o de dentro, sem depender da cor do fundo do botão.
        let (big, small, off) = (u * 0.22, u * 0.18, vec2(u * 0.1, -u * 0.08));
        let (c2, mut pts) = (c + off, Vec::new());
        for i in 0..=48 {
            let a = (i as f32 * 7.5).to_radians();
            let p = c + vec2(a.cos(), a.sin()) * big;
            if (p - c2).length() >= small {
                pts.push(p);
            }
        }
        for i in (0..=48).rev() {
            let a = (i as f32 * 7.5).to_radians();
            let p = c2 + vec2(a.cos(), a.sin()) * small;
            if (p - c).length() <= big {
                pts.push(p);
            }
        }
        painter.add(Shape::closed_line(pts, Stroke::new(u * 0.06, color)));
        return;
    }
    painter.circle_filled(c, u * 0.14, color);
    let stroke = Stroke::new(u * 0.045, color);
    for i in 0..8 {
        let a = (i as f32 * 45.0).to_radians();
        let d = vec2(a.cos(), a.sin());
        painter.line_segment([c + d * (u * 0.22), c + d * (u * 0.32)], stroke);
    }
}

/// Pessoa (cabeça e ombros) desenhada à mão, no lugar do emoji 👤: a fonte pode não ter o glifo.
pub(crate) fn person_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    painter.circle_filled(center + vec2(0.0, -size * 0.2), size * 0.2, color);
    let shoulders: Vec<Pos2> = (0..=16)
        .map(|i| {
            let a = (180.0 + 180.0 * i as f32 / 16.0).to_radians();
            center + vec2(a.cos() * size * 0.38, size * 0.38 + a.sin() * size * 0.3)
        })
        .collect();
    painter.add(Shape::convex_polygon(shoulders, color, Stroke::NONE));
}

/// Fone de telefone (ligar), desenhado: um arco grosso com os dois bocais voltados para dentro.
pub(crate) fn phone_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    handset(painter, center, size, color, 0.0);
}

/// Fone desligando (recusar, desligar): o mesmo fone girado para ficar deitado, como no ícone de desligar de
/// qualquer telefone. Sem circuito de X: o X fica só para fechar.
pub(crate) fn hangup_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    handset(painter, center, size, color, 135.0_f32.to_radians());
}

/// O fone girado `turn` radianos em volta de `center`.
fn handset(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, turn: f32) {
    let (sin, cos) = turn.sin_cos();
    let at = |v: egui::Vec2| center + vec2(v.x * cos - v.y * sin, v.x * sin + v.y * cos);
    let (mid, r) = (vec2(size * 0.18, -size * 0.18), size * 0.5);
    let arc: Vec<Pos2> = (0..=12)
        .map(|i| {
            let a = (90.0 + 90.0 * i as f32 / 12.0).to_radians();
            at(mid + vec2(a.cos(), a.sin()) * r)
        })
        .collect();
    let (earpiece, mouthpiece) = (arc[0], arc[12]);
    painter.add(Shape::line(arc, Stroke::new(size * 0.14, color)));
    let stub = Stroke::new(size * 0.24, color);
    // Pontas do arco antes de girar: o bocal de cima em (0, r) e o de baixo em (-r, 0), em torno de `mid`.
    painter.line_segment([earpiece, at(mid + vec2(0.0, r - size * 0.2))], stub);
    painter.line_segment([mouthpiece, at(mid + vec2(size * 0.2 - r, 0.0))], stub);
}

/// Alto-falante com duas ondas (sala de voz), desenhado: a fonte pode não ter o emoji.
pub(crate) fn speaker_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let u = |x: f32, y: f32| center + vec2(x * size, y * size);
    painter.add(Shape::convex_polygon(vec![u(-0.38, -0.12), u(-0.22, -0.12), u(-0.02, -0.3), u(-0.02, 0.3), u(-0.22, 0.12), u(-0.38, 0.12)], color, Stroke::NONE));
    let wave = Stroke::new(size * 0.075, color);
    for r in [0.2_f32, 0.36] {
        let pts: Vec<Pos2> = (0..=8)
            .map(|i| {
                let a = (-45.0 + 90.0 * i as f32 / 8.0).to_radians();
                u(0.02, 0.0) + vec2(a.cos(), a.sin()) * (r * size)
            })
            .collect();
        painter.add(Shape::line(pts, wave));
    }
}

/// Balão de conversa (bate-papo), desenhado: contorno arredondado com a ponta para baixo à esquerda.
pub(crate) fn chat_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let u = |x: f32, y: f32| center + vec2(x * size, y * size);
    let ink = Stroke::new(size * 0.085, color);
    painter.rect_stroke(Rect::from_min_max(u(-0.4, -0.34), u(0.4, 0.14)), CornerRadius::same((size * 0.14) as u8), ink, egui::StrokeKind::Middle);
    painter.add(Shape::line(vec![u(-0.2, 0.14), u(-0.28, 0.38), u(0.0, 0.14)], ink));
}

/// Engrenagem (menu de páginas), desenhada: anel, miolo e oito dentes.
pub(crate) fn gear_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    painter.circle_stroke(center, size * 0.28, Stroke::new(size * 0.1, color));
    painter.circle_filled(center, size * 0.08, color);
    let tooth = Stroke::new(size * 0.14, color);
    for i in 0..8 {
        let d = vec2((i as f32 * 45.0).to_radians().cos(), (i as f32 * 45.0).to_radians().sin());
        painter.line_segment([center + d * (size * 0.34), center + d * (size * 0.44)], tooth);
    }
}

/// Microfone, desenhado: cápsula, concha e haste. `off` risca o ícone (mudo).
pub(crate) fn mic_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, off: bool) {
    let u = |x: f32, y: f32| center + vec2(x * size, y * size);
    let ink = Stroke::new(size * 0.085, color);
    painter.rect_stroke(Rect::from_min_max(u(-0.12, -0.4), u(0.12, 0.05)), CornerRadius::same((size * 0.12) as u8), ink, egui::StrokeKind::Middle);
    let cup: Vec<Pos2> = (0..=12).map(|i| { let a = (180.0 * i as f32 / 12.0).to_radians(); u(0.0, -0.02) + vec2(a.cos(), a.sin()) * (size * 0.26) }).collect();
    painter.add(Shape::line(cup, ink));
    painter.line_segment([u(0.0, 0.24), u(0.0, 0.4)], ink);
    painter.line_segment([u(-0.12, 0.4), u(0.12, 0.4)], ink);
    if off {
        painter.line_segment([u(-0.36, -0.36), u(0.36, 0.36)], Stroke::new(size * 0.1, color));
    }
}

/// Monitor pequeno (página de vídeo no menu), desenhado.
pub(crate) fn monitor_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let u = |x: f32, y: f32| center + vec2(x * size, y * size);
    let ink = Stroke::new(size * 0.085, color);
    painter.rect_stroke(Rect::from_min_max(u(-0.4, -0.3), u(0.4, 0.14)), CornerRadius::same((size * 0.08) as u8), ink, egui::StrokeKind::Middle);
    painter.line_segment([u(0.0, 0.14), u(0.0, 0.32)], ink);
    painter.line_segment([u(-0.2, 0.34), u(0.2, 0.34)], ink);
}

/// Folha com linhas (atividade, o registro do programa), desenhada.
pub(crate) fn log_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let u = |x: f32, y: f32| center + vec2(x * size, y * size);
    let ink = Stroke::new(size * 0.085, color);
    painter.rect_stroke(Rect::from_min_max(u(-0.28, -0.4), u(0.28, 0.4)), CornerRadius::same((size * 0.08) as u8), ink, egui::StrokeKind::Middle);
    for y in [-0.2_f32, 0.0, 0.2] {
        painter.line_segment([u(-0.14, y), u(0.14, y)], ink);
    }
}

/// Pessoa com um sinal de mais (convidar), desenhada.
pub(crate) fn invite_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    person_icon(painter, center + vec2(-size * 0.1, size * 0.04), size * 0.9, color);
    draw_plus(painter, center + vec2(size * 0.3, -size * 0.2), size * 0.15, color);
}

/// Triângulo com exclamação (aviso, contato pendente), desenhado.
pub(crate) fn alert_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let u = |x: f32, y: f32| center + vec2(x * size, y * size);
    let ink = Stroke::new(size * 0.1, color);
    painter.add(Shape::closed_line(vec![u(0.0, -0.4), u(0.42, 0.34), u(-0.42, 0.34)], ink));
    painter.line_segment([u(0.0, -0.1), u(0.0, 0.1)], ink);
    painter.circle_filled(u(0.0, 0.22), size * 0.05, color);
}

/// Botão só com um ícone desenhado, sem moldura (lista de contatos, painel do usuário). `draw` pinta o ícone;
/// `name` é o que o leitor de tela diz e `tip`, o hover; `color` fixa a tinta (sem ela: texto, e forte ao passar o mouse). Num
/// `Ui` desligado o ícone apaga e não clica.
pub(crate) fn glyph_button(ui: &mut Ui, name: &str, tip: &str, color: Option<Color32>, draw: impl FnOnce(&egui::Painter, Pos2, f32, Color32)) -> egui::Response {
    bare_button(ui, vec2(34.0, 34.0), name, tip, |h| color.unwrap_or(if h { pal().strong } else { pal().text }), 20.0, draw)
}

/// Linha de menu com ícone desenhado à esquerda; `selected` marca a linha como o item atual e `tint` fixa a
/// tinta (vermelho no destrutivo).
pub(crate) fn menu_item(ui: &mut Ui, selected: bool, tint: Option<Color32>, label: &str, draw: impl FnOnce(&egui::Painter, Pos2, f32, Color32)) -> egui::Response {
    menu_row(ui, None, selected, tint, label, None, draw)
}

/// Largura mínima de todo menu (engrenagem, confirmações, membro, convite): um só tamanho, para o menu não mudar de
/// largura de um gatilho para outro.
pub(crate) const MENU_WIDTH: f32 = 280.0;

/// Linha de menu completa. `blocked` com motivo apaga a linha (sem relevo nem clique) e leva o motivo ao hover e ao
/// nome acessível, para teclado e leitor de tela não ficarem com um item mudo; `shortcut` aparece à direita, na mesma
/// linha da ação a que pertence.
pub(crate) fn menu_row(ui: &mut Ui, blocked: Option<&str>, selected: bool, tint: Option<Color32>, label: &str, shortcut: Option<&str>, draw: impl FnOnce(&egui::Painter, Pos2, f32, Color32)) -> egui::Response {
    let enabled = blocked.is_none();
    // Apagado continua alcançável pelo Tab (sem clicar), senão o motivo só existiria para o mouse.
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::focusable_noninteractive() };
    let (rect, reply) = ui.allocate_exact_size(vec2(ui.available_width().max(MENU_WIDTH), 30.0), sense);
    let name = menu_row_name(label, blocked);
    // O foco antes do `widget_info`: o nome só é anunciado no quadro em que o controle ganha o foco.
    if enabled && take_menu_focus(ui.ctx()) {
        reply.request_focus();
    }
    reply.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, name.clone()));
    let painter = ui.painter();
    if enabled && selected {
        painter.rect_filled(rect, CornerRadius::same(8), pal().selected);
    } else if enabled && reply.hovered() {
        painter.rect_filled(rect, CornerRadius::same(8), pal().hover);
    }
    let ink = if !enabled {
        pal().muted.gamma_multiply(0.6)
    } else {
        tint.unwrap_or(if selected || reply.hovered() { pal().strong } else { pal().text })
    };
    draw(painter, rect.left_center() + vec2(18.0, 0.0), 18.0, ink);
    painter.text(rect.left_center() + vec2(36.0, 0.0), egui::Align2::LEFT_CENTER, label, TextStyle::Button.resolve(ui.style()), ink);
    if let Some(keys) = shortcut {
        painter.text(rect.right_center() - vec2(12.0, 0.0), egui::Align2::RIGHT_CENTER, keys, FontId::proportional(12.0), pal().muted);
    } else if enabled && selected {
        // O item atual não pode depender só do fundo: o ponto de acento diz "você está aqui" também sem o relevo.
        painter.circle_filled(rect.right_center() - vec2(16.0, 0.0), 4.0, pal().focus);
    }
    focus_ring(ui, &reply, 8);
    match blocked {
        Some(why) => reply.on_hover_text(why),
        None => reply.on_hover_cursor(egui::CursorIcon::PointingHand),
    }
}

/// Abre o menu do `trigger`. Se o gatilho foi acionado pelo teclado (Enter ou Espaço, sem soltar o ponteiro), o foco
/// entra no primeiro item habilitado: sem isso o Tab sai do popup para a tela atrás dele. Com o mouse nada muda.
pub(crate) fn menu(trigger: &egui::Response) -> egui::Popup<'static> {
    let ctx = &trigger.ctx;
    if trigger.clicked() && !ctx.input(|i| i.pointer.any_released()) {
        let now = ctx.cumulative_frame_nr();
        ctx.data_mut(|d| d.insert_temp(menu_focus_id(), now));
    }
    egui::Popup::menu(trigger)
}

fn menu_focus_id() -> egui::Id {
    egui::Id::new("menu-foco-no-primeiro")
}

/// Consome o pedido de foco de [`menu`]: vale neste quadro e no seguinte (o popup pode desenhar um depois do clique),
/// e vai ao primeiro item que perguntar. Um menu sem itens (formulário) deixa o pedido vencer sozinho.
fn take_menu_focus(ctx: &egui::Context) -> bool {
    let now = ctx.cumulative_frame_nr();
    ctx.data_mut(|d| match d.get_temp::<u64>(menu_focus_id()) {
        Some(at) => {
            d.remove_temp::<u64>(menu_focus_id());
            now.saturating_sub(at) <= 1
        }
        None => false,
    })
}

/// Nome acessível da linha de menu: o motivo do bloqueio vai junto, como nos botões de ligar da lista de contatos.
fn menu_row_name(label: &str, blocked: Option<&str>) -> String {
    match blocked {
        Some(why) => format!("{label} (indisponível: {})", why.to_lowercase()),
        None => label.to_string(),
    }
}

/// Item destrutivo com a pergunta dentro do menu: o primeiro clique troca o conteúdo pela frase do efeito, com
/// `confirm` (vermelho) e *Cancelar*; só o segundo clique age. Devolve `true` no clique que confirma. O efeito fica
/// escrito porque hover não existe no teclado nem no toque. Quem chama usa `PopupCloseBehavior::CloseOnClickOutside`
/// (senão o primeiro clique fecharia o menu), fecha com `ui.close()` ao receber `true` e chama `reset_confirm` quando
/// o menu fecha por fora.
#[allow(clippy::too_many_arguments)]
pub(crate) fn confirm_item(ui: &mut Ui, id: egui::Id, label: &str, hint: &str, question: &str, effect: &str, confirm: &str, draw: impl FnOnce(&egui::Painter, Pos2, f32, Color32)) -> bool {
    ui.set_min_width(MENU_WIDTH);
    let focus_cancel = id.with("foco-cancelar");
    let focus_item = id.with("foco-item");
    if !ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false) {
        let item = menu_item(ui, false, Some(pal().error), label, draw).on_hover_text(hint);
        // Voltou da pergunta (Cancelar ou Esc): o foco do teclado volta ao item de onde saiu.
        if ui.data_mut(|d| d.remove_temp::<bool>(focus_item)).unwrap_or(false) {
            item.request_focus();
        }
        if item.clicked() {
            ui.data_mut(|d| {
                d.insert_temp(id, true);
                d.insert_temp(focus_cancel, true);
            });
        }
        return false;
    }
    // Esc desfaz a pergunta em vez de fechar o menu todo; o segundo Esc fecha.
    let esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    ui.add(egui::Label::new(RichText::new(question).strong().color(pal().strong)).wrap());
    ui.add(egui::Label::new(RichText::new(effect).size(13.0).color(pal().text)).wrap());
    let mut done = false;
    if menu_item(ui, false, Some(pal().error), confirm, trash_icon).clicked() {
        done = true;
        reset_confirm(ui, id);
    }
    let cancel = menu_item(ui, false, None, "Cancelar", close_icon);
    // O item que tinha o foco sumiu; o foco vai ao padrão seguro, não ao destrutivo.
    if ui.data_mut(|d| d.remove_temp::<bool>(focus_cancel)).unwrap_or(false) {
        cancel.request_focus();
    }
    if esc || cancel.clicked() {
        reset_confirm(ui, id);
        ui.data_mut(|d| d.insert_temp(focus_item, true));
    }
    done
}

/// Volta o item de `confirm_item` ao começo (menu fechado por fora, ou resposta dada).
pub(crate) fn reset_confirm(ui: &Ui, id: egui::Id) {
    ui.data_mut(|d| {
        d.remove_temp::<bool>(id);
        d.remove_temp::<bool>(id.with("foco-cancelar"));
        d.remove_temp::<bool>(id.with("foco-item"));
    });
}

/// Devolve o foco ao botão que abriu o menu quando ele fecha pelo teclado (Esc, Enter num item). Com o mouse o foco
/// não importa, e roubá-lo de onde a pessoa clicou seria pior. `open` é se o menu está aberto neste quadro.
pub(crate) fn menu_focus_back(ui: &Ui, trigger: &egui::Response, open: bool) {
    let id = trigger.id.with("menu-aberto");
    let was = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    ui.data_mut(|d| d.insert_temp(id, open));
    if was && !open && !ui.input(|i| i.pointer.any_pressed()) {
        trigger.request_focus();
    }
}

/// Olho (mostrar a chave), desenhado: contorno de amêndoa e pupila. `off` risca o olho (esconder).
pub(crate) fn eye_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, off: bool) {
    let ink = Stroke::new(size * 0.09, color);
    let (w, h) = (size * 0.5, size * 0.28);
    let arc = |sign: f32| (0..=12).map(move |i| {
        let x = -1.0 + i as f32 / 6.0;
        center + vec2(x * w, sign * h * (1.0 - x * x))
    });
    let mut lens: Vec<Pos2> = arc(-1.0).collect();
    lens.extend(arc(1.0).collect::<Vec<_>>().into_iter().rev());
    painter.add(egui::Shape::closed_line(lens, ink));
    painter.circle_stroke(center, size * 0.14, ink);
    if off {
        painter.line_segment([center + vec2(-w * 0.85, h * 1.6), center + vec2(w * 0.85, -h * 1.6)], Stroke::new(size * 0.11, color));
    }
}

/// Copiar (duas folhas sobrepostas), desenhado.
pub(crate) fn copy_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let ink = Stroke::new(size * 0.09, color);
    let side = size * 0.42;
    let back = Rect::from_center_size(center + vec2(size * 0.08, -size * 0.08), vec2(side, side * 1.2));
    let front = Rect::from_center_size(center + vec2(-size * 0.08, size * 0.08), vec2(side, side * 1.2));
    painter.rect_stroke(back, CornerRadius::same(2), ink, egui::StrokeKind::Middle);
    painter.rect_filled(front, CornerRadius::same(2), pal().bg);
    painter.rect_stroke(front, CornerRadius::same(2), ink, egui::StrokeKind::Middle);
}

/// Relógio, desenhado: aro e dois ponteiros.
pub(crate) fn clock_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let ink = Stroke::new(size * 0.1, color);
    painter.circle_stroke(center, size * 0.4, ink);
    painter.line_segment([center, center + vec2(0.0, -size * 0.24)], ink);
    painter.line_segment([center, center + vec2(size * 0.17, size * 0.08)], ink);
}

/// Lupa (tela cheia), desenhada: aro e cabo.
pub(crate) fn magnifier_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let ink = Stroke::new(size * 0.1, color);
    painter.circle_stroke(center + vec2(-size * 0.08, -size * 0.08), size * 0.26, ink);
    painter.line_segment([center + vec2(size * 0.12, size * 0.12), center + vec2(size * 0.34, size * 0.34)], Stroke::new(size * 0.14, color));
}

/// Botão só com a lupa, sem moldura (tela cheia). A dica diz a ação.
pub(crate) fn magnifier_button(ui: &mut Ui, tip: &str) -> egui::Response {
    bare_button(ui, vec2(32.0, 34.0), tip, tip, |h| if h { pal().strong } else { pal().muted }, 22.0, magnifier_icon)
}

/// Cartão fantasma com o +: o lugar de uma coisa nova (sala, convite). O sinal usa o `muted` cheio, que passa
/// de 3:1 sobre a base nos dois temas, e sobe para o texto ao passar o mouse. Devolve se foi clicado.
pub(crate) fn ghost_card(ui: &mut Ui, tip: &str) -> bool {
    let (rect, ghost) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), egui::Sense::click());
    ghost.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
    raised(ui.painter(), rect, 12, pal().bg, 3);
    draw_plus(ui.painter(), rect.center(), 8.0, if ghost.hovered() { pal().text } else { pal().muted });
    focus_ring(ui, &ghost, 12);
    ghost.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Roda `add` num filho com o tamanho exato do que sobra no painel, cortando o que passar. Sem isso, um conteúdo mais
/// largo que o painel (nome longo, linha sem quebra) alarga o quadro do painel, e o `SidePanel` usa o retângulo do
/// quadro para posicionar o resto: a coluna de salas e contatos mudava de largura conforme o conteúdo.
pub(crate) fn confined<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, egui::Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    add(&mut child)
}

#[cfg(test)]
mod tests {
    /// Largura da coluna esquerda depois de desenhar `wide` de largura de conteúdo, com ou sem `confined`.
    fn panel_width(wide: f32, confine: bool) -> f32 {
        let ctx = egui::Context::default();
        let mut width = 0.0;
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 600.0))), ..Default::default() }, |ctx| {
                let out = egui::SidePanel::left("p").exact_width(300.0).resizable(false).frame(egui::Frame::new().inner_margin(Margin::symmetric(12, 14))).show(ctx, |ui| {
                    let body = |ui: &mut Ui| {
                        ui.allocate_exact_size(vec2(wide, 10.0), egui::Sense::hover());
                    };
                    if confine { confined(ui, body) } else { body(ui) }
                });
                width = out.response.rect.width();
            });
        }
        width
    }

    /// Largura do quadro de baixo (como o perfil) depois de conteúdo de `wide` de largura, com ou sem fixar a largura.
    fn footer_width(wide: f32, pin: bool) -> f32 {
        let ctx = egui::Context::default();
        let mut width = 0.0;
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 600.0))), ..Default::default() }, |ctx| {
                egui::SidePanel::left("p").exact_width(300.0).resizable(false).frame(egui::Frame::new().inner_margin(Margin::symmetric(12, 14))).show(ctx, |ui| confined(ui, |ui| {
                    let full = ui.available_width();
                    ui.allocate_exact_size(vec2(wide, 10.0), egui::Sense::hover());
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                        if pin {
                            ui.set_max_width(full);
                        }
                        let r = sunken_frame(ui, 12, 8, |ui| ui.set_width(ui.available_width()));
                        width = r.response.rect.width();
                    });
                }));
            });
        }
        width
    }

    #[test]
    fn resetting_a_confirmation_drops_the_question_and_its_pending_focus() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("confirmar");
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.data_mut(|d| {
                    d.insert_temp(id, true);
                    d.insert_temp(id.with("foco-cancelar"), true);
                    d.insert_temp(id.with("foco-item"), true);
                });
                reset_confirm(ui, id);
                for key in [id, id.with("foco-cancelar"), id.with("foco-item")] {
                    assert_eq!(ui.data(|d| d.get_temp::<bool>(key)), None);
                }
            });
        });
    }

    #[test]
    fn the_footer_keeps_its_width_whatever_the_content_above() {
        assert_eq!(footer_width(100.0, true), footer_width(700.0, true));
        assert!(footer_width(700.0, false) > footer_width(100.0, false), "sem fixar, o rodapé acompanha o conteúdo");
    }

    #[test]
    fn a_confined_panel_keeps_its_width_whatever_the_content() {
        assert_eq!(panel_width(100.0, true), 300.0);
        assert_eq!(panel_width(700.0, true), 300.0);
        assert!(panel_width(700.0, false) > 300.0, "sem confined o quadro cresce");
    }

    use super::*;

    fn luminance(c: Color32) -> f32 {
        let lin = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn ghost_card_sign_is_visible_on_the_base_in_both_themes() {
        // O + dos cartões fantasmas usa `muted` cheio; sem a opacidade de antes ele passa de 3:1 nos dois temas.
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            assert!(contrast(p.muted, p.bg) >= 3.0, "o + dos cartões fantasmas some no tema {name}");
        }
    }

    #[test]
    fn focus_ring_passes_3_to_1_on_both_bases_in_both_themes() {
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            for (on, bg) in [("bg", p.bg), ("deep", p.deep), ("hover", p.hover), ("selected", p.selected)] {
                let r = contrast(p.focus, bg);
                assert!(r >= 3.0, "tema {name}: o aro de foco sobre {on} tem contraste {r:.2} (< 3)");
            }
        }
    }

    /// Roda `frames` quadros de um gatilho com menu e devolve os nomes que o leitor de tela ouviu ao ganhar foco.
    /// No quadro 2 o gatilho recebe o foco; no `press_at`, Enter.
    fn open_menu_by_keyboard(press: bool) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut trigger = None;
        let mut heard = Vec::new();
        for frame in 0..6 {
            let mut input = egui::RawInput::default();
            if press && frame == 3 {
                input.events.push(egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() });
                input.events.push(egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: Default::default() });
            }
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if frame == 1 {
                        ctx.memory_mut(|m| m.request_focus(trigger.unwrap()));
                    }
                    let dots = dots_button(ui, "n:Gatilho");
                    trigger = Some(dots.id);
                    menu(&dots).show(|ui| {
                        menu_row(ui, Some("sem chave"), false, None, "n:Apagado", None, close_icon);
                        menu_row(ui, None, false, None, "n:Primeiro", None, close_icon);
                        menu_row(ui, None, false, None, "n:Segundo", None, close_icon);
                    });
                });
            });
            heard.extend(out.platform_output.events.iter().map(|e| e.widget_info().label.clone().unwrap_or_default()));
        }
        heard
    }

    #[test]
    fn a_menu_opened_by_keyboard_focuses_its_first_enabled_item() {
        let heard = open_menu_by_keyboard(true);
        assert!(heard.iter().any(|l| l == "n:Primeiro"), "o foco não entrou no menu (ouvido: {heard:?})");
        assert!(!heard.iter().any(|l| l.starts_with("n:Apagado") || l == "n:Segundo"), "o foco foi ao item errado (ouvido: {heard:?})");
    }

    #[test]
    fn a_menu_that_nobody_opened_takes_no_focus() {
        let heard = open_menu_by_keyboard(false);
        assert!(!heard.iter().any(|l| l == "n:Primeiro"), "o foco entrou sozinho (ouvido: {heard:?})");
    }

    #[test]
    fn a_blocked_menu_row_carries_its_reason_in_the_accessible_name() {
        assert_eq!(menu_row_name("Convidar", None), "Convidar");
        assert_eq!(menu_row_name("Convidar", Some("Abra um servidor")), "Convidar (indisponível: abra um servidor)");
    }

    #[test]
    fn trust_shield_check_is_readable_on_its_fill() {
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            let r = contrast(p.bg, p.strong);
            assert!(r >= 4.5, "tema {name}: o visto do escudo sobre o preenchimento tem contraste {r:.2} (< 4,5)");
            assert!(contrast(p.strong, p.bg) >= 3.0, "tema {name}: o escudo some sobre a base");
        }
    }

    #[test]
    fn both_themes_keep_aa_contrast_for_text_on_the_base() {
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            for (what, c) in [("text", p.text), ("muted", p.muted), ("strong", p.strong), ("error", p.error)] {
                for (on, bg) in [("bg", p.bg), ("deep", p.deep)] {
                    let r = contrast(c, bg);
                    assert!(r >= 4.5, "tema {name}: {what} sobre {on} tem contraste {r:.2} (< 4,5)");
                }
            }
        }
    }

    #[test]
    fn white_text_on_every_fill_keeps_aa_in_both_themes() {
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            for (what, fill) in [("acento", BLURPLE), ("verde", p.green), ("vermelho", p.red)] {
                // O hover escurece o preenchimento, então vale o contraste do estado em repouso e do hover.
                for f in [fill, fill.lerp_to_gamma(Color32::BLACK, 0.08)] {
                    let r = contrast(Color32::WHITE, f);
                    assert!(r >= 4.5, "tema {name}: branco sobre {what} tem contraste {r:.2} (< 4,5)");
                }
            }
        }
    }

    #[test]
    fn text_on_a_neutral_button_is_readable_in_both_themes() {
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            let ink = ink_on_with(p, p.bg);
            for (state, fill) in [("repouso", p.bg), ("hover", p.hover)] {
                let r = contrast(ink, fill);
                assert!(r >= 4.5, "tema {name}: texto do botão neutro em {state} tem contraste {r:.2} (< 4,5)");
            }
            assert_eq!(ink_on_with(p, BLURPLE), Color32::WHITE, "o acento segue com texto branco");
        }
    }

    /// Os nomes que o leitor de tela ouve ao pôr o foco no controle desenhado por `draw`. O nome só sai como
    /// evento quando o controle ganha o foco: o primeiro quadro descobre o id, o segundo pede o foco antes de
    /// desenhar.
    fn heard(draw: fn(&mut Ui) -> egui::Response) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut id = None;
        let mut heard = Vec::new();
        for _ in 0..3 {
            let out = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if let Some(id) = id.take() {
                        ctx.memory_mut(|m| m.request_focus(id));
                    }
                    let reply = draw(ui);
                    if heard.is_empty() {
                        id = Some(reply.id);
                    }
                });
            });
            heard.extend(out.platform_output.events.iter().map(|e| e.widget_info().label.clone().unwrap_or_default()));
        }
        heard
    }

    #[test]
    fn every_icon_button_has_an_accessible_name() {
        type Draw = fn(&mut Ui) -> egui::Response;
        let buttons: [(&str, Draw); 12] = [
            ("n:Novo contato", |ui| plus_button(ui, "n:Novo contato")),
            ("n:Fechar", |ui| close_button(ui, "n:Fechar")),
            ("n:Desligar", |ui| hangup_button(ui, "n:Desligar")),
            ("n:Tela cheia", |ui| magnifier_button(ui, "n:Tela cheia")),
            ("Compartilhar a minha tela", |ui| screen_button(ui, false)),
            ("Parar de assistir", stop_watching_button),
            ("n:Ligar", |ui| phone_button(ui, true, "n:Ligar", BLURPLE)),
            ("n:Sair", |ui| exit_button(ui, "n:Sair")),
            ("n:Mais ações de Ana", |ui| dots_button(ui, "n:Mais ações de Ana")),
            ("n:Apagar a sala", |ui| trash_button(ui, "n:Apagar a sala")),
            ("n:Menu", |ui| glyph_button(ui, "n:Menu", "dica", None, gear_icon)),
            ("n:Bate-papo", |ui| glyph_button(ui, "n:Bate-papo", "dica", Some(BLURPLE), chat_icon)),
        ];
        for (want, draw) in buttons {
            let heard = heard(draw);
            assert!(heard.iter().any(|l| l == want), "botão sem nome acessível: {want} (ouvido: {heard:?})");
        }
    }

    #[test]
    fn text_buttons_have_an_accessible_name_even_when_disabled() {
        // O desligado também recebe o foco: sem isso o teclado nunca chegaria ao motivo.
        assert!(heard(|ui| button(ui, true, "n:Cancelar", pal().bg)).iter().any(|l| l == "n:Cancelar"));
        assert!(heard(|ui| button(ui, false, "n:Enviar", BLURPLE)).iter().any(|l| l == "n:Enviar"), "botão desligado fora do Tab");
        assert!(heard(|ui| big_button(ui, "n:Criar", BLURPLE)).iter().any(|l| l == "n:Criar"));
    }

    #[test]
    fn bare_icons_stay_visible_on_the_hover_plate_in_both_themes() {
        // Ícone sem moldura: 3:1 (não é texto) sobre a base e sobre a placa de hover.
        for (name, p) in [("escuro", &DARK_PALETTE), ("claro", &LIGHT_PALETTE)] {
            for (what, c) in [("muted", p.muted), ("text", p.text), ("strong", p.strong)] {
                for (on, bg) in [("base", p.bg), ("placa", p.hover)] {
                    let r = contrast(c, bg);
                    assert!(r >= 3.0, "tema {name}: {what} sobre {on} tem contraste {r:.2} (< 3)");
                }
            }
        }
    }

    #[test]
    fn status_colors_are_visible_on_the_base_in_both_themes() {
        // Pontos e ícones de estado vêm sempre com rótulo; o piso é 2,8:1 porque o mesmo tom também precisa
        // segurar texto branco (4,5:1) quando é preenchimento.
        for p in [&DARK_PALETTE, &LIGHT_PALETTE] {
            for c in [p.green, p.red, p.yellow] {
                assert!(contrast(c, p.bg) >= 2.8, "{:?} sobre a base", c);
            }
        }
    }

    #[test]
    fn light_switch_changes_the_palette() {
        set_light(true);
        let light = pal().bg;
        set_light(false);
        assert_ne!(light, pal().bg);
        assert!(!is_light());
    }
}
