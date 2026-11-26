# ramster

**What is eating your RAM, and how to get it back.** Built for developers who run several AI agents,
build daemons, emulators, Docker and three browsers on one Mac at the same time.

- **Sees memory the way macOS does**: footprint plus compressed memory (Activity Monitor's number), not RSS,
  with helpers grouped under their app (WebKit tabs → Safari, the Docker VM → Docker, MCP servers → AI agents).
- **Knows dev tools**: idle Gradle/Kotlin/Maven daemons, Android emulators, iOS Simulators, the Docker VM,
  nx/turbo daemons, orphaned MCP and language servers left behind by finished agent sessions, duplicate MCP servers,
  loaded local LLMs (Ollama, LM Studio, llama.cpp, MLX, vLLM), Xcode's SourceKit and build services,
  JetBrains IDEs, VS Code/Cursor extension hosts, idle Next/Vite/webpack/Metro dev servers, Jest/Vitest
  leftovers and Electron apps. Add your own with [rule packs](#custom-rules).
- **Frees memory safely**: three tiers (`safe`, `confirm`, `info`). It never touches a busy process, and before
  every signal it checks the process start time so a reused PID is never hit.
- **Learns your habits**: a tiny background sampler (~4.6 MB RAM, ~3 MB of history a day) shows what you keep open
  and forget, what pressure looks like across the day, and what to change.
- **AI-friendly**: `--json` on every command, plus an MCP server so your agents can check headroom before
  heavy builds and clean up after themselves.
- Written in Rust: one ~800 KB binary with three dependencies (`libc`, `serde`, `serde_json`). It reads the
  kernel directly (libproc, host_statistics64) without spawning `ps` or `top`; a full scan of ~1,000 processes
  takes about 20 ms. Everything stays on your machine.

## Install

```sh
make install                 # cargo build --release → ~/.local/bin/ramster (override with PREFIX=...)
# or: cargo install --path .
ramster agent install        # optional: background sampler at login (launchd)
```

Requires macOS (Apple Silicon or Intel) and Rust 1.82+ to build. Linux is on the roadmap ([PLAN.md](PLAN.md)).

## Menu bar app

```sh
make app                     # → dist/ramster.app (menu bar app + CLI inside, ~1.3 MB)
make install-app             # → ~/Applications/ramster.app
```

A native AppKit status item (Rust + objc2: no SwiftUI, no webview). The title shows memory in use,
colored by pressure (orange = warning, red = critical). The menu shows top apps, **Free N GB safely**,
items that need your OK (with a confirmation dialog), advice, the usage report, and a toggle for the
background sampler.

Built to cost as little as possible:

| | ramster-bar | typical SwiftUI menu bar app | Tauri / Electron |
|---|---|---|---|
| RAM idle | **16 MB** | 40–80 MB | 60–300 MB |
| RAM after opening the menu | **~26 MB**, flat across reopens (AppKit's menu drawing caches) | | |
| CPU idle | **0.0%**, one coalesced timer every 15 s that reads system memory only | | |

Processes are scanned only when you open the menu (~20 ms). CPU is measured against the previous scan,
so opening the menu never waits, and freeing runs on a background thread.

## Use

```sh
ramster                      # overview: memory, top apps, what can be freed
ramster status               # fast: memory + top apps
ramster suggest              # every finding with id, tier, size, reason and a prevention tip
ramster free --safe          # free all safe items (idle build daemons, orphaned MCP/LSP servers, ...)
ramster free docker          # free by id or rule name; asks before "confirm" items
ramster free --all --yes     # everything actionable, no questions
ramster report --hours 24    # behaviour: pressure over the day, heaviest and "forgotten" apps, advice
ramster rules                # custom rules loaded from ~/.ramster/rules.d
```

Example:

```
Memory 40.9 GB / 48.0 GB used · 21.0 GB compressed · swap 4.6 GB · pressure WARNING

SAFE TO FREE · 7.4 GB  restarts on demand, nothing lost · ramster free --safe
  4.8 GB    Gradle daemon 9.8.2  [gradle-daemon:89269]
            idle right now, up 2h12m, heap limit 4096m
  2.4 GB    Gradle daemon 9.7.1  [gradle-daemon:80165]
  170 MB    nx daemon  [nx-daemon:96005]

NEEDS YOUR OK · 14.9 GB  stops something you may be using · ramster free <id>
  8.6 GB    Docker VM holds 8.6 GB for 1 container(s)  [docker]
            containers actually use 12 MB; the rest is VM overhead and cache
  6.3 GB    1 booted iOS Simulator(s)  [ios-simulator]
```

### Safety tiers

| tier | meaning | examples |
|---|---|---|
| `safe` | restarts on demand, nobody is using it | idle Gradle/Kotlin daemons (not used recently), orphaned MCP/LSP servers, idle nx/turbo daemons, detached processes whose directory was deleted, Ollama models idle for 20+ min (reloaded on the next request), Xcode services left over after Xcode quit, leftover Jest/Vitest workers |
| `confirm` | stops something a human may still want | Android emulator, iOS Simulators, Docker, idle GUI apps, detached node/python processes, Gradle daemons used in the last 20 min, llama.cpp/MLX/vLLM servers, LM Studio models, a bloated SourceKit while Xcode is open, idle dev servers and watch-mode test runners |
| `info` | advice only | browser memory, duplicate MCP servers, agent sessions, missing Gradle idle timeout, JetBrains IDEs, editor extension hosts (with the heaviest language servers named), Electron apps |

Busy processes (CPU above about 2% right now, or above 1% in recent history) are never suggested.

## Let your AI agents use it (MCP)

```sh
ramster setup              # detect your agents, show the plan, ask before each step
ramster setup --yes        # do every pending step without asking
ramster setup --dry-run    # only show what would change (also the default when not in a terminal)
ramster setup --remove     # undo: unregister the MCP server and remove the instruction line
```

It finds Claude Code, Codex, Cursor, Gemini CLI and opencode, registers the MCP server with each and adds
the `memory_check` line (below) to the agent's global instructions:

| agent | MCP server | instructions |
|---|---|---|
| Claude Code | `claude mcp add --scope user ramster -- <bin> mcp` | `~/.claude/CLAUDE.md` |
| Codex | `codex mcp add ramster -- <bin> mcp` (or `[mcp_servers.ramster]` in `~/.codex/config.toml`) | `~/.codex/AGENTS.md` |
| Cursor | `mcpServers.ramster` in `~/.cursor/mcp.json` | printed for you to paste into Settings → Rules → User Rules |
| Gemini CLI | `mcpServers.ramster` in `~/.gemini/settings.json` | `~/.gemini/GEMINI.md` |
| opencode | `mcp.ramster` in `~/.config/opencode/opencode.json` | `~/.config/opencode/AGENTS.md` |

Steps already done are skipped, so running it again changes nothing. Before changing a file for the first
time ramster saves a copy as `<file>.ramster.bak`; other settings keep their order. Config files with
comments (JSONC) are left alone and ramster prints the snippet to add yourself. Run `make install` first so
agents point at `~/.local/bin/ramster`, not at a build directory.

Or by hand:

```sh
claude mcp add --scope user ramster -- ~/.local/bin/ramster mcp            # Claude Code
codex mcp add ramster -- ~/.local/bin/ramster mcp                           # Codex
```

Tools: `memory_status`, `memory_check {need_mb}`, `memory_suggest`, `memory_free {ids, confirm}`, `memory_report {hours}`.
The server tells agents to call `memory_check` before heavy work, to free only `safe` items on their own,
and to ask you before anything in the `confirm` tier. `memory_free` refuses `confirm` items unless `confirm=true`.

Add this line to your global `CLAUDE.md` / `AGENTS.md` so agents actually use it:

```
Before starting memory-heavy work (Gradle/Xcode builds, emulators, Docker, big test runs), call ramster's memory_check.
```

## Background sampler

`ramster agent install` writes `~/Library/LaunchAgents/dev.ramster.daemon.plist` and starts `ramster daemon`.

- Samples every 60 s; ~4.6 MB footprint, a few ms of CPU per sample, low-priority I/O.
- History: `~/.ramster/history/YYYY-MM-DD.ndjson`, about 3 MB a day, kept 7 days.
- Under sustained pressure it shows a notification with the biggest reclaimable items (at most every 30 min).
- With `"auto_free_safe": true` it frees `safe` items on its own while pressure stays at warning or worse.

`~/.ramster/config.json`:

```json
{
  "interval_sec": 60,
  "retention_days": 7,
  "idle_minutes": 20,
  "notify": true,
  "auto_free_safe": false,
  "never_touch": ["my-important-server"],
  "quit_candidates": []
}
```

`never_touch` matches app names, process names and command lines (case-insensitive substring).

## Custom rules

Drop rule packs into `~/.ramster/rules.d/*.json` to teach ramster about your own tools, and share them
with your team. Examples are in [contrib/rules.d](contrib/rules.d).

```json
{
  "rules": [
    {
      "id": "elasticsearch",
      "title": "Local Elasticsearch node",
      "match": {"args": ["org.elasticsearch.bootstrap.Elasticsearch", "org.opensearch.bootstrap.OpenSearch"]},
      "tier": "confirm",
      "min_mb": 500,
      "when": "idle",
      "action": "term",
      "tip": "Cap the heap for development: -Xms1g -Xmx1g in config/jvm.options.d/heap.options."
    }
  ]
}
```

| field | meaning |
|---|---|
| `id` | rule name; findings get ids like `elasticsearch:1234`, so `ramster free elasticsearch` works |
| `match` | `name` (executable), `args` (command line), `path` (executable path): case-insensitive substrings, a string or a list (any entry matches); `app`: exact app name. Every given field must match. With only `app`, the rule covers the whole app |
| `tier` | `safe`, `confirm` (default) or `info` |
| `min_mb` | ignore smaller matches (default 100) |
| `when` | `idle` (default), `detached` (idle and its terminal or agent is gone) or `always` |
| `action` | `term` (default, SIGTERM the process tree), `quit` (apps only), `none`, or `{"cmd": ["brew", "services", "stop", "postgresql@16"]}` |
| `title`, `detail`, `tip` | text shown with the finding |

Whatever a rule says, ramster never suggests a busy process, only signals your own processes, re-checks
start times before signalling, and honours `never_touch`. `safe` rules can be freed by agents and by
`auto_free_safe` without asking, so use it only for things that restart on demand. `ramster rules` lists
what loaded and points at mistakes; the daemon picks up changes on its next check.

## How it measures

- **Per process**: `proc_pid_rusage` → `ri_phys_footprint` (Activity Monitor's "Memory", includes compressed pages),
  CPU time, `proc_pidinfo` for parent and start time, `KERN_PROCARGS2` for the command line (your own processes only).
- **Grouping**: the responsible-process API (`responsibility_get_pid_responsible_for_pid`) attributes XPC helpers,
  WebKit tabs and VM services to their app; helpers that disclaim responsibility fall back to their nearest app
  ancestor. Agent CLIs and everything they spawn count as "AI agents". Things started from a terminal keep
  their own name.
- **System**: `host_statistics64` (app, wired, compressed, cached), `vm.swapusage`, `kern.memorystatus_vm_pressure_level`.

Processes owned by other users (root daemons, WindowServer) show up in the system totals but not per process,
unless you run ramster as root.

## How it compares

| tool | what it covers | what it lacks |
|---|---|---|
| [zclean](https://github.com/TheStack-ai/zclean), [cc-reaper](https://github.com/theQuert/cc-reaper), [proc-janitor](https://github.com/jhlee0409/proc-janitor) | reap orphaned agent processes (MCP servers, subagents) | no memory model, pressure, history, dev-tool rules or MCP interface |
| [DaemonHunter](https://github.com/PaulWoitaschek/DaemonHunter), [daemonitor](https://github.com/cdsap/daemonitor) | idle Gradle/Kotlin daemons | Gradle only |
| [Stats](https://github.com/exelban/stats), [macmon](https://github.com/vladkens/macmon), [btop](https://github.com/aristocratos/btop) | monitoring | they show memory, they don't tell you what to free or free it |
| system-monitor MCP servers | `ps`/psutil over MCP | RSS only (wrong on macOS), no pressure, no freeing |

ramster combines footprint-accurate measurement, dev-aware rules, safe freeing, habit history and an MCP
interface in one small binary.

## Development

```sh
make test    # unit tests (rules, report, MCP protocol, real-kernel probe checks)
make lint    # rustfmt + clippy -D warnings
```

Layout: `src/lib.rs` (core library), `src/probe` (OS backend), `src/analyze` (grouping, rules in
`rules.rs` and `devtools.rs`, rules.d in `custom.rs`, actions, report), `src/mcp.rs`, `src/daemon.rs` (sampler + launchd), `src/history.rs`, `src/ui.rs` (terminal output),
`src/main.rs` (CLI), `src/bin/ramster-bar.rs` (menu bar app), `packaging/Info.plist`.

The CLI and daemon don't link AppKit; only `ramster-bar` does. `RAMSTER_BAR_SELFTEST=n` opens and closes
the menu n times (for screenshots and memory checks); `RAMSTER_BAR_DEBUG=1` logs rebuilds.
