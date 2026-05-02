use frankenterm_core::osc_protocol_omnibus::{
    HyperlinkAllocOutcome, HyperlinkId, HyperlinkRegistry, HyperlinkUri, Osc8HoverDecision,
    Osc8HoverPolicy, osc8_hover_decision,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiOsc8HoverAction {
    NotOverHyperlink,
    ShowStatus { id: HyperlinkId, uri: String },
    Suppress { id: HyperlinkId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiOsc8ClickAction {
    NotOverHyperlink,
    OpenUrl { id: HyperlinkId, uri: String },
    SelectSpan { id: HyperlinkId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiOsc8A11yAnnouncement {
    None,
    LinkTarget { id: HyperlinkId, text: String },
}

#[must_use]
pub fn register_gui_hyperlink(
    registry: &mut HyperlinkRegistry,
    raw_uri: impl Into<String>,
) -> Option<HyperlinkId> {
    match registry.allocate(HyperlinkUri::new(raw_uri.into())) {
        HyperlinkAllocOutcome::Allocated(id) => Some(id),
        HyperlinkAllocOutcome::DeniedFull => None,
    }
}

#[must_use]
pub fn resolve_hover_action(
    registry: &HyperlinkRegistry,
    id: HyperlinkId,
    policy: Osc8HoverPolicy,
) -> GuiOsc8HoverAction {
    let Some(uri) = registry.resolve(id) else {
        return GuiOsc8HoverAction::NotOverHyperlink;
    };

    match osc8_hover_decision(uri, policy) {
        Osc8HoverDecision::ShowStatus => GuiOsc8HoverAction::ShowStatus {
            id,
            uri: uri.raw.clone(),
        },
        Osc8HoverDecision::Suppress => GuiOsc8HoverAction::Suppress { id },
    }
}

#[must_use]
pub fn resolve_click_action(
    registry: &HyperlinkRegistry,
    id: HyperlinkId,
    selection_modifier_held: bool,
) -> GuiOsc8ClickAction {
    let Some(uri) = registry.resolve(id) else {
        return GuiOsc8ClickAction::NotOverHyperlink;
    };

    if selection_modifier_held {
        GuiOsc8ClickAction::SelectSpan { id }
    } else {
        GuiOsc8ClickAction::OpenUrl {
            id,
            uri: uri.raw.clone(),
        }
    }
}

#[must_use]
pub fn resolve_a11y_announcement(
    registry: &HyperlinkRegistry,
    id: HyperlinkId,
) -> GuiOsc8A11yAnnouncement {
    let Some(uri) = registry.resolve(id) else {
        return GuiOsc8A11yAnnouncement::None;
    };

    GuiOsc8A11yAnnouncement::LinkTarget {
        id,
        text: format!("Link to {}", uri.raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(raw_uri: &str) -> (HyperlinkRegistry, HyperlinkId) {
        let mut registry = HyperlinkRegistry::new(8);
        let id = register_gui_hyperlink(&mut registry, raw_uri).expect("link is registered");
        (registry, id)
    }

    #[test]
    fn hover_action_uses_well_known_policy() {
        let (registry, id) = registry_with("https://example.com/docs");

        assert_eq!(
            resolve_hover_action(&registry, id, Osc8HoverPolicy::WellKnownOnly),
            GuiOsc8HoverAction::ShowStatus {
                id,
                uri: "https://example.com/docs".to_string(),
            }
        );

        let (registry, id) = registry_with("ssh://example.com/repo");

        assert_eq!(
            resolve_hover_action(&registry, id, Osc8HoverPolicy::WellKnownOnly),
            GuiOsc8HoverAction::Suppress { id }
        );
        assert_eq!(
            resolve_hover_action(&registry, id, Osc8HoverPolicy::AllSchemes),
            GuiOsc8HoverAction::ShowStatus {
                id,
                uri: "ssh://example.com/repo".to_string(),
            }
        );
    }

    #[test]
    fn click_action_opens_or_selects_registered_link() {
        let (registry, id) = registry_with("mailto:operator@example.com");

        assert_eq!(
            resolve_click_action(&registry, id, false),
            GuiOsc8ClickAction::OpenUrl {
                id,
                uri: "mailto:operator@example.com".to_string(),
            }
        );
        assert_eq!(
            resolve_click_action(&registry, id, true),
            GuiOsc8ClickAction::SelectSpan { id }
        );
    }

    #[test]
    fn none_or_unresolved_link_is_noop() {
        let registry = HyperlinkRegistry::new(8);

        assert_eq!(
            resolve_hover_action(&registry, HyperlinkId::NONE, Osc8HoverPolicy::AllSchemes),
            GuiOsc8HoverAction::NotOverHyperlink
        );
        assert_eq!(
            resolve_click_action(&registry, HyperlinkId(7), false),
            GuiOsc8ClickAction::NotOverHyperlink
        );
        assert_eq!(
            resolve_a11y_announcement(&registry, HyperlinkId::NONE),
            GuiOsc8A11yAnnouncement::None
        );
    }

    #[test]
    fn a11y_announcement_names_link_target() {
        let (registry, id) = registry_with("https://example.com/a11y");

        assert_eq!(
            resolve_a11y_announcement(&registry, id),
            GuiOsc8A11yAnnouncement::LinkTarget {
                id,
                text: "Link to https://example.com/a11y".to_string(),
            }
        );
    }

    #[test]
    fn full_registry_refuses_more_gui_links() {
        let mut registry = HyperlinkRegistry::new(1);

        assert_eq!(
            register_gui_hyperlink(&mut registry, "https://example.com/one"),
            Some(HyperlinkId(1))
        );
        assert_eq!(
            register_gui_hyperlink(&mut registry, "https://example.com/two"),
            None
        );
    }
}
