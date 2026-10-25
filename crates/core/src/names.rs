//! File-name rules that differ between platforms: Unicode normalisation,
//! case-insensitive collisions and names Windows cannot store.

use unicode_normalization::{UnicodeNormalization, is_nfc};

/// `path` in Unicode NFC, the form used in records (macOS file systems may
/// hand out NFD names; Windows and Linux usually NFC).
pub fn to_nfc(path: &str) -> String {
    if is_nfc(path) {
        path.to_owned()
    } else {
        path.nfc().collect()
    }
}

/// A key under which names that a case-insensitive file system (default
/// macOS and Windows) treats as the same entry compare equal.
pub fn case_key(path: &str) -> String {
    to_nfc(path).to_lowercase()
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Why Windows cannot store a file or directory named `name`, if it can't.
pub fn windows_name_problem(name: &str) -> Option<&'static str> {
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c < ' ')
    {
        return Some("contains a character Windows does not allow (<>:\"|?* or control)");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("ends with a dot or space, which Windows strips");
    }
    let stem = name.split('.').next().unwrap_or("").trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Some("is a reserved device name on Windows (CON, NUL, COM1, …)");
    }
    None
}

/// Longest relative path accepted on Windows without long-path support
/// (MAX_PATH is 260 including the folder's own path; leave room for it).
pub const WINDOWS_MAX_REL_PATH: usize = 200;

/// Why `path` cannot be created on Windows, if it can't.
pub fn windows_path_problem(path: &str) -> Option<String> {
    for part in path.split('/') {
        if let Some(why) = windows_name_problem(part) {
            return Some(format!("{part:?} {why}"));
        }
    }
    if path.encode_utf16().count() > WINDOWS_MAX_REL_PATH {
        return Some(format!(
            "path is longer than {WINDOWS_MAX_REL_PATH} characters"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_and_case_keys() {
        let nfd = "cafe\u{301}.txt";
        assert_eq!(to_nfc(nfd), "caf\u{e9}.txt");
        assert_eq!(case_key("Caf\u{e9}.TXT"), case_key(nfd));
        assert_ne!(case_key("a.txt"), case_key("b.txt"));
    }

    #[test]
    fn windows_names() {
        for bad in [
            "CON",
            "con.txt",
            "Lpt1.log",
            "a:b",
            "what?",
            "trailing.",
            "space ",
            "tab\t",
        ] {
            assert!(windows_name_problem(bad).is_some(), "{bad:?}");
        }
        for ok in [
            "console.txt",
            "COM10",
            "normal",
            ".hidden",
            "a.b.c",
            "nul_byte_free",
        ] {
            assert!(windows_name_problem(ok).is_none(), "{ok:?}");
        }
        assert!(windows_path_problem("ok/AUX/x").is_some());
        assert!(windows_path_problem(&"a/".repeat(150)).is_some());
        assert!(windows_path_problem("docs/report.pdf").is_none());
    }
}
