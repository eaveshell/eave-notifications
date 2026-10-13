//! The spec's body markup (a little HTML: `<b>`, `<i>`, `<u>`, `<a href>`, `<img>`), made plain.
//! eave shows notifications as plain text, so tags go and entities become their characters.

/// `text` without its tags, with entities decoded and `<br>` as a line break. A `<` that doesn't
/// start a tag stays, so a body without markup comes through unchanged.
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['<', '&']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('<') {
            match tag(rest) {
                Some((name, length)) => {
                    if name.eq_ignore_ascii_case("br") {
                        out.push('\n');
                    }
                    rest = &rest[length..];
                }
                None => {
                    out.push('<');
                    rest = &rest[1..];
                }
            }
        } else {
            match entity(rest) {
                Some((character, length)) => {
                    out.push(character);
                    rest = &rest[length..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The tag at the start of `text`, if it is one: its name and its length. Tags start with a
/// letter, `/` or `!`, and end at the next `>`.
fn tag(text: &str) -> Option<(&str, usize)> {
    let inside = text.strip_prefix('<')?;
    let first = inside.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '/' || first == '!') {
        return None;
    }
    let end = inside.find('>')?;
    let name = inside[..end]
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("");
    Some((name, end + 2))
}

/// The entity at the start of `text`, if it is one: its character and its length.
fn entity(text: &str) -> Option<(char, usize)> {
    let end = text.find(';').filter(|end| *end <= 10)?;
    let name = &text[1..end];
    let character = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((character, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_go_and_text_stays() {
        assert_eq!(strip("<b>Bold</b> and <i>it</i>"), "Bold and it");
        assert_eq!(strip(r#"see <a href="https://x.org">this</a>"#), "see this");
        assert_eq!(strip("one<br/>two<br>three"), "one\ntwo\nthree");
        assert_eq!(strip(r#"<img src="a.png" alt="pic"/>after"#), "after");
    }

    #[test]
    fn entities_decode_and_stray_signs_stay() {
        assert_eq!(strip("a &amp; b &lt;3 &#233;&#x41;"), "a & b <3 éA");
        assert_eq!(strip("1 < 2 & 3 > 2"), "1 < 2 & 3 > 2");
        assert_eq!(strip("&bogus; &"), "&bogus; &");
    }
}
