//! XPS 路径迷你语言, 与 WPF `StreamGeometry` 相同.

use crate::geom::{bounds_of, parse_floats, FillRule, Matrix, Point, Rect};

#[derive(Clone, Debug)]
pub struct PathGeom {
    pub fill: FillRule,
    pub fill_explicit: bool,
    pub cmds: Vec<PathCmd>,
}

#[derive(Clone, Copy, Debug)]
pub enum PathCmd {
    Move(Point),
    Line(Point),
    Cubic(Point, Point, Point),
    Close,
}

impl PathGeom {
    pub fn empty() -> Self {
        Self { fill: FillRule::EvenOdd, fill_explicit: false, cmds: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        !self.cmds.iter().any(|c| matches!(c, PathCmd::Line(_) | PathCmd::Cubic(..)))
    }

    pub fn bounds(&self) -> Option<Rect> {
        let mut pts = Vec::new();
        for cmd in &self.cmds {
            match *cmd {
                PathCmd::Move(p) | PathCmd::Line(p) => pts.push(p),
                PathCmd::Cubic(a, b, c) => {
                    pts.push(a);
                    pts.push(b);
                    pts.push(c);
                }
                PathCmd::Close => {}
            }
        }
        bounds_of(pts)
    }

    pub fn transformed(&self, m: Matrix) -> Self {
        Self {
            fill: self.fill,
            fill_explicit: self.fill_explicit,
            cmds: self.cmds.iter().map(|c| c.map(m)).collect(),
        }
    }
}

impl PathCmd {
    fn map(self, m: Matrix) -> Self {
        match self {
            PathCmd::Move(p) => PathCmd::Move(m.apply(p)),
            PathCmd::Line(p) => PathCmd::Line(m.apply(p)),
            PathCmd::Cubic(a, b, c) => PathCmd::Cubic(m.apply(a), m.apply(b), m.apply(c)),
            PathCmd::Close => PathCmd::Close,
        }
    }
}

pub fn parse_path(input: &str) -> PathGeom {
    let b = input.as_bytes();
    let mut i = 0;
    let mut geom = PathGeom::empty();
    let mut cmd = 0u8;
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut prev_cubic: Option<Point> = None;

    skip(b, &mut i);
    if i < b.len() && (b[i] == b'F' || b[i] == b'f') {
        i += 1;
        skip(b, &mut i);
        geom.fill_explicit = true;
        if i < b.len() && b[i] == b'1' {
            geom.fill = FillRule::NonZero;
            i += 1;
        } else if i < b.len() && b[i] == b'0' {
            geom.fill = FillRule::EvenOdd;
            i += 1;
        }
    }

    while i < b.len() {
        skip(b, &mut i);
        if i >= b.len() {
            break;
        }
        if b[i].is_ascii_alphabetic() {
            cmd = b[i];
            i += 1;
        } else if cmd == b'M' {
            cmd = b'L';
        } else if cmd == b'm' {
            cmd = b'l';
        } else if cmd == 0 || cmd == b'Z' || cmd == b'z' {
            break;
        }
        match cmd {
            b'M' | b'm' => {
                let Some((x, y)) = pair(b, &mut i) else { break };
                let (x, y) = abs(cmd, cx, cy, x, y);
                geom.cmds.push(PathCmd::Move(Point::new(x, y)));
                cx = x;
                cy = y;
                sx = x;
                sy = y;
                prev_cubic = None;
            }
            b'L' | b'l' => {
                let Some((x, y)) = pair(b, &mut i) else { break };
                let (x, y) = abs(cmd, cx, cy, x, y);
                geom.cmds.push(PathCmd::Line(Point::new(x, y)));
                cx = x;
                cy = y;
                prev_cubic = None;
            }
            b'H' | b'h' => {
                let Some(x) = number(b, &mut i) else { break };
                let x = if cmd == b'h' { cx + x } else { x };
                geom.cmds.push(PathCmd::Line(Point::new(x, cy)));
                cx = x;
                prev_cubic = None;
            }
            b'V' | b'v' => {
                let Some(y) = number(b, &mut i) else { break };
                let y = if cmd == b'v' { cy + y } else { y };
                geom.cmds.push(PathCmd::Line(Point::new(cx, y)));
                cy = y;
                prev_cubic = None;
            }
            b'C' | b'c' => {
                let Some((x1, y1)) = pair(b, &mut i) else { break };
                let Some((x2, y2)) = pair(b, &mut i) else { break };
                let Some((x, y)) = pair(b, &mut i) else { break };
                let (x1, y1) = abs(cmd, cx, cy, x1, y1);
                let (x2, y2) = abs(cmd, cx, cy, x2, y2);
                let (x, y) = abs(cmd, cx, cy, x, y);
                geom.cmds.push(PathCmd::Cubic(Point::new(x1, y1), Point::new(x2, y2), Point::new(x, y)));
                prev_cubic = Some(Point::new(x2, y2));
                cx = x;
                cy = y;
            }
            b'S' | b's' => {
                let Some((x2, y2)) = pair(b, &mut i) else { break };
                let Some((x, y)) = pair(b, &mut i) else { break };
                let (x2, y2) = abs(cmd, cx, cy, x2, y2);
                let (x, y) = abs(cmd, cx, cy, x, y);
                let p1 = reflect(cx, cy, prev_cubic);
                geom.cmds.push(PathCmd::Cubic(p1, Point::new(x2, y2), Point::new(x, y)));
                prev_cubic = Some(Point::new(x2, y2));
                cx = x;
                cy = y;
            }
            b'Q' | b'q' => {
                let Some((x1, y1)) = pair(b, &mut i) else { break };
                let Some((x, y)) = pair(b, &mut i) else { break };
                let (x1, y1) = abs(cmd, cx, cy, x1, y1);
                let (x, y) = abs(cmd, cx, cy, x, y);
                let c1 = Point::new(cx + (2.0 / 3.0) * (x1 - cx), cy + (2.0 / 3.0) * (y1 - cy));
                let c2 = Point::new(x + (2.0 / 3.0) * (x1 - x), y + (2.0 / 3.0) * (y1 - y));
                geom.cmds.push(PathCmd::Cubic(c1, c2, Point::new(x, y)));
                prev_cubic = Some(Point::new(x1, y1));
                cx = x;
                cy = y;
            }
            b'A' | b'a' => {
                let Some((rx, ry)) = pair(b, &mut i) else { break };
                let Some(rot) = number(b, &mut i) else { break };
                let Some(large) = number(b, &mut i) else { break };
                let Some(sweep) = number(b, &mut i) else { break };
                let Some((x, y)) = pair(b, &mut i) else { break };
                let (x, y) = abs(cmd, cx, cy, x, y);
                arc_to(
                    Point::new(cx, cy),
                    rx,
                    ry,
                    rot,
                    large != 0.0,
                    sweep != 0.0,
                    Point::new(x, y),
                    &mut geom.cmds,
                );
                cx = x;
                cy = y;
                prev_cubic = None;
            }
            b'Z' | b'z' => {
                geom.cmds.push(PathCmd::Close);
                cx = sx;
                cy = sy;
                prev_cubic = None;
            }
            _ => break,
        }
    }
    geom
}

fn abs(cmd: u8, cx: f32, cy: f32, x: f32, y: f32) -> (f32, f32) {
    if cmd.is_ascii_lowercase() { (cx + x, cy + y) } else { (x, y) }
}

fn reflect(cx: f32, cy: f32, prev: Option<Point>) -> Point {
    match prev {
        Some(p) => Point::new(2.0 * cx - p.x, 2.0 * cy - p.y),
        None => Point::new(cx, cy),
    }
}

fn skip(b: &[u8], i: &mut usize) {
    while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
        *i += 1;
    }
}

fn pair(b: &[u8], i: &mut usize) -> Option<(f32, f32)> {
    Some((number(b, i)?, number(b, i)?))
}

fn number(b: &[u8], i: &mut usize) -> Option<f32> {
    skip(b, i);
    let start = *i;
    if *i < b.len() && (b[*i] == b'+' || b[*i] == b'-') {
        *i += 1;
    }
    let mut saw = false;
    while *i < b.len() && b[*i].is_ascii_digit() {
        saw = true;
        *i += 1;
    }
    if *i < b.len() && b[*i] == b'.' {
        *i += 1;
        while *i < b.len() && b[*i].is_ascii_digit() {
            saw = true;
            *i += 1;
        }
    }
    if *i < b.len() && (b[*i] == b'e' || b[*i] == b'E') {
        let mark = *i;
        *i += 1;
        if *i < b.len() && (b[*i] == b'+' || b[*i] == b'-') {
            *i += 1;
        }
        let exp = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == exp {
            *i = mark;
        }
    }
    if !saw {
        *i = start;
        return None;
    }
    std::str::from_utf8(&b[start..*i]).ok()?.parse().ok()
}

pub fn rect_path(x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) -> PathGeom {
    let mut g = PathGeom::empty();
    if w <= 0.0 || h <= 0.0 {
        return g;
    }
    let rx = rx.min(w / 2.0).max(0.0);
    let ry = ry.min(h / 2.0).max(0.0);
    if rx <= 0.0 || ry <= 0.0 {
        g.cmds.push(PathCmd::Move(Point::new(x, y)));
        g.cmds.push(PathCmd::Line(Point::new(x + w, y)));
        g.cmds.push(PathCmd::Line(Point::new(x + w, y + h)));
        g.cmds.push(PathCmd::Line(Point::new(x, y + h)));
        g.cmds.push(PathCmd::Close);
        return g;
    }
    let k = 0.55228474983;
    let kx = rx * k;
    let ky = ry * k;
    g.cmds.push(PathCmd::Move(Point::new(x + rx, y)));
    g.cmds.push(PathCmd::Line(Point::new(x + w - rx, y)));
    g.cmds.push(PathCmd::Cubic(
        Point::new(x + w - rx + kx, y),
        Point::new(x + w, y + ry - ky),
        Point::new(x + w, y + ry),
    ));
    g.cmds.push(PathCmd::Line(Point::new(x + w, y + h - ry)));
    g.cmds.push(PathCmd::Cubic(
        Point::new(x + w, y + h - ry + ky),
        Point::new(x + w - rx + kx, y + h),
        Point::new(x + w - rx, y + h),
    ));
    g.cmds.push(PathCmd::Line(Point::new(x + rx, y + h)));
    g.cmds.push(PathCmd::Cubic(
        Point::new(x + rx - kx, y + h),
        Point::new(x, y + h - ry + ky),
        Point::new(x, y + h - ry),
    ));
    g.cmds.push(PathCmd::Line(Point::new(x, y + ry)));
    g.cmds.push(PathCmd::Cubic(
        Point::new(x, y + ry - ky),
        Point::new(x + rx - kx, y),
        Point::new(x + rx, y),
    ));
    g.cmds.push(PathCmd::Close);
    g
}

pub fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> PathGeom {
    let mut g = PathGeom::empty();
    if rx <= 0.0 || ry <= 0.0 {
        return g;
    }
    let k = 0.55228474983;
    let kx = rx * k;
    let ky = ry * k;
    g.cmds.push(PathCmd::Move(Point::new(cx + rx, cy)));
    g.cmds.push(PathCmd::Cubic(
        Point::new(cx + rx, cy + ky),
        Point::new(cx + kx, cy + ry),
        Point::new(cx, cy + ry),
    ));
    g.cmds.push(PathCmd::Cubic(
        Point::new(cx - kx, cy + ry),
        Point::new(cx - rx, cy + ky),
        Point::new(cx - rx, cy),
    ));
    g.cmds.push(PathCmd::Cubic(
        Point::new(cx - rx, cy - ky),
        Point::new(cx - kx, cy - ry),
        Point::new(cx, cy - ry),
    ));
    g.cmds.push(PathCmd::Cubic(
        Point::new(cx + kx, cy - ry),
        Point::new(cx + rx, cy - ky),
        Point::new(cx + rx, cy),
    ));
    g.cmds.push(PathCmd::Close);
    g
}

pub fn line_path(x0: f32, y0: f32, x1: f32, y1: f32) -> PathGeom {
    let mut g = PathGeom::empty();
    g.cmds.push(PathCmd::Move(Point::new(x0, y0)));
    g.cmds.push(PathCmd::Line(Point::new(x1, y1)));
    g
}

fn arc_to(
    from: Point,
    mut rx: f32,
    mut ry: f32,
    rot_deg: f32,
    large: bool,
    sweep: bool,
    to: Point,
    out: &mut Vec<PathCmd>,
) {
    if (to.x - from.x).abs() < 1e-6 && (to.y - from.y).abs() < 1e-6 {
        return;
    }
    rx = rx.abs();
    ry = ry.abs();
    if rx < 1e-6 || ry < 1e-6 {
        out.push(PathCmd::Line(to));
        return;
    }
    let phi = rot_deg.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();
    let dx = (from.x - to.x) / 2.0;
    let dy = (from.y - to.y) / 2.0;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;
    let mut rx2 = rx * rx;
    let mut ry2 = ry * ry;
    let x1p2 = x1p * x1p;
    let y1p2 = y1p * y1p;
    let lambda = x1p2 / rx2 + y1p2 / ry2;
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
        rx2 = rx * rx;
        ry2 = ry * ry;
    }
    let num = (rx2 * ry2 - rx2 * y1p2 - ry2 * x1p2).max(0.0);
    let den = rx2 * y1p2 + ry2 * x1p2;
    let mut coef = if den.abs() < 1e-12 { 0.0 } else { (num / den).sqrt() };
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = coef * -ry * x1p / rx;
    let cx = cos_phi * cxp - sin_phi * cyp + (from.x + to.x) / 2.0;
    let cy = sin_phi * cxp + cos_phi * cyp + (from.y + to.y) / 2.0;
    let theta1 = vector_angle((1.0, 0.0), ((x1p - cxp) / rx, (y1p - cyp) / ry));
    let mut dtheta = vector_angle(((x1p - cxp) / rx, (y1p - cyp) / ry), ((-x1p - cxp) / rx, (-y1p - cyp) / ry));
    if !sweep && dtheta > 0.0 {
        dtheta -= std::f32::consts::TAU;
    } else if sweep && dtheta < 0.0 {
        dtheta += std::f32::consts::TAU;
    }
    let n = ((dtheta.abs() / std::f32::consts::FRAC_PI_2).ceil() as i32).clamp(1, 8);
    let delta = dtheta / n as f32;
    let t = (4.0 / 3.0) * (delta / 4.0).tan();
    let mut a0 = theta1;
    for _ in 0..n {
        let a1 = a0 + delta;
        let (s0, c0) = a0.sin_cos();
        let (s1, c1) = a1.sin_cos();
        let p1 = ellipse_point(cx, cy, rx, ry, cos_phi, sin_phi, c0 - t * s0, s0 + t * c0);
        let p2 = ellipse_point(cx, cy, rx, ry, cos_phi, sin_phi, c1 + t * s1, s1 - t * c1);
        let p = ellipse_point(cx, cy, rx, ry, cos_phi, sin_phi, c1, s1);
        out.push(PathCmd::Cubic(p1, p2, p));
        a0 = a1;
    }
}

fn vector_angle(u: (f32, f32), v: (f32, f32)) -> f32 {
    let sign = if u.0 * v.1 - u.1 * v.0 < 0.0 { -1.0 } else { 1.0 };
    let dot = u.0 * v.0 + u.1 * v.1;
    let lu = (u.0 * u.0 + u.1 * u.1).sqrt();
    let lv = (v.0 * v.0 + v.1 * v.1).sqrt();
    if lu < 1e-8 || lv < 1e-8 {
        return 0.0;
    }
    sign * (dot / (lu * lv)).clamp(-1.0, 1.0).acos()
}

fn ellipse_point(cx: f32, cy: f32, rx: f32, ry: f32, cos_phi: f32, sin_phi: f32, x: f32, y: f32) -> Point {
    Point::new(
        cx + rx * cos_phi * x - ry * sin_phi * y,
        cy + rx * sin_phi * x + ry * cos_phi * y,
    )
}

pub fn point_list(input: &str) -> Vec<Point> {
    let n = parse_floats(input);
    n.chunks(2).map(|c| Point::new(c[0], *c.get(1).unwrap_or(&0.0))).filter(|_| true).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_commands() {
        let g = parse_path("F1 M 10,10 L 80,10 L 80,80 L 10,80 Z");
        assert_eq!(g.fill, FillRule::NonZero);
        assert!(matches!(g.cmds[0], PathCmd::Move(_)));
        assert!(matches!(g.cmds.last(), Some(PathCmd::Close)));
        let b = g.bounds().unwrap();
        assert!((b.w - 70.0).abs() < 1e-3);
    }

    #[test]
    fn relative_and_implicit_line() {
        let g = parse_path("M0,0 10,0 10,10 Z");
        assert_eq!(g.cmds.len(), 4);
    }
}
