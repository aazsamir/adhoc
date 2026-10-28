//! `adhoc example`: print a starter config.

/// Kept as a real file so it is tested by the same parser users hit.
const EXAMPLE: &str = include_str!("../example_config.yaml");

pub fn cmd_example() {
    print!("{EXAMPLE}");
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn the_printed_example_is_a_valid_config() {
        let cfg: Config = serde_saphyr::from_str(super::EXAMPLE).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].primary_app_port(), 8080);
        assert_eq!(cfg.services[0].name, "web");
    }
}
