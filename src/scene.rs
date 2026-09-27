//! 烘焙到页面坐标之后的显示列表. 坐标仍是 XPS (Y 向下, 1/96 英寸).

use crate::geom::{Affine, Color};
use crate::pathgeom::PathGeom;

#[derive(Clone, Debug)]
pub struct Page {
    pub width: f32,
    pub height: f32,
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug)]
pub enum Node {
    Group {
        opacity: f32,
        clip: Option<PathGeom>,
        children: Vec<Node>,
    },
    Path(PathDraw),
}

#[derive(Clone, Debug)]
pub struct PathDraw {
    pub path: PathGeom,
    pub fill: Option<Fill>,
    pub stroke: Option<Stroke>,
    pub clip: Option<PathGeom>,
    /// 非空时写成 PDF ActualText, 方便复制原文.
    pub unicode: String,
}

#[derive(Clone, Debug)]
pub struct Fill {
    pub paint: Paint,
    pub alpha: f32,
}

#[derive(Clone, Debug)]
pub enum Paint {
    Solid(Color),
    Image(ImagePaint),
}

#[derive(Clone, Debug)]
pub struct ImagePaint {
    pub source: ImageSource,
    pub tiles: Vec<Affine>,
}

#[derive(Clone, Debug)]
pub enum ImageSource {
    Part(String),
    Raster(Raster),
}

#[derive(Clone, Debug)]
pub struct Raster {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, Debug)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

#[derive(Clone, Debug)]
pub struct Stroke {
    pub color: Color,
    pub alpha: f32,
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter: f32,
    pub dash: Vec<f32>,
    pub phase: f32,
}
