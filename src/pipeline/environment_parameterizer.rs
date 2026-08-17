//! Deterministic environment-literal parameterization for Skill proposals.

use once_cell::sync::Lazy;
use regex::Regex;

pub use crate::domain::skill_proposal::EnvironmentContext;

static URL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?-u:\b)https?://[A-Za-z0-9._-]+(?::[0-9]{1,5})?(?:/[^\s,;]*)?")
        .expect("valid URL parameterization regex")
});

static IPV4: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?-u:\b)(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?-u:\b)")
        .expect("valid IPv4 parameterization regex")
});

static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?-u:\b)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}(?-u:\b)")
        .expect("valid UUID parameterization regex")
});

static ABSOLUTE_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?m)(^|[\s\(\[\{\"'=])/(?:[A-Za-z0-9._~-]+/)*[A-Za-z0-9._~-]+"#)
        .expect("valid absolute path parameterization regex")
});

pub fn parameterize(text: &str, context: &EnvironmentContext) -> String {
    let mut output = text.to_string();
    for (literal, replacement) in [
        (context.workspace_root.as_deref(), "{{workspace_root}}"),
        (context.home_dir.as_deref(), "{{home_dir}}"),
        (context.temp_dir.as_deref(), "{{temp_dir}}"),
    ] {
        if let Some(literal) = literal.filter(|value| !value.is_empty()) {
            output = output.replace(literal, replacement);
        }
    }
    output = URL.replace_all(&output, "{{base_url}}").into_owned();
    output = ABSOLUTE_PATH
        .replace_all(&output, "${1}{{absolute_path}}")
        .into_owned();
    output = IPV4.replace_all(&output, "{{target_host}}").into_owned();
    UUID.replace_all(&output, "{{resource_id}}").into_owned()
}
