use std::sync::Arc;

use cosca::error::{ChildFate, Error};

use super::*;

// `release_unreaped`'s `Some` branch has no test: cosca's `Unreaped` has no public constructor, and
// a real spawn hands one back only when cosca's own teardown of a failed spawn could not end the
// child, which no test here can force.

/// The release changes nothing of an error that holds no child.
#[skuld::test]
fn an_error_holding_no_child_comes_back_whole() {
    let source = || Arc::new(Error::Io(std::io::Error::other("the attach failed")));
    let released = release_unreaped(Error::MayHaveStarted {
        fate: ChildFate::Running { id: None },
        wrapper_elevated: true,
        detail: None,
        source: source(),
        unreaped: None,
        cleanup: None,
    });
    match released {
        Error::MayHaveStarted {
            fate: ChildFate::Running { id: None },
            wrapper_elevated: true,
            detail: None,
            source: kept,
            unreaped: None,
            cleanup: None,
        } => assert_eq!(
            cosca::error::Report::new(&*kept).to_string(),
            cosca::error::Report::new(&*source()).to_string()
        ),
        other => panic!("changed by the release: {other:?}"),
    }

    let bare = release_unreaped(Error::Containment {
        detail: "forced".into(),
        source: None,
    });
    assert!(matches!(bare, Error::Containment { .. }), "{bare:?}");
}
