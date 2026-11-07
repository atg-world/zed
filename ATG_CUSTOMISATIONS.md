# ATG Zed: what differs from upstream

- **Focus mode on by default:** thinking, code changes and command output start folded in agent chats; click to open.
- **Idle chats unload after 2 hours** (`agent.idle_thread_unload_minutes`, 0 turns it off). They reload from disk when opened; working chats are never unloaded.
- **Claude Code is added as an External Agent** (see the intranet page "How we keep laptops fast").
- **Dictation tunnel** (command: `agent: toggle dictation tunnel`, off by default): while on, a program on this PC (DeskBridge) can speak words into the open agent chat without any window in front. Zed listens on 127.0.0.1 only and writes `{port, token}` to `%APPDATA%\Zed\dictation-tunnel.json`; turning it off deletes the file. See `crates/agent_ui/src/dictation_tunnel.rs`.
- **A file opened from the full-screen chat opens beside it:** the chat shrinks back to its normal width and stays open (upstream closes it). Revealing another panel still hides a full-screen one. See `Workspace::dismiss_zoomed_items_to_reveal`.
- **HTML pages open rendered** (`crates/html_preview`, Windows): opening an `.html` / `.htm` file shows the page, drawn by the system WebView2 in a small window floating over the tab (Zed's own window hides child windows). Nothing runs until a page is open; closing the tab releases it all. Links to websites, new windows and downloads are refused. Measured 4 Nov 2026: about 135 MB of its own memory while open, 0 when closed. Command `html preview: edit source` opens the text. Markdown opens rendered through the existing setting `markdown_preview.open_markdown_files_in_preview` (set in the user settings).
- Everything else is upstream Zed; keep this list short and update it with every change.
