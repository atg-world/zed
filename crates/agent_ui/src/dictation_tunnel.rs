//! Dictation tunnel: lets another program on this PC (DeskBridge, driven from a phone) speak words into the agent panel
//! while the tunnel is switched on, whichever window is in front.
//!
//! Off by default. `agent: toggle dictation tunnel` turns it on or off. While it is on:
//!   * Zed listens on 127.0.0.1 only (loopback, a random port) and writes `{port, token}` to
//!     `<config dir>/dictation-tunnel.json` (on Windows `%APPDATA%\Zed\`), which only this user can read.
//!   * A client sends one JSON line `{"token": "...", "text": "...", "backspace": 0, "submit": false}` and gets one line
//!     back: `{"ok": true}` or `{"ok": false, "reason": "no-chat" | "bad-token" | "bad-request"}`.
//!   * The text goes into the message box of the open agent thread (the window in front if it has one, else any window).
//!   * Nothing is ever typed into another window: no open thread means `no-chat`.
//! Turning it off closes the listener and deletes the file, so a stale file simply means "off" to the client.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use gpui::{App, Context, Global, WindowHandle, actions};
use serde::Deserialize;
use workspace::{MultiWorkspace, Workspace};

use crate::AgentPanel;

actions!(
    agent,
    [
        /// Turns the dictation tunnel on or off. While on, a program on this PC (such as DeskBridge) can speak words into the
        /// agent panel without any window needing focus.
        ToggleDictationTunnel,
    ]
);

const DESCRIPTOR_FILE: &str = "dictation-tunnel.json";
/// A request line longer than this is refused (a spoken sentence is a few hundred bytes).
const MAX_LINE_BYTES: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BACKSPACE: usize = 200;

#[derive(Debug, PartialEq)]
pub(crate) struct DictationRequest {
    pub text: String,
    pub backspace: usize,
    pub submit: bool,
}

#[derive(Deserialize)]
struct WireRequest {
    token: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    backspace: usize,
    #[serde(default)]
    submit: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Refusal {
    BadRequest,
    BadToken,
}

impl Refusal {
    fn reason(&self) -> &'static str {
        match self {
            Refusal::BadRequest => "bad-request",
            Refusal::BadToken => "bad-token",
        }
    }
}

/// Parses one request line and checks the token. Pure, so it is unit-tested without any window.
pub(crate) fn parse_request(line: &str, expected_token: &str) -> Result<DictationRequest, Refusal> {
    let wire: WireRequest = serde_json::from_str(line).map_err(|_| Refusal::BadRequest)?;
    if !constant_time_eq(wire.token.as_bytes(), expected_token.as_bytes()) {
        return Err(Refusal::BadToken);
    }
    if wire.text.is_empty() && wire.backspace == 0 && !wire.submit {
        return Err(Refusal::BadRequest);
    }
    Ok(DictationRequest {
        text: wire.text,
        backspace: wire.backspace.min(MAX_BACKSPACE),
        submit: wire.submit,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Pending {
    request: DictationRequest,
    reply: mpsc::SyncSender<&'static str>,
}

struct RunningTunnel {
    stop: Arc<AtomicBool>,
    port: u16,
}

#[derive(Default)]
struct DictationTunnelState(Option<RunningTunnel>);

impl Global for DictationTunnelState {}

pub fn init(cx: &mut App) {
    cx.set_global(DictationTunnelState::default());
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace.register_action(|workspace, _: &ToggleDictationTunnel, _window, cx| {
                let message = toggle(cx);
                struct DictationTunnelToast;
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<DictationTunnelToast>(),
                        message,
                    )
                    .autohide(),
                    cx,
                );
            });
        },
    )
    .detach();
    cx.on_app_quit(|cx| {
        stop(cx);
        async {}
    })
    .detach();
}

/// Flips the tunnel and returns the sentence to show the person.
fn toggle(cx: &mut App) -> String {
    if cx.global::<DictationTunnelState>().0.is_some() {
        stop(cx);
        return "Dictation tunnel off".to_string();
    }
    match start(cx) {
        Ok(()) => "Dictation tunnel on. Words sent to Zed from DeskBridge now go into the open chat.".to_string(),
        Err(error) => {
            log::error!("dictation tunnel: {error:#}");
            "Couldn't turn the dictation tunnel on".to_string()
        }
    }
}

fn descriptor_path() -> std::path::PathBuf {
    paths::config_dir().join(DESCRIPTOR_FILE)
}

/// Writes the port and token so only this user can read them (owner-only on Linux and macOS; the Windows config folder is
/// already private to the user).
fn write_descriptor(contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(descriptor_path())?.write_all(contents.as_bytes())
}

fn start(cx: &mut App) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    write_descriptor(&serde_json::json!({ "port": port, "token": token }).to_string())?;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = async_channel::unbounded::<Pending>();

    // The listening thread: plain blocking sockets, one short request per connection, hands each request to the app thread.
    let thread_stop = stop.clone();
    std::thread::Builder::new()
        .name("dictation-tunnel".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                handle_connection(stream, &token, &tx);
            }
        })?;

    // The app thread: applies each request to the open chat and tells the connection how it went.
    cx.spawn(async move |cx| {
        while let Ok(pending) = rx.recv().await {
            let reason = cx.update(|cx| deliver(&pending.request, cx));
            pending.reply.send(reason).ok();
        }
    })
    .detach();

    cx.global_mut::<DictationTunnelState>().0 = Some(RunningTunnel { stop, port });
    Ok(())
}

fn stop(cx: &mut App) {
    if let Some(tunnel) = cx.global_mut::<DictationTunnelState>().0.take() {
        tunnel.stop.store(true, Ordering::Relaxed);
        // wake the thread blocked in accept so it sees the flag and ends
        TcpStream::connect(("127.0.0.1", tunnel.port)).ok();
    }
    std::fs::remove_file(descriptor_path()).ok();
}

fn handle_connection(stream: TcpStream, token: &str, tx: &async_channel::Sender<Pending>) {
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
    let Ok(read_half) = stream.try_clone() else { return };
    let mut line = String::new();
    if BufReader::new(read_half.take(MAX_LINE_BYTES)).read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut stream = stream;
    let reply = match parse_request(line.trim_end(), token) {
        Err(refusal) => refusal.reason(),
        Ok(request) => {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if tx.send_blocking(Pending { request, reply: reply_tx }).is_err() {
                "closed"
            } else {
                reply_rx.recv_timeout(REPLY_TIMEOUT).unwrap_or("timeout")
            }
        }
    };
    let body = if reply.is_empty() {
        r#"{"ok":true}"#.to_string()
    } else {
        serde_json::json!({ "ok": false, "reason": reply }).to_string()
    };
    stream.write_all(body.as_bytes()).ok();
    stream.write_all(b"\n").ok();
    stream.shutdown(Shutdown::Both).ok();
}

/// Puts the request into the open agent thread. Returns "" on success, or the reason it could not.
fn deliver(request: &DictationRequest, cx: &mut App) -> &'static str {
    let mut windows: Vec<WindowHandle<MultiWorkspace>> = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect();
    // the window in front goes first
    if let Some(active) = cx.active_window().and_then(|window| window.downcast::<MultiWorkspace>()) {
        windows.retain(|window| window.window_id() != active.window_id());
        windows.insert(0, active);
    }

    for window in windows {
        let delivered = window.update(cx, |multi_workspace, window, cx| {
            for workspace in multi_workspace.workspaces() {
                let workspace = workspace.clone();
                let done = workspace.update(cx, |workspace, cx| {
                    let panel = workspace.panel::<AgentPanel>(cx)?;
                    let thread_view = panel.read(cx).active_thread_view(cx)?;
                    thread_view.update(cx, |thread_view, cx| {
                        thread_view.message_editor.update(cx, |message_editor, cx| {
                            message_editor.apply_dictation(
                                request.backspace,
                                &request.text,
                                request.submit,
                                window,
                                cx,
                            );
                        });
                    });
                    Some(())
                });
                if done.is_some() {
                    return true;
                }
            }
            false
        });
        if matches!(delivered, Ok(true)) {
            return "";
        }
    }
    "no-chat"
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn words_backspace_and_submit_are_parsed() {
        let request = parse_request(&format!(r#"{{"token":"{TOKEN}","text":"hello","backspace":2,"submit":true}}"#), TOKEN).unwrap();
        assert_eq!(request, DictationRequest { text: "hello".into(), backspace: 2, submit: true });
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused() {
        assert_eq!(parse_request(r#"{"token":"nope","text":"hi"}"#, TOKEN), Err(Refusal::BadToken));
        assert_eq!(parse_request(r#"{"text":"hi"}"#, TOKEN), Err(Refusal::BadRequest));
    }

    #[test]
    fn garbage_and_empty_requests_are_refused() {
        assert_eq!(parse_request("not json", TOKEN), Err(Refusal::BadRequest));
        assert_eq!(parse_request(&format!(r#"{{"token":"{TOKEN}"}}"#), TOKEN), Err(Refusal::BadRequest));
    }

    #[test]
    fn backspace_is_clamped() {
        let request = parse_request(&format!(r#"{{"token":"{TOKEN}","backspace":99999}}"#), TOKEN).unwrap();
        assert_eq!(request.backspace, MAX_BACKSPACE);
    }

    #[test]
    fn token_comparison_checks_length_and_content() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
