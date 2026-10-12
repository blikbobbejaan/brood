//! A type-variable declaration the stdlib image carries resolves at a call site from the
//! arguments, with its module never loaded (ADR-370). Its own process because the footer
//! index is process-global and additive: this test PLANTS an entry, which would break the
//! in-crate `the_footer_is_the_source_scan_byte_for_byte` gate beside it.
//!
//! The first case is planted as well as read from std because `math/max` and `math/min`
//! have a by-name rule (`is_extremum`) that answers from the operands whatever the footer
//! says, so a verdict on either passes with the call-site fallback removed.
//! `image-sig-probe/pick` has no rule, no binding and no module: the only thing that can
//! type the call is the footer's `(?A ?A -> ?A)`.

use brood::core::value::Tag;
use brood::eval::derive::register_image_sigs;
use brood::types::check::expr_ty_of;
use brood::types::Ty;
use brood::Interp;

#[test]
fn a_planted_type_variable_declaration_resolves_from_the_arguments() {
    if std::env::var_os("BROOD_NO_IMAGE_SIGS").is_some() {
        eprintln!("BROOD_NO_IMAGE_SIGS set — the footer is off by request, nothing to gate");
        return;
    }
    let mut interp = Interp::new();
    register_image_sigs(vec![(
        "image-sig-probe/pick".to_string(),
        "(?A ?A -> ?A)".to_string(),
        2,
        2,
    )]);
    let form = brood::syntax::reader::read_one(&mut interp.heap, "(image-sig-probe/pick 1 2)")
        .expect("parse");
    let ty = expr_ty_of(&interp.heap, form).expect("the call has a type");
    // `?A` bound to two int literals is a subtype of `int`; a reading that missed the footer
    // has nothing else to say about an unbound name and reads `any` (or nothing).
    assert!(
        ty.is_subtype(&Ty::of(Tag::Int)),
        "(image-sig-probe/pick 1 2) reads `{ty}` — the footer's declaration was not resolved"
    );
}

#[test]
fn a_std_type_variable_declaration_types_the_element_with_its_module_unloaded() {
    // `math/clamp` declares `(?A ?A ?A -> ?A)` and has no by-name rule: the result type
    // comes from the arguments, through the footer, with `math` never loaded. Its own
    // process for the same reason as the planted case — whether `math` is loaded is
    // process-wide state once an image is installed.
    if std::env::var_os("BROOD_NO_IMAGE_SIGS").is_some() {
        eprintln!("BROOD_NO_IMAGE_SIGS set — the footer is off by request, nothing to gate");
        return;
    }
    // Build the image, when the cache holds none for this std, in a THROWAWAY runtime:
    // `stdimage/build` loads every module into the runtime that runs it, and this test's
    // precondition is that `math` is not loaded in the runtime under test.
    {
        let mut builder = Interp::new();
        builder
            .eval_str("(or (%std-image-installed) (%std-image-install) (stdimage/build))")
            .expect("build the stdlib image");
    }
    let mut interp = Interp::new();
    let installed = interp
        .eval_str("(or (%std-image-installed) (%std-image-install))")
        .map(|v| interp.print(v))
        .expect("install the stdlib image");
    assert_ne!(
        installed, "nil",
        "no stdlib image could be installed even after building one"
    );
    let loaded = |interp: &mut Interp| -> bool {
        let v = interp
            .eval_str("(contains? *features* \"math\")")
            .expect("read *features*");
        interp.print(v) == "true"
    };
    assert!(
        !loaded(&mut interp),
        "math is loaded in a fresh process — the footer is not what answers"
    );
    let form =
        brood::syntax::reader::read_one(&mut interp.heap, "(math/clamp 5 0 10)").expect("parse");
    let ty = expr_ty_of(&interp.heap, form).expect("the call has a type");
    assert!(
        ty.is_subtype(&Ty::of(Tag::Int)),
        "(math/clamp 5 0 10) reads `{ty}` with math unloaded — the declaration's `?A` was not bound"
    );
    assert!(!loaded(&mut interp), "typing the call loaded math");
}
