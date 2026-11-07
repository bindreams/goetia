//! `goetia daemon list --json`, parsed, for the suites that run it as an unprivileged user.

use std::process::Output;

/// The one document `--json` promises, plus the raw streams for a failure message.
pub struct ListJson {
    pub doc: serde_json::Value,
    pub code: Option<i32>,
    pub context: String,
}

impl ListJson {
    /// The ids of the entries under `key` (`daemons`, `undetermined`, `errors`); `None` for an
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
}

/// Runs `daemon list --json` through `run` (a suite's unprivileged runner) and parses stdout.
pub fn list_json(run: impl FnOnce(&[&str]) -> Output) -> ListJson {
    let output = run(&["daemon", "list", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");
    let doc = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {context}"));
    ListJson {
        doc,
        code: output.status.code(),
        context,
    }
}
