//! XPS 用户坐标: 原点在左上, Y 向下, 1 单位 = 1/96 英寸.
//! 变换沿用 WPF 行向量: `x' = m11*x + m21*y + ox`, `y' = m12*x + m22*y + oy`.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    pub fn union(self, other: Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Self::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Matrix {
    pub m11: f32,
    pub m12: f32,
    pub m21: f32,
    pub m22: f32,
    pub ox: f32,
    pub oy: f32,
}

impl Matrix {
    pub const fn identity() -> Self {
        Self { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, ox: 0.0, oy: 0.0 }
    }

    pub fn is_identity(self) -> bool {
        (self.m11 - 1.0).abs() < 1e-5
            && self.m12.abs() < 1e-5
            && self.m21.abs() < 1e-5
            && (self.m22 - 1.0).abs() < 1e-5
            && self.ox.abs() < 1e-5
            && self.oy.abs() < 1e-5
    }

    pub fn translate(x: f32, y: f32) -> Self {
        Self { ox: x, oy: y, ..Self::identity() }
    }

    pub fn scale(sx: f32, sy: f32) -> Self {
        Self { m11: sx, m22: sy, ..Self::identity() }
    }

    /// 正角度在 Y 向下的坐标系里是顺时针, 与 WPF `RotateTransform` 一致.
    pub fn rotate_deg(angle: f32) -> Self {
        let (s, c) = angle.to_radians().sin_cos();
        Self { m11: c, m12: s, m21: -s, m22: c, ox: 0.0, oy: 0.0 }
    }

    pub fn scale_at(sx: f32, sy: f32, cx: f32, cy: f32) -> Self {
        Self::translate(-cx, -cy)
            .then(Self::scale(sx, sy))
            .then(Self::translate(cx, cy))
    }

    pub fn rotate_at(angle: f32, cx: f32, cy: f32) -> Self {
        Self::translate(-cx, -cy)
            .then(Self::rotate_deg(angle))
            .then(Self::translate(cx, cy))
    }

    pub fn skew_at(angle_x: f32, angle_y: f32, cx: f32, cy: f32) -> Self {
        let skew = Self {
            m11: 1.0,
            m12: angle_y.to_radians().tan(),
            m21: angle_x.to_radians().tan(),
            m22: 1.0,
            ox: 0.0,
            oy: 0.0,
        };
        Self::translate(-cx, -cy)
            .then(skew)
            .then(Self::translate(cx, cy))
    }

    /// 先应用 `self`, 再应用 `next`.
    pub fn then(self, next: Self) -> Self {
        Self {
            m11: next.m11 * self.m11 + next.m21 * self.m12,
            m12: next.m12 * self.m11 + next.m22 * self.m12,
            m21: next.m11 * self.m21 + next.m21 * self.m22,
            m22: next.m12 * self.m21 + next.m22 * self.m22,
            ox: next.m11 * self.ox + next.m21 * self.oy + next.ox,
            oy: next.m12 * self.ox + next.m22 * self.oy + next.oy,
        }
    }

    pub fn apply(self, p: Point) -> Point {
        Point {
            x: self.m11 * p.x + self.m21 * p.y + self.ox,
            y: self.m12 * p.x + self.m22 * p.y + self.oy,
        }
    }

    pub fn transform_rect(self, r: Rect) -> Rect {
        let pts = [
            self.apply(Point::new(r.x, r.y)),
            self.apply(Point::new(r.right(), r.y)),
            self.apply(Point::new(r.x, r.bottom())),
            self.apply(Point::new(r.right(), r.bottom())),
        ];
        bounds_of(pts.into_iter()).unwrap_or(r)
    }

    pub fn inverse(self) -> Option<Self> {
        let det = self.m11 * self.m22 - self.m12 * self.m21;
        if det.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        let m11 = self.m22 * inv;
        let m21 = -self.m21 * inv;
        let m12 = -self.m12 * inv;
        let m22 = self.m11 * inv;
        Some(Self {
            m11,
            m12,
            m21,
            m22,
            ox: -(m11 * self.ox + m21 * self.oy),
            oy: -(m12 * self.ox + m22 * self.oy),
        })
    }

    /// 均匀缩放的近似, 用来把线宽换到变换之后的用户空间.
    pub fn linear_scale(self) -> f32 {
        let sx = (self.m11 * self.m11 + self.m12 * self.m12).sqrt();
        let sy = (self.m21 * self.m21 + self.m22 * self.m22).sqrt();
        if sx <= 1e-8 && sy <= 1e-8 {
            1.0
        } else if sx <= 1e-8 {
            sy
        } else if sy <= 1e-8 {
            sx
        } else {
            (sx * sy).sqrt()
        }
    }

    /// `RenderTransformOrigin` 是元素包围盒上的比例点.
    pub fn around_origin(self, bounds: Option<Rect>, fx: f32, fy: f32) -> Self {
        if self.is_identity() {
            return self;
        }
        let Some(b) = bounds else {
            return self;
        };
        let ox = b.x + fx * b.w;
        let oy = b.y + fy * b.h;
        Self::translate(-ox, -oy).then(self).then(Self::translate(ox, oy))
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Self::identity()
    }
}

pub fn bounds_of(pts: impl IntoIterator<Item = Point>) -> Option<Rect> {
    let mut iter = pts.into_iter();
    let first = iter.next()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for p in iter {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Some(Rect::new(min_x, min_y, (max_x - min_x).max(0.0), (max_y - min_y).max(0.0)))
}

pub fn union_rect(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// `(ix, iy)` 到 XPS 平面的仿射. `iy` 向上 (0 在图像底边).
#[derive(Clone, Copy, Debug)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Affine {
    pub fn then_matrix(self, m: Matrix) -> Self {
        Self {
            a: m.m11 * self.a + m.m21 * self.b,
            b: m.m12 * self.a + m.m22 * self.b,
            c: m.m11 * self.c + m.m21 * self.d,
            d: m.m12 * self.c + m.m22 * self.d,
            e: m.m11 * self.e + m.m21 * self.f + m.ox,
            f: m.m12 * self.e + m.m22 * self.f + m.oy,
        }
    }

    pub fn flip_x(self) -> Self {
        Self { a: -self.a, c: self.c, e: self.e + self.a, b: -self.b, d: self.d, f: self.f + self.b }
    }

    pub fn flip_y(self) -> Self {
        Self { a: self.a, c: -self.c, e: self.e + self.c, b: self.b, d: -self.d, f: self.f + self.d }
    }
}

pub fn parse_floats(input: &str) -> Vec<f32> {
    let b = input.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && is_sep(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        if b[i] == b'+' || b[i] == b'-' {
            i += 1;
        }
        let mut saw = false;
        while i < b.len() && b[i].is_ascii_digit() {
            saw = true;
            i += 1;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                saw = true;
                i += 1;
            }
        }
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            let e = i;
            i += 1;
            if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                i += 1;
            }
            let exp = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == exp {
                i = e;
            }
        }
        if !saw {
            i = start + 1;
            continue;
        }
        if let Ok(text) = std::str::from_utf8(&b[start..i]) {
            if let Ok(v) = text.parse::<f32>() {
                out.push(v);
            }
        }
    }
    out
}

fn is_sep(c: u8) -> bool {
    c.is_ascii_whitespace() || c == b','
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    EvenOdd,
    NonZero,
}

#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn mul_alpha(self, a: f32) -> Self {
        Self { a: (self.a * a).clamp(0.0, 1.0), ..self }
    }

    pub fn to_bytes(self) -> [u8; 4] {
        [
            (self.r.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.g.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        ]
    }
}

pub fn parse_color(input: &str) -> Option<Color> {
    let s = input.trim();
    if s.eq_ignore_ascii_case("transparent") {
        return Some(Color::rgba(0.0, 0.0, 0.0, 0.0));
    }
    if let Some(rest) = s.strip_prefix("sc#").or_else(|| s.strip_prefix("sc#".to_ascii_uppercase().as_str())) {
        let nums = parse_floats(rest);
        if nums.len() >= 4 {
            return Some(Color::rgba(
                scrgb(nums[1]),
                scrgb(nums[2]),
                scrgb(nums[3]),
                nums[0].clamp(0.0, 1.0),
            ));
        }
        if nums.len() == 3 {
            return Some(Color::rgba(scrgb(nums[0]), scrgb(nums[1]), scrgb(nums[2]), 1.0));
        }
        return None;
    }
    let hex = s.strip_prefix('#')?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        6 => Some(Color::rgba(
            ((v >> 16) & 0xff) as f32 / 255.0,
            ((v >> 8) & 0xff) as f32 / 255.0,
            (v & 0xff) as f32 / 255.0,
            1.0,
        )),
        8 => Some(Color::rgba(
            ((v >> 16) & 0xff) as f32 / 255.0,
            ((v >> 8) & 0xff) as f32 / 255.0,
            (v & 0xff) as f32 / 255.0,
            ((v >> 24) & 0xff) as f32 / 255.0,
        )),
        _ => None,
    }
}

fn scrgb(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

pub fn static_resource_key(input: &str) -> Option<&str> {
    let s = input.trim();
    let rest = s.strip_prefix("{StaticResource")?;
    let rest = rest.trim().strip_suffix('}')?;
    let key = rest.trim();
    if key.is_empty() { None } else { Some(key) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_then_rotate_90() {
        let p = Matrix::translate(10.0, 0.0).apply(Point::new(1.0, 0.0));
        assert!((p.x - 11.0).abs() < 1e-4);
        let q = Matrix::rotate_deg(90.0).apply(Point::new(1.0, 0.0));
        assert!(q.x.abs() < 1e-4 && (q.y - 1.0).abs() < 1e-4);
    }

    #[test]
    fn color_argb() {
        let c = parse_color("#80FF0000").unwrap();
        assert!((c.a - 128.0 / 255.0).abs() < 1e-4);
        assert!((c.r - 1.0).abs() < 1e-4);
        assert!(c.g.abs() < 1e-4);
    }

    #[test]
    fn inverse_translate() {
        let m = Matrix::translate(10.0, -3.0);
        let p = m.inverse().unwrap().apply(m.apply(Point::new(2.0, 4.0)));
        assert!((p.x - 2.0).abs() < 1e-4 && (p.y - 4.0).abs() < 1e-4);
    }
}
