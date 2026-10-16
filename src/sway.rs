//! The little bit of sway IPC woverview needs: read the tree, focus a window.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use serde_json::Value;

const RUN_COMMAND: u32 = 0;
const GET_TREE: u32 = 4;
const SCRATCHPAD: &str = "__i3_scratch";

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub con_id: i64,
    /// The window's ext-foreign-toplevel-list identifier, used to capture it.
    pub identifier: String,
    pub app_id: String,
    pub title: String,
    pub workspace: String,
    /// Where sway put the window, in layout coordinates.
    pub rect: Rect,
    pub focused: bool,
    /// On screen right now: on a visible workspace, and not behind a
    /// fullscreen window or another tab.
    pub visible: bool,
    pub floating: bool,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    /// Ordered by workspace, then top to bottom, then left to right.
    pub windows: Vec<Window>,
    /// Where the focused output sits, in layout coordinates.
    pub focused_output_rect: Rect,
    /// Pixels per layout unit on the focused output.
    pub focused_output_scale: f64,
    pub focused_workspace: Option<String>,
}

pub struct Ipc(UnixStream);

impl Ipc {
    pub fn connect() -> io::Result<Self> {
        let path = std::env::var_os("SWAYSOCK")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SWAYSOCK is not set; is sway running?"))?;
        Ok(Self(UnixStream::connect(path)?))
    }

    pub fn snapshot(&mut self) -> io::Result<Snapshot> {
        let tree = self.request(GET_TREE, "")?;
        let mut walk = Walk::default();
        walk.visit(&tree, "", "");
        walk.windows.sort_by(|(a_ws, a), (b_ws, b)| {
            a_ws.cmp(b_ws)
                .then(a.rect.y.total_cmp(&b.rect.y))
                .then(a.rect.x.total_cmp(&b.rect.x))
        });
        let (focused_output_rect, focused_output_scale) = walk
            .outputs
            .iter()
            .find(|(name, ..)| Some(name) == walk.focused_output.as_ref())
            .map_or((Rect::default(), 1.0), |(_, rect, scale)| (*rect, *scale));
        Ok(Snapshot {
            windows: walk.windows.into_iter().map(|(_, window)| window).collect(),
            focused_output_rect,
            focused_output_scale,
            focused_workspace: walk.focused_workspace,
        })
    }

    pub fn focus(&mut self, con_id: i64) -> io::Result<()> {
        let reply = self.request(RUN_COMMAND, &format!("[con_id={con_id}] focus"))?;
        match reply[0]["success"].as_bool() {
            Some(true) => Ok(()),
            _ => Err(io::Error::other(format!("sway refused to focus window {con_id}: {reply}"))),
        }
    }

    fn request(&mut self, kind: u32, payload: &str) -> io::Result<Value> {
        let mut message = Vec::with_capacity(14 + payload.len());
        message.extend_from_slice(b"i3-ipc");
        message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(payload.as_bytes());
        self.0.write_all(&message)?;

        let mut header = [0; 14];
        self.0.read_exact(&mut header)?;
        let length = u32::from_ne_bytes(header[6..10].try_into().unwrap());
        let mut body = vec![0; length as usize];
        self.0.read_exact(&mut body)?;
        serde_json::from_slice(&body).map_err(io::Error::other)
    }
}

#[derive(Default)]
struct Walk {
    /// Each window with the position of its workspace in the tree.
    windows: Vec<(usize, Window)>,
    workspaces_seen: usize,
    outputs: Vec<(String, Rect, f64)>,
    focused_output: Option<String>,
    focused_workspace: Option<String>,
}

impl Walk {
    fn visit(&mut self, node: &Value, output: &str, workspace: &str) {
        let name = node["name"].as_str().unwrap_or_default();
        let (output, workspace) = match node["type"].as_str() {
            Some("output") => {
                let scale = node["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
                self.outputs.push((name.to_owned(), rect(&node["rect"]), scale));
                (name, workspace)
            }
            Some("workspace") => {
                self.workspaces_seen += 1;
                (output, name)
            }
            _ => (output, workspace),
        };

        let focused = node["focused"].as_bool() == Some(true);
        if focused {
            self.focused_output = Some(output.to_owned());
            self.focused_workspace = Some(workspace.to_owned());
        }

        // Only views have a pid; split containers and workspaces don't.
        if node["pid"].is_number() {
            // The scratchpad sits first in the tree. Show it last.
            let order = if workspace == SCRATCHPAD { usize::MAX } else { self.workspaces_seen };
            self.windows.push((order, window(node, workspace, focused)));
        }

        let children = node["nodes"].as_array().into_iter().flatten();
        let floating = node["floating_nodes"].as_array().into_iter().flatten();
        for child in children.chain(floating) {
            self.visit(child, output, workspace);
        }
    }
}

fn window(node: &Value, workspace: &str, focused: bool) -> Window {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let app_id = match node["app_id"].as_str() {
        Some(app_id) => app_id.to_owned(),
        None => text(&node["window_properties"]["class"]),
    };
    // `rect` is the container, borders included; `window_rect` is the
    // content inside it.
    let (outer, inner) = (rect(&node["rect"]), rect(&node["window_rect"]));
    Window {
        con_id: node["id"].as_i64().unwrap_or_default(),
        identifier: text(&node["foreign_toplevel_identifier"]),
        app_id,
        title: text(&node["name"]),
        workspace: if workspace == SCRATCHPAD { "scratchpad".to_owned() } else { workspace.to_owned() },
        rect: Rect { x: outer.x + inner.x, y: outer.y + inner.y, width: inner.width, height: inner.height },
        focused,
        visible: node["visible"].as_bool() == Some(true),
        floating: node["type"].as_str() == Some("floating_con"),
    }
}

fn rect(value: &Value) -> Rect {
    let number = |key: &str| value[key].as_f64().unwrap_or_default();
    Rect { x: number("x"), y: number("y"), width: number("width"), height: number("height") }
}
