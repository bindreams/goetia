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
