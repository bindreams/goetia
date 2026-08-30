use super::{AccountId, User};

fn parse(yaml: &str) -> Result<User, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(yaml)
}

#[skuld::test]
fn user_bare_string_root_is_reserved_token() {
    assert_eq!(parse("root").unwrap(), User::Root);
}

#[skuld::test]
fn user_bare_string_is_name() {
    assert_eq!(parse("bindreams").unwrap(), User::Name("bindreams".to_string()));
}

#[skuld::test]
fn user_struct_name_has_no_reserved_words() {
    assert_eq!(parse("name: root").unwrap(), User::Name("root".to_string()));
}

#[skuld::test]
fn user_struct_id_uid() {
    assert_eq!(parse("id: 1001").unwrap(), User::Id(AccountId::Uid(1001)));
}

#[skuld::test]
fn user_struct_id_sid() {
    let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    assert_eq!(
        parse(&format!("id: \"{sid}\"")).unwrap(),
        User::Id(AccountId::Sid(sid.to_string()))
    );
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
