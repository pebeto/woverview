//! Captures windows through ext-image-copy-capture-v1.
//!
//! The compositor renders every toplevel on its own, so windows on hidden
//! workspaces, in the scratchpad or behind other windows come out whole.
//! This opens its own Wayland connection and is meant to run on a worker
//! thread, so copying and shrinking pictures never holds up the overlay.

use std::collections::VecDeque;
use std::error::Error;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::time::{Duration, Instant};

use memmap2::Mmap;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::MemfdFlags;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self, ExtImageCopyCaptureFrameV1, FailureReason,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self, ExtImageCopyCaptureSessionV1,
};

use crate::debug::{self, debug};
use crate::shrink::{Shrunk, Zoom};

/// A window to capture.
pub struct Target {
    pub identifier: String,
    /// On screen right now. The opening animation starts such a window at
    /// its real size, so its first frame keeps every pixel.
    pub on_screen: bool,
}

/// What the overview tells the capture thread while it runs.
#[derive(Clone, Default)]
pub struct Control {
    /// How much thumbnails shrink, which sets how small frames get.
    pub zoom: Zoom,
    paused: Arc<AtomicBool>,
}

impl Control {
    /// Holds live frames back. Every capture has sway copy a window back
    /// from the GPU, which delays the frames the overview animates with.
    pub fn pause(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// One picture of a window, straight from the compositor.
pub struct Frame {
    pub identifier: String,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: wl_shm::Format,
    pub pixels: Pixels,
    /// The parts that changed since the previous frame of this window, in
    /// buffer pixels. `None` for a window's first frame.
    pub damage: Option<Vec<Damage>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Damage {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub enum Pixels {
    /// The shared memory the compositor wrote into, handed over as is,
    /// with its file, so it can go back to the compositor without a copy.
    Shared { map: Mmap, fd: OwnedFd },
    /// A copy, for when the shared memory gets reused for the next frame.
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        match self {
            Pixels::Shared { map, .. } => map,
            Pixels::Owned(bytes) => bytes,
        }
    }
}

/// Formats woverview knows how to read, best first. Sway sometimes offers
/// only the 24-bit ones.
const FORMATS: [wl_shm::Format; 6] = [
    wl_shm::Format::Argb8888,
    wl_shm::Format::Xrgb8888,
    wl_shm::Format::Abgr8888,
    wl_shm::Format::Xbgr8888,
    wl_shm::Format::Bgr888,
    wl_shm::Format::Rgb888,
];

/// How often in a row a window may fail because its size or format changed
/// mid-copy.
const ATTEMPTS: u32 = 3;
/// Without live updates, how long to wait for the first frames.
const TIMEOUT: Duration = Duration::from_secs(3);
/// How soon to offer a frame again when the overview had no room for it.
const RETRY: Duration = Duration::from_millis(5);
/// The longest the thread sleeps while every window sits still.
const IDLE: Duration = Duration::from_secs(1);

/// Captures these windows and passes each frame to `send` as it arrives,
/// shrunk to about the size `control` says it's shown at. `send` hands a
/// frame back when the overview has no room for it yet.
///
/// Without `live`, that's one frame per window; windows that haven't
/// answered after a few seconds are dropped. With `live`, it keeps going:
/// whenever a window changes it sends a new frame, at most one per window
/// every `live`, until the receiving end goes away.
pub fn capture(
    targets: &[Target],
    live: Option<Duration>,
    control: &Control,
    mut send: impl FnMut(Frame) -> Result<(), TrySendError<Frame>>,
) -> Result<(), Box<dyn Error>> {
    debug!("capture thread up");
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let qh = queue.handle();
    let mut state = State {
        shm: globals.bind(&qh, 1..=1, ())?,
        live,
        control: control.clone(),
        toplevels: Vec::new(),
        captures: Vec::new(),
        ready: VecDeque::new(),
    };
    let _list: ExtForeignToplevelListV1 = globals.bind(&qh, 1..=1, ())?;
    let sources: ExtForeignToplevelImageCaptureSourceManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let manager: ExtImageCopyCaptureManagerV1 = globals.bind(&qh, 1..=1, ())?;

    // The list announces every toplevel, with its identifier, right after the bind.
    queue.roundtrip(&mut state)?;
    debug!("compositor lists {} toplevels", state.toplevels.len());

    for target in targets {
        let Some(toplevel) = state.toplevels.iter().find(|t| t.identifier.as_ref() == Some(&target.identifier)) else {
            eprintln!("woverview: no toplevel with identifier {}", target.identifier);
            continue;
        };
        let index = state.captures.len();
        let source = sources.create_source(&toplevel.handle, &qh, ());
        let session = manager.create_session(&source, Options::empty(), &qh, index);
        state.captures.push(Capture::new(target, source, session));
    }

    let started = Instant::now();
    loop {
        // Hand frames over. If the overview is behind, keep the rest.
        let mut behind = false;
        while let Some(frame) = state.ready.pop_front() {
            let note = debug::enabled().then(|| {
                let Frame { identifier, width, height, format, damage, .. } = &frame;
                let changed = damage.as_ref().map_or("all".to_owned(), |d| format!("{} rects", d.len()));
                format!("frame {identifier} {width}x{height} {format:?} {changed}")
            });
            match send(frame) {
                Ok(()) => {
                    if let Some(note) = note {
                        debug!("{note}");
                    }
                }
                Err(TrySendError::Full(frame)) => {
                    state.ready.push_front(frame);
                    behind = true;
                    break;
                }
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        if state.ready.is_empty() && state.captures.iter().all(|c| c.finished) {
            return Ok(());
        }

        let now = Instant::now();
        let soonest = now + if behind { RETRY } else { IDLE };
        let wake = match live {
            Some(interval) => {
                // Hold off while the overview asks us to, hasn't picked up
                // what it already has, or hasn't laid out the thumbnails
                // yet, which is what tells us how small to make the frames.
                let control = &state.control;
                let wait = behind || control.paused() || control.zoom.factor().is_none();
                for index in 0..state.captures.len() {
                    let capture = &mut state.captures[index];
                    if capture.next.is_some_and(|at| at <= now) {
                        if wait {
                            capture.next = Some(now + interval);
                        } else {
                            capture.next = None;
                            state.start(index, &qh);
                        }
                    }
                }
                let next = state.captures.iter().filter_map(|c| c.next).min();
                next.map_or(soonest, |next| next.min(soonest))
            }
            None => {
                let deadline = started + TIMEOUT;
                if now >= deadline {
                    let late: Vec<_> = state.captures.iter().filter(|c| !c.finished).map(|c| &c.identifier).collect();
                    eprintln!("woverview: gave up waiting for {late:?}");
                    return Ok(());
                }
                deadline.min(soonest)
            }
        };
        dispatch(&mut queue, &mut state, wake.saturating_duration_since(now))?;
    }
}

/// Like `EventQueue::blocking_dispatch`, but gives up after `timeout`.
fn dispatch(queue: &mut EventQueue<State>, state: &mut State, timeout: Duration) -> Result<(), Box<dyn Error>> {
    queue.flush()?;
    if let Some(guard) = queue.prepare_read() {
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let timeout = Timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => {}
            Ok(_) => {
                guard.read()?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

struct Toplevel {
    handle: ExtForeignToplevelHandleV1,
    identifier: Option<String>,
}

struct Capture {
    identifier: String,
    on_screen: bool,
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    /// Buffer constraints from the session, replaced as a batch every time
    /// the session sends a new set.
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    batch_done: bool,
    /// Where the compositor copies to. Live captures reuse it frame after
    /// frame, so the compositor only has to redraw what changed.
    buffer: Option<Buffer>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// What the compositor says changed, for the frame in flight.
    damage: Vec<Damage>,
    /// The window at about thumbnail size, as a 4-byte format.
    shrunk: Option<Shrunk>,
    /// When to ask for the next live frame.
    next: Option<Instant>,
    sent: u32,
    failures: u32,
    finished: bool,
}

struct Buffer {
    wl_buffer: WlBuffer,
    map: Mmap,
    fd: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    /// Nothing was ever copied into it, so all of it needs filling in.
    blank: bool,
}

impl Buffer {
    fn new(shm: &WlShm, qh: &QueueHandle<State>, width: u32, height: u32, format: wl_shm::Format) -> Result<Self, Box<dyn Error>> {
        // wlroots wants the stride to be a whole number of pixels, so don't
        // pad rows to 4 bytes: with 3-byte formats that gets rejected.
        let stride = width * bytes_per_pixel(format);
        let len = stride as usize * height as usize;

        let fd = rustix::fs::memfd_create("woverview", MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, len as u64)?;
        // SAFETY: only the compositor writes to this memfd, and only while a
        // frame is in flight; we read it between frames.
        let map = unsafe { Mmap::map(&fd)? };
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let wl_buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
        pool.destroy();
        Ok(Self { wl_buffer, map, fd, width, height, stride, format, blank: true })
    }
}

impl Capture {
    fn new(target: &Target, source: ExtImageCaptureSourceV1, session: ExtImageCopyCaptureSessionV1) -> Self {
        Self {
            identifier: target.identifier.clone(),
            on_screen: target.on_screen,
            source,
            session,
            size: None,
            formats: Vec::new(),
            batch_done: false,
            buffer: None,
            frame: None,
            damage: Vec::new(),
            shrunk: None,
            next: None,
            sent: 0,
            failures: 0,
            finished: false,
        }
    }

    /// A constraint event after `done` starts a fresh batch.
    fn constraint(&mut self) {
        if self.batch_done {
            self.batch_done = false;
            self.size = None;
            self.formats.clear();
        }
    }

    fn start(&mut self, index: usize, shm: &WlShm, qh: &QueueHandle<State>) -> Result<(), Box<dyn Error>> {
        let (width, height) = self.size.ok_or("the compositor sent no buffer size")?;
        let format = FORMATS
            .into_iter()
            .find(|f| self.formats.contains(f))
            .ok_or_else(|| format!("no usable shm format among {:?}", self.formats))?;
        let fits = self.buffer.as_ref().is_some_and(|b| (b.width, b.height, b.format) == (width, height, format));
        if !fits {
            self.drop_buffer();
            self.buffer = Some(Buffer::new(shm, qh, width, height, format)?);
        }
        let buffer = self.buffer.as_mut().unwrap();

        let frame = self.session.create_frame(qh, index);
        frame.attach_buffer(&buffer.wl_buffer);
        // The client only reports damage it did to the buffer itself, which
        // after the first copy is none: we never write to it.
        if buffer.blank {
            frame.damage_buffer(0, 0, width as i32, height as i32);
            buffer.blank = false;
        }
        frame.capture();
        self.frame = Some(frame);
        self.damage.clear();
        Ok(())
    }

    /// Turns the buffer the compositor just filled into a frame: the
    /// shrunk copy, redone only where the window changed, while the buffer
    /// stays to take the next frame.
    ///
    /// A window on screen is the exception the first time: the opening
    /// starts it at its real size, so it keeps every pixel. The same goes
    /// before the overview has a layout to shrink to. In a 4-byte format,
    /// the buffer then changes hands as it is, so the compositor can show it
    /// without a copy, and the next frame gets a new buffer. A 3-byte format
    /// gets converted to 4 bytes instead: sway's GL renderer reads 3-byte
    /// rows as if they were padded to a multiple of 4 bytes, which runs off
    /// the end of the buffer unless the width happens to fit, and with
    /// NVIDIA's driver that crashes sway.
    fn take_frame(&mut self, factor: Option<u32>) -> Frame {
        let identifier = self.identifier.clone();
        let buffer = self.buffer.as_ref().unwrap();
        let first_on_screen = self.sent == 0 && self.on_screen;
        let factor = match shrink_factor(buffer.format, factor, first_on_screen) {
            Some(factor) => factor,
            None => {
                let (width, height, stride, format) = (buffer.width, buffer.height, buffer.stride, buffer.format);
                let Buffer { wl_buffer, map, fd, .. } = self.buffer.take().unwrap();
                wl_buffer.destroy();
                return Frame { identifier, width, height, stride, format, pixels: Pixels::Shared { map, fd }, damage: None };
            }
        };

        let damage = std::mem::take(&mut self.damage);
        let damage = match &mut self.shrunk {
            Some(shrunk) if shrunk.fits(buffer.width, buffer.height, buffer.format, factor) => {
                Some(damage.into_iter().map(|d| shrunk.update(&buffer.map, buffer.stride, d)).collect())
            }
            _ => {
                let shrunk = Shrunk::new(&buffer.map, buffer.width, buffer.height, buffer.stride, buffer.format, factor);
                self.shrunk = Some(shrunk);
                None
            }
        };
        let shrunk = self.shrunk.as_ref().unwrap();
        Frame {
            identifier,
            width: shrunk.width,
            height: shrunk.height,
            stride: shrunk.width * 4,
            format: shrunk.format(),
            pixels: Pixels::Owned(shrunk.pixels.clone()),
            damage,
        }
    }

    fn drop_buffer(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.wl_buffer.destroy();
        }
    }

    fn finish(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.drop_buffer();
        self.session.destroy();
        self.source.destroy();
        self.next = None;
        self.finished = true;
    }
}

/// How much to shrink a frame, or `None` to hand its buffer to the
/// compositor as it is. Never `None` for a 3-byte format: see `take_frame`.
fn shrink_factor(format: wl_shm::Format, factor: Option<u32>, first_on_screen: bool) -> Option<u32> {
    match factor {
        Some(factor) if !first_on_screen => Some(factor),
        _ if bytes_per_pixel(format) == 3 => Some(1),
        _ => None,
    }
}

pub fn bytes_per_pixel(format: wl_shm::Format) -> u32 {
    match format {
        wl_shm::Format::Bgr888 | wl_shm::Format::Rgb888 => 3,
        _ => 4,
    }
}

struct State {
    shm: WlShm,
    live: Option<Duration>,
    control: Control,
    toplevels: Vec<Toplevel>,
    captures: Vec<Capture>,
    ready: VecDeque<Frame>,
}

impl State {
    fn start(&mut self, index: usize, qh: &QueueHandle<Self>) {
        let capture = &mut self.captures[index];
        if capture.finished || capture.frame.is_some() {
            return;
        }
        if let Err(err) = capture.start(index, &self.shm, qh) {
            eprintln!("woverview: can't capture {}: {err}", capture.identifier);
            capture.finish();
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(Toplevel { handle: toplevel, identifier: None });
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } = event
            && let Some(toplevel) = state.toplevels.iter_mut().find(|t| t.handle == *handle)
        {
            toplevel.identifier = Some(identifier);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        let capture = &mut state.captures[*index];
        match event {
            Event::BufferSize { width, height } => {
                capture.constraint();
                capture.size = Some((width, height));
            }
            Event::ShmFormat { format } => {
                capture.constraint();
                if let WEnum::Value(format) = format {
                    capture.formats.push(format);
                }
            }
            Event::DmabufDevice { .. } | Event::DmabufFormat { .. } => capture.constraint(),
            Event::Done => {
                if capture.sent == 0 && capture.frame.is_none() {
                    debug!("constraints for {}", capture.identifier);
                }
                capture.batch_done = true;
                // A live capture waiting out its interval picks up the new
                // constraints when it starts again.
                if capture.next.is_none() {
                    state.start(*index, qh);
                }
            }
            Event::Stopped if !capture.finished => {
                if capture.sent == 0 {
                    eprintln!("woverview: {} closed before it was captured", capture.identifier);
                }
                capture.finish();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        let (live, factor) = (state.live, state.control.zoom.factor());
        let capture = &mut state.captures[*index];
        match event {
            Event::Damage { x, y, width, height } => capture.damage.push(Damage { x, y, width, height }),
            Event::Ready => {
                capture.frame.take().unwrap().destroy();
                let frame = capture.take_frame(factor);
                capture.sent += 1;
                capture.failures = 0;
                match live {
                    Some(interval) => capture.next = Some(Instant::now() + interval),
                    None => capture.finish(),
                }
                state.ready.push_back(frame);
            }
            Event::Failed { reason } => {
                if let Some(frame) = capture.frame.take() {
                    frame.destroy();
                }
                capture.failures += 1;
                // The window changed size or format under us. The session
                // sends the new constraints; try again with those.
                let retry = reason == WEnum::Value(FailureReason::BufferConstraints) && capture.failures < ATTEMPTS;
                if retry {
                    capture.drop_buffer();
                    if capture.batch_done {
                        state.start(*index, qh);
                    }
                } else {
                    if reason != WEnum::Value(FailureReason::Stopped) {
                        eprintln!("woverview: capturing {} failed: {reason:?}", capture.identifier);
                    }
                    capture.finish();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_byte_buffers_never_go_to_the_compositor_as_is() {
        for format in FORMATS {
            for factor in [None, Some(1), Some(2)] {
                for first_on_screen in [false, true] {
                    let shrink = shrink_factor(format, factor, first_on_screen);
                    if bytes_per_pixel(format) == 3 {
                        assert!(shrink.is_some(), "{format:?} {factor:?} {first_on_screen}");
                    }
                    if first_on_screen {
                        assert!(matches!(shrink, None | Some(1)), "on-screen windows open at full size");
                    }
                }
            }
        }
    }

    /// Talks to the running sway session: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn captures_every_window_in_the_session() {
        let windows = crate::sway::Ipc::connect().unwrap().snapshot().unwrap().windows;
        let targets: Vec<_> = windows.iter().map(|w| Target { identifier: w.identifier.clone(), on_screen: w.visible }).collect();
        let (sender, receiver) = std::sync::mpsc::sync_channel(targets.len());
        capture(&targets, None, &Control::default(), |frame| sender.try_send(frame)).unwrap();
        let frames: Vec<_> = receiver.try_iter().collect();

        // A window can really be all black, which in 24 bits is all zeros,
        // so one blank frame proves nothing. All of them blank means the
        // copy itself is broken.
        let mut blank = Vec::new();
        for frame in &frames {
            let pixels = frame.pixels.as_ref();
            assert!(pixels.len() >= frame.stride as usize * frame.height as usize);
            if pixels.iter().all(|&byte| byte == 0) {
                blank.push(&frame.identifier);
            }
        }
        assert!(blank.len() < frames.len(), "every frame came back blank");
        if !blank.is_empty() {
            eprintln!("note: all black, maybe for real: {blank:?}");
        }
        let missing: Vec<_> = windows
            .iter()
            .filter(|w| !frames.iter().any(|f| f.identifier == w.identifier))
            .map(|w| &w.title)
            .collect();
        assert!(missing.is_empty(), "not captured: {missing:?}");
    }
}
