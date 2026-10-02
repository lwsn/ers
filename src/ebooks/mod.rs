//! Ebook formats.

mod epub;
mod fictionbook;
mod url;

use anyhow::{Result, bail};

pub use epub::Epub;
pub use fictionbook::FictionBook;
pub use url::Url;

use crate::models::{BookMetadata, TocEntry};
use crate::util::is_url;

pub trait Ebook: Send {
    /// Absolute file path or URL; used as the key for saved state.
    fn path(&self) -> &str;

    /// Load the book structure. Must be called before anything else below.
    fn initialize(&mut self) -> Result<()>;

    /// Identifiers of the book contents (chapters) in reading order.
    fn contents(&self) -> &[String];

    fn toc_entries(&self) -> &[TocEntry];

    fn get_meta(&self) -> BookMetadata;

    /// HTML source of a content.
    fn get_raw_text(&mut self, content: &str) -> Result<String>;

    /// Image file name and bytes. `impath` is already resolved against the
    /// content it was found in when [`Ebook::resolves_image_paths`] is true.
    fn get_img_bytestr(&mut self, impath: &str) -> Result<(String, Vec<u8>)>;

    /// Whether image paths found in a content are relative to that content
    /// (and should be resolved against it before calling `get_img_bytestr`).
    fn resolves_image_paths(&self) -> bool {
        false
    }

    fn cleanup(&mut self) {}
}

pub fn get_ebook_obj(filepath: &str) -> Result<Box<dyn Ebook>> {
    if is_url(filepath) {
        return Ok(Box::new(Url::new(filepath)));
    }
    let ext = std::path::Path::new(filepath)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "epub" | "epub3" => Ok(Box::new(Epub::open(filepath)?)),
        "fb2" => Ok(Box::new(FictionBook::new(filepath)?)),
        "mobi" | "azw" | "azw3" => bail!("Format not supported yet: {ext} (Supported: epub, fb2)"),
        _ => bail!("Format not supported. (Supported: epub, fb2)"),
    }
}

/// Helpers for namespace-agnostic lookups in roxmltree documents.
pub(crate) mod xml {
    use roxmltree::{Document, Node, ParsingOptions};

    pub fn parse(text: &str) -> Result<Document<'_>, roxmltree::Error> {
        Document::parse_with_options(text, ParsingOptions { allow_dtd: true, ..Default::default() })
    }

    pub fn is(node: &Node, name: &str) -> bool {
        node.is_element() && node.tag_name().name() == name
    }

    pub fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
        node.children().find(|n| is(n, name))
    }

    pub fn children<'a, 'i>(node: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> {
        node.children().filter(move |n| is(n, name))
    }

    pub fn descendant<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
        node.descendants().find(|n| is(n, name))
    }

    pub fn all_text(node: Node) -> String {
        node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect()
    }
}
