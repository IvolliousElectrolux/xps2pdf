//! 显示列表写成 PDF. 文字轮廓配 ActualText, 这样 macOS 的预览也能复制原文.

use std::collections::HashMap;
use std::io::{Read, Seek};

use pdf_writer::writers::Resources;
use pdf_writer::types::{LineCapStyle, LineJoinStyle};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, TextStr};

use crate::error::Error;
use crate::geom::{Affine, FillRule};
use crate::package::Package;
use crate::parse::jpeg_info;
use crate::pathgeom::{PathCmd, PathGeom};
use crate::scene::{ImagePaint, ImageSource, LineCap, LineJoin, Node, Page, Paint, PathDraw, Stroke};

const XPS_TO_PT: f32 = 72.0 / 96.0;

struct CachedImage {
    id: Ref,
}

pub struct Document {
    pdf: Pdf,
    next: Ref,
    pages_id: Ref,
    page_ids: Vec<Ref>,
    images: HashMap<String, CachedImage>,
    gs: HashMap<u16, Ref>,
}

impl Document {
    pub fn new() -> Self {
        let mut pdf = Pdf::new();
        pdf.set_version(1, 6);
        Self {
            pdf,
            next: Ref::new(3),
            pages_id: Ref::new(2),
            page_ids: Vec::new(),
            images: HashMap::new(),
            gs: HashMap::new(),
        }
    }

    pub fn add_page<R: Read + Seek>(
        &mut self,
        package: &mut Package<R>,
        page: &Page,
        warnings: &mut Vec<String>,
    ) -> Result<(), Error> {
        let page_id = self.alloc();
        let content_id = self.alloc();
        let mut content = Content::new();
        let mut used = Used::default();
        emit_nodes(
            &mut content,
            &page.nodes,
            page.width,
            page.height,
            &mut used,
            self,
            package,
            warnings,
        );
        let raw = content.finish();
        let bytes = deflate(raw.as_slice());
        {
            let mut stream = self.pdf.stream(content_id, &bytes);
            stream.filter(Filter::FlateDecode);
            stream.finish();
        }
        {
            let mut page_w = self.pdf.page(page_id);
            page_w
                .parent(self.pages_id)
                .media_box(Rect::new(0.0, 0.0, page.width * XPS_TO_PT, page.height * XPS_TO_PT))
                .contents(content_id);
            write_resources(page_w.resources(), &used);
            page_w.finish();
        }
        self.page_ids.push(page_id);
        Ok(())
    }

    pub fn finish(mut self) -> Vec<u8> {
        let n = self.page_ids.len() as i32;
        self.pdf.catalog(Ref::new(1)).pages(self.pages_id);
        self.pdf.pages(self.pages_id).kids(self.page_ids).count(n);
        self.pdf.finish()
    }

    fn alloc(&mut self) -> Ref {
        self.next.bump()
    }

    fn ext_gs(&mut self, key: u16) -> Ref {
        if let Some(id) = self.gs.get(&key) {
            return *id;
        }
        let id = self.alloc();
        let alpha = key as f32 / 1000.0;
        let mut gs = self.pdf.ext_graphics(id);
        gs.non_stroking_alpha(alpha);
        gs.stroking_alpha(alpha);
        gs.finish();
        self.gs.insert(key, id);
        id
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct Used {
    xobjects: Vec<(String, Ref)>,
    gstates: Vec<(String, Ref)>,
}

fn emit_nodes<R: Read + Seek>(
    content: &mut Content,
    nodes: &[Node],
    page_w: f32,
    page_h: f32,
    used: &mut Used,
    doc: &mut Document,
    package: &mut Package<R>,
    warnings: &mut Vec<String>,
) {
    for node in nodes {
        match node {
            Node::Group { opacity, clip, children } => {
                if *opacity >= 0.999 {
                    content.save_state();
                    if let Some(clip) = clip {
                        clip_path(content, clip, page_h);
                    }
                    emit_nodes(content, children, page_w, page_h, used, doc, package, warnings);
                    content.restore_state();
                } else {
                    let mut sub = Content::new();
                    let mut sub_used = Used::default();
                    if let Some(clip) = clip {
                        clip_path(&mut sub, clip, page_h);
                    }
                    emit_nodes(&mut sub, children, page_w, page_h, &mut sub_used, doc, package, warnings);
                    let form_id = doc.alloc();
                    let form_name = format!("Fm{}", form_id.get());
                    write_form(doc, form_id, sub, &sub_used, page_w, page_h);
                    remember_xo(used, form_name.clone(), form_id);
                    content.save_state();
                    apply_alpha(content, used, doc, *opacity);
                    content.x_object(Name(form_name.as_bytes()));
                    content.restore_state();
                }
            }
            Node::Path(draw) => emit_path(content, draw, page_h, used, doc, package, warnings),
        }
    }
}

fn write_form(doc: &mut Document, id: Ref, content: Content, used: &Used, page_w: f32, page_h: f32) {
    let raw = content.finish();
    let bytes = deflate(raw.as_slice());
    let mut form = doc.pdf.form_xobject(id, &bytes);
    form.filter(Filter::FlateDecode);
    form.bbox(Rect::new(0.0, 0.0, page_w * XPS_TO_PT, page_h * XPS_TO_PT));
    {
        let mut group = form.group();
        group.transparency();
        group.isolated(true);
        group.color_space().device_rgb();
    }
    write_resources(form.resources(), used);
    form.finish();
}

fn emit_path<R: Read + Seek>(
    content: &mut Content,
    draw: &PathDraw,
    page_h: f32,
    used: &mut Used,
    doc: &mut Document,
    package: &mut Package<R>,
    warnings: &mut Vec<String>,
) {
    let marked = !draw.unicode.is_empty();
    if marked {
        content
            .begin_marked_content_with_properties(Name(b"Span"))
            .properties()
            .actual_text(TextStr(&draw.unicode));
    }
    content.save_state();
    if let Some(clip) = &draw.clip {
        clip_path(content, clip, page_h);
    }
    if let Some(fill) = &draw.fill {
        content.save_state();
        apply_alpha(content, used, doc, fill.alpha);
        match &fill.paint {
            Paint::Solid(color) => {
                content.set_fill_rgb(color.r, color.g, color.b);
                if write_path(content, &draw.path, page_h) {
                    if draw.path.fill == FillRule::EvenOdd {
                        content.fill_even_odd();
                    } else {
                        content.fill_nonzero();
                    }
                }
            }
            Paint::Image(image) => {
                if write_path(content, &draw.path, page_h) {
                    if draw.path.fill == FillRule::EvenOdd {
                        content.clip_even_odd();
                    } else {
                        content.clip_nonzero();
                    }
                    content.end_path();
                    paint_image(content, image, page_h, used, doc, package, warnings);
                }
            }
        }
        content.restore_state();
    }
    if let Some(stroke) = &draw.stroke {
        content.save_state();
        apply_alpha(content, used, doc, stroke.alpha);
        apply_stroke(content, stroke);
        if write_path(content, &draw.path, page_h) {
            content.stroke();
        }
        content.restore_state();
    }
    content.restore_state();
    if marked {
        content.end_marked_content();
    }
}

fn paint_image<R: Read + Seek>(
    content: &mut Content,
    image: &ImagePaint,
    page_h: f32,
    used: &mut Used,
    doc: &mut Document,
    package: &mut Package<R>,
    warnings: &mut Vec<String>,
) {
    let Some(id) = image_ref(doc, package, warnings, &image.source) else {
        return;
    };
    let name = format!("Im{}", id.get());
    remember_xo(used, name.clone(), id);
    for tile in &image.tiles {
        content.save_state();
        content.transform(affine_pdf(*tile, page_h));
        content.x_object(Name(name.as_bytes()));
        content.restore_state();
    }
}

fn image_ref<R: Read + Seek>(
    doc: &mut Document,
    package: &mut Package<R>,
    warnings: &mut Vec<String>,
    source: &ImageSource,
) -> Option<Ref> {
    match source {
        ImageSource::Part(part) => {
            if let Some(hit) = doc.images.get(part) {
                return Some(hit.id);
            }
            let bytes = match package.read_part(part) {
                Ok(b) => b,
                Err(e) => {
                    push_warn(warnings, format!("图像 {part}: {e}"));
                    return None;
                }
            };
            let id = embed_bytes(doc, &bytes, warnings)?;
            doc.images.insert(part.clone(), CachedImage { id });
            Some(id)
        }
        ImageSource::Raster(raster) => embed_rgba(doc, raster.w, raster.h, &raster.rgba),
    }
}

fn embed_bytes(doc: &mut Document, bytes: &[u8], warnings: &mut Vec<String>) -> Option<Ref> {
    if let Some((w, h, components)) = jpeg_info(bytes) {
        if components == 1 || components == 3 {
            return Some(embed_jpeg(doc, bytes, w, h, components));
        }
    }
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            embed_rgba(doc, rgba.width(), rgba.height(), rgba.as_raw())
        }
        Err(e) => {
            push_warn(warnings, format!("无法解码图像 (JPEG XR 等格式暂不支持): {e}"));
            None
        }
    }
}

fn embed_jpeg(doc: &mut Document, bytes: &[u8], w: u32, h: u32, components: u8) -> Ref {
    let id = doc.alloc();
    let mut image = doc.pdf.image_xobject(id, bytes);
    image.filter(Filter::DctDecode);
    image.width(w as i32);
    image.height(h as i32);
    image.bits_per_component(8);
    image.interpolate(true);
    if components == 1 {
        image.color_space().device_gray();
    } else {
        image.color_space().device_rgb();
    }
    image.finish();
    id
}

fn embed_rgba(doc: &mut Document, w: u32, h: u32, rgba: &[u8]) -> Option<Ref> {
    if w == 0 || h == 0 || rgba.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    let n = (w as usize) * (h as usize);
    let mut rgb = Vec::with_capacity(n * 3);
    let mut alpha = Vec::with_capacity(n);
    let mut gray = Vec::with_capacity(n);
    let mut is_gray = true;
    let mut opaque = true;
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
        gray.push(px[0]);
        alpha.push(px[3]);
        if px[0] != px[1] || px[1] != px[2] {
            is_gray = false;
        }
        if px[3] != 255 {
            opaque = false;
        }
    }
    let samples: &[u8] = if is_gray { &gray } else { &rgb };
    let mask = if opaque { None } else { Some(write_raw_image(doc, w, h, &deflate(&alpha), true, None)) };
    Some(write_raw_image(doc, w, h, &deflate(samples), is_gray, mask))
}

fn write_raw_image(doc: &mut Document, w: u32, h: u32, data: &[u8], gray: bool, mask: Option<Ref>) -> Ref {
    let id = doc.alloc();
    let mut image = doc.pdf.image_xobject(id, data);
    image.filter(Filter::FlateDecode);
    image.width(w as i32);
    image.height(h as i32);
    image.bits_per_component(8);
    image.interpolate(true);
    if let Some(mask) = mask {
        image.s_mask(mask);
    }
    if gray {
        image.color_space().device_gray();
    } else {
        image.color_space().device_rgb();
    }
    image.finish();
    id
}

fn apply_alpha(content: &mut Content, used: &mut Used, doc: &mut Document, alpha: f32) {
    if alpha >= 0.999 {
        return;
    }
    let key = (alpha.clamp(0.0, 1.0) * 1000.0).round() as u16;
    let id = doc.ext_gs(key);
    let name = format!("Gs{key}");
    if !used.gstates.iter().any(|(n, _)| n == &name) {
        used.gstates.push((name.clone(), id));
    }
    content.set_parameters(Name(name.as_bytes()));
}

fn apply_stroke(content: &mut Content, stroke: &Stroke) {
    content.set_stroke_rgb(stroke.color.r, stroke.color.g, stroke.color.b);
    content.set_line_width((stroke.width * XPS_TO_PT).max(0.01));
    content.set_line_cap(match stroke.cap {
        LineCap::Butt => LineCapStyle::ButtCap,
        LineCap::Round => LineCapStyle::RoundCap,
        LineCap::Square => LineCapStyle::ProjectingSquareCap,
    });
    content.set_line_join(match stroke.join {
        LineJoin::Miter => LineJoinStyle::MiterJoin,
        LineJoin::Round => LineJoinStyle::RoundJoin,
        LineJoin::Bevel => LineJoinStyle::BevelJoin,
    });
    content.set_miter_limit(stroke.miter);
    if !stroke.dash.is_empty() {
        let dashes: Vec<f32> = stroke.dash.iter().map(|d| d * XPS_TO_PT).collect();
        content.set_dash_pattern(dashes, stroke.phase * XPS_TO_PT);
    }
}

fn clip_path(content: &mut Content, path: &PathGeom, page_h: f32) {
    if !write_path(content, path, page_h) {
        return;
    }
    if path.fill == FillRule::EvenOdd {
        content.clip_even_odd();
    } else {
        content.clip_nonzero();
    }
    content.end_path();
}

fn write_path(content: &mut Content, path: &PathGeom, page_h: f32) -> bool {
    let mut any = false;
    for cmd in &path.cmds {
        match *cmd {
            PathCmd::Move(p) => {
                let (x, y) = pdf_xy(p.x, p.y, page_h);
                content.move_to(x, y);
            }
            PathCmd::Line(p) => {
                let (x, y) = pdf_xy(p.x, p.y, page_h);
                content.line_to(x, y);
                any = true;
            }
            PathCmd::Cubic(a, b, c) => {
                let (x1, y1) = pdf_xy(a.x, a.y, page_h);
                let (x2, y2) = pdf_xy(b.x, b.y, page_h);
                let (x, y) = pdf_xy(c.x, c.y, page_h);
                content.cubic_to(x1, y1, x2, y2, x, y);
                any = true;
            }
            PathCmd::Close => {
                content.close_path();
            }
        }
    }
    any
}

fn pdf_xy(x: f32, y: f32, page_h: f32) -> (f32, f32) {
    (x * XPS_TO_PT, (page_h - y) * XPS_TO_PT)
}

fn affine_pdf(aff: Affine, page_h: f32) -> [f32; 6] {
    [
        XPS_TO_PT * aff.a,
        -XPS_TO_PT * aff.b,
        XPS_TO_PT * aff.c,
        -XPS_TO_PT * aff.d,
        XPS_TO_PT * aff.e,
        (page_h - aff.f) * XPS_TO_PT,
    ]
}

fn remember_xo(used: &mut Used, name: String, id: Ref) {
    if used.xobjects.iter().any(|(n, _)| n == &name) {
        return;
    }
    used.xobjects.push((name, id));
}

fn write_resources(mut resources: Resources<'_>, used: &Used) {
    if !used.xobjects.is_empty() {
        let mut dict = resources.x_objects();
        for (name, id) in &used.xobjects {
            dict.pair(Name(name.as_bytes()), *id);
        }
    }
    if !used.gstates.is_empty() {
        let mut dict = resources.ext_g_states();
        for (name, id) in &used.gstates {
            dict.pair(Name(name.as_bytes()), *id);
        }
    }
}

fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

fn push_warn(warnings: &mut Vec<String>, msg: String) {
    if warnings.iter().any(|w| w == &msg) || warnings.len() >= 16 {
        return;
    }
    warnings.push(msg);
}
