// pathfinder/demo/immersive/magicleap.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

use crate::display::Display;
use crate::display::DisplayCamera;
use crate::display::DisplayError;

use crate::immersive::ImmersiveDemo;

use egl;
use egl::EGL_NO_SURFACE;
use egl::EGLContext;
use egl::EGLDisplay;

use gl;
use gl::types::GLuint;

use log;
use log::error;
use log::warn;
use log::info;
use log::debug;

use pathfinder_geometry::basic::point::Point2DI32;
use pathfinder_geometry::basic::point::Point2DF32;
use pathfinder_geometry::basic::rect::RectF32;
use pathfinder_geometry::basic::rect::RectI32;
use pathfinder_geometry::basic::transform3d::Transform3DF32;
use pathfinder_geometry::basic::transform3d::Perspective;

use smallvec::SmallVec;

use std::error::Error;
use std::ffi::CStr;
use std::ffi::CString;
use std::fmt;
use std::io::Write;
use std::mem;
use std::ptr;
use std::thread;
use std::time::Duration;
use std::os::raw::c_char;
use std::os::raw::c_void;
use std::str::Utf8Error;

use usvg;

#[no_mangle]
pub fn magicleap_pathfinder_demo(egl_display: EGLDisplay, egl_context: EGLContext) -> MLResult {
    match run_demo(egl_display, egl_context) {
        Ok(()) => ML_RESULT_OK,
        Err(MagicLeapError::ML(err)) => {
            error!("ML error {:?}", err);
            err
        },
        Err(MagicLeapError::SVG(err)) => {
            error!("SVG error {:?}", err);
            ML_RESULT_UNSPECIFIED_FAILURE
        },
    }
}

fn run_demo(egl_display: EGLDisplay, egl_context: EGLContext) -> Result<(), MagicLeapError> {
    let _ = log::set_boxed_logger(Box::new(MLLogger));
    log::set_max_level(LOG_LEVEL);

    let display = MagicLeapDisplay::new(egl_display, egl_context)?;
    let mut demo = ImmersiveDemo::new(display)?;

    while demo.running() {
        demo.render_scene()?;
    }

    Ok(())
}

pub struct MagicLeapDisplay {
    egl_display: EGLDisplay,
    egl_context: EGLContext,
    framebuffer_id: GLuint,
    graphics_client: MLHandle,
    size: Point2DI32,
    cameras: Vec<MagicLeapCamera>,
    frame_handle: MLHandle,
    running: bool,
    in_frame: bool,
}

pub struct MagicLeapCamera {
    color_id: GLuint,
    depth_id: GLuint,
    viewport: RectI32,
    virtual_camera: MLGraphicsVirtualCameraInfo,
}

#[derive(Debug)]
pub enum MagicLeapError {
    SVG(usvg::Error),
    ML(MLResult),
}

impl Display for MagicLeapDisplay {
    type Camera = MagicLeapCamera;
    type Error = MagicLeapError;

    fn make_current(&mut self) -> Result<(), MagicLeapError> {
        debug!("PF making GL context current");
        unsafe {
            egl::make_current(self.egl_display, EGL_NO_SURFACE, EGL_NO_SURFACE, self.egl_context);
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffer_id);
        }
        Ok(())
    }

    fn begin_frame(&mut self) -> Result<&mut[MagicLeapCamera], MagicLeapError> {
        if self.in_frame { return Ok(&mut self.cameras[..]); }
        debug!("PF beginning frame");
        let mut params = unsafe { mem::zeroed() };
        let mut virtual_camera_array = unsafe { mem::zeroed() };
        unsafe {
            egl::make_current(self.egl_display, EGL_NO_SURFACE, EGL_NO_SURFACE, self.egl_context);
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffer_id);
            MLGraphicsInitFrameParams(&mut params).ok()?;
            let mut result = MLGraphicsBeginFrame(self.graphics_client, &params, &mut self.frame_handle, &mut virtual_camera_array);
            if result == ML_RESULT_TIMEOUT {
                info!("PF frame timeout");
                  let mut sleep = Duration::from_millis(1);
                let max_sleep = Duration::from_secs(5);
                while result == ML_RESULT_TIMEOUT {                    
                    sleep = (sleep * 2).min(max_sleep);
                    info!("PF exponential backoff {}ms", sleep.as_millis());
                    thread::sleep(sleep);
                    result = MLGraphicsBeginFrame(self.graphics_client, &params, &mut self.frame_handle, &mut virtual_camera_array);
                }
                 info!("PF frame finished timeout");
            }
            result.ok()?;
        }
        let viewport = RectI32::from(virtual_camera_array.viewport);
        self.cameras.clear();
        for i in 0..(virtual_camera_array.num_virtual_cameras as usize) {
            self.cameras.push(MagicLeapCamera {
                color_id: virtual_camera_array.color_id.as_gl_uint(),
                depth_id: virtual_camera_array.depth_id.as_gl_uint(),
                viewport: viewport,
                virtual_camera: virtual_camera_array.virtual_cameras[i],
             });
        }
        self.in_frame = true;
        debug!("PF begun frame");
        Ok(&mut self.cameras[..])
    }

    fn end_frame(&mut self) -> Result<(), MagicLeapError> {
        if !self.in_frame { return Ok(()); }
        debug!("PF ending frame");
        let graphics_client = self.graphics_client;
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            for camera in self.cameras.drain(..) {
                MLGraphicsSignalSyncObjectGL(graphics_client, camera.virtual_camera.sync_object).ok()?;
            }
            MLGraphicsEndFrame(graphics_client, self.frame_handle).ok()?;
        }
        self.in_frame = false;
        debug!("PF ended frame");
        Ok(())
    }

    fn running(&self) -> bool {
        self.running
    }

    fn size(&self) -> Point2DI32 {
        self.size
    }
}

impl DisplayCamera for MagicLeapCamera {
    type Error = MagicLeapError;

    fn make_current(&mut self) -> Result<(), MagicLeapError> {
        let viewport = self.bounds();
        let layer_id = self.virtual_camera.virtual_camera_name as i32;
        unsafe {
            gl::FramebufferTextureLayer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, self.color_id, 0, layer_id);
            gl::FramebufferTextureLayer(gl::FRAMEBUFFER, gl::DEPTH_ATTACHMENT, self.depth_id, 0, layer_id);
            gl::Viewport(viewport.origin().x(), viewport.origin().y(), viewport.size().x(), viewport.size().y());
        }
        Ok(())
    }

    fn bounds(&self) -> RectI32 {
        self.viewport
    }

    fn perspective(&self) -> Perspective {
        let bounds = self.bounds();
        let projection = Transform3DF32::from(self.virtual_camera.projection);
        Perspective::new(&projection, bounds.size())
    }

    fn view(&self) -> Transform3DF32 {
        Transform3DF32::from(self.virtual_camera.transform).inverse()
    }
}

fn get_proc_address(s: &str) -> *const c_void {
    egl::get_proc_address(s) as *const c_void
}

impl MagicLeapDisplay {
    fn new(egl_display: EGLDisplay, egl_context: EGLContext) -> Result<MagicLeapDisplay, MagicLeapError> {
        let mut framebuffer_id = 0;
        let graphics_options = MLGraphicsOptions::default();
        let mut graphics_client =  unsafe { mem::zeroed() };
        let mut head_tracker = unsafe { mem::zeroed() };
        let mut targets = unsafe { mem::zeroed() };
        let handle = MLHandle::from(egl_context);
        unsafe {
            egl::make_current(egl_display, EGL_NO_SURFACE, EGL_NO_SURFACE, egl_context);
            gl::load_with(get_proc_address);
            gl::GenFramebuffers(1, &mut framebuffer_id);
            MLGraphicsCreateClientGL(&graphics_options, handle, &mut graphics_client).ok()?;
            MLLifecycleSetReadyIndication().ok()?;
            MLHeadTrackingCreate(&mut head_tracker).ok()?;
            MLGraphicsGetRenderTargets(graphics_client, &mut targets).ok()?;
        }
        let (max_width, max_height) = targets.buffers.iter().map(|buffer| buffer.color)
            .chain(targets.buffers.iter().map(|buffer| buffer.depth))
            .map(|target| (target.width as i32, target.height as i32))
            .max()
            .unwrap_or_default();
        Ok(MagicLeapDisplay {
            egl_display,
            egl_context,
            framebuffer_id,
            graphics_client,
            size: Point2DI32::new(max_width, max_height),
            cameras: Vec::new(),
            frame_handle: ML_HANDLE_INVALID,
            running: true,
            in_frame: false,
        })
    }
}

impl Drop for MagicLeapDisplay {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteFramebuffers(1, &self.framebuffer_id);
            MLGraphicsDestroyClient(&mut self.graphics_client);
        }
    }
}

impl From<usvg::Error> for MagicLeapError {
    fn from(err: usvg::Error) -> MagicLeapError {
        MagicLeapError::SVG(err)
    }
}

impl From<MLResult> for MagicLeapError {
    fn from(err: MLResult) -> MagicLeapError {
        MagicLeapError::ML(err)
    }
}

impl fmt::Display for MagicLeapError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            MagicLeapError::SVG(ref err) => err.fmt(formatter),
            MagicLeapError::ML(ref err) => err.fmt(formatter),
        }
    }
}

impl Error for MagicLeapError {
}

impl DisplayError for MagicLeapError {
}

// Types from the MagicLeap C API

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MLHandle(u64);

impl MLHandle {
    fn take(&mut self) -> Option<MLHandle> {
        if *self == ML_HANDLE_INVALID {
            None
        } else {
            let result = Some(*self);
            self.0 = 0;
            result
        }
    }

    fn as_gl_uint(self) -> GLuint {
        self.0 as GLuint
    }
}

impl<T> From<*mut T> for MLHandle {
    fn from(ptr: *mut T) -> MLHandle {
        MLHandle(ptr as u64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MLResult(u32);

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsOptions {
    graphics_flags: u32,
    color_format: MLSurfaceFormat,
    depth_format: MLSurfaceFormat,
}

impl Default for MLGraphicsOptions {
    fn default() -> MLGraphicsOptions {
        MLGraphicsOptions {
            graphics_flags: 0,
            color_format: MLSurfaceFormat::RGBA8UNormSRGB,
            depth_format: MLSurfaceFormat::D32Float,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsRenderTargetsInfo {
    min_clip: f32,
    max_clip: f32,
    num_virtual_cameras: u32,
    buffers: [MLGraphicsRenderBufferInfo; ML_BUFFER_COUNT],
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsRenderBufferInfo {
    color: MLGraphicsRenderTarget,
    depth: MLGraphicsRenderTarget,
}
  
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsRenderTarget {
  width: u32,
  height: u32,
  id: MLHandle,
  format: MLSurfaceFormat,
}

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
enum MLSurfaceFormat {
    Unknown = 0,
    RGBA8UNorm,
    RGBA8UNormSRGB,
    RGB10A2UNorm,
    RGBA16Float,
    D32Float,
    D24NormS8,
    D32FloatS8,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsVirtualCameraInfoArray {
    num_virtual_cameras: u32,
    color_id: MLHandle,
    depth_id: MLHandle,
    viewport: MLRectf,
    virtual_cameras: [MLGraphicsVirtualCameraInfo; ML_VIRTUAL_CAMERA_COUNT],
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsVirtualCameraInfo {
    left_half_angle: f32,
    right_half_angle: f32,
    top_half_angle: f32,
    bottom_half_angle: f32,
    sync_object: MLHandle,
    projection: MLMat4f,
    transform: MLTransform,
    virtual_camera_name: MLGraphicsVirtualCameraName,
}

#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum MLGraphicsVirtualCameraName {
    Combined = -1,
    Left = 0,
    Right,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsFrameParams {
    near_clip: f32,
    far_clip: f32,
    focus_distance: f32,
    surface_scale: f32,
    protected_surface: bool,
    projection_type: MLGraphicsProjectionType,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLSnapshotPtr(*mut c_void);

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLCoordinateFrameUID {
    data: [u64; 2],
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLHeadTrackingStaticData {
    coord_frame_head: MLCoordinateFrameUID,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsClipExtentsInfo {
    virtual_camera_name: MLGraphicsVirtualCameraName,
    projection: MLMat4f,
    transform: MLTransform,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLGraphicsClipExtentsInfoArray {
    num_virtual_cameras: u32,
    full_extents: MLGraphicsClipExtentsInfo,
    virtual_camera_extents: [MLGraphicsClipExtentsInfo; ML_VIRTUAL_CAMERA_COUNT],
}

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
enum MLGraphicsProjectionType {
    SignedZ = 0,
    ReversedInfiniteZ = 1,
    UnsignedZ = 2,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLTransform {
    rotation: MLQuaternionf,
    position: MLVec3f,
}

impl From<MLTransform> for Transform3DF32 {
    fn from(mat: MLTransform) -> Self {
        Transform3DF32::from(mat.rotation)
           .pre_mul(&Transform3DF32::from(mat.position))
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLVec3f {
    x: f32,
    y: f32,
    z: f32,
}

impl From<MLVec3f> for Transform3DF32 {
    fn from(v: MLVec3f) -> Self {
        Transform3DF32::from_translation(v.x, v.y, v.z)
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLRectf {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl From<MLRectf> for RectF32 {
    fn from(r: MLRectf) -> Self {
        RectF32::new(Point2DF32::new(r.x, r.y), Point2DF32::new(r.w, r.h))
    }
}

impl From<MLRectf> for RectI32 {
    fn from(r: MLRectf) -> Self {
        RectF32::from(r).to_i32()
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLQuaternionf {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

impl From<MLQuaternionf> for Transform3DF32 {
    fn from(q: MLQuaternionf) -> Self {
        Transform3DF32::from_quaternion(q.x, q.y, q.z, q.w)
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct MLMat4f {
    matrix_colmajor: [f32; 16],
}

impl From<MLMat4f> for Transform3DF32 {
    fn from(mat: MLMat4f) -> Self {
        let a = mat.matrix_colmajor;
        Transform3DF32::row_major(a[0], a[4], a[8],  a[12],
                                  a[1], a[5], a[9],  a[13],
                                  a[2], a[6], a[10], a[14],
                                  a[3], a[7], a[11], a[15])
    }
}

// Constants from the MagicLeap C API

const ML_RESULT_OK: MLResult = MLResult(0);
const ML_RESULT_TIMEOUT: MLResult = MLResult(2);
const ML_RESULT_UNSPECIFIED_FAILURE: MLResult = MLResult(4);
const ML_HANDLE_INVALID: MLHandle = MLHandle(0xFFFFFFFFFFFFFFFF);
const ML_BUFFER_COUNT: usize = 3;
const ML_VIRTUAL_CAMERA_COUNT: usize = 2;

// Functions from the MagicLeap C API

extern "C" {
    fn MLGraphicsCreateClientGL(options: *const MLGraphicsOptions, gl_context: MLHandle, graphics_client : &mut MLHandle) -> MLResult;
    fn MLGraphicsDestroyClient(graphics_client: *mut MLHandle) -> MLResult;
    fn MLHeadTrackingCreate(tracker: *mut MLHandle) -> MLResult;
    fn MLHeadTrackingGetStaticData(head_tracker: MLHandle, data: *mut MLHeadTrackingStaticData) -> MLResult;
    fn MLPerceptionGetSnapshot(snapshot: *mut MLSnapshotPtr) -> MLResult;
    fn MLSnapshotGetTransform(snapshot: MLSnapshotPtr, id: *const MLCoordinateFrameUID, transform: *mut MLTransform) -> MLResult;
    fn MLPerceptionReleaseSnapshot(snapshot: MLSnapshotPtr) -> MLResult;
    fn MLLifecycleSetReadyIndication() -> MLResult;
    fn MLGraphicsGetClipExtents(graphics_client: MLHandle, array: *mut MLGraphicsClipExtentsInfoArray) -> MLResult;
    fn MLGraphicsGetRenderTargets(graphics_client: MLHandle, targets: *mut MLGraphicsRenderTargetsInfo) -> MLResult;
    fn MLGraphicsInitFrameParams(params: *mut MLGraphicsFrameParams) -> MLResult;
    fn MLGraphicsBeginFrame(graphics_client: MLHandle, params: *const MLGraphicsFrameParams, frame_handle: *mut MLHandle, virtual_camera_array: *mut MLGraphicsVirtualCameraInfoArray) -> MLResult;
    fn MLGraphicsEndFrame(graphics_client: MLHandle, frame_handle: MLHandle) -> MLResult;
    fn MLGraphicsSignalSyncObjectGL(graphics_client: MLHandle, sync_object: MLHandle) -> MLResult;
    fn MLGetResultString(result_code: MLResult) -> *const c_char;
}

// ML error handling

impl fmt::Display for MLResult {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let cmessage = unsafe { CStr::from_ptr(MLGetResultString(*self)) };
        let message = cmessage.to_str().or(Err(fmt::Error))?;
        formatter.write_str(message)
    }
}

impl MLResult {
    fn ok(self) -> Result<(), MLResult> {
        if self == ML_RESULT_OK {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl Error for MLResult {
}

// Logging

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MLLogLevel {
    Fatal = 0,
    Error = 1,
    Warning = 2,
    Info = 3,
    Debug = 4,
    Verbose = 5,
}

extern "C" {
    fn logMessage(lvl: MLLogLevel, msg: *const c_char);
}

// TODO: DRY

const LOG_LEVEL: log::LevelFilter = log::LevelFilter::Info;

struct MLLogger;

impl log::Log for MLLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= LOG_LEVEL
    }

    fn log(&self, record: &log::Record) {
        let lvl = match record.level() {
            log::Level::Error => MLLogLevel::Error,
            log::Level::Warn => MLLogLevel::Warning,
            log::Level::Info => MLLogLevel::Info,
            log::Level::Debug => MLLogLevel::Debug,
            log::Level::Trace => MLLogLevel::Verbose,
        };
        let mut msg = SmallVec::<[u8; 128]>::new();
        write!(msg, "{}\0", record.args()).unwrap();
        unsafe {
            logMessage(lvl, &msg[0] as *const _ as *const _);
        }
    }

    fn flush(&self) {}
}
