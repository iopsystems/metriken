//! The encoding of a struct whose last fields were added after it shipped.
//!
//! rmp-serde writes a struct as a positional array, so a decoder from before
//! a field existed reads an array of the old length and refuses a longer one.
//! A trailing field that is empty is left out so the struct encodes as it did
//! before the field existed. Leaving out an empty field that has a non-empty
//! field after it would shift the later field into its position, where an
//! older decoder reads it as the earlier kind (a float descriptor list read
//! as gauge histogram descriptors, a row of `None` float slots read as
//! gauge histogram slots). So every trailing field up to the last non-empty
//! one is written, the empty ones before it included, and only the empty
//! fields after it are left out.
//!
//! Decoding is serde's derived `Deserialize` with `#[serde(default)]` on each
//! trailing field, which takes any array length from the required fields'
//! count to the full one.

/// How many of the trailing fields to write, given whether each is
/// non-empty, in declaration order: up to and including the last non-empty
/// one.
pub(crate) fn written(non_empty: &[bool]) -> usize {
    non_empty.iter().rposition(|&b| b).map_or(0, |i| i + 1)
}

/// `impl Serialize` for a struct whose `required` fields are always written
/// and whose `trailing` fields (each with an `is_empty()`) are written up to
/// the last non-empty one. Field names are the Rust identifiers, as serde's
/// derive gives them for a struct without `rename`.
macro_rules! serialize_with_trailing {
    ($ty:ident { $($req:ident),+ $(,)? } trailing { $($opt:ident),+ $(,)? }) => {
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeStruct;
                let required = [$(stringify!($req)),+].len();
                let written = $crate::trailing::written(&[$(!self.$opt.is_empty()),+]);
                let mut st = serializer.serialize_struct(stringify!($ty), required + written)?;
                $(st.serialize_field(stringify!($req), &self.$req)?;)+
                let mut at = 0;
                $(
                    if at < written {
                        st.serialize_field(stringify!($opt), &self.$opt)?;
                    } else {
                        st.skip_field(stringify!($opt))?;
                    }
                    at += 1;
                )+
                debug_assert_eq!(at, [$(stringify!($opt)),+].len());
                st.end()
            }
        }
    };
}

pub(crate) use serialize_with_trailing;

#[cfg(test)]
mod tests {
    use super::written;

    #[test]
    fn writes_up_to_the_last_non_empty_field() {
        assert_eq!(written(&[false, false, false]), 0);
        assert_eq!(written(&[true, false, false]), 1);
        assert_eq!(written(&[false, true, false]), 2);
        assert_eq!(written(&[false, false, true]), 3);
        assert_eq!(written(&[true, false, true]), 3);
    }
}
