# Fixtures

## `native-0.6.0-results.json`

What tool-git 0.6.0 answered for every call of `tests/common/sequence.rs`, run over a fresh
`tests/common/fixture.rs` work folder, and the argv of every git command it ran. Each answer is
reduced to what reached the model (`status`, `summary`, and `data` as a string or compact JSON) and
normalised by `tests/common/normalise.rs`: the work folder becomes `<WORK>`, each 40-hex hash
`<C1>`, `<C2>`, … in first-seen order with its abbreviations `<Cn:len>`, an abbreviation of an
object whose full hash never appears `<H1>`, …, and each ISO-8601 date-time `<DATE>`.

`cargo test -p tool-git --test parity` compares the component's answers and argv with it, and the
ignored `mur_run_parity` suite compares what a real `mur run` hands the model. The file is frozen:
never edit it by hand.

Recorded from tool-git 0.6.0, built from commit `95e17e1`.
