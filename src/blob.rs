//! The metadata blob: a base64, canonical-JSON encoding of a resolved
//! `DaemonSpec`, embedded in every generated artifact. Each backend's
//! generator writes it into a `Spec=`-style directive so that
//! `generate(extract(artifact)) == artifact` is checkable without a
//! separate state store — see the crate-level design notes.
//!
//! Two properties this module exists to get right:
//!
//! - **Canonical JSON.** Struct field declaration order is not a stable
//!   wire contract: a cosmetic reorder of `DaemonSpec`'s fields must not
//!   change the bytes every already-installed service compares against.
//!   `encode` therefore serializes through an explicit key-sorting pass
//!   (`canonicalize`) rather than relying on any particular JSON map's
//!   default iteration order — that would only be *incidentally* sorted,
//!   not canonically so — and a dedicated `WireSpec`/`WireEnvelope` pair
//!   with `#[serde(rename = ...)]` on every field keeps the wire names
//!   stable even when `DaemonSpec`'s Rust field names change.
//! - **Version, distinct from schema.** `SCHEMA` changes only when the
//!   envelope's *shape* changes; `version` records the crate version that
//!   generated the artifact, so a release that changes generator output
//!   by one byte reports the installed service as stale-and-regeneratable
//!   (see `decide::Outcome::Stale`) rather than as a user hand-edit.
//!
//! `decode` re-runs `DaemonSpec`'s injection-gate validation (Task 3's
//! `spec::resolve` checks, reused here rather than duplicated) against
//! the deserialized content: `DaemonSpec`'s fields are `pub`, and a
//! tampered or bit-rotted artifact can carry structurally valid JSON that
//! violates them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::spec::{self, AccountId, DaemonSpec, Id, Kind, Restart, User};

// Constants ============================================================================================================

/// The envelope shape `encode`/`decode` agree on. Bumped only when the
/// JSON structure itself changes, never for a change in generator output
/// — that is what `version` is for.
pub const SCHEMA: u32 = 1;

/// The literal value every generated artifact's marker carries, byte-exact
/// per the Global Constraints (`Marker`, `Schema`, `Version`, `Spec`).
pub const MARKER: &str = "goetia";

// Blob ==================================================================================================================

/// A decoded metadata blob: the schema it was written under, the crate
/// version that generated it, and the fully re-validated `DaemonSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub schema: u32,
    pub version: String,
    pub spec: DaemonSpec,
}

/// Encode `spec` as base64(canonical JSON), tagged with `SCHEMA` and the
/// running crate's version.
pub fn encode(spec: &DaemonSpec) -> String {
    encode_with_version(spec, crate::version())
}

/// `encode`, with the embedded version pinned explicitly rather than read
/// from `crate::version()`. Lets `blob_wire_names_are_pinned` assert
/// golden bytes that stay stable across `Cargo.toml` version bumps —
/// those aren't the wire-format drift that test guards against.
fn encode_with_version(spec: &DaemonSpec, version: &str) -> String {
    BASE64.encode(canonical_json(SCHEMA, version, spec))
}

/// Decode a blob produced by `encode`. Re-validates every injection-gate
/// and shape invariant `DaemonSpec` requires; a structurally valid but
/// tampered or bit-rotted blob is rejected rather than trusted.
pub fn decode(encoded: &str) -> Result<Blob, Error> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|source| Error::Blob(format!("invalid base64: {source}")))?;
    let envelope: WireEnvelope =
        serde_json::from_slice(&bytes).map_err(|source| Error::Blob(format!("invalid blob JSON: {source}")))?;

    if envelope.schema != SCHEMA {
        return Err(Error::Blob(format!(
            "unsupported blob schema {found} (this build of goetia understands schema {supported})",
            found = envelope.schema,
            supported = SCHEMA,
        )));
    }

    let spec = daemon_spec_from_wire(envelope.spec)?;

    Ok(Blob {
        schema: envelope.schema,
        version: envelope.version,
        spec,
    })
}

// Canonical JSON ========================================================================================================

/// Build the envelope and render it as compact, key-sorted-at-every-level
/// JSON text.
fn canonical_json(schema: u32, version: &str, spec: &DaemonSpec) -> String {
    let envelope = WireEnvelope {
        schema,
        version: version.to_string(),
        spec: WireSpec::from(spec),
    };
    let value = serde_json::to_value(&envelope).expect("WireEnvelope's Serialize impl cannot fail");
    serde_json::to_string(&canonicalize(value)).expect("a canonicalized Value's Serialize impl cannot fail")
}

/// Recursively rebuild every JSON object with its entries inserted in
/// sorted-by-key order. This is the "key-sorted writer": it does not rely
/// on `serde_json::Map`'s default `BTreeMap` backing (an incidental
/// property that a future `preserve_order`-enabling dependency could
/// silently take away) — it sorts explicitly, so the guarantee holds
/// regardless of that backing store.
fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().map(|(k, v)| (k, canonicalize(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

// Wire types =============================================================================================================
//
// The wire representation of `DaemonSpec`, kept deliberately separate
// from it: every field carries an explicit `#[serde(rename = ...)]`, so a
// future rename of a `DaemonSpec` Rust field cannot silently change every
// already-written artifact's bytes.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireEnvelope {
    #[serde(rename = "schema")]
    schema: u32,
    #[serde(rename = "spec")]
    spec: WireSpec,
    #[serde(rename = "version")]
    version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireSpec {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "name")]
    name: String,
    #[serde(rename = "command")]
    command: Vec<String>,
    #[serde(rename = "cwd")]
    cwd: Option<String>,
    #[serde(rename = "env")]
    env: BTreeMap<String, String>,
    #[serde(rename = "user")]
    user: WireUser,
    #[serde(rename = "restart")]
    restart: WireRestart,
    #[serde(rename = "restart_delay")]
    restart_delay: Option<WireDuration>,
    #[serde(rename = "logs")]
    logs: Option<String>,
    #[serde(rename = "kind")]
    kind: WireKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum WireUser {
    #[serde(rename = "root")]
    Root,
    #[serde(rename = "name")]
    Name {
        #[serde(rename = "name")]
        name: String,
    },
    #[serde(rename = "uid")]
    Uid {
        #[serde(rename = "uid")]
        uid: u32,
    },
    #[serde(rename = "sid")]
    Sid {
        #[serde(rename = "sid")]
        sid: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireRestart {
    #[serde(rename = "never")]
    Never,
    #[serde(rename = "on-failure")]
    OnFailure,
    #[serde(rename = "always")]
    Always,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireKind {
    #[serde(rename = "simple")]
    Simple,
    #[serde(rename = "managed")]
    Managed,
}

/// A `std::time::Duration`, split the way `Duration`'s own fields are, so
/// the round trip through JSON is lossless to the nanosecond rather than
/// lossy the way a single milliseconds-count field would be.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WireDuration {
    #[serde(rename = "nanos")]
    nanos: u32,
    #[serde(rename = "secs")]
    secs: u64,
}

// DaemonSpec <-> wire =====================================================================================================

impl From<&DaemonSpec> for WireSpec {
    fn from(spec: &DaemonSpec) -> Self {
        WireSpec {
            id: spec.id.as_str().to_string(),
            name: spec.name.clone(),
            command: spec.command.clone(),
            cwd: spec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            env: spec.env.clone(),
            user: WireUser::from(&spec.user),
            restart: WireRestart::from(spec.restart),
            restart_delay: spec.restart_delay.map(|d| WireDuration {
                nanos: d.subsec_nanos(),
                secs: d.as_secs(),
            }),
            logs: spec.logs.as_ref().map(|p| p.to_string_lossy().into_owned()),
            kind: WireKind::from(spec.kind),
        }
    }
}

impl From<&User> for WireUser {
    fn from(user: &User) -> Self {
        match user {
            User::Root => WireUser::Root,
            User::Name(name) => WireUser::Name { name: name.clone() },
            User::Id(AccountId::Uid(uid)) => WireUser::Uid { uid: *uid },
            User::Id(AccountId::Sid(sid)) => WireUser::Sid { sid: sid.clone() },
        }
    }
}

impl From<Restart> for WireRestart {
    fn from(restart: Restart) -> Self {
        match restart {
            Restart::Never => WireRestart::Never,
            Restart::OnFailure => WireRestart::OnFailure,
            Restart::Always => WireRestart::Always,
        }
    }
}

impl From<Kind> for WireKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Simple => WireKind::Simple,
            Kind::Managed => WireKind::Managed,
        }
    }
}

/// Rebuild a `DaemonSpec` from wire data, re-running every check
/// `spec::resolve` would have run on it — see the module doc comment for
/// why a structurally valid `WireSpec` cannot be trusted as-is.
fn daemon_spec_from_wire(wire: WireSpec) -> Result<DaemonSpec, Error> {
    let id = Id::try_from(wire.id)?;

    spec::reject_control_chars(&id, "name", &wire.name)?;

    spec::reject_empty_command(&id, &wire.command)?;
    for arg in &wire.command {
        spec::reject_control_chars(&id, "command", arg)?;
    }

    let cwd = match wire.cwd {
        Some(raw) => {
            spec::reject_control_chars(&id, "cwd", &raw)?;
            let path = PathBuf::from(raw);
            spec::reject_relative_path(&id, "cwd", &path)?;
            Some(path)
        }
        None => None,
    };

    let logs = match wire.logs {
        Some(raw) => {
            spec::reject_control_chars(&id, "logs", &raw)?;
            let path = PathBuf::from(raw);
            spec::reject_relative_path(&id, "logs", &path)?;
            Some(path)
        }
        None => None,
    };

    let mut env = BTreeMap::new();
    for (key, value) in wire.env {
        spec::reject_env_key_with_equals(&id, &key)?;
        spec::reject_control_chars(&id, "env key", &key)?;
        spec::reject_control_chars(&id, &format!("env[{key}]"), &value)?;
        env.insert(key, value);
    }

    let user = user_from_wire(&id, wire.user)?;

    let restart = match wire.restart {
        WireRestart::Never => Restart::Never,
        WireRestart::OnFailure => Restart::OnFailure,
        WireRestart::Always => Restart::Always,
    };

    let restart_delay = match wire.restart_delay {
        Some(d) => Some(duration_from_wire(&id, d)?),
        None => None,
    };

    let kind = match wire.kind {
        WireKind::Simple => Kind::Simple,
        WireKind::Managed => Kind::Managed,
    };

    Ok(DaemonSpec {
        id,
        name: wire.name,
        command: wire.command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
    })
}

fn user_from_wire(id: &Id, wire: WireUser) -> Result<User, Error> {
    Ok(match wire {
        WireUser::Root => User::Root,
        WireUser::Name { name } => {
            spec::reject_control_chars(id, "user.name", &name)?;
            User::Name(name)
        }
        WireUser::Uid { uid } => User::Id(AccountId::Uid(uid)),
        WireUser::Sid { sid } => {
            spec::reject_control_chars(id, "user.id", &sid)?;
            User::Id(AccountId::Sid(sid))
        }
    })
}

/// `Duration::new` panics if `nanos >= 1_000_000_000`; a hand-tampered
/// blob can carry exactly that, so this is checked rather than trusted.
/// This is a `DaemonSpec`-content invariant on an otherwise cleanly
/// decoded envelope, so it is `Error::Invalid` like every other check in
/// `daemon_spec_from_wire` — not `Error::Blob`, which is reserved for a
/// corrupt envelope (see `error.rs`).
fn duration_from_wire(id: &Id, wire: WireDuration) -> Result<Duration, Error> {
    if wire.nanos >= 1_000_000_000 {
        return Err(Error::Invalid {
            daemon: id.as_str().to_string(),
            message: format!(
                "restart_delay nanos {n} is not a valid sub-second fraction (must be < 1_000_000_000)",
                n = wire.nanos
            ),
        });
    }
    Ok(Duration::new(wire.secs, wire.nanos))
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod blob_tests;
