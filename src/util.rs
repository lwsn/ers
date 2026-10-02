//! Small helpers shared across the crate, including Rust equivalents of a
//! few Python stdlib behaviours: `textwrap.wrap`, `str.center`,
//! `urllib.parse.unquote`, `shutil.which` and `difflib.SequenceMatcher`.

use std::borrow::Cow;
use std::path::PathBuf;

use unicode_width::UnicodeWidthChar;

pub fn is_url(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    let scheme_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    scheme_ok && !netloc.is_empty()
}

/// Truncate text, eg. `truncate("This is long silly dummy text", "...", 12, 3)`
/// returns `"This...ly dummy text"`.
pub fn truncate(text: &str, substitution: &str, maxlen: usize, startsub: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= maxlen {
        return text.to_string();
    }
    let startsub = startsub.min(maxlen);
    let sub: Vec<char> = substitution.chars().collect();
    let lensu = sub.len();
    let beg: String = chars[..startsub].iter().collect();
    let mid: String = if lensu <= maxlen - startsub {
        substitution.to_string()
    } else {
        sub[..maxlen - startsub].iter().collect()
    };
    let end: String = if lensu < maxlen - startsub {
        let from = chars.len() - (maxlen - startsub - lensu);
        chars[from..].iter().collect()
    } else {
        String::new()
    };
    beg + &mid + &end
}

/// Resolve a relative reference against a base path or URL the way
/// `urllib.parse.urljoin` does, eg. `/foo/bar/book.html` + `../img.png` =
/// `/foo/img.png`.
pub fn resolve_path(base: &str, relative: &str) -> String {
    if is_url(relative) {
        return relative.to_string();
    }
    if is_url(base) {
        let scheme_end = base.find("://").unwrap() + 3;
        let path_start = base[scheme_end..]
            .find(['/', '?', '#'])
            .map(|i| i + scheme_end)
            .unwrap_or(base.len());
        let origin = &base[..path_start];
        if let Some(rest) = relative.strip_prefix("//") {
            return format!("{}//{}", &base[..scheme_end - 2], rest);
        }
        let base_path = base[path_start..].split(['?', '#']).next().unwrap_or("");
        let base_path = if base_path.is_empty() { "/" } else { base_path };
        return format!("{}{}", origin, resolve_path(base_path, relative));
    }
    if relative.is_empty() {
        return base.to_string();
    }

    let joined = if relative.starts_with('/') {
        relative.to_string()
    } else {
        let dir = match base.rfind('/') {
            Some(i) => &base[..=i],
            None => "",
        };
        format!("{dir}{relative}")
    };

    let absolute = joined.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = joined.split('/').collect();
    for (i, seg) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        match *seg {
            "" | "." => {
                if last && i > 0 {
                    segments.push("");
                }
            }
            ".." => {
                segments.pop();
                if last {
                    segments.push("");
                }
            }
            s => segments.push(s),
        }
    }
    let path = segments.join("/");
    if absolute { format!("/{path}") } else { path }
}

/// Percent-decode a string (`urllib.parse.unquote`).
pub fn unquote(s: &str) -> Cow<'_, str> {
    if !s.contains('%') {
        return Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// Locate an executable on `PATH` (`shutil.which`).
pub fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(cmd);
    if candidate.components().count() > 1 {
        return candidate.is_file().then_some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let p = dir.join(cmd);
        if p.is_file() {
            return Some(p);
        }
        if cfg!(windows) {
            let exe = dir.join(format!("{cmd}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
        None
    })
}

pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// `str.center(width)` measured in terminal columns.
pub fn center(s: &str, width: usize) -> String {
    let w = str_width(s);
    if w >= width {
        return s.to_string();
    }
    let pad = width - w;
    let left = pad / 2 + (pad & width & 1);
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(pad - left))
}

/// A wrapped line along with the char offset of its first char in the
/// source string, so inline styles can be mapped exactly onto wrapped text.
#[derive(Debug, Clone, PartialEq)]
pub struct WrappedLine {
    pub text: String,
    pub offset: usize,
}

/// Greedy word wrap modelled on Python's `textwrap.wrap` with its defaults
/// (`drop_whitespace`, `break_long_words`), measured in terminal columns.
/// Every whitespace char is treated as a single space, so char offsets in
/// the output line up with the input.
pub fn wrap(text: &str, width: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let chars: Vec<char> = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();

    // chunks: (start, end) runs of either spaces or non-spaces
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let space = chars[i] == ' ';
        let start = i;
        while i < chars.len() && (chars[i] == ' ') == space {
            i += 1;
        }
        chunks.push((start, i));
    }
    let is_space = |c: &(usize, usize)| chars[c.0] == ' ';
    let width_of = |c: &(usize, usize)| chars[c.0..c.1].iter().map(|&c| char_width(c)).sum::<usize>();

    let mut lines: Vec<WrappedLine> = Vec::new();
    let mut idx = 0;
    while idx < chunks.len() {
        if !lines.is_empty() && is_space(&chunks[idx]) {
            idx += 1;
            continue;
        }
        let mut cur: Vec<(usize, usize)> = Vec::new();
        let mut cur_len = 0;
        while idx < chunks.len() {
            let l = width_of(&chunks[idx]);
            if cur_len + l <= width {
                cur.push(chunks[idx]);
                cur_len += l;
                idx += 1;
            } else {
                break;
            }
        }
        if idx < chunks.len() && width_of(&chunks[idx]) > width {
            // break the long word to fill the rest of this line
            let space_left = width.saturating_sub(cur_len).max(1);
            let (start, end) = chunks[idx];
            let mut split = start;
            let mut w = 0;
            while split < end {
                let cw = char_width(chars[split]);
                if w + cw > space_left && split > start {
                    break;
                }
                w += cw;
                split += 1;
                if w >= space_left {
                    break;
                }
            }
            if cur.is_empty() || split > start {
                cur.push((start, split));
                chunks[idx] = (split, end);
                if split == end {
                    idx += 1;
                }
            }
        }
        if cur.last().is_some_and(is_space) {
            cur.pop();
        }
        if let (Some(first), Some(last)) = (cur.first(), cur.last()) {
            lines.push(WrappedLine {
                text: chars[first.0..last.1].iter().collect(),
                offset: first.0,
            });
        }
    }
    lines
}

/// Sum of the sizes of `difflib.SequenceMatcher(None, a, b).get_matching_blocks()`.
pub fn matching_chars(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut total = 0;
    let mut queue = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(&a, &b, alo, ahi, blo, bhi);
        if k > 0 {
            total += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    total
}

fn longest_match(
    a: &[char],
    b: &[char],
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
    let mut prev = vec![0usize; bhi - blo + 1];
    for i in alo..ahi {
        let mut cur = vec![0usize; bhi - blo + 1];
        for j in blo..bhi {
            if a[i] == b[j] {
                let k = prev[j - blo] + 1;
                cur[j - blo + 1] = k;
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        prev = cur;
    }
    (besti, bestj, bestsize)
}

/// Decode raw bytes of an XML/HTML document, honouring a BOM or an
/// `encoding="..."` declaration in the prolog (FB2 files are often cp1251).
pub fn decode_document(bytes: &[u8]) -> String {
    if let Some((enc, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return enc.decode(bytes).0.into_owned();
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let declared = head.find("encoding=").and_then(|i| {
        let rest = &head[i + 9..];
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let rest = &rest[1..];
        rest.find(quote).map(|end| rest[..end].to_string())
    });
    if let Some(enc) = declared.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        return enc.decode(bytes).0.into_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Number of non-whitespace chars, used for reading-progress calculation.
pub fn count_letters(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths() {
        assert_eq!(resolve_path("/aaa/bbb/book.html", "../ccc.png"), "/aaa/ccc.png");
        assert_eq!(resolve_path("/aaa/bbb/book.html", "../../ccc.png"), "/ccc.png");
        assert_eq!(resolve_path("aaa/bbb/book.html", "../../ccc.png"), "ccc.png");
        assert_eq!(resolve_path("OEBPS/", "Text/ch1.xhtml"), "OEBPS/Text/ch1.xhtml");
        assert_eq!(resolve_path("", "ch1.xhtml"), "ch1.xhtml");
        assert_eq!(
            resolve_path("https://example.com/a/b.html", "../img/x.png"),
            "https://example.com/img/x.png"
        );
        assert_eq!(
            resolve_path("https://example.com/a/b.html", "/x.png"),
            "https://example.com/x.png"
        );
    }

    #[test]
    fn detects_urls() {
        assert!(is_url("https://example.com"));
        assert!(!is_url("/home/me/book.epub"));
        assert!(!is_url("C:\\books\\x.epub"));
        assert!(!is_url("file://"));
    }

    #[test]
    fn truncates() {
        assert_eq!(truncate("This is long silly dummy text", "...", 12, 3), "Thi...y text");
        assert_eq!(truncate("short", "...", 12, 3), "short");
    }

    #[test]
    fn wraps_like_textwrap() {
        let text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Curabitur rutrum massa.";
        let lines: Vec<String> = wrap(text, 17).into_iter().map(|l| l.text).collect();
        assert_eq!(
            lines,
            vec![
                "Lorem ipsum dolor",
                "sit amet,",
                "consectetur",
                "adipiscing elit.",
                "Curabitur rutrum",
                "massa."
            ]
        );
        let w = wrap(text, 17);
        assert_eq!(w[1].offset, 18);
        assert_eq!(wrap("", 10), vec![]);
        assert_eq!(wrap("   ", 10), vec![]);
        let long: Vec<String> = wrap("abcdefghij", 4).into_iter().map(|l| l.text).collect();
        assert_eq!(long, vec!["abcd", "efgh", "ij"]);
        let mixed: Vec<String> = wrap("ab cdefghij", 4).into_iter().map(|l| l.text).collect();
        assert_eq!(mixed, vec!["ab c", "defg", "hij"]);
    }

    #[test]
    fn centers() {
        assert_eq!(center("ab", 6), "  ab  ");
        assert_eq!(center("abc", 6), " abc  ");
        assert_eq!(center("abc", 7), "  abc  ");
    }

    #[test]
    fn unquotes() {
        assert_eq!(unquote("a%20b.xhtml"), "a b.xhtml");
        assert_eq!(unquote("100%"), "100%");
    }

    #[test]
    fn sequence_matching() {
        assert_eq!(matching_chars("abxcd", "abcd"), 4);
        assert_eq!(matching_chars("count of monte cristo - dumas", "count monte"), 11);
    }
}
