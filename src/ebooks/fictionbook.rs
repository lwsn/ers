use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use base64::Engine;

use super::{Ebook, xml};
use crate::models::{BookMetadata, TocEntry};
use crate::util::decode_document;

pub struct FictionBook {
    path: String,
    /// Body sections serialized as HTML, indexed by content id ("0", "1", ...)
    sections: Vec<String>,
    contents: Vec<String>,
    toc_entries: Vec<TocEntry>,
    metadata: BookMetadata,
    /// binary id -> (content-type, base64 data)
    binaries: HashMap<String, (String, String)>,
}

impl FictionBook {
    pub fn new(path: &str) -> Result<Self> {
        Ok(Self {
            path: std::path::absolute(path)?.to_string_lossy().into_owned(),
            sections: Vec::new(),
            contents: Vec::new(),
            toc_entries: Vec::new(),
            metadata: BookMetadata::default(),
            binaries: HashMap::new(),
        })
    }
}

fn escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Serialize an FB2 element as HTML the parser understands. A few FB2 tags
/// are mapped onto their HTML counterparts so they get proper formatting.
fn to_html(node: roxmltree::Node, out: &mut String) {
    if node.is_text() {
        out.push_str(&escape(node.text().unwrap_or_default(), false));
        return;
    }
    if !node.is_element() {
        return;
    }
    let name = node.tag_name().name();
    let tag = match name {
        "emphasis" => "em",
        "v" | "subtitle" | "text-author" => "p",
        "cite" | "epigraph" => "blockquote",
        "empty-line" => {
            out.push_str("<br/>");
            return;
        }
        other => other,
    };
    out.push('<');
    out.push_str(tag);
    for attr in node.attributes() {
        out.push_str(&format!(" {}=\"{}\"", attr.name(), escape(attr.value(), true)));
    }
    if tag == "image" {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for child in node.children() {
        to_html(child, out);
    }
    out.push_str(&format!("</{tag}>"));
}

impl Ebook for FictionBook {
    fn path(&self) -> &str {
        &self.path
    }

    fn initialize(&mut self) -> Result<()> {
        let bytes = std::fs::read(&self.path).with_context(|| format!("cannot open {}", self.path))?;
        let text = decode_document(&bytes);
        let doc = xml::parse(&text)?;
        let root = doc.root_element();

        let find_text = |name: &str| {
            xml::descendant(root, name).and_then(|n| n.text()).map(|s| s.trim().to_string())
        };
        let mut author = find_text("first-name");
        if let Some(last) = find_text("last-name") {
            author = match author {
                Some(first) if !first.is_empty() => Some(format!("{first} {last}")),
                _ => Some(last),
            };
        }
        self.metadata = BookMetadata {
            title: find_text("book-title"),
            creator: author,
            date: find_text("date"),
            identifier: find_text("id"),
            ..Default::default()
        };

        for body in xml::children(root, "body") {
            for section in body.children().filter(|n| n.is_element()) {
                let n = self.sections.len();
                if let Some(title) = xml::child(section, "title") {
                    let label = xml::all_text(title).split_whitespace().collect::<Vec<_>>().join(" ");
                    self.toc_entries.push(TocEntry { label, content_index: n, section: None });
                }
                let mut html = String::new();
                to_html(section, &mut html);
                self.sections.push(html);
                self.contents.push(n.to_string());
            }
        }
        if self.sections.is_empty() {
            return Err(anyhow!("no <body> content found"));
        }

        for binary in xml::children(root, "binary") {
            if let Some(id) = binary.attribute("id") {
                let ctype = binary.attribute("content-type").unwrap_or("image/jpeg").to_string();
                let data = binary.text().unwrap_or_default().to_string();
                self.binaries.insert(id.to_string(), (ctype, data));
            }
        }
        Ok(())
    }

    fn contents(&self) -> &[String] {
        &self.contents
    }

    fn toc_entries(&self) -> &[TocEntry] {
        &self.toc_entries
    }

    fn get_meta(&self) -> BookMetadata {
        self.metadata.clone()
    }

    fn get_raw_text(&mut self, content: &str) -> Result<String> {
        let idx: usize = content.parse()?;
        self.sections.get(idx).cloned().ok_or_else(|| anyhow!("no content #{idx}"))
    }

    fn get_img_bytestr(&mut self, imgid: &str) -> Result<(String, Vec<u8>)> {
        let imgid = imgid.replace('#', "");
        let (ctype, data) = self
            .binaries
            .get(&imgid)
            .ok_or_else(|| anyhow!("image '{imgid}' not found"))?;
        let clean: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(clean)?;
        let ext = ctype.split('/').nth(1).unwrap_or("img");
        Ok((format!("{imgid}.{ext}"), bytes))
    }
}
