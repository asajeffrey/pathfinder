// Copyright 2017 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A very basic text shaper for simple needs.
//!
//! Do not use this for international or high-quality text. This shaper does not do kerning,
//! ligation, or advanced typography features (`GSUB`, `GPOS`, text morphing). Consider HarfBuzz or
//! the system shaper instead.

use glyph_range::GlyphRanges;
use otf::Font;
use std::cmp;

pub fn shape_text(font: &Font, glyph_ranges: &GlyphRanges, string: &str) -> Vec<GlyphPos> {
    let mut advance = 0;
    string.chars().map(|ch| {
        let glyph_id = glyph_ranges.glyph_for(ch as u32).unwrap_or(0);
        let metrics = font.metrics_for_glyph(glyph_id);

        let pos = GlyphPos {
            glyph_id: glyph_id,
            advance: cmp::max(0, advance) as u16,
        };

        if let Ok(ref metrics) = metrics {
            advance = metrics.advance_width as i32
        }
        pos
    }).collect()
}

#[derive(Clone, Copy, Debug)]
pub struct GlyphPos {
    pub glyph_id: u16,
    pub advance: u16,
}

