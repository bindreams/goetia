use cosca::error::{ChildFate, Error};

use super::*;

/// What the release leaves of an error is what cosca said of the failed spawn, so a caller still
/// reads from the variant alone whether the program may have started. A `MayHaveStarted` holding no
/// child, and an error of any other variant, come back whole.
#[skuld::test]
fn an_error_holding_no_child_comes_back_whole() {
    let source = || Box::new(Error::Io(std::io::Error::other("the attach failed")));
    let released = release_unreaped(Error::MayHaveStarted {
        fate: ChildFate::Running { id: None },
        wrapper_elevated: true,
        source: source(),
        unreaped: None,
    });
    match released {
        Error::MayHaveStarted {
            fate: ChildFate::Running { id: None },
            wrapper_elevated: true,
            source: kept,
            unreaped: None,
        } => assert_eq!(kept.to_string(), source().to_string()),
        other => panic!("changed by the release: {other:?}"),
    }

    let bare = release_unreaped(Error::Containment {
        detail: "forced".into(),
    });
    assert!(matches!(bare, Error::Containment { .. }), "{bare:?}");
}
