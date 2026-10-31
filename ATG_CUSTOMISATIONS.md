# ATG Zed: what differs from upstream

- **Focus mode on by default:** thinking, code changes and command output start folded in agent chats; click to open.
- **Idle chats unload after 2 hours** (`agent.idle_thread_unload_minutes`, 0 turns it off). They reload from disk when opened; working chats are never unloaded.
- **Claude Code is added as an External Agent** (see the intranet page "How we keep laptops fast").
- Everything else is upstream Zed; keep this list short and update it with every change.
