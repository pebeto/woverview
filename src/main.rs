//! woverview: every open window at a glance, for sway.

mod capture;
mod debug;
mod layout;
mod paint;
mod shrink;
mod sway;
mod ui;

use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::channel::{self, Event};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use wayland_client::Connection;
use wayland_client::globals::registry_queue_init;

use crate::capture::{Control, Target};
use crate::debug::debug;
use crate::paint::Font;
use crate::sway::WindowEvent;
use crate::ui::Ui;

const DEFAULT_FPS: u32 = 30;
/// How long opening waits for pictures of the windows that are on screen.
/// They start where the real window is, so without a picture there'd be a
/// gray box in its place.
const FRAME_WAIT: Duration = Duration::from_millis(150);
const HELP: &str = "\
Usage: woverview [--current] [--fps N] [--font FONT] [--close]

Shows every open window as a thumbnail, including the ones on other
workspaces and in the scratchpad. Click one, or move with the arrow keys
(or hjkl, or Tab) and press Enter, to focus it. To close a window, click
the button on its thumbnail's corner, or middle-click the thumbnail.
Escape or a click on the background closes the overview.

Running woverview while the overview is open closes it, so one binding
can toggle it.

Options:
  -c, --current   only show windows on the focused workspace
      --fps N     redraw a thumbnail at most N times a second when its
                  window changes (default 30); 0 keeps them still
      --font FONT font for window titles, like \"JuliaMono 15\", in points
                  as in sway's font option (default: Sans at 15 px)
      --close     close the overview if it is open; never open it
  -h, --help      print this help
  -V, --version   print the version
";

struct Options {
    current: bool,
    close: bool,
    fps: u32,
    font: Font,
}

enum Parsed {
    Run(Options),
    Help,
    Version,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut options = Options { current: false, close: false, fps: DEFAULT_FPS, font: Font::default() };
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "-c" | "--current" => options.current = true,
            "--close" => options.close = true,
            "--fps" => options.fps = parse_fps(args.next())?,
            _ if arg.starts_with("--fps=") => options.fps = parse_fps(arg.strip_prefix("--fps="))?,
            "--font" => options.font = Font::parse(args.next().unwrap_or_default())?,
            _ if arg.starts_with("--font=") => options.font = Font::parse(&arg["--font=".len()..])?,
            _ => return Err(format!("unknown option '{arg}'")),
        }
    }
    Ok(Parsed::Run(options))
}

fn parse_fps(value: Option<&str>) -> Result<u32, String> {
    let value = value.ok_or("--fps needs a number")?;
    value.parse().map_err(|_| format!("--fps wants a whole number, not '{value}'"))
}

fn main() -> ExitCode {
    debug!("started");
    let args: Vec<String> = std::env::args().collect();
    let options = match parse(&args[1..]) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("woverview {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprint!("woverview: {err}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    // If an overview is already up, this run only tells it to close.
    let claim = match claim() {
        Ok(Claim::Forwarded) => return ExitCode::SUCCESS,
        Ok(Claim::Ours(claim)) => claim,
        Err(err) => {
            eprintln!("woverview: {err}");
            return ExitCode::FAILURE;
        }
    };
    if options.close {
        return ExitCode::SUCCESS;
    }
    match run(&options, &claim) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("woverview: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The socket a running overview listens on, one per Wayland display.
/// Another run that finds it asks that overview to close, so one key
/// binding toggles.
fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let display = display.rsplit('/').next().unwrap_or_default();
    dir.join(format!("woverview-{display}.sock"))
}

/// Our claim on the socket. The file goes away with it.
struct Socket {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

enum Claim {
    /// Another overview was up, and has been told to close.
    Forwarded,
    Ours(Socket),
}

fn claim() -> io::Result<Claim> {
    let path = socket_path();
    for _ in 0..3 {
        match UnixStream::connect(&path) {
            Ok(_) => return Ok(Claim::Forwarded),
            // Left behind by an overview that didn't get to clean up.
            Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
        match UnixListener::bind(&path) {
            Ok(listener) => return Ok(Claim::Ours(Socket { listener, path })),
            // Another run got there first; go tell that one instead.
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other(format!("can't reach or replace the overview behind {}", path.display())))
}

fn run(options: &Options, socket: &Socket) -> Result<(), Box<dyn std::error::Error>> {
    // Listen for windows opening and closing before reading the list, so
    // one that does in between still gets reported.
    let window_events = sway::Ipc::connect()?.subscribe_to_windows()?;
    let snapshot = sway::Ipc::connect()?.snapshot()?;
    let (current, focused_workspace) = (options.current, snapshot.focused_workspace.clone());
    let wanted = move |w: &sway::Window| !current || Some(&w.workspace) == focused_workspace.as_ref();
    let windows: Vec<_> = snapshot.windows.iter().filter(|w| wanted(w)).cloned().collect();
    debug!("{} windows from sway", windows.len());
    if windows.is_empty() {
        return Ok(());
    }
    let targets: Vec<_> = windows
        .iter()
        .filter(|w| !w.identifier.is_empty())
        .map(|w| Target { identifier: w.identifier.clone(), on_screen: w.visible })
        .collect();

    let connection = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<Ui>(&connection)?;
    let mut event_loop = EventLoop::<Ui>::try_new()?;
    let handle = event_loop.handle();

    // The overlay lays the thumbnails out first, which tells the capture
    // thread how small to make frames.
    let (labels, label_source) = channel::channel();
    let control = Control::default();
    let painter = ui::spawn_painter(labels, options.font.clone());
    let mut ui = Ui::new(&globals, &queue.handle(), windows, &snapshot, control.clone(), painter, &options.font)?;
    WaylandSource::new(connection, queue).insert(handle.clone()).map_err(|err| err.error)?;
    handle.insert_source(label_source, |event, _, ui| {
        if let Event::Msg(label) = event {
            ui.label_arrived(label);
        }
    })?;

    // Capture on a worker thread; thumbnails arrive while the overlay gets
    // ready, then keep up with their windows. Windows that open later get a
    // thread of their own.
    let live = (options.fps > 0).then(|| Duration::from_secs_f64(1.0 / options.fps as f64));
    let (frames, frame_source) = channel::sync_channel(ui.thumb_count() * 2);
    let start_capture = move |targets: Vec<Target>| {
        let (frames, control) = (frames.clone(), control.clone());
        std::thread::spawn(move || {
            if let Err(err) = capture::capture(&targets, live, &control, |frame| frames.try_send(frame)) {
                eprintln!("woverview: can't capture windows: {err}");
            }
        });
    };
    start_capture(targets);
    handle.insert_source(frame_source, |event, _, ui| {
        if let Event::Msg(frame) = event {
            ui.frame_arrived(frame);
        }
    })?;

    handle.insert_source(Timer::from_duration(FRAME_WAIT), |_, _, ui| {
        ui.stop_waiting();
        TimeoutAction::Drop
    })?;
    handle.insert_source(Timer::from_duration(ui::FRAME_DEADLINE), |_, _, ui| {
        ui.watchdog();
        TimeoutAction::ToDuration(ui::FRAME_DEADLINE / 4)
    })?;

    window_events.set_nonblocking(true)?;
    let mut pending = Vec::new();
    handle.insert_source(Generic::new(window_events, Interest::READ, Mode::Level), move |_, events, ui| {
        let mut events: &UnixStream = events;
        let mut chunk = [0; 4096];
        loop {
            match events.read(&mut chunk) {
                Ok(0) => return Ok(PostAction::Remove),
                Ok(read) => pending.extend_from_slice(&chunk[..read]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                // Losing track of windows isn't worth closing over.
                Err(err) => {
                    eprintln!("woverview: stopped hearing about windows: {err}");
                    return Ok(PostAction::Remove);
                }
            }
        }
        let events = sway::window_events(&mut pending);
        if events.is_empty() {
            return Ok(PostAction::Continue);
        }
        for event in &events {
            if let WindowEvent::Closed(con_id) = *event {
                ui.window_closed(con_id);
            }
        }
        // A window opening or closing moves its neighbours, and a new one
        // needs a workspace and a size, so read the tree again.
        let snapshot = match sway::Ipc::connect().and_then(|mut ipc| ipc.snapshot()) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                eprintln!("woverview: {err}");
                return Ok(PostAction::Continue);
            }
        };
        for event in events {
            if let WindowEvent::New(con_id) = event
                && let Some(window) = snapshot.windows.iter().find(|w| w.con_id == con_id && wanted(w))
                && ui.window_opened(window.clone(), &snapshot)
                && !window.identifier.is_empty()
            {
                start_capture(vec![Target { identifier: window.identifier.clone(), on_screen: false }]);
            }
        }
        ui.windows_moved(&snapshot);
        Ok(PostAction::Continue)
    })?;

    let listener = socket.listener.try_clone()?;
    listener.set_nonblocking(true)?;
    handle.insert_source(Generic::new(listener, Interest::READ, Mode::Level), |_, listener, ui| {
        while listener.accept().is_ok() {
            ui.dismiss();
        }
        Ok(PostAction::Continue)
    })?;

    while !ui.done {
        event_loop.dispatch(None, &mut ui)?;
    }
    Ok(())
}
