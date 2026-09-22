//! Markdown terminal renderer — formats assistant output with basic styling.
//!
//! Uses pulldown-cmark to parse markdown and crossterm for terminal colors.

use crate::{out, outln};
use std::io::{self, Write};

use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// Render markdown text to the terminal with basic styling.
///
/// Supports: headings (bold), code blocks (cyan), inline code (cyan),
/// bold, italic, list items, and links.
pub fn render_markdown(text: &str) {
    let parser = Parser::new(text);
    let mut in_code_block = false;
    let mut in_heading = false;
    let mut list_depth: u32 = 0;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                crate::cli::console::style(SetForegroundColor(Color::DarkCyan));
                outln!();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                crate::cli::console::style(ResetColor);
                outln!();
            }
            Event::Start(Tag::Heading { .. }) => {
                in_heading = true;
                outln!();
                crate::cli::console::style(SetAttribute(Attribute::Bold));
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                crate::cli::console::style(SetAttribute(Attribute::Reset));
                outln!();
            }
            Event::Start(Tag::Strong) => {
                crate::cli::console::style(SetAttribute(Attribute::Bold));
            }
            Event::End(TagEnd::Strong) => {
                crate::cli::console::style(SetAttribute(Attribute::NoBold));
            }
            Event::Start(Tag::Emphasis) => {
                crate::cli::console::style(SetAttribute(Attribute::Italic));
            }
            Event::End(TagEnd::Emphasis) => {
                crate::cli::console::style(SetAttribute(Attribute::NoItalic));
            }
            Event::Start(Tag::List(_)) => {
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) => {
                let indent = "  ".repeat(list_depth as usize);
                out!("{indent}- ");
            }
            Event::End(TagEnd::Item) => {
                outln!();
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if !in_heading {
                    outln!();
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                crate::cli::console::style(SetForegroundColor(Color::Blue));
                crate::cli::console::style(SetAttribute(Attribute::Underlined));
                out!("[");
                // Store URL for later? For simplicity, just show text
                let _ = dest_url; // suppress unused warning
            }
            Event::End(TagEnd::Link) => {
                out!("]");
                crate::cli::console::style(SetAttribute(Attribute::NoUnderline));
                crate::cli::console::style(ResetColor);
            }
            Event::Text(text) => {
                if in_code_block {
                    // Print code with cyan color (already set)
                    out!("{text}");
                } else {
                    out!("{text}");
                }
                let _ = io::stdout().flush();
            }
            Event::Code(code) => {
                crate::cli::console::style(SetForegroundColor(Color::DarkCyan));
                out!("`{code}`");
                crate::cli::console::style(ResetColor);
            }
            Event::SoftBreak => {
                if in_code_block {
                    outln!();
                } else {
                    out!(" ");
                }
            }
            Event::HardBreak => {
                outln!();
            }
            Event::Rule => {
                outln!("{}", "─".repeat(40));
            }
            _ => {}
        }
    }

    crate::cli::console::style(ResetColor);
    crate::cli::console::style(SetAttribute(Attribute::Reset));
    let _ = io::stdout().flush();
}

/// Detect whether text likely contains markdown formatting.
pub fn looks_like_markdown(text: &str) -> bool {
    text.contains("```")
        || text.contains("## ")
        || text.contains("**")
        || text.contains("- ")
        || text.contains("1. ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markdown() {
        assert!(looks_like_markdown("Here is some ```code```"));
        assert!(looks_like_markdown("## Heading"));
        assert!(looks_like_markdown("**bold** text"));
        assert!(!looks_like_markdown("plain text with no markdown"));
    }

    #[test]
    fn render_does_not_panic() {
        // RENDERING WRITES TO THE GLOBAL CONSOLE, which is why this holds the
        // lock despite asserting nothing about it. Without it this test's
        // output landed in the middle of another module's measurement:
        // `cli::paint`'s wrapping test found
        // `["\u{1b}[21m1234", "567890", " and "]` — the escape codes from
        // `**bold**` and the literal " and " from the line below, interleaved
        // with the line it had just written itself.
        //
        // A test that only checks "it does not panic" still has to leave the
        // shared state it touched alone.
        let _guard = crate::cli::console::test_lock();

        // Just ensure it doesn't panic on various inputs
        render_markdown("# Hello\n\nSome **bold** and `code`.\n\n```rust\nfn main() {}\n```\n");
        render_markdown("");
        render_markdown("plain text");

        // Left clean, so the next holder of the lock starts from nothing
        // rather than from this.
        crate::cli::console::reset();
    }
}
