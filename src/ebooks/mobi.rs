//! MOBI / AZW / AZW3 (Mobipocket and Kindle Format 8) books.
//!
//! The original epy unpacks these with KindleUnpack into a temporary
//! directory. This reads the Palm database directly instead: the text records
//! are decompressed and turned into HTML chapters in memory, and images are
//! served straight from their records.
//!
//! - MOBI 7 books are one HTML stream; it is split into chapters at
//!   `<mbp:pagebreak/>` and table of contents targets (`filepos` byte
//!   offsets) are marked with inserted anchors.
//! - KF8 books (version 8, also the second half of "joint" .mobi/.azw files)
//!   store XHTML files as skeletons with fragments cut out of them; the
//!   files are rebuilt from the skeleton and fragment indexes.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use regex::bytes::Regex;

use super::Ebook;
use crate::models::{BookMetadata, TocEntry};

const NONE: u32 = 0xffff_ffff;

pub struct Mobi {
    path: String,
    data: Vec<u8>,
    /// Start offset of every record, followed by the file length.
    offsets: Vec<usize>,
    parts: Vec<String>,
    contents: Vec<String>,
    toc_entries: Vec<TocEntry>,
    metadata: BookMetadata,
    /// Absolute record number of the first image/resource record.
    first_resource: usize,
}

impl Mobi {
    pub fn new(path: &str) -> Result<Self> {
        Ok(Self {
            path: std::path::absolute(path)?.to_string_lossy().into_owned(),
            data: Vec::new(),
            offsets: Vec::new(),
            parts: Vec::new(),
            contents: Vec::new(),
            toc_entries: Vec::new(),
            metadata: BookMetadata::default(),
            first_resource: 0,
        })
    }

    fn record(&self, n: usize) -> Result<&[u8]> {
        match (self.offsets.get(n), self.offsets.get(n + 1)) {
            (Some(&a), Some(&b)) if a <= b && b <= self.data.len() => Ok(&self.data[a..b]),
            _ => Err(anyhow!("record {n} out of range")),
        }
    }

    fn read_header(&self, rec: usize) -> Result<Header> {
        Header::parse(self.record(rec)?, rec)
    }

    /// Decompressed text of the book whose header is `h`.
    fn raw_text(&self, h: &Header) -> Result<Vec<u8>> {
        let mut huff = None;
        match h.compression {
            1 | 2 => {}
            17480 => {
                let mut reader = HuffCdic::new(self.record(h.abs(h.huff_offset)?)?)?;
                for i in 1..h.huff_count as usize {
                    reader.load_cdic(self.record(h.abs(h.huff_offset)? + i)?)?;
                }
                huff = Some(reader);
            }
            c => bail!("unknown MOBI compression type {c}"),
        }
        let mut out = Vec::with_capacity(h.text_length as usize);
        for i in 1..=h.text_records as usize {
            let rec = self.record(h.record + i)?;
            let rec = &rec[..rec.len() - trailing_size(rec, h.extra_flags).min(rec.len())];
            match h.compression {
                1 => out.extend_from_slice(rec),
                2 => palmdoc_decompress(rec, &mut out),
                _ => out.extend(huff.as_mut().unwrap().unpack(rec)?),
            }
        }
        out.truncate(h.text_length as usize);
        Ok(out)
    }

    fn read_index(&self, h: &Header, idx: u32) -> Result<Index> {
        if idx == NONE {
            return Ok(Index::default());
        }
        let start = h.abs(idx)?;
        Index::read(|n| self.record(start + n))
    }

    fn load_mobi7(&mut self, h: &Header) -> Result<()> {
        let text = self.raw_text(h)?;
        let ncx = self.read_index(h, h.ncx_index).unwrap_or_default();
        let toc: Vec<(String, usize)> = ncx
            .entries
            .iter()
            .filter_map(|e| {
                let pos = *e.tags.get(&1)?.first()? as usize;
                Some((ncx.label(e, h.codec), pos))
            })
            .collect();

        // chapter byte ranges between page breaks
        let mut ranges = Vec::new();
        let mut start = 0;
        for m in Regex::new(r"(?i)<mbp:pagebreak[^>]*>").unwrap().find_iter(&text) {
            ranges.push((start, m.start()));
            start = m.end();
        }
        ranges.push((start, text.len()));

        let mut anchors: Vec<usize> = toc.iter().map(|(_, pos)| *pos).filter(|p| *p < text.len()).collect();
        anchors.sort_unstable();
        anchors.dedup();

        let recindex = Regex::new(r#"(?i)\b(?:hi|lo)?recindex\s*=\s*["']?0*(\d+)["']?"#).unwrap();
        let mut chapters = Vec::new();
        for &(a, b) in &ranges {
            let points: Vec<(usize, String)> = anchors
                .iter()
                .filter(|p| (a..b).contains(*p))
                .map(|&p| (p - a, format!("filepos{p}")))
                .collect();
            let bytes = insert_anchors(&text[a..b], &points);
            let bytes = recindex.replace_all(&bytes, &b"src=\"${1}\""[..]);
            chapters.push(decode(&bytes, h.codec));
        }
        let entries = toc.into_iter().map(|(label, pos)| {
            let chapter = ranges.iter().position(|&(_, end)| pos < end).unwrap_or(ranges.len() - 1);
            (label, chapter, Some(format!("filepos{pos}")))
        });
        self.set_parts(chapters, entries);
        Ok(())
    }

    fn load_kf8(&mut self, h: &Header) -> Result<()> {
        let rawml = self.raw_text(h)?;
        // flow 0 is the XHTML text; the others are CSS, SVG images, etc.
        let mut flows = vec![(0, rawml.len())];
        if h.fdst != NONE {
            let fdst = self.record(h.abs(h.fdst)?)?;
            if fdst.get(..4) == Some(b"FDST") {
                flows = (0..u32_at(fdst, 8)? as usize)
                    .map(|i| Ok((u32_at(fdst, 12 + 8 * i)? as usize, u32_at(fdst, 16 + 8 * i)? as usize)))
                    .collect::<Result<_>>()?;
            }
        }
        let flow = |n: usize| flows.get(n).and_then(|&(a, b)| rawml.get(a..b.min(rawml.len())));
        let text = flow(0).ok_or_else(|| anyhow!("KF8 text flow is missing"))?;

        let skel = self.read_index(h, h.skel_index)?;
        let frag = self.read_index(h, h.frag_index)?;
        if skel.entries.is_empty() {
            bail!("KF8 skeleton index is missing");
        }
        let tag = |e: &IndexEntry, t: u8, i: usize| -> Result<usize> {
            e.tags
                .get(&t)
                .and_then(|v| v.get(i))
                .map(|&v| v as usize)
                .ok_or_else(|| anyhow!("KF8 index entry is missing tag {t}"))
        };
        // insert position of every fragment, by fragment number
        let mut frag_insert = Vec::new();
        let mut parts = Vec::new();
        // (skeleton start, assembled length) of every part
        let mut spans = Vec::new();
        let mut frags = frag.entries.iter();
        for s in &skel.entries {
            let (count, skelpos, skellen) = (tag(s, 1, 0)?, tag(s, 6, 0)?, tag(s, 6, 1)?);
            let mut base = skelpos + skellen;
            let mut part = slice(text, skelpos, base)?.to_vec();
            for _ in 0..count {
                let f = frags.next().ok_or_else(|| anyhow!("KF8 fragment index too short"))?;
                let insert: usize = std::str::from_utf8(&f.text)?.trim().parse()?;
                let len = tag(f, 6, 1)?;
                frag_insert.push(insert);
                let at = insert.checked_sub(skelpos).filter(|&p| p <= part.len());
                let at = at.ok_or_else(|| anyhow!("bad KF8 fragment insert position"))?;
                let piece = slice(text, base, base + len)?;
                part.splice(at..at, piece.iter().copied());
                base += len;
            }
            spans.push((skelpos, part.len()));
            parts.push(part);
        }

        let ncx = self.read_index(h, h.ncx_index).unwrap_or_default();
        let mut toc = Vec::new();
        let mut anchors: Vec<Vec<(usize, String)>> = vec![Vec::new(); parts.len()];
        for e in &ncx.entries {
            let label = ncx.label(e, h.codec);
            let target = e.tags.get(&6).filter(|v| v.len() >= 2);
            let pos = match target {
                Some(v) => frag_insert.get(v[0] as usize).map(|p| p + v[1] as usize),
                None => e.tags.get(&1).and_then(|v| v.first()).map(|&p| p as usize),
            };
            let Some(pos) = pos else { continue };
            let Some(pn) = spans.iter().position(|&(start, len)| (start..start + len).contains(&pos)) else {
                continue;
            };
            let section = target.map(|_| {
                let id = format!("filepos{pos}");
                anchors[pn].push((pos - spans[pn].0, id.clone()));
                id
            });
            toc.push((label, pn, section));
        }

        // SVG images in their own flow, typically a cover wrapping an embedded
        // image: point at that image instead
        let svg_ref = Regex::new(r"kindle:flow:([0-9A-Va-v]+)\?mime=image/svg\+xml").unwrap();
        let embed = Regex::new(r"kindle:embed:[0-9A-Va-v]+").unwrap();
        let svg_image = |caps: &regex::bytes::Captures| -> Vec<u8> {
            let n = std::str::from_utf8(&caps[1]).ok().and_then(|n| usize::from_str_radix(n, 32).ok());
            let image = n.and_then(flow).and_then(|svg| embed.find(svg));
            image.map_or_else(|| caps[0].to_vec(), |m| m.as_bytes().to_vec())
        };
        let chapters = parts
            .iter()
            .zip(anchors.iter_mut())
            .map(|(part, points)| {
                points.sort();
                points.dedup();
                let html = insert_anchors(part, points);
                decode(&svg_ref.replace_all(&html, &svg_image), h.codec)
            })
            .collect();
        self.set_parts(chapters, toc.into_iter());
        Ok(())
    }

    /// Store the chapters, dropping the ones without any text or images and
    /// pointing TOC entries of dropped chapters at the next remaining one.
    fn set_parts(&mut self, chapters: Vec<String>, toc: impl Iterator<Item = (String, usize, Option<String>)>) {
        let keep: Vec<bool> = chapters.iter().map(|c| has_content(c)).collect();
        let mut new_index = Vec::with_capacity(chapters.len());
        for (i, chapter) in chapters.into_iter().enumerate() {
            new_index.push(self.parts.len());
            if keep[i] {
                self.parts.push(chapter);
            }
        }
        if self.parts.is_empty() {
            self.parts.push(String::new());
        }
        self.contents = (0..self.parts.len()).map(|i| i.to_string()).collect();
        let last = self.parts.len() - 1;
        self.toc_entries = toc
            .map(|(label, chapter, section)| {
                let kept = keep.get(chapter).copied().unwrap_or(false);
                TocEntry {
                    label,
                    content_index: new_index.get(chapter).copied().unwrap_or(last).min(last),
                    section: section.filter(|_| kept),
                }
            })
            .collect();
    }
}

impl Ebook for Mobi {
    fn path(&self) -> &str {
        &self.path
    }

    fn initialize(&mut self) -> Result<()> {
        self.data = std::fs::read(&self.path).with_context(|| format!("cannot open {}", self.path))?;
        let data = &self.data;
        if data.starts_with(b"TPZ") {
            bail!("Topaz books are not supported");
        }
        if data.len() < 78 || &data[60..68] != b"BOOKMOBI" {
            bail!("not a MOBI file");
        }
        let count = u16::from_be_bytes([data[76], data[77]]) as usize;
        self.offsets = (0..count).map(|i| u32_at(data, 78 + 8 * i).map(|o| o as usize)).collect::<Result<_>>()?;
        self.offsets.push(data.len());

        let h = self.read_header(0)?;
        if h.encryption != 0 {
            bail!("DRM-protected books are not supported");
        }
        self.metadata = h.metadata();
        self.metadata.format = Some(if h.version == 8 { "KF8" } else { "MOBI" }.to_string());

        // KF8 is preferred; joint files fall back to their MOBI 7 half
        let kf8 = match (h.version, h.kf8_boundary) {
            (8, _) => Some(h.clone()),
            (_, Some(b)) => self.read_header(b as usize + 1).ok(),
            _ => None,
        };
        if let Some(k) = kf8 {
            match self.load_kf8(&k) {
                Ok(()) => {
                    self.metadata.format = Some("KF8".to_string());
                    self.first_resource = k.abs(k.first_resource).unwrap_or(0);
                    return Ok(());
                }
                Err(e) if h.version == 8 => return Err(e),
                Err(_) => {}
            }
        }
        self.first_resource = h.abs(h.first_resource).unwrap_or(0);
        self.load_mobi7(&h)
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
        self.parts.get(idx).cloned().ok_or_else(|| anyhow!("no content #{idx}"))
    }

    /// `impath` is a 1-based resource number: decimal for MOBI 7 `recindex`
    /// images, `kindle:embed:XXXX` (base 32) for KF8.
    fn get_img_bytestr(&mut self, impath: &str) -> Result<(String, Vec<u8>)> {
        let n = match impath.strip_prefix("kindle:embed:") {
            Some(rest) => {
                let digits = rest.split(['?', '#']).next().unwrap_or_default();
                u32::from_str_radix(digits, 32).ok()
            }
            None => impath.trim().parse().ok(),
        };
        let n = n.filter(|&n| n > 0).ok_or_else(|| anyhow!("bad image reference '{impath}'"))?;
        let bytes = self.record(self.first_resource + n as usize - 1)?.to_vec();
        let ext = match bytes.get(..4) {
            Some([0xff, 0xd8, ..]) => "jpg",
            Some([0x89, b'P', b'N', b'G']) => "png",
            Some([b'G', b'I', b'F', _]) => "gif",
            Some([b'B', b'M', ..]) => "bmp",
            Some(b"<?xm") | Some(b"<svg") => "svg",
            _ => bail!("resource {n} is not an image"),
        };
        Ok((format!("image{n:05}.{ext}"), bytes))
    }
}

// ---------------------------------------------------------------------------
// headers

#[derive(Clone)]
struct Header {
    /// Record number of this header.
    record: usize,
    compression: u16,
    text_length: u32,
    text_records: u16,
    encryption: u16,
    version: u32,
    codec: u32,
    title: String,
    first_resource: u32,
    huff_offset: u32,
    huff_count: u32,
    fdst: u32,
    extra_flags: u16,
    ncx_index: u32,
    frag_index: u32,
    skel_index: u32,
    kf8_boundary: Option<u32>,
    exth: Vec<(u32, Vec<u8>)>,
}

impl Header {
    fn parse(rec: &[u8], record: usize) -> Result<Self> {
        if rec.get(16..20) != Some(b"MOBI") {
            bail!("MOBI header not found");
        }
        let u32_or = |off: usize| u32_at(rec, off).unwrap_or(NONE);
        let length = u32_at(rec, 20)? as usize;
        let version = u32_at(rec, 0x24)?;
        let codec = u32_at(rec, 0x1c)?;
        let (title_off, title_len) = (u32_or(0x54) as usize, u32_or(0x58) as usize);
        let title = rec.get(title_off..title_off.saturating_add(title_len)).map(|t| decode(t, codec));
        let mut h = Header {
            record,
            compression: u16_at(rec, 0)?,
            text_length: u32_at(rec, 4)?,
            text_records: u16_at(rec, 8)?,
            encryption: u16_at(rec, 0xc)?,
            version,
            codec,
            title: title.unwrap_or_default(),
            first_resource: u32_or(0x6c),
            huff_offset: u32_or(0x70),
            huff_count: u32_or(0x74),
            fdst: if version >= 8 { u32_or(0xc0) } else { NONE },
            extra_flags: if length >= 0xe4 && version >= 5 { u16_at(rec, 0xf2).unwrap_or(0) } else { 0 },
            ncx_index: u32_or(0xf4),
            frag_index: if version >= 8 { u32_or(0xf8) } else { NONE },
            skel_index: if version >= 8 { u32_or(0xfc) } else { NONE },
            kf8_boundary: None,
            exth: Vec::new(),
        };
        if u32_or(0x80) & 0x40 != 0 {
            let start = 16 + length;
            if rec.get(start..start + 4) == Some(b"EXTH") {
                let count = u32_at(rec, start + 8)?;
                let mut pos = start + 12;
                for _ in 0..count {
                    let (id, len) = (u32_at(rec, pos)?, u32_at(rec, pos + 4)? as usize);
                    let value = rec.get(pos + 8..pos + len.max(8)).unwrap_or_default();
                    h.exth.push((id, value.to_vec()));
                    pos += len.max(8);
                }
            }
        }
        h.kf8_boundary = h
            .exth
            .iter()
            .find(|(id, v)| *id == 121 && v.len() >= 4)
            .map(|(_, v)| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
            .filter(|&b| b != NONE);
        Ok(h)
    }

    /// Absolute record number of a record referenced by this header.
    fn abs(&self, n: u32) -> Result<usize> {
        if n == NONE {
            bail!("missing record reference");
        }
        Ok(self.record + n as usize)
    }

    fn metadata(&self) -> BookMetadata {
        let get = |id: u32| -> Option<String> {
            let values: Vec<String> = self
                .exth
                .iter()
                .filter(|(i, _)| *i == id)
                .map(|(_, v)| decode(v, self.codec).trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            (!values.is_empty()).then(|| values.join(", "))
        };
        let title = get(503).or_else(|| Some(self.title.trim().to_string()).filter(|t| !t.is_empty()));
        BookMetadata {
            title,
            creator: get(100),
            description: get(103),
            publisher: get(101),
            date: get(106),
            language: get(524),
            format: None,
            identifier: get(104).or_else(|| get(113)),
            source: get(112),
        }
    }
}

// ---------------------------------------------------------------------------
// indexes

#[derive(Default)]
struct IndexEntry {
    text: Vec<u8>,
    tags: HashMap<u8, Vec<u32>>,
}

#[derive(Default)]
struct Index {
    entries: Vec<IndexEntry>,
    /// CNCX strings by offset.
    cncx: HashMap<u32, Vec<u8>>,
}

impl Index {
    /// Read an INDX index; `record(n)` returns the n-th record from the
    /// index's main record.
    fn read<'a>(record: impl Fn(usize) -> Result<&'a [u8]>) -> Result<Self> {
        let main = record(0)?;
        if main.get(..4) != Some(b"INDX") {
            bail!("INDX record not found");
        }
        let header_len = u32_at(main, 4)? as usize;
        let count = u32_at(main, 24)? as usize;
        let ncncx = u32_at(main, 52)? as usize;

        let mut index = Index::default();
        for j in 0..ncncx {
            let data = record(count + 1 + j)?;
            let mut pos = 0;
            while pos < data.len() && data[pos] != 0 {
                let start = pos;
                let (consumed, len) = vwi(data, pos)?;
                pos += consumed;
                let text = data.get(pos..pos + len as usize).unwrap_or(&data[pos.min(data.len())..]);
                index.cncx.insert(start as u32 + 0x10000 * j as u32, text.to_vec());
                pos += len as usize;
            }
        }

        // TAGX: (tag, values per entry, mask, end flag)
        let mut tagx = Vec::new();
        let mut control_bytes = 0;
        if main.get(header_len..header_len + 4) == Some(b"TAGX") {
            let first_entry = u32_at(main, header_len + 4)? as usize;
            control_bytes = u32_at(main, header_len + 8)? as usize;
            for i in (12..first_entry).step_by(4) {
                let t = main.get(header_len + i..header_len + i + 4).ok_or_else(|| anyhow!("bad TAGX"))?;
                tagx.push((t[0], t[1] as usize, t[2], t[3]));
            }
        }

        for i in 1..=count {
            let data = record(i)?;
            if data.get(..4) != Some(b"INDX") {
                bail!("INDX record not found");
            }
            let idxt = u32_at(data, 20)? as usize;
            let entries = u32_at(data, 24)? as usize;
            let mut positions = (0..entries)
                .map(|j| u16_at(data, idxt + 4 + 2 * j).map(|p| p as usize))
                .collect::<Result<Vec<_>>>()?;
            positions.push(idxt);
            for w in positions.windows(2) {
                let (start, end) = (w[0], w[1]);
                let len = *data.get(start).ok_or_else(|| anyhow!("bad IDXT"))? as usize;
                let text = slice(data, start + 1, start + 1 + len)?.to_vec();
                let tags = read_tags(data, start + 1 + len, end, control_bytes, &tagx)?;
                index.entries.push(IndexEntry { text, tags });
            }
        }
        Ok(index)
    }

    fn label(&self, entry: &IndexEntry, codec: u32) -> String {
        let text = entry.tags.get(&3).and_then(|v| v.first()).and_then(|o| self.cncx.get(o));
        decode(text.unwrap_or(&entry.text), codec).split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

fn read_tags(
    data: &[u8],
    start: usize,
    end: usize,
    control_bytes: usize,
    tagx: &[(u8, usize, u8, u8)],
) -> Result<HashMap<u8, Vec<u32>>> {
    // (tag, value count, byte count, values per entry)
    let mut found = Vec::new();
    let mut cb_index = 0;
    let mut pos = start + control_bytes;
    for &(tag, per_entry, mask, end_flag) in tagx {
        if end_flag == 1 {
            cb_index += 1;
            continue;
        }
        let value = data.get(start + cb_index).ok_or_else(|| anyhow!("bad index entry"))? & mask;
        if value == 0 {
            continue;
        }
        if value == mask && mask.count_ones() > 1 {
            let (consumed, bytes) = vwi(data, pos)?;
            pos += consumed;
            found.push((tag, None, bytes as usize, per_entry));
        } else {
            found.push((tag, Some((value >> mask.trailing_zeros()) as usize), 0, per_entry));
        }
    }
    let mut tags = HashMap::new();
    for (tag, count, bytes, per_entry) in found {
        let mut values = Vec::new();
        match count {
            Some(count) => {
                for _ in 0..count * per_entry {
                    let (consumed, v) = vwi(data, pos)?;
                    pos += consumed;
                    values.push(v);
                }
            }
            None => {
                let mut consumed_total = 0;
                while consumed_total < bytes {
                    let (consumed, v) = vwi(data, pos)?;
                    pos += consumed;
                    consumed_total += consumed;
                    values.push(v);
                }
            }
        }
        tags.insert(tag, values);
    }
    if pos > end {
        bail!("index entry overruns its record");
    }
    Ok(tags)
}

/// Forward variable width integer: 7 bits per byte, the last byte has the
/// high bit set. Returns (bytes consumed, value).
fn vwi(data: &[u8], pos: usize) -> Result<(usize, u32)> {
    let mut value = 0u32;
    for (i, &b) in data.get(pos..).unwrap_or_default().iter().enumerate().take(5) {
        value = (value << 7) | (b & 0x7f) as u32;
        if b & 0x80 != 0 {
            return Ok((i + 1, value));
        }
    }
    bail!("bad variable width integer")
}

// ---------------------------------------------------------------------------
// text records

/// Size of the trailing entries appended to a text record.
fn trailing_size(rec: &[u8], flags: u16) -> usize {
    let mut num = 0;
    let mut flags_left = flags >> 1;
    while flags_left != 0 {
        if flags_left & 1 != 0 {
            // backward variable width integer ending at the record end
            let end = rec.len().saturating_sub(num);
            let (mut value, mut shift) = (0usize, 0);
            for &b in rec[..end].iter().rev() {
                value |= ((b & 0x7f) as usize) << shift;
                shift += 7;
                if b & 0x80 != 0 || shift >= 28 {
                    break;
                }
            }
            num += value;
        }
        flags_left >>= 1;
    }
    if flags & 1 != 0
        && let Some(&b) = rec.len().checked_sub(num + 1).and_then(|i| rec.get(i))
    {
        num += (b & 0x3) as usize + 1;
    }
    num
}

fn palmdoc_decompress(input: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    let mut p = 0;
    while p < input.len() {
        let c = input[p];
        p += 1;
        match c {
            1..=8 => {
                let end = (p + c as usize).min(input.len());
                out.extend_from_slice(&input[p..end]);
                p = end;
            }
            0 | 9..=0x7f => out.push(c),
            0xc0..=0xff => out.extend_from_slice(&[b' ', c ^ 0x80]),
            _ => {
                let Some(&next) = input.get(p) else { break };
                p += 1;
                let pair = ((c as usize) << 8) | next as usize;
                let distance = (pair >> 3) & 0x7ff;
                let length = (pair & 7) + 3;
                if distance == 0 || distance > out.len() - start {
                    continue;
                }
                for _ in 0..length {
                    out.push(out[out.len() - distance]);
                }
            }
        }
    }
}

struct HuffCdic {
    /// (code length, terminal, max code) by the code's top byte
    dict1: Vec<(u32, bool, u64)>,
    mincode: Vec<u64>,
    maxcode: Vec<u64>,
    /// (phrase, already decompressed)
    dictionary: Vec<(Vec<u8>, bool)>,
}

impl HuffCdic {
    fn new(huff: &[u8]) -> Result<Self> {
        if huff.get(..8) != Some(b"HUFF\0\0\0\x18") {
            bail!("bad HUFF record");
        }
        let (off1, off2) = (u32_at(huff, 8)? as usize, u32_at(huff, 12)? as usize);
        let dict1 = (0..256)
            .map(|i| {
                let v = u32_at(huff, off1 + 4 * i)?;
                let len = v & 0x1f;
                let max = (((v >> 8) as u64 + 1) << (32 - len)) - 1;
                Ok((len, v & 0x80 != 0, max))
            })
            .collect::<Result<_>>()?;
        let mut mincode = vec![0];
        let mut maxcode = vec![0];
        for len in 1..=32u32 {
            let i = off2 + 8 * (len as usize - 1);
            mincode.push((u32_at(huff, i)? as u64) << (32 - len));
            maxcode.push(((u32_at(huff, i + 4)? as u64 + 1) << (32 - len)) - 1);
        }
        Ok(Self { dict1, mincode, maxcode, dictionary: Vec::new() })
    }

    fn load_cdic(&mut self, cdic: &[u8]) -> Result<()> {
        if cdic.get(..8) != Some(b"CDIC\0\0\0\x10") {
            bail!("bad CDIC record");
        }
        let (phrases, bits) = (u32_at(cdic, 8)? as usize, u32_at(cdic, 12)?);
        let n = (1usize << bits).min(phrases.saturating_sub(self.dictionary.len()));
        for i in 0..n {
            let off = u16_at(cdic, 16 + 2 * i)? as usize;
            let blen = u16_at(cdic, 16 + off)? as usize;
            let phrase = slice(cdic, 18 + off, 18 + off + (blen & 0x7fff))?;
            self.dictionary.push((phrase.to_vec(), blen & 0x8000 != 0));
        }
        Ok(())
    }

    fn unpack(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.unpack_depth(data, 0)
    }

    fn unpack_depth(&mut self, data: &[u8], depth: usize) -> Result<Vec<u8>> {
        if depth > 32 {
            bail!("HUFF/CDIC recursion too deep");
        }
        let mut bits_left = data.len() as i64 * 8;
        let mut padded = data.to_vec();
        padded.extend_from_slice(&[0; 8]);
        let word = |pos: usize| u64::from_be_bytes(padded[pos..pos + 8].try_into().unwrap());
        let mut pos = 0;
        let mut x = word(pos);
        let mut n: i32 = 32;
        let mut out = Vec::new();
        loop {
            if n <= 0 {
                pos += 4;
                x = word(pos);
                n += 32;
            }
            let code = (x >> n) & 0xffff_ffff;
            let (mut len, term, mut max) = self.dict1[(code >> 24) as usize];
            if !term {
                while (len as usize) < 32 && code < self.mincode[len as usize] {
                    len += 1;
                }
                max = self.maxcode[len as usize];
            }
            if len == 0 {
                bail!("bad HUFF code");
            }
            n -= len as i32;
            bits_left -= len as i64;
            if bits_left < 0 {
                break;
            }
            let r = ((max - code) >> (32 - len)) as usize;
            let (phrase, done) = self.dictionary.get(r).cloned().ok_or_else(|| anyhow!("bad HUFF code"))?;
            let phrase = if done {
                phrase
            } else {
                let p = self.unpack_depth(&phrase, depth + 1)?;
                self.dictionary[r] = (p.clone(), true);
                p
            };
            out.extend_from_slice(&phrase);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// helpers

fn u16_at(d: &[u8], off: usize) -> Result<u16> {
    let b = slice(d, off, off + 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn u32_at(d: &[u8], off: usize) -> Result<u32> {
    let b = slice(d, off, off + 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn slice(d: &[u8], a: usize, b: usize) -> Result<&[u8]> {
    d.get(a..b).ok_or_else(|| anyhow!("unexpected end of data"))
}

fn decode(bytes: &[u8], codec: u32) -> String {
    if codec == 1252 {
        encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Insert `<a id="..."></a>` at the given (sorted) byte positions, moved
/// past the end of a tag when a position falls inside one.
fn insert_anchors(html: &[u8], points: &[(usize, String)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(html.len() + points.len() * 32);
    let mut last = 0;
    for (pos, id) in points {
        let mut pos = (*pos).max(last).min(html.len());
        let head = &html[..pos];
        if head.iter().rposition(|&b| b == b'<') > head.iter().rposition(|&b| b == b'>') {
            pos = html[pos..].iter().position(|&b| b == b'>').map_or(html.len(), |i| pos + i + 1);
        }
        out.extend_from_slice(&html[last..pos]);
        out.extend_from_slice(format!("<a id=\"{id}\"></a>").as_bytes());
        last = pos;
    }
    out.extend_from_slice(&html[last..]);
    out
}

/// Whether an HTML fragment has any visible text or images.
fn has_content(html: &str) -> bool {
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag && !c.is_whitespace() => return true,
            _ => {}
        }
    }
    let lower = html.to_ascii_lowercase();
    lower.contains("<img") || lower.contains("<image")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palmdoc() {
        // literal "abc", then a back-reference copying 3 bytes from distance 3
        let mut out = Vec::new();
        let pair: u16 = 0x8000 | (3 << 3); // distance 3, length 3
        let mut input = b"abc".to_vec();
        input.extend_from_slice(&pair.to_be_bytes());
        input.push(b'x' ^ 0x80);
        palmdoc_decompress(&input, &mut out);
        assert_eq!(out, b"abcabc x");
    }

    #[test]
    fn trailing_entries() {
        // multibyte flag: low 2 bits of the last byte + 1
        assert_eq!(trailing_size(b"text\x01", 1), 2);
        // one trailing entry of size 3, encoded as a backward vwi
        assert_eq!(trailing_size(b"text\x00\x00\x83", 2), 3);
    }

    #[test]
    fn anchors_skip_tags() {
        let html = b"<p class=\"x\">Hello</p>";
        let out = insert_anchors(html, &[(3, "a".into()), (15, "b".into())]);
        assert_eq!(String::from_utf8(out).unwrap(), "<p class=\"x\"><a id=\"a\"></a>He<a id=\"b\"></a>llo</p>");
    }

    #[test]
    fn variable_width() {
        assert_eq!(vwi(&[0x81], 0).unwrap(), (1, 1));
        assert_eq!(vwi(&[0x01, 0x80], 0).unwrap(), (2, 128));
    }
}
