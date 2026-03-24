use super::*;

#[derive(Default, Eq, PartialEq, Hash, Clone, Debug)]
pub struct Labels {
    pub inner: BTreeMap<String, String>,
}

/// Check if a value matches a pattern.
/// Supports: alternation with `|`, optional parens `(a|b)`, escaped dots `\.`.
fn match_pattern(value: &str, pattern: &str) -> bool {
    if pattern.contains('|') {
        let inner = if pattern.starts_with('(') && pattern.ends_with(')') {
            &pattern[1..pattern.len() - 1]
        } else {
            pattern
        };
        inner.split('|').any(|option| {
            if option.contains("\\.") {
                value == option.replace("\\.", ".")
            } else {
                value == option
            }
        })
    } else if pattern.contains("\\.") {
        value == pattern.replace("\\.", ".")
    } else {
        value == pattern
    }
}

impl Labels {
    pub fn matches(&self, other: &Labels) -> bool {
        for (label, value) in other.inner.iter() {
            // Check if it's a negative match pattern
            if let Some(pattern) = value.strip_prefix('!') {
                // Negative match (from != or !~ operator)
                if let Some(v) = self.inner.get(label) {
                    if match_pattern(v, pattern) {
                        return false;
                    }
                }
                // If label doesn't exist, it passes the negative filter
            } else if let Some(pattern) = value.strip_prefix('~') {
                // Regex positive match (from =~ operator, marked with ~ prefix)
                let Some(v) = self.inner.get(label) else {
                    return false;
                };
                if !match_pattern(v, pattern) {
                    return false;
                }
            } else if let Some(v) = self.inner.get(label) {
                // Exact positive match (from = operator)
                if !match_pattern(v, value) {
                    return false;
                }
            } else {
                // Label doesn't exist but was required (positive match)
                return false;
            }
        }

        true
    }
}

impl From<&[(&str, &str)]> for Labels {
    fn from(other: &[(&str, &str)]) -> Self {
        Labels {
            inner: other
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

impl From<()> for Labels {
    fn from(_other: ()) -> Self {
        Labels::default()
    }
}

impl<const N: usize> From<[(&str, &str); N]> for Labels {
    fn from(other: [(&str, &str); N]) -> Self {
        Labels {
            inner: other
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

impl<const N: usize> From<[(String, String); N]> for Labels {
    fn from(other: [(String, String); N]) -> Self {
        Labels {
            inner: other.iter().cloned().collect(),
        }
    }
}

impl<const N: usize> From<[(&str, String); N]> for Labels {
    fn from(other: [(&str, String); N]) -> Self {
        Labels {
            inner: other
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }
}

impl From<&mut dyn Iterator<Item = (&str, &str)>> for Labels {
    fn from(other: &mut dyn Iterator<Item = (&str, &str)>) -> Self {
        Self {
            inner: other.map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }
}
