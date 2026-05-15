# procpane — Ideas & Direction

Brainstorm log. Decisions, not commitments.

## Problem domain (where procpane sits)

| Tool | Strength | Weakness for our use case |
|---|---|---|
| Procfile / foreman / overmind | Universal, simple, overmind's tmux panes are great for humans | No dep graph, no build phase, no structured log access for agents |
| docker-compose | Real dep graph, healthchecks, hermetic, prod-adjacent | Heavy (VM on macOS), slow iteration, no per-service log addressing for agents |
| Turborepo | Task graph, remote cache, de facto JS monorepo standard | TUI opaque to agents, `--ui=stream` has no per-task addressing, persistent tasks leave no log artifacts |
| mprocs | Nice TUI multiplexer | Same agent-opaqueness as turbo |
| process-compose | YAML, health checks, deps, REST API | Closest spiritual sibling — worth studying |
| tilt / skaffold / garden | Powerful inner-loop | K8s-flavored, complex |
| devenv / nix + process-compose | Hermetic | Slow first-boot, niche |

**The gap procpane fills:** nothing treats the AI agent as a first-class consumer of the dev session. The load-bearing primitive is `proc <name> since <cursor>` over a Unix socket.

## Scope correction

procpane is **not** a node-only runner. The PTY layer (`src/process.rs`) execs arbitrary commands — vite, next, cargo watch, python, go, anything. The constraint is at the **config layer**: it requires `turbo.json` + a JS workspace because task identity is `<package>#<script>`. Decoupling the config schema is what unlocks the polyglot story, not changing the runtime.

---

## Three ambitious directions

### 1. The agent-native dev environment standard

Become the **protocol** every dev runner exposes for agents. Define a minimal JSON-over-Unix-socket spec (`list`, `tail`, `since`, `grep`, `stop`, `restart`, `health`) and ship adapters that wrap turbo, docker-compose, process-compose, overmind, bare Procfiles. Pitch as the "MCP server for any local dev runner" to Claude Code / Cursor / Continue / Aider.

Win condition: an agent in any IDE can query any dev session without knowing what runner started it. procpane becomes infrastructure.

Risk: standards-by-fiat fail. Mitigation: ship the adapters yourself before asking anyone else to.

### 2. Time-travel debugging for dev sessions

Flip the in-memory ring buffer into a **session recorder**: every PTY byte timestamped, alongside filesystem mtimes and HTTP traffic. Then:

- `procpane replay --at 14:32:17` — reconstruct all panes at that moment
- `procpane bisect "when did /api/users start 500ing?"` — walk back to first failure, correlate with git index + fsnotify events
- `procpane explain <error-id>` — return the error + 200 preceding lines across *all* tasks + file writes in window → full causal context for agents

The "rr for dev sessions" pitch. Most differentiated of the three.

Risk: disk/memory cost. Mitigation: bounded retention, per-task budgets, compression.

### 3. The local production-mirror orchestrator

Run anything that participates in the dev loop: node processes, containers, k8s pods (kind), wrangler workers, supabase, tunnels, test watchers, LLM gateway proxies. Same `tail/since/grep/health` surface for all. `procpane.toml` composes them with real readiness gates.

Ship a **drift detector** comparing local stack to prod manifest (Compose, K8s, Wrangler) — "does this repro in prod?" becomes a tool call.

Inner-loop counterpart to Compose/K8s manifests: same topology, ergonomics tuned for laptop + agent.

Risk: scope creep into Tilt/Garden territory. Mitigation: laptop-local only, never touch deployment.

**Read:** #2 is most differentiated, #1 is most strategic, #3 is most marketable but most crowded.

---

## Features worth stealing from docker-compose

### The big one: healthcheck + depends_on conditions

Current `depends_on` gates on "process started" — a lie. Vite is running but not serving for ~500ms. Postgres listening but rejecting connections.

Add condition types:
- `service_healthy` — TCP port open / HTTP 200 on `/health` / log line matched / file exists
- `service_log_ready` — wait for regex against task stdout (dev-native, compose doesn't have this)
- `service_completed_successfully` — migrations before api

`procpane status` returns `waiting → starting → healthy → ready` instead of just "running". Unlocks `procpane wait-for api` for agents.

### Also worth stealing

- **Profiles.** `procpane run --profile minimal` vs `--profile full`. Cleaner than filter gymnastics.
- **Per-task `stop_signal` / `stop_grace_period`.** Hardcoded 5s SIGINT→SIGKILL is wrong for DBs (want SIGTERM + 30s) and wrong for watchers (want SIGINT + 1s).
- **Restart policies.** `restart: on-failure` with max retries.
- **Override files.** `procpane.local.toml` auto-merged for personal overrides, gitignored.
- **Rich env interpolation.** `${PORT:-3000}`, `${REQUIRED:?msg}`.

### Steal the idea, not the mechanism

**Service URL registry.** Can't do DNS, but can maintain `${tasks.api.url}` so `web#dev` gets `API_URL=http://localhost:3001` injected when api is healthy. Solves "what port did vite pick this time?"

### Skip

Logging drivers (in-memory ring is the product), resource limits, secrets-as-files, `develop.watch` (every dev tool has its own watcher).

### Implementation order

1. healthcheck + depends_on conditions (biggest agent UX win)
2. stop_signal / stop_grace_period per task
3. profiles
4. override files
5. env interpolation + URL registry

---

## Config: sidecar, not superset

**Decision:** keep `turbo.json` pristine. Add `procpane.toml` sidecar.

Turbo 2.x is increasingly strict about unknown keys. Stuffing tunnel/vault/healthcheck config into `turbo.json` risks breaking on every turbo upgrade and conflates layers — turbo answers "what runs in what order"; procpane answers "what does the dev environment look like."

```
turbo.json              ← task graph, owned by turbo (read-only for procpane)
procpane.toml           ← healthchecks, profiles, tunnels, env vault
procpane.local.toml     ← personal, gitignored
procpane.env.age        ← encrypted shared secrets
```

Per-task overlays attach by task id, not by mutating turbo.json:

```toml
[tasks."web#dev"]
hostname = "web.test"
healthcheck.log = "ready in"
profiles = ["minimal", "full"]

[tasks."api#dev"]
hostname = "api.test"
healthcheck.http = "/health"
env_from = ["STRIPE_TEST_KEY", "DATABASE_URL"]
```

---

## Batteries: portless-style HTTPS tunnels

Inspired by vercel-labs/portless. Local HTTPS for dev without per-task TLS config.

### Mechanics

1. Generate local root CA, install into system trust store (sudo required — writing to System keychain).
2. Issue wildcard cert for `*.test` (configurable).
3. Reverse proxy maps `web.test → ephemeral port` as each task becomes healthy.
4. Browser hits `https://web.test:8443` (default) → proxy routes.
5. Same registry powers env URL injection — `api#dev` gets `WEB_URL=https://web.test:8443` automatically.

Stretch: `procpane expose web --public` pins cloudflared/ngrok to the same hostname for webhook testing.

### Sudo: where it happens, and where it doesn't

macOS reality: binding `:443` needs root. Trusting a local CA needs admin (System keychain write). Two distinct privileged operations. Design goal: **never more than two sudo prompts ever, and only if the user opts into pretty URLs.**

| Action | Privilege | When |
|---|---|---|
| Run procpane daemon | unprivileged | every session |
| Run reverse proxy on `:8443` | unprivileged | every session |
| Install local CA into System keychain | admin (one prompt) | one-time, `procpane trust install` |
| Bind `:443` via pf redirect | admin (one prompt) | one-time, opt-in via `procpane trust install --pretty-urls` |

Default URLs: `https://web.test:8443`. Browsers handle non-default HTTPS ports fine; cookies, CORS, SameSite all work. The `:8443` is mildly ugly but **zero sudo to opt into HTTPS**, which is the right default.

For users who want `https://web.test` (no port): `--pretty-urls` adds a pf anchor that redirects `:443 → :8443`. Single extra sudo prompt at install. Daemon still runs unprivileged. `procpane trust uninstall` reverses both cleanly.

Touch-ID-for-sudo (`pam_tid.so` in `/etc/pam.d/sudo`) is common on Macs, so for many users these are Touch ID prompts, not password prompts. Recommend it in the install output.

### The DX in three lines

```bash
procpane trust install         # one Touch ID, installs CA, done forever
procpane run                   # https://web.test:8443 just works
procpane trust install --pretty-urls   # optional, drops the :8443
```

---

## Batteries: env management (Keychain + P2P, no git)

**Hard constraint:** no encrypted secrets in the repo. Sharing happens out-of-band via authenticated p2p transfer, point-to-point, ad-hoc.

### Storage

- macOS Keychain (per-machine source of truth), one service-name namespace per repo.
- Touch-ID-gated read access; daemon unlocks once per lifetime.
- Linux: encrypted file in `~/.config/procpane/<repo>/secrets` (age-encrypted with a key derived from ssh-agent). libsecret integration later if asked.

No file in git. No `secrets.age` blob. Nothing to leak.

### Sharing: p2p, not commits

Two embeddable Rust options:

| | iroh | magic-wormhole.rs |
|---|---|---|
| Transport | QUIC, NAT-traversal via n0 relays | TCP/WebSocket via wormhole relays |
| Auth | ed25519 node ID in ticket | PAKE (SPAKE2) + short word code |
| Code form | long ticket string | `7-crossover-clockwork` |
| Read-aloud DX | bad (copy/paste only) | great |
| Rust maturity | active, well-funded | stable, smaller |

**Lean magic-wormhole.** The short word code is the killer DX — works over Zoom, Slack, even a phone call. iroh wins on plumbing, but the human moment of "Alice, what's your code?" is what makes this feel friendly.

### Flow: pull, not push

Receiver initiates. Inverts the social dynamic — the new dev asks, sender approves with Touch ID.

```bash
# New dev clones repo, runs:
$ procpane run
✗ No env configured for this repo.
  Ask a teammate to run:  procpane env send 7-crossover-clockwork

$ procpane env receive 7-crossover-clockwork
⏳ Waiting for sender...

# Teammate (any of them, no GitHub username needed):
$ procpane env send 7-crossover-clockwork
🔓 Touch ID to authorize sending 12 vars to new device...
✓ Sent.

# Back on the new dev's machine:
✓ Received 12 vars, stored in Keychain.
$ procpane run        # just works now
```

The word code is generated by the receiver, shared via DM/Slack/voice, claimed by the sender. The sender's Touch ID is the authorization moment — they see what's about to leave their machine and consent to it.

### Per-task env allowlist (security story)

By default tasks get *none* of the stored secrets — opt in via `env_from` in procpane.toml:

```toml
[tasks."api#dev"]
env_from = ["STRIPE_TEST_KEY", "DATABASE_URL"]
```

Prevents a malicious `postinstall` in `node_modules` from inheriting Stripe keys just because vite is running. This matters more now that there's no encrypted-file boundary — the Keychain is the boundary.

### Updating shared values

Sender pushes a delta to a known peer they've shared with before:

```bash
$ procpane env push @alice STRIPE_TEST_KEY DATABASE_URL
⏳ Waiting for @alice's daemon to come online...
🔓 Touch ID to authorize...
✓ Pushed 2 vars to @alice.
```

Requires both sides to have a stored peering record (built up by the first `send`/`receive`). Daemon-to-daemon over the same magic-wormhole transport, but with cached peer identities so no code re-entry. If alice is offline, queue and retry next time her daemon shows up on the relay. Lightweight gossip, not a control plane.

### Linux

Encrypted local file in `~/.config/procpane/`. ssh-agent-derived key for at-rest encryption. Same p2p sharing layer — magic-wormhole is platform-agnostic.

### What this is not

- Not a production secrets manager. Doppler/Vault/AWS for prod live keys. procpane = dev secrets only.
- Not a backup system. If you wipe your Mac, ask a teammate for a fresh `send`.
- Not a CI story. CI uses platform secret stores.

### The pitch

> Dev secrets in your Keychain, never in git. Share them like Magic Wormhole files — a word code and a Touch ID — to anyone, no GitHub accounts, no key exchange.

### Open questions

1. **Sender consent UX.** Touch ID is good. Should procpane *show* the keys (not values) being sent in a confirmation dialog? Yes, probably — the moment-of-truth display matters.
2. **Audit trail.** Each `send`/`receive` logs locally to `.procpane/transfer.log` (machine-local, gitignored). Enough?
3. **Onboarding from zero.** First sender has no one to receive from — `procpane env set FOO` interactively populates Keychain directly, no transfer.
4. **Identity.** Right now "@alice" is a local nickname for a stored peer record (her wormhole node identity). Should it federate with GitHub usernames as a label-only convenience? Probably yes, but identity itself stays p2p.
5. **Relay trust.** magic-wormhole's default relay is operated by Brian Warner. Acceptable for dev secrets? Probably. Document that values are E2E-encrypted regardless.
6. **Rotation when teammate leaves.** Sender uses `procpane env rotate KEY` locally; ex-teammate's stale value stays in their Keychain until they delete it. Same as any out-of-band secret — there's no central revocation, that's the point.
