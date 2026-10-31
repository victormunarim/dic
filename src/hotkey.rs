//! Atalhos globais: teclas que valem com a janela em segundo plano.
//!
//! Cada sistema tem o seu caminho, e nenhum depende de servidor ou serviço de terceiros: no X11 a tecla é tomada
//! da raiz (`XGrabKey`); no Wayland o pedido vai ao portal `GlobalShortcuts` (o ambiente de trabalho pode pedir
//! confirmação e deixar a pessoa trocar a tecla); no Windows é `RegisterHotKey`. No macOS não há suporte: quem
//! chama fica com o atalho da janela. O módulo não conhece a interface: devolve o `id` da tecla apertada.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::sync::LockExt;

/// Tecla de um atalho global: só as que existem em todos os sistemas com o mesmo nome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Letra minúscula ASCII (`b'a'..=b'z'`).
    Letter(u8),
    /// Algarismo de 0 a 9.
    Digit(u8),
    /// F1 a F12.
    F(u8),
}

impl Key {
    /// Lê `"A"`, `"7"` ou `"F5"` (maiúsculas ou minúsculas). Qualquer outra coisa é `None`.
    pub fn from_name(name: &str) -> Option<Key> {
        let name = name.trim().to_ascii_lowercase();
        let b = name.as_bytes();
        match b {
            [c @ b'a'..=b'z'] => Some(Key::Letter(*c)),
            [c @ b'0'..=b'9'] => Some(Key::Digit(c - b'0')),
            [b'f', rest @ ..] => {
                let n: u8 = std::str::from_utf8(rest).ok()?.parse().ok()?;
                (1..=12).contains(&n).then_some(Key::F(n))
            }
            _ => None,
        }
    }

    /// Nome do tipo `m`, `7`, `F5` (a grafia do XDG e do X11).
    #[cfg(any(unix, test))]
    fn xdg_name(self) -> String {
        match self {
            Key::Letter(c) => (c as char).to_string(),
            Key::Digit(d) => d.to_string(),
            Key::F(n) => format!("F{n}"),
        }
    }
}

/// Tecla com os modificadores. Precisa de ao menos um modificador ou ser uma tecla F, senão rouba a digitação.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: Key,
}

impl Combo {
    /// `CTRL+SHIFT+d`, como o portal do XDG pede.
    #[cfg(any(unix, test))]
    fn xdg_trigger(&self) -> String {
        let mut out = String::new();
        for (on, name) in [(self.ctrl, "CTRL"), (self.alt, "ALT"), (self.shift, "SHIFT")] {
            if on {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&self.key.xdg_name());
        out
    }
}

/// Um atalho a registrar: `id` volta em `on_press`; `label` é o texto que o sistema mostra, se mostrar.
#[derive(Clone, Debug)]
pub struct Binding {
    pub id: u32,
    pub combo: Combo,
    pub label: String,
}

/// Estado do registro, lido pela janela: dá para saber se vale e, se não, por quê.
#[derive(Default)]
pub struct Status {
    active: AtomicBool,
    note: Mutex<String>,
}

impl Status {
    /// As teclas estão registradas no sistema (e então a janela não precisa tratá-las).
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    /// Por que não valem em segundo plano; vazio se valem ou ainda estão sendo registradas.
    pub fn note(&self) -> String {
        self.note.lock_unpoisoned().clone()
    }
    fn fail(&self, why: impl Into<String>) {
        *self.note.lock_unpoisoned() = why.into();
        self.active.store(false, Ordering::Relaxed);
    }
    fn ok(&self) {
        self.note.lock_unpoisoned().clear();
        self.active.store(true, Ordering::Relaxed);
    }
}

/// Os atalhos registrados. Largar o valor os desfaz.
pub struct Handle {
    stop: Arc<AtomicBool>,
    pub status: Arc<Status>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Registra `bindings` numa thread própria. Nunca falha de cara: o resultado aparece em `Handle::status`.
/// `on_press` roda nessa thread, então deve ser rápido.
pub fn register(bindings: Vec<Binding>, on_press: Arc<dyn Fn(u32) + Send + Sync>) -> Handle {
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Status::default());
    let (s, st) = (stop.clone(), status.clone());
    let spawned = std::thread::Builder::new().name("atalhos-globais".into()).spawn(move || {
        if let Err(e) = run(&bindings, &on_press, &s, &st) {
            st.fail(e);
        }
    });
    if let Err(e) = spawned {
        status.fail(format!("não deu para iniciar a thread dos atalhos: {e}"));
    }
    Handle { stop, status }
}

#[cfg(unix)]
fn run(bindings: &[Binding], on_press: &Arc<dyn Fn(u32) + Send + Sync>, stop: &AtomicBool, st: &Status) -> Result<(), String> {
    let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") || (std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none());
    if wayland {
        portal::run(bindings, on_press, stop, st)
    } else {
        x11::run(bindings, on_press, stop, st)
    }
}

#[cfg(windows)]
fn run(bindings: &[Binding], on_press: &Arc<dyn Fn(u32) + Send + Sync>, stop: &AtomicBool, st: &Status) -> Result<(), String> {
    win::run(bindings, on_press, stop, st)
}

#[cfg(not(any(unix, windows)))]
fn run(_: &[Binding], _: &Arc<dyn Fn(u32) + Send + Sync>, _: &AtomicBool, _: &Status) -> Result<(), String> {
    Err("este sistema não tem atalho global; vale só com a janela em primeiro plano".into())
}

/// Wayland: o portal `GlobalShortcuts`. A tecla de verdade é a que o ambiente de trabalho confirmar.
#[cfg(unix)]
mod portal {
    use super::*;
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures::StreamExt;
    use std::time::Duration;

    pub(super) fn run(bindings: &[Binding], on_press: &Arc<dyn Fn(u32) + Send + Sync>, stop: &AtomicBool, st: &Status) -> Result<(), String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let err = |e: ashpd::Error| format!("o ambiente de trabalho não oferece atalhos globais (portal GlobalShortcuts): {e}");
            let proxy = GlobalShortcuts::new().await.map_err(err)?;
            let session = proxy.create_session(Default::default()).await.map_err(err)?;
            let triggers: Vec<String> = bindings.iter().map(|b| b.combo.xdg_trigger()).collect();
            let news: Vec<NewShortcut> = bindings.iter().zip(&triggers).map(|(b, t)| NewShortcut::new(b.id.to_string(), b.label.clone()).preferred_trigger(Some(t.as_str()))).collect();
            proxy.bind_shortcuts(&session, &news, None, Default::default()).await.map_err(err)?.response().map_err(err)?;
            let mut pressed = proxy.receive_activated().await.map_err(err)?;
            st.ok();
            while !stop.load(Ordering::Relaxed) {
                match tokio::time::timeout(Duration::from_millis(100), pressed.next()).await {
                    Ok(Some(a)) => {
                        if let Ok(id) = a.shortcut_id().parse() {
                            on_press(id);
                        }
                    }
                    Ok(None) => return Err("o portal de atalhos fechou".into()),
                    Err(_) => {}
                }
            }
            let _ = session.close().await;
            Ok(())
        })
    }
}

/// X11: toma a tecla da janela raiz. Funciona também com as travas de maiúscula e de número ligadas.
#[cfg(unix)]
mod x11 {
    use super::*;
    use std::time::Duration;
    use x11rb::{
        connection::Connection,
        protocol::{
            Event,
            xproto::{ConnectionExt, GrabMode, ModMask},
        },
    };

    fn keysym(key: Key) -> u32 {
        match key {
            Key::Letter(c) => u32::from(c),
            Key::Digit(d) => 0x30 + u32::from(d),
            Key::F(n) => 0xffbd + u32::from(n),
        }
    }

    fn mask(c: &Combo) -> u16 {
        let mut m = 0;
        for (on, bit) in [(c.shift, ModMask::SHIFT), (c.ctrl, ModMask::CONTROL), (c.alt, ModMask::M1)] {
            if on {
                m |= u16::from(bit);
            }
        }
        m
    }

    pub(super) fn run(bindings: &[Binding], on_press: &Arc<dyn Fn(u32) + Send + Sync>, stop: &AtomicBool, st: &Status) -> Result<(), String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("sem servidor X para os atalhos globais: {e}"))?;
        let setup = conn.setup();
        let root = setup.roots.get(screen).ok_or("sem tela X")?.root;
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = conn.get_keyboard_mapping(min, max - min + 1).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        let per = usize::from(map.keysyms_per_keycode).max(1);
        let keycode = |sym: u32| (min..=max).find(|kc| map.keysyms.chunks(per).nth(usize::from(*kc - min)).is_some_and(|row| row.contains(&sym)));
        // Com CapsLock (Lock) ou NumLock (Mod2) ligados o X entrega a tecla com a trava no estado: pede todas as variações.
        let (lock, num) = (u16::from(ModMask::LOCK), u16::from(ModMask::M2));
        let mut grabbed = Vec::new();
        for b in bindings {
            let kc = keycode(keysym(b.combo.key)).ok_or_else(|| format!("o teclado não tem a tecla de {}", b.combo.xdg_trigger()))?;
            for extra in [0, lock, num, lock | num] {
                conn.grab_key(false, root, ModMask::from(mask(&b.combo) | extra), kc, GrabMode::ASYNC, GrabMode::ASYNC)
                    .map_err(|e| e.to_string())?
                    .check()
                    .map_err(|_| format!("outro programa já usa {}", b.combo.xdg_trigger()))?;
            }
            grabbed.push((kc, mask(&b.combo), b.id));
        }
        st.ok();
        let relevant = mask(&Combo { ctrl: true, alt: true, shift: true, key: Key::F(1) });
        while !stop.load(Ordering::Relaxed) {
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(e))) => {
                    let held = u16::from(e.state) & relevant;
                    if let Some((_, _, id)) = grabbed.iter().find(|(kc, m, _)| *kc == e.detail && *m == held) {
                        on_press(*id);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => return Err(format!("a conexão X dos atalhos caiu: {e}")),
            }
        }
        Ok(())
    }
}

/// Windows: `RegisterHotKey` na fila de mensagens desta thread.
#[cfg(windows)]
#[allow(unsafe_code)] // FFI do Win32; cada bloco `unsafe` diz por que vale.
mod win {
    use super::*;
    use std::time::Duration;
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey},
        WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY},
    };

    fn vk(key: Key) -> u32 {
        match key {
            Key::Letter(c) => u32::from(c.to_ascii_uppercase()),
            Key::Digit(d) => 0x30 + u32::from(d),
            Key::F(n) => 0x70 + u32::from(n) - 1,
        }
    }

    pub(super) fn run(bindings: &[Binding], on_press: &Arc<dyn Fn(u32) + Send + Sync>, stop: &AtomicBool, st: &Status) -> Result<(), String> {
        let mut done = Vec::new();
        for b in bindings {
            let mut mods = MOD_NOREPEAT;
            for (on, bit) in [(b.combo.ctrl, MOD_CONTROL), (b.combo.alt, MOD_ALT), (b.combo.shift, MOD_SHIFT)] {
                if on {
                    mods |= bit;
                }
            }
            // SAFETY: HWND nulo registra a tecla para esta thread; `id` é um inteiro e `vk` uma tecla virtual.
            let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), b.id as i32, mods, vk(b.combo.key)) };
            if ok == 0 {
                for id in done {
                    // SAFETY: desfaz só o que esta thread registrou acima.
                    unsafe { UnregisterHotKey(std::ptr::null_mut(), id) };
                }
                return Err(format!("outro programa já usa {} ou o sistema recusou", b.combo.xdg_trigger()));
            }
            done.push(b.id as i32);
        }
        st.ok();
        // SAFETY: `MSG` é só dados (inteiros e ponteiros nulos), zerado é um valor válido.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while !stop.load(Ordering::Relaxed) {
            // SAFETY: `msg` é nosso e vive durante a chamada; HWND nulo lê a fila desta thread.
            if unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                if msg.message == WM_HOTKEY {
                    on_press(msg.wParam as u32);
                }
            } else {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        for id in done {
            // SAFETY: desfaz só o que esta thread registrou.
            unsafe { UnregisterHotKey(std::ptr::null_mut(), id) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_read_by_name() {
        assert_eq!(Key::from_name("M"), Some(Key::Letter(b'm')));
        assert_eq!(Key::from_name("7"), Some(Key::Digit(7)));
        assert_eq!(Key::from_name("f12"), Some(Key::F(12)));
        for bad in ["", "F0", "F13", "Enter", "ab", "-"] {
            assert_eq!(Key::from_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_trigger_follows_the_xdg_spelling() {
        let c = Combo { ctrl: true, alt: false, shift: true, key: Key::Letter(b'd') };
        assert_eq!(c.xdg_trigger(), "CTRL+SHIFT+d");
        assert_eq!(Combo { ctrl: false, alt: true, shift: false, key: Key::F(5) }.xdg_trigger(), "ALT+F5");
    }
}
