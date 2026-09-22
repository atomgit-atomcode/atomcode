//! Where each module goes — now a view onto [`crate::el`].
//!
//! There used to be two layout trees with two engines: `Region` divided rects
//! between modules, `El` divided a width between spans. They were the same
//! algorithm on two axes, and I wrote the second one having already noted in
//! `el.rs` that a screen with two layout engines has one too many.
//!
//! They are one type now. This module stays as the name the rest of the crate
//! already uses, so merging them cost no import churn — and so that "the
//! persisted layout" keeps a word of its own, because that part really is
//! different: it is state that ops patch and undo, not a value derived per
//! frame.

pub use crate::el::{Constraint, Dir, El as Region};
