//! Key names to X keysyms, and characters to the keysyms that type them.

pub const SHIFT_L: u32 = 0xffe1;
pub const CONTROL_L: u32 = 0xffe3;
pub const ALT_L: u32 = 0xffe9;
pub const SUPER_L: u32 = 0xffeb;
pub const RETURN: u32 = 0xff0d;
pub const TAB: u32 = 0xff09;

/// The modifier keysym `name` means, case-insensitively.
pub fn modifier(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(CONTROL_L),
        "shift" => Some(SHIFT_L),
        "alt" => Some(ALT_L),
        "super" | "meta" | "cmd" | "win" => Some(SUPER_L),
        _ => None,
    }
}

/// Names a model may use for a key that X spells otherwise, case-insensitively.
fn alias(name: &str) -> Option<u32> {
    let keysym = match name.to_ascii_lowercase().as_str() {
        "enter" => RETURN,
        "esc" => 0xff1b,
        "backspace" => 0xff08,
        "del" => 0xffff,
        "pageup" | "pgup" => 0xff55,
        "pagedown" | "pgdn" => 0xff56,
        "arrowup" | "up" => 0xff52,
        "arrowdown" | "down" => 0xff54,
        "arrowleft" | "left" => 0xff51,
        "arrowright" | "right" => 0xff53,
        "space" => 0x20,
        _ => return None,
    };
    Some(keysym)
}

/// X keysym names this tool accepts, exact spelling.
const NAMED: &[(&str, u32)] = &[
    ("Return", RETURN),
    ("Tab", TAB),
    ("Escape", 0xff1b),
    ("BackSpace", 0xff08),
    ("Delete", 0xffff),
    ("Insert", 0xff63),
    ("Home", 0xff50),
    ("End", 0xff57),
    ("Page_Up", 0xff55),
    ("Page_Down", 0xff56),
    ("Left", 0xff51),
    ("Up", 0xff52),
    ("Right", 0xff53),
    ("Down", 0xff54),
    ("KP_Enter", 0xff8d),
    ("KP_Add", 0xffab),
    ("KP_Subtract", 0xffad),
    ("KP_Multiply", 0xffaa),
    ("KP_Divide", 0xffaf),
    ("KP_Decimal", 0xffae),
    ("Menu", 0xff67),
    ("Print", 0xff61),
    ("Caps_Lock", 0xffe5),
    ("Shift_L", SHIFT_L),
    ("Shift_R", 0xffe2),
    ("Control_L", CONTROL_L),
    ("Control_R", 0xffe4),
    ("Alt_L", ALT_L),
    ("Alt_R", 0xffea),
    ("Super_L", SUPER_L),
    ("Super_R", 0xffec),
];

/// `F1`–`F24` and `KP_0`–`KP_9`, exact spelling.
fn numbered(name: &str) -> Option<u32> {
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) && !name[1..].starts_with('0') {
            return Some(0xffbe + n - 1);
        }
    }
    if let Some(n) = name.strip_prefix("KP_").and_then(|n| n.parse::<u32>().ok()) {
        if n <= 9 && name.len() == 4 {
            return Some(0xffb0 + n);
        }
    }
    None
}

fn x_name_exact(name: &str) -> Option<u32> {
    NAMED
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, keysym)| *keysym)
        .or_else(|| numbered(name))
}

fn x_name_any_case(name: &str) -> Option<u32> {
    if let Some(keysym) = x_name_exact(name) {
        return Some(keysym);
    }
    if let Some((_, keysym)) = NAMED
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
    {
        return Some(*keysym);
    }
    let lower = name.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("kp_") {
        return numbered(&format!("KP_{rest}"));
    }
    lower
        .strip_prefix('f')
        .and_then(|rest| numbered(&format!("F{rest}")))
}

/// The keysym a key name means: a modifier, an alias, an X keysym name (exact case first), or a
/// single printable character.
pub fn key_name(name: &str) -> Option<u32> {
    if let Some(keysym) = modifier(name).or_else(|| alias(name)) {
        return Some(keysym);
    }
    if let Some(keysym) = x_name_any_case(name) {
        return Some(keysym);
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => char_keysym(c),
        _ => None,
    }
}

/// The keysym that types `c`: Latin-1 directly, otherwise `0x01000000 + codepoint`; newline is
/// `Return` and tab `Tab`. Other control characters have none.
pub fn char_keysym(c: char) -> Option<u32> {
    let cp = c as u32;
    match c {
        '\n' => Some(RETURN),
        '\t' => Some(TAB),
        _ if c.is_control() => None,
        _ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => Some(cp),
        _ => Some(0x0100_0000 + cp),
    }
}

/// One chord of a `key` value: the keysyms pressed together, in order.
pub type Combo = Vec<u32>;

/// Splits a `key` value into combos: separated by single spaces, each a `+`-joined list of key
/// names. `+` alone, or as the last name (`ctrl++`), is the plus key. Errors name the bad part.
pub fn parse_combos(value: &str) -> Result<Vec<Combo>, String> {
    if value.is_empty() {
        return Err(
            "key is empty; give a key name such as Return or a combo such as ctrl+s".into(),
        );
    }
    let mut combos = Vec::new();
    for combo in value.split(' ') {
        if combo.is_empty() {
            return Err(format!(
                "key '{value}' has an empty combo; separate combos with single spaces"
            ));
        }
        let names: Vec<&str> = if combo == "+" {
            vec!["+"]
        } else if let Some(head) = combo.strip_suffix("++") {
            let mut names: Vec<&str> = head.split('+').collect();
            names.push("+");
            names
        } else {
            combo.split('+').collect()
        };
        let mut keys = Vec::new();
        for name in names {
            if name.is_empty() {
                return Err(format!(
                    "key '{value}' has an empty name in '{combo}'; join key names with a single +"
                ));
            }
            match key_name(name) {
                Some(keysym) => keys.push(keysym),
                None => {
                    return Err(format!(
                        "key '{name}' in '{value}' is not a key name; use a modifier (ctrl, shift, \
                         alt, super), a name such as Return, Tab, Escape, BackSpace, Page_Down or \
                         F5, or one character"
                    ))
                }
            }
        }
        combos.push(keys);
    }
    Ok(combos)
}

/// Splits a `modifiers` value into modifier keysyms, in order.
pub fn parse_modifiers(value: &str) -> Result<Vec<u32>, String> {
    let mut keys = Vec::new();
    for name in value.split('+') {
        match modifier(name) {
            Some(keysym) => keys.push(keysym),
            None => {
                return Err(format!(
                    "modifiers '{value}' holds '{name}', which is not a modifier; the modifiers \
                     are ctrl, shift, alt and super, joined by +"
                ))
            }
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_and_aliases_are_case_insensitive() {
        assert_eq!(key_name("CTRL"), Some(CONTROL_L));
        assert_eq!(key_name("Control"), Some(CONTROL_L));
        assert_eq!(key_name("cmd"), Some(SUPER_L));
        assert_eq!(key_name("Win"), Some(SUPER_L));
        assert_eq!(key_name("Enter"), Some(RETURN));
        assert_eq!(key_name("PgDn"), Some(0xff56));
        assert_eq!(key_name("ArrowLeft"), Some(0xff51));
        assert_eq!(key_name("space"), Some(0x20));
    }

    #[test]
    fn x_names_match_exact_case_first_then_any_case() {
        assert_eq!(key_name("Return"), Some(RETURN));
        assert_eq!(key_name("return"), Some(RETURN));
        assert_eq!(key_name("page_down"), Some(0xff56));
        assert_eq!(key_name("F1"), Some(0xffbe));
        assert_eq!(key_name("f24"), Some(0xffd5));
        assert_eq!(key_name("F25"), None);
        assert_eq!(key_name("KP_7"), Some(0xffb7));
        assert_eq!(key_name("kp_enter"), Some(0xff8d));
        assert_eq!(key_name("Caps_Lock"), Some(0xffe5));
    }

    #[test]
    fn one_printable_character_is_a_name() {
        assert_eq!(key_name("a"), Some(0x61));
        assert_eq!(key_name("A"), Some(0x41));
        assert_eq!(key_name("é"), Some(0xe9));
        assert_eq!(key_name("€"), Some(0x0100_20ac));
        assert_eq!(key_name("bogus"), None);
    }

    #[test]
    fn combos_split_on_single_spaces_and_plus() {
        assert_eq!(
            parse_combos("ctrl+a Return").unwrap(),
            vec![vec![CONTROL_L, 0x61], vec![RETURN]]
        );
        assert_eq!(parse_combos("ctrl++").unwrap(), vec![vec![CONTROL_L, 0x2b]]);
        assert_eq!(parse_combos("+").unwrap(), vec![vec![0x2b]]);
        let err = parse_combos("ctrl+bogus").unwrap_err();
        assert!(err.contains("'bogus'"), "{err}");
        assert!(parse_combos("a  b").unwrap_err().contains("empty combo"));
        assert!(parse_combos("ctrl+").unwrap_err().contains("empty name"));
    }

    #[test]
    fn modifiers_refuse_a_plain_key() {
        assert_eq!(
            parse_modifiers("ctrl+Shift").unwrap(),
            vec![CONTROL_L, SHIFT_L]
        );
        assert!(parse_modifiers("ctrl+a").unwrap_err().contains("'a'"));
    }

    #[test]
    fn characters_map_to_latin1_or_unicode_keysyms() {
        assert_eq!(char_keysym('a'), Some(0x61));
        assert_eq!(char_keysym('é'), Some(0xe9));
        assert_eq!(char_keysym('ő'), Some(0x0100_0151));
        assert_eq!(char_keysym('\n'), Some(RETURN));
        assert_eq!(char_keysym('\t'), Some(TAB));
        assert_eq!(char_keysym('\u{7}'), None);
    }
}
