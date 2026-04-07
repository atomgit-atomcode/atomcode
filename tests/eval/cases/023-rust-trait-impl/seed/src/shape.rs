pub trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> &'static str;
}

pub struct Circle {
    pub radius: f64,
}

impl Shape for Circle {
    fn area(&self) -> f64 {
        std::f64::consts::PI * self.radius * self.radius
    }
    fn name(&self) -> &'static str {
        "circle"
    }
}

// TODO: add Rectangle { w, h } and Triangle { base, height } here.
