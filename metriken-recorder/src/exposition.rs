//! A parser for the Prometheus text exposition format.
//!
//! It replaces the `prometheus-parse` crate, whose label parser split the
//! label block on every `,` and each pair on every `=`. A label value
//! containing `=` (`op="a=b"`) produced three pieces and the pair was dropped,
//! a value containing `,` (`op="a,b"`) was cut at the comma, a value
//! containing `}` made the whole line fail to match and the sample was
//! dropped, and escapes (`\"`, `\\`, `\n`) were stored as written rather than
//! as the characters they stand for. Here a label value is read as the format
//! defines it: a double-quoted string in which `\\`, `\"` and `\n` are the
//! only escapes.
//!
//! Everything else follows what `prometheus-parse` 0.2.5 did, so a scrape
//! with ordinary label values converts exactly as before: `# HELP` text is
//! kept as written, `# TYPE x histogram` makes `x_bucket` lines the buckets of
//! histogram `x` (its `_sum` and `_count` are untyped), a summary's lines
//! named exactly `x` with a `quantile` label are its quantiles, a trailing
//! timestamp is ignored, and a line that does not parse is skipped.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleType {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
}

impl SampleType {
    fn parse(s: &str) -> Self {
        match s {
            "counter" => Self::Counter,
            "gauge" => Self::Gauge,
            "histogram" => Self::Histogram,
            "summary" => Self::Summary,
            _ => Self::Untyped,
        }
    }
}

/// One histogram bucket: the cumulative count of observations at or below
/// `less_than` (the bucket's `le`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HistogramCount {
    pub less_than: f64,
    pub count: f64,
}

/// One summary quantile and its value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SummaryCount {
    pub quantile: f64,
    pub count: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Counter(f64),
    Gauge(f64),
    Histogram(Vec<HistogramCount>),
    Summary(Vec<SummaryCount>),
    Untyped(f64),
}

/// One series of a scrape. `labels` is sorted by name and holds each name
/// once; for a histogram or summary it excludes `le` or `quantile`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sample {
    pub metric: String,
    pub labels: Vec<(String, String)>,
    pub value: Value,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Scrape {
    /// `# HELP` text by metric name, last seen wins.
    pub docs: HashMap<String, String>,
    pub samples: Vec<Sample>,
}

impl Scrape {
    pub(crate) fn parse(text: &str) -> Scrape {
        let mut docs = HashMap::new();
        let mut types: HashMap<String, SampleType> = HashMap::new();
        // `x_bucket` -> `x` for each `# TYPE x histogram`.
        let mut aliases: HashMap<String, String> = HashMap::new();
        let mut samples = Vec::new();
        // Histograms and summaries in the order their first line appeared,
        // keyed by (line's metric name, labels without `le`/`quantile`).
        let mut grouped: Vec<Sample> = Vec::new();
        let mut grouped_index: HashMap<(String, Vec<(String, String)>), usize> = HashMap::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('#') {
                match parse_comment(line) {
                    Some(Comment::Help { name, doc }) => {
                        docs.insert(name.to_string(), doc.to_string());
                    }
                    Some(Comment::Type { name, kind }) => {
                        let kind = SampleType::parse(kind);
                        if kind == SampleType::Histogram {
                            let bucket = format!("{name}_bucket");
                            aliases.insert(bucket.clone(), name.to_string());
                            types.insert(bucket, kind);
                        } else {
                            types.insert(name.to_string(), kind);
                        }
                    }
                    None => {}
                }
                continue;
            }
            let Some(line) = parse_sample_line(line) else {
                continue;
            };
            let Ok(value) = parse_golang_float(line.value) else {
                continue;
            };
            let kind = types.get(line.name).copied();
            // A histogram or summary line without labels cannot carry its
            // `le` or `quantile`, and is kept as a plain sample, as
            // prometheus-parse kept it.
            let grouping = match kind {
                Some(SampleType::Histogram) if !line.labels.is_empty() => Some("le"),
                Some(SampleType::Summary) if !line.labels.is_empty() => Some("quantile"),
                _ => None,
            };
            if let Some(key_label) = grouping {
                let mut labels = line.labels;
                let Some(pos) = labels.iter().position(|(k, _)| k == key_label) else {
                    continue;
                };
                let (_, bound) = labels.remove(pos);
                let Ok(bound) = parse_golang_float(&bound) else {
                    continue;
                };
                let key = (line.name.to_string(), labels);
                let index = *grouped_index.entry(key.clone()).or_insert_with(|| {
                    let metric = aliases
                        .get(line.name)
                        .cloned()
                        .unwrap_or_else(|| line.name.to_string());
                    let value = if key_label == "le" {
                        Value::Histogram(Vec::new())
                    } else {
                        Value::Summary(Vec::new())
                    };
                    grouped.push(Sample {
                        metric,
                        labels: key.1,
                        value,
                    });
                    grouped.len() - 1
                });
                match &mut grouped[index].value {
                    Value::Histogram(buckets) => buckets.push(HistogramCount {
                        less_than: bound,
                        count: value,
                    }),
                    Value::Summary(quantiles) => quantiles.push(SummaryCount {
                        quantile: bound,
                        count: value,
                    }),
                    _ => unreachable!("a grouped sample is a histogram or a summary"),
                }
                continue;
            }
            samples.push(Sample {
                metric: line.name.to_string(),
                labels: line.labels,
                value: match kind {
                    Some(SampleType::Counter) => Value::Counter(value),
                    Some(SampleType::Gauge) => Value::Gauge(value),
                    _ => Value::Untyped(value),
                },
            });
        }
        samples.extend(grouped);
        Scrape { docs, samples }
    }
}

enum Comment<'a> {
    Help { name: &'a str, doc: &'a str },
    Type { name: &'a str, kind: &'a str },
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Split off a leading run of word characters (`\w+`), returning it and the
/// rest, or `None` when there is none.
fn take_word(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !is_word(c)).unwrap_or(s.len());
    (end > 0).then(|| s.split_at(end))
}

/// Strip at least one leading whitespace character, or `None`.
fn take_space(s: &str) -> Option<&str> {
    let rest = s.trim_start();
    (rest.len() < s.len()).then_some(rest)
}

/// `# HELP <name> <doc>` or `# TYPE <name> <type>`; any other comment is
/// `None`.
fn parse_comment(line: &str) -> Option<Comment<'_>> {
    let rest = take_space(line.strip_prefix('#')?)?;
    if let Some(rest) = rest.strip_prefix("HELP") {
        let (name, rest) = take_word(take_space(rest)?)?;
        let doc = take_space(rest)?;
        return (!doc.is_empty()).then_some(Comment::Help { name, doc });
    }
    if let Some(rest) = rest.strip_prefix("TYPE") {
        let (name, rest) = take_word(take_space(rest)?)?;
        let (kind, _) = take_word(take_space(rest)?)?;
        return Some(Comment::Type { name, kind });
    }
    None
}

struct SampleLine<'a> {
    name: &'a str,
    /// Sorted by name, each name once (the last occurrence on the line).
    labels: Vec<(String, String)>,
    value: &'a str,
}

/// `<name>[{<labels>}] <value> [<timestamp>]`, or `None` when the line is not
/// one.
fn parse_sample_line(line: &str) -> Option<SampleLine<'_>> {
    let (name, rest) = take_word(line)?;
    let (labels, rest) = match rest.strip_prefix('{') {
        Some(block) => parse_labels(block)?,
        None => (Vec::new(), rest),
    };
    let rest = take_space(rest)?;
    let value = rest.split(char::is_whitespace).next()?;
    if value.is_empty() {
        return None;
    }
    Some(SampleLine {
        name,
        labels,
        value,
    })
}

/// Parse a label block from just after its `{` through its `}`, returning
/// the labels and what follows the `}`. `None` when the block is malformed:
/// a missing `=` or quote, an unterminated value, or no closing `}`.
fn parse_labels(s: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut labels: Vec<(String, String)> = Vec::new();
    let mut rest = s.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix('}') {
            labels.sort_by(|a, b| a.0.cmp(&b.0));
            return Some((labels, after));
        }
        let (name, after) = take_word(rest)?;
        let after = after.trim_start().strip_prefix('=')?;
        let after = after.trim_start().strip_prefix('"')?;
        let (value, after) = parse_quoted(after)?;
        // A repeated name is invalid exposition; the last value wins, as it
        // did when labels were collected into a map.
        labels.retain(|(k, _)| k != name);
        labels.push((name.to_string(), value));
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else if !rest.starts_with('}') {
            return None;
        }
    }
}

/// Read a label value from just after its opening `"` through the closing
/// one, returning the unescaped value and what follows. `\\`, `\"` and `\n`
/// are the format's escapes; any other backslash is kept as written, as
/// Prometheus keeps it.
fn parse_quoted(s: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((value, &s[i + 1..])),
            '\\' => match chars.next() {
                Some((_, '\\')) => value.push('\\'),
                Some((_, '"')) => value.push('"'),
                Some((_, 'n')) => value.push('\n'),
                Some((_, other)) => {
                    value.push('\\');
                    value.push(other);
                }
                None => return None,
            },
            c => value.push(c),
        }
    }
    None
}

/// A sample value: Go's float syntax, which spells NaN as `NaN` and the
/// infinities as `+Inf`/`-Inf`.
fn parse_golang_float(s: &str) -> Result<f64, std::num::ParseFloatError> {
    match s.to_lowercase().as_str() {
        "nan" => Ok(f64::NAN),
        s => s.parse::<f64>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_of(line: &str) -> Option<Vec<(String, String)>> {
        let scrape = Scrape::parse(&format!("{line}\n"));
        assert!(scrape.samples.len() <= 1);
        scrape.samples.into_iter().next().map(|s| s.labels)
    }

    fn one(k: &str, v: &str) -> Option<Vec<(String, String)>> {
        Some(vec![(k.to_string(), v.to_string())])
    }

    /// Every character `prometheus-parse` lost or mangled, read back as the
    /// value the exposition text spells.
    #[test]
    fn a_label_value_is_read_as_the_format_spells_it() {
        for (text, value) in [
            (r#"a=b"#, "a=b"),
            (r#"a="#, "a="),
            (r#"x=1,y=2"#, "x=1,y=2"),
            (r#"a,b"#, "a,b"),
            (r#"a}b"#, "a}b"),
            (r#"a{b"#, "a{b"),
            (r#"a\"b"#, "a\"b"),
            (r#"a\\b"#, "a\\b"),
            (r#"a\nb"#, "a\nb"),
            (r#"a\tb"#, "a\\tb"),
            (r#"\\"#, "\\"),
            (r#"<b>"#, "<b>"),
            (r#"a b"#, "a b"),
            (r#"a&b"#, "a&b"),
            (r#"a'b"#, "a'b"),
            ("ü€", "ü€"),
            ("", ""),
        ] {
            let line = format!(r#"xss_total{{op="{text}"}} 1"#);
            assert_eq!(labels_of(&line), one("op", value), "{line}");
        }
    }

    #[test]
    fn labels_are_sorted_and_whitespace_and_a_trailing_comma_are_allowed() {
        assert_eq!(
            labels_of(r#"m{ z = "1" , a="x=y", } 3"#),
            Some(vec![
                ("a".to_string(), "x=y".to_string()),
                ("z".to_string(), "1".to_string()),
            ])
        );
        assert_eq!(labels_of("m{} 3"), Some(vec![]));
        assert_eq!(labels_of("m 3"), Some(vec![]));
    }

    /// A malformed label block drops the line rather than recording the
    /// sample under labels it did not have.
    #[test]
    fn a_malformed_label_block_drops_the_line() {
        for line in [
            r#"m{op="a} 1"#,
            r#"m{op="a"b"} 1"#,
            r#"m{op=a} 1"#,
            r#"m{op} 1"#,
            r#"m{op="a" 1"#,
            r#"m{op="a\"} 1"#,
            r#"m{op="a"}1"#,
        ] {
            assert_eq!(labels_of(line), None, "{line}");
        }
    }

    #[test]
    fn a_histogram_keeps_a_label_value_with_special_characters() {
        let scrape = Scrape::parse(
            "# TYPE lat_seconds histogram\n\
             lat_seconds_bucket{op=\"a=b,c\",le=\"0.1\"} 1\n\
             lat_seconds_bucket{op=\"a=b,c\",le=\"+Inf\"} 2\n\
             lat_seconds_sum{op=\"a=b,c\"} 0.2\n",
        );
        let histogram = scrape
            .samples
            .iter()
            .find(|s| matches!(s.value, Value::Histogram(_)))
            .expect("the buckets form one histogram");
        assert_eq!(histogram.metric, "lat_seconds");
        assert_eq!(histogram.labels, vec![("op".into(), "a=b,c".into())]);
        assert_eq!(
            histogram.value,
            Value::Histogram(vec![
                HistogramCount {
                    less_than: 0.1,
                    count: 1.0
                },
                HistogramCount {
                    less_than: f64::INFINITY,
                    count: 2.0
                },
            ])
        );
        let sum = scrape
            .samples
            .iter()
            .find(|s| s.metric == "lat_seconds_sum")
            .expect("_sum is its own sample");
        assert_eq!(sum.value, Value::Untyped(0.2));
    }

    #[test]
    fn a_summary_groups_its_quantiles() {
        let scrape = Scrape::parse(
            "# TYPE rpc summary\n\
             rpc{op=\"q=1\",quantile=\"0.5\"} 4\n\
             rpc{op=\"q=1\",quantile=\"0.99\"} 9\n",
        );
        assert_eq!(
            scrape.samples,
            vec![Sample {
                metric: "rpc".into(),
                labels: vec![("op".into(), "q=1".into())],
                value: Value::Summary(vec![
                    SummaryCount {
                        quantile: 0.5,
                        count: 4.0
                    },
                    SummaryCount {
                        quantile: 0.99,
                        count: 9.0
                    },
                ]),
            }]
        );
    }

    #[test]
    fn help_and_type_lines_and_values() {
        let scrape = Scrape::parse(
            "# HELP c_total Requests, with = and \"quotes\".\n\
             # TYPE c_total counter\n\
             # TYPE g gauge\n\
             # a free comment\n\
             c_total 3 1700000000000\n\
             g NaN\n\
             u -Inf\n\
             bad notanumber\n",
        );
        assert_eq!(
            scrape.docs.get("c_total").map(String::as_str),
            Some("Requests, with = and \"quotes\".")
        );
        let values: Vec<_> = scrape
            .samples
            .iter()
            .map(|s| (s.metric.as_str(), format!("{:?}", s.value)))
            .collect();
        assert_eq!(
            values,
            vec![
                ("c_total", "Counter(3.0)".to_string()),
                ("g", "Gauge(NaN)".to_string()),
                ("u", "Untyped(-inf)".to_string()),
            ]
        );
    }
}
