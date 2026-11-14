//! A task's `schedule`: five-field Vixie cron plus Jenkins's `H`.
//!
//! ```text
//! minute hour day-of-month month day-of-week
//! ```
//!
//! Each field is a comma list of terms: `*`, `n`, `a-b`, `*/n`, `a-b/n`, `H`, `H(a-b)`, `H/n` or
//! `H(a-b)/n`. Ranges are minute 0-59, hour 0-23, day-of-month 1-31, month 1-12 and day-of-week
//! 0-7, where 7 is stored as 0. Names, `@` macros, `L`, `W`, `#`, `?` and any other field count
//! are refused.
//!
//! # `H`
//!
//! `H` picks one value per daemon so that tasks spread out instead of all firing at minute 0. The
//! pick is a pure function of the host name, the daemon id and the field's position, so it is the
//! same while the host name stays the same and differs across hosts and daemons. A renamed host
//! resolves `H` anew, which `diff` shows (the `# resolves to:` comment) and `install` applies.
//!
//! - Bare `H` ranges over minute 0-59, hour 0-23, day-of-month 1-28, month 1-12, day-of-week 0-6.
//! - `H(a-b)` is `a + hash mod (b-a+1)`; `a` and `b` lie within the field's full range
//!   (day-of-week 0-6).
//! - `H/n` is `H(lo-hi)/n` over bare `H`'s range.
//! - `H(a-b)/n` starts at `a + hash mod min(n, b-a+1)` and steps by `n` up to `b`, so it is never
//!   empty.
//!
//! The hash is the first 8 bytes, big-endian, of `SHA-256(hostname 0x00 id 0x00 position)`, with
//! the position a single byte, minute = 0 through day-of-week = 4.
//!
//! # Day rule
//!
//! As in Vixie cron, when day-of-month or day-of-week starts with `*` the two must both match;
//! otherwise either may. A field starting with `H` is restricted.

use std::collections::BTreeSet;

use sha2::{Digest as _, Sha256};

use super::Id;
use super::resolve::invalid;
use crate::error::Error;

/// What `H` hashes from. The host name is read once per resolution by the caller
/// (`this_hostname`).
#[derive(Debug, Clone, Copy)]
pub struct HashSeed<'a> {
    pub hostname: &'a str,
    pub id: &'a Id,
}

/// The host name that seeds `H`, read lossily as UTF-8.
pub fn this_hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// One resolved field: the values it matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronField {
    /// Day-of-week 7 is stored as 0.
    pub values: BTreeSet<u8>,
    /// The field's text does not start with `*`.
    pub restricted: bool,
}

/// How day-of-month and day-of-week combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayRule {
    /// Both must match.
    And,
    /// Either may match.
    Or,
}

/// A parsed schedule: its text and the five resolved fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    text: String,
    // Boxed: keeps `DaemonSpec`'s variants close in size.
    resolved: Box<Resolved>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    fields: [CronField; FIELD_COUNT],
    /// The hash each field with an `H` term was resolved by; none for an `H`-free field.
    hashes: [Option<u64>; FIELD_COUNT],
}

impl Schedule {
    /// Parses `text`, resolving each `H` for `seed`. Refusals are [`Error::Invalid`] for
    /// `seed.id`, naming the field.
    pub fn parse(text: &str, seed: &HashSeed) -> Result<Schedule, Error> {
        let structure = structure(text).map_err(|message| invalid(seed.id, &message))?;
        let hashes = std::array::from_fn(|position| structure[position].has_hash().then(|| field_hash(seed, position)));
        let fields = std::array::from_fn(|position| structure[position].resolve(hashes[position].unwrap_or(0)));
        Ok(Schedule {
            text: text.to_owned(),
            resolved: Box::new(Resolved { fields, hashes }),
        })
    }

    /// The schedule as authored.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Minute, hour, day-of-month, month, day-of-week.
    pub fn fields(&self) -> &[CronField; FIELD_COUNT] {
        &self.resolved.fields
    }

    /// The hash behind each field, in the order of [`Schedule::fields`]; none for an `H`-free field.
    pub(crate) fn hashes(&self) -> &[Option<u64>; FIELD_COUNT] {
        &self.resolved.hashes
    }

    pub fn day_rule(&self) -> DayRule {
        let fields = self.fields();
        if fields[DAY_OF_MONTH].restricted && fields[DAY_OF_WEEK].restricted {
            DayRule::Or
        } else {
            DayRule::And
        }
    }

    /// Rebuilds a schedule from `text` and each field with its stored hash, needing no host name.
    ///
    /// Each field is recomputed from the text's terms and its hash (none for an `H`-free field)
    /// and must equal the stored one exactly. A field with `H` and no hash, or a hash on an
    /// `H`-free field, is refused. Every refusal is [`Error::Invalid`] for `id`, naming the field.
    pub(crate) fn from_parts(
        id: &Id,
        text: String,
        stored: [(CronField, Option<u64>); FIELD_COUNT],
    ) -> Result<Schedule, Error> {
        let structure = structure(&text).map_err(|message| invalid(id, &message))?;
        for ((kind, parsed), (field, hash)) in KINDS.iter().zip(&structure).zip(&stored) {
            check(parsed, field, *hash)
                .map_err(|reason| invalid(id, &format!("schedule {} field: {reason}", kind.name)))?;
        }
        let fields = stored.each_ref().map(|(field, _)| field.clone());
        let hashes = stored.map(|(_, hash)| hash);
        Ok(Schedule {
            text,
            resolved: Box::new(Resolved { fields, hashes }),
        })
    }
}

// Fields ==============================================================================================================

const FIELD_COUNT: usize = 5;
const DAY_OF_MONTH: usize = 2;
const DAY_OF_WEEK: usize = 4;

struct FieldKind {
    name: &'static str,
    min: u8,
    /// The largest stored value.
    max: u8,
    /// The largest value a plain term may name: 7 for day-of-week, stored as 0.
    input_max: u8,
    /// The top of the range bare `H` hashes over.
    bare_hi: u8,
}

const KINDS: [FieldKind; FIELD_COUNT] = [
    FieldKind {
        name: "minute",
        min: 0,
        max: 59,
        input_max: 59,
        bare_hi: 59,
    },
    FieldKind {
        name: "hour",
        min: 0,
        max: 23,
        input_max: 23,
        bare_hi: 23,
    },
    FieldKind {
        name: "day-of-month",
        min: 1,
        max: 31,
        input_max: 31,
        bare_hi: 28,
    },
    FieldKind {
        name: "month",
        min: 1,
        max: 12,
        input_max: 12,
        bare_hi: 12,
    },
    FieldKind {
        name: "day-of-week",
        min: 0,
        max: 6,
        input_max: 7,
        bare_hi: 6,
    },
];

// The structural pass -------------------------------------------------------------------------------------------------

/// One comma-separated term, before any seed.
#[derive(Debug)]
enum Term {
    Fixed(BTreeSet<u8>),
    Hashed { lo: u8, hi: u8, step: Option<u8> },
}

impl Term {
    /// The values `hash` picks from this term.
    fn values(&self, hash: u64) -> BTreeSet<u8> {
        match self {
            Term::Fixed(values) => values.clone(),
            Term::Hashed { lo, hi, step: None } => {
                let len = u64::from(hi - lo) + 1;
                // `hash % len < len <= 60`, so the cast cannot truncate.
                BTreeSet::from([lo + (hash % len) as u8])
            }
            Term::Hashed {
                lo,
                hi,
                step: Some(step),
            } => {
                let len = u64::from(hi - lo) + 1;
                let offset = (hash % len.min(u64::from(*step))) as u8;
                (lo + offset..=*hi).step_by(usize::from(*step)).collect()
            }
        }
    }
}

struct ParsedField {
    terms: Vec<Term>,
    restricted: bool,
}

impl ParsedField {
    fn has_hash(&self) -> bool {
        self.terms.iter().any(|term| matches!(term, Term::Hashed { .. }))
    }

    /// The field `hash` resolves this one to.
    fn resolve(&self, hash: u64) -> CronField {
        CronField {
            values: self.terms.iter().flat_map(|term| term.values(hash)).collect(),
            restricted: self.restricted,
        }
    }
}

/// Splits `text` into its five fields and reads each field's terms. Messages name the field.
fn structure(text: &str) -> Result<Vec<ParsedField>, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() != FIELD_COUNT {
        return Err(format!(
            "schedule `{text}` has {} fields; expected 5 (minute hour day-of-month month day-of-week)",
            words.len()
        ));
    }
    let mut parsed = Vec::with_capacity(FIELD_COUNT);
    for (kind, word) in KINDS.iter().zip(words) {
        let terms = word
            .split(',')
            .map(|term| {
                if term.is_empty() {
                    return Err(format!("schedule {} field: an empty term", kind.name));
                }
                parse_term(kind, term).map_err(|reason| format!("schedule {} field: `{term}` {reason}", kind.name))
            })
            .collect::<Result<Vec<_>, _>>()?;
        parsed.push(ParsedField {
            terms,
            restricted: !word.starts_with('*'),
        });
    }
    Ok(parsed)
}

const GRAMMAR: &str = "a term is `*`, `n`, `a-b`, `*/n`, `a-b/n`, `H`, `H(a-b)`, `H/n` or `H(a-b)/n`";

fn unsupported() -> String {
    format!("is not supported; {GRAMMAR}")
}

/// Digits only. A number too large for `u8` saturates to 255, which every value range refuses and
/// which, as a step, gives the first value only. That matches Vixie cron for any step that fits an
/// int, and is the same answer for a larger one.
fn digits(text: &str) -> Option<u8> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().unwrap_or(u8::MAX))
}

/// `a-b`, both numbers.
fn bounds(text: &str) -> Option<(u8, u8)> {
    let (a, b) = text.split_once('-')?;
    Some((digits(a)?, digits(b)?))
}

fn parse_term(kind: &FieldKind, term: &str) -> Result<Term, String> {
    let (base, step) = match term.split_once('/') {
        Some((base, step)) => (base, Some(step)),
        None => (term, None),
    };
    let step = match step.map(digits) {
        None => None,
        Some(None) => return Err(unsupported()),
        Some(Some(0)) => return Err("has a step of 0".to_owned()),
        Some(Some(step)) => Some(step),
    };

    if let Some(rest) = base.strip_prefix('H') {
        return match rest {
            "" => Ok(Term::Hashed {
                lo: kind.min,
                hi: kind.bare_hi,
                step,
            }),
            _ => {
                let inner = rest
                    .strip_prefix('(')
                    .and_then(|r| r.strip_suffix(')'))
                    .ok_or_else(unsupported)?;
                let (lo, hi) = bounds(inner).ok_or_else(unsupported)?;
                check_bounds(kind, lo, hi, kind.max)?;
                Ok(Term::Hashed { lo, hi, step })
            }
        };
    }

    let (lo, hi) = if base == "*" {
        (kind.min, kind.input_max)
    } else if let Some(n) = digits(base) {
        // A step on a single number is not a term.
        if step.is_some() {
            return Err(unsupported());
        }
        (n, n)
    } else {
        bounds(base).ok_or_else(unsupported)?
    };
    check_bounds(kind, lo, hi, kind.input_max)?;
    // Day-of-week 7 is Sunday, stored as 0.
    let wrap = |value: u8| value % (kind.max + 1);
    Ok(Term::Fixed(
        (lo..=hi).step_by(usize::from(step.unwrap_or(1))).map(wrap).collect(),
    ))
}

fn check_bounds(kind: &FieldKind, lo: u8, hi: u8, max: u8) -> Result<(), String> {
    if lo < kind.min || hi > max {
        return Err(format!("is out of range {}-{max}", kind.min));
    }
    if lo > hi {
        return Err("starts after it ends".to_owned());
    }
    Ok(())
}

// The resolving pass --------------------------------------------------------------------------------------------------

fn field_hash(seed: &HashSeed, position: usize) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.hostname.as_bytes());
    hasher.update([0]);
    hasher.update(seed.id.as_str().as_bytes());
    hasher.update([0]);
    hasher.update([u8::try_from(position).expect("a field position is below 5")]);
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("a SHA-256 digest is 32 bytes"))
}

// Decoding ------------------------------------------------------------------------------------------------------------

/// Whether `field` is exactly what `parsed`'s terms give for `hash`. The reason does not name the
/// field.
fn check(parsed: &ParsedField, field: &CronField, hash: Option<u64>) -> Result<(), String> {
    match (parsed.has_hash(), hash) {
        (true, None) => return Err("has an `H` term but no hash".to_owned()),
        (false, Some(_)) => return Err("has a hash but no `H` term".to_owned()),
        _ => {}
    }
    let expected = parsed.resolve(hash.unwrap_or(0));
    if *field != expected {
        return Err(format!(
            "holds {:?} (restricted: {}), but its text and hash give {:?} (restricted: {})",
            field.values, field.restricted, expected.values, expected.restricted
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod schedule_tests;
