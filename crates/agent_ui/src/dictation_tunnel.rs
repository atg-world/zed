//! Dictation tunnel: lets another program on this PC (DeskBridge, driven from a phone) speak words into the agent panel
//! while the tunnel is switched on, whichever window is in front.
//!
//! Off by default. `agent: toggle dictation tunnel` turns it on or off (or start Zed with ZED_DICTATION_TUNNEL=1). While it is on:
//!   * Zed listens on 127.0.0.1 only (loopback, a random port) and writes `{port, token}` to
//!     `<config dir>/dictation-tunnel.json` (on Windows `%APPDATA%\Zed\`), which only this user can read.
//!   * A client sends one JSON line and gets one line back: `{"ok": true, ...}` or `{"ok": false, "reason": "..."}`.
//!       - words for the open chat: `{"token", "text"?, "backspace"?, "submit"?}`; they go into the message box of the open
//!         agent thread (the window in front first, then any window). Nothing is ever typed into another window: no open
//!         thread means `no-chat`.
//!       - `{"token", "op": "chats"}`: every chat (not archived) with its project, status (`running`, `needs-you`, `idle`)
//!         and last-updated time, newest first.
//!       - `{"token", "op": "chat", "id", "limit"?}`: the last messages of one chat (user, assistant, tool) and its status.
//!         A chat that is not loaded yet is loaded in the background; the answer is `loading` until it is ready.
//!       - `{"token", "op": "say", "id", "text"?, "submit"?}`: words for one specific chat, not just the open one.
//!     This is how the phone shows the same list of chats the sidebar shows, and talks into any of them.
//! Turning it off closes the listener and deletes the file, so a stale file simply means "off" to the client.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use acp_thread::{AgentThreadEntry, ThreadStatus};
use gpui::{App, Context, Entity, Global, Window, WindowHandle, actions};
use serde::Deserialize;
use workspace::{MultiWorkspace, Workspace};

use crate::thread_metadata_store::{ThreadMetadata, ThreadMetadataStore};
use crate::{Agent, AgentPanel, AgentThreadSource, ConversationView};

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
const MAX_CHAT_ENTRIES: usize = 200;
/// One message longer than this is cut (the phone shows a preview; the full text is in Zed).
const MAX_ENTRY_CHARS: usize = 4_000;
/// The whole answer stays well under the 200 KB the phone side accepts.
const MAX_REPLY_CHARS: usize = 120_000;
const MAX_CHATS: usize = 100;

#[derive(Debug, PartialEq)]
pub(crate) enum Operation {
    /// words for the open chat
    Words,
    Chats,
    Chat { id: String, limit: usize },
    Say { id: String },
}

#[derive(Debug, PartialEq)]
pub(crate) struct DictationRequest {
    pub operation: Operation,
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
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
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
    let id = wire.id.filter(|id| !id.is_empty() && id.len() <= 64);
    let operation = match wire.op.as_deref() {
        None => {
            if wire.text.is_empty() && wire.backspace == 0 && !wire.submit {
                return Err(Refusal::BadRequest);
            }
            Operation::Words
        }
        Some("chats") => Operation::Chats,
        Some("chat") => Operation::Chat {
            id: id.ok_or(Refusal::BadRequest)?,
            limit: wire.limit.unwrap_or(40).clamp(1, MAX_CHAT_ENTRIES),
        },
        Some("say") => {
            if wire.text.is_empty() && !wire.submit && wire.backspace == 0 {
                return Err(Refusal::BadRequest);
            }
            Operation::Say { id: id.ok_or(Refusal::BadRequest)? }
        }
        Some(_) => return Err(Refusal::BadRequest),
    };
    Ok(DictationRequest {
        operation,
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
    reply: mpsc::SyncSender<serde_json::Value>,
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
    // Opt in without a menu: ZED_DICTATION_TUNNEL=1 starts the tunnel with Zed (for the phone app, and for tests).
    if std::env::var("ZED_DICTATION_TUNNEL").is_ok_and(|value| value == "1") {
        if let Err(error) = start(cx) {
            log::error!("dictation tunnel (from environment): {error:#}");
        }
    }
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
            let reply = cx.update(|cx| deliver(&pending.request, cx));
            pending.reply.send(reply).ok();
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
        Err(refusal) => refused(refusal.reason()),
        Ok(request) => {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if tx.send_blocking(Pending { request, reply: reply_tx }).is_err() {
                refused("closed")
            } else {
                reply_rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| refused("timeout"))
            }
        }
    };
    let body = reply.to_string();
    stream.write_all(body.as_bytes()).ok();
    stream.write_all(b"\n").ok();
    stream.shutdown(Shutdown::Both).ok();
}

fn refused(reason: &str) -> serde_json::Value {
    serde_json::json!({ "ok": false, "reason": reason })
}

fn accepted() -> serde_json::Value {
    serde_json::json!({ "ok": true })
}

/// Every Zed window, the one in front first.
fn zed_windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    let mut windows: Vec<WindowHandle<MultiWorkspace>> = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect();
    if let Some(active) = cx.active_window().and_then(|window| window.downcast::<MultiWorkspace>()) {
        windows.retain(|window| window.window_id() != active.window_id());
        windows.insert(0, active);
    }
    windows
}

/// The agent panels of one window, with each chat they hold in memory (the open one and the retained ones).
fn conversations_in(multi_workspace: &MultiWorkspace, cx: &App) -> Vec<(Entity<AgentPanel>, Entity<ConversationView>)> {
    let mut found = Vec::new();
    for workspace in multi_workspace.workspaces() {
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else { continue };
        for conversation in panel.read(cx).conversation_views() {
            found.push((panel.clone(), conversation));
        }
    }
    found
}

fn chat_status(conversation: &ConversationView, cx: &App) -> &'static str {
    if conversation.root_thread_has_pending_tool_call(cx) {
        return "needs-you"; // waiting for an approval that only Zed can give
    }
    match conversation.root_thread(cx).map(|thread| thread.read(cx).status()) {
        Some(ThreadStatus::Generating) => "running",
        _ => "idle",
    }
}

fn project_name(metadata: &ThreadMetadata) -> String {
    metadata
        .main_worktree_paths()
        .paths()
        .first()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "No project".to_string())
}

fn cut(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push('…');
    cut
}

/// Runs the request against the open Zed windows and builds the answer.
fn deliver(request: &DictationRequest, cx: &mut App) -> serde_json::Value {
    match &request.operation {
        Operation::Words => deliver_words(request, cx),
        Operation::Chats => list_chats(cx),
        Operation::Chat { id, limit } => read_chat(id, *limit, cx),
        Operation::Say { id } => say(id, request, cx),
    }
}

/// Words for the open chat: the window in front first, then any other window.
fn deliver_words(request: &DictationRequest, cx: &mut App) -> serde_json::Value {
    for window in zed_windows(cx) {
        let delivered = window.update(cx, |multi_workspace, window, cx| {
            for workspace in multi_workspace.workspaces() {
                let workspace = workspace.clone();
                let done = workspace.update(cx, |workspace, cx| {
                    let panel = workspace.panel::<AgentPanel>(cx)?;
                    let thread_view = panel.read(cx).active_thread_view(cx)?;
                    thread_view.update(cx, |thread_view, cx| {
                        thread_view.message_editor.update(cx, |message_editor, cx| {
                            message_editor.apply_dictation(request.backspace, &request.text, request.submit, window, cx);
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
            return accepted();
        }
    }
    refused("no-chat")
}

/// Every chat that is not archived, newest first, the way the sidebar lists them.
fn list_chats(cx: &mut App) -> serde_json::Value {
    let mut live: std::collections::HashMap<String, &'static str> = std::collections::HashMap::new();
    for window in zed_windows(cx) {
        window
            .update(cx, |multi_workspace, _window, cx| {
                for (_panel, conversation) in conversations_in(multi_workspace, cx) {
                    let conversation = conversation.read(cx);
                    live.insert(conversation.thread_id.to_key_string(), chat_status(conversation, cx));
                }
            })
            .ok();
    }
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return refused("no-chat");
    };
    let mut chats: Vec<&ThreadMetadata> = store.read(cx).entries().filter(|m| !m.archived && !m.is_draft()).collect();
    chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let chats: Vec<serde_json::Value> = chats
        .into_iter()
        .take(MAX_CHATS)
        .map(|m| {
            let id = m.thread_id.to_key_string();
            serde_json::json!({
                "id": id,
                "title": cut(&m.display_title(), 200),
                "project": project_name(m),
                "status": live.get(&id).copied().unwrap_or("idle"),
                "updated": m.updated_at.to_rfc3339(),
            })
        })
        .collect();
    serde_json::json!({ "ok": true, "chats": chats })
}

/// Finds the chat in memory in any window, or starts loading it (answer `loading`) so the next ask finds it.
fn with_chat<R>(
    id: &str,
    cx: &mut App,
    mut use_chat: impl FnMut(&Entity<ConversationView>, &mut Window, &mut Context<MultiWorkspace>) -> R,
) -> Result<R, &'static str> {
    for window in zed_windows(cx) {
        let outcome = window.update(cx, |multi_workspace, window, cx| {
            let conversations = conversations_in(multi_workspace, cx);
            if let Some((_, conversation)) = conversations.iter().find(|(_, c)| c.read(cx).thread_id.to_key_string() == id) {
                return Some(use_chat(conversation, window, cx));
            }
            None
        });
        if let Ok(Some(result)) = outcome {
            return Ok(result);
        }
    }
    // not in memory: ask the first agent panel to load it (without stealing focus), and tell the caller to ask again
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return Err("no-chat");
    };
    let Some(metadata) = store.read(cx).entries().find(|m| m.thread_id.to_key_string() == id).cloned() else {
        return Err("no-chat");
    };
    for window in zed_windows(cx) {
        let started = window.update(cx, |multi_workspace, window, cx| {
            for workspace in multi_workspace.workspaces() {
                if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    let metadata = metadata.clone();
                    panel.update(cx, |panel, cx| {
                        panel.load_agent_thread(
                            Agent::from(metadata.agent_id.clone()),
                            metadata.thread_id,
                            Some(metadata.worktree_paths.folder_path_list().clone()),
                            metadata.title.clone(),
                            false,
                            AgentThreadSource::AgentPanel,
                            window,
                            cx,
                        );
                    });
                    return true;
                }
            }
            false
        });
        if matches!(started, Ok(true)) {
            return Err("loading");
        }
    }
    Err("no-chat")
}

fn read_chat(id: &str, limit: usize, cx: &mut App) -> serde_json::Value {
    let read = with_chat(id, cx, |conversation, _window, cx| {
        let conversation = conversation.read(cx);
        let status = chat_status(conversation, cx);
        let Some(thread) = conversation.root_thread(cx) else {
            return refused("loading");
        };
        let thread = thread.read(cx);
        let entries = thread.entries();
        let total = entries.len();
        // newest messages first until the answer is full, then put them back in order
        let mut budget = MAX_REPLY_CHARS;
        let mut picked: Vec<serde_json::Value> = Vec::new();
        for (index, entry) in entries.iter().enumerate().rev().take(limit) {
            let role = match entry {
                AgentThreadEntry::UserMessage(_) => "user",
                AgentThreadEntry::AssistantMessage(_) => "assistant",
                AgentThreadEntry::ToolCall(_) => "tool",
                _ => "note",
            };
            let text = cut(&entry.to_markdown(cx), MAX_ENTRY_CHARS);
            if text.len() > budget {
                break;
            }
            budget -= text.len();
            picked.push(serde_json::json!({ "i": index, "role": role, "text": text }));
        }
        picked.reverse();
        serde_json::json!({
            "ok": true,
            "id": id,
            "title": cut(&thread.title().map(|t| t.to_string()).unwrap_or_default(), 200),
            "status": status,
            "total": total,
            "entries": picked,
        })
    });
    read.unwrap_or_else(|reason| refused(reason))
}

fn say(id: &str, request: &DictationRequest, cx: &mut App) -> serde_json::Value {
    let said = with_chat(id, cx, |conversation, window, cx| {
        let Some(thread_view) = conversation.read(cx).root_thread_view() else {
            return false;
        };
        thread_view.update(cx, |thread_view, cx| {
            thread_view.message_editor.update(cx, |message_editor, cx| {
                message_editor.apply_dictation(request.backspace, &request.text, request.submit, window, cx);
            });
        });
        true
    });
    match said {
        Ok(true) => accepted(),
        Ok(false) => refused("loading"),
        Err(reason) => refused(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn words_backspace_and_submit_are_parsed() {
        let request = parse_request(&format!(r#"{{"token":"{TOKEN}","text":"hello","backspace":2,"submit":true}}"#), TOKEN).unwrap();
        assert_eq!(request, DictationRequest { operation: Operation::Words, text: "hello".into(), backspace: 2, submit: true });
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
    fn chat_operations_are_parsed_and_validated() {
        let chats = parse_request(&format!(r#"{{"token":"{TOKEN}","op":"chats"}}"#), TOKEN).unwrap();
        assert_eq!(chats.operation, Operation::Chats);
        let chat = parse_request(&format!(r#"{{"token":"{TOKEN}","op":"chat","id":"abc","limit":9999}}"#), TOKEN).unwrap();
        assert_eq!(chat.operation, Operation::Chat { id: "abc".into(), limit: MAX_CHAT_ENTRIES });
        let say = parse_request(&format!(r#"{{"token":"{TOKEN}","op":"say","id":"abc","text":"hi","submit":true}}"#), TOKEN).unwrap();
        assert_eq!(say.operation, Operation::Say { id: "abc".into() });
        assert!(say.submit);
    }

    #[test]
    fn chat_operations_without_an_id_or_with_nothing_to_say_are_refused() {
        assert_eq!(parse_request(&format!(r#"{{"token":"{TOKEN}","op":"chat"}}"#), TOKEN), Err(Refusal::BadRequest));
        assert_eq!(parse_request(&format!(r#"{{"token":"{TOKEN}","op":"say","id":"abc"}}"#), TOKEN), Err(Refusal::BadRequest));
        assert_eq!(parse_request(&format!(r#"{{"token":"{TOKEN}","op":"say","text":"hi"}}"#), TOKEN), Err(Refusal::BadRequest));
        assert_eq!(parse_request(&format!(r#"{{"token":"{TOKEN}","op":"rm -rf"}}"#), TOKEN), Err(Refusal::BadRequest));
        let long_id = "x".repeat(65);
        assert_eq!(parse_request(&format!(r#"{{"token":"{TOKEN}","op":"chat","id":"{long_id}"}}"#), TOKEN), Err(Refusal::BadRequest));
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis() {
        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("abcdefghij", 4), "abcd…");
    }

    #[test]
    fn token_comparison_checks_length_and_content() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
