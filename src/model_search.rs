use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AmpParameter {
    Gain,
    Bass,
    Mid,
    Treble,
    Presence,
    Master,
}

impl AmpParameter {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gain => "gain",
            Self::Bass => "bass",
            Self::Mid => "mid",
            Self::Treble => "treble",
            Self::Presence => "presence",
            Self::Master => "master",
        }
    }

    fn from_query_token(token: &str) -> Option<Self> {
        match normalized_word(token).as_str() {
            "gain" => Some(Self::Gain),
            "bass" => Some(Self::Bass),
            "mid" | "mids" | "middle" => Some(Self::Mid),
            "treble" | "high" | "highs" => Some(Self::Treble),
            "presence" | "pres" => Some(Self::Presence),
            "master" | "volume" => Some(Self::Master),
            _ => None,
        }
    }

    fn from_capture_token(token: &str) -> Option<Self> {
        Self::from_query_token(token).or_else(|| match normalized_word(token).as_str() {
            "g" => Some(Self::Gain),
            "b" => Some(Self::Bass),
            "m" => Some(Self::Mid),
            "t" => Some(Self::Treble),
            "p" => Some(Self::Presence),
            "mv" => Some(Self::Master),
            _ => None,
        })
    }
}

impl fmt::Display for AmpParameter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NumericConstraint {
    Exact { value: f32 },
    Range { minimum: f32, maximum: f32 },
    AtLeast { minimum: f32 },
    AtMost { maximum: f32 },
}

impl NumericConstraint {
    pub fn matches(self, value: f32) -> bool {
        const EPSILON: f32 = 0.000_1;
        match self {
            Self::Exact { value: expected } => (value - expected).abs() <= EPSILON,
            Self::Range { minimum, maximum } => {
                value + EPSILON >= minimum && value - EPSILON <= maximum
            }
            Self::AtLeast { minimum } => value + EPSILON >= minimum,
            Self::AtMost { maximum } => value - EPSILON <= maximum,
        }
    }
}

impl fmt::Display for NumericConstraint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact { value } => write!(formatter, "{}", display_number(*value)),
            Self::Range { minimum, maximum } => write!(
                formatter,
                "{}–{}",
                display_number(*minimum),
                display_number(*maximum)
            ),
            Self::AtLeast { minimum } => write!(formatter, "≥{}", display_number(*minimum)),
            Self::AtMost { maximum } => write!(formatter, "≤{}", display_number(*maximum)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelSearch {
    pub identity: String,
    pub settings: BTreeMap<AmpParameter, NumericConstraint>,
}

impl ModelSearch {
    pub fn parse(input: &str) -> Result<Self> {
        let normalized = normalize_separators(input);
        let tokens = normalized.split_whitespace().collect::<Vec<_>>();
        if tokens.is_empty() {
            bail!("hub search requires a make/model or setting query");
        }
        let mut consumed = BTreeSet::new();
        let mut settings = BTreeMap::new();
        let mut index = 0;
        while index < tokens.len() {
            let (parameter, embedded) = split_parameter_value(tokens[index], false);
            let Some(parameter) = parameter else {
                index += 1;
                continue;
            };
            let mut value_index = index;
            let value = if let Some(value) = embedded {
                value
            } else {
                let mut candidate = index + 1;
                while candidate < tokens.len()
                    && matches!(
                        normalized_word(tokens[candidate]).as_str(),
                        "on" | "at" | "is" | "set" | "to" | "="
                    )
                {
                    candidate += 1;
                }
                if candidate >= tokens.len() {
                    index += 1;
                    continue;
                }
                value_index = candidate;
                tokens[candidate]
            };
            let constraint = match parse_constraint(value) {
                Ok(constraint) => constraint,
                Err(error) if looks_like_constraint(value) => {
                    return Err(error).with_context(|| format!("invalid {parameter} constraint"));
                }
                Err(_) => {
                    index += 1;
                    continue;
                }
            };
            if settings.insert(parameter, constraint).is_some() {
                bail!("search specifies {parameter} more than once");
            }
            for consumed_index in index..=value_index {
                consumed.insert(consumed_index);
            }
            index = value_index + 1;
        }

        let identity = tokens
            .iter()
            .enumerate()
            .filter(|(index, token)| {
                !consumed.contains(index)
                    && !matches!(
                        normalized_word(token).as_str(),
                        "with" | "and" | "settings" | "setting"
                    )
            })
            .map(|(_, token)| token.trim_matches(|character: char| !character.is_alphanumeric()))
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if identity.is_empty() && settings.is_empty() {
            bail!("hub search did not contain a make/model or recognized setting");
        }
        Ok(Self { identity, settings })
    }

    pub fn matches(&self, capture: &CaptureSettings) -> bool {
        self.settings.iter().all(|(parameter, constraint)| {
            capture
                .values
                .get(parameter)
                .is_some_and(|setting| constraint.matches(setting.value))
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CaptureSetting {
    pub value: f32,
    pub evidence: SettingEvidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SettingEvidence {
    ModelName { fragment: String },
    ToneDescription { fragment: String },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CaptureSettings {
    pub values: BTreeMap<AmpParameter, CaptureSetting>,
}

impl CaptureSettings {
    pub fn from_model(model_name: &str, sole_model_description: Option<&str>) -> Self {
        let mut values = extract_values(model_name, true)
            .into_iter()
            .map(|(parameter, (value, fragment))| {
                (
                    parameter,
                    CaptureSetting {
                        value,
                        evidence: SettingEvidence::ModelName { fragment },
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(description) = sole_model_description {
            for (parameter, (value, fragment)) in extract_values(description, false) {
                values.entry(parameter).or_insert(CaptureSetting {
                    value,
                    evidence: SettingEvidence::ToneDescription { fragment },
                });
            }
        }
        Self { values }
    }

    pub fn compact(&self) -> String {
        let labels = [
            (AmpParameter::Gain, "G"),
            (AmpParameter::Bass, "B"),
            (AmpParameter::Mid, "M"),
            (AmpParameter::Treble, "T"),
            (AmpParameter::Presence, "P"),
            (AmpParameter::Master, "MV"),
        ];
        let rendered = labels
            .into_iter()
            .filter_map(|(parameter, label)| {
                self.values
                    .get(&parameter)
                    .map(|setting| format!("{label}{}", display_number(setting.value)))
            })
            .collect::<Vec<_>>();
        if rendered.is_empty() {
            "SETTINGS UNKNOWN".to_owned()
        } else {
            rendered.join(" · ")
        }
    }
}

fn extract_values(text: &str, compact_aliases: bool) -> BTreeMap<AmpParameter, (f32, String)> {
    let normalized = normalize_separators(text);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let mut values = BTreeMap::new();
    let mut index = 0;
    while index < tokens.len() {
        let (parameter, embedded) = split_parameter_value(tokens[index], compact_aliases);
        let Some(parameter) = parameter else {
            index += 1;
            continue;
        };
        let (value_token, end) = if let Some(value) = embedded {
            (value, index)
        } else {
            let mut candidate = index + 1;
            while candidate < tokens.len()
                && matches!(
                    normalized_word(tokens[candidate]).as_str(),
                    "on" | "at" | "=" | ":"
                )
            {
                candidate += 1;
            }
            if candidate >= tokens.len() {
                index += 1;
                continue;
            }
            (tokens[candidate], candidate)
        };
        if let Ok(NumericConstraint::Exact { value }) = parse_constraint(value_token) {
            let fragment = tokens[index..=end].join(" ");
            values.entry(parameter).or_insert((value, fragment));
            index = end + 1;
        } else {
            index += 1;
        }
    }
    values
}

fn split_parameter_value(
    token: &str,
    compact_aliases: bool,
) -> (Option<AmpParameter>, Option<&str>) {
    let trimmed = token
        .trim_matches(|character: char| matches!(character, ',' | ';' | '(' | ')' | '[' | ']'));
    for separator in ['=', ':'] {
        if let Some((left, right)) = trimmed.split_once(separator) {
            let parameter = if compact_aliases {
                AmpParameter::from_capture_token(left)
            } else {
                AmpParameter::from_query_token(left)
            };
            return (parameter, (!right.is_empty()).then_some(right));
        }
    }
    if compact_aliases {
        let letters = trimmed
            .chars()
            .take_while(|character| character.is_ascii_alphabetic())
            .count();
        if letters > 0 && letters < trimmed.len() {
            let (left, right) = trimmed.split_at(letters);
            if let Some(parameter) = AmpParameter::from_capture_token(left) {
                return (Some(parameter), Some(right));
            }
        }
        (AmpParameter::from_capture_token(trimmed), None)
    } else {
        (AmpParameter::from_query_token(trimmed), None)
    }
}

fn parse_constraint(raw: &str) -> Result<NumericConstraint> {
    let value = raw.trim_matches(|character: char| matches!(character, ',' | ';' | '.'));
    if let Some(minimum) = value.strip_suffix('+') {
        return Ok(NumericConstraint::AtLeast {
            minimum: parse_setting_number(minimum)?,
        });
    }
    if let Some(maximum) = value.strip_prefix("<=").or_else(|| value.strip_prefix('≤')) {
        return Ok(NumericConstraint::AtMost {
            maximum: parse_setting_number(maximum)?,
        });
    }
    if let Some(minimum) = value.strip_prefix(">=").or_else(|| value.strip_prefix('≥')) {
        return Ok(NumericConstraint::AtLeast {
            minimum: parse_setting_number(minimum)?,
        });
    }
    if let Some((minimum, maximum)) = value.split_once('-') {
        let minimum = parse_setting_number(minimum)?;
        let maximum = parse_setting_number(maximum)?;
        if minimum > maximum {
            bail!("setting range minimum is greater than its maximum");
        }
        return Ok(NumericConstraint::Range { minimum, maximum });
    }
    Ok(NumericConstraint::Exact {
        value: parse_setting_number(value)?,
    })
}

fn parse_setting_number(value: &str) -> Result<f32> {
    let parsed = value
        .parse::<f32>()
        .with_context(|| format!("invalid amp setting {value:?}"))?;
    if !parsed.is_finite() || !(0.0..=10.0).contains(&parsed) {
        bail!("amp settings must be numeric values from 0 through 10");
    }
    Ok(parsed)
}

fn looks_like_constraint(value: &str) -> bool {
    value.chars().any(|character| character.is_ascii_digit())
        || value.starts_with(['<', '>', '≤', '≥'])
}

fn normalize_separators(text: &str) -> String {
    let replaced = text
        .replace("..", "-")
        .replace(['–', '—'], "-")
        .replace(['_', ',', ';', '(', ')', '[', ']', '/', '\\'], " ");
    let characters = replaced.chars().collect::<Vec<_>>();
    characters
        .iter()
        .enumerate()
        .map(|(index, character)| {
            if *character == '-'
                && !(index > 0
                    && index + 1 < characters.len()
                    && characters[index - 1].is_ascii_digit()
                    && characters[index + 1].is_ascii_digit())
            {
                ' '
            } else {
                *character
            }
        })
        .collect()
}

fn normalized_word(token: &str) -> String {
    token
        .trim_matches(|character: char| !character.is_alphanumeric() && character != '=')
        .to_ascii_lowercase()
}

pub fn display_number(value: f32) -> String {
    if value.fract().abs() < 0.000_1 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_language_query_becomes_visible_constraints() {
        let search =
            ModelSearch::parse("marshall jcm with bass on 3-4 and mid on 6+ and high on 2-3")
                .unwrap();
        assert_eq!(search.identity, "marshall jcm");
        assert_eq!(
            search.settings[&AmpParameter::Bass],
            NumericConstraint::Range {
                minimum: 3.0,
                maximum: 4.0
            }
        );
        assert_eq!(
            search.settings[&AmpParameter::Mid],
            NumericConstraint::AtLeast { minimum: 6.0 }
        );
        assert_eq!(
            search.settings[&AmpParameter::Treble],
            NumericConstraint::Range {
                minimum: 2.0,
                maximum: 3.0
            }
        );
    }

    #[test]
    fn compact_capture_names_extract_exact_settings() {
        let capture = CaptureSettings::from_model("JCM800_G6_B3_M7_T2_P4", None);
        assert_eq!(capture.compact(), "G6 · B3 · M7 · T2 · P4");
        let dashed = CaptureSettings::from_model("JCM800-Gain-6-Bass-3-Mid-7-Treble-2", None);
        assert_eq!(dashed.compact(), "G6 · B3 · M7 · T2");
    }

    #[test]
    fn sole_model_description_only_fills_missing_settings() {
        let capture =
            CaptureSettings::from_model("JCM Bass 3", Some("Gain 6, bass 9, middle 7, highs 2"));
        assert_eq!(capture.values[&AmpParameter::Bass].value, 3.0);
        assert_eq!(capture.values[&AmpParameter::Gain].value, 6.0);
        assert_eq!(capture.values[&AmpParameter::Mid].value, 7.0);
        assert_eq!(capture.values[&AmpParameter::Treble].value, 2.0);
    }

    #[test]
    fn unknown_and_out_of_range_settings_do_not_match() {
        let search = ModelSearch::parse("JCM bass 3-4 mid 6+").unwrap();
        let missing_mid = CaptureSettings::from_model("JCM_B3", None);
        let outside = CaptureSettings::from_model("JCM_B5_M7", None);
        let exact = CaptureSettings::from_model("JCM_B4_M6", None);
        assert!(!search.matches(&missing_mid));
        assert!(!search.matches(&outside));
        assert!(search.matches(&exact));
        assert!(ModelSearch::parse("JCM bass 11").is_err());
    }

    #[test]
    fn duplicate_parameter_is_rejected() {
        assert!(ModelSearch::parse("JCM treble 2 high 3").is_err());
    }
}
