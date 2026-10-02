use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result, anyhow};
use zip::ZipArchive;

use super::{Ebook, xml};
use crate::models::{BookMetadata, TocEntry};
use crate::parser::{Token, tokenize};
use crate::util::{decode_document, resolve_path, unquote};

const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const NCX_MEDIA_TYPE: &str = "application/x-dtbncx+xml";

pub struct Epub {
    path: String,
    file: ZipArchive<File>,
    root_filepath: String,
    contents: Vec<String>,
    toc_entries: Vec<TocEntry>,
    metadata: BookMetadata,
}

struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
    properties: String,
}

impl Epub {
    pub fn open(path: &str) -> Result<Self> {
        let abs = std::path::absolute(path)?.to_string_lossy().into_owned();
        let file = ZipArchive::new(File::open(path).with_context(|| format!("cannot open {path}"))?)?;
        Ok(Self {
            path: abs,
            file,
            root_filepath: String::new(),
            contents: Vec::new(),
            toc_entries: Vec::new(),
            metadata: BookMetadata::default(),
        })
    }

    fn read_entry(&mut self, name: &str) -> Result<Vec<u8>> {
        let mut entry = self
            .file
            .by_name(name)
            .with_context(|| format!("'{name}' not found in epub"))?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn read_text(&mut self, name: &str) -> Result<String> {
        Ok(decode_document(&self.read_entry(name)?))
    }

    fn get_metadata(opf: &roxmltree::Document) -> BookMetadata {
        let mut meta = BookMetadata::default();
        for name in BookMetadata::FIELDS {
            let found = opf.descendants().find(|n| {
                n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(DC_NS)
            });
            if let Some(text) = found.and_then(|n| n.text()) {
                *meta.field_mut(name).unwrap() = Some(text.to_string());
            }
        }
        meta
    }

    fn parse_ncx(toc: &str) -> Result<Vec<(String, String)>> {
        let doc = xml::parse(toc)?;
        let mut entries = Vec::new();
        let Some(nav_map) = xml::descendant(doc.root(), "navMap") else {
            return Ok(entries);
        };
        for nav_point in nav_map.descendants().filter(|n| xml::is(n, "navPoint")) {
            let src = xml::child(nav_point, "content").and_then(|c| c.attribute("src"));
            let label = xml::child(nav_point, "navLabel")
                .and_then(|l| xml::child(l, "text"))
                .and_then(|t| t.text());
            if let (Some(src), Some(label)) = (src, label) {
                entries.push((label.to_string(), src.to_string()));
            }
        }
        Ok(entries)
    }

    /// EPUB3 navigation document: `<a>` elements inside `<nav epub:type="toc">`.
    /// Parsed with the lenient HTML tokenizer since nav docs are XHTML that
    /// frequently contain entities XML parsers reject.
    fn parse_nav(toc: &str) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        let mut nav_depth = 0usize;
        let mut toc_depth: Option<usize> = None;
        let mut current: Option<(String, String)> = None;
        tokenize(toc, |tok| match tok {
            Token::Start { name, attrs, self_closing } => {
                if name == "nav" && !self_closing {
                    nav_depth += 1;
                    let is_toc = attrs
                        .iter()
                        .any(|(k, v)| k.ends_with("type") && v.split_whitespace().any(|t| t == "toc"));
                    if is_toc && toc_depth.is_none() {
                        toc_depth = Some(nav_depth);
                    }
                } else if name == "a" && toc_depth.is_some() && !self_closing {
                    let href = attrs.into_iter().find(|(k, _)| k == "href").map(|(_, v)| v);
                    current = href.map(|h| (String::new(), h));
                }
            }
            Token::End { name } => {
                if name == "nav" {
                    if toc_depth == Some(nav_depth) {
                        toc_depth = None;
                    }
                    nav_depth = nav_depth.saturating_sub(1);
                } else if name == "a"
                    && let Some((label, href)) = current.take() {
                        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
                        entries.push((label, href));
                    }
            }
            Token::Data(d) => {
                if let Some((label, _)) = current.as_mut() {
                    label.push_str(&d);
                }
            }
        });
        entries
    }
}

impl Ebook for Epub {
    fn path(&self) -> &str {
        &self.path
    }

    fn initialize(&mut self) -> Result<()> {
        let container = self.read_text("META-INF/container.xml")?;
        let container = xml::parse(&container)?;
        self.root_filepath = xml::descendant(container.root(), "rootfile")
            .and_then(|n| n.attribute("full-path"))
            .ok_or_else(|| anyhow!("rootfile not found in container.xml"))?
            .to_string();

        let opf_text = self.read_text(&self.root_filepath.clone())?;
        let opf = xml::parse(&opf_text)?;
        let version = opf.root_element().attribute("version").unwrap_or("2.0").to_string();
        self.metadata = Self::get_metadata(&opf);

        let manifest: Vec<ManifestItem> = xml::descendant(opf.root(), "manifest")
            .map(|m| {
                m.children()
                    .filter(|n| n.is_element())
                    .map(|n| ManifestItem {
                        id: n.attribute("id").unwrap_or_default().to_string(),
                        href: n.attribute("href").unwrap_or_default().to_string(),
                        media_type: n.attribute("media-type").unwrap_or_default().to_string(),
                        properties: n.attribute("properties").unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let is_nav = |m: &ManifestItem| m.properties.split_whitespace().any(|p| p == "nav");
        let spine = xml::descendant(opf.root(), "spine");
        let mut contents = Vec::new();
        for itemref in spine.iter().flat_map(|s| s.children().filter(|n| n.is_element())) {
            let Some(idref) = itemref.attribute("idref") else { continue };
            if let Some(item) = manifest
                .iter()
                .find(|m| m.id == idref && m.media_type != NCX_MEDIA_TYPE && !is_nav(m))
            {
                contents.push(resolve_path(&self.root_filepath, &unquote(&item.href)));
            }
        }
        self.contents = contents;

        // table of contents: prefer the format matching the epub version,
        // fall back to the other one
        let ncx = manifest.iter().find(|m| m.media_type == NCX_MEDIA_TYPE);
        let nav = manifest.iter().find(|m| is_nav(m));
        let candidates = if version.starts_with('3') { [nav, ncx] } else { [ncx, nav] };
        for item in candidates.into_iter().flatten() {
            let toc_path = resolve_path(&self.root_filepath, &unquote(&item.href));
            let Ok(toc_text) = self.read_text(&toc_path) else { continue };
            let raw_entries = if item.media_type == NCX_MEDIA_TYPE {
                Self::parse_ncx(&toc_text).unwrap_or_default()
            } else {
                Self::parse_nav(&toc_text)
            };
            let mut entries = Vec::new();
            for (label, src) in raw_entries {
                let (file, section) = match src.split_once('#') {
                    Some((f, s)) => (f, Some(s.to_string())),
                    None => (src.as_str(), None),
                };
                let target = if file.is_empty() {
                    toc_path.clone()
                } else {
                    resolve_path(&toc_path, &unquote(file))
                };
                if let Some(idx) = self.contents.iter().position(|c| *c == target) {
                    entries.push(TocEntry { label, content_index: idx, section });
                }
            }
            if !entries.is_empty() {
                self.toc_entries = entries;
                break;
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
        self.read_text(content)
    }

    fn get_img_bytestr(&mut self, impath: &str) -> Result<(String, Vec<u8>)> {
        let impath = impath.trim_start_matches('/');
        Ok((impath.to_string(), self.read_entry(impath)?))
    }

    fn resolves_image_paths(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nav_document() {
        let nav = r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body>
            <nav epub:type="landmarks"><ol><li><a href="cover.xhtml">Cover</a></li></ol></nav>
            <nav epub:type="toc"><ol>
              <li><a href="ch1.xhtml">Chapter
                 <span>One</span></a>
                <ol><li><a href="ch1.xhtml#s2">Part&nbsp;2</a></li></ol></li>
            </ol></nav></body></html>"#;
        assert_eq!(
            Epub::parse_nav(nav),
            vec![
                ("Chapter One".to_string(), "ch1.xhtml".to_string()),
                ("Part 2".to_string(), "ch1.xhtml#s2".to_string()),
            ]
        );
    }
}
