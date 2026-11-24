//! HTML -> plain text (keeping line breaks) and cloze handling for Anki fields.

fn decode_entity(e: &str) -> Option<String> {
    Some(match e {
        "nbsp" => " ".into(),
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" | "#39" => "'".into(),
        _ => {
            let n = e.strip_prefix('#')?;
            let code = if let Some(h) = n.strip_prefix(['x', 'X']) {
                u32::from_str_radix(h, 16).ok()?
            } else {
                n.parse().ok()?
            };
            char::from_u32(code)?.to_string()
        }
    })
}

/// Strip HTML tags and decode entities; block-level tags and `<br>` become
/// newlines. `[sound:...]` references and `<img>` tags are dropped (media is
/// not imported in v1). `<style>` / `<script>` contents are removed.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let chars: Vec<char> = html.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            // find end of tag
            let Some(end) = chars[i..].iter().position(|&x| x == '>') else {
                out.push(c);
                i += 1;
                continue;
            };
            let tag: String = chars[i + 1..i + end].iter().collect();
            let name = tag
                .trim_start_matches('/')
                .split(|ch: char| ch.is_whitespace() || ch == '/')
                .next()
                .unwrap_or("")
                .to_lowercase();
            i += end + 1;
            match name.as_str() {
                "br" => out.push('\n'),
                "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "table"
                | "blockquote" | "hr" => {
                    // opening block tags also separate lines, but avoid piling up newlines
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                }
                "style" | "script" if !tag.starts_with('/') => {
                    // skip to the closing tag
                    let rest: String = chars[i..].iter().collect::<String>().to_lowercase();
                    let close = format!("</{name}");
                    if let Some(p) = rest.find(&close) {
                        let skip_chars = rest[..p].chars().count();
                        i += skip_chars;
                    } else {
                        i = chars.len();
                    }
                }
                _ => {}
            }
        } else if c == '&' {
            if let Some(semi) = chars[i..].iter().take(10).position(|&x| x == ';') {
                let ent: String = chars[i + 1..i + semi].iter().collect();
                if let Some(rep) = decode_entity(&ent) {
                    out.push_str(&rep);
                    i += semi + 1;
                    continue;
                }
            }
            out.push('&');
            i += 1;
        } else if c == '[' && chars[i..].iter().take(7).collect::<String>() == "[sound:" {
            if let Some(end) = chars[i..].iter().position(|&x| x == ']') {
                i += end + 1;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    // collapse >2 consecutive newlines, trim trailing spaces on lines
    let mut result = String::new();
    let mut blank = 0;
    for line in out.split('\n') {
        let l = line.trim_end();
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(l);
    }
    result.trim().to_string()
}

/// Anki cloze deletion: `{{c1::answer::hint}}`.
/// Returns (front with `[...]`, back with answers revealed).
pub fn cloze_to_front_back(text: &str) -> (String, String) {
    let mut front = String::new();
    let mut back = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{c") {
        let (before, after) = rest.split_at(start);
        front.push_str(before);
        back.push_str(before);
        // after = "{{cN::...}}"
        if let (Some(sep), Some(end)) = (after.find("::"), after.find("}}")) {
            if sep < end {
                let inner = &after[sep + 2..end];
                let answer = inner.split("::").next().unwrap_or(inner);
                front.push_str("[...]");
                back.push_str(answer);
                rest = &after[end + 2..];
                continue;
            }
        }
        front.push_str("{{c");
        back.push_str("{{c");
        rest = &after[3..];
    }
    front.push_str(rest);
    back.push_str(rest);
    (front, back)
}

pub fn has_cloze(s: &str) -> bool {
    s.contains("{{c") && s.contains("::")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_keeps_breaks() {
        assert_eq!(html_to_text("Hello<br>World"), "Hello\nWorld");
        assert_eq!(html_to_text("<div>a</div><div>b</div>"), "a\nb");
        assert_eq!(html_to_text("<b>bold</b> &amp; <i>it</i>&nbsp;x"), "bold & it x");
        assert_eq!(html_to_text("<ul><li>one</li><li>two</li></ul>"), "one\ntwo");
    }

    #[test]
    fn drops_media_and_scripts() {
        assert_eq!(html_to_text("Word [sound:a.mp3]<img src=\"x.jpg\">"), "Word");
        assert_eq!(html_to_text("a<style>.x{color:red}</style>b"), "ab");
        assert_eq!(html_to_text("a<script>alert('<b>')</script>b"), "ab");
    }

    #[test]
    fn numeric_entities_and_lone_symbols() {
        assert_eq!(html_to_text("&#65;&#x42; &unknown; a < b & c"), "AB &unknown; a < b & c");
    }

    #[test]
    fn collapses_blank_lines() {
        assert_eq!(html_to_text("a<br><br><br><br>b"), "a\n\nb");
    }

    #[test]
    fn cloze() {
        let (f, b) = cloze_to_front_back("The {{c1::mitochondria::organelle}} makes {{c2::ATP}}.");
        assert_eq!(f, "The [...] makes [...].");
        assert_eq!(b, "The mitochondria makes ATP.");
        assert!(has_cloze("{{c1::x}}"));
        assert!(!has_cloze("plain"));
        // malformed does not hang
        let (f, _) = cloze_to_front_back("broken {{c1 no end");
        assert!(f.contains("broken"));
    }
}
