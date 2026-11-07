use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::*;
use crate::spec::test_support::abs;

// Fixtures ============================================================================================================

/// The program every fixture runs.
fn program() -> String {
    abs("bin/frpc").to_string_lossy().into_owned()
}

fn full_spec() -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "Frpc Tunnel".to_string(),
        command: vec![program(), "-c".to_string(), "frpc.toml".to_string()],
        cwd: Some(abs("opt/frpc")),
        env: BTreeMap::from([
            ("RUST_LOG".to_string(), "info".to_string()),
            ("URL".to_string(), "http://x/a%20b".to_string()),
        ]),
        user: User::Name("svc-frpc".to_string()),
        restart: Restart::Always,
        restart_delay: Some(Duration::new(2, 500_000_000)),
        logs: Some(abs("var/log/frpc.log")),
        kind: Kind::Managed,
    })
    .unwrap()
}

fn minimal_spec() -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "frpc".to_string(),
        command: vec![program()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    })
    .unwrap()
}

/// A spec with no `cwd` or `logs`, so the only host-dependent part of its wire bytes is
/// `command[0]` (`PathBuf`'s `Display`/`to_string_lossy` differs by separator). Used only by
/// `blob_wire_names_are_pinned`, which splices that program into the expected JSON.
fn golden_spec() -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "Frpc Tunnel".to_string(),
        command: vec![program(), "-c".to_string(), "frpc.toml".to_string()],
        cwd: None,
        env: BTreeMap::from([
            ("RUST_LOG".to_string(), "info".to_string()),
            ("URL".to_string(), "http://x/a%20b".to_string()),
        ]),
        user: User::Name("svc-frpc".to_string()),
        restart: Restart::Always,
        restart_delay: Some(Duration::new(2, 500_000_000)),
        logs: None,
        kind: Kind::Managed,
    })
    .unwrap()
}

fn base_wire_spec() -> WireSpec {
    WireSpec {
        id: "frpc".to_string(),
        name: "frpc".to_string(),
        command: vec![program()],
        cwd: Some(abs("opt/frpc").to_string_lossy().into_owned()),
        env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
        user: WireUser::Root,
        restart: WireRestart::Never,
        restart_delay: None,
        logs: Some(abs("var/log/frpc.log").to_string_lossy().into_owned()),
        kind: WireKind::Simple,
    }
}

fn encode_wire_envelope(envelope: &WireEnvelope) -> String {
    BASE64.encode(serde_json::to_vec(envelope).expect("WireEnvelope should always serialize"))
}

// Round trip ==========================================================================================================

#[skuld::test]
fn encode_decode_round_trips() {
    for spec in [full_spec(), minimal_spec()] {
        let blob = decode(&encode(&spec)).expect("an encoded valid spec should decode");
        assert_eq!(blob.schema, SCHEMA);
        assert_eq!(blob.version, crate::version());
        assert_eq!(blob.spec, spec);
    }

    let users = [
        User::Root,
        User::Name("svc-frpc".to_string()),
        User::Id(AccountId::Uid(1000)),
        User::Id(AccountId::Sid(
            "S-1-5-21-1004336348-1177238915-682003330-512".to_string(),
        )),
    ];
    for user in users {
        let mut spec = minimal_spec().into_fields();
        spec.user = user.clone();
        let spec = DaemonSpec::new(spec).unwrap();
        let blob = decode(&encode(&spec)).expect("an encoded valid spec should decode");
        assert_eq!(blob.spec.user, user);
    }
}

// Determinism and canonical form ======================================================================================

#[skuld::test]
fn encode_is_deterministic() {
    let spec = full_spec();
    assert_eq!(
        encode(&spec),
        encode(&spec),
        "encoding the same spec twice must be byte-identical"
    );

    let a = full_spec();
    let b = full_spec();
    assert_eq!(
        encode(&a),
        encode(&b),
        "two independently-constructed equal specs must give byte-identical output"
    );
}

#[skuld::test]
fn encode_is_key_sorted_at_every_level() {
    let encoded = encode(&full_spec());
    let bytes = BASE64.decode(&encoded).expect("encode's output must be valid base64");
    let text = String::from_utf8(bytes).expect("decoded bytes must be valid utf-8");
    let value: OrderedValue = serde_json::from_str(&text).expect("decoded text must be valid json");
    assert_object_keys_sorted(&value);
}

#[skuld::test]
fn blob_wire_names_are_pinned() {
    // Golden-bytes assertion: if this fails after an intentional wire
    // change, update the JSON below, and confirm the change was
    // intentional (a rename in `DaemonSpec` should not silently do this).
    let program = serde_json::to_string(&program()).unwrap();
    let expected = format!(
        r#"{{"schema":1,"spec":{{"command":[{program},"-c","frpc.toml"],"cwd":null,"env":{{"RUST_LOG":"info","URL":"http://x/a%20b"}},"id":"frpc","kind":"managed","logs":null,"name":"Frpc Tunnel","restart":"always","restart_delay":{{"nanos":500000000,"secs":2}},"user":{{"kind":"name","name":"svc-frpc"}}}},"version":"0.1.0"}}"#
    );
    assert_eq!(encode_with_version(&golden_spec(), "0.1.0"), BASE64.encode(expected));
}

// Schema and invariant re-validation ==================================================================================

#[skuld::test]
fn decode_rejects_unknown_schema() {
    let envelope = WireEnvelope {
        schema: 2,
        version: "0.1.0".to_string(),
        spec: base_wire_spec(),
    };
    let err = decode(&encode_wire_envelope(&envelope)).expect_err("an unknown schema must be rejected");
    let message = err.to_string();
    assert!(message.contains('2'), "message should name the found schema: {message}");
    assert!(
        message.contains(&SCHEMA.to_string()),
        "message should name the supported schema: {message}"
    );
}

#[skuld::test]
#[allow(clippy::type_complexity)]
fn decode_rejects_invariant_violations() {
    let cases: Vec<(&str, Box<dyn Fn(&mut WireSpec)>, &str)> = vec![
        (
            "empty command",
            Box::new(|s: &mut WireSpec| s.command = vec![]),
            "command",
        ),
        (
            "relative command[0]",
            Box::new(|s: &mut WireSpec| s.command = vec!["frpc".to_string()]),
            "command[0]",
        ),
        (
            "relative cwd",
            Box::new(|s: &mut WireSpec| s.cwd = Some("relative/dir".to_string())),
            "cwd",
        ),
        (
            "relative logs",
            Box::new(|s: &mut WireSpec| s.logs = Some("relative/logs".to_string())),
            "logs",
        ),
        (
            "control character in name",
            Box::new(|s: &mut WireSpec| s.name = "bad\u{1}name".to_string()),
            "name",
        ),
        (
            "control character in a command argument",
            Box::new(|s: &mut WireSpec| s.command = vec!["bad\u{1}arg".to_string()]),
            "command",
        ),
        (
            "invalid id pattern",
            Box::new(|s: &mut WireSpec| s.id = "bad/id".to_string()),
            "pattern",
        ),
        (
            "env key containing `=`",
            Box::new(|s: &mut WireSpec| {
                s.env.insert("KEY=1".to_string(), "v".to_string());
            }),
            "=",
        ),
        (
            "control character in an env value",
            Box::new(|s: &mut WireSpec| {
                s.env.insert("KEY".to_string(), "bad\u{1}value".to_string());
            }),
            "env",
        ),
        (
            "control character in user.name",
            Box::new(|s: &mut WireSpec| {
                s.user = WireUser::Name {
                    name: "bad\u{1}name".to_string(),
                }
            }),
            "user",
        ),
        (
            "control character in user.id (sid)",
            Box::new(|s: &mut WireSpec| {
                s.user = WireUser::Sid {
                    sid: "bad\u{1}sid".to_string(),
                }
            }),
            "user",
        ),
        (
            "restart_delay nanos out of range",
            Box::new(|s: &mut WireSpec| {
                s.restart_delay = Some(WireDuration {
                    secs: 1,
                    nanos: 2_000_000_000,
                })
            }),
            "restart_delay",
        ),
        (
            "restart_delay that would overflow `Duration::new`",
            Box::new(|s: &mut WireSpec| {
                s.restart_delay = Some(WireDuration {
                    secs: u64::MAX,
                    nanos: u32::MAX,
                })
            }),
            "restart_delay",
        ),
    ];

    for (label, mutate, expected_substring) in cases {
        let mut wire = base_wire_spec();
        mutate(&mut wire);
        let envelope = WireEnvelope {
            schema: SCHEMA,
            version: "0.1.0".to_string(),
            spec: wire,
        };
        let err = decode(&encode_wire_envelope(&envelope)).unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }), "case `{label}`: {err:?}");
        let message = err.to_string();
        assert!(
            message.contains(expected_substring),
            "case `{label}`: error `{message}` should mention `{expected_substring}`"
        );
    }
}

#[skuld::test]
fn decode_reports_a_field_defect_before_a_bad_restart_delay() {
    let mut wire = base_wire_spec();
    wire.name = "bad
name"
        .to_string();
    wire.restart_delay = Some(WireDuration {
        secs: 1,
        nanos: 2_000_000_000,
    });
    let envelope = WireEnvelope {
        schema: SCHEMA,
        version: "0.1.0".to_string(),
        spec: wire,
    };
    let message = decode(&encode_wire_envelope(&envelope)).unwrap_err().to_string();
    assert!(message.contains("field `name`"), "{message}");
}

#[skuld::test]
fn decode_rejects_an_empty_user_name() {
    // A tampered blob carrying an empty `user.name` must not decode into
    // a spec whose backend generators would emit an empty `User=`/
    // `UserName`/`lpServiceStartName` and silently run as the platform's
    // superuser.
    let mut wire = base_wire_spec();
    wire.user = WireUser::Name { name: String::new() };
    let envelope = WireEnvelope {
        schema: SCHEMA,
        version: "0.1.0".to_string(),
        spec: wire,
    };
    let err = decode(&encode_wire_envelope(&envelope)).expect_err("an empty user.name must be rejected");
    let message = err.to_string();
    assert!(message.contains("user.name"), "error should name the field: {message}");
    assert!(message.contains("empty"), "error should say why: {message}");
}

#[skuld::test]
fn decode_rejects_a_whitespace_only_user_name() {
    // Mirrors `decode_rejects_an_empty_user_name`: a tampered blob carrying
    // a whitespace-only `user.name` is exactly as dangerous as an empty
    // one (see `reject_blank`'s doc comment), so it must not decode
    // cleanly either.
    let mut wire = base_wire_spec();
    wire.user = WireUser::Name { name: "  ".to_string() };
    let envelope = WireEnvelope {
        schema: SCHEMA,
        version: "0.1.0".to_string(),
        spec: wire,
    };
    let err = decode(&encode_wire_envelope(&envelope)).expect_err("a whitespace-only user.name must be rejected");
    let message = err.to_string();
    assert!(message.contains("user.name"), "error should name the field: {message}");
    assert!(message.contains("empty"), "error should say why: {message}");
}

#[skuld::test]
fn decode_rejects_a_whitespace_only_user_id() {
    let mut wire = base_wire_spec();
    wire.user = WireUser::Sid { sid: "  ".to_string() };
    let envelope = WireEnvelope {
        schema: SCHEMA,
        version: "0.1.0".to_string(),
        spec: wire,
    };
    let err = decode(&encode_wire_envelope(&envelope)).expect_err("a whitespace-only user.id must be rejected");
    let message = err.to_string();
    assert!(message.contains("user.id"), "error should name the field: {message}");
    assert!(message.contains("empty"), "error should say why: {message}");
}

#[skuld::test]
fn decode_rejects_an_empty_command_executable() {
    let mut wire = base_wire_spec();
    wire.command = vec![String::new()];
    let envelope = WireEnvelope {
        schema: SCHEMA,
        version: "0.1.0".to_string(),
        spec: wire,
    };
    let err = decode(&encode_wire_envelope(&envelope)).expect_err("an empty command[0] must be rejected");
    let message = err.to_string();
    assert!(message.contains("command"), "error should name the field: {message}");
    assert!(message.contains("empty"), "error should say why: {message}");
}

#[skuld::test]
fn decode_rejects_malformed_base64() {
    let err = decode("not valid base64 !!!").unwrap_err();
    assert!(matches!(err, Error::Blob { .. }), "expected Error::Blob, got {err:?}");
    assert_eq!(err.to_string(), "blob: invalid base64");
    assert!(
        std::error::Error::source(&err).is_some(),
        "the decoder's error is the source"
    );
    assert!(
        err.report().to_string().starts_with("blob: invalid base64: "),
        "{}",
        err.report()
    );
}

#[skuld::test]
fn decode_rejects_malformed_json() {
    let encoded = BASE64.encode(b"not json");
    let err = decode(&encoded).unwrap_err();
    assert!(matches!(err, Error::Blob { .. }), "expected Error::Blob, got {err:?}");
    assert_eq!(err.to_string(), "blob: invalid blob JSON");
    assert!(
        err.report().to_string().starts_with("blob: invalid blob JSON: "),
        "{}",
        err.report()
    );
}

// Sortedness checker ==================================================================================================
//
// Deliberately does not use `serde_json::Value`: that type's default
// `Map` is `BTreeMap`-backed, so re-parsing into it would force-sort on
// read regardless of the actual byte order `encode` produced, proving
// nothing. `OrderedValue` instead collects each JSON object's members
// into a plain `Vec` in the exact order `serde_json`'s streaming parser
// visits them — which always matches the source text, independent of any
// crate's map-ordering feature flags — so this test inspects the real
// wire bytes.

enum OrderedValue {
    Object(Vec<(String, OrderedValue)>),
    Array(Vec<OrderedValue>),
    Other,
}

impl<'de> Deserialize<'de> for OrderedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OrderedValueVisitor)
    }
}

struct OrderedValueVisitor;

impl<'de> Visitor<'de> for OrderedValueVisitor {
    type Value = OrderedValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<OrderedValue, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        while let Some((k, v)) = map.next_entry::<String, OrderedValue>()? {
            entries.push((k, v));
        }
        Ok(OrderedValue::Object(entries))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<OrderedValue, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(v) = seq.next_element::<OrderedValue>()? {
            items.push(v);
        }
        Ok(OrderedValue::Array(items))
    }

    fn visit_bool<E>(self, _v: bool) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_i64<E>(self, _v: i64) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_u64<E>(self, _v: u64) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_f64<E>(self, _v: f64) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_str<E>(self, _v: &str) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_string<E>(self, _v: String) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_unit<E>(self) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_none<E>(self) -> Result<OrderedValue, E> {
        Ok(OrderedValue::Other)
    }
    fn visit_some<D>(self, deserializer: D) -> Result<OrderedValue, D::Error>
    where
        D: Deserializer<'de>,
    {
        OrderedValue::deserialize(deserializer)
    }
}

fn assert_object_keys_sorted(value: &OrderedValue) {
    match value {
        OrderedValue::Object(entries) => {
            let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(
                keys, sorted,
                "object keys must appear in sorted order in the wire bytes"
            );
            for (_, v) in entries {
                assert_object_keys_sorted(v);
            }
        }
        OrderedValue::Array(items) => {
            for item in items {
                assert_object_keys_sorted(item);
            }
        }
        OrderedValue::Other => {}
    }
}
