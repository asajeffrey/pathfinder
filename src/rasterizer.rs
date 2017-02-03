// Copyright 2017 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use atlas::Atlas;
use batch::Batch;
use compute_shader::device::Device;
use compute_shader::image::Image;
use compute_shader::instance::{Instance, ShadingLanguage};
use compute_shader::profile_event::ProfileEvent;
use compute_shader::program::Program;
use compute_shader::queue::{Queue, Uniform};
use coverage::CoverageBuffer;
use euclid::rect::Rect;
use gl::types::{GLchar, GLenum, GLint, GLsizei, GLuint, GLvoid};
use gl;
use glyph_buffer::{GlyphBuffers, Vertex};
use std::ascii::AsciiExt;
use std::env;
use std::mem;
use std::ptr;

// TODO(pcwalton): Don't force that these be compiled in.
static ACCUM_CL_SHADER: &'static str = include_str!("../resources/shaders/accum.cl");
static ACCUM_COMPUTE_SHADER: &'static str = include_str!("../resources/shaders/accum.cs.glsl");

static DRAW_VERTEX_SHADER: &'static str = include_str!("../resources/shaders/draw.vs.glsl");
static DRAW_TESS_CONTROL_SHADER: &'static str = include_str!("../resources/shaders/draw.tcs.glsl");
static DRAW_TESS_EVALUATION_SHADER: &'static str =
    include_str!("../resources/shaders/draw.tes.glsl");
static DRAW_GEOMETRY_SHADER: &'static str = include_str!("../resources/shaders/draw.gs.glsl");
static DRAW_FRAGMENT_SHADER: &'static str = include_str!("../resources/shaders/draw.fs.glsl");

pub struct Rasterizer {
    pub device: Device,
    pub queue: Queue,
    draw_program: GLuint,
    accum_program: Program,
    draw_vertex_array: GLuint,
    draw_position_attribute: GLint,
    draw_glyph_index_attribute: GLint,
    draw_atlas_size_uniform: GLint,
    draw_glyph_descriptors_uniform: GLuint,
    draw_image_descriptors_uniform: GLuint,
    draw_query: GLuint,
    options: RasterizerOptions,
}

pub struct DrawAtlasProfilingEvents {
    pub draw: GLuint,
    pub accum: ProfileEvent,
}

impl Rasterizer {
    pub fn new(instance: &Instance, device: Device, queue: Queue, options: RasterizerOptions)
               -> Result<Rasterizer, ()> {
        let (draw_program, draw_position_attribute, draw_glyph_index_attribute);
        let (draw_glyph_descriptors_uniform, draw_image_descriptors_uniform);
        let draw_atlas_size_uniform;
        let (mut draw_vertex_array, mut draw_query) = (0, 0);
        unsafe {
            draw_program = gl::CreateProgram();

            let vertex_shader = try!(compile_gl_shader(gl::VERTEX_SHADER,
                                                       "Vertex shader",
                                                       DRAW_VERTEX_SHADER));
            gl::AttachShader(draw_program, vertex_shader);
            let fragment_shader = try!(compile_gl_shader(gl::FRAGMENT_SHADER,
                                                         "Fragment shader",
                                                         DRAW_FRAGMENT_SHADER));
            gl::AttachShader(draw_program, fragment_shader);

            if options.force_geometry_shader {
                let geometry_shader = try!(compile_gl_shader(gl::GEOMETRY_SHADER,
                                                             "Geometry shader",
                                                             DRAW_GEOMETRY_SHADER));
                gl::AttachShader(draw_program, geometry_shader);
            } else {
                let tess_control_shader = try!(compile_gl_shader(gl::TESS_CONTROL_SHADER,
                                                                 "Tessellation control shader",
                                                                 DRAW_TESS_CONTROL_SHADER));
                gl::AttachShader(draw_program, tess_control_shader);
                let tess_evaluation_shader =
                    try!(compile_gl_shader(gl::TESS_EVALUATION_SHADER,
                                           "Tessellation evaluation shader",
                                           DRAW_TESS_EVALUATION_SHADER));
                gl::AttachShader(draw_program, tess_evaluation_shader);
            }

            gl::LinkProgram(draw_program);

            try!(check_gl_object_status(draw_program,
                                        gl::LINK_STATUS,
                                        "Program",
                                        gl::GetProgramiv,
                                        gl::GetProgramInfoLog));

            gl::GenVertexArrays(1, &mut draw_vertex_array);

            draw_position_attribute =
                gl::GetAttribLocation(draw_program, b"aPosition\0".as_ptr() as *const GLchar);
            draw_glyph_index_attribute =
                gl::GetAttribLocation(draw_program, b"aGlyphIndex\0".as_ptr() as *const GLchar);

            draw_atlas_size_uniform =
                gl::GetUniformLocation(draw_program, b"uAtlasSize\0".as_ptr() as *const GLchar);
            draw_glyph_descriptors_uniform =
                gl::GetUniformBlockIndex(draw_program,
                                         b"ubGlyphDescriptors\0".as_ptr() as *const GLchar);
            draw_image_descriptors_uniform =
                gl::GetUniformBlockIndex(draw_program,
                                         b"ubImageDescriptors\0".as_ptr() as *const GLchar);

            gl::GenQueries(1, &mut draw_query)
        }

        // FIXME(pcwalton): Don't panic if this fails to compile; just return an error.
        let accum_source = match instance.shading_language() {
            ShadingLanguage::Cl => ACCUM_CL_SHADER,
            ShadingLanguage::Glsl => ACCUM_COMPUTE_SHADER,
        };
        let accum_program = device.create_program(accum_source).unwrap();

        Ok(Rasterizer {
            device: device,
            queue: queue,
            draw_program: draw_program,
            accum_program: accum_program,
            draw_vertex_array: draw_vertex_array,
            draw_position_attribute: draw_position_attribute,
            draw_glyph_index_attribute: draw_glyph_index_attribute,
            draw_atlas_size_uniform: draw_atlas_size_uniform,
            draw_glyph_descriptors_uniform: draw_glyph_descriptors_uniform,
            draw_image_descriptors_uniform: draw_image_descriptors_uniform,
            draw_query: draw_query,
            options: options,
        })
    }

    pub fn draw_atlas(&self,
                      atlas_rect: &Rect<u32>,
                      atlas: &Atlas,
                      glyph_buffers: &GlyphBuffers,
                      batch: &Batch,
                      coverage_buffer: &CoverageBuffer,
                      image: &Image)
                      -> Result<DrawAtlasProfilingEvents, ()> {
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, coverage_buffer.framebuffer());
            gl::Viewport(0, 0, atlas_rect.size.width as GLint, atlas_rect.size.height as GLint);

            // TODO(pcwalton): Scissor to the atlas rect to clear faster?
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            gl::BindVertexArray(self.draw_vertex_array);
            gl::UseProgram(self.draw_program);

            // Set up the buffer layout.
            gl::BindBuffer(gl::ARRAY_BUFFER, glyph_buffers.vertices);
            gl::VertexAttribIPointer(self.draw_position_attribute as GLuint,
                                     2,
                                     gl::SHORT,
                                     mem::size_of::<Vertex>() as GLint,
                                     0 as *const GLvoid);
            gl::VertexAttribIPointer(self.draw_glyph_index_attribute as GLuint,
                                     1,
                                     gl::UNSIGNED_SHORT,
                                     mem::size_of::<Vertex>() as GLint,
                                     mem::size_of::<(i16, i16)>() as *const GLvoid);
            gl::EnableVertexAttribArray(self.draw_position_attribute as GLuint);
            gl::EnableVertexAttribArray(self.draw_glyph_index_attribute as GLuint);

            gl::BindBuffer(gl::ELEMENT_ARRAY_BUFFER, glyph_buffers.indices);

            gl::BindBufferBase(gl::UNIFORM_BUFFER, 1, glyph_buffers.descriptors);
            gl::BindBufferBase(gl::UNIFORM_BUFFER, 2, batch.images());
            gl::UniformBlockBinding(self.draw_program, self.draw_glyph_descriptors_uniform, 1);
            gl::UniformBlockBinding(self.draw_program, self.draw_image_descriptors_uniform, 2);

            gl::Uniform2ui(self.draw_atlas_size_uniform,
                           atlas_rect.size.width,
                           atlas_rect.size.height);

            gl::PatchParameteri(gl::PATCH_VERTICES, 3);

            // Use blending on our floating point framebuffer to accumulate coverage.
            gl::Enable(gl::BLEND);
            gl::BlendEquation(gl::FUNC_ADD);
            gl::BlendFunc(gl::ONE, gl::ONE);

            // Enable backface culling. See comments in `draw.tcs.glsl` for more information
            // regarding why this is necessary.
            gl::CullFace(gl::BACK);
            gl::FrontFace(gl::CCW);
            gl::Enable(gl::CULL_FACE);

            // If we're using a geometry shader for debugging, we draw fake triangles. Otherwise,
            // we use patches.
            let primitive = if self.options.force_geometry_shader {
                gl::TRIANGLES
            } else {
                gl::PATCHES
            };
            // Now draw the glyph ranges.
            gl::BeginQuery(gl::TIME_ELAPSED, self.draw_query);
            batch.draw(primitive);
            gl::EndQuery(gl::TIME_ELAPSED);

            gl::Disable(gl::CULL_FACE);
            gl::Disable(gl::BLEND);

            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);

            // FIXME(pcwalton): We should have some better synchronization here if we're using
            // OpenCL, but I don't know how to do that portably (i.e. on Mac…) Just using
            // `glFlush()` seems to work in practice.
            gl::Flush();
        }

        let atlas_rect_uniform = [
            atlas_rect.origin.x,
            atlas_rect.origin.y,
            atlas_rect.max_x(),
            atlas_rect.max_y()
        ];

        let accum_uniforms = [
            (0, Uniform::Image(image)),
            (1, Uniform::Image(coverage_buffer.image())),
            (2, Uniform::UVec4(atlas_rect_uniform)),
            (3, Uniform::U32(atlas.shelf_height())),
        ];

        let accum_event = try!(self.queue.submit_compute(&self.accum_program,
                                                         &[atlas.shelf_columns()],
                                                         &accum_uniforms,
                                                         &[]).map_err(drop));

        Ok(DrawAtlasProfilingEvents {
            draw: self.draw_query,
            accum: accum_event,
        })
    }
}

fn compile_gl_shader(shader_type: GLuint, description: &str, source: &str) -> Result<GLuint, ()> {
    unsafe {
        let shader = gl::CreateShader(shader_type);
        gl::ShaderSource(shader, 1, &(source.as_ptr() as *const GLchar), &(source.len() as GLint));
        gl::CompileShader(shader);
        try!(check_gl_object_status(shader,
                                    gl::COMPILE_STATUS,
                                    description,
                                    gl::GetShaderiv,
                                    gl::GetShaderInfoLog));
        Ok(shader)
    }
}

fn check_gl_object_status(object: GLuint,
                          parameter: GLenum,
                          description: &str,
                          get_status: unsafe fn(GLuint, GLenum, *mut GLint),
                          get_log: unsafe fn(GLuint, GLsizei, *mut GLsizei, *mut GLchar))
                          -> Result<(), ()> {
    unsafe {
        let mut status = 0;
        get_status(object, parameter, &mut status);
        if status == gl::TRUE as i32 {
            return Ok(())
        }

        let mut info_log_length = 0;
        get_status(object, gl::INFO_LOG_LENGTH, &mut info_log_length);

        let mut info_log = vec![0; info_log_length as usize];
        get_log(object, info_log_length, ptr::null_mut(), info_log.as_mut_ptr() as *mut GLchar);
        if let Ok(string) = String::from_utf8(info_log) {
            println!("{} error:\n{}", description, string);
        }
        Err(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RasterizerOptions {
    pub force_geometry_shader: bool,
}

impl Default for RasterizerOptions {
    fn default() -> RasterizerOptions {
        RasterizerOptions {
            force_geometry_shader: false,
        }
    }
}

impl RasterizerOptions {
    pub fn from_env() -> Result<RasterizerOptions, ()> {
        let force_geometry_shader = match env::var("PATHFINDER_FORCE_GEOMETRY_SHADER") {
            Ok(ref string) if string.eq_ignore_ascii_case("on") ||
                string.eq_ignore_ascii_case("yes") ||
                string.eq_ignore_ascii_case("1") => true,
            Ok(ref string) if string.eq_ignore_ascii_case("off") ||
                string.eq_ignore_ascii_case("no") ||
                string.eq_ignore_ascii_case("0") => false,
            Err(_) => false,
            Ok(_) => return Err(()),
        };

        Ok(RasterizerOptions {
            force_geometry_shader: force_geometry_shader,
        })
    }
}

