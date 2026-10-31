---
target: contatos
total_score: 30
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 0
target_identity: "file:/home/victor/orca/workspaces/dic/clam/examples/gui/contacts.rs"
target_fingerprint: "sha256:2e8fa87678a8547652a8a813d89f70421db0f12572730d5c41a2dd83ab3ba530"
target_path: /home/victor/orca/workspaces/dic/clam/examples/gui/contacts.rs
timestamp: 2026-10-31T00-25-36Z
slug: examples-gui-contacts-rs
---
Method: design review only (detector não se aplica: egui)

Nota 30/40 (contatos após 424a737).

Problemas: [P2] escudo de confiança verde (verde é "ok", mas expõe o IP); [P2] 5 paradas de Tab por linha e sem atalho para a busca; [P3] nome pendente com 8 hex; [P3] remover sem desfazer, modal pesado.
