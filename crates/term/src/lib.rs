//! A terminal emulator with no UI: VT parsing, a grid, scrollback and
//! keystroke encoding. Rendering lives in the app.

mod keys;

pub use keys::{Key, encode_key, encode_paste};

use std::cell::RefCell;
use std::ops::RangeInclusive;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as VteCursor, Processor};

const SCROLLBACK: usize = 10_000;

struct Listener(Rc<RefCell<Option<String>>>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => *self.0.borrow_mut() = Some(title),
            Event::ResetTitle => *self.0.borrow_mut() = None,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Size {
    lines: usize,
    columns: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A colour the renderer can resolve. Named ANSI slots stay named so the
/// view can use Paseo's palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintColor {
    DefaultFg,
    DefaultBg,
    Indexed(u8),
    Rgb(Color),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: PaintColor,
    pub bg: PaintColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCell {
    pub text: String,
    /// Columns this cell occupies. A wide character's spacer is omitted.
    pub width: u8,
    pub style: CellStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotLine {
    pub cells: Vec<SnapshotCell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub hidden: bool,
    pub shape: CursorShape,
}

/// The visible screen, already scrolled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub rows: usize,
    pub cols: usize,
    pub lines: Vec<SnapshotLine>,
    pub cursor: Option<Cursor>,
    /// Lines above the viewport still in the scrollback.
    pub scrollback: usize,
    /// How far the viewport is scrolled up from the live end.
    pub scroll_offset: usize,
}

/// A point in the full buffer: `line` 0 is the oldest scrollback row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferPoint {
    pub line: usize,
    pub col: usize,
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    rows: usize,
    cols: usize,
    selection: Option<(BufferPoint, BufferPoint)>,
    title: Rc<RefCell<Option<String>>>,
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let title = Rc::new(RefCell::new(None));
        let term = Term::new(
            config,
            &Size {
                lines: rows,
                columns: cols,
            },
            Listener(title.clone()),
        );
        Self {
            term,
            parser: Processor::new(),
            rows,
            cols,
            selection: None,
            title,
        }
    }

    /// Title set by OSC 0 / OSC 2, if the shell has sent one.
    pub fn title(&self) -> Option<String> {
        self.title.borrow().clone()
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Full reset (`ESC c`) then replay. Size is restored afterwards because
    /// a reset can drop modes the emulator was sized for.
    pub fn restore(&mut self, bytes: &[u8]) {
        self.feed(b"\x1bc");
        self.resize(self.rows, self.cols);
        self.feed(bytes);
        self.selection = None;
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.term.resize(Size {
            lines: rows,
            columns: cols,
        });
    }

    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Positive `lines` scrolls up into history.
    pub fn scroll(&mut self, lines: i32) {
        if lines == 0 {
            return;
        }
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
    }

    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn encode(&self, key: &Key) -> Vec<u8> {
        encode_key(key, self.app_cursor())
    }

    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        encode_paste(text, self.bracketed_paste())
    }

    pub fn select(&mut self, anchor: BufferPoint, focus: BufferPoint) {
        self.selection = Some((anchor, focus));
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn selection(&self) -> Option<(BufferPoint, BufferPoint)> {
        self.selection
    }

    /// Selected text, rows joined by newlines, trailing spaces trimmed.
    pub fn selected_text(&self) -> String {
        let Some((a, b)) = self.selection else {
            return String::new();
        };
        let (start, end) = ordered(a, b);
        let total = self.buffer_lines();
        let mut lines = Vec::new();
        for line in start.line..=end.line.min(total.saturating_sub(1)) {
            let row = self.buffer_line(line);
            let from = if line == start.line { start.col } else { 0 };
            let to = if line == end.line {
                end.col.saturating_add(1)
            } else {
                self.cols
            };
            let mut text = String::new();
            let mut col = 0;
            for cell in &row.cells {
                let next = col + cell.width as usize;
                if next > from && col < to {
                    text.push_str(&cell.text);
                }
                col = next;
            }
            lines.push(trim_trailing_space(&text));
        }
        lines.join("\n")
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut lines = Vec::with_capacity(self.rows);
        for row in 0..self.rows {
            lines.push(self.visible_line(row));
        }
        let cursor = self.cursor();
        let offset = self.scroll_offset();
        Snapshot {
            rows: self.rows,
            cols: self.cols,
            lines,
            cursor,
            scrollback: self.history_len(),
            scroll_offset: offset,
        }
    }

    /// Map a viewport cell to a buffer point (scrollback line 0 is oldest).
    pub fn buffer_point(&self, view_row: usize, col: usize) -> BufferPoint {
        let history = self.history_len();
        let offset = self.scroll_offset();
        let line = history.saturating_sub(offset).saturating_add(view_row);
        BufferPoint { line, col }
    }

    fn cursor(&self) -> Option<Cursor> {
        let cursor = &self.term.grid().cursor;
        let row = cursor.point.line.0 + self.scroll_offset() as i32;
        if row < 0 || row as usize >= self.rows {
            return None;
        }
        let shape = match self.term.cursor_style().shape {
            VteCursor::Block => CursorShape::Block,
            VteCursor::Underline => CursorShape::Underline,
            VteCursor::Beam => CursorShape::Bar,
            VteCursor::HollowBlock | VteCursor::Hidden => CursorShape::Block,
        };
        Some(Cursor {
            row: row as usize,
            col: cursor.point.column.0,
            hidden: !self.term.mode().contains(TermMode::SHOW_CURSOR)
                || matches!(self.term.cursor_style().shape, VteCursor::Hidden),
            shape,
        })
    }

    fn history_len(&self) -> usize {
        self.term.grid().history_size()
    }

    fn buffer_lines(&self) -> usize {
        self.history_len() + self.rows
    }

    fn visible_line(&self, row: usize) -> SnapshotLine {
        let line = Line(row as i32 - self.scroll_offset() as i32);
        self.read_line(line)
    }

    fn buffer_line(&self, index: usize) -> SnapshotLine {
        let history = self.history_len();
        let line = if index < history {
            Line(-(history as i32 - index as i32))
        } else {
            Line((index - history) as i32)
        };
        self.read_line(line)
    }

    fn read_line(&self, line: Line) -> SnapshotLine {
        let grid = self.term.grid();
        let mut cells = Vec::new();
        let mut col = 0;
        while col < self.cols {
            let point = Point::new(line, Column(col));
            let cell = &grid[point];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                col += 1;
                continue;
            }
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            cells.push(snapshot_cell(cell, width));
            col += width as usize;
        }
        SnapshotLine { cells }
    }
}

fn snapshot_cell(cell: &Cell, width: u8) -> SnapshotCell {
    let text = if cell.c == '\0' || cell.c == ' ' && cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        " ".into()
    } else {
        cell.c.to_string()
    };
    SnapshotCell {
        text,
        width,
        style: CellStyle {
            fg: paint_color(cell.fg),
            bg: paint_color(cell.bg),
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            dim: cell.flags.contains(Flags::DIM),
            inverse: cell.flags.contains(Flags::INVERSE),
            strikethrough: cell.flags.contains(Flags::STRIKEOUT),
        },
    }
}

fn paint_color(color: alacritty_terminal::vte::ansi::Color) -> PaintColor {
    use alacritty_terminal::vte::ansi::Color as Ansi;
    use alacritty_terminal::vte::ansi::NamedColor;
    match color {
        Ansi::Named(NamedColor::Background) => PaintColor::DefaultBg,
        Ansi::Named(named) => {
            let index = named as u16;
            if index <= 15 {
                PaintColor::Indexed(index as u8)
            } else {
                PaintColor::DefaultFg
            }
        }
        Ansi::Spec(rgb) => PaintColor::Rgb(Color {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }),
        Ansi::Indexed(index) => PaintColor::Indexed(index),
    }
}

fn ordered(a: BufferPoint, b: BufferPoint) -> (BufferPoint, BufferPoint) {
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn trim_trailing_space(text: &str) -> String {
    text.trim_end_matches(' ').to_string()
}

/// Columns covered by a selection on one viewport row, for painting.
pub fn selection_cols(
    selection: Option<(BufferPoint, BufferPoint)>,
    row_line: usize,
) -> Option<RangeInclusive<usize>> {
    let (start, end) = ordered(selection?.0, selection?.1);
    if row_line < start.line || row_line > end.line {
        return None;
    }
    let from = if row_line == start.line { start.col } else { 0 };
    let to = if row_line == end.line {
        end.col
    } else {
        usize::MAX
    };
    Some(from..=to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(term: &Terminal) -> String {
        term.snapshot()
            .lines
            .iter()
            .map(|line| {
                let mut text = String::new();
                for cell in &line.cells {
                    text.push_str(&cell.text);
                }
                trim_trailing_space(&text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn prints_plain_text() {
        let mut term = Terminal::new(6, 20);
        term.feed(b"hello\r\nworld");
        let text = screen_text(&term);
        assert!(text.starts_with("hello\nworld"), "{text:?}");
        let snap = term.snapshot();
        assert_eq!(snap.cursor.unwrap().row, 1);
        assert_eq!(snap.cursor.unwrap().col, 5);
    }

    #[test]
    fn the_cursor_shows_after_a_prompt_whatever_its_shape() {
        let mut term = Terminal::new(6, 20);
        term.feed(b"~/src % ");
        let cursor = term.snapshot().cursor.unwrap();
        assert_eq!((cursor.row, cursor.col, cursor.hidden), (0, 8, false));
        // A program asking for a bar still has a cursor to draw.
        term.feed(b"\x1b[6 q");
        assert!(!term.snapshot().cursor.unwrap().hidden);
        // Hiding it hides it.
        term.feed(b"\x1b[?25l");
        assert!(term.snapshot().cursor.unwrap().hidden);
    }

    #[test]
    fn records_ansi_colour() {
        let mut term = Terminal::new(4, 20);
        term.feed(b"\x1b[31mRED\x1b[0m");
        let cell = &term.snapshot().lines[0].cells[0];
        assert_eq!(cell.text, "R");
        assert_eq!(cell.style.fg, PaintColor::Indexed(1));
        assert_eq!(
            term.snapshot().lines[0].cells[2].style.fg,
            PaintColor::Indexed(1)
        );
    }

    #[test]
    fn keeps_scrollback() {
        let mut term = Terminal::new(4, 10);
        for n in 1..=20 {
            term.feed(format!("{n}\r\n").as_bytes());
        }
        assert!(
            term.snapshot().scrollback >= 16,
            "{}",
            term.snapshot().scrollback
        );
        term.scroll(100);
        let text = screen_text(&term);
        assert!(text.contains('1'), "{text:?}");
    }

    #[test]
    fn selection_copies_a_span() {
        let mut term = Terminal::new(4, 20);
        term.feed(b"abcdef\r\nghijkl");
        term.select(
            BufferPoint { line: 0, col: 1 },
            BufferPoint { line: 1, col: 2 },
        );
        assert_eq!(term.selected_text(), "bcdef\nghi");
    }

    #[test]
    fn application_cursor_and_bracketed_paste_modes() {
        let mut term = Terminal::new(4, 20);
        assert!(!term.app_cursor());
        term.feed(b"\x1b[?1h");
        assert!(term.app_cursor());
        assert_eq!(term.encode(&Key::up()), b"\x1bOA");
        term.feed(b"\x1b[?1l");
        assert_eq!(term.encode(&Key::up()), b"\x1b[A");
        term.feed(b"\x1b[?2004h");
        let pasted = term.encode_paste("hi");
        assert!(pasted.starts_with(b"\x1b[200~"), "{pasted:?}");
        assert!(pasted.ends_with(b"\x1b[201~"), "{pasted:?}");
    }

    #[test]
    fn restore_replays_a_snapshot() {
        let mut term = Terminal::new(4, 20);
        term.feed(b"stale\r\n");
        term.restore(b"fresh");
        let text = screen_text(&term);
        assert!(text.contains("fresh"), "{text:?}");
        assert!(!text.contains("stale"), "{text:?}");
    }
}
