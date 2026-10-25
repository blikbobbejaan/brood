# Fowler testregex, leftmost-first (rust-lang/regex)

Glenn Fowler's testregex suite (AT&T Research), as carried by Go's `regexp/testdata`
and then by rust-lang/regex, which re-derived every expectation for **leftmost-first**
matching (Perl's semantics, and Brood's, ADR-389) and ships the cases as TOML. Each
`[[test]]` names a pattern, a haystack and the spans of the first match and of each
group in it (`[]` for a group that did not take part).

| file | cases |
|------|-------|
| `basic.toml` | 204 |
| `repetition.toml` | 91 |
| `nullsubexpr.toml` | 50 |

- **Upstream**: <https://github.com/rust-lang/regex/tree/master/testdata/fowler>
- **Pinned commit**: `80b9d14e4be9f0b703a742d11c064abce05f210b`
- **Licence**: MIT (rust-lang/regex's; see `LICENSE`). The data's history is in
  upstream's `testdata/fowler/dat/README`.
- **Runner**: `tests/conformance_regex_test.blsp`
- **Refresh**: `scripts/fetch-corpus.sh regex`

The runner keeps a list of cases Brood's dialect answers differently by design (no
case-insensitive mode, no POSIX `[[:class:]]` sets, `\n` and `\xHH` meaning letters),
each with its reason, and fails when a listed case starts agreeing.
