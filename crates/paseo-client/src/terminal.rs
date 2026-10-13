//! Binary terminal frames and the snapshot/resize payloads they carry.
//!
//! Layout is `[opcode u8][slot u8][payload…]`, the same codec as
//! `packages/protocol/src/binary-frames/terminal.ts`. The reference app
//! sends keystrokes and resizes as JSON `terminal_input`, not as Input or
//! Resize frames; those opcodes are still decoded so a daemon that emits
//! them is not mistaken for a corrupt frame.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const OPCODE_OUTPUT: u8 = 0x01;
pub const OPCODE_INPUT: u8 = 0x02;
pub const OPCODE_RESIZE: u8 = 0x03;
pub const OPCODE_SNAPSHOT: u8 = 0x04;
pub const OPCODE_RESTORE: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Output,
    Input,
    Resize,
    Snapshot,
    Restore,
}

impl Opcode {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            OPCODE_OUTPUT => Some(Self::Output),
            OPCODE_INPUT => Some(Self::Input),
            OPCODE_RESIZE => Some(Self::Resize),
            OPCODE_SNAPSHOT => Some(Self::Snapshot),
            OPCODE_RESTORE => Some(Self::Restore),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Output => OPCODE_OUTPUT,
            Self::Input => OPCODE_INPUT,
            Self::Resize => OPCODE_RESIZE,
            Self::Snapshot => OPCODE_SNAPSHOT,
            Self::Restore => OPCODE_RESTORE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub opcode: Opcode,
    pub slot: u8,
    pub payload: Vec<u8>,
}

pub fn encode_frame(opcode: Opcode, slot: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + payload.len());
    bytes.push(opcode.as_u8());
    bytes.push(slot);
    bytes.extend_from_slice(payload);
    bytes
}

pub fn decode_frame(bytes: &[u8]) -> Option<Frame> {
    if bytes.len() < 2 {
        return None;
    }
    let opcode = Opcode::from_u8(bytes[0])?;
    Some(Frame {
        opcode,
        slot: bytes[1],
        payload: bytes[2..].to_vec(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResizeIntent {
    Claim,
    Update,
}

/// Resize payload. Unknown keys are ignored, matching the non-strict schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizePayload {
    pub rows: u32,
    pub cols: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<ResizeIntent>,
}

pub fn encode_resize(payload: &ResizePayload) -> Vec<u8> {
    serde_json::to_vec(payload).unwrap_or_default()
}

pub fn decode_resize(bytes: &[u8]) -> Option<ResizePayload> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let rows = value.get("rows")?.as_u64()?;
    let cols = value.get("cols")?.as_u64()?;
    if rows == 0 || cols == 0 || rows > u32::MAX as u64 || cols > u32::MAX as u64 {
        return None;
    }
    let intent = match value.get("intent").and_then(Value::as_str) {
        Some("claim") => Some(ResizeIntent::Claim),
        Some("update") => Some(ResizeIntent::Update),
        None => None,
        Some(_) => return None,
    };
    Some(ResizePayload {
        rows: rows as u32,
        cols: cols as u32,
        intent,
    })
}

/// One cell of a daemon snapshot. Extra fields are dropped on parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    #[serde(default)]
    pub char: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strikethrough: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub row: i32,
    pub col: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<CursorStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blink: Option<bool>,
}

/// `TerminalState` from a Snapshot frame. Wrap flags are optional; only
/// clients that advertised `terminal_reflowable_snapshot` receive them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalState {
    pub rows: u32,
    pub cols: u32,
    pub grid: Vec<Vec<Cell>>,
    pub scrollback: Vec<Vec<Cell>>,
    pub cursor: Cursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_wrapped: Option<Vec<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_wrapped: Option<Vec<bool>>,
}

pub fn encode_snapshot(state: &TerminalState) -> Vec<u8> {
    serde_json::to_vec(state).unwrap_or_default()
}

pub fn decode_snapshot(bytes: &[u8]) -> Option<TerminalState> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    // Require the fields the schema requires; serde would also accept a
    // missing cursor as an error, which is what we want.
    if !value.get("rows")?.is_number()
        || !value.get("cols")?.is_number()
        || !value.get("grid")?.is_array()
        || !value.get("scrollback")?.is_array()
        || !value.get("cursor")?.is_object()
    {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Replay a snapshot as ANSI, the way the reference client paints one:
/// reset is the caller's job (`ESC c`). Soft-wrapped rows are emitted as one
/// logical line so the emulator can reflow them.
pub fn snapshot_to_ansi(state: &TerminalState) -> Vec<u8> {
    let mut rows: Vec<&[Cell]> = Vec::with_capacity(state.scrollback.len() + state.grid.len());
    for row in &state.scrollback {
        rows.push(row);
    }
    for row in &state.grid {
        rows.push(row);
    }
    let mut wraps = Vec::new();
    if let (Some(scroll), Some(grid)) = (&state.scrollback_wrapped, &state.grid_wrapped) {
        wraps.extend(scroll.iter().copied());
        wraps.extend(grid.iter().copied());
    }
    let has_wrap = wraps.len() == rows.len();
    let mut out = String::new();
    if !has_wrap {
        out.push_str("\u{1b}[?7l");
    }
    for (index, row) in rows.iter().enumerate() {
        let continues = has_wrap && wraps[index];
        let pad = if continues {
            Some(state.cols as usize)
        } else {
            None
        };
        render_row(&mut out, row, pad);
        if index + 1 < rows.len() && !continues {
            out.push_str("\r\n");
        }
    }
    out.push_str("\u{1b}[0m");
    if let Some(style) = &state.cursor.style {
        let code = match (style, state.cursor.blink == Some(false)) {
            (CursorStyle::Block, false) => 1,
            (CursorStyle::Block, true) => 2,
            (CursorStyle::Underline, false) => 3,
            (CursorStyle::Underline, true) => 4,
            (CursorStyle::Bar, false) => 5,
            (CursorStyle::Bar, true) => 6,
        };
        out.push_str(&format!("\u{1b}[{code} q"));
    }
    let row = state.cursor.row + 1;
    let col = state.cursor.col + 1;
    out.push_str(&format!("\u{1b}[{row};{col}H"));
    if state.cursor.hidden == Some(true) {
        out.push_str("\u{1b}[?25l");
    } else {
        out.push_str("\u{1b}[?25h");
    }
    if !has_wrap {
        out.push_str("\u{1b}[?7h");
    }
    out.into_bytes()
}

fn render_row(out: &mut String, row: &[Cell], pad_to: Option<usize>) {
    let content = row_len(row);
    let length = pad_to.map(|pad| pad.max(content)).unwrap_or(content);
    let mut previous = Style::default();
    for index in 0..length {
        let cell = row.get(index);
        // An empty char marks a wide character's second column, which the
        // glyph before it already covers: writing anything shifts the row.
        if cell.is_some_and(|c| c.char.is_empty()) {
            continue;
        }
        let style = cell.map(Style::from).unwrap_or_default();
        if style != previous {
            out.push_str(&style.to_sgr());
            previous = style;
        }
        out.push_str(cell.map_or(" ", |c| c.char.as_str()));
    }
    if previous != Style::default() {
        out.push_str("\u{1b}[0m");
    }
}

fn row_len(row: &[Cell]) -> usize {
    for index in (0..row.len()).rev() {
        let cell = &row[index];
        if cell.char != " " && !cell.char.is_empty() {
            return index + 1;
        }
        if cell.fg.is_some()
            || cell.bg.is_some()
            || cell.fg_mode.is_some()
            || cell.bg_mode.is_some()
            || cell.bold == Some(true)
            || cell.italic == Some(true)
            || cell.underline == Some(true)
            || cell.dim == Some(true)
            || cell.inverse == Some(true)
            || cell.strikethrough == Some(true)
        {
            return index + 1;
        }
    }
    0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    fg: Option<u32>,
    bg: Option<u32>,
    fg_mode: Option<u32>,
    bg_mode: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    dim: bool,
    inverse: bool,
    strikethrough: bool,
}

impl Style {
    fn from(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            fg_mode: cell.fg_mode,
            bg_mode: cell.bg_mode,
            bold: cell.bold == Some(true),
            italic: cell.italic == Some(true),
            underline: cell.underline == Some(true),
            dim: cell.dim == Some(true),
            inverse: cell.inverse == Some(true),
            strikethrough: cell.strikethrough == Some(true),
        }
    }

    fn to_sgr(self) -> String {
        let mut codes = vec!["0".to_string()];
        if self.bold {
            codes.push("1".into());
        }
        if self.dim {
            codes.push("2".into());
        }
        if self.italic {
            codes.push("3".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if self.inverse {
            codes.push("7".into());
        }
        if self.strikethrough {
            codes.push("9".into());
        }
        if let (Some(mode), Some(value)) = (self.fg_mode, self.fg) {
            codes.extend(color_sgr(mode, value, false));
        }
        if let (Some(mode), Some(value)) = (self.bg_mode, self.bg) {
            codes.extend(color_sgr(mode, value, true));
        }
        format!("\u{1b}[{}m", codes.join(";"))
    }
}

fn color_sgr(mode: u32, value: u32, background: bool) -> Vec<String> {
    match mode {
        1 if value >= 8 => vec![((if background { 100 } else { 90 }) + (value - 8)).to_string()],
        1 => vec![((if background { 40 } else { 30 }) + value).to_string()],
        2 => vec![
            if background { "48" } else { "38" }.into(),
            "5".into(),
            value.to_string(),
        ],
        3 => vec![
            if background { "48" } else { "38" }.into(),
            "2".into(),
            ((value >> 16) & 0xff).to_string(),
            ((value >> 8) & 0xff).to_string(),
            (value & 0xff).to_string(),
        ],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_output_as_opcode_slot_payload() {
        let payload = b"hello";
        let encoded = encode_frame(Opcode::Output, 7, payload);
        assert_eq!(encoded[0], OPCODE_OUTPUT);
        assert_eq!(encoded[1], 7);
        assert_eq!(&encoded[2..], payload);
        let decoded = decode_frame(&encoded).unwrap();
        assert_eq!(decoded.opcode, Opcode::Output);
        assert_eq!(decoded.slot, 7);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn round_trips_resize_intents() {
        for intent in [Some(ResizeIntent::Claim), Some(ResizeIntent::Update), None] {
            let payload = ResizePayload {
                rows: 24,
                cols: 80,
                intent,
            };
            let bytes = encode_resize(&payload);
            assert_eq!(decode_resize(&bytes), Some(payload));
        }
    }

    #[test]
    fn wide_characters_keep_the_row_in_place() {
        let cell = |c: &str| Cell {
            char: c.into(),
            ..Cell::default()
        };
        let mut out = String::new();
        render_row(&mut out, &[cell("日"), cell(""), cell("x")], None);
        assert_eq!(out, "日x");
    }

    #[test]
    fn round_trips_snapshot() {
        let state = TerminalState {
            rows: 1,
            cols: 2,
            grid: vec![vec![
                Cell {
                    char: "A".into(),
                    ..Cell::default()
                },
                Cell {
                    char: "B".into(),
                    ..Cell::default()
                },
            ]],
            scrollback: vec![],
            cursor: Cursor {
                row: 0,
                col: 2,
                hidden: None,
                style: None,
                blink: None,
            },
            title: None,
            grid_wrapped: None,
            scrollback_wrapped: None,
        };
        let bytes = encode_snapshot(&state);
        assert_eq!(decode_snapshot(&bytes).as_ref(), Some(&state));
    }

    #[test]
    fn rejects_unknown_opcode_and_short_frame() {
        assert!(decode_frame(&[0xff, 0x01, 0x02]).is_none());
        assert!(decode_frame(&[OPCODE_OUTPUT]).is_none());
    }

    #[test]
    fn rejects_malformed_and_invalid_payloads() {
        assert!(decode_resize(b"{").is_none());
        assert!(decode_snapshot(b"{").is_none());
        assert!(decode_resize(br#"{"rows":"24","cols":80}"#).is_none());
        assert!(
            decode_snapshot(br#"{"rows":1,"cols":1,"grid":[[{"char":"A"}]],"scrollback":[]}"#)
                .is_none()
        );
    }

    #[test]
    fn ignores_unknown_fields() {
        let resize = decode_resize(br#"{"rows":24,"cols":80,"extra":true}"#).unwrap();
        assert_eq!(
            resize,
            ResizePayload {
                rows: 24,
                cols: 80,
                intent: None
            }
        );
        let snapshot = decode_snapshot(
            br#"{"rows":1,"cols":1,"grid":[[{"char":"A","extra":true}]],"scrollback":[],"cursor":{"row":0,"col":1}}"#,
        )
        .unwrap();
        assert_eq!(snapshot.grid[0][0].char, "A");
    }

    #[test]
    fn snapshot_ansi_places_the_cursor() {
        let state = TerminalState {
            rows: 1,
            cols: 2,
            grid: vec![vec![
                Cell {
                    char: "A".into(),
                    ..Cell::default()
                },
                Cell {
                    char: "B".into(),
                    ..Cell::default()
                },
            ]],
            scrollback: vec![],
            cursor: Cursor {
                row: 0,
                col: 2,
                hidden: None,
                style: None,
                blink: None,
            },
            title: None,
            grid_wrapped: None,
            scrollback_wrapped: None,
        };
        let ansi = String::from_utf8(snapshot_to_ansi(&state)).unwrap();
        assert!(ansi.contains("AB"));
        assert!(ansi.contains("\u{1b}[1;3H"));
    }
}
