//! Hand-rolled argument parsing.
//!
//! The flag surface is small and the internal commands must not care about
//! argument order, so this stays deliberately dumb:
//!
//!   --config x | --config=x | -config x | -config=x | -c x
//!
//! Values are returned as owned strings; unknown flags are collected so each
//! command can decide whether that is an error or just the start of the app's
//! own argv.

/// One parsed flag set.
#[derive(Debug, Default)]
pub struct Args {
    pub flags: Vec<(String, Option<String>)>,
    /// Everything after a `--` separator, verbatim.
    pub rest: Vec<String>,
}

impl Args {
    /// Parses `argv`, treating `value_flags` as flags that consume the next
    /// argument. Unknown `-x` flags are stored, but never consume a value.
    pub fn parse(argv: &[String], value_flags: &[&str]) -> Args {
        let mut args = Args::default();
        let mut iter = argv.iter().peekable();

        while let Some(arg) = iter.next() {
            if arg == "--" {
                args.rest.extend(iter.by_ref().cloned());
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                // Stop at the first non-flag argument; the rest belongs to the
                // child command.
                args.rest.push(arg.clone());
                args.rest.extend(iter.by_ref().cloned());
                break;
            }

            let trimmed = arg.trim_start_matches('-');
            let (name, inline) = match trimmed.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (trimmed, None),
            };

            let canonical = canonical_name(name);
            let takes_value = value_flags.contains(&canonical);
            let value = match (inline, takes_value) {
                (Some(value), _) => Some(value),
                (None, true) => iter.next().cloned(),
                (None, false) => None,
            };
            args.flags.push((canonical.to_string(), value));
        }
        args
    }

    /// True when the flag was given at all.
    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(flag, _)| flag == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(flag, _)| flag == name)
            .and_then(|(_, value)| value.as_deref())
    }

    /// A value with a fallback.
    pub fn value_or<'a>(&'a self, name: &str, default: &'a str) -> &'a str {
        self.value(name).unwrap_or(default)
    }
}

/// Maps the short aliases onto their long names.
fn canonical_name(name: &str) -> &str {
    match name {
        "c" => "config",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn parses_both_dash_styles() {
        let args = Args::parse(&argv(&["--config", "a.yaml", "-debug"]), &["config"]);
        assert_eq!(args.value("config"), Some("a.yaml"));
        assert!(args.has("debug"));

        let args = Args::parse(&argv(&["-config=a.yaml", "--debug"]), &["config"]);
        assert_eq!(args.value("config"), Some("a.yaml"));
        assert!(args.has("debug"));

        let args = Args::parse(&argv(&["-c", "b.yaml"]), &["config"]);
        assert_eq!(args.value("config"), Some("b.yaml"));

        let args = Args::parse(&argv(&["--config=c.yaml"]), &["config"]);
        assert_eq!(args.value("config"), Some("c.yaml"));
    }

    #[test]
    fn keeps_positionals_and_separator() {
        let args = Args::parse(
            &argv(&["-netns-pid", "42", "--", "sleep", "10"]),
            &["netns-pid"],
        );
        assert_eq!(args.value("netns-pid"), Some("42"));
        assert_eq!(args.rest, vec!["sleep", "10"]);

        let args = Args::parse(&argv(&["sleep", "10"]), &[]);
        assert_eq!(args.rest, vec!["sleep", "10"]);
    }

    #[test]
    fn repeated_flags_are_kept() {
        let args = Args::parse(&argv(&["-dns", "1.1.1.1", "-dns", "8.8.8.8"]), &["dns"]);
        let values: Vec<&str> = args
            .flags
            .iter()
            .filter(|(flag, _)| flag == "dns")
            .filter_map(|(_, value)| value.as_deref())
            .collect();
        assert_eq!(values, vec!["1.1.1.1", "8.8.8.8"]);
    }
}
