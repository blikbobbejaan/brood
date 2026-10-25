# Brood as a general-purpose language — what is missing

**Status: research note, 2026-10-25, read against `origin/main` at `288dfe03` (v0.43.0 + Unreleased).**
Nothing here was run; every claim cites the file it was read from. Claims marked
*unverified* could not be checked from the tree.

## The question

ROADMAP.md opens by calling Brood "a general-purpose language and runtime". If someone who
writes Go, Rust, Zig, Node/Deno, Java/.NET, Python or an actor-runtime language picked it up for a service, a
CLI or a desktop program today, what would they reach for and not find? This note lists
those gaps in the order they would stop someone from adopting it, not in order of how
interesting they are to build.

Each gap says what is missing, where that was checked, what "done" looks like, and which
side of the line it falls on. **Kernel/language** means a new primitive or a change to a
contract, and needs an ADR. **std** has to pass ADR-417's three questions: it is a
mechanism, something besides the editor needs it, and it has to version with the kernel.
**hive** means a package: a format or protocol library that other programs want but that
does not version with the kernel.

## What Brood already has

Some of this is ahead of most of the reference languages. A gap list should not hide that.

- **A mature actor runtime's process model, with a JIT.** Preemptive M:N scheduling with work stealing and
  live migration (docs/scheduler.md, ADR-100), and preemption inside native code since
  ADR-400..403. Links, monitors, selective receive, mailbox bounds (ADR-308), supervisors,
  `gen`, `defstatem` (ADR-437), `defapp` with signal-driven shutdown (ADR-435/436) and a
  local registry (ADR-445) and cluster-wide process groups (ADR-446). Encrypted multi-node
  distribution (docs/distribution.md). In the cross-language benchmark it has the
  second-best p99 latency in the field, behind the reference actor runtime and ahead of Go
  (brood-benchmark/README.md).
- **Live redefinition is the core design.** Redefining a function reaches every process on
  its next call. Startup images (ADR-218/314) and hot reload (docs/shared-code.md).
- **A real numeric tower.** Bigints widen on overflow, ratios are exact (ADR-196), decimals
  have an `M` literal, and floats are opt-in through `->float` (docs/language.md).
- **Unicode done properly.** Graphemes, display width and NFC/NFD/NFKC/NFKD
  (`crates/lisp/src/builtins/string.rs`). Strings are deliberately not seqable (ADR-159).
- **One toolchain.** `nest` covers new, run, test (property tests, coverage, doctests),
  check, format, doc, publish, rename (a codemod), ws, observe and mcp
  (`std/tool/nest.blsp`). There is an LSP with most of the protocol (docs/lsp.md), an
  Emacs mode, a VS Code extension and a tree-sitter grammar (sibling repos).
- **A package manager with a real solver.** PubGrub resolution (ADR-209), sha256 locks,
  ed25519 signing with trust on first use (ADR-212), and a hosted registry (ADR-211).
- **Single-binary releases with no Rust toolchain needed** (`nest release`, docs/release.md),
  and a wasm32 build that runs in a browser tab (CI job "wasm32").
- **A sound, advisory, set-theoretic type checker** with occurrence typing, record and tuple
  unions, and exhaustiveness over literal enums (docs/type-system-status.md).

## Gaps, ranked by how much they block adoption

### 1. There is no stability promise

**Missing.** README.md says the language, the standard library, the tooling and the
on-disk formats "can change dramatically (and break) without notice or migration path …
don't build anything you depend on against it yet." The 1.0 freeze gate is closed
(ADR-170), but there has been no 1.0. CHANGELOG.md runs from v0.1.0 (2026-08-02) to v0.43.0
in about twelve weeks and says "breaking" 24 times. ADR-417 removed ten std modules in one
release. A team will not adopt a language on those terms, however good it is.

**Already there to build on.** `:since` / `:deprecated` / `:beta` metadata (ADR-283), the
rename ledger with `nest rename` (`std/tool/codemod.blsp`, `std/tool/renames.blsp`), the
`:brood "<constraint>"` runtime gate (ADR-209), and docs/ecosystem-playbook.md, which already
treats a breaking change as a minor bump.

**Done looks like.** A written compatibility policy that says what is covered: the
language, std's public names, the `nest` CLI, the lock format, the image format, and
whether the dist wire format is compatible across versions. It also says how long a
deprecation lasts before removal, and that every removal ships a codemod. Then a 1.0 that
means it.

**Kind.** Policy, recorded as an ADR. No code. It is first because nothing below matters
until it exists.

### 2. Platforms: tested on Linux only

**Missing.** `.github/workflows/ci.yml` runs the tests on Linux only. macOS is
`cargo check` only ("the target only a tag used to reach"). On Windows the tree compiles,
but the job that runs a program has `continue-on-error: true`: "Whether the runtime RUNS on
Windows, which nothing has yet shown". `release.yml` builds four targets: x86_64 and
aarch64 Linux (gnu), and aarch64 and x86_64 macOS. It builds no Windows and no musl binary.
`nest release --target` only appends to a runtime the user has built on that target and
copied into `~/.cache/brood/runtimes/` by hand. Cross-compiling the runtime is out of scope
(docs/release.md, ADR-038). Go and Zig cross-compile from any host, and Deno ships its
runtime for every target.

**Done looks like.** The test suite runs on macOS and Windows in CI. The Windows smoke job
becomes a gate. release.yml also publishes Windows and musl runtimes, and
`nest release --target` downloads the matching runtime for the installed version, checking
its hash, instead of asking the user to scp one into place.

**Kind.** Kernel and build infrastructure. Downloading the runtime changes ADR-038, so it
needs an ADR.

### 3. Native interop: no C FFI, and the WASM route is half built

**Half the usual complaint.** There is no C FFI, and that is deliberate.
docs/interop.md rejects a native `.so` ABI because it would be tied to the kernel's `Value`
layout, could crash the runtime and would reopen ADR-037's supply-chain hole. The answer
it chose is WebAssembly components, and slices 1 and 2 have shipped (ADR-145): wasmtime is
embedded, WIT-typed calls go through `std/wasm.blsp`, `bytes` are marshalled, and calls are
fuel-metered. `libloading` is used only to load tree-sitter grammars
(`crates/lisp/Cargo.toml`).

**Missing.** A guest has no WASI. `host/wasm.rs` says "no WASI wired in", so a component
cannot touch files, the network or the clock. Guests cannot hold `resource` handles. There
is no `:native` manifest clause, lock entry or build-on-fetch, which docs/interop.md calls
"shovel-ready" but deferred until a consumer exists (`std/tool/package.blsp` has no
`:native`). In practice, any native capability, such as a new hash, a codec or SQLite,
still means a Rust builtin and a new runtime. Embedding works the other way round only
from Rust: `Interp::{new, eval_str, eval_source, print}` (`crates/lisp/src/lib.rs`). There
is no C ABI, no `cdylib`, and no API for a host program to register its own functions.

**Done looks like.** WASI preview 2 with grants declared in the manifest. Guest resources
as opaque handles. `:native` with `:wasm-build` and `:wasm-artifact`, hash-pinned in the
lock. The test is SQLite running as a component from a hive package (gap 5). Separately, a
small C embedding API (create a runtime, evaluate, call a function, register a host
function), so Brood can be a scripting language inside other programs, as Lua and
Guile are.

**Kind.** Kernel. ADR-071/145 cover the WASM work. A C embedding API needs a new ADR.

### 4. Networking stops at HTTP/1.0 over TCP

**Worse than it looks.** `std/net/http.blsp` speaks HTTP/1.0, client and server,
with no keep-alive and no chunked encoding ("deliberately small"). SSE is HTTP/1.0 for the
same reason (`std/net/sse.blsp`). The kernel has **no UDP** (`std/net/dns.blsp`;
`builtins/modules.rs:395`), so DNS runs over TCP and returns A and AAAA records only.
There is no ALPN in the TLS primitives (no match in `crates/lisp/src`), so HTTP/2 cannot
even be negotiated. HTTP/1.1 and WebSocket do exist, but downstream in hatch
(`hatch/src/http/response.blsp`, `hatch/src/http/websocket.blsp`). Nothing provides
HTTP/2, HTTP/3, gRPC or QUIC.

**Done looks like.** UDP sockets in the kernel, delivered to the mailbox the same way TCP
is (ADR-062). ALPN and client certificates in `%tls-*`. In std, an HTTP/1.1 client with
keep-alive and chunked decoding, because `nest` downloads tarballs and every program
calls APIs. hatch's server stays the server. HTTP/2 and gRPC become hive packages once
ALPN exists.

**Kind.** UDP and ALPN are kernel mechanisms and need an ADR. An HTTP/1.1 client passes
ADR-417 because the toolchain is its second user. HTTP/2, gRPC and WebSocket clients are
hive packages.

### 5. Data access: Postgres only

**Verified.** std has no database drivers, and that is correct under ADR-417.
`store-postgres` is a Postgres wire driver and pool written in pure Brood, with SCRAM and
the extended protocol, behind `store`'s adaptor contract (`store-postgres/README.md`).
There is no SQLite. It cannot be written in pure Brood and is blocked on gap 3. MySQL,
Redis and the message brokers (Kafka, NATS, AMQP) were not found in the sibling
repositories. Whether they exist on the live registry is *unverified*.

**Done looks like.** SQLite as a WASM component package (after gap 3), and Redis and MySQL
as pure-Brood packages over `tcp`'s binary mode, following store-postgres's pattern.

**Kind.** hive packages. The only kernel dependency is gap 3.

### 6. The library ecosystem is small, and there are std holes in time and crypto

**Update 2026-10-25: the two std holes are closed.** Time zones are `std/tz` (zoneinfo and
POSIX rules); ECDSA, RSA, X25519 and Argon2id are in `std/crypto` (ADR-448). The ecosystem
part below stands.

**Missing.** The first-party packages in the sibling repositories are hatch, store,
store-postgres, s3, mail plus two adapters, observe plus two adapters, b2d and four bedit
themes. That is about a dozen. The live registry count is *unverified*. Two holes sit at
std's level rather than in the ecosystem:

- **No time zones.** `std/datetime.blsp`: "There is no zone *type* and no zone database …
  Rendering is always UTC." Go, Java, .NET, Python (`zoneinfo`) and Temporal all ship IANA
  zones.
- **Thin public-key crypto.** `std/crypto.blsp` has ChaCha20-Poly1305, PBKDF2, HMAC,
  SHA-1/2, BLAKE3 and ed25519 (`keypair` / `sign` / `verify`). It has no RSA, ECDSA/P-256,
  X25519 or argon2/scrypt, so JWT RS256/ES256, OIDC, and SSH or WebAuthn verification all
  need native code.

YAML, TOML, protobuf and msgpack are absent from std, which is correct by the rule. Whether
any of them is on hive is *unverified*.

**Done looks like.** IANA zones: the zone engine in std, with tzdata as a separately
updatable hive data package. RSA, ECDSA and X25519 primitives next to ed25519. Then
protocol and format packages as programs ask for them.

**Kind.** Crypto primitives are kernel, since they need Rust or a WASM component. The time
zone engine is std, a mechanism that logging, `nest` and every service need. Formats are
hive.

### 7. Debugging and observability do not connect to the outside world

**Missing.** There is no Debug Adapter Protocol. `std/tool/debug.blsp` speaks its own wire
protocol, a process-native debugger that is better than a breakpoint in several ways, but
no editor other than bedit can drive it. `std/jsonrpc` mentions DAP only as a possible
framing. Profiling has a sampling CPU profiler (`profile-start` / `profile-stop`) and
`perf/report`, whose counters need a `--features perf-stats` build
(`std/tool/perf.blsp`). It has no flamegraph, pprof or speedscope export, and no
`nest profile` command. Telemetry is good inside the process (`std/telemetry.blsp`,
aggregators, `system-monitor`), and the `observe` package ships logs, metrics and errors to
AppSignal. There is no OTLP / OpenTelemetry exporter and no trace context propagated over
HTTP (W3C `traceparent`). ROADMAP's observability item is still 🟡 (`defevent` schemas, the
remote tier).

**Done looks like.** A DAP server that translates DAP into `debug`'s existing wire
protocol, so VS Code, Neovim and Zed can set breakpoints, read paused processes and
evaluate in their scope. `nest profile` writing folded stacks or pprof. An `observe-otlp`
provider.

**Kind.** The DAP adapter is std (`std/tool`): it versions with `debug`'s wire, and an
editor other than bedit needs it. Profile export is std tooling. OTLP is a hive package.

### 8. No enforced convention for errors

**Missing.** ROADMAP.md line 125 says the `[:ok v]` / `[:error reason]` convention, a
structured error map shared by `throw` and the tuple, `result` / `unwrap` / `unwrap-or`, and
tuple-union exhaustiveness in the checker are "deliberately not started". Without the
checker part, "the convention is a naming rule rather than an enforced one". Go has
`error`, and Rust has `Result` and `?`. A newcomer to Brood finds
`try`/`catch`/`finally` (ADR-306) and tuples by habit.

**Done looks like.** One error shape, a `with`-style chaining macro, and exhaustiveness over
`(or (tuple :ok T) (tuple :error E))` in `check/exhaustive.rs`, so a forgotten `[:error …]`
branch warns.

**Kind.** Language: a prelude macro plus checker work, recorded as an ADR.

### 9. Cost: memory per runtime and per process, and compute speed

**Measured in brood-benchmark/README.md, 2026-09-16.** Base RSS is 52.8 MB, against Go 1.8,
Python 9.8 and Node 42.8. Holding 300k live processes takes 1.31 GB, against the
reference actor runtime's 924 MB. A tree of 20k supervised children takes 491 MB, against its
153 MB. Compute is 7.9× C on the geomean of the shared rows, between Node (5.3) and the actor
runtime (10.4). Unboxed
numeric arrays are still listed as "immutability-fraught" (docs/runtime-frontier.md §D),
which rules Brood out for numeric work.

**Done looks like.** The open items in docs/runtime-frontier.md (shared IC tables M2, one
memory block per process M7), a lean runtime without the GUI (docs/release.md notes that
every release carries about 4 MB of GUI), and an answer on numeric arrays, perhaps
immutable unboxed vectors behind a `bytes`-like handle.

**Kind.** Kernel. Unboxed arrays need an ADR.

### 10. Very large codebases, and an advisory type system

At 3.07M lines a cold `nest check` takes 161 s and 3.8 GB on one thread, and a cold
`nest run` takes 39 s (docs/large-project-scaling.md). Warm runs are fine, and the doc
already holds the queue (kernel tooling). Separately, the checker is advisory by design: it
never gates the live image, globals stay `dynamic()` because of hot reload, `sig` covers
787 of 3357 std `defn`s, and type variables are at slice 2 (docs/type-system-status.md,
docs/type-variables.md). That is a choice, not a defect, but a Go, Rust or TypeScript team
will read it as one. The fix is documentation: say that a project wanting a gate runs
`nest check --strict` in CI.

## Corrections to the first, informal list

This note started as a quick list from skimming `std/`, README and ROADMAP. Checked against the
code, some of it was wrong:

- **"No single static binary story"** is wrong. `nest release` ships one binary without a
  Rust toolchain (ADR-038). What is missing is cross-*compiling* the runtime, a Windows or
  musl runtime in releases, and an asset filesystem (gap 2).
- **"No C FFI"** is right, but it leaves out that a WASM component interop path exists and
  partly works (ADR-145). The real gap is WASI, resources and `:native` packaging (gap 3).
- **"Roadmap defers keyword args, hygienic macros, true namespace isolation"** is stale on
  two of the three. The "Out of scope" list in ROADMAP.md is out of date. Namespaces
  shipped: `defmodule` namespaces, package-rooted modules (ADR-065/070) and enforced
  privacy (ADR-146). `&key` is refused, not deferred (ADR-163, in ADR-170's freeze list).
  Rationals, also on that list, shipped as ADR-196. Only full macro hygiene is still held
  back (ROADMAP "Held candidates", ADR-066), and auto-qualification in quasiquote covers
  most of it.
- **"WebSocket only downstream"** is right, but the larger fact is that std's HTTP is 1.0
  and the kernel has no UDP (gap 4).
- **Supply chain** is better than the list suggested. Lock hashes, ed25519 signing
  with trust on first use, and a structured conflict proof have all shipped. Still open: an
  enforced signing mode (docs/packages.md "Future work"), yanking, and a vulnerability
  advisory database. No `yank` or `advisory` was found in hive/src or std/tool/package.blsp.
  `std/tool/audit.blsp` audits documentation coverage, not security.
- **OpenTelemetry** is partly covered: telemetry and the `observe` package exist. What is
  missing is the OTLP exporter (gap 7).

**Smaller, unranked.** No locale-aware collation. No `nest` benchmarking command (like
`go test -bench`). The grammar is not in github/linguist. Marketplace or MELPA publication
of the editor extensions is *unverified*.

## Recommended order

1. **The stability policy (gap 1).** It costs only writing and unblocks everything else.
   Do it before 1.0.
2. **Platforms (gap 2).** Make Windows and macOS gates, and ship their runtimes. Without
   them, half the developers who would try Brood cannot.
3. **UDP, ALPN and an HTTP/1.1 client (gap 4).** Small kernel additions with a large reach.
4. **Finish WASM interop (gap 3).** WASI, resources and `:native`, proven by an SQLite
   package (gap 5). After this, the ecosystem can grow without a kernel release.
5. **DAP and profile export (gap 7).** They connect Brood's existing debugger and profiler to
   the editors and tools people already use.
6. **The error convention (gap 8), then time zones and crypto (gap 6).**
7. **Ecosystem packages (gaps 5 and 6).** Redis, MySQL, OTLP, YAML/TOML, HTTP/2 and gRPC, in
   whatever order programs ask for them.
8. **Footprint and scale (gaps 9 and 10).** Continue the frontier docs' queues, which are
   measured and owned already.
