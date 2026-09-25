//! `Outline`: a source file's symbols, from its syntax tree.
//!
//! What a model reads a whole file to learn — which functions, types and
//! methods it holds, what they take and where they are — in a few hundred
//! bytes instead of the file. Each language's definitions are named by a
//! tree-sitter QUERY, the same mechanism editors use for their symbol lists,
//! so adding a construct is one line of query rather than a tree walk.
//!
//! Each grammar is a cargo feature (`lang-rust`, `lang-ts`, `lang-python`,
//! `lang-go`, `lang-js`), all on by default. A file in a language this binary
//! was built without, or one Outline does not know, is answered plainly —
//! never with an empty outline that would read as "no symbols".

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tree_sitter::StreamingIterator as _;

/// The most symbols one outline shows. The rest are counted.
pub const MAX_OUTLINE_SYMBOLS: usize = 400;
/// The largest file outlined.
pub const MAX_OUTLINE_BYTES: u64 = 4 * 1024 * 1024;
/// A signature longer than this is cut.
const MAX_SIGNATURE_CHARS: usize = 160;
/// How long a parse may take before it is abandoned.
const PARSE_DEADLINE: Duration = Duration::from_secs(10);

/// The tool as the model is told about it.
pub(super) fn definition() -> Value {
    json!({
        "name": super::OUTLINE,
        "description": "List a source file's symbols — functions, methods, types, classes, \
            traits, impls, constants — with each one's signature and line range, nested the way \
            the code nests them. Much cheaper than Read when you need to know what a file \
            contains or where something is defined; Read the lines it points at afterwards. \
            Supports Rust, TypeScript/TSX, JavaScript/JSX, Python and Go.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The source file, relative to the workspace or absolute."}
            },
            "required": ["path"]
        }
    })
}

/// A language Outline understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}

impl Lang {
    fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "TSX",
            Self::JavaScript => "JavaScript",
            Self::Python => "Python",
            Self::Go => "Go",
        }
    }

    fn feature(self) -> &'static str {
        match self {
            Self::Rust => "lang-rust",
            Self::TypeScript | Self::Tsx => "lang-ts",
            Self::JavaScript => "lang-js",
            Self::Python => "lang-python",
            Self::Go => "lang-go",
        }
    }

    /// The grammar, when this binary was built with it.
    fn grammar(self) -> Option<tree_sitter::Language> {
        match self {
            Self::Rust => grammars::rust(),
            Self::TypeScript => grammars::typescript(),
            Self::Tsx => grammars::tsx(),
            Self::JavaScript => grammars::javascript(),
            Self::Python => grammars::python(),
            Self::Go => grammars::go(),
        }
    }

    /// The definitions, as a query: each pattern captures the definition
    /// under its KIND and, where it has one, its `@name`.
    fn query(self) -> &'static str {
        match self {
            Self::Rust => RUST_QUERY,
            Self::TypeScript | Self::Tsx => TS_QUERY,
            Self::JavaScript => JS_QUERY,
            Self::Python => PYTHON_QUERY,
            Self::Go => GO_QUERY,
        }
    }
}

const RUST_QUERY: &str = r"
(function_item name: (_) @name) @fn
(function_signature_item name: (_) @name) @fn
(struct_item name: (_) @name) @struct
(enum_item name: (_) @name) @enum
(union_item name: (_) @name) @union
(trait_item name: (_) @name) @trait
(impl_item type: (_) @name) @impl
(mod_item name: (_) @name) @mod
(const_item name: (_) @name) @const
(static_item name: (_) @name) @static
(type_item name: (_) @name) @type
(macro_definition name: (_) @name) @macro
";

const TS_QUERY: &str = r"
(function_declaration name: (_) @name) @function
(generator_function_declaration name: (_) @name) @function
(class_declaration name: (_) @name) @class
(abstract_class_declaration name: (_) @name) @class
(method_definition name: (_) @name) @method
(method_signature name: (_) @name) @method
(abstract_method_signature name: (_) @name) @method
(interface_declaration name: (_) @name) @interface
(type_alias_declaration name: (_) @name) @type
(enum_declaration name: (_) @name) @enum
(internal_module name: (_) @name) @namespace
(public_field_definition name: (_) @name) @field
(lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @function
(variable_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @function
";

const JS_QUERY: &str = r"
(function_declaration name: (_) @name) @function
(generator_function_declaration name: (_) @name) @function
(class_declaration name: (_) @name) @class
(method_definition name: (_) @name) @method
(field_definition property: (_) @name) @field
(lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @function
(variable_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @function
";

const PYTHON_QUERY: &str = r"
(function_definition name: (_) @name) @def
(class_definition name: (_) @name) @class
";

const GO_QUERY: &str = r"
(function_declaration name: (_) @name) @func
(method_declaration name: (_) @name) @method
(type_spec name: (_) @name) @type
(type_alias name: (_) @name) @type
";

/// One function per grammar, each present or `None` by its feature, so a
/// build without a language still compiles and says what it lacks.
mod grammars {
    use tree_sitter::Language;

    #[cfg(feature = "lang-rust")]
    pub(super) fn rust() -> Option<Language> {
        Some(tree_sitter_rust::LANGUAGE.into())
    }
    #[cfg(not(feature = "lang-rust"))]
    pub(super) fn rust() -> Option<Language> {
        None
    }

    #[cfg(feature = "lang-ts")]
    pub(super) fn typescript() -> Option<Language> {
        Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
    }
    #[cfg(not(feature = "lang-ts"))]
    pub(super) fn typescript() -> Option<Language> {
        None
    }

    #[cfg(feature = "lang-ts")]
    pub(super) fn tsx() -> Option<Language> {
        Some(tree_sitter_typescript::LANGUAGE_TSX.into())
    }
    #[cfg(not(feature = "lang-ts"))]
    pub(super) fn tsx() -> Option<Language> {
        None
    }

    #[cfg(feature = "lang-js")]
    pub(super) fn javascript() -> Option<Language> {
        Some(tree_sitter_javascript::LANGUAGE.into())
    }
    #[cfg(not(feature = "lang-js"))]
    pub(super) fn javascript() -> Option<Language> {
        None
    }

    #[cfg(feature = "lang-python")]
    pub(super) fn python() -> Option<Language> {
        Some(tree_sitter_python::LANGUAGE.into())
    }
    #[cfg(not(feature = "lang-python"))]
    pub(super) fn python() -> Option<Language> {
        None
    }

    #[cfg(feature = "lang-go")]
    pub(super) fn go() -> Option<Language> {
        Some(tree_sitter_go::LANGUAGE.into())
    }
    #[cfg(not(feature = "lang-go"))]
    pub(super) fn go() -> Option<Language> {
        None
    }
}

/// One definition found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Symbol {
    kind: String,
    name: String,
    signature: String,
    start_byte: usize,
    end_byte: usize,
    first_line: usize,
    last_line: usize,
}

/// Outline the file `input` names under `root`.
pub(super) fn run(root: &Path, input: &Value) -> Result<String, String> {
    let named = input
        .get("path")
        .and_then(Value::as_str)
        .ok_or("say which file to outline, as `path`")?;
    let probe = json!({ "path": named }).to_string();
    let path = crate::tools::file_ops::target_path(root, &probe).map_err(|e| e.to_string())?;
    if path.is_dir() {
        return Err(format!(
            "{} is a directory; Outline takes one source file (use LS or Glob for a directory)",
            path.display()
        ));
    }
    let Some(lang) = Lang::of(&path) else {
        return Err(format!(
            "Outline does not support {} files. It reads Rust (.rs), TypeScript (.ts, .tsx), \
             JavaScript (.js, .jsx, .mjs, .cjs), Python (.py) and Go (.go); Read this file instead.",
            path.extension()
                .map_or_else(|| "extension-less".to_string(), |e| format!(".{}", e.to_string_lossy()))
        ));
    };
    let Some(grammar) = lang.grammar() else {
        return Err(format!(
            "this build of incode was made without {} support (the `{}` feature); Read the file \
             instead",
            lang.name(),
            lang.feature()
        ));
    };
    let size = std::fs::metadata(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > MAX_OUTLINE_BYTES {
        return Err(format!(
            "{} is {} KiB; files over {} KiB are not outlined — Grep it for what you need",
            path.display(),
            size / 1024,
            MAX_OUTLINE_BYTES / 1024
        ));
    }
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (symbols, broken_at) = outline(lang, &grammar, &source)?;
    Ok(render(&path, lang, &symbols, broken_at))
}

/// Every definition in `source`, in order, and the first line with a syntax
/// error, if any.
fn outline(
    lang: Lang,
    grammar: &tree_sitter::Language,
    source: &str,
) -> Result<(Vec<Symbol>, Option<usize>), String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(grammar)
        .map_err(|e| format!("the {} grammar could not be loaded: {e}", lang.name()))?;
    let deadline = Instant::now() + PARSE_DEADLINE;
    let mut past_deadline = |_: &tree_sitter::ParseState| {
        if Instant::now() > deadline {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let bytes = source.as_bytes();
    let tree = parser
        .parse_with_options(
            &mut |at, _| bytes.get(at..).unwrap_or_default(),
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut past_deadline)),
        )
        .ok_or_else(|| {
            format!(
                "the file could not be parsed within {} s",
                PARSE_DEADLINE.as_secs()
            )
        })?;
    let root = tree.root_node();

    let query = tree_sitter::Query::new(grammar, lang.query())
        .map_err(|e| format!("the {} outline query is invalid: {e}", lang.name()))?;
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, root, bytes);
    let mut symbols: Vec<Symbol> = Vec::new();
    while let Some(found) = matches.next() {
        let mut definition = None;
        let mut name = None;
        for capture in found.captures {
            let label = names.get(capture.index as usize).copied().unwrap_or("");
            if label == "name" {
                name = capture.node.utf8_text(bytes).ok();
            } else {
                definition = Some((label, capture.node));
            }
        }
        let Some((kind, node)) = definition else {
            continue;
        };
        let symbol = Symbol {
            kind: kind_label(lang, kind, node),
            name: name.unwrap_or("").to_string(),
            signature: signature(node, bytes),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            first_line: node.start_position().row + 1,
            last_line: node.end_position().row + 1,
        };
        if !symbols.contains(&symbol) {
            symbols.push(symbol);
        }
    }
    // Outer before inner: by start, then the longer first.
    symbols.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(b.end_byte.cmp(&a.end_byte))
    });
    let broken_at = root.has_error().then(|| first_error_line(root)).flatten();
    Ok((symbols, broken_at))
}

/// The kind as a reader says it. A Python `def` inside a class is a method.
fn kind_label(lang: Lang, captured: &str, node: tree_sitter::Node<'_>) -> String {
    if lang == Lang::Python && captured == "def" {
        let mut up = node.parent();
        while let Some(parent) = up {
            match parent.kind() {
                "class_definition" => return "method".to_string(),
                "function_definition" => return "def".to_string(),
                _ => up = parent.parent(),
            }
        }
    }
    captured.to_string()
}

/// A definition's signature: its source from the start up to its body,
/// whitespace collapsed and cut. A definition with no body field (a
/// declaration, a constant, an arrow function held in a variable) is cut at
/// its first `{` or line end instead.
fn signature(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let start = node.start_byte();
    let end = body_start(node).unwrap_or_else(|| {
        let text = &source[start..node.end_byte()];
        let cut = text
            .iter()
            .position(|&b| b == b'{' || b == b'\n')
            .unwrap_or(text.len());
        start + cut
    });
    let raw = String::from_utf8_lossy(source.get(start..end.max(start)).unwrap_or_default());
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = flat.trim_end_matches([' ', '{', ':', ';']).trim_end();
    crate::text::truncate(flat, MAX_SIGNATURE_CHARS)
}

/// Where a definition's body begins: its own `body`, or — for a function
/// held in a variable (`const f = (x) => …`) — the body of that function.
fn body_start(node: tree_sitter::Node<'_>) -> Option<usize> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body.start_byte());
    }
    let mut cursor = node.walk();
    let declarators: Vec<_> = node.named_children(&mut cursor).collect();
    declarators
        .into_iter()
        .filter(|child| child.kind() == "variable_declarator")
        .find_map(|declarator| declarator.child_by_field_name("value"))
        .and_then(|value| value.child_by_field_name("body"))
        .map(|body| body.start_byte())
}

/// The line of the first node tree-sitter could not parse.
fn first_error_line(root: tree_sitter::Node<'_>) -> Option<usize> {
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return Some(node.start_position().row + 1);
        }
        if node.has_error() {
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    None
}

/// The outline as the model reads it: one line per symbol, indented by
/// nesting, bounded.
fn render(path: &Path, lang: Lang, symbols: &[Symbol], broken_at: Option<usize>) -> String {
    let mut out = format!(
        "{} ({}, {} symbols)\n",
        path.display(),
        lang.name(),
        symbols.len()
    );
    if let Some(line) = broken_at {
        out.push_str(&format!(
            "(the file has a syntax error near line {line}; symbols after it may be missing)\n"
        ));
    }
    if symbols.is_empty() {
        out.push_str("(no definitions found)\n");
        return out;
    }
    let mut open: Vec<usize> = Vec::new();
    for symbol in symbols.iter().take(MAX_OUTLINE_SYMBOLS) {
        while open.last().is_some_and(|&end| symbol.start_byte >= end) {
            open.pop();
        }
        let indent = "  ".repeat(open.len());
        let lines = if symbol.first_line == symbol.last_line {
            format!("L{}", symbol.first_line)
        } else {
            format!("L{}-{}", symbol.first_line, symbol.last_line)
        };
        out.push_str(&format!(
            "{indent}{} {} [{lines}]: {}\n",
            symbol.kind, symbol.name, symbol.signature
        ));
        open.push(symbol.end_byte);
    }
    if symbols.len() > MAX_OUTLINE_SYMBOLS {
        out.push_str(&format!(
            "… {} more symbols not shown; Grep for a name, or Read a range\n",
            symbols.len() - MAX_OUTLINE_SYMBOLS
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline_of(name: &str, source: &str) -> Result<String, String> {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join(name), source).expect("seed");
        run(dir.path(), &json!({ "path": name }))
    }

    const RUST: &str = r#"
use std::fmt;

/// A point.
pub struct Point { pub x: i32, pub y: i32 }

pub trait Area { fn area(&self) -> f64; }

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

pub fn distance(a: &Point, b: &Point) -> f64 {
    0.0
}

mod inner { pub const LIMIT: usize = 10; pub fn helper<T: Clone>(t: &T) -> T { t.clone() } }
"#;

    /// **SYMBOLS WITH KIND, NAME, SIGNATURE AND LINES, NESTED.**
    #[test]
    fn a_rust_file_is_outlined_nested_with_signatures() {
        let out = outline_of("lib.rs", RUST).expect("outlines");
        assert!(out.contains("(Rust, "), "{out}");
        assert!(
            out.contains("\nfn distance [L15-17]: pub fn distance(a: &Point, b: &Point) -> f64\n"),
            "{out}"
        );
        assert!(out.contains("struct Point [L5]: pub struct Point"), "{out}");
        assert!(
            out.contains("impl Point [L9-13]: impl fmt::Display for Point"),
            "{out}"
        );
        // Nested one level under the impl and the module.
        assert!(
            out.contains(
                "\n  fn fmt [L10-12]: fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("\n  const LIMIT [L19]: pub const LIMIT: usize = 10"),
            "{out}"
        );
        assert!(
            out.contains("\n  fn helper [L19]: pub fn helper<T: Clone>(t: &T) -> T"),
            "{out}"
        );
        assert!(
            out.contains("\n  fn area [L7]: fn area(&self) -> f64"),
            "{out}"
        );
    }

    #[test]
    fn typescript_and_tsx_are_outlined() {
        let ts = r#"
export interface User { id: number; name: string }
export type Id = string | number;
export class UserStore {
  constructor(private readonly path: string) {}
  get(id: number): User | undefined { return undefined; }
}
export function greet(user: User, loud = false): string { return user.name; }
export const add = (a: number, b: number): number => a + b;
enum Color { Red, Green }
"#;
        let out = outline_of("a.ts", ts).expect("outlines");
        assert!(out.contains("interface User [L2]"), "{out}");
        assert!(
            out.contains("type Id [L3]: type Id = string | number"),
            "{out}"
        );
        assert!(
            out.contains("class UserStore [L4-7]: class UserStore"),
            "{out}"
        );
        assert!(
            out.contains("  method get [L6]: get(id: number): User | undefined"),
            "{out}"
        );
        assert!(
            out.contains("function greet [L8]: function greet(user: User, loud = false): string"),
            "{out}"
        );
        assert!(
            out.contains("function add [L9]: const add = (a: number, b: number): number =>"),
            "{out}"
        );
        assert!(out.contains("enum Color [L10]"), "{out}");

        let tsx = "export function App(): JSX.Element { return <div>hi</div>; }\n";
        let out = outline_of("a.tsx", tsx).expect("outlines");
        assert!(out.contains("(TSX, 1 symbols)"), "{out}");
        assert!(
            out.contains("function App [L1]: function App(): JSX.Element"),
            "{out}"
        );
    }

    #[test]
    fn javascript_python_and_go_are_outlined() {
        let js = "class A { run(x) { return x; } }\nfunction top(a, b) { return a; }\nconst f = (x) => x;\n";
        let out = outline_of("a.jsx", js).expect("outlines");
        assert!(out.contains("class A [L1]"), "{out}");
        assert!(out.contains("  method run [L1]: run(x)"), "{out}");
        assert!(
            out.contains("function top [L2]: function top(a, b)"),
            "{out}"
        );
        assert!(out.contains("function f [L3]: const f = (x) =>"), "{out}");

        let py = "class Store:\n    def get(self, key: str) -> int:\n        return 1\n\ndef main() -> None:\n    pass\n";
        let out = outline_of("a.py", py).expect("outlines");
        assert!(out.contains("class Store [L1-3]: class Store"), "{out}");
        assert!(
            out.contains("  method get [L2-3]: def get(self, key: str) -> int"),
            "{out}"
        );
        assert!(out.contains("def main [L5-6]: def main() -> None"), "{out}");

        let go = "package p\n\ntype Point struct {\n\tX int\n}\n\nfunc (p *Point) Dist(q Point) float64 {\n\treturn 0\n}\n\nfunc Top() {}\n";
        let out = outline_of("a.go", go).expect("outlines");
        assert!(out.contains("type Point [L3-5]: Point struct"), "{out}");
        assert!(
            out.contains("method Dist [L7-9]: func (p *Point) Dist(q Point) float64"),
            "{out}"
        );
        assert!(out.contains("func Top [L11]: func Top()"), "{out}");
    }

    /// **AN UNSUPPORTED LANGUAGE IS SAID PLAINLY**, never an empty outline.
    #[test]
    fn an_unsupported_language_is_refused_plainly() {
        let why = outline_of("Main.java", "class Main {}").expect_err("unsupported");
        assert!(why.contains("does not support .java files"), "{why}");
        assert!(why.contains("Read this file instead"), "{why}");
        let why = outline_of("Makefile", "all:\n").expect_err("no extension");
        assert!(why.contains("extension-less"), "{why}");
    }

    #[test]
    fn a_syntax_error_is_reported_with_its_line() {
        let out =
            outline_of("bad.rs", "fn ok() {}\n\nfn broken( {\n").expect("outlines what it can");
        assert!(out.contains("syntax error near line"), "{out}");
        assert!(out.contains("fn ok [L1]"), "{out}");
    }

    #[test]
    fn the_outline_is_bounded_and_counts_the_rest() {
        let source: String = (0..MAX_OUTLINE_SYMBOLS + 12)
            .map(|n| format!("fn f{n}() {{}}\n"))
            .collect();
        let out = outline_of("many.rs", &source).expect("outlines");
        assert_eq!(
            out.lines().filter(|l| l.starts_with("fn f")).count(),
            MAX_OUTLINE_SYMBOLS
        );
        assert!(
            out.contains("… 12 more symbols not shown"),
            "{}",
            &out[out.len() - 120..]
        );
    }

    #[test]
    fn a_file_too_large_or_outside_the_workspace_is_refused() {
        let outer = tempfile::tempdir().expect("tmp");
        let ws = outer.path().join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        std::fs::write(outer.path().join("out.rs"), "fn secret() {}").expect("seed");
        let why = run(&ws, &json!({"path": "../out.rs"})).expect_err("outside");
        assert!(why.contains("escapes the workspace"), "{why}");

        let big = ws.join("big.rs");
        let file = std::fs::File::create(&big).expect("create");
        file.set_len(MAX_OUTLINE_BYTES + 1).expect("sparse");
        let why = run(&ws, &json!({"path": "big.rs"})).expect_err("too big");
        assert!(why.contains("not outlined"), "{why}");
    }
}
