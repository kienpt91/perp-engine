//! Minimal argument parser: `command [positional...] [--flag] [--key value]`.

use std::collections::BTreeMap;

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    /// First positional argument, if any.
    pub command: Option<String>,
    /// Remaining positional arguments.
    pub positional: Vec<String>,
    options: BTreeMap<String, Option<String>>,
}

/// Flags that take no value.
const SWITCHES: &[&str] = &["init", "allow-non-loopback", "strict", "help"];

impl Args {
    /// Parse arguments (without the program name). `--key=value` is accepted.
    pub fn parse<I: IntoIterator<Item = String>>(items: I) -> Result<Args, String> {
        let mut args = Args::default();
        let mut it = items.into_iter();
        while let Some(item) = it.next() {
            let Some(name) = item.strip_prefix("--") else {
                if args.command.is_none() {
                    args.command = Some(item);
                } else {
                    args.positional.push(item);
                }
                continue;
            };
            if name.is_empty() {
                return Err("unexpected bare '--'".into());
            }
            let (key, inline) = match name.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (name.to_string(), None),
            };
            let value = if SWITCHES.contains(&key.as_str()) {
                if inline.is_some() {
                    return Err(format!("--{key} does not take a value"));
                }
                None
            } else if let Some(v) = inline {
                Some(v)
            } else {
                Some(it.next().ok_or_else(|| format!("--{key} needs a value"))?)
            };
            if args.options.insert(key.clone(), value).is_some() {
                return Err(format!("--{key} given twice"));
            }
        }
        Ok(args)
    }

    /// True when the switch or option was given.
    pub fn has(&self, key: &str) -> bool {
        self.options.contains_key(key)
    }

    /// String value of an option.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.options.get(key).and_then(|v| v.as_deref())
    }

    /// Unsigned integer option with a default.
    pub fn number(&self, key: &str, default: u64) -> Result<u64, String> {
        match self.value(key) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("--{key}: '{v}' is not a non-negative integer")),
        }
    }

    /// Reject options that the command does not know.
    pub fn only(&self, allowed: &[&str]) -> Result<(), String> {
        match self.options.keys().find(|k| !allowed.contains(&k.as_str())) {
            Some(k) => Err(format!("unknown option --{k}")),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parse(s: &[&str]) -> Result<Args, String> {
        Args::parse(s.iter().map(|x| x.to_string()))
    }

    #[test]
    fn splits_command_positionals_and_options() {
        let a = parse(&["replay", "log.wal", "--market", "2", "--init", "--depth=5"]).unwrap();
        assert_eq!(a.command.as_deref(), Some("replay"));
        assert_eq!(a.positional, vec!["log.wal".to_string()]);
        assert_eq!(a.value("market"), Some("2"));
        assert_eq!(a.number("depth", 10).unwrap(), 5);
        assert!(a.has("init"));
        assert_eq!(a.value("init"), None);
    }

    #[test]
    fn defaults_and_bad_numbers() {
        let a = parse(&["x", "--events", "12a"]).unwrap();
        assert_eq!(a.number("seed", 7).unwrap(), 7);
        assert!(a.number("events", 1).unwrap_err().contains("events"));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse(&["x", "--"]).is_err());
        assert!(parse(&["x", "--seed"]).is_err());
        assert!(parse(&["x", "--seed", "1", "--seed", "2"]).is_err());
        assert!(parse(&["x", "--init=1"]).is_err());
    }

    #[test]
    fn unknown_options_are_reported() {
        let a = parse(&["x", "--seed", "1", "--bogus", "2"]).unwrap();
        assert!(a.only(&["seed"]).unwrap_err().contains("bogus"));
        assert!(a.only(&["seed", "bogus"]).is_ok());
    }

    #[test]
    fn empty_input_has_no_command() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.command, None);
    }
}
