//! **What a runtime knows about where its modules come from belongs to that runtime**, not to
//! the OS process (KI-226, the KI-224 class).
//!
//! Two memos the module resolver keeps were process-wide statics:
//!
//! - the set of modules a `require` has already failed to FIND. Whether a module can be found
//!   depends on `*load-path*`, a global of ONE runtime, so a miss in one runtime made another
//!   runtime — whose load path holds the module — skip the search and report the name
//!   unbound;
//! - the kind index of the images a runtime opened (which modules an image holds, and which
//!   of their names are macros). A runtime that never opened an image trusted another
//!   runtime's, deferred a module's load past expansion, and compiled its macro as a call.
//!
//! Each runtime runs on its own thread (a root context belongs to a thread), and the steps
//! are sequenced over channels, so the interleaving is forced rather than left to timing.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;

use brood::Interp;

/// A runtime on its own thread, evaluating one source string per request.
struct Runtime {
    requests: mpsc::Sender<String>,
    replies: mpsc::Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl Runtime {
    fn start() -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<String>();
        let (reply_sender, reply_receiver) = mpsc::channel::<String>();
        let thread = std::thread::spawn(move || {
            let mut interp = Interp::new();
            for source in request_receiver {
                let reply = match interp.eval_str(&source) {
                    Ok(value) => interp.print(value),
                    Err(error) => format!("error: {error}"),
                };
                if reply_sender.send(reply).is_err() {
                    break;
                }
            }
        });
        Runtime {
            requests: request_sender,
            replies: reply_receiver,
            thread: Some(thread),
        }
    }

    fn eval(&self, source: &str) -> String {
        self.requests
            .send(source.to_string())
            .expect("runtime thread alive");
        self.replies
            .recv_timeout(std::time::Duration::from_secs(120))
            .unwrap_or_else(|error| panic!("no reply to {source}: {error}"))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let (closed, _) = mpsc::channel();
        self.requests = closed;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A fresh, empty directory for one test.
fn scratch_directory(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("brood-ki226-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    directory
}

fn quoted(path: &std::path::Path) -> String {
    format!("{:?}", path.display().to_string())
}

/// A module one runtime cannot find must still be found by a runtime whose load path has it.
#[test]
fn a_module_absent_in_one_runtime_is_still_found_in_another() {
    let directory = scratch_directory("absent");
    let empty = directory.join("empty");
    let full = directory.join("full");
    std::fs::create_dir_all(&empty).expect("create the empty load-path directory");
    std::fs::create_dir_all(&full).expect("create the full load-path directory");
    std::fs::write(
        full.join("kiabsent.blsp"),
        "(defmodule kiabsent \"found on one runtime's load path\")\n(defn probe () :found)\n",
    )
    .expect("write the module");

    let has_it = Runtime::start();
    let lacks_it = Runtime::start();
    // Each setter clears the absence memo; both run before the miss.
    has_it.eval(&format!("(reflect/set-load-path (list {}))", quoted(&full)));
    lacks_it.eval(&format!(
        "(reflect/set-load-path (list {}))",
        quoted(&empty)
    ));

    let missed = lacks_it.eval("(try (kiabsent/probe) (catch e :unbound))");
    assert_eq!(
        missed, ":unbound",
        "control: the runtime without the module on its load path must not find it"
    );
    assert_eq!(
        has_it.eval("(kiabsent/probe)"),
        ":found",
        "a module one runtime failed to find was skipped by another runtime whose load path \
         holds it — the absence memo was process-wide (KI-226)"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// An image opened by one runtime must not tell another runtime that a name is a function.
#[test]
fn another_runtimes_image_does_not_defer_this_runtimes_macro() {
    let directory = scratch_directory("kinds");
    let image = directory.join("kikind.img");
    let modules = directory.join("modules");
    std::fs::create_dir_all(&modules).expect("create the module directory");
    // From source, `kikind/quoted` is a MACRO: `(kikind/quoted hello)` is the symbol `hello`.
    std::fs::write(
        modules.join("kikind.blsp"),
        "(defmodule kikind \"its one name is a macro\")\n\
         (defmacro quoted (form) `(quote ~form))\n",
    )
    .expect("write the module");

    // Runtime A images a module of the same name whose `kikind/quoted` is a FUNCTION, and
    // opens that image — registering its kinds.
    let imaging = Runtime::start();
    let written = imaging.eval(&format!(
        "(do (defn kikind/quoted (form) form)
             (%image-write {path} [[\"kikind\" ['kikind/quoted]]] \"ki226-fp\")
             (count (%image-index {path} \"ki226-fp\")))",
        path = quoted(&image)
    ));
    assert_eq!(
        written, "1",
        "control: runtime A's image holds the one section"
    );

    // Runtime B never opened that image; its `kikind` is the source module with the macro.
    let reading = Runtime::start();
    reading.eval(&format!(
        "(reflect/set-load-path (list {}))",
        quoted(&modules)
    ));
    assert_eq!(
        reading.eval("(kikind/quoted hello)"),
        "hello",
        "another runtime's image kind index made this runtime defer a module whose name is a \
         macro, so the macro call compiled as a function call (KI-226)"
    );
    let _ = std::fs::remove_dir_all(&directory);
}
