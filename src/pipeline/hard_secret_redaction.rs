//! Non-disableable secret boundary for Skill proposal compilation.

use once_cell::sync::Lazy;
use regex::Regex;

static SENSITIVE_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(?:\\?["'])?(api[\s_-]*key|access[\s_-]*token|auth[\s_-]*token|session[\s_-]*token|client[\s_-]*secret|password|passwd|secret|cookie)(?:\\?["'])?\s*[:=]\s*(?:\\?"[^"\r\n]{8,}\\?"|\\?'[^'\r\n]{8,}\\?'|[^\s,;]{8,})"#,
    )
    .expect("valid hard-redaction assignment regex")
});

static AUTHORIZATION_HEADER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(?:\\?["'])?(authorization|proxy-authorization)(?:\\?["'])?\s*:\s*(?:\\?["'])?(basic|bearer)\s+[A-Za-z0-9+/=_-]{8,}(?:\\?["'])?"#,
    )
    .expect("valid authorization header regex")
});

static COOKIE_HEADER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)(?:\\?["'])?(cookie|set-cookie)(?:\\?["'])?\s*:\s*[^\r\n]{8,}"#)
        .expect("valid cookie header regex")
});

static URI_USERINFO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)([a-z][a-z0-9+.-]*://[^\s:/@]+:)([^\s/@]{4,})(@)")
        .expect("valid URI userinfo regex")
});

#[derive(Clone, PartialEq, Eq)]
pub struct SanitizedText {
    text: String,
    finding_count: usize,
}

impl std::fmt::Debug for SanitizedText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SanitizedText")
            .field("bytes", &self.text.len())
            .field("finding_count", &self.finding_count)
            .finish()
    }
}

impl SanitizedText {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn finding_count(&self) -> usize {
        self.finding_count
    }
}

/// Scrub compiler input regardless of the configurable output-redaction flag.
pub fn hard_scrub(text: &str) -> SanitizedText {
    let mut finding_count = 0_usize;
    let known = crate::pipeline::redact::redact_all(text);
    if known.as_ref() != text {
        finding_count += 1;
    }
    let assigned =
        SENSITIVE_ASSIGNMENT.replace_all(known.as_ref(), |captures: &regex::Captures| {
            finding_count += 1;
            format!("{}=[redacted:credential]", &captures[1])
        });
    let uri = URI_USERINFO.replace_all(assigned.as_ref(), |captures: &regex::Captures| {
        finding_count += 1;
        format!("{}[redacted:uri-credential]{}", &captures[1], &captures[3])
    });
    let authorization =
        AUTHORIZATION_HEADER.replace_all(uri.as_ref(), |captures: &regex::Captures| {
            finding_count += 1;
            format!("{}: [redacted:authorization]", &captures[1])
        });
    let cookie = COOKIE_HEADER.replace_all(authorization.as_ref(), |captures: &regex::Captures| {
        finding_count += 1;
        format!("{}: [redacted:cookie]", &captures[1])
    });
    SanitizedText {
        text: cookie.into_owned(),
        finding_count,
    }
}

/// Generated artifacts fail closed when scrubbing would change any byte.
pub fn hard_scan(text: &str) -> Result<(), usize> {
    let sanitized = hard_scrub(text);
    if sanitized.finding_count == 0 && sanitized.text == text {
        Ok(())
    } else {
        Err(sanitized.finding_count.max(1))
    }
}
