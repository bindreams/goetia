use std::collections::BTreeSet;

use super::{CronField, DayRule, HashSeed, Schedule};
use crate::error::Error;
use crate::spec::Id;

const HOST: &str = "build-host";
const FIELD_NAMES: [&str; 5] = ["minute", "hour", "day-of-month", "month", "day-of-week"];

fn id(text: &str) -> Id {
    Id::try_from(text).unwrap()
}

fn parse_for(daemon: &str, host: &str, text: &str) -> Result<Schedule, Error> {
    let id = id(daemon);
    Schedule::parse(
        text,
        &HashSeed {
            hostname: host,
            id: &id,
        },
    )
}

fn parse(text: &str) -> Schedule {
    parse_for("nightly", HOST, text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn refusal(error: Error) -> (String, String) {
    match error {
        Error::Invalid { daemon, message, .. } => (daemon, message),
        other => panic!("expected Error::Invalid, got {other:?}"),
    }
}

/// `term` in field `index`, `*` in the others.
fn line_with(index: usize, term: &str) -> String {
    let mut fields = ["*"; 5];
    fields[index] = term;
    fields.join(" ")
}

fn set(values: &[u8]) -> BTreeSet<u8> {
    values.iter().copied().collect()
}

fn range(lo: u8, hi: u8) -> BTreeSet<u8> {
    (lo..=hi).collect()
}

#[skuld::test]
fn parses_every_vixie_form() {
    // (field index, term, expected values)
    let table: Vec<(usize, &str, BTreeSet<u8>)> = vec![
        (0, "*", range(0, 59)),
        (0, "5", set(&[5])),
        (0, "1-5", range(1, 5)),
        (0, "*/15", set(&[0, 15, 30, 45])),
        (0, "10-50/20", set(&[10, 30, 50])),
        (0, "1,5,10-12", set(&[1, 5, 10, 11, 12])),
        (0, "5,5", set(&[5])),
        (0, "0-59/1", range(0, 59)),
        (0, "*/60", set(&[0])),
        (0, "*/100", set(&[0])),
        (0, "*/300", set(&[0])),
        (0, "*/99999999999", set(&[0])),
        (1, "*/25", set(&[0])),
        (4, "*/8", set(&[0])),
        (1, "*", range(0, 23)),
        (1, "23", set(&[23])),
        (1, "*/6", set(&[0, 6, 12, 18])),
        (1, "0,12", set(&[0, 12])),
        (2, "*", range(1, 31)),
        (2, "1-31/10", set(&[1, 11, 21, 31])),
        (2, "31", set(&[31])),
        (3, "*", range(1, 12)),
        (3, "*/4", set(&[1, 5, 9])),
        (3, "12", set(&[12])),
        (4, "*", range(0, 6)),
        (4, "7", set(&[0])),
        (4, "0-7", range(0, 6)),
        (4, "5-7", set(&[5, 6, 0])),
        (4, "*/2", set(&[0, 2, 4, 6])),
        (4, "1,7", set(&[0, 1])),
        (4, "1-5", range(1, 5)),
    ];
    for (index, term, expected) in table {
        let line = line_with(index, term);
        let schedule = parse(&line);
        assert_eq!(schedule.text(), line);
        assert_eq!(
            schedule.fields()[index].values,
            expected,
            "{} field of {line:?}",
            FIELD_NAMES[index]
        );
        assert_eq!(schedule.fields()[index].restricted, !term.starts_with('*'), "{line:?}");
    }
}

#[skuld::test]
fn separators_are_runs_of_blanks() {
    let schedule = parse("  5 \t 6   *  * *  ");
    assert_eq!(schedule.fields()[0].values, set(&[5]));
    assert_eq!(schedule.fields()[1].values, set(&[6]));
}

#[skuld::test]
fn refuses_each_unsupported_form_naming_its_field() {
    // (field index, term)
    let table: Vec<(usize, &str)> = vec![
        // out of range
        (0, "60"),
        (0, "5-60"),
        (0, "999"),
        (0, "99999999999999999999"),
        (1, "24"),
        (2, "0"),
        (2, "32"),
        (3, "0"),
        (3, "13"),
        (4, "8"),
        (4, "0-8"),
        // a > b
        (0, "5-1"),
        (4, "6-0"),
        // n = 0
        (0, "*/0"),
        (0, "1-5/0"),
        (0, "H/0"),
        (0, "H(1-5)/0"),
        // empty list terms
        (0, "1,,2"),
        (0, "1,"),
        (0, ",1"),
        // names, macros, L, W, #, ?
        (3, "jan"),
        (3, "JAN"),
        (3, "jan-mar"),
        (4, "mon"),
        (4, "mon-fri"),
        (0, "@daily"),
        (2, "L"),
        (2, "15W"),
        (2, "LW"),
        (4, "5#2"),
        (4, "5L"),
        (2, "?"),
        // malformed
        (0, "5/2"),
        (0, "*/"),
        (0, "*/x"),
        (0, "*/2/3"),
        (0, "1-5-7"),
        (0, "-1"),
        (0, "+5"),
        (0, "1-"),
        (0, "-5"),
        (0, "**"),
        (0, "*-5"),
        // H
        (0, "H(10-5)"),
        (0, "H(0-60)"),
        (0, "H(60-60)"),
        (2, "H(0-5)"),
        (2, "H(1-32)"),
        (3, "H(1-13)"),
        (4, "H(0-7)"),
        (4, "H(7-7)"),
        (0, "H(5)"),
        (0, "H5"),
        (0, "H-5"),
        (0, "H()"),
        (0, "H(1-5"),
        (0, "H1-5)"),
        (0, "H(1-5)/"),
        (0, "H(a-b)"),
        (0, "h"),
        (0, "HH"),
        (0, "H/x"),
        (0, "H(1-5)5"),
    ];
    for (index, term) in table {
        let line = line_with(index, term);
        let (daemon, message) = refusal(parse_for("nightly", HOST, &line).expect_err(&line));
        assert_eq!(daemon, "nightly", "{line:?}");
        assert!(
            message.contains(FIELD_NAMES[index]),
            "{line:?} must name its field: {message}"
        );
        assert!(message.contains("schedule"), "{line:?}: {message}");
    }
}

#[skuld::test]
fn refuses_a_wrong_number_of_fields() {
    for line in [
        "",
        "   ",
        "*",
        "* * * *",
        "* * * * * *",
        "* * * * * * *",
        "@daily",
        "@reboot * * * *",
    ] {
        let (daemon, message) = refusal(parse_for("nightly", HOST, line).expect_err(line));
        assert_eq!(daemon, "nightly");
        assert!(message.contains("schedule"), "{line:?}: {message}");
    }
}

#[skuld::test]
fn h_matches_the_golden_vectors() {
    let minute = |daemon: &str, text: &str| parse_for(daemon, HOST, text).unwrap().fields()[0].values.clone();
    assert_eq!(minute("nightly", "H * * * *"), set(&[13]));
    assert_eq!(minute("backup", "H * * * *"), set(&[23]));
    assert_eq!(minute("nightly", "H/15 * * * *"), set(&[13, 28, 43, 58]));
    // A step past the field's range leaves one value, the pick.
    assert_eq!(minute("nightly", "H/61 * * * *"), set(&[13]));
    assert_eq!(parse("* H * * *").fields()[1].values, set(&[2]));
}

#[skuld::test]
fn h_depends_on_hostname_id_and_position() {
    let minutes = |daemon: &str, host: &str| parse_for(daemon, host, "H * * * *").unwrap().fields()[0].values.clone();
    let by_id: BTreeSet<BTreeSet<u8>> = (0..50).map(|i| minutes(&format!("job-{i}"), HOST)).collect();
    let by_host: BTreeSet<BTreeSet<u8>> = (0..50).map(|i| minutes("nightly", &format!("host-{i}"))).collect();
    assert!(by_id.len() > 20, "50 ids gave only {} distinct minutes", by_id.len());
    assert!(
        by_host.len() > 20,
        "50 hostnames gave only {} distinct minutes",
        by_host.len()
    );

    // The position is part of the hash: `H H` is not one value repeated in two fields.
    let differs = (0..50).any(|i| {
        let s = parse_for(&format!("job-{i}"), HOST, "H H * * *").unwrap();
        let [minute, hour, ..] = s.fields();
        minute.values != hour.values
    });
    assert!(differs);
}

#[skuld::test]
fn bare_h_stays_within_jenkins_ranges() {
    let bounds: [(u8, u8); 5] = [(0, 59), (0, 23), (1, 28), (1, 12), (0, 6)];
    for i in 0..300 {
        let s = parse_for(&format!("job-{i}"), "some-host", "H H H H H").unwrap();
        for (field, (lo, hi)) in s.fields().iter().zip(bounds) {
            assert_eq!(field.values.len(), 1);
            assert!(field.restricted);
            assert!(field.values.iter().all(|v| (lo..=hi).contains(v)), "job-{i}: {field:?}");
        }
    }
}

#[skuld::test]
fn h_forms_stay_in_range() {
    for i in 0..300 {
        let daemon = format!("job-{i}");
        let at = |text: &str, index: usize| {
            parse_for(&daemon, "some-host", text).unwrap().fields()[index]
                .values
                .clone()
        };

        let ranged = at("H(10-20) * * * *", 0);
        assert_eq!(ranged.len(), 1);
        assert!(ranged.iter().all(|v| (10..=20).contains(v)), "{daemon}: {ranged:?}");

        let stepped = at("H(0-5)/10 * * * *", 0);
        assert_eq!(stepped.len(), 1, "{daemon}: {stepped:?}");
        assert!(stepped.iter().all(|v| (0..=5).contains(v)), "{daemon}: {stepped:?}");

        let narrow = at("H(10-12)/20 * * * *", 0);
        assert_eq!(narrow.len(), 1, "{daemon}: {narrow:?}");
        assert!(narrow.iter().all(|v| (10..=12).contains(v)), "{daemon}: {narrow:?}");

        let chain = at("H(0-59)/20 * * * *", 0);
        let first = *chain.first().unwrap();
        assert!(first < 20);
        assert_eq!(chain, set(&[first, first + 20, first + 40]));

        let uneven = at("H(1-10)/4 * * * *", 0);
        let first = *uneven.first().unwrap();
        assert!((1..=4).contains(&first), "{daemon}: {uneven:?}");
        assert_eq!(uneven, (first..=10).step_by(4).collect::<BTreeSet<u8>>());

        let day = at("* * H(1-31) * *", 2);
        assert!(
            day.len() == 1 && day.iter().all(|v| (1..=31).contains(v)),
            "{daemon}: {day:?}"
        );

        let week = at("* * * * H(0-6)", 4);
        assert!(
            week.len() == 1 && week.iter().all(|v| (0..=6).contains(v)),
            "{daemon}: {week:?}"
        );

        let every_other = at("H/2 * * * *", 0);
        assert_eq!(every_other.len(), 30, "{daemon}: {every_other:?}");

        let hour_steps = at("* H/5 * * *", 1);
        let first = *hour_steps.first().unwrap();
        assert!(
            first < 5 && hour_steps.iter().all(|v| *v <= 23 && (v - first) % 5 == 0),
            "{daemon}: {hour_steps:?}"
        );
    }
}

#[skuld::test]
fn an_h_term_joins_a_list() {
    for i in 0..50 {
        let s = parse_for(&format!("job-{i}"), HOST, "0,H(30-40),45 * * * *").unwrap();
        let values = &s.fields()[0].values;
        assert!(values.contains(&0) && values.contains(&45));
        assert!(values.iter().any(|v| (30..=40).contains(v)));
        assert_eq!(values.len(), 3);
    }
}

#[skuld::test]
fn day_rule_follows_vixie() {
    for (line, rule) in [
        ("0 0 */2 * 1", DayRule::And),
        ("0 0 1 * 1", DayRule::Or),
        ("0 0 H * 1", DayRule::Or),
        ("0 0 * * *", DayRule::And),
        ("0 0 1 * *", DayRule::And),
        ("0 0 * * 1", DayRule::And),
        ("0 0 1 * H", DayRule::Or),
        ("0 0 *,5 * 1", DayRule::And),
        ("0 0 5,* * 1", DayRule::Or),
    ] {
        assert_eq!(parse(line).day_rule(), rule, "{line:?}");
    }
}

const ROUND_TRIP_LINES: &[&str] = &[
    "* * * * *",
    "5 4 3 2 1",
    "*/15 */6 */10 */4 */2",
    "10-50/20 0-12 1-31/10 1-12 0-7",
    "H H H H H",
    "H/15 H(2-6)/2 H(1-31) H/5 H(0-6)",
    "0,H(30-40),45 * 1,H * 7",
    "H(10-12)/20 * * * *",
];

/// The stored form of `schedule`: its resolved fields.
fn parts(schedule: &Schedule) -> [CronField; 5] {
    schedule.fields().clone()
}

#[skuld::test]
fn from_parts_round_trips_parse() {
    for host in [HOST, "other"] {
        for daemon in ["nightly", "backup", "a", "x.y_z-1"] {
            for text in ROUND_TRIP_LINES {
                let parsed = parse_for(daemon, host, text).unwrap();
                let rebuilt = Schedule::from_parts(&id(daemon), parsed.text().to_owned(), parts(&parsed));
                assert_eq!(rebuilt.unwrap_or_else(|e| panic!("{text:?}: {e}")), parsed);
            }
        }
    }
}

#[skuld::test]
fn schedules_with_the_same_picks_are_equal_whatever_the_host() {
    // `host0` and `host1` hash differently and both pick 0 of `H(0-1)`.
    let (a, b) = (
        parse_for("nightly", "host0", "H(0-1) * * * *").unwrap(),
        parse_for("nightly", "host1", "H(0-1) * * * *").unwrap(),
    );
    assert_eq!(a.fields(), b.fields());
    assert_eq!(a, b);

    let other_pick = parse_for("nightly", "host5", "H(0-1) * * * *").unwrap();
    assert_ne!(a, other_pick);
}

fn field(values: &[u8], restricted: bool) -> CronField {
    CronField {
        values: set(values),
        restricted,
    }
}

/// `text` as parsed for `nightly` on `build-host`, with field `index` swapped for `replacement`,
/// then handed to `from_parts`.
fn rebuild(text: &str, index: usize, replacement: CronField) -> Result<Schedule, Error> {
    let mut stored = parts(&parse(text));
    stored[index] = replacement;
    Schedule::from_parts(&id("nightly"), text.to_owned(), stored)
}

#[skuld::test]
fn from_parts_refuses_what_parse_cannot_give() {
    // (text, field index, replacement values, restricted)
    type Row = (&'static str, usize, Vec<u8>, bool);
    let table: Vec<Row> = vec![
        // an H-free field is exactly what the text gives
        ("5 * * * *", 0, vec![6], true),
        ("5 * * * *", 0, vec![5, 6], true),
        ("5 * * * *", 0, vec![], true),
        ("*/15 * * * *", 0, vec![0, 15, 30], true),
        ("* * * * *", 1, vec![0, 1], false),
        ("5 H * * *", 0, vec![6], true),
        ("* * * * *", 4, (0..=7).collect(), false),
        ("* * * * *", 0, vec![60], false),
        ("* * * * *", 3, vec![0], false),
        // a field with an H term has every fixed term's values, and only values some term could give
        ("5,H(10-20) * * * *", 0, vec![6, 21], true),
        ("5,H(10-20) * * * *", 0, vec![21], true),
        ("5,H(10-20) * * * *", 0, vec![5, 9], true),
        ("H(10-20) * * * *", 0, vec![21], true),
        ("H(10-12)/20 * * * *", 0, vec![13], true),
        ("H * * * *", 0, vec![], true),
        ("H * * * *", 0, vec![13, 60], true),
        ("* * H * *", 2, vec![0], true),
        ("* * H * *", 2, vec![30], true),
        ("* * * H *", 3, vec![13], true),
        ("* * * * H", 4, vec![7], true),
        // the restricted flag is the text's
        ("5 * * * *", 0, vec![5], false),
        ("H * * * *", 0, vec![13], false),
        ("* * * * *", 0, (0..=59).collect(), true),
    ];
    for (text, index, values, restricted) in table {
        let replacement = CronField {
            values: values.iter().copied().collect(),
            restricted,
        };
        let case = format!("{text:?} {index} {replacement:?}");
        let (daemon, message) = refusal(rebuild(text, index, replacement).expect_err(&case));
        assert_eq!(daemon, "nightly");
        assert!(
            message.contains(FIELD_NAMES[index]),
            "{case} must name its field: {message}"
        );
    }
}

#[skuld::test]
fn from_parts_keeps_the_values_an_h_field_is_given() {
    // A decode does not re-derive which pick `H` made: any values the field's terms could give are
    // kept, however many.
    for (text, index, values) in [
        ("H * * * *", 0, vec![14]),
        ("H * * * *", 0, vec![13, 14]),
        ("H(10-20) * * * *", 0, vec![15]),
        ("H/15 * * * *", 0, vec![13, 28, 43, 58]),
        ("5,H(10-20) * * * *", 0, vec![5, 20]),
        ("5,H(10-20) * * * *", 0, vec![5, 12, 20]),
        ("* * H * *", 2, vec![9]),
    ] {
        let rebuilt = rebuild(text, index, field(&values, true)).unwrap_or_else(|e| panic!("{text:?}: {e}"));
        assert_eq!(rebuilt.fields()[index].values, set(&values), "{text:?}");
    }
}

#[skuld::test]
fn an_empty_term_is_worded_without_blank_quotes() {
    // A blank field is no word at all, so a lone empty term cannot be written; `,` is the least.
    for line in ["1,,2 * * * *", ", * * * *", "* 1, * * *"] {
        let (_, message) = refusal(parse_for("nightly", HOST, line).unwrap_err());
        assert!(
            message.contains("an empty term") && !message.contains("``"),
            "{line:?}: {message}"
        );
    }
}

#[skuld::test]
fn from_parts_refuses_text_that_does_not_parse() {
    let stored = parts(&parse("* * * * *"));
    for text in ["", "foo * * * *", "* * * * * *", "60 * * * *", "H(10-5) * * * *"] {
        let (daemon, _) =
            refusal(Schedule::from_parts(&id("nightly"), text.to_owned(), stored.clone()).expect_err(text));
        assert_eq!(daemon, "nightly");
    }
}

#[skuld::test]
fn this_hostname_is_not_empty() {
    assert!(!super::this_hostname().is_empty());
}
