/// A 2D point with f32 coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
    pub const fn new(x: f32, y: f32) -> Self { Self { x, y } }
}

/// A 2D size (width × height) in f32 units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self { width: 0.0, height: 0.0 };
    pub const fn new(width: f32, height: f32) -> Self { Self { width, height } }
    pub fn area(self) -> f32 { self.width * self.height }
}

/// Axis-aligned rectangle defined by origin and size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { origin: Point { x, y }, size: Size { width, height } }
    }
    pub fn x(self) -> f32 { self.origin.x }
    pub fn y(self) -> f32 { self.origin.y }
    pub fn width(self) -> f32 { self.size.width }
    pub fn height(self) -> f32 { self.size.height }
    pub fn right(self) -> f32 { self.origin.x + self.size.width }
    pub fn bottom(self) -> f32 { self.origin.y + self.size.height }
    pub fn center(self) -> Point { Point::new(self.origin.x + self.size.width / 2.0, self.origin.y + self.size.height / 2.0) }
    pub fn contains(self, p: Point) -> bool { p.x >= self.x() && p.x <= self.right() && p.y >= self.y() && p.y <= self.bottom() }
    pub fn split_horizontal(self, n: usize) -> Vec<Rect> {
        if n == 0 { return vec![]; }
        let tile_w = self.size.width / n as f32;
        (0..n).map(|i| Rect::new(self.origin.x + i as f32 * tile_w, self.origin.y, tile_w, self.size.height)).collect()
    }
    pub fn split_vertical(self, n: usize) -> Vec<Rect> {
        if n == 0 { return vec![]; }
        let tile_h = self.size.height / n as f32;
        (0..n).map(|i| Rect::new(self.origin.x, self.origin.y + i as f32 * tile_h, self.size.width, tile_h)).collect()
    }
    pub fn grid(self, cols: usize, rows: usize) -> Vec<Rect> {
        let tw = self.size.width / cols as f32;
        let th = self.size.height / rows as f32;
        (0..rows).flat_map(|r| (0..cols).map(move |c| Rect::new(self.origin.x + c as f32 * tw, self.origin.y + r as f32 * th, tw, th))).collect()
    }
    pub fn to_ndc(self, viewport: Size) -> [f32; 4] {
        let x0 = (self.x() / viewport.width) * 2.0 - 1.0;
        let y0 = 1.0 - (self.y() / viewport.height) * 2.0;
        let x1 = (self.right() / viewport.width) * 2.0 - 1.0;
        let y1 = 1.0 - (self.bottom() / viewport.height) * 2.0;
        [x0, y0, x1, y1]
    }
}
