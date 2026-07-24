//! Egui adapter: translates egui events into hxy-editor's neutral
//! [`InputEvent`]s, feeds them through the editor's filter (which
//! decides consumption), and executes returned effects.

use hxy_editor::Disposition;
use hxy_editor::Effect;
use hxy_editor::HexEditor;
use hxy_editor::InputEvent;
use hxy_editor::Key;
use hxy_editor::Modifiers;

fn translate_key(key: egui::Key) -> Option<Key> {
    use egui::Key as K;
    Some(match key {
        K::ArrowLeft => Key::ArrowLeft,
        K::ArrowRight => Key::ArrowRight,
        K::ArrowUp => Key::ArrowUp,
        K::ArrowDown => Key::ArrowDown,
        K::Escape => Key::Escape,
        K::Tab => Key::Tab,
        K::Backspace => Key::Backspace,
        K::Num0 => Key::Digit(0),
        K::Num1 => Key::Digit(1),
        K::Num2 => Key::Digit(2),
        K::Num3 => Key::Digit(3),
        K::Num4 => Key::Digit(4),
        K::Num5 => Key::Digit(5),
        K::Num6 => Key::Digit(6),
        K::Num7 => Key::Digit(7),
        K::Num8 => Key::Digit(8),
        K::Num9 => Key::Digit(9),
        K::A => Key::Letter('a'),
        K::B => Key::Letter('b'),
        K::C => Key::Letter('c'),
        K::D => Key::Letter('d'),
        K::E => Key::Letter('e'),
        K::F => Key::Letter('f'),
        K::G => Key::Letter('g'),
        K::H => Key::Letter('h'),
        K::I => Key::Letter('i'),
        K::J => Key::Letter('j'),
        K::K => Key::Letter('k'),
        K::L => Key::Letter('l'),
        K::M => Key::Letter('m'),
        K::N => Key::Letter('n'),
        K::O => Key::Letter('o'),
        K::P => Key::Letter('p'),
        K::Q => Key::Letter('q'),
        K::R => Key::Letter('r'),
        K::S => Key::Letter('s'),
        K::T => Key::Letter('t'),
        K::U => Key::Letter('u'),
        K::V => Key::Letter('v'),
        K::W => Key::Letter('w'),
        K::X => Key::Letter('x'),
        K::Y => Key::Letter('y'),
        K::Z => Key::Letter('z'),
        _ => return None,
    })
}

fn translate_modifiers(m: egui::Modifiers) -> Modifiers {
    Modifiers { shift: m.shift, command: m.command, alt: m.alt }
}

pub(crate) fn handle_input(editor: &mut HexEditor, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    let mut filter = editor.input_filter();
    ctx.input_mut(|i| {
        i.events.retain(|event| {
            let translated = match event {
                egui::Event::Key { key, pressed: true, modifiers, .. } => translate_key(*key)
                    .map(|k| InputEvent::Key { key: k, modifiers: translate_modifiers(*modifiers) }),
                egui::Event::Text(s) => Some(InputEvent::Text(s.clone())),
                _ => None,
            };
            match translated {
                Some(ev) => filter.feed(&ev) == Disposition::Passed,
                None => true,
            }
        });
    });
    for effect in editor.apply_input(filter.finish()) {
        match effect {
            Effect::CopyText(text) => ctx.copy_text(text),
        }
    }
}
