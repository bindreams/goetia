use super::RawManifest;

fn parse(yaml: &str) -> Result<RawManifest, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(yaml)
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
    // Two expected messages, not one: `command` and `env` are not
    // `Option`s, so their own deserializers raise `invalid type` before
    // `no_null` is ever reached. The refusal is what is asserted, not the
    // field name — `daemons.frpc` is the whole path serde_yaml_ng attaches
    // to a custom error raised inside a `deserialize_with`.
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
            "command" => "invalid type: unit value, expected a sequence",
            "env" => "invalid type: unit value, expected a map",
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
fn a_null_user_name_is_rejected() {
    // The `user:` field's own guard does not reach this leaf — see
    // `RawUser`'s `NoNullName`. Asserted from a whole manifest, not just
    // from `RawUser`, because this is the position that decides a service's
    // security principal.
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
    user:
      name: null
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("an explicit `null` is not a username"),
        "{err}"
    );
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
