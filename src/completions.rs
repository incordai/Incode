//! `incode completions <shell>`: a completion script for bash, zsh, fish or
//! PowerShell, generated from a table of the command line.
//!
//! The parser is hand-written (`main.rs` `parse_args`), so there is no
//! argument library to generate completions from. The table — [`Command`] —
//! is kept beside `USAGE` in `main.rs`, whose tests hold the two to the same
//! flags and check that the parser accepts every one of them.
//!
//! Every script follows one logic: the word before the cursor decides what is
//! offered. After a flag with fixed values, those values; after a flag that
//! takes a path or free text, nothing, so the shell completes file names; else
//! the flags of the command in force — `incode` itself, or the subcommand in
//! first place — and, in the first position after it, its subcommands or
//! verbs. bash and zsh get a function each, over their own word arrays (bash
//! counts from 0, zsh from 1); fish gets `complete` lines and PowerShell a
//! native argument completer.

/// What a flag takes after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// Nothing: a switch.
    None,
    /// One of these words.
    Choice(&'static [&'static str]),
    /// A path: the shell completes file names.
    Path,
    /// Free text (a model, a task): nothing to offer.
    Text,
}

/// One flag, in its long and short spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    pub long: &'static str,
    pub short: Option<&'static str>,
    pub value: Value,
    /// One line, shown by fish beside the flag.
    pub help: &'static str,
}

/// `incode`, or one of its subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub flags: &'static [Flag],
    /// Words that may come first after the command (`mcp list`, `plugin add`).
    pub verbs: &'static [&'static str],
    /// Commands in first place after this one (only `incode` has them).
    pub subcommands: &'static [Command],
}

/// A flag with fixed values: its spellings, and the values.
type Choices = (Vec<&'static str>, &'static [&'static str]);

/// The shells a script is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl Shell {
    /// Every shell, by the name `incode completions` takes.
    pub const NAMES: [&'static str; 4] = ["bash", "zsh", "fish", "powershell"];

    /// The shell `name` names (`pwsh` too), or `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            "powershell" | "pwsh" => Some(Self::PowerShell),
            _ => None,
        }
    }
}

impl Command {
    fn words(&self) -> Vec<&'static str> {
        let mut words = Vec::new();
        for flag in self.flags {
            words.push(flag.long);
            words.extend(flag.short);
        }
        words
    }

    /// The words offered first after this command: its verbs, or the names
    /// of its subcommands.
    fn first_words(&self) -> Vec<&'static str> {
        let mut words: Vec<&'static str> = self.verbs.to_vec();
        words.extend(self.subcommands.iter().map(|c| c.name));
        words
    }

    /// The flags that take something, grouped by what: choices with their
    /// words, then every flag after which nothing is offered.
    fn valued(&self) -> (Vec<Choices>, Vec<&'static str>) {
        let mut choices = Vec::new();
        let mut open = Vec::new();
        for flag in self.flags {
            let spellings: Vec<&'static str> =
                std::iter::once(flag.long).chain(flag.short).collect();
            match flag.value {
                Value::Choice(words) => choices.push((spellings, words)),
                Value::Path | Value::Text => open.extend(spellings),
                Value::None => {}
            }
        }
        (choices, open)
    }
}

/// Every word a script will contain, from `spec` down. Each must be a plain
/// token — letters, digits and `-` — so no shell reads one as syntax.
#[must_use]
pub fn all_words(spec: &Command) -> Vec<&'static str> {
    let mut words = vec![spec.name];
    words.extend(spec.words());
    words.extend(spec.first_words());
    for flag in spec.flags {
        if let Value::Choice(values) = flag.value {
            words.extend(values);
        }
    }
    for sub in spec.subcommands {
        words.extend(all_words(sub));
    }
    words
}

fn is_plain(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The completion script for `shell`.
///
/// # Errors
///
/// A word in `spec` is not a plain token ([`all_words`]): it is refused
/// rather than written into a script the shell runs.
pub fn script(shell: Shell, spec: &Command) -> Result<String, String> {
    if let Some(bad) = all_words(spec).into_iter().find(|w| !is_plain(w)) {
        return Err(format!(
            "`{bad}` is not a plain word; it cannot go into a completion script"
        ));
    }
    Ok(match shell {
        Shell::Bash => format!(
            "# {name} completion for bash. Load it with:\n\
             #   source <({name} completions bash)\n{body}",
            name = spec.name,
            body = bash(spec)
        ),
        Shell::Zsh => format!(
            "#compdef {name}\n\
             # {name} completion for zsh. Load it, after compinit, with:\n\
             #   source <({name} completions zsh)\n{body}",
            name = spec.name,
            body = zsh(spec)
        ),
        Shell::Fish => fish(spec),
        Shell::PowerShell => powershell(spec),
    })
}

// ---------------------------------------------------------------------------
// bash (and zsh)
// ---------------------------------------------------------------------------

fn bash(spec: &Command) -> String {
    let function = format!("_{}", spec.name.replace('-', "_"));
    let mut out = format!(
        "{function}() {{\n\
         \x20   local cur=\"${{COMP_WORDS[COMP_CWORD]}}\" prev=\"${{COMP_WORDS[COMP_CWORD-1]}}\"\n\
         \x20   local sub=\"\" words=\"\"\n"
    );
    if !spec.subcommands.is_empty() {
        let names: Vec<&str> = spec.subcommands.iter().map(|c| c.name).collect();
        out.push_str(&format!(
            "    if [ \"$COMP_CWORD\" -gt 1 ]; then\n\
             \x20       case \"${{COMP_WORDS[1]}}\" in\n\
             \x20           {}) sub=\"${{COMP_WORDS[1]}}\" ;;\n\
             \x20       esac\n\
             \x20   fi\n",
            names.join("|")
        ));
    }
    out.push_str("    case \"$sub\" in\n");
    out.push_str(&format!(
        "        \"\")\n{}        ;;\n",
        bash_command(spec, 1)
    ));
    for sub in spec.subcommands {
        out.push_str(&format!(
            "        {})\n{}        ;;\n",
            sub.name,
            bash_command(sub, 2)
        ));
    }
    out.push_str(&format!(
        "    esac\n\
         \x20   COMPREPLY=($(compgen -W \"$words\" -- \"$cur\"))\n\
         }}\n\
         complete -o default -F {function} {}\n",
        spec.name
    ));
    out
}

/// The `case "$prev"` for one command; `first` is the word position right
/// after it.
fn bash_command(command: &Command, first: usize) -> String {
    let (choices, open) = command.valued();
    let mut out = String::from("            case \"$prev\" in\n");
    for (spellings, values) in &choices {
        out.push_str(&format!(
            "                {}) words=\"{}\" ;;\n",
            spellings.join("|"),
            values.join(" ")
        ));
    }
    if !open.is_empty() {
        out.push_str(&format!(
            "                {}) return 0 ;;\n",
            open.join("|")
        ));
    }
    let first_words = command.first_words();
    out.push_str(&format!(
        "                *) words=\"{}\"",
        command.words().join(" ")
    ));
    if !first_words.is_empty() {
        out.push_str(&format!(
            "\n                    [ \"$COMP_CWORD\" -eq {first} ] && words=\"$words {}\"",
            first_words.join(" ")
        ));
    }
    out.push_str(" ;;\n            esac\n");
    out
}

// ---------------------------------------------------------------------------
// zsh: the same logic over zsh's own `$words` (1-based) and `$CURRENT`
// ---------------------------------------------------------------------------

fn zsh(spec: &Command) -> String {
    let function = format!("_{}", spec.name.replace('-', "_"));
    let mut out = format!(
        "{function}() {{\n\
         \x20   local prev=\"${{words[CURRENT-1]}}\" sub=\"\"\n\
         \x20   local -a found\n"
    );
    if !spec.subcommands.is_empty() {
        let names: Vec<&str> = spec.subcommands.iter().map(|c| c.name).collect();
        out.push_str(&format!(
            "    if (( CURRENT > 2 )); then\n\
             \x20       case \"${{words[2]}}\" in\n\
             \x20           {}) sub=\"${{words[2]}}\" ;;\n\
             \x20       esac\n\
             \x20   fi\n",
            names.join("|")
        ));
    }
    out.push_str("    case \"$sub\" in\n");
    out.push_str(&format!(
        "        \"\")\n{}        ;;\n",
        zsh_command(spec, 2)
    ));
    for sub in spec.subcommands {
        out.push_str(&format!(
            "        {})\n{}        ;;\n",
            sub.name,
            zsh_command(sub, 3)
        ));
    }
    out.push_str(&format!(
        "    esac\n\
         \x20   compadd -a found\n\
         }}\n\
         compdef {function} {}\n",
        spec.name
    ));
    out
}

/// The `case "$prev"` for one command; `first` is the `$CURRENT` right after
/// it. After a path flag `_files` completes; after free text, nothing.
fn zsh_command(command: &Command, first: usize) -> String {
    let mut out = String::from("            case \"$prev\" in\n");
    let mut paths = Vec::new();
    let mut texts = Vec::new();
    for flag in command.flags {
        let spellings: Vec<&str> = std::iter::once(flag.long).chain(flag.short).collect();
        match flag.value {
            Value::Choice(values) => out.push_str(&format!(
                "                {}) found=({}) ;;\n",
                spellings.join("|"),
                values.join(" ")
            )),
            Value::Path => paths.extend(spellings),
            Value::Text => texts.extend(spellings),
            Value::None => {}
        }
    }
    if !paths.is_empty() {
        out.push_str(&format!(
            "                {}) _files; return ;;\n",
            paths.join("|")
        ));
    }
    if !texts.is_empty() {
        out.push_str(&format!("                {}) return ;;\n", texts.join("|")));
    }
    out.push_str(&format!(
        "                *) found=({})",
        command.words().join(" ")
    ));
    let first_words = command.first_words();
    if !first_words.is_empty() {
        out.push_str(&format!(
            "\n                    (( CURRENT == {first} )) && found+=({})",
            first_words.join(" ")
        ));
    }
    out.push_str(" ;;\n            esac\n");
    out
}

// ---------------------------------------------------------------------------
// fish
// ---------------------------------------------------------------------------

fn fish(spec: &Command) -> String {
    let name = spec.name;
    let mut out = format!(
        "# {name} completion for fish. Load it with:\n\
         #   {name} completions fish | source\n\
         complete -c {name} -f\n"
    );
    let subs: Vec<&str> = spec.subcommands.iter().map(|c| c.name).collect();
    let top = if subs.is_empty() {
        String::new()
    } else {
        format!("-n 'not __fish_seen_subcommand_from {}' ", subs.join(" "))
    };
    out.push_str(&fish_flags(name, spec, &top));
    // First place only, as the parser takes them: counted, because
    // `__fish_use_subcommand` stays true after a flag (`incode --admin up`).
    let first = spec.first_words();
    if !first.is_empty() {
        out.push_str(&format!(
            "complete -c {name} -n 'test (count (commandline -opc)) -eq 1' -a '{}'\n",
            first.join(" ")
        ));
    }
    for sub in spec.subcommands {
        let inside = format!("-n '__fish_seen_subcommand_from {}' ", sub.name);
        out.push_str(&fish_flags(name, sub, &inside));
        if !sub.verbs.is_empty() {
            out.push_str(&format!(
                "complete -c {name} -n 'test (count (commandline -opc)) -eq 2; and \
                 __fish_seen_subcommand_from {}' -a '{}'\n",
                sub.name,
                sub.verbs.join(" ")
            ));
        }
    }
    out
}

fn fish_flags(name: &str, command: &Command, condition: &str) -> String {
    let mut out = String::new();
    for flag in command.flags {
        let mut line = format!(
            "complete -c {name} {condition}-l {}",
            flag.long.trim_start_matches('-')
        );
        if let Some(short) = flag.short {
            line.push_str(&format!(" -s {}", short.trim_start_matches('-')));
        }
        match flag.value {
            Value::None => {}
            Value::Choice(values) => line.push_str(&format!(" -x -a '{}'", values.join(" "))),
            Value::Path => line.push_str(" -r -F"),
            Value::Text => line.push_str(" -x"),
        }
        line.push_str(&format!(" -d '{}'", flag.help.replace('\'', "\\'")));
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// PowerShell
// ---------------------------------------------------------------------------

fn powershell(spec: &Command) -> String {
    let name = spec.name;
    let list = |words: &[&str]| -> String {
        words
            .iter()
            .map(|w| format!("'{w}'"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut out = format!(
        "# {name} completion for PowerShell. Load it with:\n\
         #   {name} completions powershell | Out-String | Invoke-Expression\n\
         Register-ArgumentCompleter -Native -CommandName '{name}','{name}.exe' -ScriptBlock {{\n\
         \x20   param($wordToComplete, $commandAst, $cursorPosition)\n\
         \x20   $before = @($commandAst.CommandElements |\n\
         \x20       Where-Object {{ $_.Extent.EndOffset -lt $cursorPosition -or\n\
         \x20           ($wordToComplete -eq '' -and $_.Extent.EndOffset -le $cursorPosition) }} |\n\
         \x20       ForEach-Object {{ $_.Extent.Text }})\n\
         \x20   $count = $before.Count\n\
         \x20   $prev = $before[$count - 1]\n\
         \x20   $sub = ''\n"
    );
    if !spec.subcommands.is_empty() {
        let names: Vec<&str> = spec.subcommands.iter().map(|c| c.name).collect();
        out.push_str(&format!(
            "    if ($count -gt 1 -and @({}) -contains $before[1]) {{ $sub = $before[1] }}\n",
            list(&names)
        ));
    }
    out.push_str("    $words = @()\n");
    let mut branches = vec![(String::new(), powershell_command(spec, 1, &list))];
    for sub in spec.subcommands {
        branches.push((sub.name.to_string(), powershell_command(sub, 2, &list)));
    }
    for (i, (sub, body)) in branches.iter().enumerate() {
        let keyword = if i == 0 { "if" } else { "elseif" };
        out.push_str(&format!(
            "    {keyword} ($sub -eq '{sub}') {{\n{body}    }}\n"
        ));
    }
    out.push_str(
        "    $words | Where-Object { $_ -like \"$wordToComplete*\" } | ForEach-Object {\n\
         \x20       [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)\n\
         \x20   }\n\
         }\n",
    );
    out
}

fn powershell_command(command: &Command, first: usize, list: &dyn Fn(&[&str]) -> String) -> String {
    let (choices, open) = command.valued();
    let mut out = String::new();
    let mut keyword = "if";
    for (spellings, values) in &choices {
        out.push_str(&format!(
            "        {keyword} (@({}) -contains $prev) {{ $words = @({}) }}\n",
            list(spellings),
            list(values)
        ));
        keyword = "elseif";
    }
    if !open.is_empty() {
        out.push_str(&format!(
            "        {keyword} (@({}) -contains $prev) {{ return }}\n",
            list(&open)
        ));
        keyword = "elseif";
    }
    // With no condition before it, the default is plain statements: a bare
    // `{ … }` in PowerShell is a script-block VALUE, written to the output
    // instead of run.
    let guarded = keyword == "elseif";
    let indent = if guarded { "            " } else { "        " };
    if guarded {
        out.push_str("        else {\n");
    }
    out.push_str(&format!("{indent}$words = @({})\n", list(&command.words())));
    let first_words = command.first_words();
    if !first_words.is_empty() {
        out.push_str(&format!(
            "{indent}if ($count -eq {first}) {{ $words += @({}) }}\n",
            list(&first_words)
        ));
    }
    if guarded {
        out.push_str("        }\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUB: Command = Command {
        name: "tool",
        flags: &[Flag {
            long: "--project",
            short: None,
            value: Value::None,
            help: "in this project",
        }],
        verbs: &["list", "add"],
        subcommands: &[],
    };

    const SPEC: Command = Command {
        name: "demo",
        flags: &[
            Flag {
                long: "--mode",
                short: Some("-p"),
                value: Value::Choice(&["read-only", "write"]),
                help: "the mode",
            },
            Flag {
                long: "--workspace",
                short: Some("-w"),
                value: Value::Path,
                help: "a folder",
            },
        ],
        verbs: &[],
        subcommands: &[SUB],
    };

    #[test]
    fn every_shell_gets_a_script_naming_every_word() {
        for name in Shell::NAMES {
            let shell = Shell::parse(name).expect("a shell");
            let text = script(shell, &SPEC).expect("a script");
            // Flags without their dashes: fish spells them `-l mode`.
            for word in [
                "mode",
                "read-only",
                "write",
                "workspace",
                "tool",
                "list",
                "project",
            ] {
                assert!(text.contains(word), "{name} lacks `{word}`:\n{text}");
            }
        }
        assert_eq!(Shell::parse("pwsh"), Some(Shell::PowerShell));
        assert_eq!(Shell::parse("cmd"), None);
    }

    /// A word that is not a plain token would be shell syntax in the script.
    #[test]
    fn a_word_that_is_not_plain_is_refused() {
        const BAD: Command = Command {
            name: "demo",
            flags: &[Flag {
                long: "--x;rm",
                short: None,
                value: Value::None,
                help: "",
            }],
            verbs: &[],
            subcommands: &[],
        };
        for name in Shell::NAMES {
            let shell = Shell::parse(name).expect("a shell");
            assert!(script(shell, &BAD).is_err(), "{name}");
        }
    }
}
