use super::{AccountId, RawUser, User};
use crate::spec::resolve::resolve_user;

fn parse(yaml: &str) -> Result<RawUser, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(yaml)
}

/// Parse and resolve, the way `spec::load` does.
fn parse_resolved(yaml: &str) -> User {
    resolve_user(Some(parse(yaml).expect("fixture should parse")))
}

#[skuld::test]
fn user_bare_string_is_carried_through_verbatim() {
    // Deserialization records the syntax and nothing else: the `root`
    // reserved word is `resolve_user`'s to apply, after interpolation has
    // had its turn at the text.
    assert_eq!(parse("root").unwrap(), RawUser::Scalar("root".to_string()));
    assert_eq!(parse("bindreams").unwrap(), RawUser::Scalar("bindreams".to_string()));
}

#[skuld::test]
fn user_bare_string_root_is_reserved_token() {
    assert_eq!(parse_resolved("root"), User::Root);
}

#[skuld::test]
fn user_bare_string_is_name() {
    assert_eq!(parse_resolved("bindreams"), User::Name("bindreams".to_string()));
}

#[skuld::test]
fn user_struct_name_has_no_reserved_words() {
    // `{name: root}` is the escape hatch for an account genuinely called
    // `root`, on either side of the raw/resolved boundary.
    assert_eq!(parse("name: root").unwrap(), RawUser::Name("root".to_string()));
    assert_eq!(parse_resolved("name: root"), User::Name("root".to_string()));
}

#[skuld::test]
fn user_struct_id_uid() {
    assert_eq!(parse("id: 1001").unwrap(), RawUser::Id(AccountId::Uid(1001)));
    assert_eq!(parse_resolved("id: 1001"), User::Id(AccountId::Uid(1001)));
}

#[skuld::test]
fn user_struct_id_sid() {
    let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    assert_eq!(
        parse(&format!("id: \"{sid}\"")).unwrap(),
        RawUser::Id(AccountId::Sid(sid.to_string()))
    );
}

#[skuld::test]
fn an_absent_user_resolves_to_root() {
    assert_eq!(resolve_user(None), User::Root);
}

#[skuld::test]
fn user_struct_rejects_both_fields() {
    let err = parse("name: bindreams\nid: 1001").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("name") && msg.contains("id"),
        "error should name both fields: {msg}"
    );
}

#[skuld::test]
fn user_struct_rejects_neither() {
    let err = parse("{}").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("name") && msg.contains("id"),
        "error should name both accepted fields: {msg}"
    );
}

#[skuld::test]
fn user_struct_rejects_unknown_field() {
    let err = parse("bogus: true").unwrap_err();
    assert!(err.to_string().contains("bogus"));
}
