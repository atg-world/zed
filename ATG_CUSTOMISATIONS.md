# ATG Zed: what differs from upstream

- **Focus mode on by default:** thinking, code changes and command output start folded in agent chats; click to open.
- **Idle chats unload after 2 hours** (`agent.idle_thread_unload_minutes`, 0 turns it off). They reload from disk when opened; working chats are never unloaded.
- **Claude Code is added as an External Agent** (see the intranet page "How we keep laptops fast").
- **Dictation tunnel** (command: `agent: toggle dictation tunnel`, off by default): while on, a program on this PC (DeskBridge) can speak words into the open agent chat without any window in front. Zed listens on 127.0.0.1 only and writes `{port, token}` to `%APPDATA%\Zed\dictation-tunnel.json`; turning it off deletes the file. See `crates/agent_ui/src/dictation_tunnel.rs`.
- Everything else is upstream Zed; keep this list short and update it with every change.
