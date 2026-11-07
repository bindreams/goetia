//! The manifest field a value belongs to, shared by substitution (`interpolate`) and the rules
//! (`resolve`), which name a field to say whose text an error is about.

/// A value a rule judges, named so an error can say whose text is to blame.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Field {
    Name,
    /// An element of `command`; `Command(0)` is the program.
    Command(usize),
    Cwd,
    Logs,
    /// An `env` name, which substitution never changes ([`super::interpolate::spec`] refuses a `$` in one).
    EnvKey,
    /// The value of the `env` entry with this name.
    EnvValue(String),
    /// `user` or `user.name`, whichever form was written: both are one field to an error.
    User,
    Restart,
    RestartDelay,
    Kind,
}

impl Field {
    /// The manifest path of the field, as a substitution error names it. [`Field::User`] is `user`: the
    /// `user.name` form is the one path a caller spells itself.
    pub(super) fn path(&self) -> String {
        match self {
            Field::Name => "name".to_string(),
            Field::Command(index) => format!("command[{index}]"),
            Field::Cwd => "cwd".to_string(),
            Field::Logs => "logs".to_string(),
            Field::EnvKey => "env key".to_string(),
            Field::EnvValue(key) => format!("env[{key:?}]"),
            Field::User => "user".to_string(),
            Field::Restart => "restart".to_string(),
            Field::RestartDelay => "restart-delay".to_string(),
            Field::Kind => "type".to_string(),
        }
    }
}
