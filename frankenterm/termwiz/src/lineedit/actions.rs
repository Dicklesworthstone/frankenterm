pub type RepeatCount = usize;

#[derive(Debug, Clone, Copy)]
pub enum Movement {
    BackwardChar(RepeatCount),
    BackwardWord(RepeatCount),
    ForwardChar(RepeatCount),
    ForwardWord(RepeatCount),
    StartOfLine,
    EndOfLine,
    None,
}

#[derive(Debug, Clone)]
pub enum Action {
    AcceptLine,
    Cancel,
    EndOfFile,
    InsertChar(RepeatCount, char),
    InsertText(RepeatCount, String),
    Repaint,
    Move(Movement),
    Kill(Movement),
    KillAndMove(Movement, Movement),
    HistoryPrevious,
    HistoryNext,
    Complete,
    NoAction,
    HistoryIncSearchBackwards,
    HistoryIncSearchForwards,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Movement ──────────────────────────────────────

    #[test]
    fn movement_backward_char() {
        let m = Movement::BackwardChar(3);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("BackwardChar(3)"));
    }

    #[test]
    fn movement_backward_word() {
        let m = Movement::BackwardWord(1);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("BackwardWord(1)"));
    }

    #[test]
    fn movement_forward_char() {
        let m = Movement::ForwardChar(5);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("ForwardChar(5)"));
    }

    #[test]
    fn movement_forward_word() {
        let m = Movement::ForwardWord(2);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("ForwardWord(2)"));
    }

    #[test]
    fn movement_start_of_line() {
        let m = Movement::StartOfLine;
        assert!(format!("{:?}", m).contains("StartOfLine"));
    }

    #[test]
    fn movement_end_of_line() {
        let m = Movement::EndOfLine;
        assert!(format!("{:?}", m).contains("EndOfLine"));
    }

    #[test]
    fn movement_none() {
        let m = Movement::None;
        assert!(format!("{:?}", m).contains("None"));
    }

    #[test]
    fn movement_clone_copy() {
        let m = Movement::ForwardChar(7);
        let copied = m;
        assert!(format!("{:?}", m) == format!("{:?}", copied));
    }

    // ── Action ────────────────────────────────────────

    #[test]
    fn action_accept_line() {
        let a = Action::AcceptLine;
        assert!(format!("{:?}", a).contains("AcceptLine"));
    }

    #[test]
    fn action_cancel() {
        let a = Action::Cancel;
        assert!(format!("{:?}", a).contains("Cancel"));
    }

    #[test]
    fn action_end_of_file() {
        let a = Action::EndOfFile;
        assert!(format!("{:?}", a).contains("EndOfFile"));
    }

    #[test]
    fn action_insert_char() {
        let a = Action::InsertChar(2, 'x');
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("InsertChar"));
        assert!(dbg.contains("'x'"));
    }

    #[test]
    fn action_insert_text() {
        let a = Action::InsertText(1, "hello".to_string());
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("InsertText"));
        assert!(dbg.contains("hello"));
    }

    #[test]
    fn action_repaint() {
        let a = Action::Repaint;
        assert!(format!("{:?}", a).contains("Repaint"));
    }

    #[test]
    fn action_move() {
        let a = Action::Move(Movement::EndOfLine);
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("Move"));
        assert!(dbg.contains("EndOfLine"));
    }

    #[test]
    fn action_kill() {
        let a = Action::Kill(Movement::BackwardWord(1));
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("Kill"));
        assert!(dbg.contains("BackwardWord"));
    }

    #[test]
    fn action_kill_and_move() {
        let a = Action::KillAndMove(Movement::StartOfLine, Movement::EndOfLine);
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("KillAndMove"));
        assert!(dbg.contains("StartOfLine"));
        assert!(dbg.contains("EndOfLine"));
    }

    #[test]
    fn action_history_previous() {
        let a = Action::HistoryPrevious;
        assert!(format!("{:?}", a).contains("HistoryPrevious"));
    }

    #[test]
    fn action_history_next() {
        let a = Action::HistoryNext;
        assert!(format!("{:?}", a).contains("HistoryNext"));
    }

    #[test]
    fn action_complete() {
        let a = Action::Complete;
        assert!(format!("{:?}", a).contains("Complete"));
    }

    #[test]
    fn action_no_action() {
        let a = Action::NoAction;
        assert!(format!("{:?}", a).contains("NoAction"));
    }

    #[test]
    fn action_history_inc_search_backwards() {
        let a = Action::HistoryIncSearchBackwards;
        assert!(format!("{:?}", a).contains("HistoryIncSearchBackwards"));
    }

    #[test]
    fn action_history_inc_search_forwards() {
        let a = Action::HistoryIncSearchForwards;
        assert!(format!("{:?}", a).contains("HistoryIncSearchForwards"));
    }

    #[test]
    fn action_clone() {
        let a = Action::InsertText(3, "test".to_string());
        let cloned = a.clone();
        assert_eq!(format!("{:?}", a), format!("{:?}", cloned));
    }

    #[test]
    fn movement_zero_repeat_count() {
        let m = Movement::ForwardChar(0);
        assert!(format!("{:?}", m).contains("ForwardChar(0)"));
    }

    #[test]
    fn movement_large_repeat_count() {
        let m = Movement::BackwardWord(usize::MAX);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("BackwardWord"));
    }
}
