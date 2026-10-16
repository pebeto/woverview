//! The overlay.
//!
//! A layer surface holds the backdrop, and every window picture, title and
//! rounded corner sits on a subsurface of its own. The compositor scales,
//! moves and fades those on the GPU; woverview never composes a frame. One
//! number drives the animation: `progress`, 0 where every window sits where
//! sway put it and 1 where it sits in its slot in the grid. Each step only
//! tells the compositor where things go.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use memmap2::Mmap;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::Sender as LoopSender;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::cursor_shape::CursorShapeManager;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer as PoolBuffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle, delegate_noop};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::capture::{Control, Damage, Frame, Pixels, bytes_per_pixel};
use crate::debug::debug;
use crate::layout::{self, Rect};
use crate::paint::{self, Image};
use crate::shrink::Shrunk;
use crate::sway::{self, Window};

/// Gap between the screen edge and the thumbnails, as a fraction of the
/// screen height. `SPACING` is the gap between thumbnails, same unit.
const MARGIN: f64 = 0.05;
const SPACING: f64 = 0.03;
/// Even a lone window shrinks this much, so the overview reads as zoomed out.
const MAX_SCALE: f64 = 0.8;
/// Corner radius and selection ring width, in logical pixels.
const RADIUS: u32 = 12;
const RING: u32 = 4;
/// The title line under each thumbnail, and the gap above and below it.
const LABEL_LINE: u32 = 24;
const LABEL_GAP: u32 = 10;

/// How long a full open or close takes, in seconds. Partial ones, like
/// closing while still opening, take a share of it.
const OPEN_TIME: f64 = 0.25;
const CLOSE_TIME: f64 = 0.2;
/// How small a window that fades in starts out, next to its slot size.
const FADE_SCALE: f64 = 0.85;
/// The faintest anything gets, rather than fully clear, so the compositor
/// keeps treating it as on screen. One step of 255.
const FAINTEST: f64 = 1.0 / 255.0;

const BACKDROP: (f64, f64, f64) = (0.05, 0.05, 0.07);
const BACKDROP_ALPHA: f64 = 1.0;
const PLACEHOLDER_ALPHA: f64 = 0.08;
const ACCENT: (f64, f64, f64) = (0.35, 0.6, 1.0);

/// A subsurface the compositor scales and fades for us.
struct Piece {
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    alpha: WpAlphaModifierSurfaceV1,
}

impl Piece {
    /// Puts the piece at `rect` (logical pixels, on the overlay), at `opacity`.
    fn place(&self, rect: Rect, opacity: f64) {
        self.subsurface.set_position(rect.x.round() as i32, rect.y.round() as i32);
        self.viewport.set_destination((rect.width.round() as i32).max(1), (rect.height.round() as i32).max(1));
        self.alpha.set_multiplier(multiplier(opacity));
        self.surface.commit();
    }

    /// Shows `buffer`, redrawing only `damage` if given.
    fn show(&self, buffer: &WlBuffer, damage: Option<&[Damage]>) {
        self.surface.attach(Some(buffer), 0, 0);
        self.damage(damage);
    }

    fn show_pooled(&self, buffer: &PoolBuffer, damage: Option<&[Damage]>) {
        if buffer.attach_to(&self.surface).is_ok() {
            self.damage(damage);
        }
    }

    fn damage(&self, damage: Option<&[Damage]>) {
        match damage {
            Some(rects) => rects.iter().for_each(|d| self.surface.damage_buffer(d.x, d.y, d.width, d.height)),
            None => self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX),
        }
        self.surface.commit();
    }

    /// Puts this piece on top of `other`.
    fn raise_above(&self, other: &WlSurface) {
        self.subsurface.place_above(other);
    }
}

/// The root surface's own content: one pixel, stretched over the output
/// and faded in with progress.
struct Backdrop {
    viewport: WpViewport,
    alpha: WpAlphaModifierSurfaceV1,
}

fn multiplier(opacity: f64) -> u32 {
    (opacity.clamp(FAINTEST, 1.0) * u32::MAX as f64) as u32
}

/// What a thumbnail shows.
enum Content {
    Nothing,
    /// The compositor's own capture buffer, shown as is. `map` lets us
    /// shrink it once the opening is over.
    Shared { buffer: WlBuffer, map: Mmap, width: u32, height: u32, stride: u32, format: wl_shm::Format },
    Pooled { buffer: PoolBuffer, width: u32, height: u32 },
}

impl Content {
    fn size(&self) -> Option<(u32, u32)> {
        match self {
            Content::Nothing => None,
            Content::Shared { width, height, .. } | Content::Pooled { width, height, .. } => Some((*width, *height)),
        }
    }
}

impl Drop for Content {
    fn drop(&mut self) {
        if let Content::Shared { buffer, .. } = self {
            buffer.destroy();
        }
    }
}

struct Thumb {
    window: Window,
    /// Where the window is on screen when the overview opens, in overlay
    /// coordinates. `None` if it isn't showing.
    home: Option<Rect>,
    /// Where it flies from while opening and to while closing. `None`
    /// makes it fade instead.
    anchor: Option<Rect>,
    /// Its place in the grid.
    slot: Rect,
    picture: Piece,
    content: Content,
    corners: [Piece; 4],
    label: Piece,
    label_buffer: Option<WlBuffer>,
    /// The width the title was last asked for at.
    label_width: u32,
}

impl Thumb {
    /// The size the layout works with: the window's width on screen, and the
    /// shape of what we captured. A client on a hidden workspace may not
    /// have redrawn since sway resized it, so the two can disagree, and
    /// stretching the old buffer to the new shape would distort it.
    fn natural_size(&self) -> (f64, f64) {
        let rect = self.window.rect;
        let on_screen = rect.width >= 1.0 && rect.height >= 1.0;
        match self.content.size() {
            Some((w, h)) if on_screen => (rect.width, rect.width * h as f64 / w as f64),
            Some((w, h)) => (w as f64, h as f64),
            None if on_screen => (rect.width, rect.height),
            None => (1280.0, 800.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Up but showing nothing, until the windows on screen have arrived.
    Waiting,
    Opening,
    Open,
    Closing,
    Closed,
}

struct Animation {
    from: f64,
    to: f64,
    start: Option<Instant>,
    duration: f64,
    /// Whether the frame showing where it starts has gone out. See `tick`.
    primed: bool,
}

/// A title for the painter thread to draw.
pub struct LabelJob {
    index: usize,
    workspace: String,
    title: String,
    width: u32,
    scale: f64,
}

pub struct Label {
    index: usize,
    width: u32,
    image: Image,
}

/// Draws titles on a thread of its own: setting up fonts takes a moment,
/// and the opening shouldn't wait for it.
pub fn spawn_painter(results: LoopSender<Label>) -> mpsc::Sender<LabelJob> {
    let (jobs, queue) = mpsc::channel::<LabelJob>();
    std::thread::spawn(move || {
        for job in queue {
            let image = paint::label(&job.workspace, &job.title, job.width, LABEL_LINE, job.scale);
            if results.send(Label { index: job.index, width: job.width, image }).is_err() {
                return;
            }
        }
    });
    jobs
}

pub struct Ui {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    shm: Shm,
    single_pixel: WpSinglePixelBufferManagerV1,
    pool: SlotPool,
    cursor_shapes: Option<CursorShapeManager>,
    cursor: Option<WpCursorShapeDeviceV1>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    qh: QueueHandle<Ui>,

    layer: LayerSurface,
    backdrop: Backdrop,
    placeholder: WlBuffer,
    corner_buffers: Vec<WlBuffer>,
    thumbs: Vec<Thumb>,
    ring: Piece,
    /// The ring as last drawn, and the size it was drawn for.
    ring_image: Option<((u32, u32), PoolBuffer)>,

    /// The output's size and top-left corner, in layout coordinates, and
    /// its pixels per logical pixel.
    size: (f64, f64),
    origin: (f64, f64),
    scale: f64,
    control: Control,
    painter: mpsc::Sender<LabelJob>,

    selected: Option<usize>,
    picked: Option<usize>,
    phase: Phase,
    progress: f64,
    animation: Option<Animation>,
    last_frame: Option<Instant>,
    configured: bool,
    waited_enough: bool,
    pub done: bool,
}

impl Ui {
    pub fn new(
        globals: &GlobalList,
        qh: &QueueHandle<Ui>,
        windows: Vec<Window>,
        snapshot: &sway::Snapshot,
        control: Control,
        painter: mpsc::Sender<LabelJob>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let compositor = CompositorState::bind(globals, qh)?;
        let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), globals, qh)?;
        let shm = Shm::bind(globals, qh)?;
        let layer_shell = LayerShell::bind(globals, qh)?;
        let viewporter: WpViewporter = globals.bind(qh, 1..=1, ())?;
        let single_pixel: WpSinglePixelBufferManagerV1 = globals.bind(qh, 1..=1, ())?;
        let alpha: WpAlphaModifierV1 = globals.bind(qh, 1..=1, ())?;
        let pool = SlotPool::new(4 << 20, &shm)?;
        let empty = Region::new(&compositor)?;

        // Leaving out the output lets sway put the overlay where the user is.
        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some("woverview"), None::<&WlOutput>);
        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        let root = layer.wl_surface().clone();

        let new_piece = || {
            let (subsurface, surface) = subcompositor.create_subsurface(root.clone(), qh);
            // Pointer events all go to the overlay itself.
            surface.set_input_region(Some(empty.wl_region()));
            let viewport = viewporter.get_viewport(&surface, qh, ());
            let alpha = alpha.get_surface(&surface, qh, ());
            Piece { surface, subsurface, viewport, alpha }
        };
        let backdrop = Backdrop { viewport: viewporter.get_viewport(&root, qh, ()), alpha: alpha.get_surface(&root, qh, ()) };
        let channel = |v: f64| (v * u32::MAX as f64) as u32;
        let placeholder = single_pixel.create_u32_rgba_buffer(
            channel(PLACEHOLDER_ALPHA),
            channel(PLACEHOLDER_ALPHA),
            channel(PLACEHOLDER_ALPHA),
            channel(PLACEHOLDER_ALPHA),
            qh,
            (),
        );

        let scale = snapshot.focused_output_scale;
        let corner_buffers = (0..4)
            .map(|i| static_buffer(&shm, &paint::corner(i, RADIUS, BACKDROP, scale), qh))
            .collect::<io::Result<Vec<_>>>()?;

        // Subsurfaces stack in the order they're made. Windows fading in or
        // out go at the bottom; ones coming from or going to the desktop
        // stack the way sway does, tiled under floating.
        let origin = (snapshot.focused_output_rect.x, snapshot.focused_output_rect.y);
        let selected = windows.iter().position(|w| w.focused);
        let mut order: Vec<usize> = (0..windows.len()).collect();
        order.sort_by_key(|&i| (windows[i].visible, windows[i].floating));
        let mut made: Vec<Option<Thumb>> = windows.iter().map(|_| None).collect();
        for i in order {
            let window = windows[i].clone();
            let home = window.visible.then(|| on_overlay(&window, origin)).flatten();
            made[i] = Some(Thumb {
                window,
                home,
                anchor: home,
                slot: Rect::default(),
                picture: new_piece(),
                content: Content::Nothing,
                corners: [new_piece(), new_piece(), new_piece(), new_piece()],
                label: new_piece(),
                label_buffer: None,
                label_width: 0,
            });
        }
        let thumbs = made.into_iter().map(Option::unwrap).collect();
        let ring = new_piece();

        let mut ui = Self {
            registry: RegistryState::new(globals),
            seats: SeatState::new(globals, qh),
            outputs: OutputState::new(globals, qh),
            cursor_shapes: CursorShapeManager::bind(globals, qh).ok(),
            shm,
            single_pixel,
            pool,
            cursor: None,
            keyboard: None,
            pointer: None,
            qh: qh.clone(),
            layer,
            backdrop,
            placeholder,
            corner_buffers,
            thumbs,
            ring,
            ring_image: None,
            size: (snapshot.focused_output_rect.width, snapshot.focused_output_rect.height),
            origin,
            scale,
            control,
            painter,
            selected,
            picked: None,
            phase: Phase::Waiting,
            progress: 0.0,
            animation: None,
            last_frame: None,
            configured: false,
            waited_enough: false,
            done: false,
        };
        ui.control.pause(true);
        ui.relayout();
        Ok(ui)
    }

    pub fn thumb_count(&self) -> usize {
        self.thumbs.len()
    }

    /// Closes without picking anything.
    pub fn dismiss(&mut self) {
        self.close(None);
    }

    /// The opening has waited long enough for windows on screen.
    pub fn stop_waiting(&mut self) {
        if self.phase == Phase::Waiting {
            let late = self.thumbs.iter().filter(|t| t.home.is_some() && t.content.size().is_none()).count();
            debug!("stopped waiting for {late} windows on screen");
        }
        self.waited_enough = true;
        self.start_if_ready();
    }

    /// Shows a new capture of a window.
    pub fn frame_arrived(&mut self, frame: Frame) {
        let Some(index) = self.thumbs.iter().position(|t| t.window.identifier == frame.identifier) else {
            return;
        };
        let size_before = self.thumbs[index].natural_size();
        let (width, height, stride, format) = (frame.width, frame.height, frame.stride, frame.format);
        let content = match frame.pixels {
            // Only 4-byte formats go to sway as they are; see
            // `capture::Capture::take_frame` for what 3-byte ones do to it.
            Pixels::Shared { map, fd } if bytes_per_pixel(format) == 4 && self.shm.formats().contains(&format) => {
                let pool = self.shm.wl_shm().create_pool(fd.as_fd(), map.len() as i32, &self.qh, ());
                let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &self.qh, ());
                pool.destroy();
                Content::Shared { buffer, map, width, height, stride, format }
            }
            pixels => {
                // Sway can't safely show this format as is, or it's a
                // shrunk copy already.
                let (pixels, stride, format) = match pixels {
                    Pixels::Owned(bytes) => (bytes, stride, format),
                    Pixels::Shared { map, .. } => {
                        let shrunk = Shrunk::new(&map, width, height, stride, format, 1);
                        let format = shrunk.format();
                        (shrunk.pixels, width * 4, format)
                    }
                };
                let Some(buffer) = self.pooled(width, height, stride, format, &pixels) else {
                    return;
                };
                Content::Pooled { buffer, width, height }
            }
        };
        // Swap the picture before letting go of the old one.
        let thumb = &mut self.thumbs[index];
        let old = std::mem::replace(&mut thumb.content, content);
        if self.phase != Phase::Waiting {
            show(thumb, &self.placeholder, frame.damage.as_deref());
            self.layer.wl_surface().commit();
        }
        drop(old);
        if self.thumbs[index].natural_size() != size_before {
            self.relayout();
            if self.phase != Phase::Waiting {
                self.render();
            }
        }
        self.start_if_ready();
    }

    pub fn label_arrived(&mut self, label: Label) {
        let Some(thumb) = self.thumbs.get_mut(label.index) else {
            return;
        };
        if label.width != thumb.label_width {
            return;
        }
        let Ok(buffer) = static_buffer(&self.shm, &label.image, &self.qh) else {
            return;
        };
        if let Some(old) = thumb.label_buffer.replace(buffer) {
            old.destroy();
        }
        if self.phase != Phase::Waiting {
            thumb.label.show(thumb.label_buffer.as_ref().unwrap(), None);
            self.render();
        }
    }

    fn start_if_ready(&mut self) {
        let arrived = self.thumbs.iter().all(|t| t.home.is_none() || t.content.size().is_some());
        if self.phase == Phase::Waiting && self.configured && (arrived || self.waited_enough) {
            self.start_opening();
        }
    }

    fn start_opening(&mut self) {
        debug!("opening");
        for thumb in &self.thumbs {
            show(thumb, &self.placeholder, None);
            for (piece, buffer) in thumb.corners.iter().zip(&self.corner_buffers) {
                piece.show(buffer, None);
            }
            if let Some(label) = &thumb.label_buffer {
                thumb.label.show(label, None);
            }
        }
        self.draw_ring();
        self.phase = Phase::Opening;
        self.animate(1.0, true);
        self.render();
    }

    /// Closes, flying the picked window, if any, to where it is on its
    /// workspace. Sway switches there right away, underneath, so the
    /// desktop the overlay fades out to is the one the user picked.
    fn close(&mut self, picked: Option<usize>) {
        match self.phase {
            Phase::Closed | Phase::Closing => return,
            Phase::Waiting => {
                // Nothing on screen yet, so nothing to animate.
                self.phase = Phase::Closing;
                self.progress = 0.0;
            }
            Phase::Opening | Phase::Open => {
                debug!("closing");
                self.phase = Phase::Closing;
                self.animate(0.0, false);
            }
        }
        self.picked = picked;
        self.control.pause(true);

        let destination = picked.map(|i| self.thumbs[i].window.workspace.clone());
        for (i, thumb) in self.thumbs.iter_mut().enumerate() {
            // What will be on screen after closing goes back where it came
            // from; the rest fades out.
            thumb.anchor = match &destination {
                None => thumb.home,
                Some(_) if picked == Some(i) => on_overlay(&thumb.window, self.origin),
                Some(workspace) if *workspace == thumb.window.workspace => thumb.home,
                Some(_) => None,
            };
        }
        if let Some(i) = picked {
            // The picked window goes on top of everything on its way.
            let thumb = &self.thumbs[i];
            thumb.picture.raise_above(&self.ring.surface);
            for corner in &thumb.corners {
                corner.raise_above(&thumb.picture.surface);
            }
            thumb.label.raise_above(&thumb.corners[3].surface);
            let con_id = thumb.window.con_id;
            if let Err(err) = sway::Ipc::connect().and_then(|mut ipc| ipc.focus(con_id)) {
                eprintln!("woverview: {err}");
            }
        }
        if self.animation.is_none() {
            self.arrived();
        } else {
            self.render();
        }
    }

    /// Runs `progress` to `to`. With `prime`, the clock only starts after
    /// one frame at the current progress has gone out.
    fn animate(&mut self, to: f64, prime: bool) {
        let from = self.progress;
        let full = if to > from { OPEN_TIME } else { CLOSE_TIME };
        let duration = full * (to - from).abs();
        self.animation = Some(Animation { from, to, start: None, duration, primed: !prime });
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let Some(running) = self.animation.as_mut() else {
            return;
        };
        // The first frame of the opening is where the compositor takes in
        // every new picture, which can take a couple of refreshes. Spend it
        // showing the starting point, which looks just like the desktop, and
        // start the clock after; otherwise the animation stutters right
        // where it moves fastest.
        if !running.primed {
            running.primed = true;
            self.render();
            return;
        }
        let start = *running.start.get_or_insert(now);
        if let Some(last) = self.last_frame.replace(now)
            && now - last > Duration::from_millis(25)
        {
            debug!("slow frame: {:.0} ms", (now - last).as_secs_f64() * 1e3);
        }
        let t = if running.duration > 0.0 { ((now - start).as_secs_f64() / running.duration).min(1.0) } else { 1.0 };
        // Ease out: quick to leave, gentle to land.
        let eased = 1.0 - (1.0 - t).powi(3);
        self.progress = running.from + (running.to - running.from) * eased;
        if t >= 1.0 {
            self.animation = None;
            self.last_frame = None;
        }
        self.render();
        if self.animation.is_none() {
            self.arrived();
        }
    }

    /// The animation reached its end.
    fn arrived(&mut self) {
        match self.phase {
            Phase::Opening => {
                debug!("open");
                self.phase = Phase::Open;
                self.sharpen();
                self.control.pause(false);
            }
            Phase::Closing => {
                debug!("closed");
                self.phase = Phase::Closed;
                self.done = true;
            }
            _ => {}
        }
    }

    /// Windows that were on screen opened with their full-size picture, which
    /// the compositor shrinks with plain bilinear filtering: text comes out
    /// jagged. Now that they sit still, swap in a properly shrunk copy.
    fn sharpen(&mut self) {
        let Some(factor) = self.control.zoom.factor() else {
            return;
        };
        for index in 0..self.thumbs.len() {
            let Content::Shared { map, width, height, stride, format, .. } = &self.thumbs[index].content else {
                continue;
            };
            let shrunk = Shrunk::new(map, *width, *height, *stride, *format, factor);
            let format = shrunk.format();
            let Some(buffer) = self.pooled(shrunk.width, shrunk.height, shrunk.width * 4, format, &shrunk.pixels) else {
                continue;
            };
            let thumb = &mut self.thumbs[index];
            let old = std::mem::replace(&mut thumb.content, Content::Pooled { buffer, width: shrunk.width, height: shrunk.height });
            show(thumb, &self.placeholder, None);
            drop(old);
        }
        self.layer.wl_surface().commit();
    }

    /// Tells the compositor where everything goes at the current progress.
    fn render(&mut self) {
        let p = self.progress;
        self.backdrop.alpha.set_multiplier(multiplier(BACKDROP_ALPHA * p));
        let mut selected_rect = None;
        for (i, thumb) in self.thumbs.iter().enumerate() {
            let (rect, opacity) = match thumb.anchor {
                Some(anchor) => (lerp(anchor, thumb.slot, p), 1.0),
                None => (scale_about_center(thumb.slot, FADE_SCALE + (1.0 - FADE_SCALE) * p), p),
            };
            thumb.picture.place(rect, opacity);
            // Real windows have square corners, so the rounding comes in
            // with progress, as the backdrop does.
            let r = RADIUS as f64;
            let corners = [
                (rect.x, rect.y),
                (rect.x + rect.width - r, rect.y),
                (rect.x + rect.width - r, rect.y + rect.height - r),
                (rect.x, rect.y + rect.height - r),
            ];
            for (piece, (x, y)) in thumb.corners.iter().zip(corners) {
                piece.place(Rect { x, y, width: r, height: r }, opacity * BACKDROP_ALPHA * p);
            }
            // Titles only come in over the last stretch, once things settle.
            let label_width = thumb.slot.width;
            let label = Rect {
                x: rect.x + (rect.width - label_width) / 2.0,
                y: rect.y + rect.height + LABEL_GAP as f64,
                width: label_width,
                height: LABEL_LINE as f64,
            };
            thumb.label.place(label, opacity * ((p - 0.6) / 0.4).clamp(0.0, 1.0));
            if self.selected == Some(i) {
                selected_rect = Some(rect);
            }
        }
        if let Some(rect) = selected_rect {
            let ring = RING as f64;
            let around = Rect { x: rect.x - ring, y: rect.y - ring, width: rect.width + 2.0 * ring, height: rect.height + 2.0 * ring };
            self.ring.place(around, p);
        }
        let root = self.layer.wl_surface();
        if self.animation.is_some() {
            root.frame(&self.qh, FrameCallbackData(root.clone()));
        }
        root.commit();
    }

    fn relayout(&mut self) {
        let (width, height) = self.size;
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let margin = (height * MARGIN).round();
        let area = Rect { x: margin, y: margin, width: width - 2.0 * margin, height: height - 2.0 * margin };
        let label_height = (LABEL_GAP + LABEL_LINE + LABEL_GAP) as f64;
        let params = layout::Params { spacing: (height * SPACING).round(), label_height, max_scale: MAX_SCALE };
        let sizes: Vec<_> = self.thumbs.iter().map(Thumb::natural_size).collect();
        let rects = layout::arrange(&sizes, area, &params);
        // Every window shrinks by the same factor, so any one of them tells it.
        if let (Some(rect), Some(size)) = (rects.first(), sizes.first()) {
            self.control.zoom.set(rect.width / size.0);
        }
        for (index, (thumb, rect)) in self.thumbs.iter_mut().zip(rects).enumerate() {
            thumb.slot = Rect { x: rect.x.round(), y: rect.y.round(), width: rect.width.round(), height: rect.height.round() };
            let width = thumb.slot.width as u32;
            if width != thumb.label_width {
                thumb.label_width = width;
                let job = LabelJob {
                    index,
                    workspace: thumb.window.workspace.clone(),
                    title: match thumb.window.title.as_str() {
                        "" => thumb.window.app_id.clone(),
                        title => title.to_owned(),
                    },
                    width,
                    scale: self.scale,
                };
                let _ = self.painter.send(job);
            }
        }
        if self.phase != Phase::Waiting {
            self.draw_ring();
        }
    }

    /// Draws the selection ring for the selected thumbnail's size, unless
    /// the current one already fits.
    fn draw_ring(&mut self) {
        let Some(slot) = self.selected.map(|i| self.thumbs[i].slot) else {
            return;
        };
        let size = (slot.width as u32 + 2 * RING, slot.height as u32 + 2 * RING);
        if self.ring_image.as_ref().is_some_and(|(drawn, _)| *drawn == size) {
            return;
        }
        let image = paint::ring(size.0, size.1, RING as f64, (RADIUS + RING) as f64, ACCENT, self.scale);
        let Some(buffer) = self.pooled(image.width, image.height, image.stride(), wl_shm::Format::Argb8888, &image.pixels) else {
            return;
        };
        self.ring.show_pooled(&buffer, None);
        self.ring_image = Some((size, buffer));
    }

    fn select(&mut self, index: usize) {
        if self.selected.replace(index) != Some(index) {
            self.draw_ring();
            self.render();
        }
    }

    fn interactive(&self) -> bool {
        matches!(self.phase, Phase::Opening | Phase::Open)
    }

    fn thumb_at(&self, x: f64, y: f64) -> Option<usize> {
        self.thumbs.iter().position(|t| t.slot.contains(x, y))
    }

    /// Moves the selection to the nearest thumbnail in direction (dx, dy).
    fn step(&mut self, dx: f64, dy: f64) {
        let Some(current) = self.selected else {
            self.select(0);
            return;
        };
        let (cx, cy) = self.thumbs[current].slot.center();
        let nearest = self
            .thumbs
            .iter()
            .enumerate()
            .filter_map(|(i, thumb)| {
                let (x, y) = thumb.slot.center();
                let (vx, vy) = (x - cx, y - cy);
                let along = vx * dx + vy * dy;
                let across = (vx * dy - vy * dx).abs();
                // Straying sideways costs more than going far.
                (along > 1.0).then_some((i, along + 2.0 * across))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((i, _)) = nearest {
            self.select(i);
        }
    }

    fn cycle(&mut self, offset: isize) {
        let len = self.thumbs.len() as isize;
        if len == 0 {
            return;
        }
        let next = match self.selected {
            Some(current) => (current as isize + offset).rem_euclid(len),
            None => 0,
        };
        self.select(next as usize);
    }

    fn key(&mut self, keysym: Keysym) {
        if keysym == Keysym::Escape {
            self.close(None);
            return;
        }
        if !self.interactive() {
            return;
        }
        match keysym {
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                if let Some(i) = self.selected {
                    self.close(Some(i));
                }
            }
            Keysym::Left | Keysym::h => self.step(-1.0, 0.0),
            Keysym::Right | Keysym::l => self.step(1.0, 0.0),
            Keysym::Up | Keysym::k => self.step(0.0, -1.0),
            Keysym::Down | Keysym::j => self.step(0.0, 1.0),
            Keysym::Tab => self.cycle(1),
            Keysym::ISO_Left_Tab => self.cycle(-1),
            _ => {}
        }
    }

    /// Copies pixels into a buffer from the pool.
    fn pooled(&mut self, width: u32, height: u32, stride: u32, format: wl_shm::Format, pixels: &[u8]) -> Option<PoolBuffer> {
        let len = (stride * height) as usize;
        match self.pool.create_buffer(width as i32, height as i32, stride as i32, format) {
            Ok((buffer, canvas)) => {
                canvas[..len].copy_from_slice(&pixels[..len]);
                Some(buffer)
            }
            Err(err) => {
                eprintln!("woverview: out of buffer memory: {err}");
                None
            }
        }
    }
}

/// Attaches whatever the thumbnail has to show, or the placeholder.
fn show(thumb: &Thumb, placeholder: &WlBuffer, damage: Option<&[Damage]>) {
    match &thumb.content {
        Content::Nothing => thumb.picture.show(placeholder, None),
        Content::Shared { buffer, .. } => thumb.picture.show(buffer, None),
        Content::Pooled { buffer, .. } => thumb.picture.show_pooled(buffer, damage),
    }
}

/// A buffer whose pixels never change, for titles and corners.
fn static_buffer(shm: &Shm, image: &Image, qh: &QueueHandle<Ui>) -> io::Result<WlBuffer> {
    let fd = rustix::fs::memfd_create("woverview", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = File::from(fd);
    file.write_all(&image.pixels)?;
    let pool = shm.wl_shm().create_pool(file.as_fd(), image.pixels.len() as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        image.width as i32,
        image.height as i32,
        image.stride() as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

/// Where a window sits on the overlay, if sway gave it a real size.
fn on_overlay(window: &Window, (x, y): (f64, f64)) -> Option<Rect> {
    let r = window.rect;
    (r.width >= 1.0 && r.height >= 1.0).then_some(Rect { x: r.x - x, y: r.y - y, width: r.width, height: r.height })
}

fn lerp(from: Rect, to: Rect, t: f64) -> Rect {
    let mix = |a: f64, b: f64| a + (b - a) * t;
    Rect { x: mix(from.x, to.x), y: mix(from.y, to.y), width: mix(from.width, to.width), height: mix(from.height, to.height) }
}

fn scale_about_center(rect: Rect, scale: f64) -> Rect {
    let (w, h) = (rect.width * scale, rect.height * scale);
    Rect { x: rect.x + (rect.width - w) / 2.0, y: rect.y + (rect.height - h) / 2.0, width: w, height: h }
}

impl CompositorHandler for Ui {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {
        self.tick();
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
}

// Surfaces keep track of the outputs they're on, which needs this.
impl OutputHandler for Ui {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl LayerShellHandler for Ui {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.done = true;
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let (width, height) = configure.new_size;
        if width > 0 && height > 0 && (width as f64, height as f64) != self.size {
            self.size = (width as f64, height as f64);
            self.relayout();
        }
        if !self.configured {
            debug!("overlay configured");
            self.configured = true;
            // The backdrop: one pixel, stretched over the output, faded in
            // with progress.
            let root = self.layer.wl_surface();
            let channel = |v: f64| (v * u32::MAX as f64) as u32;
            let (r, g, b) = (channel(BACKDROP.0), channel(BACKDROP.1), channel(BACKDROP.2));
            let pixel = self.single_pixel.create_u32_rgba_buffer(r, g, b, u32::MAX, &self.qh, ());
            root.attach(Some(&pixel), 0, 0);
            root.damage_buffer(0, 0, 1, 1);
            self.backdrop.viewport.set_destination(self.size.0 as i32, self.size.1 as i32);
            self.backdrop.alpha.set_multiplier(multiplier(0.0));
            root.commit();
            self.start_if_ready();
        }
    }
}

impl SeatHandler for Ui {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seats.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
            if let (Some(shapes), Some(pointer)) = (&self.cursor_shapes, &self.pointer) {
                self.cursor = Some(shapes.get_shape_device(pointer, qh));
            }
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for Ui {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32) {}

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, event: KeyEvent) {
        self.key(event.keysym);
    }

    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, event: KeyEvent) {
        // Only moving around repeats; picking and closing happen once.
        if !matches!(event.keysym, Keysym::Escape | Keysym::Return | Keysym::KP_Enter | Keysym::space) {
            self.key(event.keysym);
        }
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, _: KeyEvent) {}

    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, _: Modifiers, _: RawModifiers, _: u32) {}
}

impl PointerHandler for Ui {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlPointer, events: &[PointerEvent]) {
        const BUTTON_LEFT: u32 = 0x110;
        const BUTTON_RIGHT: u32 = 0x111;
        for event in events {
            let (x, y) = event.position;
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    if let Some(cursor) = &self.cursor {
                        cursor.set_shape(serial, Shape::Default);
                    }
                }
                PointerEventKind::Motion { .. } if self.interactive() => {
                    if let Some(i) = self.thumb_at(x, y) {
                        self.select(i);
                    }
                }
                PointerEventKind::Release { button, .. } if self.interactive() => match (button, self.thumb_at(x, y)) {
                    (BUTTON_LEFT, Some(i)) => self.close(Some(i)),
                    (BUTTON_LEFT | BUTTON_RIGHT, None) => self.close(None),
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

impl ShmHandler for Ui {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Ui {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Ui);
delegate_dispatch2!(Ui);

delegate_noop!(Ui: ignore WlBuffer);
delegate_noop!(Ui: WlShmPool);
delegate_noop!(Ui: WpViewporter);
delegate_noop!(Ui: WpViewport);
delegate_noop!(Ui: WpSinglePixelBufferManagerV1);
delegate_noop!(Ui: WpAlphaModifierV1);
delegate_noop!(Ui: WpAlphaModifierSurfaceV1);
