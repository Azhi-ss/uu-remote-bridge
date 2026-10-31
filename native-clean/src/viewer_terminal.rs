//! Terminal ownership, viewport geometry and local-event to UU input mapping.
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
};

pub struct TerminalView;

impl TerminalView {
    pub fn check() -> Result<(), String> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(
                "desktop-view requires an interactive terminal; use desktop-snapshot for a file"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn enter() -> Result<Self, String> {
        Self::check()?;
        enable_raw_mode().map_err(|e| e.to_string())?;
        // Construct the guard before any fallible output, so partial setup restores raw mode too.
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )
        .map_err(|e| e.to_string())?;
        Ok(guard)
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(
            out,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            crossterm::style::ResetColor,
            Show,
            LeaveAlternateScreen
        );
        let _ = out.flush();
        let _ = disable_raw_mode();
    }
}

/// One terminal cell contains two vertically stacked source pixels. Keep drawing
/// and mouse mapping on the exact same viewport, leaving the final row for help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub columns: usize,
    pub rows: usize,
}

impl Viewport {
    pub fn fit(width: usize, height: usize, requested: usize, terminal: (u16, u16)) -> Self {
        let max_rows = usize::from(terminal.1).saturating_sub(1).max(1);
        let max_columns = usize::from(terminal.0).max(1).min(requested.clamp(1, 512));
        let columns = max_columns
            .min(max_rows * 2 * width.max(1) / height.max(1))
            .max(1);
        let rows = (height * columns)
            .div_ceil(width.max(1) * 2)
            .clamp(1, max_rows);
        Self { columns, rows }
    }

    fn position(self, column: u16, row: u16) -> (f64, f64) {
        // The center of the terminal cell represents the remote point.
        (
            ((f64::from(column) + 0.5) / self.columns as f64).clamp(0., 1.),
            ((f64::from(row) + 0.5) / self.rows as f64).clamp(0., 1.),
        )
    }

    fn contains(self, column: u16, row: u16) -> bool {
        usize::from(column) < self.columns && usize::from(row) < self.rows
    }
}

#[derive(Default)]
pub struct InputState {
    buttons: BTreeSet<u32>,
    mouse_modifiers: BTreeSet<u32>,
    win_prefix: bool,
}

#[derive(Default)]
pub struct InputBatch {
    pub events: Vec<Value>,
    pub exit: bool,
}

impl InputState {
    pub fn handle(&mut self, event: Event, viewport: Option<Viewport>) -> InputBatch {
        let mut batch = InputBatch::default();
        match event {
            Event::FocusLost => batch.events = self.release_all(),
            Event::Paste(text) => {
                self.win_prefix = false;
                // Keep each control message small without splitting a UTF-8 codepoint.
                let chars: Vec<char> = text.chars().collect();
                for chunk in chars.chunks(1024) {
                    batch.events.push(
                        json!({"action":"text_input", "content":chunk.iter().collect::<String>()}),
                    );
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                // Crossterm represents legacy byte 0x1d (Ctrl+]) as Ctrl+5.
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char(']' | '5'))
                {
                    batch.exit = true;
                    return batch;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
                    self.win_prefix = !self.win_prefix;
                    return batch;
                }
                let win = std::mem::take(&mut self.win_prefix);
                if win && key.code == KeyCode::Esc {
                    return batch;
                }
                batch.events = self.key(key, win);
            }
            Event::Mouse(mouse) => {
                let Some(view) = viewport else {
                    return batch;
                };
                let inside = view.contains(mouse.column, mouse.row);
                // A release outside the image must still release a drag started inside.
                if !inside && self.buttons.is_empty() {
                    return batch;
                }
                let (x, y) = view.position(mouse.column, mouse.row);
                batch
                    .events
                    .push(json!({"action":"mouse_move_absolute","abs_x":x,"abs_y":y}));
                match mouse.kind {
                    MouseEventKind::Down(button) if inside => {
                        self.set_mouse_modifiers(mouse.modifiers, &mut batch.events);
                        let button = button_code(button);
                        if self.buttons.insert(button) {
                            batch
                                .events
                                .push(json!({"action":"mouse_press","button":button}));
                        }
                    }
                    MouseEventKind::Up(button) => {
                        let button = button_code(button);
                        if self.buttons.remove(&button) {
                            batch
                                .events
                                .push(json!({"action":"mouse_release","button":button}));
                        }
                        if self.buttons.is_empty() {
                            self.set_mouse_modifiers(KeyModifiers::empty(), &mut batch.events);
                        }
                    }
                    MouseEventKind::Drag(_) => {
                        self.set_mouse_modifiers(mouse.modifiers, &mut batch.events)
                    }
                    MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
                        if inside =>
                    {
                        let (x, y) = match mouse.kind {
                            MouseEventKind::ScrollUp => (0, 120),
                            MouseEventKind::ScrollDown => (0, -120),
                            MouseEventKind::ScrollLeft => (-120, 0),
                            _ => (120, 0),
                        };
                        let owned: Vec<u32> = modifier_codes(mouse.modifiers)
                            .difference(&self.mouse_modifiers)
                            .copied()
                            .collect();
                        batch
                            .events
                            .extend(owned.iter().map(|&vk| key_action(vk, true)));
                        batch
                            .events
                            .push(json!({"action":"mouse_scroll","delta_x":x,"delta_y":y}));
                        batch
                            .events
                            .extend(owned.iter().rev().map(|&vk| key_action(vk, false)));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        batch
    }

    fn key(&self, key: KeyEvent, win: bool) -> Vec<Value> {
        let mut modifiers = modifier_codes(key.modifiers);
        if key.code == KeyCode::BackTab {
            modifiers.insert(16);
        }
        if win {
            modifiers.insert(91);
        }
        let shortcut = modifiers.iter().any(|vk| matches!(vk, 17 | 18 | 91));
        if let KeyCode::Char(ch) = key.code {
            if !shortcut {
                return vec![json!({"action":"text_input","content":ch.to_string()})];
            }
        }
        let Some(vk) = virtual_key(key.code) else {
            return vec![];
        };
        // Traditional terminals only report keystrokes, not physical key-up. Send
        // complete chords (also on repeat), so a missing release cannot stick a key.
        let owned: Vec<u32> = modifiers
            .difference(&self.mouse_modifiers)
            .copied()
            .collect();
        let mut events: Vec<Value> = owned.iter().map(|&vk| key_action(vk, true)).collect();
        events.push(key_action(vk, true));
        events.push(key_action(vk, false));
        events.extend(owned.iter().rev().map(|&vk| key_action(vk, false)));
        events
    }

    fn set_mouse_modifiers(&mut self, flags: KeyModifiers, events: &mut Vec<Value>) {
        let next = modifier_codes(flags);
        events.extend(
            self.mouse_modifiers
                .difference(&next)
                .map(|&vk| key_action(vk, false)),
        );
        events.extend(
            next.difference(&self.mouse_modifiers)
                .map(|&vk| key_action(vk, true)),
        );
        self.mouse_modifiers = next;
    }

    pub fn release_all(&mut self) -> Vec<Value> {
        self.win_prefix = false;
        let mut events: Vec<Value> = std::mem::take(&mut self.buttons)
            .into_iter()
            .map(|button| json!({"action":"mouse_release","button":button}))
            .collect();
        events.extend(
            std::mem::take(&mut self.mouse_modifiers)
                .into_iter()
                .map(|vk| key_action(vk, false)),
        );
        events
    }
}

fn key_action(vk: u32, down: bool) -> Value {
    json!({"action":if down {"kbd_press"} else {"kbd_release"},"key":vk})
}

fn modifier_codes(flags: KeyModifiers) -> BTreeSet<u32> {
    [
        (KeyModifiers::SHIFT, 16),
        (KeyModifiers::CONTROL, 17),
        (KeyModifiers::ALT, 18),
        (KeyModifiers::SUPER, 91),
    ]
    .into_iter()
    .filter_map(|(flag, vk)| flags.contains(flag).then_some(vk))
    .collect()
}

fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
    }
}

fn virtual_key(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::Backspace => 8,
        KeyCode::Tab | KeyCode::BackTab => 9,
        KeyCode::Enter => 13,
        KeyCode::Esc => 27,
        KeyCode::PageUp => 33,
        KeyCode::PageDown => 34,
        KeyCode::End => 35,
        KeyCode::Home => 36,
        KeyCode::Left => 37,
        KeyCode::Up => 38,
        KeyCode::Right => 39,
        KeyCode::Down => 40,
        KeyCode::Insert => 45,
        KeyCode::Delete => 46,
        KeyCode::F(n @ 1..=24) => 111 + u32::from(n),
        KeyCode::Char(' ') => 32,
        KeyCode::Char(c) if c.is_ascii_alphanumeric() => u32::from(c.to_ascii_uppercase()),
        KeyCode::Char(c) => match c {
            '!' => 49,
            '@' => 50,
            '#' => 51,
            '$' => 52,
            '%' => 53,
            '^' => 54,
            '&' => 55,
            '*' => 56,
            '(' => 57,
            ')' => 48,
            ';' | ':' => 186,
            '=' | '+' => 187,
            ',' | '<' => 188,
            '-' | '_' => 189,
            '.' | '>' => 190,
            '/' | '?' => 191,
            '`' | '~' => 192,
            '[' | '{' => 219,
            '\\' | '|' => 220,
            ']' | '}' => 221,
            '\'' | '"' => 222,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::MouseEvent;
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::CONTROL,
        })
    }

    #[test]
    fn viewport_fits_terminal_and_shares_mouse_coordinates() {
        let view = Viewport::fit(2816, 1760, 200, (100, 30));
        assert_eq!(
            view,
            Viewport {
                columns: 92,
                rows: 29
            }
        );
        assert!(!view.contains(92, 28));
        assert!(!view.contains(40, 29));
        assert_eq!(
            Viewport::fit(1, 1000, 100, (0, 0)),
            Viewport {
                columns: 1,
                rows: 1
            }
        );
    }

    #[test]
    fn ctrl_c_is_remote_and_only_ctrl_bracket_exits() {
        let mut state = InputState::default();
        let batch = state.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL), None);
        assert!(!batch.exit);
        assert_eq!(
            batch.events,
            vec![
                key_action(17, true),
                key_action(67, true),
                key_action(67, false),
                key_action(17, false)
            ]
        );
        assert!(
            state
                .handle(key(KeyCode::Char(']'), KeyModifiers::CONTROL), None)
                .exit
        );
        assert!(
            state
                .handle(key(KeyCode::Char('5'), KeyModifiers::CONTROL), None)
                .exit
        );
    }

    #[test]
    fn windows_prefix_and_unicode_paste() {
        let mut state = InputState::default();
        state.handle(key(KeyCode::Char('g'), KeyModifiers::CONTROL), None);
        assert_eq!(
            state
                .handle(key(KeyCode::Char('r'), KeyModifiers::empty()), None)
                .events,
            vec![
                key_action(91, true),
                key_action(82, true),
                key_action(82, false),
                key_action(91, false)
            ]
        );
        assert_eq!(
            state.handle(Event::Paste("你好\n🙂".into()), None).events[0]["content"],
            "你好\n🙂"
        );
        assert_eq!(
            state
                .handle(key(KeyCode::Char('A'), KeyModifiers::SHIFT), None)
                .events[0]["content"],
            "A"
        );
    }

    #[test]
    fn drag_releases_outside_viewport_and_on_focus_loss() {
        let mut state = InputState::default();
        let view = Some(Viewport {
            columns: 100,
            rows: 30,
        });
        assert!(state
            .handle(
                mouse(MouseEventKind::Down(MouseButton::Left), 120, 35),
                view
            )
            .events
            .is_empty());
        state.handle(mouse(MouseEventKind::Down(MouseButton::Left), 10, 10), view);
        let batch = state.handle(mouse(MouseEventKind::Up(MouseButton::Left), 120, 35), view);
        assert_eq!(batch.events[0]["abs_x"], 1.0);
        assert_eq!(
            batch.events[1],
            json!({"action":"mouse_release","button":1})
        );
        assert_eq!(batch.events[2], key_action(17, false));
        state.handle(
            mouse(MouseEventKind::Down(MouseButton::Right), 10, 10),
            view,
        );
        assert_eq!(state.handle(Event::FocusLost, view).events.len(), 2);
        assert!(state.release_all().is_empty());
    }

    #[test]
    fn resize_changes_hit_testing_and_scroll_uses_windows_units() {
        let mut state = InputState::default();
        let view = Some(Viewport::fit(2816, 1760, 200, (80, 24)));
        let batch = state.handle(mouse(MouseEventKind::ScrollDown, 10, 10), view);
        assert_eq!(
            batch.events[2],
            json!({"action":"mouse_scroll","delta_x":0,"delta_y":-120})
        );
        assert_eq!(batch.events[1], key_action(17, true));
        assert_eq!(batch.events[3], key_action(17, false));
        assert!(state
            .handle(mouse(MouseEventKind::Down(MouseButton::Left), 90, 10), view)
            .events
            .is_empty());
    }
}
