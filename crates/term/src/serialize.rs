//! The screen as bytes that draw it again: Paseo's daemon's `SerializeAddon`
//! snapshot, for a client attaching to a running terminal. Rows of styled
//! text (history first, a wrapped row left to wrap), the alternate screen
//! when a program has it, then the cursor, the modes the program set
//! (cursor keys, keypad, paste, mouse, focus, wrapping), the cursor's shape
//! and the title. It assumes the receiving emulator is the same size and
//! has just been reset.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};

/// The flags a pen carries from cell to cell.
const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl Pen {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE,
        }
    }

    fn sgr(&self, out: &mut Vec<u8>) {
        let mut codes = vec!["0".to_string()];
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if self.flags.contains(flag) {
                codes.push(code.into());
            }
        }
        if self.flags.contains(Flags::DOUBLE_UNDERLINE) {
            codes.push("21".into());
        } else if self.flags.intersects(Flags::ALL_UNDERLINES) {
            codes.push("4".into());
        }
        codes.extend(color(self.fg, 30, 90, 38));
        codes.extend(color(self.bg, 40, 100, 48));
        out.extend(format!("\x1b[{}m", codes.join(";")).as_bytes());
    }
}

/// The SGR parameters for a colour: `base` for the first eight named ones,
/// `bright` for the next eight, `extended` (38 or 48) for the others. None
/// for the default.
fn color(color: Color, base: u16, bright: u16, extended: u16) -> Option<String> {
    match color {
        Color::Named(named) => {
            let index = named as u16;
            match index {
                0..8 => Some((base + index).to_string()),
                8..16 => Some((bright + index - 8).to_string()),
                _ => None,
            }
        }
        Color::Indexed(index) => Some(format!("{extended};5;{index}")),
        Color::Spec(rgb) => Some(format!("{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b)),
    }
}

/// A cell that draws as nothing: a space or an empty cell with no colour
/// behind it and nothing that would show.
fn blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\0')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.zerowidth().is_none()
}

pub(crate) fn screen<T: EventListener>(term: &Term<T>, scrollback: usize) -> Vec<u8> {
    let grid = term.grid();
    let mode = *term.mode();
    let alt = mode.contains(TermMode::ALT_SCREEN);
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let history = if alt {
        0
    } else {
        grid.history_size().min(scrollback)
    };
    let mut out = Vec::new();
    if alt {
        out.extend(b"\x1b[?1049h\x1b[H");
    }
    let mut pen = Pen::default();
    let last = rows as i32 - 1;
    // The row before wrapped: the receiver's cursor waits at its end, and
    // only a character written moves it on to this row.
    let mut after_wrap = false;
    for line in -(history as i32)..=last {
        let row = &grid[Line(line)];
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let mut end = if wrapped {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&col| !blank(&row[Column(col)]))
                .map_or(0, |col| col + 1)
        };
        if after_wrap && end == 0 {
            end = 1;
        }
        after_wrap = wrapped;
        for col in 0..end {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let want = Pen::of(cell);
            if want != pen {
                want.sgr(&mut out);
                pen = want;
            }
            let c = if cell.c == '\0' { ' ' } else { cell.c };
            let mut buffer = [0u8; 4];
            out.extend(c.encode_utf8(&mut buffer).as_bytes());
            for mark in cell.zerowidth().into_iter().flatten() {
                out.extend(mark.encode_utf8(&mut buffer).as_bytes());
            }
        }
        if line != last && !wrapped {
            if pen != Pen::default() {
                out.extend(b"\x1b[0m");
                pen = Pen::default();
            }
            out.extend(b"\r\n");
        }
    }
    out.extend(b"\x1b[0m");
    let cursor = grid.cursor.point;
    out.extend(format!("\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes());
    let modes: [(TermMode, &[u8]); 9] = [
        (TermMode::APP_CURSOR, b"\x1b[?1h"),
        (TermMode::APP_KEYPAD, b"\x1b="),
        (TermMode::BRACKETED_PASTE, b"\x1b[?2004h"),
        (TermMode::FOCUS_IN_OUT, b"\x1b[?1004h"),
        (TermMode::MOUSE_REPORT_CLICK, b"\x1b[?1000h"),
        (TermMode::MOUSE_DRAG, b"\x1b[?1002h"),
        (TermMode::MOUSE_MOTION, b"\x1b[?1003h"),
        (TermMode::SGR_MOUSE, b"\x1b[?1006h"),
        (TermMode::UTF8_MOUSE, b"\x1b[?1005h"),
    ];
    for (flag, set) in modes {
        if mode.contains(flag) {
            out.extend(set);
        }
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        out.extend(b"\x1b[?7l");
    }
    let style = term.cursor_style();
    let shape = match (style.shape, style.blinking) {
        (CursorShape::Underline, true) => Some(3),
        (CursorShape::Underline, false) => Some(4),
        (CursorShape::Beam, true) => Some(5),
        (CursorShape::Beam, false) => Some(6),
        _ => None,
    };
    if let Some(shape) = shape {
        out.extend(format!("\x1b[{shape} q").as_bytes());
    }
    out.extend(if mode.contains(TermMode::SHOW_CURSOR) {
        &b"\x1b[?25h"[..]
    } else {
        &b"\x1b[?25l"[..]
    });
    out
}
