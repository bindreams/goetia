use super::reject_nulls;

/// The scan on its own, without the typed parse that `RawManifest::parse`
/// runs before it. Positions this module owns are exercised through
/// `parse` in `raw_tests`; what is tested here is the scan's own contract.
fn scan(yaml: &str) -> Result<(), String> {
    reject_nulls(yaml).map_err(|error| error.to_string())
}

#[skuld::test]
fn a_null_key_is_refused_as_a_key_and_not_as_a_value() {
    // "omit the key instead" is the answer to `restart: null`, not to a
    // key that is itself a null: there is no field to unset.
    for yaml in [
        "~: 1\n",
        "daemons:\n  frpc:\n    ~: 1\n",
        "daemons:\n  frpc:\n    user:\n      ~: 1\n",
    ] {
        let err = scan(yaml).unwrap_err();
        assert!(err.contains("is not a field name"), "{yaml:?}: {err}");
    }
}

#[skuld::test]
fn a_locally_tagged_key_keeps_the_slot_of_the_value_beside_it() {
    // A key carrying a local tag arrives as an enum. The typed parse reads
    // it as the field it names, so the scan has to as well, or the two
    // walks disagree about which slot the value sits in and the refusal
    // contradicts its own key path.
    for (yaml, expected) in [
        (
            "daemons:\n  frpc:\n    !mine env:\n      A: null\n",
            "is not an environment value",
        ),
        ("!mine daemons:\n  frpc: null\n", "is not a daemon"),
        (
            "daemons:\n  frpc:\n    !mine command: [/x, null]\n",
            "is not a command element",
        ),
        (
            "daemons:\n  frpc:\n    !mine user:\n      !mine name: null\n",
            "is not a username",
        ),
    ] {
        let err = scan(yaml).unwrap_err();
        assert!(err.contains(expected), "{yaml:?}: expected `{expected}`, got: {err}");
    }
}

#[skuld::test]
fn the_scan_refuses_nothing_but_a_null() {
    // `deserialize_any` checks a standard tag's content against the tag
    // before the visitor is reached, so the scan sees errors that are not
    // its business. The typed parse has already accepted this document —
    // it reads every scalar as a string and ignores the tag — so there is
    // nothing left to report.
    for value in [
        "!!int abc",
        "!!float abc",
        "!!bool yes",
        "!!bool maybe",
        "!!null \"\"",
        "!!null \"x\"",
    ] {
        let yaml = format!("daemons:\n  frpc:\n    command: [/x]\n    env:\n      V: {value}\n");
        scan(&yaml).unwrap_or_else(|err| panic!("`{value}` is not a null: {err}"));
    }
}

#[skuld::test]
fn a_null_after_a_tag_the_scan_cannot_read_is_still_refused() {
    // Discarding an error the scan may not report means *keep walking*: a
    // null behind an unreadable tag must not escape with it.
    for yaml in [
        "daemons:\n  frpc:\n    command: [/x]\n    env: {V: !!int abc}\n    restart: null\n",
        "daemons:\n  frpc:\n    command: [/x]\n    env: {V: !!int abc, W: null}\n",
        "daemons:\n  frpc:\n    command: [/x, !!int abc, null]\n",
        "daemons:\n  a:\n    command: [/x]\n    env: {V: !!bool maybe}\n  null:\n    command: [/y]\n",
        "daemons:\n  frpc:\n    command: [/x]\n    env: {!!int abc: v, W: null}\n",
        // The key is the unreadable one, and the null is *under* it: the
        // typed parse read that key as `abc` and `env`, so the subtree is
        // part of the manifest and cannot be skipped along with the key.
        "daemons:\n  !!int abc:\n    command: [/x]\n    restart: null\n",
        "daemons:\n  frpc:\n    command: [/x]\n    !!int env:\n      A: null\n",
        "daemons:\n  frpc:\n    command: [/x]\n    env: {!!int abc: null}\n",
    ] {
        let err = scan(yaml).unwrap_err();
        assert!(err.contains("an explicit `null`"), "{yaml:?}: {err}");
    }
}

#[skuld::test]
fn the_slot_table_answers_the_positions_the_typed_parse_takes_first() {
    // A null daemon body, a null `user.id` and a null field name are the
    // typed parse's to refuse — `RawSpec` needs a `command`, `AccountId`
    // guards its own unit, and a null key is an unknown field — so these
    // sentences are the ones the scan *would* give. They are pinned here
    // because nothing else reaches them, and because a field of that shape
    // added to `RawSpec` would make them live again.
    for (yaml, expected) in [
        ("daemons:\n  frpc: null\n", "is not a daemon"),
        (
            "daemons:\n  frpc:\n    command: [/x]\n    user:\n      id: null\n",
            "is not an account id",
        ),
        ("daemons: null\n", "is not a set of daemons"),
        ("daemons:\n  frpc:\n    command: null\n", "is not a command"),
        ("daemons:\n  frpc:\n    env: null\n", "is not an environment block"),
    ] {
        let err = scan(yaml).unwrap_err();
        assert!(err.contains(expected), "{yaml:?}: expected `{expected}`, got: {err}");
    }
}
