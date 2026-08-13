pub mod adapter;
pub mod core;
pub mod error;
pub mod panic_safety;
pub mod scan;
pub mod vfs;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn main() {
    panic_safety::init_panic_hook();
}
