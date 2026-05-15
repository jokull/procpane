# procpane

**The batteries-included dev runner for Turborepo monorepos.**

You use `turbo`. procpane reads the same `turbo.json` and adds the things turbo isn't trying to be:

- **Healthcheck-gated orchestration.** Dependencies wait until services are *actually* ready, not just spawned.
- **Local HTTPS, no config.** `https://web.test` routes to whatever port Vite picked. Same browser, same cookies, no `:8443` if you want.
- **Secrets in your Keychain, never in git.** Touch ID unlocks them. Per-task allowlists keep them out of every other process — including `postinstall` scripts in `node_modules`.
- **Peer-to-peer secret sharing.** Onboard a teammate with a four-word code and a Touch ID. No vault to buy, no encrypted file to commit, no GitHub key gymnastics.
- **Agent-native logs.** Every task is a queryable ring buffer over a Unix socket. `tail`, `grep`, `since <cursor>` — your coding agent can read your dev session as easily as you can.

Assumes **macOS** and a **Turborepo project**. GitHub handles are used as identity labels for shared peers, but nothing requires GitHub itself.

---

## The five-minute tour

### Bring up your dev stack

```bash
$ procpane up
✓ procpane#prebuild        (turbo cache hit, 0.4s)
✓ shared#build             ready
✓ api#dev                  healthy   https://api.test
✓ web#dev                  healthy   https://web.test
✓ worker#dev               healthy
4 tasks running. Logs at .procpane/sock.
```

One command. Real readiness states (`starting → healthy → ready`), not just "process exists". `web#dev` gets `API_URL=https://api.test` injected automatically when `api` becomes healthy — no `.env` to keep in sync with the port Vite picked.

### Wait for it (for scripts and agents)

```bash
$ procpane wait-for api && curl https://api.test/health
{"ok": true}
```

`wait-for` blocks until the healthcheck passes. Composes cleanly into test scripts, agent recipes, and `make test`-style chains.

### Onboard a teammate without a vault

```bash
# New dev, fresh clone
$ procpane up
✗ Missing 7 required env vars: STRIPE_TEST_KEY, DATABASE_URL, ...
  Ask a teammate to send them:
    procpane env send strawberry-thunder-walrus-pickle

$ procpane env receive strawberry-thunder-walrus-pickle
⏳ Waiting for sender...
```

```bash
# Anyone on the team who has the secrets
$ procpane env send strawberry-thunder-walrus-pickle
About to send to new device:
  STRIPE_TEST_KEY, DATABASE_URL, SENDGRID_KEY, ... (7 keys)
🔓 Touch ID to authorize
✓ Sent.
```

```bash
# Back on the new dev's machine
✓ Received 7 vars, stored in Keychain.
$ procpane up   # just works
```

No `.env.example` to maintain. No "ask in #eng for the staging password." No encrypted blob to rotate when someone leaves.

### Let an agent drive the dev session

```bash
$ claude
> The api task returned 500 on /users — figure out why.
```

The agent runs `procpane proc api#dev grep -i error -B 5 -A 20`, finds the stack trace, opens the file, reads the relevant code, proposes a fix, runs `procpane wait-for api` after restart, and verifies with `curl https://api.test/users`. No screenshotting a TUI. No "can you copy the error and paste it here." It just works because every primitive is queryable.

---

## Install

```bash
brew install jokull/tap/procpane    # (planned)
# or
cargo install --path .
```

### First-run setup (one Touch ID, ever)

```bash
$ procpane trust install
This will install procpane's local Certificate Authority into your
System keychain so browsers trust https://*.test URLs.

  Add to /Library/Keychains/System.keychain (sudo)
🔓 Touch ID or password
✓ CA installed. https://*.test:8443 is now trusted.

(Optional) Drop the :8443 by adding a pf redirect:
  procpane trust install --pretty-urls
```

`--pretty-urls` is one more (optional) sudo to redirect `:443 → :8443` via macOS `pf`. The daemon itself never runs as root. `procpane trust uninstall` reverses everything cleanly.

---

## Healthcheck-gated orchestration

Tasks declare what "ready" means. Dependencies wait on it.

```toml
# procpane.toml
[tasks."postgres#dev"]
healthcheck.tcp = 5432

[tasks."migrate#run"]
healthcheck.exit = 0          # ran to completion, exit 0

[tasks."api#dev"]
hostname = "api.test"
healthcheck.http = "/health"
depends_on.postgres = "healthy"
depends_on.migrate = "completed"

[tasks."web#dev"]
hostname = "web.test"
healthcheck.log = "ready in"  # match against task stdout
depends_on.api = "healthy"
```

Healthcheck kinds:
- `tcp = <port>` — port is accepting connections
- `http = "<path>"` — `GET https://<hostname><path>` returns 2xx
- `log = "<regex>"` — task stdout matches the pattern (perfect for Vite's `ready in 423ms`)
- `exit = 0` — task ran to completion with the given exit code (one-shot tasks like migrations)

Status surfaces in `procpane status` and is queryable: `procpane wait-for <task>` blocks until healthy; `procpane status --json` returns the full state machine for agents.

---

## Local HTTPS for `*.test`

Map each task to a hostname. procpane runs a reverse proxy that routes to whatever ephemeral port the task picked, with a valid cert from the local CA.

```toml
[tasks."web#dev"]
hostname = "web.test"

[tasks."api#dev"]
hostname = "api.test"
```

```bash
$ procpane up
✓ web#dev  healthy   https://web.test:8443  → :54231
✓ api#dev  healthy   https://api.test:8443  → :54232
```

Other tasks automatically receive these as env: `web#dev` gets `API_URL=https://api.test:8443`. No `.env` drift when ports change.

**Stretch:** `procpane expose api --public` pins a cloudflared tunnel to `api.test`, useful for Stripe/GitHub webhook testing without leaving your editor.

---

## Secrets: Keychain + peer-to-peer

The model:

1. **Secrets live in your Keychain**, per-repo namespace, Touch-ID-gated.
2. **Tasks declare what they need** in `procpane.toml`. Anything not declared is invisible to that process — including malicious `postinstall` scripts.
3. **Sharing is point-to-point** over an authenticated channel (magic-wormhole), with the sender approving each transfer by Touch ID.
4. **Nothing touches disk or git.**

```toml
[tasks."api#dev"]
env_from = ["STRIPE_TEST_KEY", "DATABASE_URL"]   # explicit allowlist

[tasks."web#dev"]
env_from = ["PUBLIC_POSTHOG_KEY"]                # cannot see Stripe keys
```

```bash
$ procpane env set STRIPE_TEST_KEY
Value: ********              # never appears on disk
✓ Stored in Keychain.

$ procpane env list
STRIPE_TEST_KEY     used by:  api#dev
DATABASE_URL        used by:  api#dev
PUBLIC_POSTHOG_KEY  used by:  web#dev

$ procpane env send <code>      # share with a teammate, Touch ID to confirm
$ procpane env push @alice      # update a peer you've shared with before
```

### Why agents can't exfiltrate them

Coding agents run as your user. They can shell out, `curl`, `cat ~/.zshrc`. If you put `STRIPE_LIVE_KEY=...` in `.env`, the agent can read it the same way it reads any file.

procpane's per-task allowlist means the agent's spawned process — and any subprocess it forks, including a tool call — sees only the env vars its task is declared to use. Reading the Keychain directly requires Touch ID. An agent running `next dev` in the `web#dev` slot literally cannot see `STRIPE_TEST_KEY` because it was never put into its process env.

This is not a sandbox; a sufficiently determined attacker can prompt-inject the agent into prompting you for Touch ID. But it raises the floor from "every env var is in scope for every process" to "every env var is in scope for exactly the task that declared it."

---

## Agent ergonomics

Every task is a queryable ring buffer. One daemon, one Unix socket, length-prefixed JSON.

```bash
procpane status                       # what's running, what state
procpane status --json                # for agents
procpane proc web#dev tail -n 50
procpane proc web#dev grep -i error -A 3 -B 3
procpane proc web#dev since <cursor>  # incremental polling
procpane grep -i "error|warn"         # cross-task, prefixed with task name
procpane wait-for api                 # block until healthy
```

Lines have monotonic seq numbers, so `since <cursor>` lets an agent poll without re-reading. Output is ANSI-stripped on ingress — `grep` against clean text. PTY-backed so Vite/Next/esbuild produce their full color output without downgrading for a non-TTY consumer.

### Example: agent watch loop

```bash
CURSOR=0
while true; do
  OUT=$(procpane proc api#dev since $CURSOR --json)
  CURSOR=$(echo "$OUT" | jq .next_cursor)
  echo "$OUT" | jq -r '.lines[].text'
  sleep 2
done
```

---

## Config: turbo.json + procpane.toml

procpane never modifies `turbo.json`. Orchestration concerns (graph, persistence, build dependencies) stay there. procpane's batteries — healthchecks, hostnames, profiles, env declarations — live in a sidecar:

```
turbo.json              # task graph (turbo owns this)
procpane.toml           # healthchecks, hostnames, env_from, profiles
procpane.local.toml     # personal overrides, gitignored
```

procpane reads the subset of `turbo.json` that matters for orchestration:

- `tasks.<n>.dependsOn` ✓
- `tasks.<n>.persistent` ✓
- `tasks.<n>.with` ✓
- `tasks.<n>.env`, `passThroughEnv` ✓ (parsed; PATH is auto-augmented with each ancestor `node_modules/.bin`)
- `tasks.<n>.interactive` — refused with an error
- `tasks.<n>.cache`, `outputs`, `inputs`, `outputLogs` — parsed and ignored (no caching)

If `turbo run <task>` works, `procpane up <task>` resolves the same graph.

---

## How it works

- **One daemon per repo**, lifetime bound by a PID lockfile at `.procpane/lock`.
- **Unix socket** at `.procpane/sock` carries length-prefixed JSON.
- **Per-task ring buffer**, 2048 lines, ANSI-stripped on ingress. Lines have a monotonic seq number; `since <cursor>` returns lines with `seq >= cursor`.
- **One PTY per task** so Vite/Next/esbuild don't downgrade their output for a non-TTY.
- **Process groups** (`setpgid`) so `kill(-pgid, SIGINT)` reaps `next dev`'s workers cleanly.
- **DAG via petgraph**: `dependsOn`, `^`-topological deps, and `pkg#task` explicit deps are honored. `with:` siblings launch alongside without a graph edge.
- **In-memory only**: no logs on disk. Stopping the daemon discards the buffers. Persistence is the agent's job.
- **Build phase delegated to turbo.** Non-persistent dependencies run as a single `turbo run …` step before any persistent task spawns. You get turbo's cache for free — warm-cache dev boot is sub-second. The combined output lands in a `procpane#prebuild` buffer; if turbo exits non-zero, the run aborts. Pass `--no-prebuild` to skip.
- **Local CA + reverse proxy**: `rcgen`-generated CA, installed once into System keychain, signs ephemeral wildcard certs for `*.test`. Reverse proxy runs unprivileged on `:8443`, optional pf redirect from `:443`.
- **Keychain integration**: macOS Security framework via `security-framework` crate, per-repo service name, daemon caches decrypted values in memory, never on disk.
- **P2P transfer**: `magic-wormhole.rs` for short-code authenticated transfers, sender confirms key list before Touch ID approval.

---

## Non-goals

- Caching, hashing, remote cache — use `turbo run build`
- File-change-triggered task restart — use the task's own watcher (Vite HMR, tsx watch, tsc --watch)
- Interactive (`interactive: true`) tasks
- Windows
- Linux as a first-class target (the runtime works; the batteries are macOS-shaped)
- On-disk log persistence
- Production secrets management — use Doppler / Vault / AWS / 1Password for prod
