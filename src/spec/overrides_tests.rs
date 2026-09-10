use std::collections::BTreeMap;

use super::{RawOverride, Supplied};
use crate::spec::{Backend, RawManifest, RawUser};

fn parse(yaml: &str) -> Result<RawManifest, serde_yaml_ng::Error> {
    RawManifest::parse(yaml)
}

/// The nine overridable field names, byte-exact with `RawOverride`'s serde
/// names — shared by every table-driven test below so no field can be
/// added later without a row.
const OVERRIDABLE_FIELDS: [&str; 9] = [
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

#[skuld::test]
fn backend_specific_parses_a_per_backend_override() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        user: svc-frpc
        command: [/bin/frpc, --scm-only]
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let overrides = &manifest.daemons["frpc"].backend_specific;
    let scm = overrides.get(&Backend::Scm).expect("scm override should be present");

    assert_eq!(scm.user, Some(RawUser::Scalar("svc-frpc".to_string())));
    assert_eq!(
        scm.command.as_deref(),
        Some(&["/bin/frpc".to_string(), "--scm-only".to_string()][..])
    );
}

#[skuld::test]
fn a_daemon_without_backend_specific_has_no_overrides() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    assert!(
        manifest.daemons["frpc"].backend_specific.is_empty(),
        "an absent `backend-specific` should be an empty map, not a panic"
    );
}

#[skuld::test]
fn duplicate_backend_key_is_rejected() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        user: a
      scm:
        user: b
";
    let err = parse(yaml).unwrap_err();
    assert!(err.to_string().contains("scm"), "error should name `scm`: {err}");
}

#[skuld::test]
fn unknown_backend_key_is_rejected() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      windwos:
        user: a
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("windwos"),
        "error should name `windwos`: {err}"
    );
}

#[skuld::test]
fn unknown_field_in_an_override_is_rejected() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        bogus: true
";
    let err = parse(yaml).unwrap_err();
    assert!(err.to_string().contains("bogus"), "error should name `bogus`: {err}");
}

#[skuld::test]
fn an_override_cannot_nest_backend_specific() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        backend-specific:
          scm:
            user: a
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("backend-specific"),
        "error should name `backend-specific`: {err}"
    );
}

#[skuld::test]
fn every_overridable_field_parses() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        name: Frpc Tunnel
        command: [/bin/frpc, --scm]
        cwd: /opt/frpc
        env:
          LOG: info
        user: svc-frpc
        restart: always
        restart-delay: 2s
        logs: /var/log/frpc.log
        type: managed
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let scm = &manifest.daemons["frpc"].backend_specific[&Backend::Scm];

    assert!(scm.name.is_some());
    assert!(scm.command.is_some());
    assert!(scm.cwd.is_some());
    assert!(scm.env.is_some());
    assert!(scm.user.is_some());
    assert!(scm.restart.is_some());
    assert!(scm.restart_delay.is_some());
    assert!(scm.logs.is_some());
    assert!(scm.kind.is_some());
}

#[skuld::test]
fn explicit_null_is_rejected_for_every_overridable_field() {
    // Table-driven so no field can be added to `RawOverride` later without
    // a row here. The refusal comes from `no_null`'s scan, which is
    // schema-blind and so walks this subtree like any other; the wording
    // differs per field because `Slot` routes an override exactly like the
    // daemon it overrides — see `no_null`'s `Slot::value`.
    for field in OVERRIDABLE_FIELDS {
        let yaml = format!(
            "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        {field}: null
"
        );
        let err = parse(&yaml).unwrap_err();
        let expected = match field {
            "command" => "an explicit `null` is not a command",
            "env" => "an explicit `null` is not an environment block",
            "user" => "an explicit `null` is not a user",
            _ => "an explicit `null` is not a way to unset this field",
        };
        assert!(
            err.to_string().contains(expected),
            "field `{field}`: expected `{expected}`, got: {err}"
        );
    }
}

#[skuld::test]
fn a_null_command_element_in_an_override_is_rejected() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        command: [null]
";
    let err = parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("an explicit `null` is not a command element"),
        "{err}"
    );
}

#[skuld::test]
fn a_null_user_name_in_an_override_is_rejected() {
    // The override twin of `raw_tests.rs`'s base-position test: both fields
    // hold a `RawUser`, so one guard covers both, and this is the assertion
    // that it does.
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
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
fn null_env_values_in_an_override_are_rejected() {
    for spelling in ["null", "~", ""] {
        let yaml = format!(
            "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
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
fn an_empty_env_value_in_an_override_is_accepted() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        env:
          A: \"\"
";
    let manifest = parse(yaml).expect("an empty env value should parse");
    let env = manifest.daemons["frpc"].backend_specific[&Backend::Scm]
        .env
        .as_ref()
        .expect("env should be present");
    assert_eq!(env.get("A"), Some(&String::new()));
}

#[skuld::test]
fn null_env_keys_in_an_override_are_rejected() {
    let cases = [
        (
            "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        env:
          null: v
",
            "unquoted `null` key",
        ),
        (
            "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        env:
          ~: v
",
            "unquoted `~` key",
        ),
        (
            "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
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
fn a_quoted_null_env_key_is_accepted() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        env:
          \"null\": v
";
    let manifest = parse(yaml).expect("a quoted `null` key should parse");
    let env = manifest.daemons["frpc"].backend_specific[&Backend::Scm]
        .env
        .as_ref()
        .expect("env should be present");
    assert_eq!(env.get("null"), Some(&"v".to_string()));
}

#[skuld::test]
fn a_daemon_with_no_command_parses() {
    let yaml = "
daemons:
  frpc:
    name: frpc
    backend-specific:
      scm: {}
";
    let manifest = parse(yaml).expect("a manifest with no `command` anywhere should still parse");
    assert_eq!(manifest.daemons["frpc"].command, None);
    assert_eq!(manifest.daemons["frpc"].backend_specific[&Backend::Scm].command, None);
}

#[skuld::test]
fn unquoted_scalars_coerce_in_an_override_exactly_as_in_the_base() {
    // The base-position half of this property — nine scalar shapes, not
    // three — is `load_preserves_authored_scalar_text_alongside_interpolation`
    // in `resolve_tests.rs`. This is the override-position half: an
    // unquoted scalar still coerces to its authored text through `no_null`,
    // exactly as it does through the base's plain `String`/`Vec<String>`
    // deserializer.
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        name: 42
        command: [/bin/sleep, 30]
        env:
          PORT: 8080
";
    let manifest = parse(yaml).expect("unquoted scalars should coerce, not fail");
    let scm = &manifest.daemons["frpc"].backend_specific[&Backend::Scm];

    assert_eq!(scm.name.as_deref(), Some("42"));
    assert_eq!(
        scm.command.as_deref(),
        Some(&["/bin/sleep".to_string(), "30".to_string()][..])
    );
    let env: &BTreeMap<String, String> = scm.env.as_ref().unwrap();
    assert_eq!(env.get("PORT"), Some(&"8080".to_string()));
}

// Sanity: `RawOverride`'s `Default` is the empty override, used by the
// no-op-merge path in Task 3.
#[skuld::test]
fn raw_override_default_is_empty() {
    let ovr = RawOverride::default();
    assert_eq!(ovr.name, None);
    assert_eq!(ovr.command, None);
    assert_eq!(ovr.env, None);
}

// Merge ===============================================================================================================

#[skuld::test]
fn merge_replaces_scalar_fields() {
    let yaml = "
daemons:
  frpc:
    name: base-name
    command: [/bin/frpc]
    user: base-user
    backend-specific:
      scm:
        name: scm-name
        user: scm-user
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (merged, _supplied) = spec.merged_for(Backend::Scm);
    assert_eq!(merged.name.as_deref(), Some("scm-name"));
    assert_eq!(merged.user, Some(RawUser::Scalar("scm-user".to_string())));
}

#[skuld::test]
fn merge_replaces_the_whole_command_vector() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc, --base]
    backend-specific:
      scm:
        command: [/bin/frpc-scm]
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (merged, _supplied) = spec.merged_for(Backend::Scm);
    assert_eq!(merged.command.as_deref(), Some(&["/bin/frpc-scm".to_string()][..]));
}

#[skuld::test]
fn merge_unions_env_with_the_override_winning() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    env:
      A: \"1\"
      B: \"2\"
    backend-specific:
      scm:
        env:
          B: \"3\"
          C: \"4\"
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (merged, _supplied) = spec.merged_for(Backend::Scm);
    let expected: BTreeMap<String, String> = [("A", "1"), ("B", "3"), ("C", "4")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(merged.env, expected);
}

#[skuld::test]
fn merge_leaves_fields_the_override_does_not_mention() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    cwd: /opt/frpc
    backend-specific:
      scm:
        user: svc-frpc
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (merged, _supplied) = spec.merged_for(Backend::Scm);
    assert_eq!(merged.cwd.as_deref(), Some("/opt/frpc"));
}

#[skuld::test]
fn merge_of_a_backend_with_no_override_returns_the_base() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        user: svc-frpc
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (merged, supplied) = spec.merged_for(Backend::Systemd);
    assert_eq!(merged, spec.without_overrides());
    assert_eq!(supplied, Supplied::NONE);
}

#[skuld::test]
fn a_merged_spec_carries_no_overrides() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        user: svc-frpc
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (merged, _supplied) = spec.merged_for(Backend::Scm);
    assert!(merged.backend_specific.is_empty());
    assert!(spec.without_overrides().backend_specific.is_empty());
}

#[skuld::test]
fn supplied_names_exactly_the_fields_the_override_wrote() {
    let yaml = "
daemons:
  frpc:
    name: base-name
    command: [/bin/frpc]
    cwd: /opt/frpc
    env:
      A: \"1\"
    logs: /var/log/frpc.log
    type: simple
    backend-specific:
      scm:
        user: svc-frpc
        restart: always
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (_merged, supplied) = spec.merged_for(Backend::Scm);
    assert_eq!(
        supplied,
        Supplied {
            user: true,
            restart: true,
            ..Supplied::NONE
        }
    );
}

// The direct guard on the defect `Supplied` exists to prevent: without it,
// every field of this base spec — including `user: {id: 1000}`, the
// published-interface example from `README.md` — would be handed to
// `Backend::Systemd.error` as if `systemd:` had written it.
#[skuld::test]
fn supplied_is_none_for_a_backend_with_no_block() {
    let yaml = "
daemons:
  frpc:
    name: base-name
    command: [/bin/frpc]
    cwd: /opt/frpc
    env:
      A: \"1\"
    user:
      id: 1000
    restart: always
    restart-delay: 2s
    logs: /var/log/frpc.log
    type: simple
    backend-specific:
      scm:
        user: svc-frpc
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (_merged, supplied) = spec.merged_for(Backend::Systemd);
    assert_eq!(supplied, Supplied::NONE);
}

#[skuld::test]
fn an_empty_env_block_counts_as_supplied() {
    let yaml = "
daemons:
  frpc:
    command: [/bin/frpc]
    backend-specific:
      scm:
        env: {}
";
    let manifest = parse(yaml).expect("valid manifest should parse");
    let spec = &manifest.daemons["frpc"];
    let (_merged, supplied) = spec.merged_for(Backend::Scm);
    assert!(supplied.env);
}
