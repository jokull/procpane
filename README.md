# procpane

An agent-friendly process runner for `turbo.json` monorepos. Run your `dev` tasks in the background and let AI agents grep, tail, and poll their logs.

## Why

Turborepo's TUI is great for humans, opaque to agents — it writes to an in-memory ratatui buffer, copy/paste is broken, no programmatic query. Stream mode (`--ui=stream`) interleaves all tasks with no per-task addressing. Persistent tasks (`dev`) leave no log artifacts at all.

`procpane` is an independent, in-memory runner that consumes the same `turbo.json` you already have. Use `turbo run build` for builds and remote cache; use `procpane run <task>` for agent-driven `dev` sessions.

## Install

```bash
cargo install --path .
```

Or `cargo build --release` and use `target/release/procpane`.

Requires macOS or Linux. Windows is not supported.

## Usage

All commands are non-blocking. `run` daemonizes and returns immediately.

```bash
procpane run <task...>          # launch tasks, detach, return
procpane run --foreground <task>  # don't detach
procpane status                 # list procs (table or --json)
procpane stop                   # SIGINT then SIGKILL the run
procpane proc <name> tail -n 50     # last N lines
procpane proc <name> grep PATTERN -A 3 -B 3
procpane proc <name> since <cursor>  # incremental polling
procpane grep PATTERN           # cross-task grep, prefixed with task name
```

Task names follow the same rules as `turbo run`: bare task names (`dev`) expand to every package that has the script; qualified ids (`web#dev`) target one. Short package names work — both `web#dev` and `@demo/web#dev` resolve to the same task.

## Example

```bash
# Start dev: vite frontend + hono api + shared lib build
procpane run web#dev

# Find an error in any task
procpane grep -i 'error|warn'

# Watch one task incrementally
CURSOR=0
while true; do
  OUT=$(procpane proc '@demo/api#dev' since $CURSOR --json)
  CURSOR=$(echo "$OUT" | jq .next_cursor)
  echo "$OUT" | jq -r '.lines[].text'
  sleep 2
done

# Stop everything (SIGINT → 5s grace → SIGKILL of every task's process group)
procpane stop
```

## How it works

- **One daemon per repo**, lifetime bound by a PID lockfile at `${repo}/.procpane/lock`.
- **Unix socket** at `${repo}/.procpane/sock` carries length-prefixed JSON.
- **Per-task ring buffer**, 2048 lines, ANSI-stripped on ingress. Lines have a monotonic seq number; `since <cursor>` returns lines with `seq >= cursor`.
- **One PTY per task** so Vite/Next/esbuild don't downgrade their output for a non-TTY.
- **Process groups** (`setpgid`) so `kill(-pgid, SIGINT)` reaps `next dev`'s workers cleanly.
- **DAG via `petgraph`**: `dependsOn`, `^`-topological deps, and `pkg#task` explicit deps are honored. `with:` siblings launch alongside without a graph edge.
- **In-memory only**: no logs on disk. Stopping the daemon discards the buffers. Persistence is the agent's job (pipe `tail --json` to a file).
- **Build phase delegated to turbo.** Non-persistent dependencies (e.g. `^build` chains) run as a single `<pm> exec turbo run …` step before any persistent task spawns. You get turbo's cache for free — warm-cache dev boot is sub-second. The combined output lands in a `procpane#prebuild` buffer; if turbo exits non-zero, the run aborts. Pass `--no-prebuild` to skip this and manage builds yourself.

## Compatibility

`procpane` parses the subset of `turbo.json` that matters for orchestration:

- `tasks.<n>.dependsOn` ✓
- `tasks.<n>.persistent` ✓
- `tasks.<n>.with` ✓
- `tasks.<n>.env`, `passThroughEnv` ✓ (parsed; PATH is auto-augmented with each ancestor `node_modules/.bin`)
- `tasks.<n>.interactive` — refused with an error (no MVP support)
- `tasks.<n>.cache`, `outputs`, `inputs`, `outputLogs` — parsed and ignored (no caching)

If `turbo run <task>` works, `procpane run <task>` resolves the same task graph for these fields.

## Non-goals

- Caching, hashing, remote cache (use `turbo run build`)
- File-change-triggered task restart (use the task's own watcher — Vite HMR, tsx watch, tsc --watch)
- Interactive (`interactive: true`) tasks
- Windows
- On-disk log persistence
