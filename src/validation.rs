//! Hand validation, reproducing `Violations.java`: collects every field violation of one request,
//! then fails with all of them at once. Required strings are trimmed (Java `String.strip`
//! semantics), non-blank and within their max length (UTF-16 code units, like Java);
//! optional strings (partial updates) are only checked when present.

use crate::error::{ApiError, FieldViolation};
use uuid::Uuid;

pub const TITLE_MAX: usize = 100;
pub const CONTENT_MAX: usize = 1000;
pub const NAME_MAX: usize = 50;
pub const EMAIL_MAX: usize = 100;
pub const LIMIT_DEFAULT: i32 = 50;
pub const LIMIT_MAX: i32 = 200;

#[derive(Debug, Default)]
pub struct Violations {
    list: Vec<FieldViolation>,
}

/// `Character.isWhitespace`: Unicode space separators except the non-breaking ones, plus the
/// ASCII/C0 controls Java treats as whitespace. (Rust's `char::is_whitespace` differs: it also
/// strips U+00A0, U+2007, U+202F, U+0085.)
pub fn java_is_whitespace(c: char) -> bool {
    matches!(c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{001C}'..='\u{001F}'
        | ' ' | '\u{1680}' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200A}'
        | '\u{2028}' | '\u{2029}' | '\u{205F}' | '\u{3000}')
}

/// `String.strip()`.
pub fn java_strip(s: &str) -> &str {
    s.trim_matches(java_is_whitespace)
}

/// Java regex `^[^\s@]+@[^\s@]+\.[^\s@]+$` (`\s` is ASCII-only there).
pub fn looks_like_email(s: &str) -> bool {
    let bad = |c: char| matches!(c, ' ' | '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '@');
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    if local.is_empty() || local.chars().any(bad) || domain.chars().any(bad) {
        return false;
    }
    // domain = [^\s@]+ "." [^\s@]+ : some dot that is neither the first nor the last char.
    let chars: Vec<char> = domain.chars().collect();
    chars.len() >= 3 && chars[1..chars.len() - 1].contains(&'.')
}

/// Canonical 8-4-4-4-12 form only (`Uuid::parse_str` also accepts braces, URNs, simple form).
pub fn parse_canonical_uuid(s: &str) -> Option<Uuid> {
    let b = s.as_bytes();
    let ok = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    if ok { Uuid::parse_str(s).ok() } else { None }
}

impl Violations {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, field: &str, description: impl Into<String>) {
        self.list.push(FieldViolation {
            field: field.to_owned(),
            description: description.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn text(&mut self, field: &str, value: &str, max_len: usize) -> String {
        let trimmed = java_strip(value);
        if trimmed.contains('\0') {
            self.add(field, format!("{field} cannot contain NUL characters"));
        } else if trimmed.is_empty() {
            self.add(field, format!("{field} is required and cannot be blank"));
        } else if trimmed.encode_utf16().count() > max_len {
            self.add(field, format!("{field} cannot exceed {max_len} characters"));
        }
        trimmed.to_owned()
    }

    pub fn email(&mut self, field: &str, value: &str) -> String {
        let before = self.list.len();
        let trimmed = self.text(field, value, EMAIL_MAX);
        if self.list.len() == before && !looks_like_email(&trimmed) {
            self.add(field, format!("{field} must be a valid email address"));
        }
        trimmed
    }

    pub fn uuid(&mut self, field: &str, value: &str) -> Option<Uuid> {
        let parsed = parse_canonical_uuid(value);
        if parsed.is_none() {
            self.add(field, format!("{field} must be a valid UUID"));
        }
        parsed
    }

    pub fn limit(&mut self, field: &str, value: Option<i32>) -> i32 {
        match value {
            None => LIMIT_DEFAULT,
            Some(v) => {
                if !(1..=LIMIT_MAX).contains(&v) {
                    self.add(field, format!("{field} must be between 1 and {LIMIT_MAX}"));
                }
                v
            }
        }
    }

    pub fn offset(&mut self, field: &str, value: Option<i64>) -> i64 {
        match value {
            None => 0,
            Some(v) => {
                if v < 0 {
                    self.add(field, format!("{field} must be 0 or greater"));
                }
                v
            }
        }
    }

    pub fn finish(self) -> Result<(), ApiError> {
        if self.list.is_empty() {
            Ok(())
        } else {
            Err(ApiError::BadInput(self.list))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violations(v: Violations) -> Vec<(String, String)> {
        match v.finish() {
            Err(ApiError::BadInput(l)) => l.into_iter().map(|x| (x.field, x.description)).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn text_trims_and_checks() {
        let mut v = Violations::new();
        assert_eq!(v.text("title", "  hello \n", 100), "hello");
        assert!(v.is_empty());
        v.text("title", "   ", 100);
        v.text("content", &"x".repeat(1001), 1000);
        v.text("name", "a\0b", 50);
        assert_eq!(
            violations(v),
            vec![
                (
                    "title".into(),
                    "title is required and cannot be blank".into()
                ),
                (
                    "content".into(),
                    "content cannot exceed 1000 characters".into()
                ),
                ("name".into(), "name cannot contain NUL characters".into()),
            ]
        );
    }

    #[test]
    fn length_is_utf16_units_like_java() {
        let mut v = Violations::new();
        // 50 emoji = 100 UTF-16 units: over a 50 limit although only 50 code points.
        v.text("name", &"😀".repeat(50), 50);
        assert_eq!(violations(v).len(), 1);
        let mut ok = Violations::new();
        ok.text("name", &"😀".repeat(25), 50);
        assert!(ok.is_empty());
    }

    #[test]
    fn strip_follows_java_whitespace() {
        assert_eq!(java_strip("\u{3000}x\u{2003}"), "x");
        // NBSP is not whitespace for Java's strip(), so it survives.
        assert_eq!(java_strip("\u{00A0}x"), "\u{00A0}x");
        let mut v = Violations::new();
        v.text("title", "\u{00A0}", 10);
        assert!(v.is_empty());
    }

    #[test]
    fn email_rules() {
        for ok in ["a@b.co", "a.b+c@sub.example.org", "x@y.z.w"] {
            assert!(looks_like_email(ok), "{ok}");
        }
        for bad in [
            "", "a", "a@b", "@b.c", "a@.c", "a@b.", "a b@c.d", "a@@b.c", "a@b@c.d", "a@b c.d",
        ] {
            assert!(!looks_like_email(bad), "{bad}");
        }
        let mut v = Violations::new();
        v.email("email", "nope");
        v.email("email", &format!("{}@x.io", "a".repeat(100)));
        let l = violations(v);
        assert_eq!(l[0].1, "email must be a valid email address");
        assert_eq!(l[1].1, "email cannot exceed 100 characters");
    }

    #[test]
    fn uuid_is_canonical_only() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert!(parse_canonical_uuid(id).is_some());
        assert!(parse_canonical_uuid(&id.to_uppercase()).is_some());
        for bad in [
            "",
            "1-2-3-4-5",
            "123e4567e89b12d3a456426614174000",
            "{123e4567-e89b-12d3-a456-426614174000}",
            "urn:uuid:123e4567-e89b-12d3-a456-426614174000",
            "123e4567-e89b-12d3-a456-42661417400g",
        ] {
            assert!(parse_canonical_uuid(bad).is_none(), "{bad}");
        }
        let mut v = Violations::new();
        assert!(v.uuid("author_id", "x").is_none());
        assert_eq!(violations(v)[0].1, "author_id must be a valid UUID");
    }

    #[test]
    fn limit_and_offset() {
        let mut v = Violations::new();
        assert_eq!(v.limit("limit", None), 50);
        assert_eq!(v.offset("offset", None), 0);
        assert_eq!(v.limit("limit", Some(200)), 200);
        assert!(v.is_empty());
        v.limit("limit", Some(0));
        v.limit("messages_limit", Some(201));
        v.offset("offset", Some(-1));
        assert_eq!(
            violations(v).into_iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![
                "limit must be between 1 and 200",
                "messages_limit must be between 1 and 200",
                "offset must be 0 or greater"
            ]
        );
    }
}
