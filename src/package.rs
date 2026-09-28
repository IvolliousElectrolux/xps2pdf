//! XPS / OXPS 是 OPC 压缩包. 部件名大小写在 macOS 上按字节比较, 这里做不区分大小写的回退.

use std::io::{Read, Seek};

use zip::ZipArchive;

use crate::error::Error;

pub struct Package<R: Read + Seek> {
    zip: ZipArchive<R>,
    names: Vec<String>,
}

impl<R: Read + Seek> Package<R> {
    pub fn open(reader: R) -> Result<Self, Error> {
        let mut zip = ZipArchive::new(reader).map_err(|e| Error::msg(format!("无法打开 XPS 压缩包: {e}")))?;
        let mut names = Vec::with_capacity(zip.len());
        for i in 0..zip.len() {
            let entry = zip.by_index(i).map_err(|e| Error::msg(format!("读取压缩包目录失败: {e}")))?;
            if entry.is_dir() {
                names.push(String::new());
                continue;
            }
            names.push(normalize_part(entry.name()));
        }
        Ok(Self { zip, names })
    }

    pub fn read_part(&mut self, name: &str) -> Result<Vec<u8>, Error> {
        let want = normalize_part(name);
        if let Some(bytes) = self.read_named(&want)? {
            return Ok(bytes);
        }
        let prefix = format!("{want}/[");
        let mut pieces: Vec<(u32, usize)> = Vec::new();
        for (idx, name) in self.names.iter().enumerate() {
            if name.starts_with(&prefix) && (name.ends_with(".piece") || name.ends_with(".last.piece")) {
                if let Some(ord) = piece_ord(name) {
                    pieces.push((ord, idx));
                }
            }
        }
        if pieces.is_empty() {
            return Err(Error::msg(format!("压缩包里没有部件 {want}")));
        }
        pieces.sort_by_key(|(ord, _)| *ord);
        let mut out = Vec::new();
        for (_, idx) in pieces {
            out.extend(self.read_index(idx)?);
        }
        Ok(out)
    }

    fn read_named(&mut self, want: &str) -> Result<Option<Vec<u8>>, Error> {
        if let Some(idx) = self.names.iter().position(|n| n == want) {
            return Ok(Some(self.read_index(idx)?));
        }
        let lower = want.to_ascii_lowercase();
        if let Some(idx) = self.names.iter().position(|n| n.to_ascii_lowercase() == lower) {
            return Ok(Some(self.read_index(idx)?));
        }
        Ok(None)
    }

    fn read_index(&mut self, idx: usize) -> Result<Vec<u8>, Error> {
        let mut entry = self.zip.by_index(idx).map_err(|e| Error::msg(format!("读取部件失败: {e}")))?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|e| Error::msg(format!("读取部件失败: {e}")))?;
        if buf.len() > 512 * 1024 * 1024 {
            return Err(Error::msg("单个部件超过 512 MB, 已中止"));
        }
        Ok(buf)
    }

    pub fn part_names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().filter(|n| !n.is_empty()).map(|s| s.as_str())
    }
}

pub fn normalize_part(raw: &str) -> String {
    let decoded = percent_decode(raw.trim());
    let slash = decoded.replace('\\', "/");
    let mut stack = Vec::new();
    for seg in slash.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            stack.pop();
            continue;
        }
        stack.push(seg);
    }
    stack.join("/")
}

pub fn resolve_uri(base_part: &str, uri: &str) -> String {
    let uri = uri.trim();
    let uri = uri.split(['?', '#']).next().unwrap_or(uri);
    if uri.starts_with('/') {
        return normalize_part(uri);
    }
    let base = normalize_part(base_part);
    let base_dir = match base.rfind('/') {
        Some(i) => &base[..i],
        None => "",
    };
    if base_dir.is_empty() {
        normalize_part(uri)
    } else {
        normalize_part(&format!("{base_dir}/{uri}"))
    }
}

fn piece_ord(name: &str) -> Option<u32> {
    let i = name.rfind('[')?;
    let rest = &name[i + 1..];
    let end = rest.find(']')?;
    rest[..end].parse().ok()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn xml_to_utf8(bytes: &[u8]) -> Result<String, Error> {
    let text = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        utf16_to_string(&bytes[2..], true)?
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        utf16_to_string(&bytes[2..], false)?
    } else if let Ok(s) = std::str::from_utf8(bytes) {
        s.to_string()
    } else {
        let zeros = bytes.iter().take(64).filter(|b| **b == 0).count();
        if zeros > 8 {
            utf16_to_string(bytes, true)?
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    Ok(declare_missing_prefixes(text))
}

/// PDFTron 等写出的页面会使用 `trn:smooth`, 却不在该部件里声明 `xmlns:trn`.
/// 这在 XML 里是非法的, 这里给未声明的前缀补一个占位声明, 未知属性随后会被忽略.
fn declare_missing_prefixes(xml: String) -> String {
    let missing = missing_prefixes(&xml);
    if missing.is_empty() {
        return xml;
    }
    let Some(at) = root_name_end(&xml) else {
        return xml;
    };
    let mut decl = String::new();
    for prefix in &missing {
        decl.push_str(" xmlns:");
        decl.push_str(prefix);
        decl.push_str("=\"urn:xps2pdf:");
        decl.push_str(prefix);
        decl.push('"');
    }
    let mut out = String::with_capacity(xml.len() + decl.len());
    out.push_str(&xml[..at]);
    out.push_str(&decl);
    out.push_str(&xml[at..]);
    out
}

fn missing_prefixes(xml: &str) -> Vec<String> {
    let bytes = xml.as_bytes();
    let mut i = 0;
    let mut stack: Vec<std::collections::HashSet<String>> = vec![std::collections::HashSet::from(["xml".to_string()])];
    let mut missing = std::collections::BTreeSet::new();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if let Some(next) = skip_special(xml, i) {
            i = next;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'/') {
            if stack.len() > 1 {
                stack.pop();
            }
            i = skip_to_gt(bytes, i);
            continue;
        }
        i += 1;
        let name = read_name(xml, &mut i);
        let mut used = Vec::new();
        let mut decls = Vec::new();
        push_prefix(&name, &mut used);
        let mut self_closing = false;
        loop {
            i = skip_ws(bytes, i);
            if i >= bytes.len() {
                break;
            }
            if bytes[i] == b'>' {
                i += 1;
                break;
            }
            if bytes[i] == b'/' {
                self_closing = true;
                i += 1;
                continue;
            }
            if !is_name_start(bytes[i]) {
                i += 1;
                continue;
            }
            let attr = read_name(xml, &mut i);
            if let Some(prefix) = attr.strip_prefix("xmlns:") {
                if is_ncname(prefix) {
                    decls.push(prefix.to_string());
                }
            } else {
                push_prefix(&attr, &mut used);
            }
            i = skip_ws(bytes, i);
            if bytes.get(i) == Some(&b'=') {
                i += 1;
                i = skip_ws(bytes, i);
                i = skip_value(bytes, i);
            }
        }
        let mut here = stack.last().cloned().unwrap_or_default();
        for decl in &decls {
            here.insert(decl.clone());
        }
        for prefix in &used {
            if !here.contains(prefix) {
                missing.insert(prefix.clone());
            }
        }
        if !self_closing {
            stack.push(here);
        }
    }
    missing.into_iter().collect()
}

fn root_name_end(xml: &str) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if let Some(next) = skip_special(xml, i) {
            i = next;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'/') {
            i = skip_to_gt(bytes, i);
            continue;
        }
        i += 1;
        let start = i;
        i = skip_name_bytes(bytes, i);
        if i > start {
            return Some(i);
        }
    }
    None
}

fn skip_special(xml: &str, i: usize) -> Option<usize> {
    let rest = &xml[i..];
    if rest.starts_with("<!--") {
        return rest[4..].find("-->").map(|end| i + 4 + end + 3).or(Some(xml.len()));
    }
    if rest.starts_with("<?") {
        return rest[2..].find("?>").map(|end| i + 2 + end + 2).or(Some(xml.len()));
    }
    if rest.starts_with("<![CDATA[") {
        return rest[9..].find("]]>").map(|end| i + 9 + end + 3).or(Some(xml.len()));
    }
    if rest.starts_with("<!") {
        return rest.find('>').map(|end| i + end + 1).or(Some(xml.len()));
    }
    None
}

fn read_name<'a>(xml: &'a str, i: &mut usize) -> &'a str {
    let start = *i;
    *i = skip_name_bytes(xml.as_bytes(), *i);
    &xml[start..*i]
}

fn skip_name_bytes(bytes: &[u8], mut i: usize) -> usize {
    if i < bytes.len() && is_name_start(bytes[i]) {
        i += 1;
        while i < bytes.len() && is_name_char(bytes[i]) {
            i += 1;
        }
    }
    i
}

fn push_prefix(name: &str, used: &mut Vec<String>) {
    let Some((prefix, rest)) = name.split_once(':') else {
        return;
    };
    if prefix != "xmlns" && is_ncname(prefix) && is_ncname(rest) {
        used.push(prefix.to_string());
    }
}

fn is_ncname(name: &str) -> bool {
    let mut chars = name.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_name_start(first) || first == b':' {
        return false;
    }
    chars.all(is_name_char)
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b':'
}

fn is_name_char(c: u8) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == b'-' || c == b'.'
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_value(bytes: &[u8], mut i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }
    let quote = bytes[i];
    if quote == b'"' || quote == b'\'' {
        i += 1;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        if i < bytes.len() {
            i += 1;
        }
        return i;
    }
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
        i += 1;
    }
    i
}

fn skip_to_gt(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'>' {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    i
}

fn utf16_to_string(bytes: &[u8], le: bool) -> Result<String, Error> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
        .collect();
    String::from_utf16(&units).map_err(|_| Error::msg("XML 不是有效的 UTF-16"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_font() {
        let p = resolve_uri("Documents/1/Pages/1.fpage", "../Resources/Fonts/A.odttf");
        assert_eq!(p, "Documents/1/Resources/Fonts/A.odttf");
    }

    #[test]
    fn declares_missing_prefix_only() {
        let raw = br##"<?xml version="1.0"?>
<FixedPage xmlns="http://schemas.microsoft.com/xps/2005/06" xml:lang="EN">
  <Path Fill="#FF000000" trn:smooth="false" Data="M 0,0 L 1,1"/>
</FixedPage>"##;
        let xml = xml_to_utf8(raw).unwrap();
        assert!(xml.contains("xmlns:trn=\"urn:xps2pdf:trn\""));
        assert!(xml.contains("xml:lang=\"EN\""));
        let again = xml_to_utf8(xml.as_bytes()).unwrap();
        assert_eq!(again.matches("xmlns:trn=").count(), 1);
    }
}
