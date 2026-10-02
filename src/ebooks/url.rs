use anyhow::Result;

use super::Ebook;
use crate::models::{BookMetadata, TocEntry};
use crate::util::{is_url, resolve_path};

pub struct Url {
    path: String,
    contents: Vec<String>,
    html: String,
}

impl Url {
    pub fn new(url: &str) -> Self {
        Self { path: url.to_string(), contents: vec!["_".into()], html: String::new() }
    }

    fn get(url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        Ok(ureq::get(url)
            .header("User-Agent", concat!("ers/v", env!("CARGO_PKG_VERSION")))
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.8")
            .call()?)
    }
}

impl Ebook for Url {
    fn path(&self) -> &str {
        &self.path
    }

    fn initialize(&mut self) -> Result<()> {
        self.html = Self::get(&self.path)?.body_mut().read_to_string()?;
        Ok(())
    }

    fn contents(&self) -> &[String] {
        &self.contents
    }

    fn toc_entries(&self) -> &[TocEntry] {
        &[]
    }

    fn get_meta(&self) -> BookMetadata {
        BookMetadata::default()
    }

    fn get_raw_text(&mut self, _content: &str) -> Result<String> {
        Ok(self.html.clone())
    }

    fn get_img_bytestr(&mut self, src: &str) -> Result<(String, Vec<u8>)> {
        let image_url = if is_url(src) { src.to_string() } else { resolve_path(&self.path, src) };
        let bytes = Self::get(&image_url)?.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?;
        let name = image_url
            .split(['?', '#'])
            .next()
            .and_then(|p| p.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .unwrap_or("image")
            .to_string();
        Ok((name, bytes))
    }
}
