//! A Brood program kept alive in a browser tab: the release bundle of a game, run by the
//! interpreter built for wasm32, driven by the page's event loop rather than run to
//! completion (that is the playground, `crates/playground`).
//!
//! The page fetches the bundle (`nest release` over an empty runtime writes one: the
//! project's manifest and modules, nothing else) and calls, in order:
//!
//! - `use_prelude_image(bytes)`, optionally — the prelude booted from an image this build
//!   wrote (`prelude_image()`, run once at build time in a fresh instance) rather than from
//!   source;
//! - `mount(bytes)` — make it the bundle `require` loads modules from;
//! - `start(args)` — run its `:main` with `args`, as a release binary would;
//! - `pump(budget_ms)` after every `input` and when the delay it returns elapses — it runs
//!   what is runnable and fires the receive timeouts that are due, never for longer than
//!   the budget, so the page keeps painting;
//! - `events()` after each pump — the display events the program posted (`%tab-post`);
//! - `input(text)` — one input message, as a Brood form, to the subscribed process;
//! - `ended()` — `undefined` while the program runs, then `""` or its error.

use std::cell::RefCell;
use std::sync::Arc;

use brood::Interp;
use brood::eval::compile::ProgramExit;
use wasm_bindgen::prelude::*;

thread_local! {
    // The interpreter owns the shared runtime the program's heaps are built over; it
    // must outlive the program, so it is kept beside the program's exit handle.
    static PROGRAM: RefCell<Option<(Interp, Arc<ProgramExit>)>> = const { RefCell::new(None) };
}

/// Mount the release bundle in `bundle` as the one `require` loads modules from.
#[wasm_bindgen]
pub fn mount(bundle: &[u8]) -> Result<(), JsError> {
    brood::bundle::mount_bytes(bundle).map_err(|e| JsError::new(&e))
}

/// Boot the prelude from `image` — what `prelude_image` answered for this same build —
/// instead of evaluating its source, which is most of the runtime's own startup in a tab.
/// Call before `start`; an image from another build is refused by its fingerprint and the
/// boot reads the source, as without one.
#[wasm_bindgen]
pub fn use_prelude_image(image: Vec<u8>) {
    brood::boot::provide_prelude_image(image);
}

/// The prelude image of this build, booting the prelude from source to make it: what a
/// build step saves beside the bundle for `use_prelude_image`. Empty when the prelude had
/// already booted (call it in a fresh instance).
#[wasm_bindgen]
pub fn prelude_image() -> Vec<u8> {
    brood::boot::capture_prelude_image().unwrap_or_default()
}

/// Start the mounted bundle's `:main` with `args`, the way a release binary boots
/// (`project-release/run-bundle`), and return at once: the program runs as `pump` drives it.
#[wasm_bindgen]
pub fn start(args: Vec<String>) -> Result<(), JsError> {
    let args: Vec<String> = args.iter().map(|a| brood_string(a)).collect();
    let source = format!("(project-release/run-bundle (list {}))", args.join(" "));
    let mut interp = Interp::new();
    let exit = interp
        .start_program(&source)
        .map_err(|e| JsError::new(&e.to_string()))?;
    PROGRAM.with(|p| *p.borrow_mut() = Some((interp, exit)));
    Ok(())
}

/// Run what is runnable and fire the receive timeouts that are due, for at most
/// `budget_ms`, and return the milliseconds until the next one is: 0 when the budget ran
/// out with work left (pump again after the page has painted), -1 with none pending.
#[wasm_bindgen]
pub fn pump(budget_ms: f64) -> f64 {
    let budget = std::time::Duration::from_secs_f64(budget_ms.max(1.0) / 1000.0);
    match brood::host::tab::pump(budget) {
        Some(delay) => delay.as_secs_f64() * 1000.0,
        None => -1.0,
    }
}

/// The display events posted since the last call, as `name NUL json NUL name NUL json …`
/// (a JSON text never holds a raw NUL), so a frame's JSON is handed over once, unparsed.
#[wasm_bindgen]
pub fn events() -> String {
    let mut out = String::new();
    for (name, data) in brood::host::tab::take_events() {
        out.push_str(&name);
        out.push('\0');
        out.push_str(&data);
        out.push('\0');
    }
    out
}

/// Hand `text` — one input message as a Brood form — to the program's subscribed process;
/// whether one has subscribed.
#[wasm_bindgen]
pub fn input(text: &str) -> bool {
    brood::host::tab::deliver_input(text)
}

/// How the program ended: nothing while it runs, then `""` for a normal end or its error.
#[wasm_bindgen]
pub fn ended() -> Option<String> {
    PROGRAM.with(|p| {
        p.borrow()
            .as_ref()
            .and_then(|(_, exit)| exit.ended())
            .map(|result| match result {
                Ok(()) => String::new(),
                Err(e) => e.to_string(),
            })
    })
}

/// The running runtime's version, for the page to show.
#[wasm_bindgen]
pub fn version() -> String {
    brood::VERSION.to_string()
}

/// `s` as a Brood string literal.
fn brood_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::brood_string;

    #[test]
    fn an_argument_becomes_a_string_literal() {
        assert_eq!(brood_string("--tab"), "\"--tab\"");
        assert_eq!(brood_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
