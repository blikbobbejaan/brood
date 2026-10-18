//! The browser-tab display primitives: `%tab-post` queues a display event for the page a
//! program runs in, `%tab-subscribe` makes the calling process the one its input goes to.
//! Mechanism is `crate::host::tab`; the policy — which events, what JSON — is the engine's
//! (`b2d-tab`).

use crate::core::heap::Heap;
use crate::core::value::{EnvId, Value};
use crate::error::LispResult;

use super::args::{arg, expect_string};

/// Every primitive this file contributes: name, arity, signature, arglist, docstring.
pub(super) fn register(primitives: &mut super::Primitives) {
    use super::signature_types::*;
    use crate::core::value::Arity;
    use crate::types::Sig;
    primitives.def(
        "%tab-post",
        Arity::exact(2),
        Sig::new(vec![string, string], nil_ty),
        &["event", "data"],
        "Queue display event `event` (a name: \"frame\", \"texture\", \"sound\", \"window\", …) with payload `data` (its JSON) for the browser page this program runs in, which drains them after each pump. Outside a tab nothing drains them: the oldest are dropped past 4096. Returns nil.",
        tab_post,
    );
    primitives.def(
        "%tab-subscribe",
        Arity::exact(0),
        Sig::nullary(nil_ty),
        &[],
        "Make the calling process the one the page's input goes to: each message arrives as a string holding a Brood form, to be read (never evaluated). The last subscriber wins. Returns nil.",
        tab_subscribe,
    );
}

/// `(%tab-post event data)`.
fn tab_post(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let event = expect_string(heap, "%tab-post", arg(args, 0))?;
    let data = expect_string(heap, "%tab-post", arg(args, 1))?;
    crate::host::tab::post(event, data);
    Ok(Value::nil())
}

/// `(%tab-subscribe)`.
fn tab_subscribe(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
    crate::host::tab::subscribe(crate::process::self_pid());
    Ok(Value::nil())
}
