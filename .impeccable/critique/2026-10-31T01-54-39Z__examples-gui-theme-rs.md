---
target: botões
total_score: 32
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 1
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/theme.rs"
target_fingerprint: "sha256:a631a71f3245f2e35441aa70c7ab541f3cac5de100730fd87364a74b477b6efd"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/theme.rs
timestamp: 2026-10-31T01-54-39Z
slug: examples-gui-theme-rs
---
# Crítica de Botões, 2ª rodada (32/40)

Method: design review only (detector não se aplica: GUI egui nativa, sem display).

P1: `soft_button` não chama `widget_info`: todo botão de texto (~20) sem nome acessível e sem estado desligado. Teste cobre só ícones.
P2: oito cópias da rotina do ícone sem moldura; dica embutida em uns (plus, close, hangup) e deixada ao chamador em outros (exit, trash, dots; menu do convite sem dica); lista do teste feita à mão.
P2: Recusar e Desligar cinza em repouso, vermelho só no hover; ao lado do Atender verde o Recusar some.
P2: ícone sem moldura só troca muted por strong no hover; falta placa suave.
P3: botão de texto e phone_button sem mãozinha; botão desligado com Sense::hover não recebe Tab (motivo só no mouse); screen_button adivinha hover por rect_contains_pointer.
