//! Typed parameters (ADR-420): how a parameter symbol carries a type, read the same way by
//! every consumer of a parameter list — the expander that strips the types
//! (`eval/macros/typed.rs`), the CST scope analysis, the std index's arity count, and the
//! checker passes that read raw `defn` forms.
//!
//! `(defn area (w:number h:number -> number) …)`: a parameter `name:type` is typed with the
//! type NAME after the first colon; `name:` is typed with the FORM that follows it
//! (`xs: (list int)`); `-> TYPE` closes the list with the result type. A leading colon is a
//! keyword, never a typed name.

/// The result-type marker that closes a typed parameter list.
pub const ARROW: &str = "->";

/// How a parameter symbol's text carries a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedName<'a> {
    /// `name:type` — the type is the name after the colon.
    Inline(&'a str, &'a str),
    /// `name:` — the type is the next form in the list.
    Next(&'a str),
}

impl<'a> TypedName<'a> {
    /// The binder the parameter declares.
    pub fn name(self) -> &'a str {
        match self {
            TypedName::Inline(name, _) | TypedName::Next(name) => name,
        }
    }
}

/// Split a parameter symbol's text, or `None` for a plain name (or a keyword).
pub fn split(text: &str) -> Option<TypedName<'_>> {
    let at = text.find(':')?;
    if at == 0 {
        return None;
    }
    let (name, ty) = (&text[..at], &text[at + 1..]);
    Some(if ty.is_empty() {
        TypedName::Next(name)
    } else {
        TypedName::Inline(name, ty)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parameter_symbol_splits_at_its_first_colon() {
        assert_eq!(split("w:number"), Some(TypedName::Inline("w", "number")));
        assert_eq!(
            split("p:geom/point"),
            Some(TypedName::Inline("p", "geom/point"))
        );
        assert_eq!(split("xs:"), Some(TypedName::Next("xs")));
        assert_eq!(split("x"), None);
        assert_eq!(split(":ok"), None, "a keyword is not a typed name");
        assert_eq!(split("w:number").map(TypedName::name), Some("w"));
    }
}
