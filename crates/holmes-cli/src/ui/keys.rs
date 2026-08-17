//! Key-ownership model: which UI surface currently consumes keys, and what Esc does
//! one level at a time. Both key dispatch (inline_ui) and hint text (status line /
//! cards) derive from this single source of truth, so they can never disagree.

/// The surface that owns keyboard input right now, derived from app state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOwner {
    /// Normal prompt editing (idle or type-ahead while a turn runs).
    LineInput,
    /// Slash-command autocomplete is showing (input starts with '/', no space yet).
    SlashMenu,
    /// `@path` file completion is showing; ↑↓/Tab/Esc belong to it.
    FileCompletion,
    /// A permission approval card is up; it consumes keys until resolved.
    PermissionCard,
}

/// What Esc does right now. The ladder steps back one UI layer at a time: an open
/// card first, then a floating completion list, then an in-flight turn, then the
/// text in the prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscStep {
    /// Close the slash menu (falls back to clearing the input, which removes the
    /// '/' prefix and therefore the menu — same observable behavior as before).
    DismissMenu,
    /// Close the `@path` completion list (keeps the typed text, unlike DismissMenu —
    /// the query is real input the user may still want).
    DismissFileCompletion,
    /// Empty the prompt (idle).
    ClearInput,
    /// Cooperatively interrupt the running turn.
    InterruptTurn,
    /// Reject the pending permission request.
    RejectCard,
}

impl EscStep {
    /// Short verb phrase for hint text, e.g. "Esc: interrupt turn".
    pub fn label(&self) -> &'static str {
        match self {
            EscStep::DismissMenu => "dismiss menu",
            EscStep::DismissFileCompletion => "close completion",
            EscStep::ClearInput => "clear input",
            EscStep::InterruptTurn => "interrupt turn",
            EscStep::RejectCard => "reject request",
        }
    }
}

/// Derive the current key owner from app state. The permission card always wins —
/// while it is open every key belongs to it. File completion outranks the slash menu
/// (both can technically match on `/x @y`; the completion is the more local context).
pub fn key_owner(
    permission_card_active: bool,
    slash_menu_active: bool,
    file_completion_active: bool,
) -> KeyOwner {
    if permission_card_active {
        KeyOwner::PermissionCard
    } else if file_completion_active {
        KeyOwner::FileCompletion
    } else if slash_menu_active {
        KeyOwner::SlashMenu
    } else {
        KeyOwner::LineInput
    }
}

/// The Esc step for a given owner + busy state. `busy` only matters for LineInput
/// (Esc clears the prompt when idle, interrupts the turn while it runs); the menu,
/// completion list and card behave the same either way.
pub fn esc_step(owner: KeyOwner, busy: bool) -> EscStep {
    match owner {
        KeyOwner::PermissionCard => EscStep::RejectCard,
        KeyOwner::FileCompletion => EscStep::DismissFileCompletion,
        KeyOwner::SlashMenu => EscStep::DismissMenu,
        KeyOwner::LineInput if busy => EscStep::InterruptTurn,
        KeyOwner::LineInput => EscStep::ClearInput,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_card_owns_keys_above_everything() {
        assert_eq!(key_owner(true, true, true), KeyOwner::PermissionCard);
        assert_eq!(key_owner(true, false, false), KeyOwner::PermissionCard);
        assert_eq!(key_owner(false, true, false), KeyOwner::SlashMenu);
        assert_eq!(key_owner(false, false, true), KeyOwner::FileCompletion);
        assert_eq!(key_owner(false, false, false), KeyOwner::LineInput);
    }

    #[test]
    fn file_completion_outranks_slash_menu() {
        // `/cmd @pa` can satisfy both; the completion list is the local context.
        assert_eq!(key_owner(false, true, true), KeyOwner::FileCompletion);
    }

    #[test]
    fn esc_ladder_steps_back_one_layer_at_a_time() {
        // Card beats completion beats turn beats input; the menu resolves to dismiss.
        assert_eq!(
            esc_step(KeyOwner::PermissionCard, true),
            EscStep::RejectCard
        );
        assert_eq!(
            esc_step(KeyOwner::FileCompletion, true),
            EscStep::DismissFileCompletion
        );
        assert_eq!(esc_step(KeyOwner::SlashMenu, false), EscStep::DismissMenu);
        assert_eq!(esc_step(KeyOwner::LineInput, true), EscStep::InterruptTurn);
        assert_eq!(esc_step(KeyOwner::LineInput, false), EscStep::ClearInput);
    }

    #[test]
    fn every_esc_step_has_hint_text() {
        for step in [
            EscStep::DismissMenu,
            EscStep::DismissFileCompletion,
            EscStep::ClearInput,
            EscStep::InterruptTurn,
            EscStep::RejectCard,
        ] {
            assert!(!step.label().is_empty());
        }
    }
}
