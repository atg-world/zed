//! Folds every step between a prompt and its answer into one row (ATG).
//!
//! A turn is a prompt and everything the agent does until the next prompt. The person wants to read the prompt and the answer;
//! the steps in between (tool calls, thinking, and the "let me look at this" lines said before another tool call) are folded into a
//! single row that opens on a click. Anything that needs the person, such as a permission request, is never folded: an agent
//! waiting silently behind a closed row would look like a hang.
//!
//! This file only decides *which* entries are steps. It knows nothing about windows, so it is unit-tested on its own.

/// What an entry is, as far as folding is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    /// the person's prompt: starts a turn
    User,
    /// an assistant message with text to read
    AssistantText,
    /// an assistant message with only thinking, or nothing visible
    AssistantThought,
    /// a tool call that is running, done or failed
    Tool,
    /// a tool call waiting for the person
    ToolNeedsPerson,
    /// anything else (a question to the person, a context summary, a session note): always shown
    Other,
}

/// How one entry is drawn when steps are folded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// drawn as usual
    Shown,
    /// a step of the fold that starts at entry `fold`: hidden until that fold is opened
    Step { fold: usize },
    /// the first step of a fold: draws the row, and the step itself when the fold is open
    Header { steps: usize, last_turn: bool },
}

pub(crate) fn plan(kinds: &[EntryKind]) -> Vec<Slot> {
    let mut slots = vec![Slot::Shown; kinds.len()];
    let mut start = 0;
    while start < kinds.len() {
        let end = kinds[start + 1..]
            .iter()
            .position(|kind| *kind == EntryKind::User)
            .map_or(kinds.len(), |offset| start + 1 + offset);
        plan_turn(kinds, &mut slots, start, end);
        start = end;
    }
    slots
}

fn plan_turn(kinds: &[EntryKind], slots: &mut [Slot], start: usize, end: usize) {
    // Text said before the last tool call is narration ("let me check this"); the answer is what comes after the last tool call.
    let last_tool = (start..end).rev().find(|&i| matches!(kinds[i], EntryKind::Tool | EntryKind::ToolNeedsPerson));
    let is_step = |i: usize| match kinds[i] {
        EntryKind::Tool | EntryKind::AssistantThought => true,
        EntryKind::AssistantText => last_tool.is_some_and(|tool| i < tool),
        EntryKind::User | EntryKind::ToolNeedsPerson | EntryKind::Other => false,
    };
    let steps: Vec<usize> = (start..end).filter(|&i| is_step(i)).collect();
    let Some(&first) = steps.first() else { return };
    for &i in &steps {
        slots[i] = Slot::Step { fold: first };
    }
    slots[first] = Slot::Header { steps: steps.len(), last_turn: end == kinds.len() };
}

#[cfg(test)]
mod tests {
    use super::EntryKind::*;
    use super::*;

    #[test]
    fn everything_between_the_prompt_and_the_answer_is_one_fold() {
        let slots = plan(&[User, AssistantThought, Tool, Tool, AssistantText]);
        assert_eq!(
            slots,
            vec![Slot::Shown, Slot::Header { steps: 3, last_turn: true }, Slot::Step { fold: 1 }, Slot::Step { fold: 1 }, Slot::Shown]
        );
    }

    #[test]
    fn narration_before_a_tool_call_is_folded_but_the_final_text_is_not() {
        let slots = plan(&[User, AssistantText, Tool, AssistantText]);
        assert_eq!(slots[1], Slot::Header { steps: 2, last_turn: true });
        assert_eq!(slots[2], Slot::Step { fold: 1 });
        assert_eq!(slots[3], Slot::Shown, "the answer after the last tool call stays");
    }

    #[test]
    fn a_turn_with_only_an_answer_has_nothing_to_fold() {
        assert_eq!(plan(&[User, AssistantText]), vec![Slot::Shown, Slot::Shown]);
        assert_eq!(plan(&[]), vec![]);
    }

    #[test]
    fn a_turn_still_working_folds_everything_after_the_prompt() {
        let slots = plan(&[User, AssistantText, Tool, Tool]);
        assert_eq!(slots[1], Slot::Header { steps: 3, last_turn: true });
        assert_eq!(slots[3], Slot::Step { fold: 1 });
    }

    #[test]
    fn a_request_for_the_person_is_never_folded() {
        let slots = plan(&[User, Tool, ToolNeedsPerson, Tool, AssistantText]);
        assert_eq!(slots[1], Slot::Header { steps: 2, last_turn: true });
        assert_eq!(slots[2], Slot::Shown, "the permission prompt stays visible");
        assert_eq!(slots[3], Slot::Step { fold: 1 });
    }

    #[test]
    fn questions_and_notes_are_never_folded() {
        let slots = plan(&[User, Tool, Other, AssistantText]);
        assert_eq!(slots[2], Slot::Shown);
    }

    #[test]
    fn each_turn_folds_on_its_own_and_only_the_last_is_marked_last() {
        let slots = plan(&[User, Tool, AssistantText, User, Tool, Tool, AssistantText]);
        assert_eq!(slots[1], Slot::Header { steps: 1, last_turn: false });
        assert_eq!(slots[4], Slot::Header { steps: 2, last_turn: true });
        assert_eq!(slots[5], Slot::Step { fold: 4 });
    }

    #[test]
    fn steps_before_any_prompt_still_fold_as_one_turn() {
        let slots = plan(&[Tool, Tool, AssistantText]);
        assert_eq!(slots[0], Slot::Header { steps: 2, last_turn: true });
    }
}
