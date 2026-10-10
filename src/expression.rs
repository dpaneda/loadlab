use std::str::FromStr;

use meval::Expr;

use crate::error::LoadlabError;

#[derive(Debug, Clone)]
pub struct CompiledExpression {
    source: String,
    expr: Expr,
}

impl CompiledExpression {
    /// Compile an expression whose result is a number of bytes.
    pub fn bytes(source: &str) -> Result<Self, LoadlabError> {
        Self::compile(source, normalize_units)
    }

    /// Compile an expression whose result is dimensionless, such as a CPU utilization
    /// percentage.
    ///
    /// The byte unit table is deliberately not applied here. `1MiB` is a perfectly valid
    /// arithmetic expression, so it would parse without complaint and then be read as a
    /// utilization of 1048576 percent -- an error ten million times too large, in a
    /// controller that would then happily ask for it. Refusing it at configuration time
    /// costs one line here and turns a silent wrong answer into a 400.
    pub fn percent(source: &str) -> Result<Self, LoadlabError> {
        Self::dimensionless(source)
    }

    /// Compile a dimensionless rate expression, such as a network operations-per-second target.
    ///
    /// Identical to [`Self::percent`] in what it accepts -- the unit table is not applied, because
    /// an operation rate is not a byte count either. It exists as a separate name so a reader of
    /// the network controller is not told it is computing a percentage.
    pub fn rate(source: &str) -> Result<Self, LoadlabError> {
        Self::percent(source)
    }

    fn dimensionless(source: &str) -> Result<Self, LoadlabError> {
        Self::compile(source, |input| Ok(input.to_owned()))
    }

    fn compile(
        source: &str,
        normalize: impl Fn(&str) -> Result<String, LoadlabError>,
    ) -> Result<Self, LoadlabError> {
        let normalized = normalize(source)?;
        let expr = Expr::from_str(&normalized)
            .map_err(|error| LoadlabError::InvalidExpression(format!("{source:?}: {error}")))?;

        // Validate that `t` binds now, so a missing variable is reported at configuration time
        // rather than on the first controller tick.
        let _ = expr
            .clone()
            .bind("t")
            .map_err(|error| LoadlabError::InvalidExpression(format!("{source:?}: {error}")))?;

        Ok(Self {
            source: source.to_owned(),
            expr,
        })
    }

    pub fn evaluate(&self, elapsed_seconds: f64) -> Result<f64, LoadlabError> {
        let function = self
            .expr
            .clone()
            .bind("t")
            .map_err(|error| LoadlabError::InvalidExpression(error.to_string()))?;
        let value = function(elapsed_seconds);
        if !value.is_finite() {
            return Err(LoadlabError::InvalidExpression(format!(
                "expression {:?} evaluated to a non-finite value at t={elapsed_seconds}",
                self.source
            )));
        }
        Ok(value)
    }
}

/// Replace human-friendly numeric suffixes with plain floating point constants.
///
/// Supported units are decimal B/KB/MB/GB/TB and binary KiB/MiB/GiB/TiB.
/// The parser intentionally operates only on numeric tokens immediately followed
/// by a unit so that arbitrary expression syntax is still handled by `meval`.
fn normalize_units(input: &str) -> Result<String, LoadlabError> {
    let mut output = String::with_capacity(input.len() + 16);
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_digit() || bytes[i] == b'.' {
            let number_start = i;
            let mut saw_digit = false;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                saw_digit = true;
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    saw_digit = true;
                    i += 1;
                }
            }
            if !saw_digit {
                return Err(LoadlabError::InvalidExpression(format!(
                    "invalid number near {:?}",
                    &input[number_start..]
                )));
            }

            let number = &input[number_start..i];
            let unit_start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let unit = &input[unit_start..i];
            let multiplier = match unit.to_ascii_lowercase().as_str() {
                "" => None,
                "b" => Some(1.0),
                "kb" => Some(1_000.0),
                "mb" => Some(1_000_000.0),
                "gb" => Some(1_000_000_000.0),
                "tb" => Some(1_000_000_000_000.0),
                "kib" => Some(1_024.0),
                "mib" => Some(1_048_576.0),
                "gib" => Some(1_073_741_824.0),
                "tib" => Some(1_099_511_627_776.0),
                _ => {
                    return Err(LoadlabError::InvalidExpression(format!(
                        "unknown unit {unit:?}"
                    )));
                }
            };

            match multiplier {
                Some(multiplier) => {
                    output.push('(');
                    output.push_str(number);
                    output.push('*');
                    output.push_str(&multiplier.to_string());
                    output.push(')');
                }
                None => output.push_str(number),
            }
        } else {
            let ch = input[i..].chars().next().ok_or_else(|| {
                LoadlabError::InvalidExpression("invalid UTF-8 boundary".to_owned())
            })?;
            output.push(ch);
            i += ch.len_utf8();
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::CompiledExpression;

    #[test]
    fn evaluates_byte_units() {
        let expr = CompiledExpression::bytes("500MB + 100MB").unwrap();
        assert_eq!(expr.evaluate(0.0).unwrap(), 600_000_000.0);
    }

    #[test]
    fn evaluates_time_variable_and_functions() {
        let expr = CompiledExpression::bytes("500MB + 100MB * sin(t)").unwrap();
        let value = expr.evaluate(std::f64::consts::PI / 2.0).unwrap();
        assert!((value - 600_000_000.0).abs() < 0.5);
    }

    #[test]
    fn rejects_unknown_units() {
        assert!(CompiledExpression::bytes("1XB").is_err());
    }

    /// A CPU target is a percentage, not a byte count. `1MiB` parses as arithmetic, so
    /// without this it would be accepted and then read as 1048576 percent.
    #[test]
    fn percent_refuses_byte_units_instead_of_guessing() {
        let error = CompiledExpression::percent("1MiB").expect_err("bytes are not a percent");
        // meval itself rejects the identifier, which is enough: the point is that the
        // byte table is not applied, so no silent multiplication by 1048576 happens.
        assert!(error.to_string().contains("1MiB"), "{error}");

        // And the same text is perfectly valid for a byte target, which is what makes
        // refusing it here a real distinction rather than a general parsing failure.
        assert!(CompiledExpression::bytes("1MiB").is_ok());
    }

    #[test]
    fn percent_evaluates_plain_arithmetic_and_functions() {
        let expr = CompiledExpression::percent("100 + 50*sin(t/10)").expect("valid");
        assert_eq!(expr.evaluate(0.0).expect("t=0"), 100.0);
        let peak = expr
            .evaluate(std::f64::consts::FRAC_PI_2 * 10.0)
            .expect("at the peak");
        assert!((peak - 150.0).abs() < 0.5, "peak was {peak}");
    }

    /// A network rate is dimensionless exactly like a CPU target: `1MiB` parses as arithmetic
    /// and would be read as a million operations per second, so it has to be refused too.
    #[test]
    fn rate_is_dimensionless_like_percent() {
        let expr = CompiledExpression::rate("500 + 250*sin(t/286.5)").expect("valid");
        assert_eq!(expr.evaluate(0.0).expect("t=0"), 500.0);
        assert!(CompiledExpression::rate("1MiB").is_err());
    }

    /// A dimensionless target can still use unitless shorthand, which is what an operator
    /// reaches for first.
    #[test]
    fn percent_accepts_unitless_suffixes() {
        let expr = CompiledExpression::percent("1e2").expect("scientific notation");
        assert_eq!(expr.evaluate(0.0).expect("t=0"), 100.0);
    }
}
