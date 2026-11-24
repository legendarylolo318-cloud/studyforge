//! Markdown -> flat list of display blocks (for the live preview pane).
//!
//! Slint cannot lay out mixed bold/plain runs inside one wrapped paragraph, so
//! blocks carry whole-block `bold` / `italic` flags (set when the entire block
//! is emphasised); partial emphasis markers are dropped and the text kept.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    ListItem,
    Code,
    Quote,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    /// List nesting depth (0-based) / quote depth.
    pub indent: u8,
}

fn level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[derive(Default)]
struct Cur {
    text: String,
    /// total chars of text that were inside strong / emphasis
    bold_chars: usize,
    italic_chars: usize,
    plain_chars: usize,
}

pub fn render_blocks(md: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut cur = Cur::default();
    let mut kinds: Vec<BlockKind> = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut bold = 0usize;
    let mut italic = 0usize;
    let mut quote = 0u8;
    let mut in_code = false;
    let mut link_dest: Vec<String> = Vec::new();
    let mut para_in_item = 0usize;

    let flush = |cur: &mut Cur, kind: BlockKind, indent: u8, out: &mut Vec<Block>| {
        let text = std::mem::take(&mut cur.text);
        let total = cur.bold_chars.max(cur.italic_chars) + cur.plain_chars;
        let all_bold = total > 0 && cur.bold_chars >= total.saturating_sub(0) && cur.plain_chars == 0 && cur.bold_chars > 0;
        let all_italic = total > 0 && cur.plain_chars == 0 && cur.italic_chars > 0;
        *cur = Cur::default();
        let trimmed = if kind == BlockKind::Code { text.trim_end_matches('\n').to_string() } else { text.trim().to_string() };
        if !trimmed.is_empty() || kind == BlockKind::Rule {
            out.push(Block { kind, text: trimmed, bold: all_bold, italic: all_italic, indent });
        }
    };

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::Heading { level: l, .. }) => kinds.push(BlockKind::Heading(level(l))),
            Event::Start(Tag::Paragraph) => {
                if kinds.last() == Some(&BlockKind::ListItem) {
                    para_in_item += 1;
                    if !cur.text.ends_with(' ') && !cur.text.is_empty() && !cur.text.ends_with("• ") {
                        cur.text.push(' ');
                    }
                } else {
                    kinds.push(if quote > 0 { BlockKind::Quote } else { BlockKind::Paragraph });
                }
            }
            Event::Start(Tag::BlockQuote(_)) => quote += 1,
            Event::End(TagEnd::BlockQuote(_)) => quote = quote.saturating_sub(1),
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
                kinds.push(BlockKind::Code);
            }
            Event::Start(Tag::List(start)) => {
                if kinds.last() == Some(&BlockKind::ListItem) && !cur.text.trim().is_empty() {
                    let indent = lists.len().saturating_sub(1) as u8;
                    flush(&mut cur, BlockKind::ListItem, indent, &mut out);
                }
                lists.push(start)
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                kinds.push(BlockKind::ListItem);
                let depth = lists.len().saturating_sub(1);
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                let _ = depth;
                cur.text.push_str(&marker);
                cur.plain_chars += marker.chars().count();
            }
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => link_dest.push(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                if let Some(d) = link_dest.pop() {
                    let s = format!(" ({d})");
                    cur.plain_chars += s.chars().count();
                    cur.text.push_str(&s);
                }
            }
            Event::TaskListMarker(done) => {
                let m = if done { "☑ " } else { "☐ " };
                cur.text.push_str(m);
                cur.plain_chars += 2;
            }
            Event::Text(t) | Event::Code(t) => {
                let n = t.chars().count();
                if in_code {
                    cur.plain_chars += n;
                } else if bold > 0 {
                    cur.bold_chars += n;
                    if italic > 0 {
                        cur.italic_chars += n;
                    }
                } else if italic > 0 {
                    cur.italic_chars += n;
                } else {
                    cur.plain_chars += n;
                }
                cur.text.push_str(&t);
            }
            Event::SoftBreak => cur.text.push(' '),
            Event::HardBreak => cur.text.push('\n'),
            Event::Rule => out.push(Block { kind: BlockKind::Rule, text: String::new(), bold: false, italic: false, indent: 0 }),
            Event::End(TagEnd::Heading(_)) | Event::End(TagEnd::Paragraph) | Event::End(TagEnd::CodeBlock) | Event::End(TagEnd::Item) => {
                if matches!(ev, Event::End(TagEnd::CodeBlock)) {
                    in_code = false;
                }
                // Paragraphs inside list items belong to the item, which flushes at its end.
                if matches!(ev, Event::End(TagEnd::Paragraph)) && para_in_item > 0 {
                    para_in_item -= 1;
                    continue;
                }
                let kind = kinds.pop().unwrap_or(BlockKind::Paragraph);
                let indent = match kind {
                    BlockKind::ListItem => lists.len().saturating_sub(1) as u8,
                    BlockKind::Quote => quote,
                    _ => 0,
                };
                flush(&mut cur, kind, indent, &mut out);
            }
            _ => {}
        }
    }
    flush(&mut cur, BlockKind::Paragraph, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(b: &[Block]) -> Vec<String> {
        b.iter().map(|b| b.text.clone()).collect()
    }

    #[test]
    fn headings_and_paragraphs() {
        let b = render_blocks("# Title\n\nSome *text* here.\n\n## Sub");
        assert_eq!(b[0].kind, BlockKind::Heading(1));
        assert_eq!(b[0].text, "Title");
        assert_eq!(b[1].kind, BlockKind::Paragraph);
        assert_eq!(b[1].text, "Some text here.");
        assert!(!b[1].italic, "partial emphasis is not block italic");
        assert_eq!(b[2].kind, BlockKind::Heading(2));
    }

    #[test]
    fn whole_block_emphasis() {
        let b = render_blocks("**all bold**\n\n*all italic*");
        assert!(b[0].bold && !b[0].italic);
        assert!(b[1].italic && !b[1].bold);
    }

    #[test]
    fn lists_ordered_and_bullets() {
        let b = render_blocks("- a\n- b\n  - nested\n\n1. one\n2. two");
        assert_eq!(texts(&b), vec!["• a", "• b", "• nested", "1. one", "2. two"]);
        assert_eq!(b[2].indent, 1);
        assert!(b.iter().all(|x| x.kind == BlockKind::ListItem));
    }

    #[test]
    fn code_block_keeps_newlines() {
        let b = render_blocks("```rust\nfn main() {\n}\n```");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, BlockKind::Code);
        assert_eq!(b[0].text, "fn main() {\n}");
    }

    #[test]
    fn links_as_text() {
        let b = render_blocks("see [docs](https://example.com) now");
        assert_eq!(b[0].text, "see docs (https://example.com) now");
    }

    #[test]
    fn quote_rule_and_tasks() {
        let b = render_blocks("> quoted\n\n---\n\n- [x] done\n- [ ] todo");
        assert_eq!(b[0].kind, BlockKind::Quote);
        assert_eq!(b[1].kind, BlockKind::Rule);
        assert_eq!(b[2].text, "• ☑ done");
        assert_eq!(b[3].text, "• ☐ todo");
    }

    #[test]
    fn empty_and_html_do_not_panic() {
        assert!(render_blocks("").is_empty());
        let _ = render_blocks("<script>alert(1)</script>\n\n<b>x</b>");
    }
}
