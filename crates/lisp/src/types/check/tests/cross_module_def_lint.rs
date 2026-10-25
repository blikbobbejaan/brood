//! The `:cross-module-def` lint.

use super::*;

// ---- a load-time `def` of another module's global (`:cross-module-def`) ----
// Two modules re-binding one global while they load race under the parallel cold load
// (ADR-426): each reads the old value and one write is lost. The lint reads the EXPANDED
// forms, because the `def` usually lives in a macro's template in the module that owns the
// global and becomes a cross-module write only where the macro is used.

/// `src` checked as a file with `owner` (a module's source) already loaded — how a project
/// file is checked against the modules it uses: their globals are bound, and their macros
/// are the ones the checker expands.
fn warns_after(owner: &str, src: &str) -> bool {
    let mut interp = crate::Interp::new();
    interp.eval_str(owner).expect("load the owning module");
    let mut heap =
        crate::core::heap::Heap::with_regions(interp.heap.prelude_arc(), interp.heap.runtime_arc());
    heap.set_global(crate::core::value::EnvId::GLOBAL);
    let forms = crate::syntax::reader::read_all(&mut heap, src).expect("parse");
    check_file_mode(&mut heap, &forms, &[], false)
        .iter()
        .any(|(_, m)| m.starts_with(cross_module_def::CROSS_MODULE_DEF_PREFIX))
}

/// bedit's theme, in miniature: two globals, and a macro that re-binds the owner's own.
const OWNER: &str = "(defmodule theme) (def x 0) (def rows []) \
                     (defmacro defrow (n) `(def theme/rows (conj theme/rows ~n)))";

#[test]
fn cross_module_def_warns_on_a_load_time_rebinding_of_another_modules_global() {
    for src in [
        "(defmodule a) (def theme/x 1)",
        "(defmodule a) (do (def theme/rows (conj theme/rows 1)))",
        "(defmodule a) (let (row 1) (def theme/rows (conj theme/rows row)))",
        // bedit's `defchrome`: the def is the owner's own in its template, and a foreign one
        // where the macro expands
        "(defmodule vcsproc) (theme/defrow 1)",
    ] {
        assert!(
            warns_after(OWNER, src),
            "must warn on a load-time cross-module def ({src})"
        );
    }
}

#[test]
fn cross_module_def_stays_silent_on_its_own_globals_new_names_and_code_that_runs_later() {
    for src in [
        "(defmodule a) (def x 1)",
        "(defmodule a) (def a/x 1)",
        // the owner using its own macro writes its own global
        "(defmodule theme) (theme/defrow 1)",
        // the project-qualified spelling of the file's own module
        "(defmodule theme) (def bedit/theme/x 1)",
        // a NEW name is no re-binding: the linmap rewrite names helpers `fn/linmap-loop__N`
        "(defmodule a) (def theme/not-there-yet 1)",
        "(defmodule a) (def lm-tally/linmap-loop__1 1)",
        // a function body runs when it is called, not while the module loads
        "(defmodule a) (defn f () (def theme/x 1))",
        // data, and a template the module itself never runs
        "(defmodule a) (def forms '(def theme/x 1))",
        "(defmodule a) (defmacro reg (n) `(def theme/rows (conj theme/rows ~n)))",
        // no `defmodule`: root globals, no namespace of its own to compare against
        "(def theme/x 1)",
    ] {
        assert!(!warns_after(OWNER, src), "must not warn ({src})");
    }
}

#[test]
fn cross_module_def_check_allow_suppresses_only_its_category() {
    assert!(!warns_after(
        OWNER,
        "(defmodule a) (check-allow :cross-module-def (def theme/x 1))"
    ));
    assert!(warns_after(
        OWNER,
        "(defmodule a) (check-allow :discarded-catch (def theme/x 1))"
    ));
}
