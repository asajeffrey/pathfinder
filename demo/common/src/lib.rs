// pathfinder/demo/common/src/lib.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A demo app for Pathfinder.

use crate::device::{GroundLineVertexArray, GroundProgram, GroundSolidVertexArray};
use crate::ui::{DemoUI, UIAction};
use crate::window::{CameraTransform, Event, Mode, Keycode, SVGPath, Window, WindowSize};
use clap::{App, Arg};
use image::ColorType;
use pathfinder_geometry::basic::point::{Point2DF32, Point2DI32, Point3DF32};
use pathfinder_geometry::basic::rect::{RectF32, RectI32};
use pathfinder_geometry::basic::transform2d::Transform2DF32;
use pathfinder_geometry::basic::transform3d::{Perspective, Transform3DF32};
use pathfinder_geometry::color::ColorU;
use pathfinder_geometry::distortion::BarrelDistortionCoefficients;
use pathfinder_gl::GLDevice;
use pathfinder_gpu::resources::ResourceLoader;
use pathfinder_gpu::{DepthFunc, DepthState, Device, Primitive, RenderState, StencilFunc};
use pathfinder_gpu::{StencilState, UniformData};
use pathfinder_renderer::builder::{RenderOptions, RenderTransform, SceneBuilder};
use pathfinder_renderer::gpu::renderer::{RenderMode, Renderer};
use pathfinder_renderer::gpu_data::{BuiltScene, Stats};
use pathfinder_renderer::post::{DEFRINGING_KERNEL_CORE_GRAPHICS, STEM_DARKENING_FACTORS};
use pathfinder_renderer::scene::Scene;
use pathfinder_renderer::z_buffer::ZBuffer;
use pathfinder_svg::BuiltSVG;
use pathfinder_ui::{MousePosition, UIEvent};
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use rayon::ThreadPoolBuilder;
use std::f32::consts::FRAC_PI_4;
use std::fs::File;
use std::io::Read;
use std::iter;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use usvg::{Options as UsvgOptions, Tree};

static DEFAULT_SVG_VIRTUAL_PATH: &'static str = "svg/Ghostscript_Tiger.svg";

const MOUSELOOK_ROTATION_SPEED: f32 = 0.007;
const CAMERA_VELOCITY: f32 = 0.02;

// How much the scene is scaled when a scale gesture is performed.
const CAMERA_SCALE_SPEED_2D: f32 = 6.0;
// How much the scene is scaled when a zoom button is clicked.
const CAMERA_ZOOM_AMOUNT_2D: f32 = 0.1;

const NEAR_CLIP_PLANE: f32 = 0.01;
const FAR_CLIP_PLANE:  f32 = 10.0;

const LIGHT_BG_COLOR:     ColorU = ColorU { r: 248, g: 248, b: 248, a: 255 };
const DARK_BG_COLOR:      ColorU = ColorU { r: 32,  g: 32,  b: 32,  a: 255 };
const GROUND_SOLID_COLOR: ColorU = ColorU { r: 80,  g: 80,  b: 80,  a: 255 };
const GROUND_LINE_COLOR:  ColorU = ColorU { r: 127, g: 127, b: 127, a: 255 };

const APPROX_FONT_SIZE: f32 = 16.0;

const MESSAGE_TIMEOUT_SECS: u64 = 5;

pub const GRIDLINE_COUNT: u8 = 10;

pub mod window;

mod device;
mod ui;

pub struct DemoApp<W> where W: Window {
    pub window: W,
    pub should_exit: bool,
    pub options: Options,

    window_size: WindowSize,

    scene_view_box: RectF32,
    monochrome_scene_color: Option<ColorU>,

    camera: Camera,
    frame_counter: u32,
    pending_screenshot_path: Option<PathBuf>,
    mouselook_enabled: bool,
    dirty: bool,
    expire_message_event_id: u32,
    message_epoch: u32,
    last_mouse_position: Point2DI32,

    current_frame: Option<Frame>,

    ui: DemoUI<GLDevice>,
    scene_thread_proxy: SceneThreadProxy,
    renderer: Renderer<GLDevice>,

    ground_program: GroundProgram<GLDevice>,
    ground_solid_vertex_array: GroundSolidVertexArray<GLDevice>,
    ground_line_vertex_array: GroundLineVertexArray<GLDevice>,
}

impl<W> DemoApp<W> where W: Window {
    pub fn new(window: W, window_size: WindowSize, mut options: Options) -> DemoApp<W> {
        let expire_message_event_id = window.create_user_event_id();

        let device = GLDevice::new(window.gl_version(), window.gl_default_framebuffer());
        let resources = window.resource_loader();

        options.command_line_overrides();

        let view_box_size = window.view_box_size(options.mode);

        // Set up Rayon.
        let mut thread_pool_builder = ThreadPoolBuilder::new();
        thread_pool_builder = options.adjust_thread_pool_settings(thread_pool_builder);
        thread_pool_builder = window.adjust_thread_pool_settings(thread_pool_builder);
        thread_pool_builder.build_global().unwrap();

        let built_svg = load_scene(resources, &options.input_path);
        let message = get_svg_building_message(&built_svg);
        let scene_view_box = built_svg.scene.view_box;
        let monochrome_scene_color = built_svg.scene.monochrome_color();

        let renderer = Renderer::new(device,
                                     resources,
                                     RectI32::new(Point2DI32::default(), view_box_size),
                                     window_size.device_size());
        let scene_thread_proxy = SceneThreadProxy::new(built_svg.scene, options.clone());
        scene_thread_proxy.set_drawable_size(view_box_size);

        let camera = Camera::new(options.mode, scene_view_box, view_box_size);

        let ground_program = GroundProgram::new(&renderer.device, resources);
        let ground_solid_vertex_array =
            GroundSolidVertexArray::new(&renderer.device,
                                        &ground_program,
                                        &renderer.quad_vertex_positions_buffer());
        let ground_line_vertex_array = GroundLineVertexArray::new(&renderer.device,
                                                                  &ground_program);

        let mut ui = DemoUI::new(&renderer.device, resources, options.clone());
        let mut message_epoch = 0;
        emit_message::<W>(&mut ui, &mut message_epoch, expire_message_event_id, message);

        DemoApp {
            window,
            should_exit: false,
            options,

            window_size,

            scene_view_box,
            monochrome_scene_color,

            camera,
            frame_counter: 0,
            pending_screenshot_path: None,
            mouselook_enabled: false,
            dirty: true,
            expire_message_event_id,
            message_epoch,
            last_mouse_position: Point2DI32::default(),

            current_frame: None,

            ui,
            scene_thread_proxy,
            renderer,

            ground_program,
            ground_solid_vertex_array,
            ground_line_vertex_array,
        }
    }

    pub fn prepare_frame(&mut self, events: Vec<Event>) -> u32 {
        // Handle events.
        let ui_events = self.handle_events(events);

        // Update the scene.
        self.build_scene();

        // Get the render message, and determine how many scenes it contains.
        let render_msg = self.scene_thread_proxy.receiver.recv().unwrap();
        let render_scene_count = render_msg.render_scenes.len() as u32;

        // Save the frame.
        self.current_frame = Some(Frame::new(render_msg, ui_events));

        // Begin drawing the scene.
        for render_scene_index in 0..render_scene_count {
            self.window.make_current(self.ui.mode, Some(render_scene_index));
            self.renderer.device.clear(Some(self.background_color().to_f32().0), Some(1.0), Some(0));
        }

        render_scene_count
    }

    fn build_scene(&mut self) {
        let render_transforms = match self.camera {
            Camera::ThreeD { ref transforms, ref mut transform, ref mut velocity, .. } => {
                if transform.offset(*velocity) {
                    self.dirty = true;
                }
                transforms.iter()
                    .map(|tr| {
                         let perspective = tr.perspective
                             .post_mul(&tr.view)
                             .post_mul(&transform.to_transform());
                         RenderTransform::Perspective(perspective)
                    }).collect()
            }
            Camera::TwoD(transform) => vec![RenderTransform::Transform2D(transform)],
        };

        let is_first_frame = self.frame_counter == 0;
        let stem_darkening_font_size = if self.ui.stem_darkening_effect_enabled {
            Some(APPROX_FONT_SIZE * self.window_size.backing_scale_factor)
        } else {
            None
        };
        let barrel_distortion = match self.ui.mode {
            Mode::VR => Some(self.window.barrel_distortion_coefficients()),
            _ => None,
        };
        let msg = MainToSceneMsg::Build(BuildOptions {
            render_transforms,
            stem_darkening_font_size,
            subpixel_aa_enabled: self.ui.subpixel_aa_effect_enabled,
            barrel_distortion,
        });

        if is_first_frame && self.options.pipeline {
            self.scene_thread_proxy.sender.send(msg.clone()).unwrap();
            self.dirty = true;
        }

        self.scene_thread_proxy.sender.send(msg).unwrap();
    }

    fn handle_events(&mut self, events: Vec<Event>) -> Vec<UIEvent> {
        let mut ui_events = vec![];
        self.dirty = false;

        for event in events {
            match event {
                Event::Quit { .. } |
                Event::KeyDown(Keycode::Escape) => {
                    self.should_exit = true;
                    self.dirty = true;
                }
                Event::WindowResized(new_size) => {
                    self.window_size = new_size;
                    let view_box_size = self.window.view_box_size(self.ui.mode);
                    self.scene_thread_proxy.set_drawable_size(view_box_size);
                    self.renderer.set_main_framebuffer_size(self.window_size.device_size());
                    self.dirty = true;
                }
                Event::MouseDown(new_position) => {
                    let mouse_position = self.process_mouse_position(new_position);
                    ui_events.push(UIEvent::MouseDown(mouse_position));
                }
                Event::MouseMoved(new_position) if self.mouselook_enabled => {
                    let mouse_position = self.process_mouse_position(new_position);
                    if let Camera::ThreeD { ref mut transform, .. } = self.camera {
                        let rotation = mouse_position.relative
                                                     .to_f32()
                                                     .scale(MOUSELOOK_ROTATION_SPEED);
                        transform.yaw += rotation.x();
                        transform.pitch += rotation.y();
                        self.dirty = true;
                    }
                }
                Event::MouseDragged(new_position) => {
                    let mouse_position = self.process_mouse_position(new_position);
                    ui_events.push(UIEvent::MouseDragged(mouse_position));
                    self.dirty = true;
                }
                Event::Zoom(d_dist) => {
                    if let Camera::TwoD(ref mut transform) = self.camera {
                        let position = get_mouse_position(&self.window,
                                                          self.window_size.backing_scale_factor);
                        *transform = transform.post_translate(-position);
                        let scale_delta = 1.0 + d_dist * CAMERA_SCALE_SPEED_2D;
                        *transform = transform.post_scale(Point2DF32::splat(scale_delta));
                        *transform = transform.post_translate(position);
                    }
                }
                Event::Look { pitch, yaw } => {
                    if let Camera::ThreeD { ref mut transform, .. } = self.camera {
                        transform.pitch += pitch;
                        transform.yaw += yaw;
                    }
                }
                Event::CameraTransforms(new_transforms) => {
                    if let Camera::ThreeD { ref mut transforms, .. } = self.camera {
                        *transforms = new_transforms;
                    }
                }
                Event::KeyDown(Keycode::Alphanumeric(b'w')) => {
                    if let Camera::ThreeD { ref mut velocity, .. } = self.camera {
                        let scale_factor = scale_factor_for_view_box(self.scene_view_box);
                        velocity.set_z(-CAMERA_VELOCITY / scale_factor);
                        self.dirty = true;
                    }
                }
                Event::KeyDown(Keycode::Alphanumeric(b's')) => {
                    if let Camera::ThreeD { ref mut velocity, .. } = self.camera {
                        let scale_factor = scale_factor_for_view_box(self.scene_view_box);
                        velocity.set_z(CAMERA_VELOCITY / scale_factor);
                        self.dirty = true;
                    }
                }
                Event::KeyDown(Keycode::Alphanumeric(b'a')) => {
                    if let Camera::ThreeD { ref mut velocity, .. } = self.camera {
                        let scale_factor = scale_factor_for_view_box(self.scene_view_box);
                        velocity.set_x(-CAMERA_VELOCITY / scale_factor);
                        self.dirty = true;
                    }
                }
                Event::KeyDown(Keycode::Alphanumeric(b'd')) => {
                    if let Camera::ThreeD { ref mut velocity, .. } = self.camera {
                        let scale_factor = scale_factor_for_view_box(self.scene_view_box);
                        velocity.set_x(CAMERA_VELOCITY / scale_factor);
                        self.dirty = true;
                    }
                }
                Event::KeyUp(Keycode::Alphanumeric(b'w')) |
                Event::KeyUp(Keycode::Alphanumeric(b's')) => {
                    if let Camera::ThreeD { ref mut velocity, .. } = self.camera {
                        velocity.set_z(0.0);
                        self.dirty = true;
                    }
                }
                Event::KeyUp(Keycode::Alphanumeric(b'a')) |
                Event::KeyUp(Keycode::Alphanumeric(b'd')) => {
                    if let Camera::ThreeD { ref mut velocity, .. } = self.camera {
                        velocity.set_x(0.0);
                        self.dirty = true;
                    }
                }
                Event::KeyDown(Keycode::Tab) => {
                    self.options.ui = match self.options.ui {
                        UIVisibility::None => UIVisibility::Stats,
                        UIVisibility::Stats => UIVisibility::All,
                        UIVisibility::All => UIVisibility::None,
                    }
                }
                Event::OpenSVG(ref svg_path) => {
                    let built_svg = load_scene(self.window.resource_loader(), svg_path);
                    self.ui.message = get_svg_building_message(&built_svg);

                    let view_box_size = self.window.view_box_size(self.ui.mode);
                    self.scene_view_box = built_svg.scene.view_box;
                    self.monochrome_scene_color = built_svg.scene.monochrome_color();
                    self.camera = Camera::new(self.ui.mode, self.scene_view_box, view_box_size);
                    self.scene_thread_proxy.load_scene(built_svg.scene, view_box_size);
                    self.dirty = true;
                }
                Event::User { message_type: event_id, message_data: expected_epoch } if
                        event_id == self.expire_message_event_id &&
                        expected_epoch as u32 == self.message_epoch => {
                    self.ui.message = String::new();
                    self.dirty = true;
                }
                _ => continue,
            }
        }

        ui_events
    }

    fn process_mouse_position(&mut self, new_position: Point2DI32) -> MousePosition {
        let absolute = new_position.scale(self.window_size.backing_scale_factor as i32);
        let relative = absolute - self.last_mouse_position;
        self.last_mouse_position = absolute;
        MousePosition { absolute, relative }
    }

    pub fn draw_scene(&mut self, render_scene_index: u32) {
        let viewport = self.window.make_current(self.ui.mode, Some(render_scene_index));
        self.renderer.set_viewport(viewport);
        self.draw_environment(render_scene_index);
        self.render_vector_scene(render_scene_index);

        let frame = self.current_frame.as_mut().unwrap();
        let render_scene = &frame.render_msg.render_scenes[render_scene_index as usize];
        match frame.render_stats {
            None => {
                frame.render_stats = Some(RenderStats {
                    rendering_time: self.renderer.shift_timer_query(),
                    stats: render_scene.built_scene.stats(),
                })
            }
            Some(ref mut render_stats) => {
                render_stats.stats = render_stats.stats + render_scene.built_scene.stats()
            }
        }
    }

    pub fn finish_drawing_frame(&mut self) {
        let mut frame = self.current_frame.take().unwrap();

        let viewport = self.window.make_current(self.ui.mode, None);
        self.renderer.set_viewport(viewport);

        if self.pending_screenshot_path.is_some() {
            self.take_screenshot();
        }

        if self.options.ui != UIVisibility::None {
            if let Some(render_stats) = frame.render_stats.take() {
                self.renderer.debug_ui.add_sample(render_stats.stats,
                                                  frame.render_msg.tile_time,
                                                  render_stats.rendering_time);
                self.renderer.draw_debug_ui();
            }

            for ui_event in &frame.ui_events {
                self.dirty = true;
                self.renderer.debug_ui.ui.event_queue.push(*ui_event);
            }

            self.renderer.debug_ui.ui.mouse_position =
                get_mouse_position(&self.window, self.window_size.backing_scale_factor);
            self.ui.show_text_effects = self.monochrome_scene_color.is_some();

            let mut ui_action = UIAction::None;
            if self.options.ui == UIVisibility::All {
                self.ui.update(&self.renderer.device,
                               &mut self.window,
                               &mut self.renderer.debug_ui,
                               &mut ui_action);
            }
            frame.ui_events = self.renderer.debug_ui.ui.event_queue.drain();
            self.handle_ui_action(&mut ui_action);
        }

        // Switch camera mode (2D/3D) if requested.
        //
        // FIXME(pcwalton): This should really be an MVC setup.
        if self.camera.mode() != self.ui.mode {
            let view_box_size = self.window.view_box_size(self.ui.mode);
            self.camera = Camera::new(self.ui.mode, self.scene_view_box, view_box_size);
        }

        for ui_event in frame.ui_events {
            match ui_event {
                UIEvent::MouseDown(_) if self.camera.is_3d() => {
                    // If nothing handled the mouse-down event, toggle mouselook.
                    self.mouselook_enabled = !self.mouselook_enabled;
                }
                UIEvent::MouseDragged(position) => {
                    if let Camera::TwoD(ref mut transform) = self.camera {
                        *transform = transform.post_translate(position.relative.to_f32());
                    }
                }
                _ => {}
            }
        }

        self.window.present();
        self.frame_counter += 1;
    }

    fn draw_environment(&self, viewport_index: u32) {
        let render_msg = &self.current_frame.as_ref().unwrap().render_msg;
        let render_transform = &render_msg.render_scenes[viewport_index as usize].transform;

        let perspective = match *render_transform {
            RenderTransform::Transform2D(..) => return,
            RenderTransform::Perspective(perspective) => perspective,
        };

        if self.ui.background == Background::None {
	    return;
	}

        let ground_scale = self.scene_view_box.max_x() * 2.0;

        let mut base_transform = perspective.transform;
        base_transform = base_transform.post_mul(&Transform3DF32::from_translation(
            -0.5 * self.scene_view_box.max_x(),
            self.scene_view_box.max_y(),
            -0.5 * ground_scale));

        // Draw gridlines. Use the stencil buffer to avoid Z-fighting.
        let mut transform = base_transform;
        let gridline_scale = ground_scale / GRIDLINE_COUNT as f32;
        transform =
            transform.post_mul(&Transform3DF32::from_scale(gridline_scale, 1.0, gridline_scale));
        let device = &self.renderer.device;
        device.bind_vertex_array(&self.ground_line_vertex_array.vertex_array);
        device.use_program(&self.ground_program.program);
        device.set_uniform(&self.ground_program.transform_uniform, UniformData::Mat4([
            transform.c0,
            transform.c1,
            transform.c2,
            transform.c3,
        ]));
        device.set_uniform(&self.ground_program.color_uniform,
                           UniformData::Vec4(GROUND_LINE_COLOR.to_f32().0));
        device.draw_arrays(Primitive::Lines, (GRIDLINE_COUNT as u32 + 1) * 4, &RenderState {
            depth: Some(DepthState { func: DepthFunc::Always, write: true }),
            stencil: Some(StencilState {
                func: StencilFunc::Always,
                reference: 2,
                mask: 2,
                write: true,
            }),
            ..RenderState::default()
        });

        // Fill ground.
        let mut transform = base_transform;
        transform =
            transform.post_mul(&Transform3DF32::from_scale(ground_scale, 1.0, ground_scale));
        device.bind_vertex_array(&self.ground_solid_vertex_array.vertex_array);
        device.use_program(&self.ground_program.program);
        device.set_uniform(&self.ground_program.transform_uniform, UniformData::Mat4([
            transform.c0,
            transform.c1,
            transform.c2,
            transform.c3,
        ]));
        device.set_uniform(&self.ground_program.color_uniform,
                           UniformData::Vec4(GROUND_SOLID_COLOR.to_f32().0));
        device.draw_arrays(Primitive::TriangleFan, 4, &RenderState {
            depth: Some(DepthState { func: DepthFunc::Less, write: true }),
            stencil: Some(StencilState {
                func: StencilFunc::NotEqual,
                reference: 2,
                mask: 2,
                write: false,
            }),
            ..RenderState::default()
        });
    }

    fn render_vector_scene(&mut self, viewport_index: u32) {
        let render_msg = &self.current_frame.as_ref().unwrap().render_msg;
        let built_scene = &render_msg.render_scenes[viewport_index as usize].built_scene;

        match self.monochrome_scene_color {
            None => self.renderer.set_render_mode(RenderMode::Multicolor),
            Some(fg_color) => {
                self.renderer.set_render_mode(RenderMode::Monochrome {
                    fg_color: fg_color.to_f32(),
                    bg_color: self.background_color().to_f32(),
                    gamma_correction: self.ui.gamma_correction_effect_enabled,
                    defringing_kernel: if self.ui.subpixel_aa_effect_enabled {
                        // TODO(pcwalton): Select FreeType defringing kernel as necessary.
                        Some(DEFRINGING_KERNEL_CORE_GRAPHICS)
                    } else {
                        None
                    }
                })
            }
        }

        if self.ui.mode == Mode::TwoD {
            self.renderer.disable_depth();
        } else {
            self.renderer.enable_depth();
        }

        self.renderer.render_scene(&built_scene);
    }

    fn handle_ui_action(&mut self, ui_action: &mut UIAction) {
        match ui_action {
            UIAction::None => {}

            UIAction::TakeScreenshot(ref path) => {
                self.pending_screenshot_path = Some((*path).clone());
                self.dirty = true;
            }

            UIAction::ZoomIn => {
                if let Camera::TwoD(ref mut transform) = self.camera {
                    let scale = Point2DF32::splat(1.0 + CAMERA_ZOOM_AMOUNT_2D);
                    let center = center_of_window(&self.window_size);
                    *transform = transform.post_translate(-center)
                                          .post_scale(scale)
                                          .post_translate(center);
                    self.dirty = true;
                }
            }
            UIAction::ZoomOut => {
                if let Camera::TwoD(ref mut transform) = self.camera {
                    let scale = Point2DF32::splat(1.0 - CAMERA_ZOOM_AMOUNT_2D);
                    let center = center_of_window(&self.window_size);
                    *transform = transform.post_translate(-center)
                                          .post_scale(scale)
                                          .post_translate(center);
                    self.dirty = true;
                }
            }
            UIAction::Rotate(theta) => {
                if let Camera::TwoD(ref mut transform) = self.camera {
                    let old_rotation = transform.rotation();
                    let center = center_of_window(&self.window_size);
                    *transform = transform.post_translate(-center)
                                          .post_rotate(*theta - old_rotation)
                                          .post_translate(center);
                }
            }
        }
    }

    fn take_screenshot(&mut self) {
        let screenshot_path = self.pending_screenshot_path.take().unwrap();
        let drawable_size = self.window_size.device_size();
        let pixels = self.renderer.device.read_pixels_from_default_framebuffer(drawable_size);
        image::save_buffer(screenshot_path,
                           &pixels,
                           drawable_size.x() as u32,
                           drawable_size.y() as u32,
                           ColorType::RGBA(8)).unwrap();
    }

    fn background_color(&self) -> ColorU {
        match self.ui.background {
            Background::None => ColorU::black(),
            Background::Dark => DARK_BG_COLOR,
            Background::Light => LIGHT_BG_COLOR,
        }
    }

}

struct SceneThreadProxy {
    sender: Sender<MainToSceneMsg>,
    receiver: Receiver<SceneToMainMsg>,
}

impl SceneThreadProxy {
    fn new(scene: Scene, options: Options) -> SceneThreadProxy {
        let (main_to_scene_sender, main_to_scene_receiver) = mpsc::channel();
        let (scene_to_main_sender, scene_to_main_receiver) = mpsc::channel();
        SceneThread::new(scene, scene_to_main_sender, main_to_scene_receiver, options);
        SceneThreadProxy { sender: main_to_scene_sender, receiver: scene_to_main_receiver }
    }

    fn load_scene(&self, scene: Scene, view_box_size: Point2DI32) {
        self.sender.send(MainToSceneMsg::LoadScene { scene, view_box_size }).unwrap();
    }

    fn set_drawable_size(&self, drawable_size: Point2DI32) {
        self.sender.send(MainToSceneMsg::SetDrawableSize(drawable_size)).unwrap();
    }
}

struct SceneThread {
    scene: Scene,
    sender: Sender<SceneToMainMsg>,
    receiver: Receiver<MainToSceneMsg>,
    options: Options,
}

impl SceneThread {
    fn new(scene: Scene,
           sender: Sender<SceneToMainMsg>,
           receiver: Receiver<MainToSceneMsg>,
           options: Options) {
        thread::spawn(move || (SceneThread { scene, sender, receiver, options }).run());
    }

    fn run(mut self) {
        while let Ok(msg) = self.receiver.recv() {
            match msg {
                MainToSceneMsg::LoadScene { scene, view_box_size } => {
                    self.scene = scene;
                    self.scene.view_box = RectF32::new(Point2DF32::default(),
                                                       view_box_size.to_f32());
                }
                MainToSceneMsg::SetDrawableSize(size) => {
                    self.scene.view_box = RectF32::new(Point2DF32::default(), size.to_f32());
                }
                MainToSceneMsg::Build(build_options) => {
                    let start_time = Instant::now();
                    let scene = &self.scene;
                    let jobs = self.options.jobs;
                    let renderer = |render_transform: &RenderTransform| {
                        let built_scene = build_scene(scene,
                                                      &build_options,
                                                      (*render_transform).clone(),
                                                      jobs);
                        RenderScene { built_scene, transform: (*render_transform).clone() }
                    };
                    let render_scenes = match jobs {
                        Some(j) if j<2 => build_options.render_transforms.iter()    .map(renderer).collect(),
                        _              => build_options.render_transforms.par_iter().map(renderer).collect(),
                    };
                    let tile_time = Instant::now() - start_time;
                    self.sender.send(SceneToMainMsg { render_scenes, tile_time }).unwrap();
                }
            }
        }
    }
}

#[derive(Clone)]
enum MainToSceneMsg {
    LoadScene { scene: Scene, view_box_size: Point2DI32 },
    SetDrawableSize(Point2DI32),
    Build(BuildOptions),
}

#[derive(Clone)]
struct BuildOptions {
    render_transforms: Vec<RenderTransform>,
    stem_darkening_font_size: Option<f32>,
    barrel_distortion: Option<BarrelDistortionCoefficients>,
    subpixel_aa_enabled: bool,
}

struct SceneToMainMsg {
    render_scenes: Vec<RenderScene>,
    tile_time: Duration,
}

pub struct RenderScene {
    built_scene: BuiltScene,
    transform: RenderTransform,
}

#[derive(Clone)]
pub struct Options {
    pub jobs: Option<usize>,
    pub mode: Mode,
    pub input_path: SVGPath,
    pub ui: UIVisibility,
    pub background: Background,
    pub pipeline: bool,
    hidden_field_for_future_proofing: (),
}

impl Default for Options {
    fn default() -> Self {
        Options {
            jobs: None,
            mode: Mode::TwoD,
            input_path: SVGPath::Default,
            ui: UIVisibility::All,
            background: Background::Light,
            pipeline: true,
            hidden_field_for_future_proofing: (),
        }
    }
}

impl Options {
    fn command_line_overrides(&mut self) {
        let matches = App::new("tile-svg")
            .arg(
                Arg::with_name("jobs")
                    .short("j")
                    .long("jobs")
                    .value_name("THREADS")
                    .takes_value(true)
                    .help("Number of threads to use"),
            )
            .arg(Arg::with_name("3d").short("3").long("3d").help("Run in 3D").conflicts_with("vr"))
            .arg(Arg::with_name("vr").short("V").long("vr").help("Run in VR").conflicts_with("3d"))
            .arg(
                Arg::with_name("ui")
                    .short("u")
                    .long("ui")
                    .takes_value(true)
                    .possible_values(&["none", "stats", "all"])
                    .help("How much UI to show"),
            )
            .arg(
                Arg::with_name("bg")
                    .short("b")
                    .long("bg")
                    .takes_value(true)
                    .possible_values(&["none", "dark", "light"])
                    .help("Background color scheme"),
            )
            .arg(Arg::with_name("pipeline").short("P").long("pipeline").help("Pipeline scenes").conflicts_with("no-pipeline"))
            .arg(Arg::with_name("no-pipeline").short("p").long("no-pipeline").help("Don't pipeline scenes").conflicts_with("pipeline"))
            .arg(Arg::with_name("INPUT").help("Path to the SVG file to render").index(1))
            .get_matches();

        if let Some(jobs) = matches.value_of("jobs") {
            self.jobs = jobs.parse().ok();
        }

        if matches.is_present("3d") {
            self.mode = Mode::ThreeD;
        } else if matches.is_present("vr") {
            self.mode = Mode::VR;
        }

        if let Some(ui) = matches.value_of("ui") {
            self.ui = match ui {
                "none" => UIVisibility::None,
                "stats" => UIVisibility::Stats,
                _ => UIVisibility::All,
            };
        }

        if let Some(bg) = matches.value_of("bg") {
            self.background = match bg {
                "dark" => Background::Dark,
                "light" => Background::Light,
                _ => Background::None,
            }
        }

        if matches.is_present("pipeline") {
            self.pipeline = true;
        } else if matches.is_present("no-pipeline") {
            self.pipeline = false;
        }

        if let Some(path) = matches.value_of("INPUT") {
            self.input_path = SVGPath::Path(PathBuf::from(path));
        };
    }

    fn adjust_thread_pool_settings(&self, mut thread_pool_builder: ThreadPoolBuilder) -> ThreadPoolBuilder {
        if let Some(jobs) = self.jobs {
            thread_pool_builder = thread_pool_builder.num_threads(jobs);
        }
        thread_pool_builder
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum UIVisibility {
    None,
    Stats,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Background {
    None  = 0,
    Dark  = 1,
    Light = 2,
}

impl From<u8> for Background {
    fn from(val: u8) -> Background {
        match val {
            1 => Background::Dark,
            2 => Background::Light,
            _ => Background::None,
        }
    }
}

#[derive(Clone, Copy)]
struct RenderStats {
    rendering_time: Option<Duration>,
    stats: Stats,
}

fn load_scene(resource_loader: &dyn ResourceLoader, input_path: &SVGPath) -> BuiltSVG {
    let mut data;
    match *input_path {
        SVGPath::Default => data = resource_loader.slurp(DEFAULT_SVG_VIRTUAL_PATH).unwrap(),
        SVGPath::Resource(ref name) => data = resource_loader.slurp(name).unwrap(),
        SVGPath::Path(ref path) => {
            data = vec![];
            File::open(path).unwrap().read_to_end(&mut data).unwrap();
        }
    };

    BuiltSVG::from_tree(Tree::from_data(&data, &UsvgOptions::default()).unwrap())
}

fn build_scene(scene: &Scene,
               build_options: &BuildOptions,
               render_transform: RenderTransform,
               jobs: Option<usize>)
               -> BuiltScene {
    let render_options = RenderOptions {
        transform: render_transform,
        dilation: match build_options.stem_darkening_font_size {
            None => Point2DF32::default(),
            Some(font_size) => {
                let (x, y) = (STEM_DARKENING_FACTORS[0], STEM_DARKENING_FACTORS[1]);
                Point2DF32::new(x, y).scale(font_size)
            }
        },
        barrel_distortion: build_options.barrel_distortion,
        subpixel_aa_enabled: build_options.subpixel_aa_enabled,
    };

    let built_options = render_options.prepare(scene.bounds);
    let effective_view_box = scene.effective_view_box(&built_options);
    let z_buffer = ZBuffer::new(effective_view_box);

    let quad = built_options.quad();

    let built_objects = panic::catch_unwind(|| {
         match jobs {
            Some(1) => scene.build_objects_sequentially(built_options, &z_buffer),
            _ => scene.build_objects(built_options, &z_buffer),
        }
    });

    let built_objects = match built_objects {
        Ok(built_objects) => built_objects,
        Err(_) => {
            eprintln!("Scene building crashed! Dumping scene:");
            println!("{:?}", scene);
            process::exit(1);
        }
    };

    let mut built_scene = BuiltScene::new(scene.view_box, &quad, scene.objects.len() as u32);
    built_scene.shaders = scene.build_shaders();

    let mut scene_builder = SceneBuilder::new(built_objects, z_buffer, effective_view_box);
    built_scene.solid_tiles = scene_builder.build_solid_tiles();
    while let Some(batch) = scene_builder.build_batch() {
        built_scene.batches.push(batch);
    }

    built_scene
}

fn center_of_window(window_size: &WindowSize) -> Point2DF32 {
    window_size.device_size().to_f32().scale(0.5)
}

enum Camera {
    TwoD(Transform2DF32),
    ThreeD {
        // The mode will either be 3D or VR
        mode: Mode,
        // For each camera, the perspective from camera coordinates to display coordinates,
        // and the view transform from world coordinates to camera coordinates.
        transforms: Vec<CameraTransform>,
        // The model transform from world coordinates to SVG coordinates
        transform: CameraTransform3D,
        // The camera's velocity (in world coordinates)
        velocity: Point3DF32,
    },
}

impl Camera {
    fn new(mode: Mode, view_box: RectF32, view_box_size: Point2DI32) -> Camera {
        if mode == Mode::TwoD {
            Camera::new_2d(view_box, view_box_size)
        } else {
            Camera::new_3d(mode, view_box, view_box_size)
        }
    }

    fn new_2d(view_box: RectF32, view_box_size: Point2DI32) -> Camera {
        let scale = i32::min(view_box_size.x(), view_box_size.y()) as f32 *
            scale_factor_for_view_box(view_box);
        let origin = view_box_size.to_f32().scale(0.5) - view_box.size().scale(scale * 0.5);
        Camera::TwoD(Transform2DF32::from_scale(&Point2DF32::splat(scale)).post_translate(origin))
    }

    fn new_3d(mode: Mode, view_box: RectF32, view_box_size: Point2DI32) -> Camera {
        let viewport_count = mode.viewport_count();
        let aspect = view_box_size.x() as f32 / view_box_size.y() as f32;
        let projection = Transform3DF32::from_perspective(FRAC_PI_4, aspect, NEAR_CLIP_PLANE, FAR_CLIP_PLANE);
        let transform = CameraTransform {
            perspective: Perspective::new(&projection, view_box_size),
            view: Transform3DF32::default(),
        };
        let transforms = iter::repeat(transform).take(viewport_count).collect();

        Camera::ThreeD {
            mode,
            transforms,
            transform: CameraTransform3D::new(view_box),
            velocity: Point3DF32::default(),
        }
    }

    fn is_3d(&self) -> bool {
        match *self { Camera::ThreeD { .. } => true, Camera::TwoD { .. } => false }
    }

    fn mode(&self) -> Mode {
        match *self { Camera::ThreeD { mode, .. } => mode, Camera::TwoD { .. } => Mode::TwoD }
    }
}

#[derive(Clone, Copy)]
struct CameraTransform3D {
    position: Point3DF32,
    yaw: f32,
    pitch: f32,
    scale: f32,
}

impl CameraTransform3D {
    fn new(view_box: RectF32) -> CameraTransform3D {
        let scale = scale_factor_for_view_box(view_box);
        CameraTransform3D {
            position: Point3DF32::new(0.5 * view_box.max_x(),
                                      -0.5 * view_box.max_y(),
                                      1.5 / scale,
                                      1.0),
            yaw: 0.0,
            pitch: 0.0,
            scale,
        }
    }

    fn offset(&mut self, vector: Point3DF32) -> bool {
        let update = !vector.is_zero();
        if update {
            let rotation = Transform3DF32::from_rotation(-self.yaw, -self.pitch, 0.0);
            self.position = self.position + rotation.transform_point(vector);
        }
        update
    }

    fn to_transform(&self) -> Transform3DF32 {
        let mut transform = Transform3DF32::from_rotation(self.yaw, self.pitch, 0.0);
        transform = transform.post_mul(&Transform3DF32::from_uniform_scale(2.0 * self.scale));
        transform = transform.post_mul(&Transform3DF32::from_translation(-self.position.x(),
                                                                         -self.position.y(),
                                                                         -self.position.z()));

        // Flip Y.
        transform = transform.post_mul(&Transform3DF32::from_scale(1.0, -1.0, 1.0));

        transform
    }
}

fn scale_factor_for_view_box(view_box: RectF32) -> f32 {
    1.0 / f32::min(view_box.size().x(), view_box.size().y())
}

fn get_mouse_position<W>(window: &W, scale_factor: f32) -> Point2DF32 where W: Window {
    window.mouse_position().to_f32().scale(scale_factor)
}

fn get_svg_building_message(built_svg: &BuiltSVG) -> String {
    if built_svg.result_flags.is_empty() {
        return String::new();
    }
    format!("Warning: These features in the SVG are unsupported: {}.", built_svg.result_flags)
}

fn emit_message<W>(ui: &mut DemoUI<GLDevice>,
                   message_epoch: &mut u32,
                   expire_message_event_id: u32,
                   message: String)
                   where W: Window {
    if message.is_empty() {
        return;
    }

    ui.message = message;
    let expected_epoch = *message_epoch + 1;
    *message_epoch = expected_epoch;
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(MESSAGE_TIMEOUT_SECS));
        W::push_user_event(expire_message_event_id, expected_epoch);
    });
}

struct Frame {
    render_msg: SceneToMainMsg,
    ui_events: Vec<UIEvent>,
    render_stats: Option<RenderStats>,
}

impl Frame {
    fn new(render_msg: SceneToMainMsg, ui_events: Vec<UIEvent>) -> Frame {
        Frame { render_msg, ui_events, render_stats: None }
    }
}
