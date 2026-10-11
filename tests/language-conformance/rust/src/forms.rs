pub async fn fetch(url: &str) -> String {
    url.to_string()
}

pub const SQUARE: fn(i64) -> i64 = |value| value * value;

#[derive(Debug, Default)]
pub struct Point {
    pub x: i64,
    y: i64,
}

impl Point {
    #[inline]
    #[must_use]
    pub fn norm(&self) -> i64 {
        self.x.abs() + self.y.abs()
    }
}
