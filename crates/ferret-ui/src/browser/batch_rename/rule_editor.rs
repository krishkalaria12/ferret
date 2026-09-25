use ferret_ops::{
    CaseChange, CaseStyle, ExtensionChange, FindReplace, MatchSyntax, NumberPlacement, Numbering,
    RenameRule, RuleTarget, TextAffix,
};
use gpui::{AppContext as _, Context, Entity, SharedString, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};

use super::BatchRenameDialog;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuleKind {
    FindReplace,
    Prefix,
    Suffix,
    Number,
    Case,
    Extension,
}

impl RuleKind {
    pub(super) const ALL: [Self; 6] = [
        Self::FindReplace,
        Self::Prefix,
        Self::Suffix,
        Self::Number,
        Self::Case,
        Self::Extension,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::FindReplace => "Find & replace",
            Self::Prefix => "Prefix",
            Self::Suffix => "Suffix",
            Self::Number => "Number",
            Self::Case => "Case",
            Self::Extension => "Extension",
        }
    }
}

pub(super) struct RuleEditor {
    pub(super) kind: RuleKind,
    pub(super) find: Entity<InputState>,
    pub(super) replacement: Entity<InputState>,
    pub(super) text: Entity<InputState>,
    pub(super) start: Entity<InputState>,
    pub(super) step: Entity<InputState>,
    pub(super) width: Entity<InputState>,
    pub(super) syntax: MatchSyntax,
    pub(super) case_sensitive: bool,
    pub(super) target: RuleTarget,
    pub(super) case_style: CaseStyle,
    pub(super) placement: NumberPlacement,
    pub(super) error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl RuleEditor {
    pub(super) fn new(
        kind: RuleKind,
        window: &mut Window,
        cx: &mut Context<BatchRenameDialog>,
    ) -> Self {
        let find = input(window, cx, "Find", "");
        let replacement = input(window, cx, "Replace with", "");
        let text = input(
            window,
            cx,
            match kind {
                RuleKind::Extension => "New extension",
                _ => "Text",
            },
            "",
        );
        let start = input(window, cx, "Start", "1");
        let step = input(window, cx, "Step", "1");
        let width = input(window, cx, "Digits", "3");
        let subscriptions = [&find, &replacement, &text, &start, &step, &width]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, _: &InputEvent, cx| {
                    this.schedule_preview(cx)
                })
            })
            .collect();
        Self {
            kind,
            find,
            replacement,
            text,
            start,
            step,
            width,
            syntax: MatchSyntax::Literal,
            case_sensitive: false,
            target: RuleTarget::Stem,
            case_style: CaseStyle::Lower,
            placement: NumberPlacement::Suffix,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn record(&self, cx: &gpui::App) -> std::result::Result<RenameRule, SharedString> {
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        match self.kind {
            RuleKind::FindReplace => {
                let find = value(&self.find);
                if find.is_empty() {
                    return Err(SharedString::from("Enter text or a pattern to find."));
                }
                Ok(RenameRule::FindReplace(FindReplace {
                    find,
                    replacement: value(&self.replacement),
                    syntax: self.syntax,
                    case_sensitive: self.case_sensitive,
                    target: self.target,
                }))
            }
            RuleKind::Prefix | RuleKind::Suffix => {
                let text = value(&self.text);
                if text.is_empty() {
                    return Err(SharedString::from(match self.kind {
                        RuleKind::Prefix => "Enter a prefix.",
                        _ => "Enter a suffix.",
                    }));
                }
                let rule = TextAffix {
                    value: text,
                    target: self.target,
                };
                Ok(if self.kind == RuleKind::Prefix {
                    RenameRule::Prefix(rule)
                } else {
                    RenameRule::Suffix(rule)
                })
            }
            RuleKind::Number => {
                let start = parse_number(&value(&self.start), "Start")?;
                let step = parse_number(&value(&self.step), "Step")?;
                let width = parse_number(&value(&self.width), "Digits")?;
                if step == 0 {
                    return Err(SharedString::from("Step cannot be zero."));
                }
                if !(1..=ferret_ops::config::MAX_NUMBER_WIDTH as i64).contains(&width) {
                    return Err(SharedString::from(format!(
                        "Digits must be from 1 to {}.",
                        ferret_ops::config::MAX_NUMBER_WIDTH
                    )));
                }
                Ok(RenameRule::Number(Numbering {
                    start,
                    step,
                    width: width as usize,
                    placement: self.placement,
                }))
            }
            RuleKind::Case => Ok(RenameRule::Case(CaseChange {
                style: self.case_style,
                target: self.target,
            })),
            RuleKind::Extension => {
                let extension = value(&self.text);
                if extension.trim().trim_start_matches('.').is_empty() {
                    return Err(SharedString::from("Enter a new extension."));
                }
                Ok(RenameRule::Extension(ExtensionChange { extension }))
            }
        }
    }
}

fn input(
    window: &mut Window,
    cx: &mut Context<BatchRenameDialog>,
    placeholder: &str,
    value: &str,
) -> Entity<InputState> {
    cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder(placeholder);
        state.set_value(value, window, cx);
        state
    })
}

fn parse_number(value: &str, label: &str) -> std::result::Result<i64, SharedString> {
    value
        .trim()
        .parse()
        .map_err(|_| SharedString::from(format!("{label} must be a whole number.")))
}
