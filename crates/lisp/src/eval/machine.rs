//! The tree-walker's frame stack (ADR-416).
//!
//! The evaluator used to keep its pending work on the Rust stack: evaluating an argument, an
//! `if` test, a `let` binding or a non-last body form was a nested `eval` call. Here that work
//! is a [`Kont`] — a continuation naming what to do with the sub-form's value — on a stack the
//! [`Machine`] owns, and the sub-form runs on the same loop. A tail position pushes nothing, so
//! proper tail calls hold by construction.
//!
//! **One continuation is one old nested `eval`.** It sets the pushing frame's registers aside
//! (its trace tracker [`TwFrame`] and lazy-expansion count) and the sub-form starts a frame of
//! its own, exactly as the nested call did. A value pops it and resumes the pushing frame. An
//! error pops it the way the nested call's error left it: the inner frame's trace entry is
//! closed ([`close_frame`]), the sub-form's position is attached when the old site used
//! `eval_at` (`Kont::form`), and the continuation's operand roots are dropped, as `root_scope`
//! dropped them. The error then belongs to the pushing frame and keeps unwinding.
//!
//! **GC.** A continuation holds no raw handle: what it keeps (the unchosen branch, the forms
//! left, the arguments so far, the environment) sits on the heap's operand stacks as
//! [`Root`]/[`EnvRoot`] slots, which both collectors relocate in place — the discipline every
//! site already followed, now owned by the frame. `Marks` are the stack depths from before a
//! continuation rooted anything; finishing it truncates back to them.
//!
//! **Leaves run in place.** An atom, or a symbol bound here, evaluates without running
//! anything — it cannot collect or fail — so a sub-form that is one ([`leaf_value`]) is read
//! directly and costs no frame. `(+ a b)` pushes nothing.
//!
//! Still native recursion (ADR-416 stage 2b): macro expansion, `&optional` defaults, the
//! passthrough redirect's slow head lookup, a seq-view realised for `apply`, and every native
//! that calls back into Brood (`apply_closure` runs its own machine).

use super::*;
use crate::core::heap::EnvRoot;

/// The deepest the frame stack may grow: runaway non-tail recursion fails here, catchably,
/// as it does at the VM's frame limit. Each level of a non-tail recursion is one
/// continuation (plus its few operand slots).
const MAX_TW_FRAMES: usize = 1 << 18;

/// "recursion too deep …" — the tree-walker's frame-count guard.
fn tw_frame_depth_error(frames: usize) -> LispError {
    LispError::runtime(format!(
        "recursion too deep: exceeded the tree-walker's {frames}-frame non-tail-call \
         limit (runaway non-tail recursion?)",
    ))
    .with_code(crate::error::error_codes::STACK_DEPTH_EXCEEDED)
    .with_hint(
        "rewrite as a tail-recursive loop (proper tail calls are O(1) stack), \
         or drive the iteration with a process",
    )
}

/// The operand-stack depths from before a continuation rooted anything.
#[derive(Clone, Copy)]
struct Marks {
    roots: usize,
    envs: usize,
}

impl Marks {
    fn take(heap: &Heap) -> Marks {
        Marks {
            roots: heap.roots_len(),
            envs: heap.env_roots_len(),
        }
    }

    fn restore(self, heap: &mut Heap) {
        heap.truncate_roots(self.roots);
        heap.truncate_env_roots(self.envs);
    }
}

/// A pending piece of work: what the frame that pushed it does with the value of the
/// sub-form now running.
struct Kont {
    /// The pushing frame's trace tracker and lazy-expansion count, set aside while the
    /// sub-form runs as a frame of its own.
    entered: Option<TwFrame>,
    lazy: u32,
    /// The sub-form, when an error leaving it takes its position (the old `eval_at`).
    form: Option<Root>,
    marks: Marks,
    kind: KontKind,
}

enum KontKind {
    /// `if`: choose a branch on the test's value.
    If {
        then_r: Root,
        else_r: Root,
        env_r: EnvRoot,
    },
    /// A body held as a cons list (`do`, `let`, `letrec`): `cursor` is the cell whose form is
    /// running.
    Body { cursor: Root, env_r: EnvRoot },
    /// A closure body's non-last form is running; `rest` are the forms after it (the last of
    /// them is the body's tail).
    Forms {
        rest: SmallVec<[Root; 4]>,
        env_r: EnvRoot,
    },
    /// A `let`/`letrec` binding's value.
    Bind(BindState),
    /// `def`'s value, under a tier-0 ceiling.
    Def { name: Symbol, env_r: EnvRoot },
    /// An element of a vector, set or map literal.
    Literal(LiteralState),
    /// A computed head (`((f) …)`).
    Head { call_form_r: Root, env_r: EnvRoot },
    /// An argument of a call.
    Args(ArgsState),
}

struct BindState {
    /// Name/value-interleaved, every name already checked to be a symbol.
    binds: SmallVec<[Root; 8]>,
    /// The binding whose value is pending.
    i: usize,
    scope_r: EnvRoot,
    body_r: Root,
    /// Plain `let`: a later binding goes into a fresh child frame once an earlier value could
    /// have captured this one (see `bind_sequential`'s history in the devlog, and below).
    lexical: bool,
    captured: bool,
}

#[derive(Clone, Copy)]
enum LiteralKind {
    Vector,
    Set,
    /// `src`/`out` hold key, value, key, value, …
    Map,
}

struct LiteralState {
    kind: LiteralKind,
    src: SmallVec<[Root; 8]>,
    out: SmallVec<[Root; 8]>,
    env_r: EnvRoot,
}

struct ArgsState {
    callee_r: Root,
    call_form_r: Root,
    /// The cell whose form is the next (or the running) argument.
    spine_r: Root,
    env_r: EnvRoot,
    args: SmallVec<[Root; 4]>,
}

/// What one step left for the driver.
enum Step {
    /// The running frame produced its value.
    Value(Value),
    /// `expr`/`env` were replaced — a tail position, or a sub-form whose continuation was
    /// pushed. Step again.
    Continue,
}

/// A form that evaluates without running anything: an atom is itself, and a symbol bound
/// here reads its binding. Neither can collect or fail, so a caller evaluates it in place
/// instead of pushing a frame. `None` for everything else — a call, a collection literal, or
/// an unbound symbol (whose lookup may autoload a module).
#[inline]
fn leaf_value(heap: &Heap, form: Value, env: EnvId) -> Option<Value> {
    match form.unpack() {
        ValueRef::Sym(s) => heap
            .env_get(env, s)
            .map(|v| crate::builtins::contracts::boundary_resolve(heap, env, v)),
        ValueRef::Pair(_) | ValueRef::Vector(_) | ValueRef::Map(_) | ValueRef::Set(_) => None,
        _ => Some(form),
    }
}

/// The cdr of a cons cell, `nil` for anything else.
fn cdr(heap: &Heap, cell: Value) -> Value {
    match cell.unpack() {
        ValueRef::Pair(p) => heap.pair(p).1,
        _ => Value::nil(),
    }
}

/// `BROOD_EVAL_TRACE`: print each form a frame starts on.
#[cfg(debug_assertions)]
pub(super) fn eval_trace(heap: &Heap, form: Value) {
    static EVAL_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if crate::diagnostics::debug_flags::is_armed(&EVAL_TRACE, "BROOD_EVAL_TRACE") {
        eprintln!("[eval-trace] {}", crate::syntax::printer::print(heap, form));
    }
}

#[cfg(not(debug_assertions))]
pub(super) fn eval_trace(_: &Heap, _: Value) {}

/// The tree-walker's registers and its frame stack.
pub(super) struct Machine<'e> {
    expr: Value,
    env: EnvId,
    /// Consecutive LAZY macro expansions in the running frame (a macro defined in the same
    /// top-level form as its use, so the compile pass never saw it). Each re-enters the loop
    /// with the expansion as `expr`; a macro that expands to itself would otherwise spin here
    /// forever — the compile-time `macroexpand` bounds its fixpoint at `MAX_EXPAND_ROUNDS`,
    /// and this is the same bound for the runtime path. Reset whenever a closure body is
    /// entered, so a tail loop THROUGH a lazily expanded macro (one expansion per iteration)
    /// is never charged for its iterations.
    lazy: u32,
    /// The running frame's trace tracker. At the bottom of the stack this is the caller's
    /// (`eval` reads it after the run).
    entered: &'e mut Option<TwFrame>,
    konts: Vec<Kont>,
}

impl<'e> Machine<'e> {
    pub(super) fn new(expr: Value, env: EnvId, entered: &'e mut Option<TwFrame>) -> Self {
        Machine {
            expr,
            env,
            lazy: 0,
            entered,
            konts: Vec::new(),
        }
    }

    /// Run until the bottom frame has its value or its error.
    pub(super) fn run(&mut self, heap: &mut Heap) -> LispResult {
        loop {
            let mut outcome = match self.step(heap) {
                Ok(Step::Continue) => continue,
                Ok(Step::Value(v)) => Ok(v),
                Err(e) => Err(e),
            };
            // Hand the finished frame's outcome to the frames below it until one of them
            // has more to evaluate.
            loop {
                let Some(k) = self.konts.pop() else {
                    return outcome;
                };
                let inner = std::mem::replace(self.entered, k.entered);
                self.lazy = k.lazy;
                let next = match outcome {
                    Ok(v) => self.resume(heap, k.kind, k.marks, v),
                    Err(e) => {
                        let mut e = close_frame(e, inner);
                        if let Some(form_r) = k.form {
                            let form = heap.read_root(form_r);
                            e = e.or_form_pos(heap, form);
                        }
                        k.marks.restore(heap);
                        Err(e)
                    }
                };
                match next {
                    Ok(Step::Continue) => break,
                    Ok(Step::Value(v)) => outcome = Ok(v),
                    Err(e) => outcome = Err(e),
                }
            }
        }
    }

    /// Set the running frame aside behind `kind` and start a frame evaluating `form` in
    /// `env`. `marks` are the depths from before `kind` rooted anything; `tag` says whether an
    /// error leaving `form` takes its position.
    fn push(
        &mut self,
        heap: &mut Heap,
        marks: Marks,
        kind: KontKind,
        form: Value,
        tag: bool,
        env: EnvId,
    ) -> Result<Step, LispError> {
        let form_r = tag.then(|| heap.root(form));
        self.konts.push(Kont {
            entered: self.entered.take(),
            lazy: std::mem::take(&mut self.lazy),
            form: form_r,
            marks,
            kind,
        });
        eval_trace(heap, form);
        if self.konts.len() > MAX_TW_FRAMES {
            // Raised by the new frame, as the old nested `eval` raised it on entry.
            return Err(tw_frame_depth_error(MAX_TW_FRAMES).or_form_pos(heap, form));
        }
        self.expr = form;
        self.env = env;
        Ok(Step::Continue)
    }

    /// Continue the frame that pushed `kind`, now that its sub-form produced `v`. The
    /// frame's registers are already restored. A path that finishes with `kind` restores
    /// `marks` before anything that can fail; a path that needs another sub-form re-pushes
    /// with the same `marks`.
    fn resume(
        &mut self,
        heap: &mut Heap,
        kind: KontKind,
        marks: Marks,
        v: Value,
    ) -> Result<Step, LispError> {
        match kind {
            KontKind::If {
                then_r,
                else_r,
                env_r,
            } => {
                let branch = heap.read_root(if truthy(v) { then_r } else { else_r });
                self.env = heap.read_root_env(env_r);
                marks.restore(heap);
                self.expr = branch;
                Ok(Step::Continue)
            }
            KontKind::Body { cursor, env_r } => {
                let next = cdr(heap, heap.read_root(cursor));
                let env = heap.read_root_env(env_r);
                marks.restore(heap);
                self.body(heap, next, env)
            }
            KontKind::Forms { rest, env_r } => {
                let rest: SmallVec<[Value; 4]> = rest.iter().map(|&r| heap.read_root(r)).collect();
                let env = heap.read_root_env(env_r);
                marks.restore(heap);
                self.forms(heap, &rest, env)
            }
            KontKind::Bind(mut st) => {
                st.define(heap, v);
                st.i += 2;
                self.binds(heap, st, marks)
            }
            KontKind::Def { name, env_r } => {
                let env = heap.read_root_env(env_r);
                marks.restore(heap);
                def_bind(heap, name, v, env).map(Step::Value)
            }
            KontKind::Literal(mut st) => {
                let r = heap.root(v);
                st.out.push(r);
                self.literal_next(heap, st, marks)
            }
            KontKind::Head { call_form_r, env_r } => {
                let call_form = heap.read_root(call_form_r);
                let env = heap.read_root_env(env_r);
                marks.restore(heap);
                let spine = cdr(heap, call_form);
                self.call(heap, v, call_form, spine, env)
            }
            KontKind::Args(mut st) => {
                let r = heap.root(v);
                st.args.push(r);
                let next = cdr(heap, heap.read_root(st.spine_r));
                st.spine_r = heap.advance_root(st.spine_r, next);
                self.args_next(heap, st, marks)
            }
        }
    }

    /// One step of the running frame: a leaf answers, a special form or a call either
    /// answers, hands a tail position back to the loop, or pushes a sub-form.
    fn step(&mut self, heap: &mut Heap) -> Result<Step, LispError> {
        match self.expr.unpack() {
            ValueRef::Sym(s) => {
                let env = self.env;
                let expr = self.expr;
                return match heap.env_get(env, s) {
                    Some(v) => Ok(Step::Value(crate::builtins::contracts::boundary_resolve(
                        heap, env, v,
                    ))),
                    // A miss may autoload the name's module (ADR-335) — a collection — so
                    // the form is rooted across it for the position tag.
                    None => heap
                        .root_scope(|heap| {
                            let expr_r = heap.root(expr);
                            let looked_up = crate::eval::derive::global_miss(heap, env, s);
                            let expr = heap.read_root(expr_r);
                            looked_up.map_err(|e| e.or_form_pos(heap, expr))
                        })
                        .map(Step::Value),
                };
            }
            ValueRef::Vector(id) => {
                let items = heap.vector(id).to_vec();
                return self.literal(heap, LiteralKind::Vector, &items);
            }
            ValueRef::Map(id) => {
                // Key, value, key, value — evaluated in that order, then canonicalised
                // (last-wins on equal keys).
                let items: Vec<Value> = heap
                    .map_entries(id)
                    .into_iter()
                    .flat_map(|(k, v)| [k, v])
                    .collect();
                return self.literal(heap, LiteralKind::Map, &items);
            }
            ValueRef::Set(id) => {
                let items = heap.set_elems(id);
                return self.literal(heap, LiteralKind::Set, &items);
            }
            ValueRef::Pair(_) => {} // combination, handled below
            _ => return Ok(Step::Value(self.expr)),
        }

        // GC safepoint — fires at ANY eval depth (ADR-061). The loop-top is the one point in
        // a step where the only live LOCAL transients are `expr` / `env`; they're passed as
        // roots below. Every pending frame's transients sit on the heap operand stack
        // (`roots`/`env_roots`), which `collect` relocates in place, and the dynamic stack is
        // scanned too — so no Rust frame holds an unrooted LOCAL handle a moving collection
        // would strand. The one exception is the macro-expansion compile pass (its forms
        // aren't operand-stack rooted), which opts out via `MACRO_BLOCK`. (The soft-memory
        // limit check below is deliberately *not* gated.) Cost when not collecting: one TLS
        // read + a `gc_due` compare.
        if !crate::process::macro_block_active() && heap.gc_due() {
            // Copying collection: `expr`/`env` MOVE to fresh slabs, so write the relocated
            // handles back into the registers (the dynamic stack and explicit root stack are
            // relocated in place by `collect`).
            let mut roots = [self.expr];
            let mut envs = [self.env];
            heap.collect(&mut roots, &mut envs);
            self.expr = roots[0];
            self.env = envs[0];
        }
        // RUNTIME-region safepoint — auto-compact the shared code region once hot-reload churn
        // crosses the threshold (the shared-code analog of the LOCAL collect above). Same
        // `macro_block_active` gate and same `expr`/`env` rewrite contract:
        // `maybe_runtime_collect` only rewrites RUNTIME handles (never moves LOCAL data), and
        // the rooted set it touches — globals + `roots`/`env_roots`/`dynamics` + both LOCAL
        // gens — is exactly the ADR-061 invariant the LOCAL safepoint already relies on (so
        // the VM's frame slots on `roots` are covered), plus the live `expr`/`env` passed
        // here. Runs only when this heap uniquely owns the runtime (single-process /
        // quiescent); a shared runtime backs off. Gated on `rt_dirty` (a real mint since the
        // last check), mirroring the VM safepoint: without it, `rt_gc_due` — a `cur_code()`
        // `ArcSwap` load — ran on every tree-walk step, and during a lingering multigen
        // drain that is pure overhead.
        if heap.rt_dirty() && !crate::process::macro_block_active() {
            heap.rt_dirty_clear();
            if heap.rt_gc_due() {
                let mut roots = [self.expr];
                let mut envs = [self.env];
                heap.maybe_runtime_collect(&mut roots, &mut envs);
                self.expr = roots[0];
                self.env = envs[0];
            }
        }
        // RUNTIME-drain cooperative report (ADR-091 Stage 3c). While a generation drain is
        // armed (rare), report whether this process still references the draining
        // generation, so the collector's cross-process union learns when it's dead. The
        // probe is read-only (it moves nothing), so it needs no GC/macro gate; the hot path
        // is one atomic load returning false.
        if heap.drain_active() {
            crate::process::report_drain_liveness(heap);
        }
        // Memory safety backstop (ADR-043): if total allocation has crossed the soft
        // ceiling, fail *here* with a clean, catchable error rather than running on to the
        // hard allocator limit (which aborts the whole process). Off by default; the test
        // runners set it so adversarial / hostile code can't exhaust host RAM. Checked at
        // every step, so a runaway loop sitting in *argument* position (`(f (build …))`) is
        // caught too. Cheap when disabled: one relaxed load of a zero, early `None`.
        // Crossing it stops the process holding the most memory, which is not necessarily
        // this one (`process/pressure.rs`, KI-229).
        match crate::core::alloc::soft_limit_state() {
            crate::core::alloc::SoftLimit::Clear => {}
            crate::core::alloc::SoftLimit::Near => crate::process::near_soft_limit(heap),
            crate::core::alloc::SoftLimit::Over(used) => {
                if let Some(error) = crate::process::soft_limit_crossed(heap, used) {
                    return Err(error.or_form_pos(heap, self.expr));
                }
            }
        }
        // Per-process heap limit (`(proc/flag :max-heap n)`): the sticky flag a collection
        // armed raises here, killing (catchably) just this process.
        if let Some(live) = heap.take_proc_limit_hit() {
            let limit = heap.proc_mem_limit().unwrap_or(0);
            return Err(proc_memory_limit_error(live, limit).or_form_pos(heap, self.expr));
        }
        // Mailbox bound (`(proc/flag :max-mailbox n)`, ADR-307): the breach a sender armed
        // raises here, in the flooded process only.
        if let Some((len, limit)) = crate::process::take_current_mailbox_overflow() {
            return Err(proc_mailbox_limit_error(len, limit).or_form_pos(heap, self.expr));
        }
        // Reduction-counted preemption: bound the work a process does before it yields its
        // worker (fairness — a CPU-bound process can't monopolise a core). Counted per
        // *combination* (a function call / special form), which is where loops actually
        // occur — leaf evals (symbols, literals) return above without a tick. A cheap
        // thread-local decrement; a no-op for the root thread. See `process::tick`.
        //
        // On the rollover the tick also honours a pending hard `:kill`
        // (`tick_reporting_hard_kill`): the tree-walker runs nested behind native frames
        // (`eval`/`reflect/eval-string`), so it can't return a `Killed` outcome — but it can
        // *unwind*, and the untrappable kill signal is converted to process death by the
        // body driver. Without this (and the same check in the passthrough redirect, where
        // thin-wrapper-heavy loops actually tick), a green process spinning inside `eval`
        // could not be killed at all.
        if crate::process::tick_reporting_hard_kill() {
            // Deliberately undecorated (no `or_form_pos`): a control signal is not a
            // user-visible error — it never prints, it only unwinds to the driver.
            return Err(LispError::kill_signal());
        }

        let expr = self.expr;
        let (head, rest) = match expr.unpack() {
            ValueRef::Pair(p) => heap.pair(p),
            _ => unreachable!(),
        };

        // --- special forms ---
        if let ValueRef::Sym(s) = head.unpack() {
            match special_form(s) {
                Some(SpecialForm::Quote) => {
                    // `(quote x)` returns x literally — but only x; reject `(quote a b)`
                    // rather than silently dropping the tail. A *missing* operand is the
                    // degenerate case, not a dropped one, and follows the house rule the
                    // other forms use — `(do)`, `(if)`, `((fn ()))` all yield nil, so
                    // `(quote)` does too (tests/malformed_test.blsp §degenerate special
                    // forms).
                    let (form, r) = uncons(heap, rest);
                    if !matches!(r.unpack(), ValueRef::Nil) {
                        return Err(LispError::arity("quote: expected exactly one argument")
                            .or_form_pos(heap, expr));
                    }
                    return Ok(Step::Value(form));
                }
                Some(SpecialForm::If) => {
                    // (if test then else?) — read the operands straight off the cons spine;
                    // a missing branch defaults to nil, so no intermediate Vec is allocated
                    // per conditional.
                    let (test_form, r) = uncons(heap, rest);
                    let (then_form, r) = uncons(heap, r);
                    let (else_form, r) = uncons(heap, r);
                    // The else-branch is a *single* form — reject a trailing tail rather
                    // than silently dropping it. In Emacs Lisp `(if c then e1 e2 …)` runs
                    // the trailing forms as an implicit-`progn` else; that muscle memory
                    // would otherwise vanish here without a peep. Point at `do`.
                    if !matches!(r.unpack(), ValueRef::Nil) {
                        return Err(LispError::arity(
                            "if: too many arguments — the else-branch is a single form; \
                             wrap multiple forms in (do …)",
                        )
                        .or_form_pos(heap, expr));
                    }
                    if let Some(test) = leaf_value(heap, test_form, self.env) {
                        self.expr = if truthy(test) { then_form } else { else_form };
                        return Ok(Step::Continue);
                    }
                    let marks = Marks::take(heap);
                    let then_r = heap.root(then_form);
                    let else_r = heap.root(else_form);
                    let env_r = heap.root_env(self.env);
                    let kind = KontKind::If {
                        then_r,
                        else_r,
                        env_r,
                    };
                    return self.push(heap, marks, kind, test_form, true, self.env);
                }
                Some(SpecialForm::Do) => return self.body(heap, rest, self.env),
                Some(SpecialForm::Def) => return self.def(heap, expr, rest),
                Some(SpecialForm::Fn) => {
                    // Fallback: a multi-clause / pattern-parameter `fn` normally lowers to
                    // `match*` in the compile pass, but can reach eval unlowered (built by a
                    // quasiquote, or a macro expanded lazily within its defining form).
                    // Lower it here and re-enter. The common case is detected away cheaply,
                    // so this never touches an ordinary `fn`.
                    if crate::eval::macros::fn_needs_lowering(heap, expr) {
                        self.expr = crate::eval::macros::macroexpand_all(heap, expr, self.env)?;
                        return Ok(Step::Continue);
                    }
                    let module = top_level_module(heap, self.env);
                    return make_closure(heap, None, rest, self.env, module).map(Step::Value);
                }
                Some(SpecialForm::Quasiquote) => {
                    // One template, and no trailing tail — the same rule `quote` follows,
                    // for the same reason: `` `(a) b `` must not silently drop `b`. (And
                    // the same degenerate case: no template at all yields nil, like
                    // `(quote)` / `(do)` / `(if)`, because nothing is being *dropped*.)
                    let (template, r) = uncons(heap, rest);
                    if !matches!(r.unpack(), ValueRef::Nil) {
                        return Err(
                            LispError::arity("quasiquote: expected exactly one argument")
                                .or_form_pos(heap, expr),
                        );
                    }
                    // Expand the template into *builder code* (a pure structural transform
                    // — no eval re-entry, so no GC-rooting hazard), then hand it back to
                    // the loop to evaluate. Tail position: the builder code's value is this
                    // form's value, and the loop-top safepoint roots the new `expr`.
                    self.expr = crate::eval::macros::expand_quasiquote(heap, template)
                        .map_err(|e| e.or_form_pos(heap, expr))?;
                    return Ok(Step::Continue);
                }
                Some(SpecialForm::Let) => return self.let_form(heap, expr, rest, false),
                Some(SpecialForm::Letrec) => return self.let_form(heap, expr, rest, true),
                None => {}
            }
        }

        // --- macro expansion + function application ---
        // Resolve the head once: a symbol head is looked up in the environment (a single
        // walk, shared by the macro check and the callee), any other head form is
        // evaluated. A macro expands and re-enters the loop; anything else becomes the
        // callee. `call_form` is this combination — the fallback position for an error from
        // a sub-eval / primitive / closure-arity check that didn't tag itself with a
        // more-inner one (`or_form_pos` checks `pos.is_none()` first, so the innermost
        // annotation wins).
        let mut call_form = expr;
        let mut spine = rest;
        let callee = match head.unpack() {
            ValueRef::Sym(s) => {
                // An unbound-symbol error from a *tail-position* call exits this frame
                // directly — no outer `or_form_pos` will see it. Attach `call_form`'s
                // position here so the diagnostic points at the failing call's line, not the
                // enclosing top-level form's start. A hit does no GC work; a MISS may
                // autoload the module (ADR-335), which collects — so that path roots
                // `call_form` + `env` and re-derives the spine.
                let v = match heap.env_get(self.env, s) {
                    Some(v) => crate::builtins::contracts::boundary_resolve(heap, self.env, v),
                    None => {
                        let env = self.env;
                        let (v, new_call_form, new_env) = heap.root_scope(|heap| {
                            let call_form_r = heap.root(call_form);
                            let env_r = heap.root_env(env);
                            let looked_up = crate::eval::derive::global_miss(heap, env, s);
                            let call_form = heap.read_root(call_form_r);
                            let v = looked_up.map_err(|e| e.or_form_pos(heap, call_form))?;
                            Ok((v, call_form, heap.read_root_env(env_r)))
                        })?;
                        call_form = new_call_form;
                        self.env = new_env;
                        spine = cdr(heap, call_form);
                        v
                    }
                };
                if let ValueRef::Macro(mid) = v.unpack() {
                    return self.expand_macro(heap, mid, call_form, spine);
                }
                v
            }
            _ => match leaf_value(heap, head, self.env) {
                Some(callee) => callee,
                None => {
                    // A computed head (`((f) …)`) is evaluated as a frame of its own; the
                    // call resumes from `Head` with the callee.
                    let marks = Marks::take(heap);
                    let call_form_r = heap.root(call_form);
                    let env_r = heap.root_env(self.env);
                    let kind = KontKind::Head { call_form_r, env_r };
                    return self.push(heap, marks, kind, head, true, self.env);
                }
            },
        };
        self.call(heap, callee, call_form, spine, self.env)
    }

    /// A macro call: expand it (native recursion — stage 2b) and re-enter the loop with the
    /// expansion.
    fn expand_macro(
        &mut self,
        heap: &mut Heap,
        mid: ClosureId,
        call_form: Value,
        spine: Value,
    ) -> Result<Step, LispError> {
        self.lazy += 1;
        if self.lazy > crate::eval::macros::MAX_EXPAND_ROUNDS {
            return Err(LispError::runtime(format!(
                "macro expansion did not reach a fixpoint after {} rounds \
                 (a macro that expands to itself?)",
                crate::eval::macros::MAX_EXPAND_ROUNDS
            ))
            .or_form_pos(heap, call_form));
        }
        // Macro expansion can collect at any depth (ADR-061); root `env` across it so the
        // loop re-reads the relocated handle. `arg_forms` is consumed by `bind_params`
        // inside `apply_closure`, which roots it itself.
        let arg_forms = heap.list_to_vec(spine)?;
        let env = self.env;
        let (expanded, new_env) = heap.root_scope(|heap| {
            let env_r = heap.root_env(env);
            let out =
                apply_closure(heap, mid, &arg_forms).map_err(|e| e.or_form_pos(heap, call_form))?;
            // A template may emit `/name` — the root escape (ADR-236) that stops a user
            // module capturing a prelude reference. The compile pass resolves it, but a
            // macro reached HERE was not expanded at compile time (it was defined after its
            // use site, or defined at runtime), so nothing has rewritten it and the
            // evaluator would see the literal symbol `/nil?` and call it unbound. Strip it
            // on the way out — the runtime half of `macros::resolve`'s root case. Cheap: the
            // scan allocates nothing and rebuilds only when an escape is actually present.
            let out = crate::eval::macros::strip_root_escapes(heap, out);
            Ok((out, heap.read_root_env(env_r)))
        })?;
        self.env = new_env;
        self.expr = expanded;
        Ok(Step::Continue)
    }

    /// `(def name value?)`.
    fn def(&mut self, heap: &mut Heap, expr: Value, rest: Value) -> Result<Step, LispError> {
        let args = heap.list_to_vec(rest)?;
        // `(def name value?)` — at most a name and one value. Reject a trailing tail rather
        // than silently dropping it (a `(def pi 3.14 "doc")` docstring habit, or stray forms
        // the user expected to run, would vanish without a peep). `def` takes no docstring
        // — use `defn`, or attach docs to the value.
        if args.len() > 2 {
            return Err(LispError::arity(
                "def: too many arguments — expected (def name value); \
                 def takes no docstring and runs no extra forms",
            )
            .or_form_pos(heap, expr));
        }
        let name = as_symbol(
            heap,
            "def: name",
            args.first()
                .copied()
                .ok_or_else(|| LispError::runtime("def: missing name"))?,
        )?;
        // ADR-070: root a qualified def TARGET exactly as a qualified *reference* roots.
        // During a module load this is a no-op — the expand pass already rewrote the target
        // to its rooted spelling. It matters for a `def` built as DATA and eval'd with no
        // compile-ns in scope, which is how instrumentation rebinds a function:
        // `std/tool/debug`'s `(eval (list 'def 'main/greet wrapper))`. Unrooted, that writes
        // a NEW global `main/greet` and leaves the real `brk/main/greet` — the one every
        // compiled call site resolves — untouched, so the "traced" function is never the
        // one that runs. Idempotent on an already-rooted name.
        let name = heap.root_qualified_ref(name).unwrap_or(name);
        // **Reserved names** (ADR-166): a global `def` may not rebind anything the language
        // ships — the prelude, the builtins, or an embedded std module. Checked here because
        // every `def` routes through this arm (the VM defers `def` forms to the evaluator),
        // so one gate covers both engines, `load`, `eval` and the REPL.
        //
        // **Before** the value is evaluated: the name is fully known by now, so running the
        // RHS first only meant a rejected `def` had already fired its side effects —
        // `(def map (do (print "SIDE") 1))` printed, *then* errored.
        if heap.is_reserved_global(name) {
            let shown = value::symbol_name(name);
            return Err(LispError::type_err(format!(
                "def: `{shown}` is a reserved name — it ships with Brood and cannot be redefined"
            ))
            .with_hint(format!(
                "Reserved names are everything inside the `brood` binary: the prelude, the \
                 builtins, and the embedded std modules. Your own code and your packages stay \
                 fully redefinable (that is what hot reload is for). Either pick another name, \
                 shadow it locally — `(let ({shown} …) …)` is still fine — or define it inside a \
                 `(defmodule your/mod …)`, where `(defn {shown} …)` defines `your/mod/{shown}` \
                 and is yours."
            ))
            .or_form_pos(heap, expr));
        }
        let Some(&rhs) = args.get(1) else {
            return def_bind(heap, name, Value::nil(), self.env).map(Step::Value);
        };
        // Above tier 0 the RHS runs on the VM (it's already macroexpanded+resolved if this
        // `def` came through the normal pipeline; `run` falls back to the tree-walker for
        // anything it can't compile). Without this a top-level `(def x <expr>)` evaluates
        // `<expr>` entirely on the tree-walker — `def` is a special form, so the whole form
        // (RHS included) was deferring. Not `run_top_form`: this tags the error with the
        // RHS's own position.
        if compile::tier_ceiling() >= compile::Tier::Bytecode {
            let env = self.env;
            let (v, new_env) = heap.root_scope(|heap| {
                let env_r = heap.root_env(env);
                let v = compile::run(heap, rhs, env).map_err(|e| e.or_form_pos(heap, rhs))?;
                Ok((v, heap.read_root_env(env_r)))
            })?;
            self.env = new_env;
            return def_bind(heap, name, v, new_env).map(Step::Value);
        }
        if let Some(v) = leaf_value(heap, rhs, self.env) {
            return def_bind(heap, name, v, self.env).map(Step::Value);
        }
        let marks = Marks::take(heap);
        let env_r = heap.root_env(self.env);
        self.push(
            heap,
            marks,
            KontKind::Def { name, env_r },
            rhs,
            true,
            self.env,
        )
    }

    /// `let` (sequential, lexical) or `letrec` (every name visible to every value).
    fn let_form(
        &mut self,
        heap: &mut Heap,
        expr: Value,
        rest: Value,
        rec: bool,
    ) -> Result<Step, LispError> {
        let who = if rec { "letrec" } else { "let" };
        let (binds_form, body) = uncons(heap, rest);
        if !matches!(rest.unpack(), ValueRef::Pair(_)) {
            return Err(LispError::runtime(format!("{who}: missing bindings")));
        }
        let binds = as_binding_vec(heap, who, binds_form)?;
        if binds.len() % 2 != 0 {
            return Err(scheme_binding_error(heap, who, &binds));
        }
        let pattern_target = binds
            .iter()
            .step_by(2)
            .any(|&b| !matches!(b.unpack(), ValueRef::Sym(_)));
        let scope = if rec {
            // Plain-symbol targets only — letrec exists for mutual recursion of *named*
            // values; pattern binding would muddy what "the names visible in the RHSs"
            // means.
            if pattern_target {
                // A non-symbol target is always an error here (letrec has no
                // destructuring), so flagging the nested-Scheme shape is zero-regression:
                // `(letrec ((f …) (g …)) …)` — even and function-valued, which the literal
                // guard misses — gets the flatten hint via scheme_binding_error's own
                // `looks_nested`.
                if binds.iter().all(|&b| {
                    matches!(b.unpack(), ValueRef::Pair(_))
                        && heap.seq_items(b).map(|it| it.len() == 2).unwrap_or(false)
                }) {
                    return Err(scheme_binding_error(heap, "letrec", &binds));
                }
                return Err(LispError::runtime(
                    "letrec: binding targets must be plain symbols",
                ));
            }
            // Mutual local recursion: every binding's name is visible in every binding's
            // RHS (and to itself). The frame is a last-write-wins association vector
            // (`env_define` pushes, lookup scans from the end), so all names are made
            // visible by pre-defining them to nil, then each RHS is evaluated in the scope
            // and the real value pushed — closures built during the bind phase capture the
            // scope and resolve names lazily at call time, so mutual recursion just works.
            // Brood stays immutable: touching a name before its RHS has been evaluated
            // reads the placeholder, but for the recursion-of-functions case letrec is
            // meant for, the bodies don't fire until call time, by which point the real
            // value is the latest entry. (No eval here → no GC.)
            let scope = heap.new_env(Some(self.env));
            for pair in binds.chunks(2) {
                let bind_name = as_symbol(heap, "binding name", pair[0])?;
                heap.env_define(scope, bind_name, Value::nil());
            }
            scope
        } else {
            // An *even*-length nested-Scheme form — `(let ((a 1) (b 2)) …)` — otherwise
            // slips past the odd-length guard above into the pattern-lowering fallback and
            // dies with a confusing "unbound symbol" (it evaluates `(b 2)` as a call).
            // Catch the literal-valued shape here so the flatten hint fires. Guarded by
            // `is_literal_atom` on every value slot so it can't regress a genuine bare-list
            // destructure like `(let ((a b) '(1 2)) …)`.
            if even_bindings_look_scheme(heap, &binds) {
                return Err(scheme_binding_error(heap, "let", &binds));
            }
            // Fallback: a pattern (non-symbol) binding target reached eval unlowered — same
            // paths as `fn`. Lower via the compile pass and re-enter; the common all-symbol
            // `let` skips this.
            if pattern_target {
                self.expr = crate::eval::macros::macroexpand_all(heap, expr, self.env)?;
                return Ok(Step::Continue);
            }
            heap.new_env(Some(self.env))
        };
        let marks = Marks::take(heap);
        let st = BindState {
            binds: binds.iter().map(|&b| heap.root(b)).collect(),
            i: 0,
            scope_r: heap.root_env(scope),
            body_r: heap.root(body),
            lexical: !rec,
            captured: false,
        };
        self.binds(heap, st, marks)
    }

    /// Bind from `st.i` on, pushing at the first value that is not a leaf; then run the
    /// body.
    fn binds(
        &mut self,
        heap: &mut Heap,
        mut st: BindState,
        marks: Marks,
    ) -> Result<Step, LispError> {
        while st.i < st.binds.len() {
            let name = heap.read_root(st.binds[st.i]);
            if let Err(e) = as_symbol(heap, "binding name", name) {
                marks.restore(heap);
                return Err(e);
            }
            let rhs = heap.read_root(st.binds[st.i + 1]);
            let scope = heap.read_root_env(st.scope_r);
            match leaf_value(heap, rhs, scope) {
                Some(v) => {
                    st.define(heap, v);
                    st.i += 2;
                }
                None => return self.push(heap, marks, KontKind::Bind(st), rhs, true, scope),
            }
        }
        let scope = heap.read_root_env(st.scope_r);
        let body = heap.read_root(st.body_r);
        marks.restore(heap);
        self.body(heap, body, scope)
    }

    /// Run a body held as a cons list: each non-last form for effect, the last as this
    /// frame's tail. Errors on an improper list.
    fn body(&mut self, heap: &mut Heap, body: Value, env: EnvId) -> Result<Step, LispError> {
        let mut cell = body;
        loop {
            match cell.unpack() {
                ValueRef::Nil => return Ok(Step::Value(Value::nil())),
                ValueRef::Pair(p) => {
                    let (form, next) = heap.pair(p);
                    if matches!(next.unpack(), ValueRef::Nil) {
                        self.expr = form;
                        self.env = env;
                        return Ok(Step::Continue);
                    }
                    if leaf_value(heap, form, env).is_some() {
                        cell = next;
                        continue;
                    }
                    let marks = Marks::take(heap);
                    let cursor = heap.root(cell);
                    let env_r = heap.root_env(env);
                    return self.push(
                        heap,
                        marks,
                        KontKind::Body { cursor, env_r },
                        form,
                        true,
                        env,
                    );
                }
                _ => return Err(LispError::type_err("improper body list")),
            }
        }
    }

    /// Run a closure body (non-empty): each non-last form for effect, the last as this
    /// frame's tail.
    fn forms(&mut self, heap: &mut Heap, forms: &[Value], env: EnvId) -> Result<Step, LispError> {
        let (&last, init) = forms.split_last().expect("a closure body runs non-empty");
        for (i, &form) in init.iter().enumerate() {
            if leaf_value(heap, form, env).is_some() {
                continue;
            }
            let marks = Marks::take(heap);
            let rest = forms[i + 1..].iter().map(|&f| heap.root(f)).collect();
            let env_r = heap.root_env(env);
            return self.push(
                heap,
                marks,
                KontKind::Forms { rest, env_r },
                form,
                true,
                env,
            );
        }
        self.lazy = 0;
        self.expr = last;
        self.env = env;
        Ok(Step::Continue)
    }

    /// A vector, set or map literal: each element evaluated in order.
    fn literal(
        &mut self,
        heap: &mut Heap,
        kind: LiteralKind,
        items: &[Value],
    ) -> Result<Step, LispError> {
        let marks = Marks::take(heap);
        let st = LiteralState {
            kind,
            src: items.iter().map(|&it| heap.root(it)).collect(),
            out: SmallVec::with_capacity(items.len()),
            env_r: heap.root_env(self.env),
        };
        self.literal_next(heap, st, marks)
    }

    fn literal_next(
        &mut self,
        heap: &mut Heap,
        mut st: LiteralState,
        marks: Marks,
    ) -> Result<Step, LispError> {
        while st.out.len() < st.src.len() {
            let form = heap.read_root(st.src[st.out.len()]);
            let env = heap.read_root_env(st.env_r);
            match leaf_value(heap, form, env) {
                Some(v) => {
                    let r = heap.root(v);
                    st.out.push(r);
                }
                // Untagged, as the literal's old nested `eval` was.
                None => return self.push(heap, marks, KontKind::Literal(st), form, false, env),
            }
        }
        let out: Vec<Value> = st.out.iter().map(|&r| heap.read_root(r)).collect();
        marks.restore(heap);
        Ok(Step::Value(match st.kind {
            LiteralKind::Vector => heap.alloc_vector(out),
            LiteralKind::Set => heap.set_from_elems(out),
            LiteralKind::Map => {
                let pairs = out.chunks(2).map(|kv| (kv[0], kv[1])).collect();
                heap.map_from_pairs(pairs)
            }
        }))
    }

    /// Evaluate a call's arguments off the cons `spine`, then apply `callee`. The callee, the
    /// call form, the spine cursor, the environment and every argument so far are rooted for
    /// as long as an argument runs as a frame of its own.
    fn call(
        &mut self,
        heap: &mut Heap,
        callee: Value,
        call_form: Value,
        spine: Value,
        env: EnvId,
    ) -> Result<Step, LispError> {
        if matches!(spine.unpack(), ValueRef::Nil) {
            return self.apply(heap, callee, SmallVec::new(), call_form, env);
        }
        let marks = Marks::take(heap);
        let st = ArgsState {
            call_form_r: heap.root(call_form),
            callee_r: heap.root(callee),
            spine_r: heap.root(spine),
            env_r: heap.root_env(env),
            args: SmallVec::new(),
        };
        self.args_next(heap, st, marks)
    }

    fn args_next(
        &mut self,
        heap: &mut Heap,
        mut st: ArgsState,
        marks: Marks,
    ) -> Result<Step, LispError> {
        loop {
            let form = match heap.read_root(st.spine_r).unpack() {
                ValueRef::Nil => break,
                ValueRef::Pair(p) => heap.pair(p).0,
                // A dotted argument spine — `(f a . b)`. Tagged with the call form's own
                // position like every other call-shape error.
                _ => {
                    let call_form = heap.read_root(st.call_form_r);
                    marks.restore(heap);
                    return Err(LispError::type_err("improper argument list in call")
                        .with_hint(
                            "The argument list ends in a dotted tail. A call's arguments must be \
                             a proper list — to call with a list of arguments use `(apply f xs)`.",
                        )
                        .or_form_pos(heap, call_form));
                }
            };
            let env = heap.read_root_env(st.env_r);
            match leaf_value(heap, form, env) {
                Some(v) => {
                    let r = heap.root(v);
                    st.args.push(r);
                    let next = cdr(heap, heap.read_root(st.spine_r));
                    st.spine_r = heap.advance_root(st.spine_r, next);
                }
                None => return self.push(heap, marks, KontKind::Args(st), form, true, env),
            }
        }
        let argv: SmallVec<[Value; 8]> = st.args.iter().map(|&r| heap.read_root(r)).collect();
        let callee = heap.read_root(st.callee_r);
        let call_form = heap.read_root(st.call_form_r);
        let env = heap.read_root_env(st.env_r);
        marks.restore(heap);
        self.apply(heap, callee, argv, call_form, env)
    }

    /// Apply `callee` to the evaluated `argv`: a native answers, a closure's body becomes
    /// this frame's (its last form the tail).
    fn apply(
        &mut self,
        heap: &mut Heap,
        callee: Value,
        argv: SmallVec<[Value; 8]>,
        call_form: Value,
        env: EnvId,
    ) -> Result<Step, LispError> {
        let mut call_form = call_form;
        let mut env = env;
        // Inline `apply` unfolding: when the callee is the `apply` builtin, splice the
        // trailing sequence into `argv` and dispatch the real callee here — instead of going
        // through `call_native(apply)` → `apply_builtin` → `eval::apply` (which would add
        // ~4 Rust frames per chained `(apply f …)` and defeat TCO for `apply`-driven
        // recursion). Looped so nested `(apply apply (list f xs))` is also flattened. A
        // mis-arity (`(apply f)`) is left for `call_native` to flag with the canonical
        // "expected at least 2 arguments" message.
        let mut cur_callee = callee;
        let mut cur_argv = argv;
        // Re-entered when `apply` is unfolded or a thin-wrapper closure (`passthrough_arm`)
        // redirects to its inner call — both rewrite `cur_callee`/`cur_argv` and
        // `continue 'dispatch` rather than recursing.
        'dispatch: loop {
            while let ValueRef::Native(id) = cur_callee.unpack() {
                if heap.native(id).name != "apply" || cur_argv.len() < 2 {
                    break;
                }
                let last = cur_argv.pop().expect("argv non-empty (checked above)");
                let mut real = cur_argv.remove(0);
                // A lazy seq-view as the spliced arg list realises first — `seq_items`
                // can't run its transducer. The realise re-enters `eval` (a GC safepoint
                // that relocates LOCAL handles), so root `real` and the remaining leading
                // args across it and re-read after — otherwise
                // `(apply <local-closure> … <seq-view>)` derefs a stale handle →
                // use-after-GC (ADR-114 re-read discipline; mirrors `realize_seqviews`).
                let last = if matches!(last.unpack(), ValueRef::SeqView(_)) {
                    heap.root_scope(|heap| -> LispResult {
                        let real_r = heap.root(real);
                        let arg_roots: Vec<_> = cur_argv.iter().map(|&v| heap.root(v)).collect();
                        let realized = crate::builtins::realize_seqview(heap, env, last)?;
                        real = heap.read_root(real_r);
                        for (slot, &r) in cur_argv.iter_mut().zip(arg_roots.iter()) {
                            *slot = heap.read_root(r);
                        }
                        Ok(realized)
                    })
                    .map_err(|e| e.or_form_pos(heap, call_form))?
                } else {
                    last
                };
                cur_argv.extend(
                    heap.seq_items(last)
                        .map_err(|e| e.or_form_pos(heap, call_form))?,
                );
                cur_callee = real;
            }

            match cur_callee.unpack() {
                ValueRef::Native(id) => {
                    return call_native(heap, id, &cur_argv, env)
                        .map(Step::Value)
                        .map_err(|e| e.or_form_pos(heap, call_form));
                }
                ValueRef::Fn(id) => {
                    // Thin-wrapper elision: redirect a pure pass-through arm (`(%add a b)`
                    // for `+`, etc.) straight to its inner call on the already-evaluated
                    // args, skipping the redundant frame + bind + body walk. Only redirect
                    // when it resolves to a function; anything else falls through to the
                    // normal path.
                    if let Some((head, map)) = passthrough_arm(heap, id, cur_argv.len()) {
                        let cl_env = heap.closure(id).env.unwrap_or_else(|| heap.global());
                        // Inner-head resolution. A bound, non-dynamic symbol is a plain
                        // lookup, which cannot collect. Anything else goes through a full
                        // `eval` — and that CAN collect: an unbound qualified head is a lazy
                        // module load (ADR-335), i.e. arbitrary evaluation. So the slow path
                        // roots the args across it (a `bytes` arg reached `seq` stale under
                        // `BROOD_TIER=0 nest test` before it did, deterministic with
                        // `BROOD_GC_STRESS=1`). The shared `passthrough_redirect_ok` then
                        // gates the redirect (callable inner only), counts the reduction, and
                        // honours the deadline.
                        let fast = match head {
                            Value::Sym(s) if !heap.is_dynamic(s) => heap.env_get(cl_env, s),
                            _ => None,
                        };
                        let inner = match fast {
                            Some(v) => v,
                            None => heap
                                .root_scope(|heap| {
                                    let arg_roots: SmallVec<[Root; 8]> =
                                        cur_argv.iter().map(|&v| heap.root(v)).collect();
                                    let form_r = heap.root(call_form);
                                    let env_r = heap.root_env(env);
                                    let inner = eval(heap, head, cl_env)?;
                                    for (slot, &r) in cur_argv.iter_mut().zip(arg_roots.iter()) {
                                        *slot = heap.read_root(r);
                                    }
                                    call_form = heap.read_root(form_r);
                                    env = heap.read_root_env(env_r);
                                    Ok(inner)
                                })
                                .map_err(|e: LispError| e.or_form_pos(heap, call_form))?,
                        };
                        // A wrapper forwarding to a contract shim of its own module forwards
                        // to the original instead (the boundary, ADR-383).
                        let inner = crate::builtins::contracts::boundary_redirect(heap, id, inner)
                            .unwrap_or(inner);
                        // A redirect back to the *same* closure is direct self-recursion
                        // (`(defn hog () (hog))`), not a thin wrapper — fall through to the
                        // normal call path (which re-enters the loop, whose reduction check
                        // can preempt) rather than spinning the redirect.
                        let self_cycle =
                            matches!(inner.unpack(), ValueRef::Fn(iid) if iid.0 == id.0);
                        if !self_cycle
                            && passthrough_redirect_ok(inner)
                                .map_err(|e| e.or_form_pos(heap, call_form))?
                        {
                            let mut next_argv: SmallVec<[Value; 8]> =
                                SmallVec::with_capacity(map.len());
                            for &i in &map {
                                next_argv.push(cur_argv[i]);
                            }
                            cur_callee = inner;
                            cur_argv = next_argv;
                            continue 'dispatch;
                        }
                    }
                    // Tree-walker→VM router (see `tw_vm_route`): a VM-eligible callee runs
                    // at engine speed instead of being tree-walked because its CALLER
                    // happened to defer. Off below the bytecode tier.
                    if let Some(r) = tw_vm_route(heap, id, &cur_argv) {
                        return r
                            .map(Step::Value)
                            .map_err(|e| e.or_form_pos(heap, call_form));
                    }
                    // `bind_params` selects the arm matching this call's arity, binds it,
                    // and hands back that arm's body. An arity/bind failure appends a
                    // name-only trace entry for the callee — the same attribution the VM
                    // produces (a no-arm call defers to `apply_closure`, whose bind error is
                    // name-only).
                    let cl_name = heap.closure(id).name;
                    let cl_module = heap.closure(id).module;
                    let (scope, body) = bind_params(heap, id, &cur_argv).map_err(|e| {
                        let mut e = e.or_form_pos(heap, call_form);
                        e.push_trace(crate::error::TraceFrame {
                            name: cl_name.map(value::symbol_name_ref),
                            file: None,
                            pos: None,
                        });
                        e
                    })?;
                    // Error-trace bookkeeping: this frame is now entering `id`'s body. Pass
                    // the name captured *above* (pre-`bind_params`): the default eval may
                    // have compacted the RUNTIME region and staled `id`.
                    let shim_file = contract_shim_file(heap, cl_module, &body);
                    record_tw_entry(heap, self.entered, cl_name, shim_file, call_form);
                    if body.is_empty() {
                        return Ok(Step::Value(Value::nil()));
                    }
                    return self.forms(heap, &body, scope);
                }
                // A keyword in head position is an accessor (ADR-165); the same helper
                // `apply` uses, so a direct `(:k m)` and a routed `(apply :k …)` can't
                // diverge.
                ValueRef::Keyword(_) => {
                    return apply_keyword(heap, cur_callee, &cur_argv)
                        .map(Step::Value)
                        .map_err(|e| e.or_form_pos(heap, call_form));
                }
                other => {
                    return Err(not_a_function_error(heap, other).or_form_pos(heap, call_form));
                }
            }
        }
    }
}

impl BindState {
    /// Bind the pending binding (`i`) to `val`. Plain `let`: once a value that could have
    /// built a closure has been bound, the next binding goes into a fresh CHILD frame instead
    /// of being appended to the frame that closure may have captured. Frame lookup is by name
    /// over the whole chain, so appending made a later binding visible to an earlier closure:
    /// `(let (x 1 f (fn () x) x 2) (f))` answered 2, where the compiled tiers (one slot per
    /// binding, resolved at compile time) answer 1. Only a list value form can build a
    /// closure, so the common atom-valued `let` still binds into one frame. `letrec` binds
    /// into its one frame, its names meant to be visible to every value.
    fn define(&mut self, heap: &mut Heap, val: Value) {
        let name = match heap.read_root(self.binds[self.i]).unpack() {
            ValueRef::Sym(s) => s,
            _ => unreachable!("binding names are checked before their value runs"),
        };
        let rhs_is_list = matches!(
            heap.read_root(self.binds[self.i + 1]).unpack(),
            ValueRef::Pair(_)
        );
        let mut scope = heap.read_root_env(self.scope_r);
        if self.lexical {
            if self.captured {
                // `val` stays live across `new_env`: an allocation, never a collection.
                scope = heap.new_env(Some(scope));
                self.scope_r = heap.root_env(scope);
            }
            self.captured = rhs_is_list;
        }
        heap.env_define(scope, name, val);
    }
}

/// Bind `name` to `val` as a global (the rest of `def`, once its value is known).
fn def_bind(heap: &mut Heap, name: Symbol, val: Value, env: EnvId) -> LispResult {
    let val = name_value(heap, val, name);
    let root = heap.env_root(env);
    // Arity-change diagnostic: if `def` is *rebinding* a callable to one of a different
    // arity, callers expecting the old shape will hit a runtime arity error on the next
    // call. Surface it at reload time so the mismatch isn't a silent surprise (the
    // hot-reload + late-binding contract — docs/shared-code.md). Fires only when an old
    // binding exists, so the prelude/std first-time build is silent.
    if let Some(old) = heap.env_get(root, name) {
        if let (Some(old_a), Some(new_a)) = (value_arity(heap, old), value_arity(heap, val))
            && (old_a.min != new_a.min || old_a.max != new_a.max)
            && reload_diagnostics_enabled(heap, root)
        {
            eprintln!(
                "[reload] arity changed for {}: {} -> {}",
                value::symbol_name(name),
                arity_to_string(old_a),
                arity_to_string(new_a),
            );
        }
        // Macro-redefinition diagnostic (hot reload): redefining a macro does *not*
        // re-expand callers already compiled with the old expansion — they keep the old
        // code until re-eval'd. `defmacro` is a macro lowering to `(def name (%make-macro
        // …))`, so the diagnostic lives on the `def` side.
        if matches!(old.unpack(), ValueRef::Macro(_))
            && matches!(val.unpack(), ValueRef::Macro(_))
            && reload_diagnostics_enabled(heap, root)
        {
            eprintln!(
                "[reload] macro {} redefined; callers expanded before now keep the old expansion — re-eval them",
                value::symbol_name(name)
            );
        }
    }
    heap.env_define(root, name, val);
    // While an embedded std module is loading, its own definitions become reserved as they
    // appear — that is how `set/union` and friends join the reserved set without a static
    // name list. Same rule as the seed filter: an embedded module's FUNCTIONS become
    // reserved, its data globals (its own registries, e.g. `protocol`'s
    // `*protocols*`/`*impls*`) stay rebindable — they are rebound by the module's own
    // functions at runtime.
    if heap.in_module_load()
        && matches!(
            val.unpack(),
            ValueRef::Fn(_) | ValueRef::Macro(_) | ValueRef::Native(_)
        )
    {
        heap.reserve_global(name);
    }
    // The name's uncontracted alias, if one is bound, follows every rebinding (ADR-383) —
    // before the hook below re-wraps a closure, and for the rebindings the hook does not
    // handle.
    crate::builtins::contracts::mirror_alias(heap, root, name, val);
    // A closure whose name carries a declared signature is offered to the contract policy
    // (ADR-381): armed, the binding becomes a checking shim; unarmed, this is one store
    // lookup. Order-free — a `sig` above its `defn` is enforced here, one below it at
    // `%register-sig` — and a hot reload re-wraps, since it is a `def`. Whole-module batches
    // (an embedded std module, the prelude at boot) are swept from Brood instead, so the
    // stdlib image never carries a shim.
    if matches!(val.unpack(), ValueRef::Fn(_))
        && !heap.in_module_load()
        && heap.declared_sig_value(name).is_some()
    {
        crate::builtins::contracts::contract_apply(
            heap,
            name,
            crate::builtins::contracts::OnShim::Rewrap,
        )?;
    }
    Ok(Value::symbol(name))
}
