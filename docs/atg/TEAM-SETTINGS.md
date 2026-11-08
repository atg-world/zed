# Zed for the team: what the defaults skip, why, and what it costs (ERP-2333)

**Audience.** People who finish tasks and should never have to think about Zed's settings. Everything here is a default of the ATG fork
(`assets/settings/default.json`), so every person and every project gets it with no file to copy. Evidence is marked
**measured** or **expected**; "expected" means not yet measured on our machines.

## 1. The skip list (in the fork default since 9 Nov 2026)

`file_scan_exclusions` now also skips: `node_modules`, `target`, `.next`, `dist`, `__pycache__`, `.venv`, `venv`, `coverage`
(plus Zed's own `.git`, `.svn`, `.hg`, `.jj`, `.sl`, `.repo`, `CVS`, `.DS_Store`, `Thumbs.db`, `.classpath`, `.settings`).

| | |
|---|---|
| **Why** | These are downloaded or generated, nobody edits them by hand. Zed keeps a record of every file it scans, watches them for changes, and searches them. In a folder that is not a git project nothing else keeps them out. |
| **Measured** | The intranet folder: 204,529 files on disk, 143,335 (70%) in these folders (`node_modules` 97,365; `__pycache__` 22,319; `.venv` 18,837; `.next` 3,317). Turning the list on took one Zed from 403,401 tracked files to 288,376 (-29%). |
| **Helps** | Less scanning work when a project opens, fewer file watchers, faster file search and project search, no junk in results. Everyone who opens a folder that holds these. |
| **Not proven** | Memory. Zed's total memory did not fall by a visible amount (1,069 MiB before, 1,062 MiB after, in the same session), and see section 2: file count is not the main driver of memory. Treat this as a CPU, search and tidiness win until measured otherwise. |

### Drawbacks, honestly

1. Files in those folders disappear from the file tree, file search, project search and the agent's `@` file picker. Nothing is deleted. (Claude's own tools read the disk directly and still see them.)
2. A folder that has one of these names but is **not** generated becomes invisible: a hand-written `dist` of a static site, a Cargo or Maven `target` whose reports you want to read, an HTML `coverage` report, a checked-in `venv`. Open such a file by path (File > Open) or override in the project.
3. A project that sets its **own** `file_scan_exclusions` in `.zed/settings.json` **replaces** this list. It must copy these lines or it loses the skip. This is the one trap.
4. Debugging into a library inside `node_modules` is not possible from Zed. (Language servers are off in the team profile anyway.)

## 2. What actually moves Zed's memory (measured 8-9 Nov 2026)

| Time | Tracked files | Zed resident memory | Note |
|---|---|---|---|
| 20:55 | 1,183 | 350 MiB | just started, one small folder |
| 21:27 | 293,061 | 1,289 MiB | `Documents` opened as a project |
| 02:10 | 175,765 | 1,683 MiB | new launch, 79 buffers, 2 projects, 3 chats |
| 02:17 | 175,862 | 1,886 MiB | 147 buffers, same files |

File count alone does **not** explain it: fewer files, more memory. The earlier rule of thumb ("300 MB per 100,000 files") was too simple and is withdrawn.
Likely contributors, still to be measured one by one: chat panels that render hundreds of messages, open buffers (the text itself is small, 2.5 MB),
graphics memory (virtual size 4.4 GB), and git work for 12 repositories. The new `memory breakdown` log line records buffers, folders and repositories every
time Zed logs memory, so each can be tied to the growth.

What `Documents` still holds after the skip list: 420,815 files on disk, of which Zed loads about 174,000. The big ones are data, not code:
`brand-n8n-automation/workflows` 229,310, `intranet/intranet-v2` 46,233, `protoboard/data` 20,192, `vikaas/marketing-tech-be` 12,223.
**Do not open `Documents` itself as a project; open the project folder.** Opening the root loads every project twice.

## 3. The team profile (what a task finisher's Zed should be)

Status: **in fork default** = shipped; **Bassi's file** = in his personal settings only; **proposed** = not applied.

| Setting | Value | Status | What it does | Drawback |
|---|---|---|---|---|
| `file_scan_exclusions` | the list above | in fork default | skips generated folders | section 1 |
| `agent.max_idle_retained_threads` | 1 (Zed's default is 5) | Bassi's file | idle chats beyond this are unloaded and reload from disk when opened | a chat you return to after switching reloads (now about 0.15 s) |
| `enable_language_server` | false (default true) | Bassi's file | no language-server processes (they can use 0.5-2 GB each) | no live error underlines, no go-to-definition, no rename-symbol. Fine for people who direct Claude; **wrong for people who write code by hand** |
| `show_edit_predictions`, `edit_predictions.provider` | false, none | Bassi's file | no background prediction requests | no inline suggestions |
| `show_completions_on_input`, `inlay_hints` | false, off | Bassi's file | less background work | no auto-complete popup |
| `telemetry` | off | Bassi's file | nothing sent | none for the team |
| `auto_update` | false (default true) | Bassi's file | the fork is updated by us, not by Zed's updater | someone must roll out updates |
| `restore_on_startup` | `last_workspace` (default `last_session`) | **proposed** | reopens one project at launch, not every project from last time | people re-open a second project by hand. Expected to cut start-up memory; **not measured** |
| `file_scan_depth` | 3 (default 5) | **proposed** | loads fewer nested folders eagerly in folders that are not git projects | deep files are found on demand, not up front. **Not measured** |
| theme, fonts, keymap, window title, agent path | personal | never a default | taste | n/a |

## 4. Overriding for one project

Put a `.zed/settings.json` in the project. To add a skip, **repeat the whole list** (section 1), then your extra entries. To bring back a folder, leave its entry out.

## 5. How to measure

`powershell -NoProfile -File tools/zed-adapter/memory-by-kind.ps1` (DeskBridge repo): Zed, helpers and engines by memory.
`%LOCALAPPDATA%\Zed\logs\Zed.log`: `memory usage`, `worktree diagnostics`, `memory breakdown`, `[chat-open]` (click-to-chat milliseconds).
