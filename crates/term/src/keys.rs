//! Keystroke to PTY bytes. Arrows follow DECCKM (application cursor mode);
//! paste follows bracketed-paste mode when the shell asked for it.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub fn plain(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    pub fn up() -> Self {
        Self::plain("up")
    }
}

pub fn encode_key(key: &Key, app_cursor: bool) -> Vec<u8> {
    let name = key.key.to_ascii_lowercase();
    if let Some(bytes) = special(&name, key, app_cursor) {
        return bytes;
    }
    if name.chars().count() == 1 {
        let ch = name.chars().next().unwrap();
        return encode_char(ch, key);
    }
    Vec::new()
}

pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    out.extend_from_slice(text.as_bytes());
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

fn encode_char(ch: char, key: &Key) -> Vec<u8> {
    if key.ctrl && ch.is_ascii_alphabetic() {
        let byte = (ch.to_ascii_lowercase() as u8) & 0x1f;
        return prefix_alt(key.alt, &[byte]);
    }
    if key.ctrl && ch == ' ' {
        return prefix_alt(key.alt, &[0]);
    }
    if key.ctrl && ('['..='_').contains(&ch) {
        let byte = (ch as u8) & 0x1f;
        return prefix_alt(key.alt, &[byte]);
    }
    let mut bytes = Vec::new();
    if key.alt {
        bytes.push(0x1b);
    }
    let mut buf = [0; 4];
    bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    bytes
}

fn prefix_alt(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

fn special(name: &str, key: &Key, app_cursor: bool) -> Option<Vec<u8>> {
    let modified = key.ctrl || key.alt || key.shift;
    let arrow = match name {
        "up" => Some(b'A'),
        "down" => Some(b'B'),
        "right" => Some(b'C'),
        "left" => Some(b'D'),
        _ => None,
    };
    if let Some(letter) = arrow {
        if app_cursor && !modified {
            return Some(vec![0x1b, b'O', letter]);
        }
        return Some(csi_modified(letter, key));
    }
    match name {
        "home" => {
            if app_cursor && !modified {
                return Some(b"\x1bOH".to_vec());
            }
            Some(csi_modified(b'H', key))
        }
        "end" => {
            if app_cursor && !modified {
                return Some(b"\x1bOF".to_vec());
            }
            Some(csi_modified(b'F', key))
        }
        "pageup" => Some(tilde(5, key)),
        "pagedown" => Some(tilde(6, key)),
        "insert" => Some(tilde(2, key)),
        "delete" => Some(tilde(3, key)),
        "f1" => Some(fn_key(1, key, app_cursor)),
        "f2" => Some(fn_key(2, key, app_cursor)),
        "f3" => Some(fn_key(3, key, app_cursor)),
        "f4" => Some(fn_key(4, key, app_cursor)),
        "f5" => Some(fn_key(5, key, app_cursor)),
        "f6" => Some(fn_key(6, key, app_cursor)),
        "f7" => Some(fn_key(7, key, app_cursor)),
        "f8" => Some(fn_key(8, key, app_cursor)),
        "f9" => Some(fn_key(9, key, app_cursor)),
        "f10" => Some(fn_key(10, key, app_cursor)),
        "f11" => Some(fn_key(11, key, app_cursor)),
        "f12" => Some(fn_key(12, key, app_cursor)),
        "enter" | "return" => Some(prefix_alt(key.alt, if key.ctrl { b"\n" } else { b"\r" })),
        "backspace" => Some(prefix_alt(
            key.alt,
            if key.ctrl { &[0x08] } else { &[0x7f] },
        )),
        "tab" if key.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(prefix_alt(key.alt, b"\t")),
        "escape" => Some(prefix_alt(false, &[0x1b])),
        "space" => encode_char(' ', key).into(),
        _ => None,
    }
}

fn modifier_param(key: &Key) -> u8 {
    1 + u8::from(key.shift) + u8::from(key.alt) * 2 + u8::from(key.ctrl) * 4
}

fn csi_modified(letter: u8, key: &Key) -> Vec<u8> {
    if !key.ctrl && !key.alt && !key.shift {
        return vec![0x1b, b'[', letter];
    }
    format!("\u{1b}[1;{}{}", modifier_param(key), letter as char).into_bytes()
}

fn tilde(code: u8, key: &Key) -> Vec<u8> {
    if !key.ctrl && !key.alt && !key.shift {
        return format!("\u{1b}[{code}~").into_bytes();
    }
    format!("\u{1b}[{code};{}~", modifier_param(key)).into_bytes()
}

fn fn_key(n: u8, key: &Key, _app_cursor: bool) -> Vec<u8> {
    let ss3 = match n {
        1 => Some(b'P'),
        2 => Some(b'Q'),
        3 => Some(b'R'),
        4 => Some(b'S'),
        _ => None,
    };
    if let Some(letter) = ss3
        && !key.ctrl
        && !key.alt
        && !key.shift
    {
        return vec![0x1b, b'O', letter];
    }
    let code: u8 = match n {
        1 => 11,
        2 => 12,
        3 => 13,
        4 => 14,
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => 15,
    };
    tilde(code, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Key {
        Key::plain(name)
    }

    #[test]
    fn arrows_home_and_paging() {
        assert_eq!(encode_key(&key("up"), false), b"\x1b[A");
        assert_eq!(encode_key(&key("down"), false), b"\x1b[B");
        assert_eq!(encode_key(&key("right"), false), b"\x1b[C");
        assert_eq!(encode_key(&key("left"), false), b"\x1b[D");
        assert_eq!(encode_key(&key("up"), true), b"\x1bOA");
        assert_eq!(encode_key(&key("home"), false), b"\x1b[H");
        assert_eq!(encode_key(&key("end"), true), b"\x1bOF");
        assert_eq!(encode_key(&key("pageup"), false), b"\x1b[5~");
        assert_eq!(encode_key(&key("pagedown"), false), b"\x1b[6~");
    }

    #[test]
    fn function_keys_and_modifiers() {
        assert_eq!(encode_key(&key("f1"), false), b"\x1bOP");
        assert_eq!(encode_key(&key("f5"), false), b"\x1b[15~");
        assert_eq!(encode_key(&key("f12"), false), b"\x1b[24~");
        let mut up = key("up");
        up.ctrl = true;
        assert_eq!(encode_key(&up, false), b"\x1b[1;5A");
        let mut c = key("c");
        c.ctrl = true;
        assert_eq!(encode_key(&c, false), b"\x03");
        let mut x = key("x");
        x.alt = true;
        assert_eq!(encode_key(&x, false), b"\x1bx");
    }

    #[test]
    fn editing_keys_and_paste() {
        assert_eq!(encode_key(&key("enter"), false), b"\r");
        assert_eq!(encode_key(&key("backspace"), false), b"\x7f");
        assert_eq!(encode_key(&key("tab"), false), b"\t");
        let mut tab = key("tab");
        tab.shift = true;
        assert_eq!(encode_key(&tab, false), b"\x1b[Z");
        assert_eq!(encode_paste("hi", false), b"hi");
        assert_eq!(encode_paste("hi", true), b"\x1b[200~hi\x1b[201~");
    }
}
