//! One simple command, read for the deny and ask rules: its words, unquoted,
//! with leading assignments and wrapper commands (`sudo`, `env`, `timeout 5`,
//! …) peeled off one at a time, and the script it hands to a shell.

/// Split one simple command into words, quote-aware, with the quotes kept.
///
/// Called by [`peel`].
pub(super) fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match quote {
            Some(q) => {
                word.push(c);
                if c == '\\' && q == '"' {
                    if let Some(&n) = chars.get(i) {
                        word.push(n);
                        i += 1;
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                ' ' | '\t' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                '$' if chars.get(i) == Some(&'$') => {
                    word.push_str("$$");
                    i += 1;
                }
                // `$'…'`, kept as written, to where bash ends it.
                '$' if chars.get(i) == Some(&'\'') => {
                    let (_, end) = crate::ansi_c::quoted(&chars, i + 1);
                    word.extend(&chars[i - 1..end]);
                    i = end;
                }
                '\'' | '"' => {
                    quote = Some(c);
                    word.push(c);
                }
                '\\' => {
                    word.push(c);
                    if let Some(&n) = chars.get(i) {
                        word.push(n);
                        i += 1;
                    }
                }
                _ => word.push(c),
            },
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// A word with its outer quotes and backslashes removed, and a `$'…'` in it
/// decoded (`crate::ansi_c`): `$'\x72m'` is `rm`.
///
/// Called by [`peel`].
pub(super) fn unquote(word: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        i += 1;
        match (quote, c) {
            (None, '$') if next == Some('$') => {
                out.push_str("$$");
                i += 1;
            }
            (None, '$') if next == Some('\'') => {
                let (text, end) = crate::ansi_c::quoted(&chars, i + 1);
                out.push_str(&text);
                i = end;
            }
            // `$"…"` is `"…"`.
            (None, '$') if next == Some('"') => {}
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None | Some('"'), '\\') => {
                if let Some(n) = next {
                    out.push(n);
                    i += 1;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn is_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Commands that run the rest of their arguments as a command, and how many
/// leading non-flag arguments of their own to skip first.
fn wrapper(word: &str) -> Option<usize> {
    let name = word.rsplit('/').next().unwrap_or(word);
    match name {
        "sudo" | "doas" | "env" | "nohup" | "time" | "command" | "builtin" | "exec" | "nice"
        | "ionice" | "xargs" | "stdbuf" | "setsid" | "chroot" | "unbuffer" | "caffeinate" => {
            Some(usize::from(name == "chroot"))
        }
        "timeout" => Some(1),
        _ => None,
    }
}

/// Flags of `sudo` that take a value as the next word.
fn flag_takes_value(wrapper: &str, flag: &str) -> bool {
    matches!(
        (wrapper, flag),
        (
            "sudo",
            "-u" | "-g" | "-C" | "-h" | "-p" | "-U" | "-r" | "-t"
        ) | ("doas", "-u" | "-C")
            | ("nice", "-n")
            | ("timeout", "-s" | "-k" | "--signal" | "--kill-after")
            | ("xargs", "-I" | "-n" | "-P" | "-L" | "-d" | "-E" | "-s")
            | ("env", "-u" | "-C" | "-S")
            | ("stdbuf", "-i" | "-o" | "-e")
    )
}

/// `text`, then `text` with each leading assignment and wrapper peeled off in
/// turn — each form also unquoted, since `'rm' -rf /` and `$'\x72m' -rf /` run
/// `rm` — and the script, when the command hands one to a shell.
///
/// Called by the splitter's `finish_segment` (`permissions::rules::split`).
pub(super) fn peel(text: &str) -> (Vec<String>, Option<String>) {
    let all = words(text);
    let plain: Vec<String> = all.iter().map(|w| unquote(w)).collect();
    let mut forms = vec![all.join(" "), plain.join(" ")];
    let mut at = 0;
    while at < all.len() {
        let word = &all[at];
        if is_assignment(word) {
            at += 1;
        } else if let Some(skip) = wrapper(&unquote(word)) {
            let name = unquote(word);
            let name = name.rsplit('/').next().unwrap_or(&name).to_string();
            at += 1;
            while at < all.len() && (all[at].starts_with('-') || is_assignment(&all[at])) {
                let flag = all[at].clone();
                at += 1;
                if flag_takes_value(&name, &flag) {
                    at += 1;
                }
            }
            at += skip;
        } else {
            break;
        }
        if at < all.len() {
            forms.push(all[at..].join(" "));
            forms.push(plain[at..].join(" "));
        }
    }
    // `sh -c '<script>'`, `bash -lc "<script>"`, `eval <words>`.
    let rest = &all[at.min(all.len())..];
    let script = match rest.first().map(|w| unquote(w)) {
        Some(shell)
            if matches!(
                shell.rsplit('/').next().unwrap_or(&shell),
                "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish"
            ) =>
        {
            rest.iter()
                .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
                .and_then(|flag| rest.get(flag + 1))
                .map(|script| unquote(script))
        }
        Some(eval) if eval == "eval" => Some(
            rest[1..]
                .iter()
                .map(|w| unquote(w))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    (forms, script)
}

#[cfg(test)]
mod tests {
    use super::{peel, unquote, words};

    /// Quotes are kept in a word, and a `$'…'` runs to where bash ends it.
    #[test]
    fn words_keep_their_quotes() {
        assert_eq!(words(r"a 'b c' $'d\'e' f"), ["a", "'b c'", r"$'d\'e'", "f"]);
        assert_eq!(unquote(r"$'\x72m'"), "rm");
    }

    /// A wrapper and its flags (with their values) are peeled off; the
    /// script handed to a shell is returned.
    #[test]
    fn wrappers_are_peeled_and_a_shell_script_is_found() {
        let (forms, script) = peel("sudo -u root rm -rf /");
        assert!(forms.contains(&"rm -rf /".to_string()), "{forms:?}");
        assert_eq!(script, None);
        assert_eq!(peel("bash -c 'rm a'").1.as_deref(), Some("rm a"));
        assert_eq!(peel("eval rm 'b'").1.as_deref(), Some("rm b"));
    }
}
