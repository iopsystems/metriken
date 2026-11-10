//! The JSON encoding of a result's numbers.
//!
//! A finite value is a JSON number, as serde writes it. A non-finite one is
//! the string `"+Inf"`, `"-Inf"` or `"NaN"`, the spelling the Prometheus
//! HTTP API uses: JSON has no number for them, and serde_json would write
//! `null`, which it then refuses to read back as an `f64`. Either form is
//! read.
//!
//! Used through `#[serde(with = ...)]` on the result types' value, interval
//! and band fields, and on the display types' numbers
//! ([`crate::display`]); one module per field shape.

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// One number of a result, encoded as the module docs say.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Num(f64);

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = self.0;
        if v.is_finite() {
            s.serialize_f64(v)
        } else if v.is_nan() {
            s.serialize_str("NaN")
        } else if v > 0.0 {
            s.serialize_str("+Inf")
        } else {
            s.serialize_str("-Inf")
        }
    }
}

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Num;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#"a number, or "+Inf", "-Inf" or "NaN""#)
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Num, E> {
                Ok(Num(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Num, E> {
                Ok(Num(v as f64))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Num, E> {
                Ok(Num(v as f64))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Num, E> {
                match v {
                    "+Inf" => Ok(Num(f64::INFINITY)),
                    "-Inf" => Ok(Num(f64::NEG_INFINITY)),
                    "NaN" => Ok(Num(f64::NAN)),
                    _ => Err(E::invalid_value(de::Unexpected::Str(v), &self)),
                }
            }
        }
        d.deserialize_any(V)
    }
}

type Pair = (Num, Num);

/// `f64`: one number of a display point.
pub(crate) mod num {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        Num(*v).serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        Num::deserialize(d).map(|n| n.0)
    }
}

/// `Option<f64>`: a display point's uncertainty edge.
pub(crate) mod opt_num {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        v.map(Num).serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(Option::<Num>::deserialize(d)?.map(|n| n.0))
    }
}

/// A series' bands, as the result types hold them.
type Bands = Option<Vec<Option<(f64, f64)>>>;

fn pair((a, b): (f64, f64)) -> Pair {
    (Num(a), Num(b))
}

fn unpair((a, b): Pair) -> (f64, f64) {
    (a.0, b.0)
}

/// `(f64, f64)`: a value's `(timestamp, value)`, or a band.
pub(crate) mod pair {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(v: &(f64, f64), s: S) -> Result<S::Ok, S::Error> {
        super::pair(*v).serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<(f64, f64), D::Error> {
        Pair::deserialize(d).map(unpair)
    }
}

/// `Option<(f64, f64)>`: a sample's interval.
pub(crate) mod opt_pair {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        v: &Option<(f64, f64)>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        v.map(super::pair).serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<(f64, f64)>, D::Error> {
        Ok(Option::<Pair>::deserialize(d)?.map(unpair))
    }
}

/// `Vec<(f64, f64)>`: a series' values.
pub(crate) mod vec_pair {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(v: &[(f64, f64)], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(|p| super::pair(*p)))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<(f64, f64)>, D::Error> {
        Ok(Vec::<Pair>::deserialize(d)?
            .into_iter()
            .map(unpair)
            .collect())
    }
}

/// `Option<Vec<(f64, f64)>>`: a series' intervals.
pub(crate) mod opt_vec_pair {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        v: &Option<Vec<(f64, f64)>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        v.as_ref()
            .map(|v| v.iter().map(|p| super::pair(*p)).collect::<Vec<_>>())
            .serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<(f64, f64)>>, D::Error> {
        Ok(Option::<Vec<Pair>>::deserialize(d)?.map(|v| v.into_iter().map(unpair).collect()))
    }
}

/// `Option<Vec<Option<(f64, f64)>>>`: a series' bands.
pub(crate) mod opt_vec_opt_pair {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(v: &Bands, s: S) -> Result<S::Ok, S::Error> {
        v.as_ref()
            .map(|v| v.iter().map(|p| p.map(super::pair)).collect::<Vec<_>>())
            .serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bands, D::Error> {
        Ok(Option::<Vec<Option<Pair>>>::deserialize(d)?
            .map(|v| v.into_iter().map(|p| p.map(unpair)).collect()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::promql::{MatrixSample, QueryResult, Sample};

    fn metric() -> HashMap<String, String> {
        HashMap::from([("__name__".to_string(), "m".to_string())])
    }

    /// Every non-finite number of a result is its Prometheus string, every
    /// finite one a number, and the JSON reads back to the same values.
    #[test]
    fn non_finite_numbers_are_strings_and_read_back() {
        let values = vec![
            (1.0, f64::INFINITY),
            (2.0, f64::NEG_INFINITY),
            (3.0, f64::NAN),
            (4.0, 0.5),
        ];
        let bands = vec![
            (10.0, f64::INFINITY),
            (f64::NEG_INFINITY, 0.0),
            (f64::NAN, f64::NAN),
            (0.25, 0.75),
        ];
        let result = QueryResult::Matrix {
            result: vec![MatrixSample::new(metric(), values.clone())
                .with_intervals(Some(bands.clone()))
                .with_bands(Some(bands.iter().copied().map(Some).collect()))],
            notices: Vec::new(),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(
            json["result"][0]["values"],
            serde_json::json!([[1.0, "+Inf"], [2.0, "-Inf"], [3.0, "NaN"], [4.0, 0.5]])
        );
        let expected_bands =
            serde_json::json!([[10.0, "+Inf"], ["-Inf", 0.0], ["NaN", "NaN"], [0.25, 0.75]]);
        assert_eq!(json["result"][0]["intervals"], expected_bands);
        assert_eq!(json["result"][0]["bands"], expected_bands);

        let QueryResult::Matrix { result, .. } = serde_json::from_value(json).unwrap() else {
            panic!("not a matrix")
        };
        let same = |a: &[(f64, f64)], b: &[(f64, f64)]| {
            a.len() == b.len()
                && a.iter().zip(b).all(|(x, y)| {
                    (x.0 == y.0 || (x.0.is_nan() && y.0.is_nan()))
                        && (x.1 == y.1 || (x.1.is_nan() && y.1.is_nan()))
                })
        };
        assert!(same(&result[0].values, &values));
        assert!(same(result[0].intervals.as_ref().unwrap(), &bands));
        let read_bands: Vec<(f64, f64)> = result[0]
            .bands
            .as_ref()
            .unwrap()
            .iter()
            .map(|b| b.unwrap())
            .collect();
        assert!(same(&read_bands, &bands));

        let scalar = QueryResult::Scalar {
            result: (1.0, f64::NEG_INFINITY),
            notices: Vec::new(),
        };
        let json = serde_json::to_value(&scalar).unwrap();
        assert_eq!(json["result"], serde_json::json!([1.0, "-Inf"]));
        let QueryResult::Scalar { result, .. } = serde_json::from_value(json).unwrap() else {
            panic!("not a scalar")
        };
        assert_eq!(result, (1.0, f64::NEG_INFINITY));

        let sample =
            Sample::new(metric(), (1.0, f64::INFINITY)).with_interval(Some((2.0, f64::NAN)));
        let json = serde_json::to_value(&sample).unwrap();
        assert_eq!(json["value"], serde_json::json!([1.0, "+Inf"]));
        assert_eq!(json["interval"], serde_json::json!([2.0, "NaN"]));
    }

    /// JSON written before, all numbers (integers included), reads as it
    /// did; a string other than the three, or `null`, does not read.
    #[test]
    fn numbers_read_as_before_and_other_strings_are_refused() {
        let old = r#"{"resultType":"matrix","result":[{"metric":{"__name__":"m"},
            "values":[[1,2],[3.5,-4.25]],"intervals":[[1,3],[0.5,1e3]],
            "bands":[[1,3],null]}]}"#;
        let QueryResult::Matrix { result, .. } = serde_json::from_str(old).unwrap() else {
            panic!("not a matrix")
        };
        assert_eq!(result[0].values, vec![(1.0, 2.0), (3.5, -4.25)]);
        assert_eq!(result[0].intervals, Some(vec![(1.0, 3.0), (0.5, 1000.0)]));
        assert_eq!(result[0].bands, Some(vec![Some((1.0, 3.0)), None]));

        for bad in [r#""inf""#, r#""Infinity""#, "null"] {
            let json = format!(
                r#"{{"resultType":"matrix","result":[{{"metric":{{}},"values":[[1,{bad}]]}}]}}"#
            );
            assert!(
                serde_json::from_str::<QueryResult>(&json).is_err(),
                "{bad} read"
            );
        }
    }
}
