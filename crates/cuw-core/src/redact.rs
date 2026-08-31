/// Shorten a token to its first 4 chars plus an ellipsis, for safe logging.
/// Tokens never appear in full anywhere (plan §5). Char-based, so a short or
/// multibyte token never panics.
pub fn redact(token: &str) -> String {
    let prefix: String = token.chars().take(4).collect();
    format!("{prefix}…")
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn keeps_only_the_prefix() {
        let full = "sk-ant-oat01-abcdef";
        let r = redact(full);
        assert!(r.starts_with("sk-a"));
        assert!(!r.contains(full));
        assert!(!r.contains("abcdef"));
    }

    #[test]
    fn short_token_does_not_panic() {
        assert_eq!(redact("ab"), "ab…");
        assert_eq!(redact(""), "…");
    }
}
