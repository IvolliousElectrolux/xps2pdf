//! XPS 把嵌入字体的前 32 字节用部件名里的 GUID 做 XOR. 见 ECMA-388 字体混淆.

use std::sync::Arc;

use ttf_parser::{Face, GlyphId, OutlineBuilder};

use crate::geom::{Matrix, Point};
use crate::pathgeom::{PathCmd, PathGeom};

/// 伪斜体: 规范要求向右斜约 20 度.
const ITALIC_SHEAR: f32 = 0.36397023426;

pub fn deobfuscate(part_name: &str, data: &[u8]) -> Vec<u8> {
    let lower = part_name.to_ascii_lowercase();
    if !(lower.ends_with(".odttf") || lower.ends_with(".odttc")) {
        return data.to_vec();
    }
    let Some(keys) = guid_keys(part_name) else {
        return data.to_vec();
    };
    let mut magic_only = None;
    for key in keys {
        let mut out = data.to_vec();
        apply_key(&mut out, &key);
        if !font_magic_ok(&out) {
            continue;
        }
        if font_ok(&out) {
            return out;
        }
        if magic_only.is_none() {
            magic_only = Some(out);
        }
    }
    magic_only.unwrap_or_else(|| data.to_vec())
}

fn apply_key(data: &mut [u8], key: &[u8; 16]) {
    let n = data.len().min(32);
    for i in 0..n {
        data[i] ^= key[i % 16];
    }
}

fn font_magic_ok(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let tag = &data[..4];
    tag == [0, 1, 0, 0] || tag == b"OTTO" || tag == b"true" || tag == b"ttcf" || tag == [0, 0, 1, 0]
}

/// 规范要求 Data1/Data2/Data3 小端. PDFTron 的 SilverDox 则把十六进制 GUID 整段倒序.
fn guid_keys(part_name: &str) -> Option<[[u8; 16]; 2]> {
    let stem = std::path::Path::new(part_name)
        .file_stem()
        .and_then(|s| s.to_str())?;
    let stem = stem.trim_matches(|c| c == '{' || c == '}');
    let hex: String = stem.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok();
    let spec = [
        b(3)?, b(2)?, b(1)?, b(0)?, b(5)?, b(4)?, b(7)?, b(6)?, b(8)?, b(9)?, b(10)?, b(11)?,
        b(12)?, b(13)?, b(14)?, b(15)?,
    ];
    let mut reversed = [0u8; 16];
    for i in 0..16 {
        reversed[i] = b(15 - i)?;
    }
    Some([spec, reversed])
}

#[derive(Clone, Debug)]
pub struct Placement {
    pub gid: u16,
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
}

#[derive(Clone, Debug)]
struct Mapping {
    code_units: usize,
    glyph_count: usize,
    glyph: Option<u16>,
    advance: Option<f32>,
    u: f32,
    v: f32,
}

pub fn layout_glyphs(
    font: &[u8],
    em: f32,
    origin_x: f32,
    origin_y: f32,
    unicode: &str,
    indices: Option<&str>,
    rtl: bool,
    sideways: bool,
) -> Vec<Placement> {
    let Ok(face) = Face::parse(font, 0) else {
        return Vec::new();
    };
    let upem = face.units_per_em().max(1) as f32;
    let scale = em / upem;
    let utf16: Vec<u16> = unicode.encode_utf16().collect();
    let mappings = indices.map(parse_indices).unwrap_or_default();
    let use_cmap_only = indices.is_none();

    let mut out = Vec::new();
    let mut pen_x = origin_x;
    let mut pen_y = origin_y;

    if use_cmap_only {
        for ch in unicode.chars() {
            let gid = face.glyph_index(ch).map(|g| g.0).unwrap_or(0);
            let adv = font_advance(&face, gid, scale, sideways);
            push_place(&mut out, &mut pen_x, &mut pen_y, gid, 0.0, 0.0, adv, rtl, sideways);
        }
        return out;
    }

    let mut ci = 0usize;
    let mut mi = 0usize;
    let guard = utf16.len() + mappings.len() + 2;
    let mut steps = 0usize;
    while (ci < utf16.len() || mi < mappings.len()) && steps < guard {
        steps += 1;
        let (code_units, glyph_count) = if mi < mappings.len() {
            (mappings[mi].code_units.max(1), mappings[mi].glyph_count.max(1))
        } else {
            (1, 1)
        };
        for g in 0..glyph_count {
            let mapping = if mi < mappings.len() {
                let m = mappings[mi].clone();
                mi += 1;
                m
            } else {
                Mapping { code_units: 1, glyph_count: 1, glyph: None, advance: None, u: 0.0, v: 0.0 }
            };
            let gid = if let Some(id) = mapping.glyph {
                id
            } else if g == 0 {
                cmap_at(&face, &utf16, ci).unwrap_or(0)
            } else {
                0
            };
            let adv = if let Some(a) = mapping.advance {
                a / 100.0 * em
            } else {
                font_advance(&face, gid, scale, sideways)
            };
            let u = mapping.u / 100.0 * em;
            let v = mapping.v / 100.0 * em;
            push_place(&mut out, &mut pen_x, &mut pen_y, gid, u, v, adv, rtl, sideways);
        }
        ci += code_units;
    }
    out
}

fn push_place(
    out: &mut Vec<Placement>,
    pen_x: &mut f32,
    pen_y: &mut f32,
    gid: u16,
    u: f32,
    v: f32,
    adv: f32,
    rtl: bool,
    sideways: bool,
) {
    if sideways {
        out.push(Placement { gid, x: *pen_x, y: *pen_y, u, v });
        *pen_y += adv;
    } else if rtl {
        *pen_x -= adv;
        out.push(Placement { gid, x: *pen_x, y: *pen_y, u, v });
    } else {
        out.push(Placement { gid, x: *pen_x, y: *pen_y, u, v });
        *pen_x += adv;
    }
}

fn font_advance(face: &Face<'_>, gid: u16, scale: f32, sideways: bool) -> f32 {
    let id = GlyphId(gid);
    if sideways {
        face.glyph_ver_advance(id)
            .or_else(|| face.glyph_hor_advance(id))
            .unwrap_or(face.units_per_em()) as f32
            * scale
    } else {
        face.glyph_hor_advance(id).unwrap_or(0) as f32 * scale
    }
}

fn cmap_at(face: &Face<'_>, utf16: &[u16], index: usize) -> Option<u16> {
    let ch = char_at(utf16, index)?;
    face.glyph_index(ch).map(|g| g.0)
}

fn char_at(utf16: &[u16], index: usize) -> Option<char> {
    let u = *utf16.get(index)?;
    if (0xD800..=0xDBFF).contains(&u) {
        if let Some(v) = utf16.get(index + 1) {
            if (0xDC00..=0xDFFF).contains(v) {
                let c = 0x10000 + (((u as u32 - 0xD800) << 10) | (*v as u32 - 0xDC00));
                return char::from_u32(c);
            }
        }
    }
    char::from_u32(u as u32)
}

fn parse_indices(input: &str) -> Vec<Mapping> {
    let s = input.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = s.split(';').collect();
    if s.ends_with(';') && parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    parts.into_iter().map(parse_one_index).collect()
}

fn parse_one_index(part: &str) -> Mapping {
    let mut rest = part.trim();
    let mut code_units = 1usize;
    let mut glyph_count = 1usize;
    if let Some(inner) = rest.strip_prefix('(') {
        if let Some(end) = inner.find(')') {
            let spec = &inner[..end];
            rest = inner[end + 1..].trim();
            if let Some((a, b)) = spec.split_once(':') {
                code_units = a.trim().parse().unwrap_or(1).max(1);
                glyph_count = b.trim().parse().unwrap_or(1).max(1);
            } else {
                code_units = spec.trim().parse().unwrap_or(1).max(1);
            }
        }
    }
    let fields: Vec<&str> = rest.split(',').collect();
    let field = |i: usize| fields.get(i).map(|s| s.trim()).filter(|s| !s.is_empty());
    Mapping {
        code_units,
        glyph_count,
        glyph: field(0).and_then(|s| s.parse().ok()),
        advance: field(1).and_then(|s| s.parse().ok()),
        u: field(2).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        v: field(3).and_then(|s| s.parse().ok()).unwrap_or(0.0),
    }
}

pub fn outline_placements(
    font: &Arc<Vec<u8>>,
    em: f32,
    placements: &[Placement],
    italic: bool,
    sideways: bool,
    world: Matrix,
) -> PathGeom {
    let mut geom = PathGeom::empty();
    geom.fill = crate::geom::FillRule::NonZero;
    let Ok(face) = Face::parse(font, 0) else {
        return geom;
    };
    let upem = face.units_per_em().max(1) as f32;
    let scale = em / upem;
    let shear = if italic { ITALIC_SHEAR } else { 0.0 };
    for place in placements {
        let (ox, oy) = if sideways {
            (-place.v, place.u)
        } else {
            (place.u, -place.v)
        };
        let mut sink = OutlineSink {
            cmds: Vec::new(),
            scale,
            shear,
            sideways,
            pen_x: place.x + ox,
            pen_y: place.y + oy,
            world,
            current: Point::new(0.0, 0.0),
        };
        let _ = face.outline_glyph(GlyphId(place.gid), &mut sink);
        geom.cmds.append(&mut sink.cmds);
    }
    geom
}

struct OutlineSink {
    cmds: Vec<PathCmd>,
    scale: f32,
    shear: f32,
    sideways: bool,
    pen_x: f32,
    pen_y: f32,
    world: Matrix,
    current: Point,
}

impl OutlineSink {
    fn map(&self, mut x: f32, mut y: f32) -> Point {
        if self.shear != 0.0 {
            x += y * self.shear;
        }
        if self.sideways {
            let nx = y;
            let ny = -x;
            x = nx;
            y = ny;
        }
        let local = Point::new(self.pen_x + x * self.scale, self.pen_y - y * self.scale);
        self.world.apply(local)
    }
}

impl OutlineBuilder for OutlineSink {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        self.current = p;
        self.cmds.push(PathCmd::Move(p));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        self.current = p;
        self.cmds.push(PathCmd::Line(p));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let p0 = self.current;
        let c = self.map(x1, y1);
        let p = self.map(x, y);
        let c1 = Point::new(p0.x + (2.0 / 3.0) * (c.x - p0.x), p0.y + (2.0 / 3.0) * (c.y - p0.y));
        let c2 = Point::new(p.x + (2.0 / 3.0) * (c.x - p.x), p.y + (2.0 / 3.0) * (c.y - p.y));
        self.current = p;
        self.cmds.push(PathCmd::Cubic(c1, c2, p));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let c1 = self.map(x1, y1);
        let c2 = self.map(x2, y2);
        let p = self.map(x, y);
        self.current = p;
        self.cmds.push(PathCmd::Cubic(c1, c2, p));
    }

    fn close(&mut self) {
        self.cmds.push(PathCmd::Close);
    }
}

pub fn font_ok(data: &[u8]) -> bool {
    Face::parse(data, 0).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obfuscation_roundtrip_header() {
        let name = "Fonts/00112233-4455-6677-8899-AABBCCDDEEFF.odttf";
        let mut raw = vec![0u8; 40];
        raw[0] = 0;
        raw[1] = 1;
        raw[2] = 0;
        raw[3] = 0;
        let key = guid_keys(name).unwrap()[0];
        apply_key(&mut raw, &key);
        assert_ne!(&raw[..4], &[0, 1, 0, 0]);
        let back = deobfuscate(name, &raw);
        assert_eq!(&back[..4], &[0, 1, 0, 0]);
    }

    #[test]
    fn pdftron_reversed_guid_roundtrip() {
        let name = "Fonts/0f510971-4a15-52b2-ea11-b20400000001.odttf";
        let mut raw = b"OTTO".to_vec();
        raw.extend_from_slice(&[0u8; 36]);
        let key = guid_keys(name).unwrap()[1];
        apply_key(&mut raw, &key);
        assert_ne!(&raw[..4], b"OTTO");
        let back = deobfuscate(name, &raw);
        assert_eq!(&back[..4], b"OTTO");
    }

    #[test]
    fn indices_cluster() {
        let m = parse_indices("(2:1)5,50;6");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].code_units, 2);
        assert_eq!(m[0].glyph, Some(5));
        assert_eq!(m[0].advance, Some(50.0));
        assert_eq!(m[1].glyph, Some(6));
    }
}
