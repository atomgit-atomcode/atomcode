mod shape;

use shape::{Circle, Shape};

fn main() {
    let shapes: Vec<Box<dyn Shape>> = vec![
        Box::new(Circle { radius: 2.0 }),
        // TODO: add Rectangle { w: 3.0, h: 4.0 }
        // TODO: add Triangle { base: 6.0, height: 5.0 }
    ];
    for s in &shapes {
        println!("{}: {:.2}", s.name(), s.area());
    }
}
