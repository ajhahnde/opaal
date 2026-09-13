#![forbid(unsafe_code)]

pub fn readiness() -> &'static str {
    "ready"
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixture_is_non_publishing_and_testable() {
        assert_eq!(super::readiness(), "ready");
    }
}
