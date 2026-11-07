//! `goetia daemon list --json`, parsed, for the suites that run it as a real process.

use std::process::Output;

use goetia::manager::Installed;

/// The one document `--json` promises, plus the raw streams for a failure message.
pub struct ListJson {
    pub doc: serde_json::Value,
    pub code: i32,
    pub context: String,
}

impl ListJson {
    /// The ids of the entries under `key` (`daemons` or `undetermined`); `None` for an
    /// entry that names no id.
    pub fn ids(&self, key: &str) -> Vec<Option<String>> {
        self.doc[key]
            .as_array()
            .unwrap_or_else(|| panic!("`{key}` must always be present, as an array: {}", self.context))
            .iter()
            .map(|entry| match &entry[if key == "daemons" { "id" } else { "name" }] {
                serde_json::Value::Null => None,
                serde_json::Value::String(name) => Some(name.clone()),
                other => panic!("a name must be a string or null: {other}"),
            })
            .collect()
    }

    /// Every id in `ids` is listed as goetia's own and none is `undetermined`. Nothing is asserted
    /// about any other daemon on the host.
    pub fn assert_own(&self, ids: &[&str]) {
        let daemons = self.ids("daemons");
        let undetermined = self.ids("undetermined");
        for id in ids {
            let id = Some((*id).to_string());
            assert!(daemons.contains(&id), "{id:?} must be listed as ours: {}", self.context);
            assert!(
                !undetermined.contains(&id),
                "{id:?} was readable, so it must not be undetermined: {}",
                self.context
            );
        }
    }
}

/// The in-process counterpart of [`ListJson::assert_own`], for a `list()` result: every id in `ids`
/// is listed as goetia's own and no `Undetermined` entry names it. Nothing is asserted about any
/// other daemon on the host.
pub fn assert_own(installed: &[Installed], ids: &[&str]) {
    for id in ids {
        assert!(
            installed
                .iter()
                .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == *id)),
            "{id:?} must be listed as ours: {installed:?}"
        );
        assert!(
            !installed
                .iter()
                .any(|entry| matches!(entry, Installed::Undetermined { name: Some(name), .. } if name == id)),
            "{id:?} was readable, so it must not be undetermined: {installed:?}"
        );
    }
}

/// Runs `daemon list --json` through `run` and parses stdout. Panics if the process died without an
/// exit code, then unless the exit code agrees with the document: `errors` is empty (a real manager answered) and the code is `4` exactly when
/// `undetermined` is not, else `0`.
pub fn list_json(run: impl FnOnce(&[&str]) -> Output) -> ListJson {
    let output = run(&["daemon", "list", "--json"]);
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("`daemon list --json` was killed by a signal: {:?}", output.status));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");
    let doc = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {context}"));
    let listed = ListJson { doc, code, context };
    assert!(
        listed.doc["errors"].as_array().is_some_and(|errors| errors.is_empty()),
        "`errors` must be present and empty, since a real manager answers `list` on this platform: {}",
        listed.context
    );
    assert_eq!(
        listed.code,
        if listed.ids("undetermined").is_empty() { 0 } else { 4 },
        "exit `4` is exactly what an `undetermined` entry reports, and nothing else here reports it: {}",
        listed.context
    );
    listed
}
