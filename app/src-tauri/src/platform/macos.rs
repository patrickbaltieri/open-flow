use std::collections::HashSet;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{handle_key, Cmd, HookState, Settings, Shared};

// --- Segredos: no macOS ficam em texto no settings.json (protegido pelas
// permissões do perfil do usuário).
//
// O Keychain foi removido em 2026-07-29: a autorização de leitura de uma entrada
// é amarrada ao hash do binário, então TODO rebuild assinado disparava um pedido
// de senha do keychain "login" — e quando essa senha diverge da senha da conta
// (troca via Apple ID/FileVault), não há o que digitar: o app fica trancado fora
// das próprias chaves. Decisão do Macks: app local, sem senha. O Windows segue
// no DPAPI, que é transparente e nunca pergunta nada.
//
// Devolver None aqui faz encrypt_key gravar o valor puro e decrypt_key descartar
// qualquer "enc:" remanescente — a migração acontece sozinha no primeiro load.
pub fn protect_secret(_name: &str, _plain: &str) -> Option<String> {
    None
}

pub fn unprotect_secret(_name: &str, _stored: &str) -> Option<String> {
    None
}

// --- Tabela de teclas (CGKeyCode, não VK) ---
pub fn modifier_group(name: &str) -> Option<Vec<u32>> {
    match name {
        "CTRL" | "CONTROL" => Some(vec![59, 62]), // L/R Control
        "WIN" | "META" | "SUPER" | "CMD" | "COMMAND" => Some(vec![55, 54]), // L/R Command
        "ALT" | "OPTION" | "OPT" => Some(vec![58, 61]), // L/R Option
        "SHIFT" => Some(vec![56, 60]),            // L/R Shift
        _ => None,
    }
}

/// diagnóstico do Windows; no mac o log de inserção não coleta esses dados
pub fn modifiers_down() -> String {
    "n/d".into()
}

pub fn foreground_window_desc() -> String {
    "n/d".into()
}

/// `None` para nome desconhecido: o atalho inteiro é descartado em vez de virar outra tecla.
/// Códigos kVK_* (posição física no teclado ANSI, não o caractere gerado).
pub fn key_from_name(name: &str) -> Option<u32> {
    let code = match name {
        "A" => 0, "S" => 1, "D" => 2, "F" => 3, "H" => 4, "G" => 5, "Z" => 6, "X" => 7,
        "C" => 8, "V" => 9, "B" => 11, "Q" => 12, "W" => 13, "E" => 14, "R" => 15, "Y" => 16,
        "T" => 17, "O" => 31, "U" => 32, "I" => 34, "P" => 35, "L" => 37, "J" => 38, "K" => 40,
        "N" => 45, "M" => 46,
        "1" => 18, "2" => 19, "3" => 20, "4" => 21, "5" => 23, "6" => 22, "7" => 26, "8" => 28,
        "9" => 25, "0" => 29,
        "F1" => 122, "F2" => 120, "F3" => 99, "F4" => 118, "F5" => 96, "F6" => 97,
        "F7" => 98, "F8" => 100, "F9" => 101, "F10" => 109, "F11" => 103, "F12" => 111,
        "F13" => 105, "F14" => 107, "F15" => 113, "F16" => 106, "F17" => 64, "F18" => 79,
        "F19" => 80, "F20" => 90,
        "NUMPAD0" => 82, "NUMPAD1" => 83, "NUMPAD2" => 84, "NUMPAD3" => 85, "NUMPAD4" => 86,
        "NUMPAD5" => 87, "NUMPAD6" => 88, "NUMPAD7" => 89, "NUMPAD8" => 91, "NUMPAD9" => 92,
        "CAPSLOCK" => 57, "INSERT" | "HELP" => 114, "HOME" => 115, "END" => 119,
        "PAGEUP" => 116, "PAGEDOWN" => 121, "DELETE" => 117, "BACKSPACE" => 51,
        "UP" => 126, "DOWN" => 125, "LEFT" => 123, "RIGHT" => 124,
        "SPACE" => 49, "ENTER" | "RETURN" => 36, "TAB" => 48, "ESC" | "ESCAPE" => 53,
        _ => return None,
    };
    Some(code)
}

// --- FFI CoreGraphics (o crate core-graphics não expõe estes dois) ---
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
}

const HID_STATE: i32 = 1; // kCGEventSourceStateHIDSystemState

fn key_down_now(code: u32) -> bool {
    unsafe { CGEventSourceKeyState(HID_STATE, code as u16) }
}

/// Estado do modificador lido do PRÓPRIO evento FlagsChanged. Perguntar ao
/// sistema (CGEventSourceKeyState) corre com a entrega do evento: na soltura a
/// consulta ainda respondia "pressionado", a tecla nunca saía de `pressed` e o
/// modo hold gravava para sempre (só o Esc parava). None = não é modificador
/// com flag própria (CapsLock, cuja flag é o lock e não a tecla).
fn modifier_down(code: u32, flags: core_graphics::event::CGEventFlags) -> Option<bool> {
    use core_graphics::event::CGEventFlags as F;
    let mask = match code {
        59 | 62 => F::CGEventFlagControl,
        58 | 61 => F::CGEventFlagAlternate,
        55 | 54 => F::CGEventFlagCommand,
        56 | 60 => F::CGEventFlagShift,
        _ => return None,
    };
    Some(flags.contains(mask))
}

static TAP_PORT: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

// --- Hotkey: CGEventTap em modo listen-only. Não consome eventos — a classe de
// bug do ABNT2/rdev não existe por construção (não traduzimos tecla→caractere,
// só usamos keycodes crus). Exige a permissão "Monitoramento de Entrada". ---
pub fn spawn_hotkey_listener(shared: Arc<Shared>, tx: Sender<Cmd>) {
    std::thread::spawn(move || {
        use core_foundation::base::TCFType;
        use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
        use core_graphics::event::{
            CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
            EventField,
        };
        let st = std::cell::RefCell::new(HookState {
            shared,
            tx,
            pressed: HashSet::new(),
            latched: None,
            recording: None,
        });
        let own_pid = std::process::id() as i64;
        println!("[hotkey] instalando event tap global...");
        let tap = match CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![CGEventType::KeyDown, CGEventType::KeyUp, CGEventType::FlagsChanged],
            move |_proxy, etype, event| {
                match etype {
                    // o macOS desativa taps "lentos" (típico após sleep/wake); sem
                    // re-habilitar, o atalho morre em silêncio até reiniciar o app
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                        let port = TAP_PORT.load(Ordering::SeqCst);
                        if !port.is_null() {
                            eprintln!("[hotkey] event tap desativado pelo sistema; re-habilitando");
                            unsafe { CGEventTapEnable(port, true) };
                        }
                    }
                    CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                        // ignora eventos sintéticos do próprio app (o Cmd+V da inserção)
                        if event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID)
                            != own_pid
                        {
                            let code = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                as u32;
                            let down = match etype {
                                CGEventType::KeyDown => true,
                                CGEventType::KeyUp => false,
                                // FlagsChanged não diz down/up: lê a flag do evento
                                _ => modifier_down(code, event.get_flags())
                                    .unwrap_or_else(|| key_down_now(code)),
                            };
                            handle_key(&mut st.borrow_mut(), code, down);
                        }
                    }
                    _ => {}
                }
                None
            },
        ) {
            Ok(t) => t,
            Err(()) => {
                eprintln!(
                    "[hotkey] CGEventTap falhou — conceda Monitoramento de Entrada ao OpenFlow \
                     em Ajustes do Sistema > Privacidade e Segurança e reinicie o app"
                );
                return;
            }
        };
        TAP_PORT.store(
            tap.mach_port.as_concrete_TypeRef() as *mut std::ffi::c_void,
            Ordering::SeqCst,
        );
        let source = match tap.mach_port.create_runloop_source(0) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("[hotkey] falha ao criar runloop source do event tap");
                return;
            }
        };
        unsafe {
            CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
        }
        tap.enable();
        CFRunLoop::run_current();
    });
}

// --- Inserção ---

/// espera Cmd/Option/Ctrl/Shift físicos soltarem: modificador físico + V sintético
/// formaria combos do sistema (Ctrl+Cmd+V, Cmd+Opt+V etc.)
pub fn wait_modifiers_released(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline
        && [55u32, 54, 58, 61, 59, 62, 56, 60].iter().any(|&k| key_down_now(k))
    {
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Cmd+V com keycode cru (kVK_ANSI_V = 9), independe de layout — mesmo espírito
/// da regra do VK_V no Windows. Exige a permissão "Acessibilidade".
pub fn paste_shortcut() -> Result<(), String> {
    use enigo::{Direction, Enigo, Key, Keyboard};
    let mut enigo = Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
    enigo.key(Key::Meta, Direction::Press).map_err(|e| e.to_string())?;
    enigo.key(Key::Other(9), Direction::Click).map_err(|e| e.to_string())?;
    enigo.key(Key::Meta, Direction::Release).map_err(|e| e.to_string())?;
    Ok(())
}

// --- Sidecar (sem CREATE_NO_WINDOW: não há janela de console no mac) ---
pub fn spawn_sidecar(s: &Settings) -> Option<std::process::Child> {
    if !std::path::Path::new(&s.python).exists() || !std::path::Path::new(&s.sidecar).exists() {
        println!("[sidecar] transcrição local não configurada — operando só em nuvem (Groq)");
        return None;
    }
    match std::process::Command::new(&s.python)
        .arg(&s.sidecar)
        .env("OPENFLOW_STT_PORT", s.stt_port.to_string())
        .env("OPENFLOW_PARENT_PID", std::process::id().to_string())
        .spawn()
    {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[sidecar] falha ao iniciar ({e}) — inicie manualmente: {} {}", s.python, s.sidecar);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::modifier_down;
    use core_graphics::event::CGEventFlags as F;

    #[test]
    fn soltura_de_modificador_vira_down_false() {
        let ambos = F::CGEventFlagControl | F::CGEventFlagAlternate;
        assert_eq!(modifier_down(59, ambos), Some(true));
        assert_eq!(modifier_down(58, ambos), Some(true));
        // soltou o Control, Option segue pressionado: é aqui que o hold travava
        assert_eq!(modifier_down(59, F::CGEventFlagAlternate), Some(false));
        assert_eq!(modifier_down(58, F::CGEventFlagAlternate), Some(true));
        assert_eq!(modifier_down(62, F::empty()), Some(false));
        // CapsLock não tem flag de tecla: cai no fallback
        assert_eq!(modifier_down(57, F::empty()), None);
    }
}
