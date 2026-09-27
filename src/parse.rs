//! 把 FixedPage 收成页面坐标下的显示列表.
//!
//! 画刷在元素自己的坐标系里计算, 再乘上 RenderTransform 和父级变换.
//! `RenderTransformOrigin` 只有在变换不是单位矩阵时才绕包围盒旋转, 避免改动已经烘焙好的平移.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Seek};
use std::sync::Arc;

use roxmltree::Node as XmlNode;

use crate::error::Error;
use crate::font::{self, Placement};
use crate::geom::{
    parse_color, parse_floats, static_resource_key, union_rect, Affine, Color, FillRule, Matrix, Point,
    Rect,
};
use crate::package::{self, Package};
use crate::pathgeom::{self, PathGeom};
use crate::scene::{Fill, ImagePaint, ImageSource, LineCap, LineJoin, Node, Page, Paint, PathDraw, Raster, Stroke};

const MAX_TILES: i32 = 48;

pub struct Session<R: Read + Seek> {
    package: Package<R>,
    fonts: HashMap<String, Arc<Vec<u8>>>,
    image_size: HashMap<String, Option<(u32, u32)>>,
    warnings: Vec<String>,
    seen_warn: HashSet<String>,
}

impl<R: Read + Seek> Session<R> {
    pub fn open(reader: R) -> Result<Self, Error> {
        Ok(Self {
            package: Package::open(reader)?,
            fonts: HashMap::new(),
            image_size: HashMap::new(),
            warnings: Vec::new(),
            seen_warn: HashSet::new(),
        })
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn package_mut(&mut self) -> &mut Package<R> {
        &mut self.package
    }

    pub fn page_parts(&mut self) -> Result<Vec<String>, Error> {
        if let Some(fdseq) = self.fixed_sequence()? {
            let mut pages = Vec::new();
            let doc_xml = self.read_xml(&fdseq)?;
            let doc = roxmltree::Document::parse(&doc_xml).map_err(|e| Error::msg(format!("FixedDocumentSequence 无法解析: {e}")))?;
            let mut docs = Vec::new();
            for node in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "DocumentReference") {
                if let Some(src) = attr(node, "Source") {
                    docs.push(package::resolve_uri(&fdseq, src));
                }
            }
            for doc_part in docs {
                let xml = match self.read_xml(&doc_part) {
                    Ok(x) => x,
                    Err(e) => {
                        self.warn(e.to_string());
                        continue;
                    }
                };
                let parsed = match roxmltree::Document::parse(&xml) {
                    Ok(d) => d,
                    Err(e) => {
                        self.warn(format!("{doc_part} 无法解析: {e}"));
                        continue;
                    }
                };
                for node in parsed.descendants().filter(|n| n.is_element() && n.tag_name().name() == "PageContent") {
                    if let Some(src) = attr(node, "Source") {
                        pages.push(package::resolve_uri(&doc_part, src));
                    }
                }
            }
            if !pages.is_empty() {
                return Ok(pages);
            }
        }
        let mut pages: Vec<String> = self
            .package
            .part_names()
            .filter(|n| {
                let l = n.to_ascii_lowercase();
                l.ends_with(".fpage") && !l.contains("/[")
            })
            .map(|s| s.to_string())
            .collect();
        pages.sort_by(|a, b| natural_cmp(a, b));
        if pages.is_empty() {
            Err(Error::msg("没有找到 FixedPage, 这不像 XPS / OXPS"))
        } else {
            Ok(pages)
        }
    }

    pub fn parse_page(&mut self, part: &str) -> Result<Page, Error> {
        let xml = self.read_xml(part)?;
        let doc = roxmltree::Document::parse(&xml).map_err(|e| Error::msg(format!("{part} 无法解析: {e}")))?;
        let root = doc.root_element();
        let width = f32_attr(root, "Width", 816.0).max(1.0);
        let height = f32_attr(root, "Height", 1056.0).max(1.0);
        let mut loader = Loader {
            session: self,
            stack: Vec::new(),
            loading: HashSet::new(),
            unknown: HashSet::new(),
        };
        let raw = loader.parse_element(root, part).unwrap_or(Raw {
            tf: Matrix::identity(),
            origin: (0.0, 0.0),
            opacity: 1.0,
            clip: None,
            body: RawBody::Group(Vec::new()),
        });
        let mut nodes = Vec::new();
        loader.bake(&raw, Matrix::identity(), &mut nodes);
        Ok(Page { width, height, nodes })
    }

    fn fixed_sequence(&mut self) -> Result<Option<String>, Error> {
        let rels = match self.package.read_part("_rels/.rels") {
            Ok(b) => b,
            Err(_) => return Ok(self.find_fdseq()),
        };
        let xml = package::xml_to_utf8(&rels)?;
        let doc = match roxmltree::Document::parse(&xml) {
            Ok(d) => d,
            Err(_) => return Ok(self.find_fdseq()),
        };
        for node in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "Relationship") {
            let kind = attr(node, "Type").unwrap_or("");
            if kind.to_ascii_lowercase().contains("fixedrepresentation") {
                if let Some(target) = attr(node, "Target") {
                    return Ok(Some(package::normalize_part(target)));
                }
            }
        }
        Ok(self.find_fdseq())
    }

    fn find_fdseq(&self) -> Option<String> {
        self.package.part_names().find(|n| n.to_ascii_lowercase().ends_with(".fdseq")).map(|s| s.to_string())
    }

    fn read_xml(&mut self, part: &str) -> Result<String, Error> {
        let bytes = self.package.read_part(part)?;
        package::xml_to_utf8(&bytes)
    }

    pub fn push_warning(&mut self, msg: impl Into<String>) {
        self.warn(msg);
    }

    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if self.seen_warn.insert(msg.clone()) && self.warnings.len() < 16 {
            self.warnings.push(msg);
        }
    }
}

struct Loader<'a, R: Read + Seek> {
    session: &'a mut Session<R>,
    stack: Vec<HashMap<String, Resource>>,
    loading: HashSet<String>,
    unknown: HashSet<String>,
}

#[derive(Clone)]
enum Resource {
    Brush(Brush),
    Geom(PathGeom),
}

#[derive(Clone)]
enum Brush {
    Solid(Color),
    Image(ImageBrush),
    Linear(GradBrush),
    Radial(GradBrush),
}

#[derive(Clone)]
struct ImageBrush {
    source: String,
    viewbox: Rect,
    viewbox_rel: bool,
    viewport: Rect,
    viewport_rel: bool,
    stretch: Stretch,
    tile: Tile,
    transform: Matrix,
    opacity: f32,
}

#[derive(Clone)]
struct GradBrush {
    start: Point,
    end: Point,
    center: Point,
    origin: Point,
    rx: f32,
    ry: f32,
    relative: bool,
    spread: Spread,
    transform: Matrix,
    opacity: f32,
    stops: Vec<(f32, Color)>,
}

#[derive(Clone, Copy)]
enum Stretch {
    Fill,
    Uniform,
    UniformToFill,
    None,
}

#[derive(Clone, Copy, PartialEq)]
enum Tile {
    None,
    Tile,
    FlipX,
    FlipY,
    FlipXY,
}

#[derive(Clone, Copy)]
enum Spread {
    Pad,
    Repeat,
    Reflect,
}

struct Raw {
    tf: Matrix,
    origin: (f32, f32),
    opacity: f32,
    clip: Option<PathGeom>,
    body: RawBody,
}

enum RawBody {
    Group(Vec<Raw>),
    Shape(Shape),
    Glyph(GlyphRaw),
}

struct Shape {
    path: PathGeom,
    fill: Option<Brush>,
    stroke: Option<StrokeRaw>,
}

struct StrokeRaw {
    brush: Brush,
    thickness: f32,
    cap: LineCap,
    join: LineJoin,
    miter: f32,
    dash: Vec<f32>,
    phase: f32,
}

struct GlyphRaw {
    font: Arc<Vec<u8>>,
    em: f32,
    places: Vec<Placement>,
    fill: Brush,
    bold: bool,
    italic: bool,
    sideways: bool,
    unicode: String,
}

impl<'a, R: Read + Seek> Loader<'a, R> {
    fn parse_element(&mut self, node: XmlNode<'_, '_>, base: &str) -> Option<Raw> {
        let name = node.tag_name().name();
        if name.contains('.') {
            return None;
        }
        if matches!(name, "PrintTicket" | "LinkTarget" | "SignatureDefinition" | "SpotLocation") {
            return None;
        }
        if !matches!(name, "Canvas" | "Path" | "Glyphs" | "FixedPage") {
            if self.unknown.insert(name.to_string()) {
                self.session.warn(format!("跳过不支持的元素 {name}"));
            }
            return None;
        }
        let pushed = self.push_resources(node, base);
        let tf = read_transform(node);
        let origin = origin_attr(node);
        let opacity = f32_attr(node, "Opacity", 1.0).clamp(0.0, 1.0);
        self.note_opacity_mask(node);
        let clip = self.read_clip(node);
        let body = if name == "Path" {
            RawBody::Shape(self.parse_path(node, base))
        } else if name == "Glyphs" {
            match self.parse_glyphs(node, base) {
                Some(g) => RawBody::Glyph(g),
                None => {
                    if pushed {
                        self.stack.pop();
                    }
                    return None;
                }
            }
        } else {
            let mut kids = Vec::new();
            for child in node.children().filter(|n| n.is_element()) {
                if child.tag_name().name().contains('.') {
                    continue;
                }
                if let Some(raw) = self.parse_element(child, base) {
                    kids.push(raw);
                }
            }
            RawBody::Group(kids)
        };
        if pushed {
            self.stack.pop();
        }
        if opacity <= 0.0 {
            return None;
        }
        Some(Raw { tf, origin, opacity, clip, body })
    }

    fn push_resources(&mut self, node: XmlNode<'_, '_>, base: &str) -> bool {
        let Some(prop) = child_suffix(node, ".Resources") else {
            return false;
        };
        let mut map = HashMap::new();
        for child in prop.children().filter(|n| n.is_element()) {
            if child.tag_name().name() == "ResourceDictionary" {
                self.fill_dictionary(child, base, &mut map);
            } else {
                self.insert_resource(child, base, &mut map);
            }
        }
        self.stack.push(map);
        true
    }

    fn fill_dictionary(&mut self, node: XmlNode<'_, '_>, base: &str, map: &mut HashMap<String, Resource>) {
        if let Some(src) = attr(node, "Source") {
            let part = package::resolve_uri(base, src);
            self.load_external(&part, map);
        }
        for child in node.children().filter(|n| n.is_element()) {
            if child.tag_name().name() == "ResourceDictionary" {
                self.fill_dictionary(child, base, map);
            } else {
                self.insert_resource(child, base, map);
            }
        }
    }

    fn load_external(&mut self, part: &str, map: &mut HashMap<String, Resource>) {
        if !self.loading.insert(part.to_string()) {
            return;
        }
        let xml = match self.session.read_xml(part) {
            Ok(x) => x,
            Err(e) => {
                self.session.warn(format!("资源字典 {part}: {e}"));
                self.loading.remove(part);
                return;
            }
        };
        if let Ok(doc) = roxmltree::Document::parse(&xml) {
            let root = doc.root_element();
            if root.tag_name().name() == "ResourceDictionary" {
                self.fill_dictionary(root, part, map);
            }
        } else {
            self.session.warn(format!("资源字典无法解析: {part}"));
        }
        self.loading.remove(part);
    }

    fn insert_resource(&mut self, node: XmlNode<'_, '_>, base: &str, map: &mut HashMap<String, Resource>) {
        let Some(key) = attr(node, "Key") else {
            return;
        };
        let key = key.to_string();
        let name = node.tag_name().name();
        if is_geometry_name(name) {
            if let Some(g) = self.parse_geometry(node) {
                map.insert(key, Resource::Geom(g));
            }
            return;
        }
        if let Some(brush) = self.parse_brush_element(node, base) {
            map.insert(key, Resource::Brush(brush));
        }
    }

    fn lookup_brush(&mut self, value: &str, base: &str) -> Option<Brush> {
        let value = value.trim();
        if value.is_empty() || value == "{}" {
            return None;
        }
        if let Some(key) = static_resource_key(value) {
            let key = key.to_string();
            for map in self.stack.iter().rev() {
                if let Some(Resource::Brush(b)) = map.get(&key) {
                    return Some(b.clone());
                }
            }
            self.session.warn(format!("找不到画刷资源 {key}"));
            return None;
        }
        if let Some(c) = parse_color(value) {
            return Some(Brush::Solid(c));
        }
        if value.starts_with('/') || value.contains('.') {
            return Some(Brush::Image(ImageBrush::from_source(package::resolve_uri(base, value))));
        }
        self.session.warn(format!("无法识别的颜色或画刷: {value}"));
        None
    }

    fn lookup_geom(&mut self, value: &str) -> Option<PathGeom> {
        if let Some(key) = static_resource_key(value) {
            let key = key.to_string();
            for map in self.stack.iter().rev() {
                if let Some(Resource::Geom(g)) = map.get(&key) {
                    return Some(g.clone());
                }
            }
            self.session.warn(format!("找不到几何资源 {key}"));
            return None;
        }
        let g = pathgeom::parse_path(value);
        if g.cmds.is_empty() { None } else { Some(g) }
    }

    fn parse_path(&mut self, node: XmlNode<'_, '_>, base: &str) -> Shape {
        let mut path = if let Some(data) = child_suffix(node, ".Data") {
            data.children().find(|n| n.is_element()).and_then(|n| self.parse_geometry(n)).or_else(|| {
                data.text().map(str::trim).filter(|s| !s.is_empty()).and_then(|s| self.lookup_geom(s))
            })
        } else if let Some(data) = attr(node, "Data") {
            self.lookup_geom(data)
        } else {
            None
        }
        .unwrap_or_else(PathGeom::empty);
        if let Some(rule) = attr(node, "FillRule") {
            if !path.fill_explicit {
                path.fill = parse_fill_rule(rule);
            }
        }
        let fill = self.read_brush_prop(node, "Fill", base);
        let stroke_brush = self.read_brush_prop(node, "Stroke", base);
        let stroke = stroke_brush.map(|brush| StrokeRaw {
            brush,
            thickness: f32_attr(node, "StrokeThickness", 1.0).max(0.0),
            cap: parse_cap(attr(node, "StrokeStartLineCap").or_else(|| attr(node, "StrokeEndLineCap"))),
            join: parse_join(attr(node, "StrokeLineJoin")),
            miter: f32_attr(node, "StrokeMiterLimit", 10.0).max(1.0),
            dash: attr(node, "StrokeDashArray").map(parse_floats).unwrap_or_default(),
            phase: f32_attr(node, "StrokeDashOffset", 0.0),
        });
        Shape { path, fill, stroke }
    }

    fn parse_glyphs(&mut self, node: XmlNode<'_, '_>, base: &str) -> Option<GlyphRaw> {
        let em = f32_attr(node, "FontRenderingEmSize", 0.0);
        if em <= 0.0 {
            return None;
        }
        let uri = attr(node, "FontUri")?;
        let part = package::resolve_uri(base, uri);
        let font = self.font_data(&part)?;
        let unicode = unescape_unicode(attr(node, "UnicodeString").unwrap_or(""));
        let indices = attr(node, "Indices").filter(|s| !s.trim().is_empty());
        let rtl = attr(node, "BidiLevel").and_then(|s| s.parse::<i32>().ok()).is_some_and(|n| n % 2 == 1);
        let sideways = flag_attr(node, "IsSideways");
        let places = font::layout_glyphs(
            &font,
            em,
            f32_attr(node, "OriginX", 0.0),
            f32_attr(node, "OriginY", 0.0),
            &unicode,
            indices,
            rtl,
            sideways,
        );
        if places.is_empty() {
            return None;
        }
        let fill = self.read_brush_prop(node, "Fill", base).unwrap_or(Brush::Solid(Color::rgba(0.0, 0.0, 0.0, 1.0)));
        let sim = attr(node, "StyleSimulations").unwrap_or("");
        Some(GlyphRaw {
            font,
            em,
            places,
            fill,
            bold: sim.contains("Bold"),
            italic: sim.contains("Italic"),
            sideways,
            unicode,
        })
    }

    fn font_data(&mut self, part: &str) -> Option<Arc<Vec<u8>>> {
        if let Some(hit) = self.session.fonts.get(part) {
            return Some(hit.clone());
        }
        let bytes = match self.session.package.read_part(part) {
            Ok(b) => b,
            Err(e) => {
                self.session.warn(format!("字体 {part}: {e}"));
                return None;
            }
        };
        let plain = font::deobfuscate(part, &bytes);
        if !font::font_ok(&plain) {
            self.session.warn(format!("无法解析字体 {part}"));
            return None;
        }
        let arc = Arc::new(plain);
        self.session.fonts.insert(part.to_string(), arc.clone());
        Some(arc)
    }

    fn read_brush_prop(&mut self, node: XmlNode<'_, '_>, prop: &str, base: &str) -> Option<Brush> {
        let suffix = format!(".{prop}");
        if let Some(el) = child_suffix(node, &suffix) {
            if let Some(child) = el.children().find(|n| n.is_element()) {
                return self.parse_brush_element(child, base);
            }
            if let Some(text) = el.text().map(str::trim).filter(|s| !s.is_empty()) {
                return self.lookup_brush(text, base);
            }
        }
        attr(node, prop).and_then(|v| self.lookup_brush(v, base))
    }

    fn parse_brush_element(&mut self, node: XmlNode<'_, '_>, base: &str) -> Option<Brush> {
        if let Some(key) = attr(node, "Key") {
            if static_resource_key(&format!("{{StaticResource {key}}}")).is_some() && attr(node, "Color").is_none() && node.tag_name().name() == "StaticResource" {
                return self.lookup_brush(&format!("{{StaticResource {key}}}"), base);
            }
        }
        let opacity = f32_attr(node, "Opacity", 1.0).clamp(0.0, 1.0);
        let transform = brush_transform(node);
        match node.tag_name().name() {
            "SolidColorBrush" => {
                let c = attr(node, "Color").and_then(|v| {
                    if let Some(key) = static_resource_key(v) {
                        self.lookup_brush(&format!("{{StaticResource {key}}}"), base).and_then(|b| match b {
                            Brush::Solid(c) => Some(c),
                            _ => None,
                        })
                    } else {
                        parse_color(v)
                    }
                })?;
                Some(Brush::Solid(c.mul_alpha(opacity)))
            }
            "ImageBrush" => {
                let src = attr(node, "ImageSource")?;
                let source = if let Some(key) = static_resource_key(src) {
                    match self.lookup_brush(&format!("{{StaticResource {key}}}"), base)? {
                        Brush::Image(img) => img.source,
                        _ => {
                            self.session.warn(format!("资源 {key} 不是图像"));
                            return None;
                        }
                    }
                } else {
                    package::resolve_uri(base, src)
                };
                Some(Brush::Image(ImageBrush {
                    source,
                    viewbox: rect_attr(node, "Viewbox", Rect::new(0.0, 0.0, 1.0, 1.0)),
                    viewbox_rel: units_rel(attr(node, "ViewboxUnits"), true),
                    viewport: rect_attr(node, "Viewport", Rect::new(0.0, 0.0, 1.0, 1.0)),
                    viewport_rel: units_rel(attr(node, "ViewportUnits"), true),
                    stretch: parse_stretch(attr(node, "Stretch")),
                    tile: parse_tile(attr(node, "TileMode")),
                    transform,
                    opacity,
                }))
            }
            "LinearGradientBrush" => Some(Brush::Linear(self.parse_grad(node, base, transform, opacity, false))),
            "RadialGradientBrush" => Some(Brush::Radial(self.parse_grad(node, base, transform, opacity, true))),
            "VisualBrush" | "DrawingBrush" => {
                self.session.warn(format!("跳过 {}", node.tag_name().name()));
                None
            }
            other => {
                self.session.warn(format!("跳过画刷 {other}"));
                None
            }
        }
    }

    fn parse_grad(&mut self, node: XmlNode<'_, '_>, base: &str, transform: Matrix, opacity: f32, _radial: bool) -> GradBrush {
        let mut stops = Vec::new();
        for stop in node.descendants().filter(|n| n.is_element() && n.tag_name().name() == "GradientStop") {
            let offset = f32_attr(stop, "Offset", 0.0);
            let color = attr(stop, "Color").and_then(|v| self.lookup_brush(v, base)).and_then(|b| match b {
                Brush::Solid(c) => Some(c),
                _ => None,
            });
            if let Some(color) = color {
                stops.push((offset, color));
            }
        }
        stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let relative = attr(node, "MappingMode").map(|s| s != "Absolute").unwrap_or(true);
        GradBrush {
            start: point_attr(node, "StartPoint", Point::new(0.0, 0.0)),
            end: point_attr(node, "EndPoint", Point::new(1.0, 1.0)),
            center: point_attr(node, "Center", Point::new(0.5, 0.5)),
            origin: point_attr(node, "GradientOrigin", Point::new(0.5, 0.5)),
            rx: f32_attr(node, "RadiusX", 0.5),
            ry: f32_attr(node, "RadiusY", 0.5),
            relative,
            spread: parse_spread(attr(node, "SpreadMethod")),
            transform,
            opacity,
            stops,
        }
    }

    fn parse_geometry(&mut self, node: XmlNode<'_, '_>) -> Option<PathGeom> {
        match node.tag_name().name() {
            "PathGeometry" | "StreamGeometry" => {
                let mut g = if let Some(fig) = attr(node, "Figures") {
                    pathgeom::parse_path(fig)
                } else if node.children().any(|n| n.is_element() && n.tag_name().name() == "PathFigure") {
                    figures_to_path(node)
                } else {
                    node.text().map(pathgeom::parse_path).unwrap_or_else(PathGeom::empty)
                };
                if let Some(rule) = attr(node, "FillRule") {
                    if !g.fill_explicit {
                        g.fill = parse_fill_rule(rule);
                    }
                }
                Some(g)
            }
            "RectangleGeometry" => {
                let r = rect_attr(node, "Rect", Rect::new(0.0, 0.0, 0.0, 0.0));
                Some(pathgeom::rect_path(r.x, r.y, r.w, r.h, f32_attr(node, "RadiusX", 0.0), f32_attr(node, "RadiusY", 0.0)))
            }
            "EllipseGeometry" => {
                let c = point_attr(node, "Center", Point::new(0.0, 0.0));
                Some(pathgeom::ellipse_path(c.x, c.y, f32_attr(node, "RadiusX", 0.0), f32_attr(node, "RadiusY", 0.0)))
            }
            "LineGeometry" => {
                let a = point_attr(node, "StartPoint", Point::new(0.0, 0.0));
                let b = point_attr(node, "EndPoint", Point::new(0.0, 0.0));
                Some(pathgeom::line_path(a.x, a.y, b.x, b.y))
            }
            "GeometryGroup" => {
                let mut g = PathGeom::empty();
                if let Some(rule) = attr(node, "FillRule") {
                    g.fill = parse_fill_rule(rule);
                    g.fill_explicit = true;
                }
                for child in node.children().filter(|n| n.is_element()) {
                    if let Some(part) = self.parse_geometry(child) {
                        g.cmds.extend(part.cmds);
                    }
                }
                Some(g)
            }
            _ => None,
        }
    }

    fn read_clip(&mut self, node: XmlNode<'_, '_>) -> Option<PathGeom> {
        if let Some(el) = child_suffix(node, ".Clip") {
            if let Some(child) = el.children().find(|n| n.is_element()) {
                return self.parse_geometry(child);
            }
            if let Some(text) = el.text().map(str::trim).filter(|s| !s.is_empty()) {
                return self.lookup_geom(text);
            }
        }
        attr(node, "Clip").and_then(|v| self.lookup_geom(v))
    }

    fn note_opacity_mask(&mut self, node: XmlNode<'_, '_>) {
        if child_suffix(node, ".OpacityMask").is_some() || attr(node, "OpacityMask").is_some() {
            self.session.warn("OpacityMask 未实现, 该遮罩已忽略");
        }
    }

    fn bake(&mut self, raw: &Raw, parent: Matrix, out: &mut Vec<Node>) {
        let world = effective_tf(raw).then(parent);
        let clip = raw.clip.as_ref().map(|c| c.transformed(world));
        match &raw.body {
            RawBody::Group(kids) => {
                let mut children = Vec::new();
                for kid in kids {
                    self.bake(kid, world, &mut children);
                }
                if children.is_empty() {
                    return;
                }
                if raw.opacity < 0.999 || clip.is_some() {
                    out.push(Node::Group { opacity: raw.opacity, clip, children });
                } else {
                    out.extend(children);
                }
            }
            RawBody::Shape(shape) => {
                self.bake_shape(shape, world, raw.opacity, clip, out);
            }
            RawBody::Glyph(glyph) => {
                self.bake_glyph(glyph, world, raw.opacity, clip, out);
            }
        }
    }

    fn bake_shape(&mut self, shape: &Shape, world: Matrix, opacity: f32, clip: Option<PathGeom>, out: &mut Vec<Node>) {
        if shape.path.is_empty() && shape.path.cmds.is_empty() {
            return;
        }
        let local_bounds = shape.path.bounds();
        let path = shape.path.transformed(world);
        let fill = shape.fill.as_ref().and_then(|b| self.bake_fill(b, local_bounds, world, &path, opacity));
        let stroke = shape.stroke.as_ref().and_then(|s| self.bake_stroke(s, world, opacity));
        if fill.is_none() && stroke.is_none() {
            return;
        }
        let mut viewport_clip = None;
        if let Some(Brush::Image(img)) = &shape.fill {
            if img.tile == Tile::None {
                if let Some(bounds) = local_bounds {
                    let vp = viewport_rect(img, bounds);
                    if vp.w > 0.0 && vp.h > 0.0 {
                        viewport_clip = Some(pathgeom::rect_path(vp.x, vp.y, vp.w, vp.h, 0.0, 0.0).transformed(img.transform.then(world)));
                    }
                }
            }
        }
        if fill.is_none() && stroke.is_none() {
            return;
        }
        if let Some(vp) = viewport_clip {
            if fill.is_some() {
                out.push(Node::Group {
                    opacity: 1.0,
                    clip: Some(vp),
                    children: vec![Node::Path(PathDraw {
                        path: path.clone(),
                        fill,
                        stroke: None,
                        clip: clip.clone(),
                        unicode: String::new(),
                    })],
                });
            }
            if stroke.is_some() {
                out.push(Node::Path(PathDraw { path, fill: None, stroke, clip, unicode: String::new() }));
            }
        } else {
            out.push(Node::Path(PathDraw { path, fill, stroke, clip, unicode: String::new() }));
        }
    }

    fn bake_glyph(&mut self, glyph: &GlyphRaw, world: Matrix, opacity: f32, clip: Option<PathGeom>, out: &mut Vec<Node>) {
        let path = font::outline_placements(&glyph.font, glyph.em, &glyph.places, glyph.italic, glyph.sideways, world);
        if path.cmds.is_empty() {
            return;
        }
        let local_bounds = glyph_bounds(glyph);
        let baked_for_brush = font::outline_placements(
            &glyph.font,
            glyph.em,
            &glyph.places,
            glyph.italic,
            glyph.sideways,
            Matrix::identity(),
        );
        let fill = self.bake_fill(&glyph.fill, local_bounds.or(baked_for_brush.bounds()), world, &path, opacity);
        let Some(fill) = fill else {
            return;
        };
        let bold = if glyph.bold {
            if let Paint::Solid(c) = &fill.paint {
                Some(Stroke {
                    color: *c,
                    alpha: fill.alpha,
                    width: glyph.em * 0.03 * world.linear_scale(),
                    cap: LineCap::Round,
                    join: LineJoin::Round,
                    miter: 4.0,
                    dash: Vec::new(),
                    phase: 0.0,
                })
            } else {
                None
            }
        } else {
            None
        };
        out.push(Node::Path(PathDraw {
            path,
            fill: Some(fill),
            stroke: bold,
            clip,
            unicode: glyph.unicode.clone(),
        }));
    }

    fn bake_fill(&mut self, brush: &Brush, local_bounds: Option<Rect>, world: Matrix, page_path: &PathGeom, opacity: f32) -> Option<Fill> {
        match brush {
            Brush::Solid(c) => {
                let alpha = c.a * opacity;
                if alpha <= 0.0 {
                    return None;
                }
                Some(Fill { paint: Paint::Solid(*c), alpha })
            }
            Brush::Image(img) => {
                let bounds = local_bounds?;
                let (iw, ih) = self.image_size(&img.source)?;
                if iw == 0 || ih == 0 {
                    return None;
                }
                let tiles = image_tiles(img, bounds, iw, ih, world);
                if tiles.is_empty() {
                    return None;
                }
                Some(Fill {
                    paint: Paint::Image(ImagePaint { source: ImageSource::Part(img.source.clone()), tiles }),
                    alpha: img.opacity * opacity,
                })
            }
            Brush::Linear(g) | Brush::Radial(g) => {
                let page_bounds = page_path.bounds()?;
                let radial = matches!(brush, Brush::Radial(_));
                let raster = raster_grad(g, radial, local_bounds.unwrap_or(page_bounds), world, page_bounds);
                let alpha = g.opacity * opacity;
                if alpha <= 0.0 {
                    return None;
                }
                Some(Fill {
                    paint: Paint::Image(ImagePaint {
                        source: ImageSource::Raster(raster),
                        tiles: vec![raster_affine(page_bounds)],
                    }),
                    alpha,
                })
            }
        }
    }

    fn bake_stroke(&mut self, stroke: &StrokeRaw, world: Matrix, opacity: f32) -> Option<Stroke> {
        if stroke.thickness <= 0.0 {
            return None;
        }
        let color = match &stroke.brush {
            Brush::Solid(c) => *c,
            Brush::Linear(g) | Brush::Radial(g) => {
                self.session.warn("渐变描边已按色标平均值近似");
                average_stops(&g.stops).mul_alpha(g.opacity)
            }
            Brush::Image(_) => {
                self.session.warn("图像描边未实现, 已跳过");
                return None;
            }
        };
        let alpha = color.a * opacity;
        if alpha <= 0.0 {
            return None;
        }
        let scale = world.linear_scale();
        let width = stroke.thickness * scale;
        Some(Stroke {
            color,
            alpha,
            width,
            cap: stroke.cap,
            join: stroke.join,
            miter: stroke.miter,
            dash: stroke.dash.iter().map(|d| d * width).collect(),
            phase: stroke.phase * width,
        })
    }

    fn image_size(&mut self, part: &str) -> Option<(u32, u32)> {
        if let Some(hit) = self.session.image_size.get(part) {
            return *hit;
        }
        let size = match self.session.package.read_part(part) {
            Ok(bytes) => image_dimensions(&bytes),
            Err(e) => {
                self.session.warn(format!("图像 {part}: {e}"));
                None
            }
        };
        if size.is_none() {
            self.session.warn(format!("无法读取图像尺寸: {part}"));
        }
        self.session.image_size.insert(part.to_string(), size);
        size
    }
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some((w, h, _)) = jpeg_info(bytes) {
        return Some((w, h));
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    reader.into_dimensions().ok()
}

pub(crate) fn jpeg_info(bytes: &[u8]) -> Option<(u32, u32, u8)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 8 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        while i < bytes.len() && bytes[i] == 0xFF {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let marker = bytes[i];
        i += 1;
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if i + 7 >= bytes.len() {
                return None;
            }
            let h = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let c = bytes[i + 7];
            return Some((w, h, c));
        }
        if len < 2 || i + len > bytes.len() {
            break;
        }
        i += len;
    }
    None
}

fn image_tiles(img: &ImageBrush, bounds: Rect, iw: u32, ih: u32, world: Matrix) -> Vec<Affine> {
    let (bx, by, bw, bh) = viewbox_px(img, iw, ih);
    if bw < 1e-3 || bh < 1e-3 {
        return Vec::new();
    }
    let vp = viewport_rect(img, bounds);
    if vp.w.abs() < 1e-4 || vp.h.abs() < 1e-4 {
        return Vec::new();
    }
    let map_space = img.transform.then(world);
    let mut tiles = Vec::new();
    let push = |tiles: &mut Vec<Affine>, rect: Rect, flip_x: bool, flip_y: bool| {
        let mut aff = viewbox_affine(rect, bx, by, bw, bh, iw, ih);
        if flip_x {
            aff = aff.flip_x();
        }
        if flip_y {
            aff = aff.flip_y();
        }
        tiles.push(aff.then_matrix(map_space));
    };
    if img.tile == Tile::None {
        let dest = place_rect(vp, bw, bh, img.stretch);
        push(&mut tiles, dest, false, false);
        return tiles;
    }
    let (i0, i1) = tile_span(vp.x, vp.w, bounds.x, bounds.right());
    let (j0, j1) = tile_span(vp.y, vp.h, bounds.y, bounds.bottom());
    if (i1 - i0) * (j1 - j0) > MAX_TILES * MAX_TILES {
        push(&mut tiles, place_rect(vp, bw, bh, img.stretch), false, false);
        return tiles;
    }
    for j in j0..j1 {
        for i in i0..i1 {
            let rect = Rect::new(vp.x + i as f32 * vp.w, vp.y + j as f32 * vp.h, vp.w, vp.h);
            let dest = place_rect(rect, bw, bh, img.stretch);
            let fx = matches!(img.tile, Tile::FlipX | Tile::FlipXY) && i.rem_euclid(2) != 0;
            let fy = matches!(img.tile, Tile::FlipY | Tile::FlipXY) && j.rem_euclid(2) != 0;
            push(&mut tiles, dest, fx, fy);
        }
    }
    tiles
}

fn viewbox_px(img: &ImageBrush, iw: u32, ih: u32) -> (f32, f32, f32, f32) {
    if img.viewbox_rel {
        (img.viewbox.x * iw as f32, img.viewbox.y * ih as f32, img.viewbox.w * iw as f32, img.viewbox.h * ih as f32)
    } else {
        (img.viewbox.x, img.viewbox.y, img.viewbox.w, img.viewbox.h)
    }
}

fn viewport_rect(img: &ImageBrush, bounds: Rect) -> Rect {
    if img.viewport_rel {
        Rect::new(
            bounds.x + img.viewport.x * bounds.w,
            bounds.y + img.viewport.y * bounds.h,
            img.viewport.w * bounds.w,
            img.viewport.h * bounds.h,
        )
    } else {
        img.viewport
    }
}

fn place_rect(vp: Rect, bw: f32, bh: f32, stretch: Stretch) -> Rect {
    if vp.w <= 0.0 || vp.h <= 0.0 || bh <= 0.0 {
        return vp;
    }
    let aspect = bw / bh;
    let (w, h) = match stretch {
        Stretch::Fill => return vp,
        Stretch::None => (bw, bh),
        Stretch::Uniform => {
            if aspect > vp.w / vp.h { (vp.w, vp.w / aspect) } else { (vp.h * aspect, vp.h) }
        }
        Stretch::UniformToFill => {
            if aspect > vp.w / vp.h { (vp.h * aspect, vp.h) } else { (vp.w, vp.w / aspect) }
        }
    };
    Rect::new(vp.x + (vp.w - w) * 0.5, vp.y + (vp.h - h) * 0.5, w, h)
}

fn viewbox_affine(dest: Rect, bx: f32, by: f32, bw: f32, bh: f32, iw: u32, ih: u32) -> Affine {
    let fu = bx / iw as f32;
    let fv = by / ih as f32;
    let fw = (bw / iw as f32).max(1e-6);
    let fh = (bh / ih as f32).max(1e-6);
    Affine {
        a: dest.w / fw,
        b: 0.0,
        c: 0.0,
        d: -dest.h / fh,
        e: dest.x - fu / fw * dest.w,
        f: dest.y + (1.0 - fv) / fh * dest.h,
    }
}

fn tile_span(start: f32, size: f32, min: f32, max: f32) -> (i32, i32) {
    if size.abs() < 1e-4 {
        return (0, 1);
    }
    let i0 = ((min - start) / size).floor() as i32 - 1;
    let i1 = ((max - start) / size).ceil() as i32 + 1;
    (i0, i1)
}

fn raster_affine(bounds: Rect) -> Affine {
    Affine { a: bounds.w, b: 0.0, c: 0.0, d: -bounds.h, e: bounds.x, f: bounds.bottom() }
}

fn raster_grad(g: &GradBrush, radial: bool, local_bounds: Rect, world: Matrix, page_bounds: Rect) -> Raster {
    let w = (page_bounds.w * 2.0).clamp(8.0, 512.0) as u32;
    let h = (page_bounds.h * 2.0).clamp(8.0, 512.0) as u32;
    let inv = world.inverse().unwrap_or(Matrix::identity());
    let brush_inv = g.transform.inverse().unwrap_or(Matrix::identity());
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let px = page_bounds.x + (x as f32 + 0.5) / w as f32 * page_bounds.w;
            let py = page_bounds.y + (y as f32 + 0.5) / h as f32 * page_bounds.h;
            let local = brush_inv.apply(inv.apply(Point::new(px, py)));
            let color = if radial { sample_radial(g, local, local_bounds) } else { sample_linear(g, local, local_bounds) };
            let bytes = color.to_bytes();
            let i = ((y * w + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&bytes);
        }
    }
    Raster { w, h, rgba }
}

fn sample_linear(g: &GradBrush, p: Point, bounds: Rect) -> Color {
    let start = map_rel(g.start, g.relative, bounds);
    let end = map_rel(g.end, g.relative, bounds);
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let len2 = dx * dx + dy * dy;
    let t = if len2 < 1e-8 { 0.0 } else { ((p.x - start.x) * dx + (p.y - start.y) * dy) / len2 };
    lerp_stops(&g.stops, apply_spread(t, g.spread))
}

fn sample_radial(g: &GradBrush, p: Point, bounds: Rect) -> Color {
    let center = map_rel(g.center, g.relative, bounds);
    let origin = map_rel(g.origin, g.relative, bounds);
    let rx = if g.relative { g.rx * bounds.w } else { g.rx };
    let ry = if g.relative { g.ry * bounds.h } else { g.ry };
    if rx.abs() < 1e-4 || ry.abs() < 1e-4 {
        return lerp_stops(&g.stops, 1.0);
    }
    let fx = (origin.x - center.x) / rx;
    let fy = (origin.y - center.y) / ry;
    let dx = (p.x - center.x) / rx - fx;
    let dy = (p.y - center.y) / ry - fy;
    let a = dx * dx + dy * dy;
    let t = if a < 1e-10 {
        0.0
    } else {
        let b = 2.0 * (fx * dx + fy * dy);
        let c = fx * fx + fy * fy - 1.0;
        let disc = (b * b - 4.0 * a * c).max(0.0).sqrt();
        let t_edge = (-b + disc) / (2.0 * a);
        if t_edge.abs() < 1e-6 { 1.0 } else { 1.0 / t_edge }
    };
    lerp_stops(&g.stops, apply_spread(t, g.spread))
}

fn map_rel(p: Point, rel: bool, bounds: Rect) -> Point {
    if rel { Point::new(bounds.x + p.x * bounds.w, bounds.y + p.y * bounds.h) } else { p }
}

fn apply_spread(t: f32, spread: Spread) -> f32 {
    match spread {
        Spread::Pad => t.clamp(0.0, 1.0),
        Spread::Repeat => t.rem_euclid(1.0),
        Spread::Reflect => {
            let m = t.rem_euclid(2.0);
            if m <= 1.0 { m } else { 2.0 - m }
        }
    }
}

fn lerp_stops(stops: &[(f32, Color)], t: f32) -> Color {
    if stops.is_empty() {
        return Color::rgba(0.0, 0.0, 0.0, 0.0);
    }
    if t <= stops[0].0 {
        return stops[0].1;
    }
    for w in stops.windows(2) {
        if t <= w[1].0 {
            let span = (w[1].0 - w[0].0).max(1e-5);
            return mix_color(w[0].1, w[1].1, (t - w[0].0) / span);
        }
    }
    stops[stops.len() - 1].1
}

fn mix_color(a: Color, b: Color, t: f32) -> Color {
    Color::rgba(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, a.a + (b.a - a.a) * t)
}

fn average_stops(stops: &[(f32, Color)]) -> Color {
    if stops.is_empty() {
        return Color::rgba(0.0, 0.0, 0.0, 1.0);
    }
    let n = stops.len() as f32;
    let mut c = Color::rgba(0.0, 0.0, 0.0, 0.0);
    for (_, s) in stops {
        c.r += s.r;
        c.g += s.g;
        c.b += s.b;
        c.a += s.a;
    }
    Color::rgba(c.r / n, c.g / n, c.b / n, c.a / n)
}

fn glyph_bounds(g: &GlyphRaw) -> Option<Rect> {
    let mut r = None;
    for p in &g.places {
        let box_ = Rect::new(p.x, p.y - g.em, g.em, g.em);
        r = union_rect(r, Some(box_));
    }
    r
}

fn effective_tf(raw: &Raw) -> Matrix {
    raw.tf.around_origin(local_bounds(raw), raw.origin.0, raw.origin.1)
}

fn local_bounds(raw: &Raw) -> Option<Rect> {
    match &raw.body {
        RawBody::Shape(s) => s.path.bounds(),
        RawBody::Glyph(g) => glyph_bounds(g),
        RawBody::Group(kids) => {
            let mut u = None;
            for kid in kids {
                u = union_rect(u, effective_tf(kid).transform_rect_opt(local_bounds(kid)));
            }
            u
        }
    }
}

trait RectMap {
    fn transform_rect_opt(self, r: Option<Rect>) -> Option<Rect>;
}

impl RectMap for Matrix {
    fn transform_rect_opt(self, r: Option<Rect>) -> Option<Rect> {
        r.map(|r| self.transform_rect(r))
    }
}

fn figures_to_path(node: XmlNode<'_, '_>) -> PathGeom {
    let mut g = PathGeom::empty();
    for fig in node.children().filter(|n| n.is_element() && n.tag_name().name() == "PathFigure") {
        let start = point_attr(fig, "StartPoint", Point::new(0.0, 0.0));
        g.cmds.push(pathgeom::PathCmd::Move(start));
        let mut cur = start;
        for seg in fig.children().filter(|n| n.is_element()) {
            match seg.tag_name().name() {
                "PolyLineSegment" => {
                    for p in pathgeom::point_list(attr(seg, "Points").unwrap_or("")) {
                        g.cmds.push(pathgeom::PathCmd::Line(p));
                        cur = p;
                    }
                }
                "LineSegment" => {
                    let p = point_attr(seg, "Point", cur);
                    g.cmds.push(pathgeom::PathCmd::Line(p));
                    cur = p;
                }
                "PolyBezierSegment" | "PolyQuadraticBezierSegment" => {
                    let pts = pathgeom::point_list(attr(seg, "Points").unwrap_or(""));
                    if seg.tag_name().name() == "PolyBezierSegment" {
                        for c in pts.chunks(3) {
                            if c.len() == 3 {
                                g.cmds.push(pathgeom::PathCmd::Cubic(c[0], c[1], c[2]));
                                cur = c[2];
                            }
                        }
                    } else {
                        for c in pts.chunks(2) {
                            if c.len() == 2 {
                                let c1 = Point::new(cur.x + (2.0 / 3.0) * (c[0].x - cur.x), cur.y + (2.0 / 3.0) * (c[0].y - cur.y));
                                let c2 = Point::new(c[1].x + (2.0 / 3.0) * (c[0].x - c[1].x), c[1].y + (2.0 / 3.0) * (c[0].y - c[1].y));
                                g.cmds.push(pathgeom::PathCmd::Cubic(c1, c2, c[1]));
                                cur = c[1];
                            }
                        }
                    }
                }
                "BezierSegment" => {
                    let p1 = point_attr(seg, "Point1", cur);
                    let p2 = point_attr(seg, "Point2", cur);
                    let p3 = point_attr(seg, "Point3", cur);
                    g.cmds.push(pathgeom::PathCmd::Cubic(p1, p2, p3));
                    cur = p3;
                }
                "QuadraticBezierSegment" => {
                    let p1 = point_attr(seg, "Point1", cur);
                    let p2 = point_attr(seg, "Point2", cur);
                    let c1 = Point::new(cur.x + (2.0 / 3.0) * (p1.x - cur.x), cur.y + (2.0 / 3.0) * (p1.y - cur.y));
                    let c2 = Point::new(p2.x + (2.0 / 3.0) * (p1.x - p2.x), p2.y + (2.0 / 3.0) * (p1.y - p2.y));
                    g.cmds.push(pathgeom::PathCmd::Cubic(c1, c2, p2));
                    cur = p2;
                }
                "ArcSegment" => {
                    let p = point_attr(seg, "Point", cur);
                    let size = point_attr(seg, "Size", Point::new(0.0, 0.0));
                    let sweep = attr(seg, "SweepDirection").map(|s| s.eq_ignore_ascii_case("clockwise") || s == "1").unwrap_or(false);
                    let large = flag_attr(seg, "IsLargeArc");
                    let mut tmp = Vec::new();
                    // 复用路径语法里的弧, 直接拼一段 A 命令.
                    let spec = format!(
                        "M {},{} A {},{} {} {} {} {},{}",
                        cur.x, cur.y, size.x, size.y, f32_attr(seg, "RotationAngle", 0.0),
                        if large { 1 } else { 0 },
                        if sweep { 1 } else { 0 },
                        p.x, p.y
                    );
                    tmp.extend(pathgeom::parse_path(&spec).cmds.into_iter().skip(1));
                    g.cmds.extend(tmp);
                    cur = p;
                }
                _ => {}
            }
        }
        if flag_attr(fig, "IsClosed") {
            g.cmds.push(pathgeom::PathCmd::Close);
        }
    }
    g
}

fn read_transform(node: XmlNode<'_, '_>) -> Matrix {
    if let Some(el) = child_suffix(node, ".RenderTransform") {
        if let Some(child) = el.children().find(|n| n.is_element()) {
            return parse_transform_node(child);
        }
    }
    attr(node, "RenderTransform").map(parse_matrix_str).unwrap_or_else(Matrix::identity)
}

fn brush_transform(node: XmlNode<'_, '_>) -> Matrix {
    if let Some(el) = child_suffix(node, ".Transform").or_else(|| child_suffix(node, ".RelativeTransform")) {
        if let Some(child) = el.children().find(|n| n.is_element()) {
            return parse_transform_node(child);
        }
    }
    attr(node, "Transform").map(parse_matrix_str).unwrap_or_else(Matrix::identity)
}

fn parse_transform_node(node: XmlNode<'_, '_>) -> Matrix {
    match node.tag_name().name() {
        "TransformGroup" => {
            let mut m = Matrix::identity();
            for child in node.children().filter(|n| n.is_element()) {
                m = m.then(parse_transform_node(child));
            }
            m
        }
        "MatrixTransform" => attr(node, "Matrix").map(parse_matrix_str).unwrap_or_else(Matrix::identity),
        "TranslateTransform" => Matrix::translate(f32_attr(node, "X", 0.0), f32_attr(node, "Y", 0.0)),
        "ScaleTransform" => Matrix::scale_at(
            f32_attr(node, "ScaleX", 1.0),
            f32_attr(node, "ScaleY", 1.0),
            f32_attr(node, "CenterX", 0.0),
            f32_attr(node, "CenterY", 0.0),
        ),
        "RotateTransform" => Matrix::rotate_at(f32_attr(node, "Angle", 0.0), f32_attr(node, "CenterX", 0.0), f32_attr(node, "CenterY", 0.0)),
        "SkewTransform" => Matrix::skew_at(
            f32_attr(node, "AngleX", 0.0),
            f32_attr(node, "AngleY", 0.0),
            f32_attr(node, "CenterX", 0.0),
            f32_attr(node, "CenterY", 0.0),
        ),
        _ => Matrix::identity(),
    }
}

fn parse_matrix_str(s: &str) -> Matrix {
    let n = parse_floats(s);
    if n.len() >= 6 {
        Matrix { m11: n[0], m12: n[1], m21: n[2], m22: n[3], ox: n[4], oy: n[5] }
    } else {
        Matrix::identity()
    }
}

impl ImageBrush {
    fn from_source(source: String) -> Self {
        Self {
            source,
            viewbox: Rect::new(0.0, 0.0, 1.0, 1.0),
            viewbox_rel: true,
            viewport: Rect::new(0.0, 0.0, 1.0, 1.0),
            viewport_rel: true,
            stretch: Stretch::Fill,
            tile: Tile::None,
            transform: Matrix::identity(),
            opacity: 1.0,
        }
    }
}

fn is_geometry_name(name: &str) -> bool {
    matches!(name, "PathGeometry" | "StreamGeometry" | "RectangleGeometry" | "EllipseGeometry" | "LineGeometry" | "GeometryGroup")
}

fn attr<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attributes().find(|a| a.name() == name).map(|a| a.value())
}

fn child_suffix<'a>(node: XmlNode<'a, 'a>, suffix: &str) -> Option<XmlNode<'a, 'a>> {
    node.children().find(|n| n.is_element() && n.tag_name().name().ends_with(suffix))
}

fn f32_attr(node: XmlNode<'_, '_>, name: &str, default: f32) -> f32 {
    attr(node, name).and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

fn flag_attr(node: XmlNode<'_, '_>, name: &str) -> bool {
    matches!(attr(node, name).map(|s| s.trim()), Some("true" | "True" | "1"))
}

fn origin_attr(node: XmlNode<'_, '_>) -> (f32, f32) {
    let n = parse_floats(attr(node, "RenderTransformOrigin").unwrap_or("0,0"));
    (n.first().copied().unwrap_or(0.0), n.get(1).copied().unwrap_or(0.0))
}

fn point_attr(node: XmlNode<'_, '_>, name: &str, default: Point) -> Point {
    let n = parse_floats(attr(node, name).unwrap_or(""));
    if n.len() >= 2 { Point::new(n[0], n[1]) } else { default }
}

fn rect_attr(node: XmlNode<'_, '_>, name: &str, default: Rect) -> Rect {
    let n = parse_floats(attr(node, name).unwrap_or(""));
    if n.len() >= 4 { Rect::new(n[0], n[1], n[2], n[3]) } else { default }
}

fn units_rel(value: Option<&str>, default_rel: bool) -> bool {
    match value {
        Some("Absolute") => false,
        Some("RelativeToBoundingBox") => true,
        _ => default_rel,
    }
}

fn parse_fill_rule(s: &str) -> FillRule {
    if s.eq_ignore_ascii_case("nonzero") { FillRule::NonZero } else { FillRule::EvenOdd }
}

fn parse_cap(s: Option<&str>) -> LineCap {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("round") => LineCap::Round,
        Some("square") => LineCap::Square,
        _ => LineCap::Butt,
    }
}

fn parse_join(s: Option<&str>) -> LineJoin {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("round") => LineJoin::Round,
        Some("bevel") => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

fn parse_stretch(s: Option<&str>) -> Stretch {
    match s {
        Some("None") => Stretch::None,
        Some("Uniform") => Stretch::Uniform,
        Some("UniformToFill") => Stretch::UniformToFill,
        _ => Stretch::Fill,
    }
}

fn parse_tile(s: Option<&str>) -> Tile {
    match s {
        Some("Tile") => Tile::Tile,
        Some("FlipX") => Tile::FlipX,
        Some("FlipY") => Tile::FlipY,
        Some("FlipXY") => Tile::FlipXY,
        _ => Tile::None,
    }
}

fn parse_spread(s: Option<&str>) -> Spread {
    match s {
        Some("Repeat") => Spread::Repeat,
        Some("Reflect") => Spread::Reflect,
        _ => Spread::Pad,
    }
}

fn unescape_unicode(s: &str) -> String {
    s.replace("{}", "{")
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let ac = nat_chunks(a);
    let bc = nat_chunks(b);
    ac.cmp(&bc)
}

fn nat_chunks(s: &str) -> Vec<Nat> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut n = c.to_digit(10).unwrap() as u64;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                n = n.saturating_mul(10).saturating_add(d as u64);
                chars.next();
            }
            out.push(Nat::Num(n));
        } else {
            let mut t = String::new();
            t.push(c);
            while let Some(n) = chars.peek() {
                if n.is_ascii_digit() {
                    break;
                }
                t.push(*n);
                chars.next();
            }
            out.push(Nat::Text(t.to_ascii_lowercase()));
        }
    }
    out
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Nat {
    Num(u64),
    Text(String),
}
