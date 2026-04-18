use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub enum Matcher {
    Contains(String),
    NotContains(String),
    Equals(String),
    AnyOf(Vec<Matcher>),
}

impl Matcher {
    pub fn contains(value: impl Into<String>) -> Self {
        Self::Contains(value.into())
    }

    pub fn not_contains(value: impl Into<String>) -> Self {
        Self::NotContains(value.into())
    }

    pub fn equals(value: impl Into<String>) -> Self {
        Self::Equals(value.into())
    }

    pub fn any_of(values: Vec<Matcher>) -> Self {
        Self::AnyOf(values)
    }

    pub fn check(&self, actual: &str, label: &str) -> Result<()> {
        match self {
            Self::Contains(expected) => {
                if actual.contains(expected) {
                    Ok(())
                } else {
                    bail!(
                        "expected {label} to contain {:?}\nactual:\n{}",
                        expected,
                        actual
                    )
                }
            }
            Self::NotContains(expected) => {
                if actual.contains(expected) {
                    bail!(
                        "expected {label} not to contain {:?}\nactual:\n{}",
                        expected,
                        actual
                    )
                } else {
                    Ok(())
                }
            }
            Self::Equals(expected) => {
                if actual == expected {
                    Ok(())
                } else {
                    bail!(
                        "expected {label} to equal {:?}\nactual:\n{}",
                        expected,
                        actual
                    )
                }
            }
            Self::AnyOf(matchers) => {
                let mut errors = Vec::new();
                for matcher in matchers {
                    match matcher.check(actual, label) {
                        Ok(()) => return Ok(()),
                        Err(err) => errors.push(err.to_string()),
                    }
                }
                bail!(
                    "expected {label} to match one of {} variants\n{}\nactual:\n{}",
                    matchers.len(),
                    errors.join("\n---\n"),
                    actual
                )
            }
        }
    }
}
