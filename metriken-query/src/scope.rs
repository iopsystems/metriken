//! A query's scope: the sources it reads (journal D1).
//!
//! A scope names sources of an archive, by uuid (which a source's copies
//! keep) or by position among the reader's `ArchiveReader::sources`, each
//! optionally with a span of time. A query given one reads only the named sources'
//! tables, and only their rows within each span; within a process group it
//! unions only the named sources (S1). The caller resolves what it presents
//! (a presented recording, a bridge piece) to the sources and spans to
//! name. Only `ArchiveReader` applies a scope; another source refuses a
//! query that carries one.

/// A source a [`Scope`] names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SourceRef {
    /// Every source with this uuid: a source and its copies.
    Uuid(String),
    /// The source at this position among `sources()` of the reader the
    /// query is sent to. A reader from `flatten_read_groups` numbers only
    /// its group's sources, so a position is not stable across readers;
    /// `Uuid` is.
    Position(usize),
}

/// One entry of a [`Scope`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScopedSource {
    pub source: SourceRef,
    /// The rows read, `[start, end]` in nanoseconds, both inclusive; `None`
    /// reads every row.
    pub span: Option<(u64, u64)>,
}

impl ScopedSource {
    /// `source`, read over `span` (`None` for every row).
    pub fn new(source: SourceRef, span: Option<(u64, u64)>) -> Self {
        Self { source, span }
    }
}

/// The sources a query reads; see the [module](self) docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    sources: Vec<ScopedSource>,
}

impl Scope {
    /// A scope of `sources`. An error when an entry's span ends before it
    /// starts.
    pub fn new(sources: Vec<ScopedSource>) -> Result<Self, String> {
        for s in &sources {
            if let Some((start, end)) = s.span {
                if end < start {
                    return Err(format!(
                        "scope entry {:?} has a span ending at {end} before it starts at {start}",
                        s.source
                    ));
                }
            }
        }
        Ok(Self { sources })
    }

    /// The entries.
    pub fn sources(&self) -> &[ScopedSource] {
        &self.sources
    }

    /// The span read of the source at `position` with `uuid`: `None` when the
    /// scope does not name it, `Some(None)` for every row. An error when two
    /// entries name it, since one source is read over one span.
    #[cfg_attr(not(feature = "archive"), allow(dead_code))]
    pub(crate) fn span_of(
        &self,
        position: usize,
        uuid: Option<&str>,
    ) -> Result<Option<Option<(u64, u64)>>, String> {
        let mut found = None;
        for s in &self.sources {
            let named = match &s.source {
                SourceRef::Uuid(u) => uuid == Some(u.as_str()),
                SourceRef::Position(p) => *p == position,
            };
            if !named {
                continue;
            }
            if found.is_some() {
                return Err(format!(
                    "the scope names the source at position {position} more than once"
                ));
            }
            found = Some(s.span);
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_is_named_once() {
        let scope = Scope::new(vec![
            ScopedSource {
                source: SourceRef::Uuid("u".into()),
                span: Some((1, 2)),
            },
            ScopedSource {
                source: SourceRef::Position(3),
                span: None,
            },
        ])
        .unwrap();
        assert_eq!(scope.span_of(0, Some("u")), Ok(Some(Some((1, 2)))));
        assert_eq!(scope.span_of(3, Some("v")), Ok(Some(None)));
        assert_eq!(scope.span_of(1, Some("v")), Ok(None));
        assert!(scope.span_of(3, Some("u")).is_err());
        assert!(Scope::new(vec![ScopedSource {
            source: SourceRef::Position(0),
            span: Some((2, 1)),
        }])
        .is_err());
    }
}
