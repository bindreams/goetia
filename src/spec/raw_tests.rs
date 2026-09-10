use super::RawManifest;

fn parse(yaml: &str) -> Result<RawManifest, serde_yaml_ng::Error> {
    RawManifest::parse(yaml)
}

#[skuld::test]
fn raw_parse_rejects_duplicate_daemon_ids() {
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
  frpc:
    command: [bin/frpc2]
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("frpc"),
        "error should name the offending id: {err}"
    );
}

#[skuld::test]
fn rejects_case_insensitively_colliding_ids() {
    let yaml = "
daemons:
  Frpc:
    command: [bin/frpc]
  frpc:
    command: [bin/frpc2]
";
    let err = parse(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Frpc") && msg.contains("frpc"),
        "error should name both colliding ids: {msg}"
    );
}

#[skuld::test]
fn accepts_a_single_daemon() {
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    assert_eq!(manifest.daemons.len(), 1);
    assert!(manifest.daemons.contains_key("frpc"));
}

#[skuld::test]
fn rejects_missing_daemons_key() {
    let err = parse("{}").unwrap_err();
    assert!(err.to_string().contains("daemons"));
}

#[skuld::test]
fn rejects_unknown_top_level_key() {
    let yaml = "
daemons: {}
extra: true
";
    let err = parse(yaml).unwrap_err();
    assert!(err.to_string().contains("extra"));
}

#[skuld::test]
fn rejects_unknown_daemon_field() {
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    bogus: true
";
    let err = parse(yaml).unwrap_err();
    assert!(err.to_string().contains("bogus"));
}

// Null rejection ======================================================================================================

#[skuld::test]
fn explicit_null_is_rejected_for_every_base_field() {
    // One message per field kind, all of them naming `null`: the scan
    // walks the document before any type is imposed on it, so `command`
    // and `env` are refused as themselves rather than as an `invalid
    // type` against the sequence or map they were about to become.
    let fields = [
        "name",
        "command",
        "cwd",
        "env",
        "user",
        "restart",
        "restart-delay",
        "logs",
        "type",
    ];
    for field in fields {
        // `command` is required, so every row but its own has to supply it.
        let mut yaml = String::from("daemons:\n  frpc:\n");
        if field != "command" {
            yaml.push_str("    command: [bin/frpc]\n");
        }
        yaml.push_str(&format!("    {field}: null\n"));

        let err = parse(&yaml).unwrap_err();
        let msg = err.to_string();
        let expected = match field {
            "command" => "an explicit `null` is not a command",
            "env" => "an explicit `null` is not an environment block",
            _ => "an explicit `null` is not a way to unset this field",
        };
        assert!(
            msg.contains(expected),
            "field `{field}`: expected `{expected}`, got: {msg}"
        );
    }
}

#[skuld::test]
fn an_explicit_null_restart_is_rejected() {
    // Standing alone, not folded into the table above: `restart` is one of
    // the three fields whose authored-text `Option<String>` could plausibly
    // be re-typed to its parsed form, and the null-rejection is easiest to
    // lose in exactly that change.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    restart: null
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string()
            .contains("an explicit `null` is not a way to unset this field")
    );
}

#[skuld::test]
fn an_explicit_null_type_is_rejected() {
    // See `an_explicit_null_restart_is_rejected`: `type` is the `Kind` half.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    type: null
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string()
            .contains("an explicit `null` is not a way to unset this field")
    );
}

#[skuld::test]
fn an_explicit_null_restart_delay_is_rejected() {
    // See `an_explicit_null_restart_is_rejected`: a `Duration` via
    // `humantime_serde::option` swallows a `null` silently.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    restart-delay: null
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string()
            .contains("an explicit `null` is not a way to unset this field")
    );
}

#[skuld::test]
fn a_null_command_element_is_rejected() {
    // `command[0]` is the executable path: an unguarded `null` element is
    // absolutized into `<manifest dir>/null`, which `reject_empty` cannot
    // catch because it is not empty. A later position is no better — it
    // becomes the literal argv string `null`.
    for command in ["[null]", "[/bin/frpc, null]", "[/bin/frpc, ~]"] {
        let yaml = format!(
            "
daemons:
  frpc:
    command: {command}
"
        );
        let err = parse(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("an explicit `null` is not a command element"),
            "`{command}`: {err}"
        );
    }
}

#[skuld::test]
fn an_empty_command_element_is_accepted() {
    // Only `null` is refused: an empty argv element is legitimate on POSIX,
    // and every generator quotes it — see `reject_empty`'s doc comment.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc, \"\"]
";
    let manifest = parse(yaml).expect("an empty argv element should parse");
    assert_eq!(
        manifest.daemons["frpc"].command,
        ["bin/frpc".to_string(), String::new()]
    );
}

#[skuld::test]
fn null_env_values_are_rejected() {
    for spelling in ["null", "~", ""] {
        let yaml = format!(
            "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      A: {spelling}
"
        );
        let err = parse(&yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("an explicit `null` is not an environment value"),
            "spelling `{spelling}`: got {err}"
        );
    }
}

#[skuld::test]
fn an_empty_env_value_is_accepted() {
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      A: \"\"
";
    let manifest = parse(yaml).expect("an empty env value should parse");
    assert_eq!(manifest.daemons["frpc"].env.get("A"), Some(&String::new()));
}

#[skuld::test]
fn null_env_keys_are_rejected() {
    let cases = [
        (
            "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      null: v
",
            "unquoted `null` key",
        ),
        (
            "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      ~: v
",
            "unquoted `~` key",
        ),
        (
            "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      ?
      : v
",
            "explicit empty key",
        ),
    ];
    for (yaml, label) in cases {
        let err = parse(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("an explicit `null` is not an environment variable name"),
            "{label}: got {err}"
        );
    }
}

#[skuld::test]
fn a_quoted_null_env_key_and_value_are_accepted() {
    // The guards refuse the `null` *value*, never the four characters: a
    // guard that rejected both would pass every test above and still be
    // wrong.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      \"null\": \"null\"
";
    let manifest = parse(yaml).expect("a quoted `null` key and value should parse");
    assert_eq!(manifest.daemons["frpc"].env.get("null"), Some(&"null".to_string()));
}

#[skuld::test]
fn duplicate_env_key_is_rejected() {
    // `env`'s values become a privileged service's environment, so the
    // insert-and-overwrite a typed `BTreeMap` would do is the one silent
    // loss this manifest surface cannot afford: the daemon would run with
    // an environment the author never wrote and cannot see they lost.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      PORT: \"8080\"
      PORT: \"9090\"
";
    let err = parse(yaml).unwrap_err();
    assert!(err.to_string().contains("PORT"), "error should name `PORT`: {err}");
}

#[skuld::test]
fn env_keys_that_differ_only_in_case_are_rejected() {
    // The same loss, one platform along: `std::process::Command` stores
    // its environment under a case-folding `EnvKey` on Windows, and the
    // SCM's registry block is read back through `GetEnvironmentVariable`,
    // which is case-insensitive too — so `{Path: a, PATH: b}` reaches the
    // child as one variable, exactly the outcome the byte-exact check was
    // written to stop. Refused on every platform, because a manifest is
    // portable and the author gets no warning on the host where it bites.
    // `DaemonsVisitor` has refused a case collision in daemon ids for the
    // same reason since before this check existed.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      Path: a
      PATH: b
";
    let err = parse(yaml).unwrap_err().to_string();
    assert!(
        err.contains("Path") && err.contains("PATH"),
        "error should name both keys: {err}"
    );
    assert!(
        err.contains("case-insensitively"),
        "error should say the comparison folds case, and why: {err}"
    );
}

#[skuld::test]
fn env_keys_that_differ_by_more_than_case_are_accepted() {
    // The other half of the rule: nothing but a case fold is collapsed.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    env:
      A: \"1\"
      AB: \"2\"
      A_: \"3\"
      B: \"4\"
";
    let manifest = parse(yaml).expect("distinct keys should parse");
    assert_eq!(manifest.daemons["frpc"].env.len(), 4);
}

#[skuld::test]
fn a_null_user_name_or_id_is_rejected() {
    // Both halves of `user:`, and both by name: this is the position that
    // decides a service's security principal, and an unrefused `null` here
    // installs it under the literal account `null` (for `name`) or under
    // whatever the untagged `AccountId` made of a unit (for `id`, whose
    // message used to be `data did not match any variant of untagged enum
    // AccountId` — a Rust type name, no `null`, and no remedy).
    for (field, expected) in [
        ("name", "an explicit `null` is not a username"),
        ("id", "an explicit `null` is not an account id"),
    ] {
        for spelling in ["null", "~", ""] {
            let yaml = format!(
                "
daemons:
  frpc:
    command: [bin/frpc]
    user:
      {field}: {spelling}
"
            );
            let err = parse(&yaml).unwrap_err().to_string();
            assert!(err.contains(expected), "`{field}: {spelling}`: {err}");
            assert!(err.contains("daemons.frpc.user"), "should name the position: {err}");
        }
    }
}

#[skuld::test]
fn a_null_daemon_id_is_rejected() {
    let cases = [
        (
            "
daemons:
  null:
    command: [bin/frpc]
",
            "unquoted `null` id",
        ),
        (
            "
daemons:
  ~:
    command: [bin/frpc]
",
            "unquoted `~` id",
        ),
        (
            "
daemons:
  ?
  :
    command: [bin/frpc]
",
            "explicit empty id",
        ),
    ];
    for (yaml, label) in cases {
        let err = parse(yaml).unwrap_err();
        assert!(
            err.to_string().contains("an explicit `null` is not a daemon id"),
            "{label}: got {err}"
        );
    }
}

#[skuld::test]
fn a_quoted_null_daemon_id_is_accepted() {
    let yaml = "
daemons:
  \"null\":
    command: [bin/frpc]
";
    let manifest = parse(yaml).expect("a quoted `null` id should parse");
    assert!(manifest.daemons.contains_key("null"));
}

#[skuld::test]
fn unquoted_scalars_still_coerce_through_the_null_guards() {
    // The other half of the guards' contract: refusing `null` must not cost
    // the unquoted-scalar coercion, which is why they deserialize
    // `Option<T>` rather than sniffing the scalar's text.
    let yaml = "
daemons:
  frpc:
    name: 42
    command: [/bin/sleep, 30]
    env:
      PORT: 8080
";
    let manifest = parse(yaml).expect("unquoted scalars should coerce, not fail");
    let spec = &manifest.daemons["frpc"];

    assert_eq!(spec.name.as_deref(), Some("42"));
    assert_eq!(spec.command, ["/bin/sleep".to_string(), "30".to_string()]);
    assert_eq!(spec.env.get("PORT"), Some(&"8080".to_string()));
}

// Rejection position and identity =====================================================================================

/// Every guarded position, with the offence deliberately placed on a line
/// of its own that is *not* the first line of its enclosing container —
/// the case a rejection built after `Option::<T>::deserialize` has already
/// returned gets wrong, because `serde_yaml_ng` has by then stamped the
/// container's own start position onto it.
#[skuld::test]
fn a_rejection_points_at_the_offending_node_and_names_it() {
    let cases: [(&str, &str, &str); 6] = [
        (
            "\
daemons:
  frpc:
    command: [bin/frpc]
    cwd: /tmp
    logs: /tmp/l.log
    name: hello
    type: simple
    restart-delay: 5s
    restart: null
",
            "daemons.frpc.restart",
            "line 9",
        ),
        (
            "\
daemons:
  frpc:
    command: [bin/frpc]
    env:
      A: \"1\"
      B: \"2\"
      C: null
      D: \"4\"
",
            "daemons.frpc.env.C",
            "line 7",
        ),
        (
            "\
daemons:
  frpc:
    command: [bin/frpc]
    env:
      A: \"1\"
      B: \"2\"
      ~: \"3\"
",
            "daemons.frpc.env",
            "line 7",
        ),
        (
            "\
daemons:
  frpc:
    command:
      - bin/frpc
      - --config
      - null
",
            "daemons.frpc.command[2]",
            "line 6",
        ),
        (
            "\
daemons:
  frpc:
    command: [bin/frpc]
    user:
      name: null
",
            "daemons.frpc.user.name",
            "line 5",
        ),
        (
            "\
daemons:
  frpc:
    command: [bin/frpc]
  ~:
    command: [bin/other]
",
            "daemons",
            "line 4",
        ),
    ];

    for (yaml, path, line) in cases {
        let err = parse(yaml).unwrap_err().to_string();
        assert!(err.contains(path), "expected path `{path}` in: {err}");
        assert!(err.contains(line), "expected `{line}` in: {err}");
    }
}

/// `serde_yaml_ng` decides null-ness from a scalar's *style*, so a
/// `!!null`-tagged **quoted** scalar arrives as ordinary text unless the
/// guard reads the resolved node instead. `!!null "null"` is the one
/// spelling YAML defines as unambiguously a null, so it must not be the
/// one spelling that gets through.
#[skuld::test]
fn a_tag_resolved_null_is_rejected_wherever_a_plain_one_is() {
    let positions: [(&str, &str); 6] = [
        (
            "daemons:\n  frpc:\n    command: [bin/frpc]\n    name: {null}\n",
            "not a way to unset this field",
        ),
        (
            "daemons:\n  frpc:\n    command: [bin/frpc]\n    env:\n      {null}: v\n",
            "not an environment variable name",
        ),
        (
            "daemons:\n  frpc:\n    command: [bin/frpc]\n    env:\n      A: {null}\n",
            "not an environment value",
        ),
        (
            "daemons:\n  frpc:\n    command: [bin/frpc, {null}]\n",
            "not a command element",
        ),
        ("daemons:\n  {null}:\n    command: [bin/frpc]\n", "not a daemon id"),
        (
            "daemons:\n  frpc:\n    command: [bin/frpc]\n    user:\n      name: {null}\n",
            "not a username",
        ),
    ];
    for (template, expected) in positions {
        for spelling in ["!!null null", "!!null ~", "!!null \"null\"", "!!null \"~\""] {
            let yaml = template.replace("{null}", spelling);
            let err = parse(&yaml).unwrap_err().to_string();
            assert!(
                err.contains(expected),
                "`{spelling}`: expected `{expected}`, got: {err}"
            );
        }
        // `!!null ""` is a null per the YAML spec but not per
        // `serde_yaml_ng`'s `parse_null`, which accepts only
        // `null`/`Null`/`NULL`/`~`. It refuses the tag itself rather than
        // reaching this crate's visitor, so only the refusal is asserted.
        let yaml = template.replace("{null}", "!!null \"\"");
        let err = parse(&yaml).unwrap_err().to_string();
        assert!(err.contains("expected null"), "`!!null \"\"`: got: {err}");
    }
}

/// An empty value is a YAML null, and `null` is refused *wherever a
/// manifest value is expected* — including where the expected value is a
/// container. `serde_yaml_ng` maps an empty plain scalar onto an empty
/// map or sequence when asked for one, so without a guard `daemons:` with
/// no body installs nothing and exits 0.
#[skuld::test]
fn an_empty_container_body_is_rejected() {
    let cases: [(&str, &str); 6] = [
        ("daemons:\n", "not a set of daemons"),
        ("daemons: null\n", "not a set of daemons"),
        (
            "daemons:\n  frpc:\n    command: [bin/frpc]\n    env:\n",
            "not an environment block",
        ),
        (
            "daemons:\n  frpc:\n    command: [bin/frpc]\n    env: null\n",
            "not an environment block",
        ),
        ("daemons:\n  frpc:\n    command:\n", "not a command"),
        ("daemons:\n  frpc:\n    command: null\n", "not a command"),
    ];
    for (yaml, expected) in cases {
        let err = parse(yaml).unwrap_err().to_string();
        assert!(err.contains(expected), "expected `{expected}`, got: {err}");
    }
}

/// The other half of the rule above: an explicitly *written* empty
/// container is a value the author chose, and stays legal.
#[skuld::test]
fn an_explicitly_empty_container_is_accepted() {
    let manifest = parse("daemons: {}\n").expect("`daemons: {}` should parse");
    assert!(manifest.daemons.is_empty());

    let manifest = parse("daemons:\n  frpc:\n    command: [bin/frpc]\n    env: {}\n").expect("`env: {}` should parse");
    assert!(manifest.daemons["frpc"].env.is_empty());
}
