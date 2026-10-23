//! What [`super::quoted`] makes of every `$'…'` body below, as it was recorded
//! before the decoder was restructured: the decoded text and where the string
//! ends.

use super::quoted;
use crate::parser_pins::{assert_pinned, esc};

const RECORDING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/ansi_c.txt"
));

/// Bodies read whole: the module's own tests, and the edges of each escape.
const BODIES: &[&str] = &[
    r"\'' ; rm",
    r"it\'s' tail",
    r"\c' ; rm",
    r"\c\'' ; rm",
    r"\x' ; rm",
    r"\1' ; rm",
    r"rm\0x' tail",
    r"rm\x00x' tail",
    r"rm\000x' tail",
    r"rm\c@x' tail",
    concat!("rm", "\\", "u0000x' tail"),
    r"a\tb'",
    r"\x72m'",
    r"\x7'",
    r"\162m'",
    r"\0'",
    concat!("\\", "u0072m'"),
    r"\U00000072m'",
    r"\cA'",
    r"\\'",
    r#"\"\?'"#,
    r"\e\E'",
    r"\z'",
    r"\xg'",
    r"abc\",
    "abc",
    "",
    "'",
    r"\",
    r"\\",
    r"\377'",
    r"\777'",
    r"\400'",
    r"\08'",
    r"\1234'",
    r"\x123'",
    r"\xFF'",
    r"\xfF'",
    concat!("\\", "ud800'"),
    concat!("\\", "udfff'"),
    concat!("\\", "uFFFF'"),
    concat!("\\", "u12345'"),
    r"\U00110000'",
    r"\UFFFFFFFF'",
    r"\U0001F600'",
    r"\U1'",
    r"\c\\'",
    r"\c'",
    r"\cé'",
    r"\c?'",
    r"\c[x'",
    r"\x'",
    r"\u'",
    r"\U'",
    r"\u'tail",
    "é日本\\x41'",
    "\\é'",
    "a\nb'",
    "\\\n'",
    r"\a\b\f\n\r\t\v'",
    r"a'b'c",
    r"\'\'\''",
];

/// Escapes and what follows them, every pair of which is read.
const ESCAPES: &[&str] = &[
    "a", "b", "e", "E", "f", "n", "r", "t", "v", "\\", "'", "\"", "?", "0", "1", "3", "4", "7",
    "8", "x", "u", "U", "c", "z", "é",
];
const TAILS: &[&str] = &[
    "",
    "'",
    "1",
    "41'",
    "7f'",
    "377'",
    "0000'",
    "00000041'",
    "zz'",
    "\\'",
    "\\",
    "'tail",
    "@x'",
    "\\\\'",
    "日'",
    "0'",
];

fn reading(body: &str, start: usize) -> String {
    let chars: Vec<char> = body.chars().collect();
    let (text, end) = quoted(&chars, start);
    format!("{} @{start} => {} {end}", esc(body), esc(&text))
}

/// **NO RESTRUCTURING CHANGES A DECODED STRING OR WHERE IT ENDS.**
#[test]
fn every_pinned_body_decodes_as_recorded() {
    let mut lines = Vec::new();
    for body in BODIES {
        let length = body.chars().count();
        for start in [0, 1, length, length + 1] {
            lines.push(reading(body, start));
        }
    }
    for escape in ESCAPES {
        for tail in TAILS {
            lines.push(reading(&format!("\\{escape}{tail}"), 0));
            lines.push(reading(&format!("x{escape}{tail}"), 0));
        }
    }
    assert_pinned(RECORDING, &lines);
}
