//! The Hyprmux backend: windows become tiles in Hyprmux, a tiling compositor that
//! runs inside one macOS window, through its client protocol
//! (https://github.com/gavrix/hypermux, docs/CLIENT_PROTOCOL.md).
//!
//! Only windowing changes. The run loop, text system, clipboard, and keyboard
//! layouts stay `MacPlatform`'s. A `HyprmuxWindow` has no `NSWindow`: it draws with
//! the Metal renderer into IOSurfaces it registers with Hyprmux, and takes input,
//! text input, and frame pacing from protocol events.
//!
//! Enabled when Hyprmux launched the process (`HYPRMUX_LAUNCH_TOKEN`), or with
//! `HYPRMUX_GPUI=1` to open a tile in a running Hyprmux.

use crate::{renderer, MacDisplay};
use anyhow::{anyhow, Result};
use cocoa::{
    appkit::NSEventType,
    base::{id, nil},
    foundation::{NSPoint, NSString},
};
use futures::channel::oneshot;
use gpui::{
    AnyWindowHandle, Bounds, Capslock, CursorStyle, DispatchEventResult, GpuSpecs, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, NavigationDirection, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PathPromptOptions, PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    Scene, ScrollDelta, ScrollWheelEvent, Size, TouchPhase, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams, WindowVisibility,
    point, px, size,
};
use hyprmux_client::{surface::Surface as IOSurface, ButtonState, Client, Event, Toplevel};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle as rwh;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
};

/// Whether windows should open as Hyprmux tiles.
pub(crate) fn enabled() -> bool {
    hyprmux_client::launched_by_hyprmux() || std::env::var("HYPRMUX_GPUI").is_ok_and(|v| v == "1")
}

const SWAPCHAIN_LENGTH: usize = 3;

/// `HYPRMUX_GPUI_DEBUG=1`: log input, focus, and frames at info level.
fn debug() -> bool {
    thread_local!(static ON: bool = std::env::var("HYPRMUX_GPUI_DEBUG").is_ok_and(|v| v == "1"));
    ON.with(|on| *on)
}

// MARK: Connection

/// The process's one connection, and where events go.
struct Connection {
    client: Rc<Client>,
    windows: RefCell<HashMap<u64, Weak<WindowCell>>>,
    frame_callbacks: RefCell<HashMap<u64, Weak<WindowCell>>>,
    buffers: RefCell<HashMap<u64, Weak<WindowCell>>>,
    /// Dialog replies by request id, given the result JSON.
    dialogs: RefCell<HashMap<u64, Box<dyn FnOnce(String)>>>,
}

thread_local! {
    static CONNECTION: RefCell<Option<Rc<Connection>>> = const { RefCell::new(None) };
}

fn connection() -> Result<Rc<Connection>> {
    if let Some(c) = CONNECTION.with(|c| c.borrow().clone()) {
        if c.client.is_connected() {
            return Ok(c);
        }
    }
    let app_id = std::env::var("HYPRMUX_APP_ID").unwrap_or_else(|_| "dev.zed.gpui".into());
    let name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "GPUI".into());
    let client = Client::connect(&app_id, &name, route_event).map_err(|e| anyhow!("Hyprmux: {e}"))?;
    let c = Rc::new(Connection {
        client,
        windows: RefCell::default(),
        frame_callbacks: RefCell::default(),
        buffers: RefCell::default(),
        dialogs: RefCell::default(),
    });
    CONNECTION.with(|slot| *slot.borrow_mut() = Some(c.clone()));
    Ok(c)
}

fn with_connection(f: impl FnOnce(&Connection)) {
    if let Some(c) = CONNECTION.with(|c| c.borrow().clone()) {
        f(&c);
    }
}

/// Every protocol event, on the main queue.
fn route_event(event: Event) {
    let Some(c) = CONNECTION.with(|c| c.borrow().clone()) else { return };
    // Look windows up into locals: GPUI callbacks may open or close windows, which
    // needs the map mutable.
    let by_surface = |surface: u64| -> Option<Rc<WindowCell>> {
        c.windows.borrow().values().filter_map(|w| w.upgrade()).find(|w| w.toplevel.surface == surface)
    };
    let by_toplevel = |toplevel: u64| -> Option<Rc<WindowCell>> {
        let w = c.windows.borrow().get(&toplevel).and_then(|w| w.upgrade());
        w
    };
    match event {
        Event::Configure { toplevel, width, height, scale, states, serial } => {
            if let Some(w) = by_toplevel(toplevel) {
                w.configure(width, height, scale, &states, serial);
            }
        }
        Event::CloseRequested { toplevel } => {
            if let Some(w) = by_toplevel(toplevel) {
                w.close_requested();
            }
        }
        Event::FrameDone { callback, .. } => {
            let w = c.frame_callbacks.borrow_mut().remove(&callback).and_then(|w| w.upgrade());
            if let Some(w) = w {
                w.frame_done();
            }
        }
        Event::BufferReleased { buffer } => {
            let w = c.buffers.borrow().get(&buffer).and_then(|w| w.upgrade());
            if let Some(w) = w {
                w.buffer_released(buffer);
            }
        }
        Event::DialogResult { id, result_json } => {
            let reply = c.dialogs.borrow_mut().remove(&id);
            if let Some(reply) = reply {
                reply(result_json);
            }
        }
        Event::Disconnected { reason } => {
            log::error!("Hyprmux disconnected: {reason}");
            // The tiles are gone; nothing can show this process's windows anymore.
            let windows: Vec<_> = c.windows.borrow().values().filter_map(|w| w.upgrade()).collect();
            for w in windows {
                w.closed_by_compositor();
            }
            // An app Hyprmux launched has nowhere left to show windows: quit, rather
            // than linger without any.
            if hyprmux_client::launched_by_hyprmux() {
                unsafe {
                    let app: id = msg_send![class!(NSApplication), sharedApplication];
                    let _: () = msg_send![app, terminate: nil];
                }
            }
        }
        Event::ProtocolError { code, message } => log::error!("Hyprmux protocol error {code}: {message}"),
        Event::PointerEnter { surface, x, y } => {
            if let Some(w) = by_surface(surface) {
                w.pointer_enter(x, y);
            }
        }
        Event::PointerLeave { surface } => {
            if let Some(w) = by_surface(surface) {
                w.pointer_leave();
            }
        }
        Event::PointerMotion { surface, x, y, modifiers, .. } => {
            if let Some(w) = by_surface(surface) {
                w.pointer_motion(x, y, modifiers);
            }
        }
        Event::PointerButton { surface, x, y, button, state, click_count, modifiers } => {
            if let Some(w) = by_surface(surface) {
                w.pointer_button(x, y, button, state == ButtonState::Down, click_count, modifiers);
            }
        }
        Event::PointerScroll { surface, x, y, dx, dy, precise, phase, modifiers, .. } => {
            if let Some(w) = by_surface(surface) {
                w.scroll(x, y, dx, dy, precise, phase, modifiers);
            }
        }
        Event::KeyboardEnter { surface, .. } => {
            if let Some(w) = by_surface(surface) {
                w.set_active(true);
            }
        }
        Event::KeyboardLeave { surface } => {
            if let Some(w) = by_surface(surface) {
                w.set_active(false);
            }
        }
        Event::Key { surface, key_code, down, repeat, characters, characters_ignoring_modifiers, modifiers, .. } => {
            if let Some(w) = by_surface(surface) {
                w.key(key_code, down, repeat, &characters, &characters_ignoring_modifiers, modifiers);
            }
        }
        Event::Modifiers { surface, modifiers } => {
            if let Some(w) = by_surface(surface) {
                w.modifiers_changed(modifiers);
            }
        }
        Event::TextPreedit { surface, text, cursor_begin, cursor_end } => {
            if let Some(w) = by_surface(surface) {
                let mut t = w.text.borrow_mut();
                t.preedit = Some((text, cursor_begin as usize..cursor_end as usize));
            }
        }
        Event::TextCommit { surface, text } => {
            if let Some(w) = by_surface(surface) {
                w.text.borrow_mut().commit.push_str(&text);
            }
        }
        Event::TextDeleteSurrounding { surface, before, after } => {
            if let Some(w) = by_surface(surface) {
                let mut t = w.text.borrow_mut();
                t.delete_before += before as usize;
                t.delete_after += after as usize;
            }
        }
        Event::TextDone { surface, .. } => {
            if let Some(w) = by_surface(surface) {
                w.apply_text_input();
            }
        }
        Event::MenuSelected { .. } => {}
    }
}

/// The cursor for the whole connection: Hyprmux shows it over the pointer's tile.
pub(crate) fn set_cursor_style(style: CursorStyle) {
    let name = match style {
        CursorStyle::Arrow => "arrow",
        CursorStyle::IBeam | CursorStyle::IBeamCursorForVerticalLayout => "ibeam",
        CursorStyle::Crosshair => "crosshair",
        CursorStyle::ClosedHand => "closed_hand",
        CursorStyle::OpenHand => "open_hand",
        CursorStyle::PointingHand | CursorStyle::DragLink => "pointing_hand",
        CursorStyle::ResizeLeft | CursorStyle::ResizeRight | CursorStyle::ResizeLeftRight | CursorStyle::ResizeColumn => {
            "resize_left_right"
        }
        CursorStyle::ResizeUp | CursorStyle::ResizeDown | CursorStyle::ResizeUpDown | CursorStyle::ResizeRow => {
            "resize_up_down"
        }
        CursorStyle::OperationNotAllowed => "not_allowed",
        _ => "arrow",
    };
    with_connection(|c| c.client.set_cursor(name));
}

// MARK: File dialogs

/// The tile a dialog belongs to: the focused one, else any.
fn dialog_surface(c: &Connection) -> Option<u64> {
    let windows: Vec<_> = c.windows.borrow().values().filter_map(|w| w.upgrade()).collect();
    windows.iter().find(|w| w.active.get()).or(windows.first()).map(|w| w.toplevel.surface)
}

/// Open panels as Hyprmux sheets: a native panel would sit on no visible window.
pub(crate) fn prompt_for_paths(options: &PathPromptOptions) -> Option<oneshot::Receiver<anyhow::Result<Option<Vec<PathBuf>>>>> {
    let c = CONNECTION.with(|c| c.borrow().clone())?;
    let surface = dialog_surface(&c)?;
    let mut properties = vec!["createDirectory"];
    if options.files {
        properties.push("openFile");
    }
    if options.directories {
        properties.push("openDirectory");
    }
    if options.multiple {
        properties.push("multiSelections");
    }
    let mut request = serde_json::json!({ "properties": properties });
    if let Some(prompt) = &options.prompt {
        request["buttonLabel"] = serde_json::Value::from(prompt.to_string());
    }
    let id = c.client.open_dialog(surface, "open", &request.to_string());
    let (tx, rx) = oneshot::channel();
    c.dialogs.borrow_mut().insert(id, Box::new(move |json: String| {
        let result: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        let paths = (!result["canceled"].as_bool().unwrap_or(true)).then(|| {
            result["filePaths"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).map(PathBuf::from).collect()
        });
        let _ = tx.send(Ok(paths));
    }));
    Some(rx)
}

pub(crate) fn prompt_for_new_path(directory: &Path, suggested_name: Option<&str>) -> Option<oneshot::Receiver<anyhow::Result<Option<PathBuf>>>> {
    let c = CONNECTION.with(|c| c.borrow().clone())?;
    let surface = dialog_surface(&c)?;
    let default_path = match suggested_name {
        Some(name) => directory.join(name),
        None => directory.to_path_buf(),
    };
    let request = serde_json::json!({
        "defaultPath": default_path.to_string_lossy(),
        "properties": ["createDirectory"],
    });
    let id = c.client.open_dialog(surface, "save", &request.to_string());
    let (tx, rx) = oneshot::channel();
    c.dialogs.borrow_mut().insert(id, Box::new(move |json: String| {
        let result: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        let path = (!result["canceled"].as_bool().unwrap_or(true))
            .then(|| result["filePath"].as_str().map(PathBuf::from))
            .flatten();
        let _ = tx.send(Ok(path));
    }));
    Some(rx)
}

// MARK: Window

struct Buffer {
    id: u64,
    surface: IOSurface,
    texture: metal::Texture,
    busy: bool,
}

#[derive(Default)]
struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    visibility_change: Option<Box<dyn FnMut(WindowVisibility)>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
}

#[derive(Default)]
struct TextUpdate {
    delete_before: usize,
    delete_after: usize,
    commit: String,
    preedit: Option<(String, std::ops::Range<usize>)>,
}

struct WindowCell {
    handle: AnyWindowHandle,
    toplevel: Toplevel,
    client: Rc<Client>,
    renderer: RefCell<renderer::Renderer>,
    swapchain: RefCell<Vec<Buffer>>,
    callbacks: RefCell<Callbacks>,
    input_handler: RefCell<Option<PlatformInputHandler>>,
    text: RefCell<TextUpdate>,
    /// Points, as Hyprmux configures them.
    size: Cell<Size<Pixels>>,
    scale: Cell<f32>,
    unacked_serial: Cell<Option<u64>>,
    configured: Cell<bool>,
    frame_pending: Cell<bool>,
    /// When the pending frame request went out.
    frame_requested_at: Cell<Option<std::time::Instant>>,
    // Debug timing.
    gpu_ms: Cell<f64>,
    last_present: Cell<Option<std::time::Instant>>,
    /// A frame was skipped because every buffer was busy.
    starved: Cell<bool>,
    active: Cell<bool>,
    hovered: Cell<bool>,
    mouse: Cell<Point<Pixels>>,
    modifiers: Cell<Modifiers>,
    pressed_button: Cell<Option<MouseButton>>,
    title: RefCell<String>,
    closed: Cell<bool>,
    /// When this tile last got the keyboard, for `window_stack`.
    last_active: Cell<u64>,
}

fn next_activation() -> u64 {
    thread_local!(static COUNTER: Cell<u64> = const { Cell::new(0) });
    COUNTER.with(|c| {
        c.set(c.get() + 1);
        c.get()
    })
}

/// The window whose tile has the keyboard.
pub(crate) fn active_window() -> Option<AnyWindowHandle> {
    let c = CONNECTION.with(|c| c.borrow().clone())?;
    let windows: Vec<_> = c.windows.borrow().values().filter_map(|w| w.upgrade()).collect();
    windows.iter().find(|w| w.active.get()).map(|w| w.handle)
}

/// Windows front to back: the most recently focused first.
pub(crate) fn window_stack() -> Vec<AnyWindowHandle> {
    let Some(c) = CONNECTION.with(|c| c.borrow().clone()) else { return Vec::new() };
    let mut windows: Vec<_> = c.windows.borrow().values().filter_map(|w| w.upgrade()).collect();
    windows.sort_by_key(|w| std::cmp::Reverse(w.last_active.get()));
    windows.iter().map(|w| w.handle).collect()
}

impl WindowCell {
    /// Runs an input event through GPUI. Callbacks are taken out while they run, so
    /// GPUI may call back into the window.
    fn dispatch(&self, input: PlatformInput) -> bool {
        let callback = self.callbacks.borrow_mut().input.take();
        let Some(mut callback) = callback else {
            if debug() {
                log::info!("hyprmux: no input callback");
            }
            return false;
        };
        let handled = !callback(input).propagate;
        let mut cbs = self.callbacks.borrow_mut();
        if cbs.input.is_none() {
            cbs.input = Some(callback);
        }
        handled
    }

    fn configure(&self, width: f64, height: f64, scale: f64, states: &[String], serial: u64) {
        let new_size = size(px(width as f32), px(height as f32));
        let new_scale = if scale > 0.0 { scale as f32 } else { 2.0 };
        let changed = new_size != self.size.get() || new_scale != self.scale.get() || !self.configured.get();
        self.size.set(new_size);
        self.scale.set(new_scale);
        self.unacked_serial.set(Some(serial));
        self.configured.set(true);
        let activated = states.iter().any(|s| s == "activated");
        if activated != self.active.get() {
            self.set_active(activated);
        }
        if changed {
            let pixels = self.pixel_size();
            self.renderer.borrow_mut().update_drawable_size(size(
                gpui::DevicePixels(pixels.0 as i32),
                gpui::DevicePixels(pixels.1 as i32),
            ));
            let callback = self.callbacks.borrow_mut().resize.take();
            if let Some(mut callback) = callback {
                callback(new_size, new_scale);
                self.callbacks.borrow_mut().resize.get_or_insert(callback);
            }
        }
        self.schedule_frame();
    }

    fn pixel_size(&self) -> (usize, usize) {
        let s = self.size.get();
        let scale = self.scale.get();
        ((f32::from(s.width) * scale).round() as usize, (f32::from(s.height) * scale).round() as usize)
    }

    fn schedule_frame(&self) {
        if self.closed.get() || !self.configured.get() {
            return;
        }
        // A request unanswered for half a second is presumed lost: ask again, so a
        // missing callback can't stall the window. Hidden tiles get none, and the
        // extra request just waits with the first.
        let stale = self
            .frame_requested_at
            .get()
            .is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(500));
        if self.frame_pending.get() && !stale {
            return;
        }
        self.frame_pending.set(true);
        self.frame_requested_at.set(Some(std::time::Instant::now()));
        let cb = self.client.request_frame(self.toplevel.surface);
        with_connection(|c| {
            if let Some(me) = c.windows.borrow().get(&self.toplevel.id).cloned() {
                c.frame_callbacks.borrow_mut().insert(cb, me);
            }
        });
    }

    fn frame_done(&self) {
        self.frame_pending.set(false);
        let waited = self.frame_requested_at.take().map(|t| t.elapsed());
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            let start = std::time::Instant::now();
            self.gpu_ms.set(0.0);
            callback(RequestFrameOptions::default());
            if debug() {
                let total = start.elapsed().as_secs_f64() * 1000.0;
                let gpu = self.gpu_ms.get();
                // Only frames that drew: GPUI skips clean windows.
                if gpu > 0.0 {
                    let since = self.last_present.get().map(|t| t.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0);
                    log::info!(
                        "hyprmux: frame cpu {:.1} ms, gpu encode {gpu:.1} ms, waited for callback {:.1} ms, since last present {since:.1} ms",
                        total - gpu,
                        waited.map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
                    );
                }
            }
            self.callbacks.borrow_mut().request_frame.get_or_insert(callback);
        }
    }

    fn buffer_released(&self, id: u64) {
        let mut chain = self.swapchain.borrow_mut();
        if let Some(b) = chain.iter_mut().find(|b| b.id == id) {
            b.busy = false;
        }
        drop(chain);
        if self.starved.replace(false) {
            // The skipped frame: GPUI redraws on the next request.
            let callback = self.callbacks.borrow_mut().request_frame.take();
            if let Some(mut callback) = callback {
                callback(RequestFrameOptions { require_presentation: true, force_render: true });
                self.callbacks.borrow_mut().request_frame.get_or_insert(callback);
            }
        }
    }

    /// A free buffer at the current pixel size. Buffers of an old size are dropped
    /// once released.
    fn acquire(&self, conn: &Connection) -> Option<usize> {
        let (w, h) = self.pixel_size();
        if w == 0 || h == 0 {
            return None;
        }
        let mut chain = self.swapchain.borrow_mut();
        chain.retain(|b| {
            let keep = b.busy || (b.surface.width() == w && b.surface.height() == h);
            if !keep {
                self.client.destroy_buffer(b.id);
                conn.buffers.borrow_mut().remove(&b.id);
            }
            keep
        });
        if let Some(i) = chain.iter().position(|b| !b.busy && b.surface.width() == w && b.surface.height() == h) {
            return Some(i);
        }
        let current = chain.iter().filter(|b| b.surface.width() == w && b.surface.height() == h).count();
        if current >= SWAPCHAIN_LENGTH {
            return None;
        }
        let surface = IOSurface::new(w, h)?;
        let texture = unsafe { self.renderer.borrow().texture_for_iosurface(surface.0, w as u64, h as u64) };
        let id = self.client.register_iosurface(surface.0);
        if let Some(me) = conn.windows.borrow().get(&self.toplevel.id).cloned() {
            conn.buffers.borrow_mut().insert(id, me);
        }
        chain.push(Buffer { id, surface, texture, busy: false });
        Some(chain.len() - 1)
    }

    fn draw(&self, scene: &Scene) {
        if self.closed.get() || !self.configured.get() {
            return;
        }
        let Some(conn) = CONNECTION.with(|c| c.borrow().clone()) else { return };
        let Some(index) = self.acquire(&conn) else {
            self.starved.set(true);
            return;
        };
        let (texture, id) = {
            let chain = self.swapchain.borrow();
            (chain[index].texture.clone(), chain[index].id)
        };
        let gpu_start = std::time::Instant::now();
        let Some(command_buffer) = self.renderer.borrow_mut().draw_to_texture(scene, &texture) else { return };
        self.swapchain.borrow_mut()[index].busy = true;
        // Hyprmux reads the IOSurface as soon as it's committed, so present only once
        // the GPU is done, from its completion handler, without blocking this thread.
        // Frames on one queue complete in order, so they present in order.
        let (toplevel, scale) = (self.toplevel.id, self.scale.get() as f64);
        let block = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2::runtime::AnyObject>| {
            dispatch2::DispatchQueue::main().exec_async(move || present_finished(toplevel, id, scale));
        });
        // SAFETY: both pointee types are opaque views of the same Objective-C block ABI.
        unsafe { command_buffer.add_completed_handler(&*block2::RcBlock::as_ptr(&block).cast()) };
        command_buffer.commit();
        self.gpu_ms.set(gpu_start.elapsed().as_secs_f64() * 1000.0);
    }

    /// The GPU finished a frame: hand it to Hyprmux.
    fn presented(&self, buffer: u64, scale: f64) {
        if self.closed.get() {
            return;
        }
        if let Some(serial) = self.unacked_serial.take() {
            self.client.ack_configure(self.toplevel, serial);
        }
        self.client.present(self.toplevel.surface, buffer, scale, false);
        self.last_present.set(Some(std::time::Instant::now()));
    }

    fn set_active(&self, active: bool) {
        if debug() {
            log::info!("hyprmux: window {} active={active}", self.toplevel.id);
        }
        if active {
            self.last_active.set(next_activation());
        }
        if self.active.replace(active) == active {
            return;
        }
        let callback = self.callbacks.borrow_mut().active_status_change.take();
        if let Some(mut callback) = callback {
            callback(active);
            self.callbacks.borrow_mut().active_status_change.get_or_insert(callback);
        }
    }

    fn set_hovered(&self, hovered: bool) {
        if self.hovered.replace(hovered) == hovered {
            return;
        }
        let callback = self.callbacks.borrow_mut().hover_status_change.take();
        if let Some(mut callback) = callback {
            callback(hovered);
            self.callbacks.borrow_mut().hover_status_change.get_or_insert(callback);
        }
    }

    // MARK: Input

    fn pointer_enter(&self, x: f64, y: f64) {
        self.set_hovered(true);
        self.pointer_motion(x, y, 0);
    }

    fn pointer_leave(&self) {
        self.set_hovered(false);
        self.dispatch(PlatformInput::MouseExited(MouseExitEvent {
            position: self.mouse.get(),
            pressed_button: None,
            modifiers: self.modifiers.get(),
        }));
    }

    fn pointer_motion(&self, x: f64, y: f64, modifiers: u64) {
        let position = point(px(x as f32), px(y as f32));
        self.mouse.set(position);
        if modifiers != 0 {
            self.modifiers.set(modifiers_from_bits(modifiers));
        }
        self.dispatch(PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: self.pressed_button.get(),
            modifiers: self.modifiers.get(),
        }));
    }

    fn pointer_button(&self, x: f64, y: f64, button: u64, down: bool, click_count: u64, modifiers: u64) {
        let button = match button {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            3 => MouseButton::Navigate(NavigationDirection::Back),
            4 => MouseButton::Navigate(NavigationDirection::Forward),
            _ => return,
        };
        let position = point(px(x as f32), px(y as f32));
        self.mouse.set(position);
        let modifiers = modifiers_from_bits(modifiers);
        self.modifiers.set(modifiers);
        let click_count = click_count.max(1) as usize;
        if down {
            self.pressed_button.set(Some(button));
            self.dispatch(PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count,
                first_mouse: false,
            }));
        } else {
            self.pressed_button.set(None);
            self.dispatch(PlatformInput::MouseUp(MouseUpEvent { button, position, modifiers, click_count }));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scroll(&self, x: f64, y: f64, dx: f64, dy: f64, precise: bool, phase: u64, modifiers: u64) {
        let position = point(px(x as f32), px(y as f32));
        self.mouse.set(position);
        // NSEventPhase bits: began 1, stationary 2, changed 4, ended 8, cancelled 16, mayBegin 32.
        let touch_phase = if phase & (1 | 32) != 0 {
            TouchPhase::Started
        } else if phase & 8 != 0 {
            TouchPhase::Ended
        } else {
            TouchPhase::Moved
        };
        let raw = point(dx as f32, dy as f32);
        let delta = if precise { ScrollDelta::Pixels(raw.map(px)) } else { ScrollDelta::Lines(raw) };
        self.dispatch(PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta,
            touch_phase,
            modifiers: modifiers_from_bits(modifiers),
        }));
    }

    fn modifiers_changed(&self, bits: u64) {
        let modifiers = modifiers_from_bits(bits);
        self.modifiers.set(modifiers);
        self.dispatch(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            capslock: Capslock { on: bits & (1 << 16) != 0 },
        }));
    }

    /// Keys arrive as AppKit's own fields, so an NSEvent built from them goes through
    /// the same keystroke parsing as a native window. Keys the input method used
    /// never arrive here; they come as text input.
    fn key(&self, key_code: u16, down: bool, repeat: bool, characters: &str, ignoring: &str, modifiers: u64) {
        let input = native_key_event(key_code, down, repeat, characters, ignoring, modifiers);
        if debug() {
            log::info!("hyprmux: key {key_code} down={down} chars={characters:?} mods={modifiers:#x} -> {input:?}");
        }
        let Some(input) = input else { return };
        match input {
            PlatformInput::KeyDown(event) => {
                let key_char = event.keystroke.key_char.clone();
                let m = event.keystroke.modifiers;
                let handled = self.dispatch(PlatformInput::KeyDown(event));
                if debug() {
                    log::info!("hyprmux: key down handled={handled} has_input_handler={}", self.input_handler.borrow().is_some());
                }
                if handled {
                    return;
                }
                // Unbound printable keys insert text, as -insertText: does in a native window.
                if let Some(text) = key_char.filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control())) {
                    if !m.control && !m.platform && !m.function {
                        self.with_input_handler(|h| h.replace_text_in_range(None, &text));
                    }
                }
            }
            other => {
                self.dispatch(other);
            }
        }
    }

    fn with_input_handler<R>(&self, f: impl FnOnce(&mut PlatformInputHandler) -> R) -> Option<R> {
        let handler = self.input_handler.borrow_mut().take();
        let mut handler = handler?;
        let result = f(&mut handler);
        let mut slot = self.input_handler.borrow_mut();
        if slot.is_none() {
            *slot = Some(handler);
        }
        Some(result)
    }

    /// One `text_input.done`: delete around the caret, commit, then show the preedit.
    fn apply_text_input(&self) {
        let update = std::mem::take(&mut *self.text.borrow_mut());
        self.with_input_handler(|h| {
            if update.delete_before > 0 || update.delete_after > 0 {
                if let Some(sel) = h.selected_text_range(false) {
                    let start = sel.range.start.saturating_sub(update.delete_before);
                    let end = sel.range.end + update.delete_after;
                    h.replace_text_in_range(Some(start..end), "");
                }
            }
            if !update.commit.is_empty() {
                h.replace_text_in_range(None, &update.commit);
            }
            match &update.preedit {
                Some((text, cursor)) if !text.is_empty() => {
                    h.replace_and_mark_text_in_range(None, text, Some(cursor.clone()));
                }
                _ => {
                    if update.commit.is_empty() && h.marked_text_range().is_some() {
                        // The composition was cancelled.
                        h.replace_text_in_range(None, "");
                    }
                }
            }
        });
    }

    fn close_requested(&self) {
        let callback = self.callbacks.borrow_mut().should_close.take();
        let close = match callback {
            Some(mut callback) => {
                let ok = callback();
                self.callbacks.borrow_mut().should_close.get_or_insert(callback);
                ok
            }
            None => true,
        };
        if close {
            self.closed_by_compositor();
        }
    }

    /// The tile is gone (closed, or Hyprmux quit): tell GPUI, once.
    fn closed_by_compositor(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.client.destroy_toplevel(self.toplevel);
        let callback = self.callbacks.borrow_mut().close.take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// An app Hyprmux launched quits when its last tile has been gone a few seconds:
/// closing the last tile means the user is done, and a windowless app would linger.
/// The grace period lets an app replace a window.
fn quit_if_no_windows_soon() {
    if !hyprmux_client::launched_by_hyprmux() {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        dispatch2::DispatchQueue::main().exec_async(|| {
            let empty = CONNECTION.with(|c| c.borrow().as_ref().is_none_or(|c| c.windows.borrow().values().all(|w| w.upgrade().is_none())));
            if empty {
                log::info!("hyprmux: no windows left, quitting");
                unsafe {
                    let app: id = msg_send![class!(NSApplication), sharedApplication];
                    let _: () = msg_send![app, terminate: nil];
                }
            }
        });
    });
}

/// Main-queue half of a frame's completion handler.
fn present_finished(toplevel: u64, buffer: u64, scale: f64) {
    let Some(c) = CONNECTION.with(|c| c.borrow().clone()) else { return };
    let w = c.windows.borrow().get(&toplevel).and_then(|w| w.upgrade());
    if let Some(w) = w {
        w.presented(buffer, scale);
    }
}

fn modifiers_from_bits(bits: u64) -> Modifiers {
    Modifiers {
        control: bits & (1 << 18) != 0,
        alt: bits & (1 << 19) != 0,
        shift: bits & (1 << 17) != 0,
        platform: bits & (1 << 20) != 0,
        function: bits & (1 << 23) != 0,
    }
}

/// An NSEvent with the key's own fields, parsed like a native key event.
fn native_key_event(key_code: u16, down: bool, repeat: bool, characters: &str, ignoring: &str, modifiers: u64) -> Option<PlatformInput> {
    unsafe {
        let event_type = if down { NSEventType::NSKeyDown } else { NSEventType::NSKeyUp };
        let chars = NSString::alloc(nil).init_str(characters);
        let ignoring = NSString::alloc(nil).init_str(ignoring);
        let event: id = msg_send![class!(NSEvent),
            keyEventWithType: event_type
            location: NSPoint::new(0., 0.)
            modifierFlags: modifiers as usize
            timestamp: 0f64
            windowNumber: 0isize
            context: nil
            characters: chars
            charactersIgnoringModifiers: ignoring
            isARepeat: repeat as cocoa::base::BOOL
            keyCode: key_code];
        let _: () = msg_send![chars, release];
        let _: () = msg_send![ignoring, release];
        if event == nil {
            return None;
        }
        crate::events::platform_input_from_native(event, None)
    }
}

// MARK: PlatformWindow

pub(crate) struct HyprmuxWindow(Rc<WindowCell>);

impl HyprmuxWindow {
    pub(crate) fn open(handle: AnyWindowHandle, options: &WindowParams, renderer_context: renderer::Context) -> Result<Self> {
        let conn = connection()?;
        let title = options
            .titlebar
            .as_ref()
            .and_then(|t| t.title.as_ref())
            .map(|t| t.to_string())
            .unwrap_or_default();
        let toplevel = conn.client.create_toplevel(if title.is_empty() { "Zed" } else { &title }, None);
        // GPUI draws its own compositions, so preedit stays with the client.
        conn.client.enable_text_input(toplevel.surface, false);
        let cell = Rc::new(WindowCell {
            handle,
            toplevel,
            client: conn.client.clone(),
            renderer: RefCell::new(renderer::Renderer::new_offscreen(renderer_context, false)),
            swapchain: RefCell::default(),
            callbacks: RefCell::default(),
            input_handler: RefCell::default(),
            text: RefCell::default(),
            size: Cell::new(options.bounds.size),
            scale: Cell::new(2.0),
            unacked_serial: Cell::new(None),
            configured: Cell::new(false),
            frame_pending: Cell::new(false),
            frame_requested_at: Cell::new(None),
            gpu_ms: Cell::new(0.0),
            last_present: Cell::new(None),
            starved: Cell::new(false),
            active: Cell::new(false),
            hovered: Cell::new(false),
            mouse: Cell::new(Point::default()),
            modifiers: Cell::new(Modifiers::default()),
            pressed_button: Cell::new(None),
            title: RefCell::new(title),
            closed: Cell::new(false),
            last_active: Cell::new(0),
        });
        conn.windows.borrow_mut().insert(toplevel.id, Rc::downgrade(&cell));
        Ok(HyprmuxWindow(cell))
    }
}

impl Drop for HyprmuxWindow {
    fn drop(&mut self) {
        let w = &self.0;
        if !w.closed.replace(true) {
            w.client.destroy_toplevel(w.toplevel);
        }
        for b in w.swapchain.borrow().iter() {
            w.client.destroy_buffer(b.id);
        }
        quit_if_no_windows_soon();
        with_connection(|c| {
            c.windows.borrow_mut().remove(&w.toplevel.id);
            let ids: Vec<u64> = w.swapchain.borrow().iter().map(|b| b.id).collect();
            let mut buffers = c.buffers.borrow_mut();
            for id in ids {
                buffers.remove(&id);
            }
        });
    }
}

impl rwh::HasWindowHandle for HyprmuxWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        Err(rwh::HandleError::NotSupported)
    }
}

impl rwh::HasDisplayHandle for HyprmuxWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(rwh::DisplayHandle::appkit())
    }
}

impl PlatformWindow for HyprmuxWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(Point::default(), self.0.size.get())
    }
    fn is_maximized(&self) -> bool {
        false
    }
    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }
    fn content_size(&self) -> Size<Pixels> {
        self.0.size.get()
    }
    /// Hyprmux owns tile sizes; configure events bring the new size.
    fn resize(&mut self, _size: Size<Pixels>) {}
    fn scale_factor(&self) -> f32 {
        self.0.scale.get()
    }
    fn appearance(&self) -> WindowAppearance {
        unsafe {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            let appearance: id = msg_send![app, effectiveAppearance];
            crate::window_appearance::window_appearance_from_native(appearance)
        }
    }
    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(MacDisplay::primary()))
    }
    fn mouse_position(&self) -> Point<Pixels> {
        self.0.mouse.get()
    }
    fn modifiers(&self) -> Modifiers {
        self.0.modifiers.get()
    }
    fn capslock(&self) -> Capslock {
        Capslock::default()
    }
    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.0.input_handler.borrow_mut() = Some(input_handler);
    }
    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.input_handler.borrow_mut().take()
    }
    fn prompt(&self, level: PromptLevel, msg: &str, detail: Option<&str>, answers: &[PromptButton]) -> Option<oneshot::Receiver<usize>> {
        let conn = CONNECTION.with(|c| c.borrow().clone())?;
        let kind = match level {
            PromptLevel::Info => "info",
            PromptLevel::Warning => "warning",
            PromptLevel::Critical => "error",
        };
        let buttons: Vec<String> = answers.iter().map(|a| a.label().to_string()).collect();
        let cancel = answers.iter().position(|a| a.is_cancel());
        let mut request = serde_json::json!({
            "type": kind, "message": msg, "detail": detail.unwrap_or(""), "buttons": buttons, "defaultId": 0,
        });
        if let Some(cancel) = cancel {
            request["cancelId"] = serde_json::Value::from(cancel);
        }
        let options = request.to_string();
        let id = conn.client.open_dialog(self.0.toplevel.surface, "message", &options);
        let (tx, rx) = oneshot::channel();
        let fallback = cancel.unwrap_or(0);
        conn.dialogs.borrow_mut().insert(id, Box::new(move |json: String| {
            let result: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            let response = result["response"].as_u64().map(|n| n as usize).unwrap_or(fallback);
            let _ = tx.send(response);
        }));
        Some(rx)
    }
    fn activate(&self) {}
    fn is_active(&self) -> bool {
        self.0.active.get()
    }
    fn visibility(&self) -> WindowVisibility {
        WindowVisibility::Visible
    }
    fn is_hovered(&self) -> bool {
        self.0.hovered.get()
    }
    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }
    fn set_title(&mut self, title: &str) {
        if *self.0.title.borrow() == title {
            return;
        }
        *self.0.title.borrow_mut() = title.to_string();
        self.0.client.set_title(self.0.toplevel, title);
    }
    fn get_title(&self) -> String {
        self.0.title.borrow().clone()
    }
    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {}
    fn minimize(&self) {}
    fn zoom(&self) {}
    fn toggle_fullscreen(&self) {}
    fn is_fullscreen(&self) -> bool {
        false
    }
    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let weak = Rc::downgrade(&self.0);
        Some(Rc::new(move || {
            if let Some(w) = weak.upgrade() {
                w.schedule_frame();
            }
        }))
    }
    fn schedule_frame(&self) {
        self.0.schedule_frame();
    }
    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }
    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }
    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active_status_change = Some(callback);
    }
    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.0.callbacks.borrow_mut().visibility_change = Some(callback);
    }
    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().hover_status_change = Some(callback);
    }
    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }
    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}
    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.callbacks.borrow_mut().should_close = Some(callback);
    }
    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {}
    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.callbacks.borrow_mut().close = Some(callback);
    }
    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().appearance_changed = Some(callback);
    }
    fn draw(&self, scene: &Scene) {
        self.0.draw(scene);
    }
    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.renderer.borrow().sprite_atlas().clone() as Arc<dyn PlatformAtlas>
    }
    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }
    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        self.0.client.set_text_cursor_rect(
            self.0.toplevel.surface,
            f32::from(bounds.origin.x) as f64,
            f32::from(bounds.origin.y) as f64,
            f32::from(bounds.size.width) as f64,
            f32::from(bounds.size.height) as f64,
        );
    }
}
