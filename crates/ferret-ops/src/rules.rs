use convert_case::{Case, Casing};
use regex::{NoExpand, Regex, RegexBuilder};

use crate::config::{MAX_NUMBER_WIDTH, MAX_RULES};
use crate::{BatchRenameError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleTarget {
    Stem,
    FullName,
    Extension,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchSyntax {
    Literal,
    Regex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberPlacement {
    Prefix,
    Suffix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseStyle {
    Lower,
    Upper,
    Title,
    Kebab,
    Snake,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindReplace {
    pub find: String,
    pub replacement: String,
    pub syntax: MatchSyntax,
    pub case_sensitive: bool,
    pub target: RuleTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextAffix {
    pub value: String,
    pub target: RuleTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Numbering {
    pub start: i64,
    pub step: i64,
    pub width: usize,
    pub placement: NumberPlacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaseChange {
    pub style: CaseStyle,
    pub target: RuleTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionChange {
    pub extension: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameRule {
    FindReplace(FindReplace),
    Prefix(TextAffix),
    Suffix(TextAffix),
    Number(Numbering),
    Case(CaseChange),
    Extension(ExtensionChange),
}

#[derive(Debug)]
pub(crate) enum TransformError {
    NumberOverflow,
}

struct CompiledRule<'a> {
    rule: &'a RenameRule,
    replacement: Option<(Regex, MatchSyntax)>,
}

pub(crate) struct CompiledRules<'a> {
    rules: Vec<CompiledRule<'a>>,
}

impl<'a> CompiledRules<'a> {
    pub(crate) fn new(rules: &'a [RenameRule]) -> Result<Self> {
        if rules.len() > MAX_RULES {
            return Err(BatchRenameError::InvalidRule {
                rule_index: MAX_RULES,
                message: format!("Ferret supports at most {MAX_RULES} rules in one batch."),
            });
        }

        let compiled = rules
            .iter()
            .enumerate()
            .map(|(rule_index, rule)| {
                let replacement = match rule {
                    RenameRule::FindReplace(rule) => {
                        if rule.find.is_empty() {
                            return Err(invalid(
                                rule_index,
                                "Find and replace needs text or a regular expression to find.",
                            ));
                        }
                        let expression = match rule.syntax {
                            MatchSyntax::Literal => regex::escape(&rule.find),
                            MatchSyntax::Regex => rule.find.clone(),
                        };
                        let regex = RegexBuilder::new(&expression)
                            .case_insensitive(!rule.case_sensitive)
                            .build()
                            .map_err(|error| {
                                invalid(
                                    rule_index,
                                    format!("The regular expression is invalid: {error}"),
                                )
                            })?;
                        Some((regex, rule.syntax))
                    }
                    RenameRule::Prefix(rule) if rule.value.is_empty() => {
                        return Err(invalid(rule_index, "Prefix cannot be empty."));
                    }
                    RenameRule::Suffix(rule) if rule.value.is_empty() => {
                        return Err(invalid(rule_index, "Suffix cannot be empty."));
                    }
                    RenameRule::Number(rule) if rule.step == 0 => {
                        return Err(invalid(rule_index, "Number step cannot be zero."));
                    }
                    RenameRule::Number(rule)
                        if rule.width == 0 || rule.width > MAX_NUMBER_WIDTH =>
                    {
                        return Err(invalid(
                            rule_index,
                            format!("Number width must be from 1 to {MAX_NUMBER_WIDTH}."),
                        ));
                    }
                    RenameRule::Extension(rule)
                        if rule.extension.trim().trim_start_matches('.').is_empty() =>
                    {
                        return Err(invalid(rule_index, "New extension cannot be empty."));
                    }
                    _ => None,
                };
                Ok(CompiledRule { rule, replacement })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { rules: compiled })
    }

    pub(crate) fn transform(
        &self,
        name: &str,
        index: usize,
    ) -> std::result::Result<String, TransformError> {
        let mut output = name.to_string();
        for compiled in &self.rules {
            output = apply_rule(&output, index, compiled)?;
        }
        Ok(output)
    }
}

fn invalid(rule_index: usize, message: impl Into<String>) -> BatchRenameError {
    BatchRenameError::InvalidRule {
        rule_index,
        message: message.into(),
    }
}

fn apply_rule(
    name: &str,
    index: usize,
    compiled: &CompiledRule<'_>,
) -> std::result::Result<String, TransformError> {
    match compiled.rule {
        RenameRule::FindReplace(rule) => {
            let Some((regex, syntax)) = &compiled.replacement else {
                return Ok(name.to_string());
            };
            Ok(map_target(name, rule.target, |part| {
                if *syntax == MatchSyntax::Literal {
                    regex
                        .replace_all(part, NoExpand(&rule.replacement))
                        .into_owned()
                } else {
                    regex
                        .replace_all(part, rule.replacement.as_str())
                        .into_owned()
                }
            }))
        }
        RenameRule::Prefix(rule) => Ok(map_target(name, rule.target, |part| {
            format!("{}{part}", rule.value)
        })),
        RenameRule::Suffix(rule) => Ok(map_target(name, rule.target, |part| {
            format!("{part}{}", rule.value)
        })),
        RenameRule::Number(rule) => {
            let index = i64::try_from(index).map_err(|_| TransformError::NumberOverflow)?;
            let offset = index
                .checked_mul(rule.step)
                .ok_or(TransformError::NumberOverflow)?;
            let value = rule
                .start
                .checked_add(offset)
                .ok_or(TransformError::NumberOverflow)?;
            let sign = if value < 0 { "-" } else { "" };
            let digits = value.unsigned_abs().to_string();
            let width = rule.width.saturating_sub(sign.len());
            let number = format!("{sign}{digits:0>width$}");
            Ok(map_target(name, RuleTarget::Stem, |stem| {
                match rule.placement {
                    NumberPlacement::Prefix => format!("{number}{stem}"),
                    NumberPlacement::Suffix => format!("{stem}{number}"),
                }
            }))
        }
        RenameRule::Case(rule) => Ok(map_target(name, rule.target, |part| match rule.style {
            CaseStyle::Lower => part.to_lowercase(),
            CaseStyle::Upper => part.to_uppercase(),
            CaseStyle::Title => part.to_case(Case::Title),
            CaseStyle::Kebab => part.to_case(Case::Kebab),
            CaseStyle::Snake => part.to_case(Case::Snake),
        })),
        RenameRule::Extension(rule) => {
            let extension = rule.extension.trim().trim_start_matches('.');
            let (stem, _) = split_name(name);
            Ok(format!("{stem}.{extension}"))
        }
    }
}

fn map_target(name: &str, target: RuleTarget, transform: impl FnOnce(&str) -> String) -> String {
    let (stem, extension) = split_name(name);
    match target {
        RuleTarget::Stem => {
            let transformed = transform(stem);
            match extension {
                Some(extension) => format!("{transformed}.{extension}"),
                None => transformed,
            }
        }
        RuleTarget::FullName => transform(name),
        RuleTarget::Extension => extension.map(transform).unwrap_or_else(|| name.to_string()),
    }
}

pub(crate) fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], Some(&name[index + 1..])),
        _ => (name, None),
    }
}

pub fn validate_file_name(name: &str) -> std::result::Result<(), &'static str> {
    ferret_vfs::validate_portable_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> ferret_vfs::VfsPath {
        ferret_vfs::VfsPath::local(format!("/tmp/{name}"))
    }

    fn transform(name: &str, rules: &[RenameRule]) -> String {
        CompiledRules::new(rules)
            .unwrap()
            .transform(name, 0)
            .unwrap()
    }

    #[test]
    fn rules_are_applied_in_order() {
        let rules = [
            RenameRule::FindReplace(FindReplace {
                find: "IMG_".into(),
                replacement: String::new(),
                syntax: MatchSyntax::Literal,
                case_sensitive: true,
                target: RuleTarget::Stem,
            }),
            RenameRule::Prefix(TextAffix {
                value: "trip-".into(),
                target: RuleTarget::Stem,
            }),
            RenameRule::Case(CaseChange {
                style: CaseStyle::Title,
                target: RuleTarget::Stem,
            }),
            RenameRule::Suffix(TextAffix {
                value: "-".into(),
                target: RuleTarget::Stem,
            }),
            RenameRule::Number(Numbering {
                start: 1,
                step: 1,
                width: 3,
                placement: NumberPlacement::Suffix,
            }),
            RenameRule::Extension(ExtensionChange {
                extension: ".bak".into(),
            }),
        ];

        assert_eq!(
            transform(path("IMG_10.txt").file_name().unwrap(), &rules),
            "Trip 10-001.bak"
        );
    }

    #[test]
    fn regex_replacement_uses_capture_groups() {
        let rules = [RenameRule::FindReplace(FindReplace {
            find: r"^(\d{4})-(\d{2})$".into(),
            replacement: "${2}_${1}".into(),
            syntax: MatchSyntax::Regex,
            case_sensitive: true,
            target: RuleTarget::Stem,
        })];
        assert_eq!(transform("2026-09", &rules), "09_2026");
    }

    #[test]
    fn literal_replacement_does_not_expand_dollar_signs() {
        let rules = [RenameRule::FindReplace(FindReplace {
            find: "old".into(),
            replacement: "$1".into(),
            syntax: MatchSyntax::Literal,
            case_sensitive: true,
            target: RuleTarget::FullName,
        })];
        assert_eq!(transform("old.txt", &rules), "$1.txt");
    }

    #[test]
    fn dotfiles_keep_their_leading_dot_as_their_stem() {
        let (stem, extension) = split_name(".gitignore");
        assert_eq!(stem, ".gitignore");
        assert_eq!(extension, None);
    }

    #[test]
    fn windows_device_names_are_rejected_with_extensions() {
        assert_eq!(
            validate_file_name("CON"),
            Err("is a reserved device name on Windows")
        );
        assert_eq!(
            validate_file_name("nul.txt"),
            Err("is a reserved device name on Windows")
        );
        assert_eq!(
            validate_file_name("COM¹.txt"),
            Err("is a reserved device name on Windows")
        );
        assert!(validate_file_name("COM+1.txt").is_ok());
        assert!(validate_file_name("console.txt").is_ok());
    }

    #[test]
    fn unsafe_names_are_rejected_before_apply() {
        assert!(validate_file_name("a/b").is_err());
        assert!(validate_file_name("a\\b").is_err());
        assert!(validate_file_name("report.").is_err());
        assert!(validate_file_name("report ").is_err());
        assert!(validate_file_name("").is_err());
    }
}
