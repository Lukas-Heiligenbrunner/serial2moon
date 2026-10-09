//! Line numbering for Marlin's `N<n> ... *<checksum>` protocol.
//!
//! `Resend: R` means "the next line I expect is R" — and R is not always the line in
//! flight. Prusa firmware, on filament runout, crash recovery or its own pause/resume,
//! throws away its planner and command queue (acking what it discards!) and then asks for
//! everything from the first discarded line again. So we keep the recently accepted lines
//! and treat every `Resend` as moving the send cursor to R: replay from history when R is
//! behind us, skip ahead when the printer already has lines, realign when R is unknown.

/// A command waiting for its `ok`. Unnumbered commands (`number: None`) bypass the
/// printer's line counter.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub number: Option<u64>,
    pub cmd: String,
}

/// What the sender should do after an event.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// (Re)send the front of the pending queue.
    SendFront,
    /// Keep waiting; nothing to send.
    Wait,
    /// Every pending line was accepted.
    Done,
}

use std::collections::VecDeque;

/// Accepted lines kept for replay. The MK3's planner holds 16 moves plus a few queued
/// commands, so this is ample.
pub const HISTORY_LINES: usize = 128;

pub struct Numbering {
    /// Number the next new numbered line gets.
    next: u64,
    /// Recently accepted numbered lines, consecutive, oldest first.
    history: VecDeque<(u64, String)>,
}

impl Numbering {
    pub fn new() -> Self {
        Numbering {
            next: 0,
            history: VecDeque::new(),
        }
    }

    /// Before `M110 N0`: numbering restarts at 0 and earlier lines can't be replayed.
    pub fn reset(&mut self) {
        self.next = 0;
        self.history.clear();
    }

    #[cfg(test)]
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Queue `cmd`; a numbered command takes the next line number.
    pub fn start(&mut self, cmd: &str, numbered: bool) -> VecDeque<Pending> {
        VecDeque::from([Pending {
            number: numbered.then_some(self.next),
            cmd: cmd.to_string(),
        }])
    }

    /// The printer acked the front line.
    pub fn on_ok(&mut self, pending: &mut VecDeque<Pending>) -> Step {
        if let Some(line) = pending.pop_front()
            && let Some(n) = line.number
        {
            self.accept(n, line.cmd);
        }
        if pending.is_empty() {
            Step::Done
        } else {
            Step::SendFront
        }
    }

    /// The printer asked to continue from line `r`.
    pub fn on_resend(&mut self, pending: &mut VecDeque<Pending>, r: u64) -> Step {
        // The line the printer should want next, and one past the last one we have pending.
        let head = pending.iter().find_map(|p| p.number).unwrap_or(self.next);
        let end = pending
            .iter()
            .rev()
            .find_map(|p| p.number)
            .map_or(self.next, |n| n + 1);

        if r == head {
            // It wants our first pending line again (checksum error, or it was discarded).
            // With only an unnumbered command pending, R is simply the line we'd send next.
            return if pending.front().is_some_and(|p| p.number.is_some()) {
                Step::SendFront
            } else {
                Step::Wait
            };
        }
        if r > head && r <= end {
            // It already has everything below R (e.g. a duplicate of ours got through).
            while let Some(line) = pending.pop_front_if(|p| p.number.is_some_and(|n| n < r)) {
                self.accept(line.number.unwrap_or_default(), line.cmd);
            }
            return if pending.is_empty() {
                Step::Done
            } else {
                Step::SendFront
            };
        }
        if r < head
            && let Some(replay) = self.replay(r, head)
        {
            for line in replay.into_iter().rev() {
                pending.push_front(line);
            }
            return Step::SendFront;
        }
        // R is outside anything we know (e.g. the printer's counter survived our
        // reconnect): adopt its numbering for what we still have to send.
        for (line, n) in pending.iter_mut().filter(|p| p.number.is_some()).zip(r..) {
            line.number = Some(n);
        }
        self.history.clear();
        Step::SendFront
    }

    /// Lines `from..to` from the history, if all of them are still there.
    fn replay(&self, from: u64, to: u64) -> Option<Vec<Pending>> {
        let &(oldest, _) = self.history.front()?;
        if from < oldest {
            return None;
        }
        let lines: Vec<Pending> = self
            .history
            .iter()
            .filter(|(n, _)| (from..to).contains(n))
            .map(|(n, cmd)| Pending {
                number: Some(*n),
                cmd: cmd.clone(),
            })
            .collect();
        (lines.len() as u64 == to - from).then_some(lines)
    }

    fn accept(&mut self, n: u64, cmd: String) {
        match self.history.back() {
            Some((last, _)) if n <= *last => {} // replayed: already recorded
            Some((last, _)) if n == *last + 1 => self.history.push_back((n, cmd)),
            _ => {
                self.history.clear(); // first line, or a jump after realigning
                self.history.push_back((n, cmd));
            }
        }
        while self.history.len() > HISTORY_LINES {
            self.history.pop_front();
        }
        self.next = n + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(pending: &VecDeque<Pending>) -> Vec<Option<u64>> {
        pending.iter().map(|p| p.number).collect()
    }

    /// A printer link that has accepted `G1 X1` … `G1 X<last>` as lines 1..=last.
    fn accepted_up_to(last: u64) -> Numbering {
        let mut nb = Numbering::new();
        nb.reset();
        let mut p = nb.start("M110 N0", true);
        assert_eq!(nb.on_ok(&mut p), Step::Done);
        for i in 1..=last {
            let mut p = nb.start(&format!("G1 X{i}"), true);
            assert_eq!(numbers(&p), vec![Some(i)]);
            assert_eq!(nb.on_ok(&mut p), Step::Done);
        }
        nb
    }

    #[test]
    fn numbers_lines_consecutively_from_a_reset() {
        let nb = accepted_up_to(3);
        assert_eq!(nb.next(), 4);
    }

    #[test]
    fn resend_of_the_line_in_flight_resends_it() {
        // Checksum error on the line we just sent.
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("G1 X5", true);
        assert_eq!(nb.on_resend(&mut p, 5), Step::SendFront);
        assert_eq!(numbers(&p), vec![Some(5)]);
        assert_eq!(nb.on_ok(&mut p), Step::Done);
        assert_eq!(nb.next(), 6);
    }

    #[test]
    fn resend_past_the_line_in_flight_means_the_printer_has_it() {
        // Our ok was lost and the re-send was a duplicate: "I already have 5, want 6".
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("G1 X5", true);
        assert_eq!(nb.on_resend(&mut p, 6), Step::Done);
        assert_eq!(nb.next(), 6);
    }

    #[test]
    fn resend_of_older_lines_replays_them_in_order() {
        // Prusa runout / crash recovery / firmware pause: the printer discarded its planner
        // and asks for everything from line 2 again.
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("G1 X5", true);
        assert_eq!(nb.on_resend(&mut p, 2), Step::SendFront);
        let replay: Vec<_> = p.iter().map(|l| (l.number, l.cmd.as_str())).collect();
        assert_eq!(
            replay,
            vec![
                (Some(2), "G1 X2"),
                (Some(3), "G1 X3"),
                (Some(4), "G1 X4"),
                (Some(5), "G1 X5"),
            ]
        );
        for expected in [
            Step::SendFront,
            Step::SendFront,
            Step::SendFront,
            Step::Done,
        ] {
            assert_eq!(nb.on_ok(&mut p), expected);
        }
        assert_eq!(nb.next(), 6);
    }

    #[test]
    fn replay_survives_the_ok_for_a_discarded_line() {
        // The printer acks the line it discards (cmdqueue_reset), and only then asks for
        // older lines — by which time we are already on the next line.
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("G1 X5", true);
        assert_eq!(nb.on_ok(&mut p), Step::Done); // the "dump" ok for discarded line 5
        let mut p = nb.start("G1 X6", true);
        assert_eq!(nb.on_resend(&mut p, 3), Step::SendFront);
        assert_eq!(numbers(&p), vec![Some(3), Some(4), Some(5), Some(6)]);
    }

    #[test]
    fn skips_lines_the_printer_already_has_during_a_replay() {
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("G1 X5", true);
        nb.on_resend(&mut p, 2);
        // A duplicate got through; the printer now wants 4.
        assert_eq!(nb.on_resend(&mut p, 4), Step::SendFront);
        assert_eq!(numbers(&p), vec![Some(4), Some(5)]);
        // ... and then reports it has everything.
        assert_eq!(nb.on_resend(&mut p, 6), Step::Done);
        assert_eq!(nb.next(), 6);
    }

    #[test]
    fn unnumbered_command_leaves_the_counter_alone() {
        // M602 on a Prusa: the firmware rewinds to its saved line and asks for exactly the
        // next line we would send — nothing to replay, the command itself still gets its ok.
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("M602", false);
        assert_eq!(numbers(&p), vec![None]);
        assert_eq!(nb.on_resend(&mut p, 5), Step::Wait);
        assert_eq!(nb.on_ok(&mut p), Step::Done);
        assert_eq!(nb.next(), 5);
    }

    #[test]
    fn unnumbered_command_replays_older_lines_first() {
        let mut nb = accepted_up_to(4);
        let mut p = nb.start("M79 S\"SM\"", false);
        assert_eq!(nb.on_resend(&mut p, 3), Step::SendFront);
        assert_eq!(numbers(&p), vec![Some(3), Some(4), None]);
    }

    #[test]
    fn resend_outside_the_history_realigns_to_the_printer() {
        // E.g. the printer's counter survived our reconnect: adopt its number.
        let mut nb = Numbering::new();
        let mut p = nb.start("G1 X1", true);
        assert_eq!(nb.on_resend(&mut p, 42), Step::SendFront);
        assert_eq!(numbers(&p), vec![Some(42)]);
        assert_eq!(nb.on_ok(&mut p), Step::Done);
        assert_eq!(nb.next(), 43);
    }

    #[test]
    fn history_is_bounded() {
        let mut nb = accepted_up_to(HISTORY_LINES as u64 + 10);
        let mut p = nb.start("G1 X0", true);
        // Line 1 fell out of the history, so it can't be replayed: realign instead.
        assert_eq!(nb.on_resend(&mut p, 1), Step::SendFront);
        assert_eq!(numbers(&p), vec![Some(1)]);
    }
}
