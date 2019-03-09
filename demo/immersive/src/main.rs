// pathfinder/demo/immersive/main.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A demo immersive app for Pathfinder.

#![allow(unused_imports)]
#![allow(dead_code)]

mod display;
mod immersive;

#[cfg(feature = "glwindow")]
mod glwindow;

use display::Display;
use immersive::ImmersiveDemo;

use std::process::exit;

fn run<D: Display>(display: D) -> Result<(), D::Error> {
    let mut demo = ImmersiveDemo::<D>::new(display)?;
    while demo.running() {
        demo.render_scene()?;
    }
    Ok(())
}

#[cfg(feature = "glwindow")]
pub fn main() {
    let result = glwindow::GlWindowDisplay::new().and_then(run);
    if let Err(err) = result {
        eprintln!("Error {}", err);
        exit(1);
    }
}

#[cfg(not(feature = "glwindow"))]
pub fn main() {
    unimplemented!()
}
