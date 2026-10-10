//! A settable quantity in the desired state: a constant number, or an expression in `t`.
//!
//! Every continuous magnitude loadlab can be asked for -- a byte count, a ratio, a rate -- is a
//! [`Setting`], so the same spelling works everywhere: a plain JSON number, or a string such as
//! `"500 + 250*sin(t/286.5)"` evaluated once per tick against the experiment clock. What the
//! expression *means* is decided by the field it is written on and by the [`Dimension`] that field
//! is interpreted in, not by the type: a `Setting` on `io.read_bps` is a byte rate, the same value
//! on `cpu.utilization` is a percentage of a core.
//!
//! The type round-trips in the spelling it was given. A constant is emitted as a JSON number, and
//! a whole number is emitted without a fractional part, so `4096` stays `4096` rather than becoming
//! `4096.0`; an expression is emitted as the string it was written as. That is what lets a client
//! read `GET /state` and write it back unchanged.

use std::{collections::HashMap, collections::hash_map::Entry, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{error::LoadlabError, expression::CompiledExpression};

/// How a [`Setting`]'s units are interpreted when it is parsed and evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dimension {
    /// A byte count or byte rate. Unit suffixes such as `MiB` are expanded, so `1GiB` is a valid
    /// number of bytes.
    Bytes,
    /// A dimensionless quantity: a percentage, a ratio, or an operation rate. The byte-unit table
    /// is deliberately not applied, so `1MiB` is refused rather than read as 1048576 percent.
    Unitless,
}

impl Dimension {
    fn compile(self, source: &str) -> Result<CompiledExpression, LoadlabError> {
        match self {
            Self::Bytes => CompiledExpression::bytes(source),
            Self::Unitless => CompiledExpression::rate(source),
        }
    }
}

/// A constant number or an expression in the experiment clock `t`.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    /// A fixed number, used as-is.
    Constant(f64),
    /// An expression in `t`, evaluated once per tick.
    Expression(String),
}

impl Default for Setting {
    fn default() -> Self {
        Self::Constant(0.0)
    }
}

impl Setting {
    /// A fixed number.
    #[must_use]
    pub fn constant(value: f64) -> Self {
        Self::Constant(value)
    }

    /// The value at `t = 0`, compiling an expression so a malformed one is refused at
    /// configuration time rather than on the controller's first tick.
    ///
    /// A negative or non-finite value is an error. The caller may impose a tighter range on the
    /// returned number (a ratio no greater than one, for instance), which is why the value is
    /// returned rather than only checked.
    pub fn validate(&self, dimension: Dimension, field: &str) -> Result<f64, LoadlabError> {
        let value = match self {
            Self::Constant(value) => *value,
            Self::Expression(source) => dimension.compile(source)?.evaluate(0.0)?,
        };
        if !value.is_finite() || value < 0.0 {
            return Err(LoadlabError::InvalidConfig(format!(
                "{field} must be a finite, non-negative value"
            )));
        }
        Ok(value)
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Constant(value) => write!(f, "{value}"),
            Self::Expression(source) => write!(f, "{source}"),
        }
    }
}

impl Serialize for Setting {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Expression(source) => serializer.serialize_str(source),
            Self::Constant(value) => {
                // A whole number is written without a fractional part, so `4096` round-trips as
                // `4096` and not `4096.0`. Anything else is a float. The bound is 2^53, past which
                // `f64` no longer represents every integer exactly.
                #[allow(clippy::cast_possible_truncation)]
                if value.is_finite()
                    && value.fract() == 0.0
                    && value.abs() < 9_007_199_254_740_992.0
                {
                    serializer.serialize_i64(*value as i64)
                } else {
                    serializer.serialize_f64(*value)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for Setting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(f64),
            Text(String),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Number(value) => Ok(Self::Constant(value)),
            Repr::Text(source) => Ok(Self::Expression(source)),
        }
    }
}

/// Per-controller compilation cache for [`Setting`] expressions.
///
/// Keyed on the dimension and the exact source text, so an expression that does not change is not
/// recompiled on every tick -- putting a parser in the control loop is exactly what this avoids.
/// Two fields with identical text and dimension share one compilation, which is harmless because a
/// compiled expression carries no per-field state.
#[derive(Debug, Default)]
pub struct ExpressionCache {
    entries: HashMap<(Dimension, String), CompiledExpression>,
}

impl ExpressionCache {
    /// The number of compiled expressions currently cached. Test-only.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn cached_len(&self) -> usize {
        self.entries.len()
    }

    /// Evaluate a setting, compiling its expression on first use.
    ///
    /// The result is not clamped: a caller decides whether negative values mean zero (every rate
    /// does) or are a configuration error (none should reach here, because [`Setting::validate`]
    /// checked `t = 0`).
    pub fn evaluate(
        &mut self,
        setting: &Setting,
        dimension: Dimension,
        timestamp: f64,
    ) -> Result<f64, LoadlabError> {
        match setting {
            Setting::Constant(value) => Ok(*value),
            Setting::Expression(source) => {
                let compiled = match self.entries.entry((dimension, source.clone())) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => entry.insert(dimension.compile(source)?),
                };
                compiled.evaluate(timestamp)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Dimension, ExpressionCache, Setting};

    #[test]
    fn a_number_and_an_expression_deserialize_into_the_right_variant() {
        let number: Setting = serde_json::from_str("10").expect("number");
        assert_eq!(number, Setting::Constant(10.0));

        let expression: Setting = serde_json::from_str(r#""10 + t""#).expect("expression");
        assert_eq!(expression, Setting::Expression("10 + t".to_owned()));
    }

    /// A whole constant must not grow a fractional part on the way out, or every integer field in
    /// `/state` would read back as `4096.0` and stop matching what the client sent.
    #[test]
    fn a_whole_constant_serializes_as_an_integer() {
        assert_eq!(
            serde_json::to_string(&Setting::Constant(4096.0)).expect("encode"),
            "4096"
        );
        assert_eq!(
            serde_json::to_string(&Setting::Constant(0.5)).expect("encode"),
            "0.5"
        );
        assert_eq!(
            serde_json::to_string(&Setting::Expression("500 + t".to_owned())).expect("encode"),
            r#""500 + t""#
        );
    }

    #[test]
    fn validate_refuses_a_negative_or_malformed_value() {
        assert!(
            Setting::Constant(-1.0)
                .validate(Dimension::Unitless, "f")
                .is_err()
        );
        assert!(
            Setting::Expression("50 - 100".to_owned())
                .validate(Dimension::Unitless, "f")
                .is_err()
        );
        // A byte-unit suffix is meaningless on a dimensionless field and must be refused, not
        // silently read as a huge rate.
        assert!(
            Setting::Expression("1MiB".to_owned())
                .validate(Dimension::Unitless, "f")
                .is_err()
        );
    }

    #[allow(clippy::float_cmp)]
    #[test]
    fn the_cache_reuses_a_compilation_and_follows_t() {
        let mut cache = ExpressionCache::default();
        let setting = Setting::Expression("10 + t".to_owned());
        assert_eq!(
            cache
                .evaluate(&setting, Dimension::Unitless, 5.0)
                .expect("t=5"),
            15.0
        );
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache
                .evaluate(&setting, Dimension::Unitless, 7.0)
                .expect("t=7"),
            17.0
        );
        assert_eq!(cache.entries.len(), 1, "the expression was recompiled");
    }
}
