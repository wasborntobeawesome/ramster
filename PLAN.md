# ramster plan

ramster started from a real 48 GB MacBook running at memory pressure "warning" all day: roughly 20 GB compressed
and swap growing, even though about 30 GB of it was held by tools that were doing nothing (idle Gradle/Kotlin
daemons, a Docker VM reserving 8+ GB for a 12 MB container, a days-old Android emulator, an idle design app).
More RAM doesn't fix that; idle tools never give theirs back.

## Why developer Macs run out of RAM
1. **Build tools never exit on their own.** Gradle keeps a daemon per version and JVM-args combination for 3 h.
   Several projects or agents building in parallel means several 2–6 GB JVMs.
2. **VMs and emulators reserve memory up front and keep it.** Docker sizes its VM for the worst case,
   the emulator grows over days, and simulators boot hundreds of processes.
3. **Agent sessions multiply everything.** Each session starts its own MCP servers (and LSPs from plugins).
   Agents also start builds and emulators but never stop them.
4. **Context switching.** Backend (Docker), Android (Gradle + emulator), iOS (Simulator), design (Figma)
   and web (two browsers) all stay open at once, even though you work on one at a time.

## Roadmap: from personal tool to universal

### Language and dependencies
Rust, chosen for the smallest footprint: a ~670 KB binary at v0.1 (~840 KB at v0.2) and a 4.6 MB daemon (the first Go prototype was
2.6 MB and 8.3 MB). Dependencies are `libc`, `serde` and `serde_json` only.
- **sysinfo** was rejected for macOS: it reports RSS, not phys_footprint, and has no pressure or compressor
  breakdown. It is the plan for the Linux backend.
- **libproc-rs** was rejected: it has no cwd on macOS and needs bindgen/libclang at build time.
- The official MCP Rust SDK **rmcp** was rejected for now: it requires tokio and adds about 2 MB. The protocol
  surface we need (initialize, server/discover, tools/list, tools/call, ping) is about 250 lines of sync JSON-RPC.
  Revisit if spec churn makes hand-rolling costly.
- **clap** was rejected: the CLI is small enough to parse by hand.

### Prior art (Nov 2026)
Orphan reapers exist (zclean, cc-reaper, proc-janitor), as do Gradle-daemon killers (DaemonHunter, daemonitor)
and monitors (Stats, macmon, btop). The orphaned-MCP problem is widely reported (Claude Code issues #33947 and
#66280, the Cursor forum). Nothing combines footprint-accurate measurement, dev-aware rules, safe freeing,
history and MCP. We borrowed two ideas: zclean's exclusions for process managers (tmux, pm2, supervisord) and
proc-janitor's grace period (we require 2 min of age).

### v0.1 (current)
- macOS backend (libproc, mach), app grouping via responsible PID with ancestor fallback
- Rules: Gradle/Kotlin/Maven daemons, Android emulator, iOS Simulator, Docker and other VMs, orphaned MCP/LSP,
  detached processes, nx/turbo daemons, idle GUI apps (history-backed), duplicate MCP, agents, browsers
- CLI with `--json`, MCP server, launchd sampler, behaviour report, notifications, opt-in auto-free

### v0.1.1: menu bar app (done)
- `ramster-bar`: native AppKit status item written in Rust (objc2). Idle 16 MB, 0% CPU; ~26 MB after the menu
  has been used. SwiftUI was rejected (its runtime costs 2–4× the RAM) and so were Tauri and Electron (webview).
- Measured facts: the ~9 MB growth after the first open is ColorSync/AppKit drawing caches, not ramster.
  It stays flat across reopens, and `leaks` reports 0 leaks.

### v0.2: easy to adopt (done, waiting on a first release)
- Menu bar: launch at login (SMAppService), app icon, Developer ID signing + notarization (Makefile + release
  workflow; needs a Developer ID certificate and the GitHub secrets)
- Universal arm64/x86_64 build, `make dist`, release workflow, Homebrew formula + cask templates
  (`packaging/homebrew`, to copy into a `homebrew-tap` repo), crate ready for `cargo publish`.
  We used a small hand-written workflow instead of cargo-dist: it needs no extra tool and handles the
  .app bundle and notarization, which cargo-dist does not.
- `ramster setup`: detects Claude Code, Codex, Cursor, Gemini CLI and opencode; registers the MCP server and adds
  the CLAUDE.md/AGENTS.md line, asking per step, idempotent, with backups and `--remove`
- More rules: Ollama/LM Studio/llama.cpp/MLX/vLLM, Xcode (SourceKitService, build service), JetBrains IDEs,
  VS Code-style extension hosts, Jest/Vitest, Next/Vite/webpack/Astro/Nuxt/Storybook/Metro dev servers, Electron apps
- Rules as data: `~/.ramster/rules.d/*.json` (match by name/args/path/app, tier, when, action), `ramster rules`
- History files older than a day are gzipped (~14× smaller)
- Still open: publish (tap repo, crates.io, first tagged release); LICENSE file

### v0.3: other platforms
- Linux backend (`sysinfo` or `procfs`): `/proc/*/smaps_rollup` (PSS), `/proc/pressure/memory` (PSI) for pressure, cgroup v2 per systemd unit;
  systemd user service instead of launchd; `systemctl --user stop` actions
- Windows backend (working set / private bytes, `taskkill`), lower priority

### v0.4: smarter
- Learn per-user patterns from history ("the emulator is usually idle after 19:00") and suggest at the right time
- Per-agent accounting: memory per Conductor workspace / agent session, so a multi-agent orchestrator
  can schedule builds instead of running five at once

### Principles
- Local only: no telemetry, no network.
- Never interrupt work: busy processes are never touched, PID identity is verified, and anything a human might be
  using needs confirmation.
- Stay tiny: the tool that frees RAM must not cost RAM (daemon < 5 MB, binary < 1 MB, history < 5 MB a day).
