//! Artifact-level and spec-level diff rendering.
//!
//! There are two diffs at different levels, and collapsing them into one
//! would defeat the reason either exists.
//!
//! [`artifact_diff`] compares generated artifact *text*. It has to work at
//! the text level because the hand-edits worth protecting are usually
//! things `DaemonSpec` cannot express at all — `MemoryMax=8G`,
//! `After=network-online.target`, `LimitNOFILE=65536`. Rendered as a
//! spec-level diff, such an edit shows as no change whatsoever, which is
//! the one outcome this diff exists to avoid.
//!
//! [`spec_diff`] instead compares two resolved specs, rendered through
//! [`render_yaml`] as canonical key-sorted YAML using the same key names
//! `goetia.yaml` uses — `restart-delay`, not `restart_delay`; `type`, not
//! `kind` — because it also backs `goetia daemon show`, and someone who
//! wrote YAML should be shown YAML they recognize.
//!
//! `DaemonSpec` deliberately does not derive `Serialize` — the metadata
//! blob embedded in each artifact has its own wire type and its own
//! naming rules. `render_yaml` builds its own `serde_yaml_ng::Value` tree
//! instead of piggybacking on a derive, on purpose.

use std::collections::BTreeMap;

use serde_yaml_ng::{Mapping, Value};
use similar::{ChangeTag, TextDiff};

use crate::spec::{AccountId, DaemonSpec, Kind, Restart, User};

/// Renders the artifact-level diff shown on `Conflict`/exit code 2: a
/// unified text diff between the artifact installed on disk (`was`) and
/// the artifact Goetia would generate (`now`). Identical input renders as
/// the empty string.
pub fn artifact_diff(was: &str, now: &str) -> String {
    unified_diff(was, now, "installed", "generated")
}

/// Renders the update diff shown when an unmodified artifact is about to
/// be regenerated from a changed spec: a unified diff between the two
/// specs' canonical YAML (see [`render_yaml`]). Identical input renders as
/// the empty string.
pub fn spec_diff(was: &DaemonSpec, now: &DaemonSpec) -> String {
    unified_diff(&render_yaml(was), &render_yaml(now), "before", "after")
}

/// Renders a resolved [`DaemonSpec`] as canonical, key-sorted YAML using
/// `goetia.yaml`'s own key names rather than `DaemonSpec`'s Rust field
/// names. Backs `goetia daemon show` and the `before`/`after` sides of
/// [`spec_diff`].
pub fn render_yaml(spec: &DaemonSpec) -> String {
    let mut fields: Vec<(&'static str, Value)> = vec![
        (
            "command",
            Value::Sequence(spec.command.iter().cloned().map(Value::from).collect()),
        ),
        ("env", Value::Mapping(env_value(&spec.env))),
        ("id", Value::from(spec.id.as_str().to_owned())),
        ("name", Value::from(spec.name.clone())),
        ("restart", Value::from(restart_key(spec.restart))),
        ("type", Value::from(kind_key(spec.kind))),
        ("user", user_value(&spec.user)),
    ];
    if let Some(cwd) = &spec.cwd {
        fields.push(("cwd", Value::from(cwd.display().to_string())));
    }
    if let Some(logs) = &spec.logs {
        fields.push(("logs", Value::from(logs.display().to_string())));
    }
    if let Some(delay) = spec.restart_delay {
        fields.push((
            "restart-delay",
            Value::from(humantime::format_duration(delay).to_string()),
        ));
    }

    // Sorted explicitly rather than relying on incidental container
    // iteration order, so the guarantee holds regardless of how `fields`
    // above is built.
    fields.sort_unstable_by_key(|(key, _)| *key);

    let mapping: Mapping = fields
        .into_iter()
        .map(|(key, value)| (Value::from(key), value))
        .collect();

    serde_yaml_ng::to_string(&Value::Mapping(mapping)).expect("a resolved DaemonSpec always renders to valid YAML")
}

// rendering helpers ===================================================================================================

/// `env`'s keys are already sorted (`DaemonSpec::env` is a `BTreeMap`), so
/// this preserves that order rather than re-deriving it.
fn env_value(env: &BTreeMap<String, String>) -> Mapping {
    env.iter()
        .map(|(k, v)| (Value::from(k.clone()), Value::from(v.clone())))
        .collect()
}

fn restart_key(restart: Restart) -> &'static str {
    match restart {
        Restart::Never => "never",
        Restart::OnFailure => "on-failure",
        Restart::Always => "always",
    }
}

fn kind_key(kind: Kind) -> &'static str {
    match kind {
        Kind::Simple => "simple",
        Kind::Managed => "managed",
    }
}

/// Mirrors the shapes `goetia.yaml` accepts for `user:`, so far as a
/// resolved [`User`] can distinguish them. `User::Name("root")` is
/// rendered as the struct form rather than the bare string `root`, because
/// the bare string `root` parses back as `User::Root`, not
/// `User::Name("root")`.
fn user_value(user: &User) -> Value {
    match user {
        User::Root => Value::from("root"),
        User::Name(name) if name == "root" => {
            let mapping: Mapping = [(Value::from("name"), Value::from("root"))].into_iter().collect();
            Value::Mapping(mapping)
        }
        User::Name(name) => Value::from(name.clone()),
        User::Id(AccountId::Uid(uid)) => {
            let mapping: Mapping = [(Value::from("id"), Value::from(*uid))].into_iter().collect();
            Value::Mapping(mapping)
        }
        User::Id(AccountId::Sid(sid)) => {
            let mapping: Mapping = [(Value::from("id"), Value::from(sid.clone()))].into_iter().collect();
            Value::Mapping(mapping)
        }
    }
}

// diffing =============================================================================================================

fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    if old == new {
        return String::new();
    }

    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- {old_label}\n+++ {new_label}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        // `Display` for `Change` already appends a trailing newline when
        // the source line itself was missing one.
        out.push_str(&format!("{sign}{change}"));
    }
    out
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod diff_tests;
