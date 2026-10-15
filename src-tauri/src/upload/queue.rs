//! The one queue (CMP-R46): upload runs, and later the in-game downloads
//! (CMP-R81, B4), first in first out, one at a time. A trigger for a `logId`
//! with a run queued and not started adds nothing, but the queued run takes
//! on the trigger's stronger properties; one for a `logId` whose run is in
//! progress queues one more run, at most.
//!
//! The queue only orders items. What a run does is the runner's; when a
//! trigger may queue at all (linked, outside a stop) is the caller's.

use std::collections::VecDeque;

/// What started a run (CMP-R45).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A debounced change of its watched file (CMP-R21).
    Change,
    /// The Companion started (the first scan's reads ended).
    Start,
    /// A later scan.
    Scan,
    /// Its **Upload** switch was turned on, or its folder picked (CMP-R41).
    SwitchOn,
    /// **Upload now** in the tray.
    UploadNow,
    /// Right after linking (CMP-R101).
    PostLink,
}

/// What a queued run does at its start, from the triggers merged into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Props {
    /// Holds apply (CMP-R45): not for **Upload now** or a post-link run.
    pub holds: bool,
    /// Only file changes asked for it, so a `deleted` hold applies too
    /// (CMP-R45, CMP-R54: "held back from file changes only").
    pub file_change_only: bool,
    /// A later scan's skip applies (CMP-R45): its last run ended uploaded or
    /// up to date and the file is still that run's.
    pub skip: bool,
    /// A run that would end up to date resends its last entry (CMP-R101).
    pub resend: bool,
}

impl Trigger {
    pub fn props(self) -> Props {
        Props {
            holds: !matches!(self, Trigger::UploadNow | Trigger::PostLink),
            file_change_only: self == Trigger::Change,
            skip: self == Trigger::Scan,
            resend: self == Trigger::PostLink,
        }
    }
}

impl Props {
    /// The stronger of two (CMP-R46): no skip and no hold unless both have
    /// them, and the post-link resend if either has it.
    pub fn merge(self, other: Props) -> Props {
        Props {
            holds: self.holds && other.holds,
            file_change_only: self.file_change_only && other.file_change_only,
            skip: self.skip && other.skip,
            resend: self.resend || other.resend,
        }
    }
}

/// An upload run for one `logId`, not started yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub log_id: String,
    pub props: Props,
}

/// One item of the queue. The in-game downloads (CMP-R81, B4) join as a
/// second variant; [`Queue::pop`] already starts them only when no upload
/// run is waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Run(Run),
}

impl Item {
    fn run(&self) -> Option<&Run> {
        match self {
            Item::Run(run) => Some(run),
        }
    }

    fn run_mut(&mut self) -> Option<&mut Run> {
        match self {
            Item::Run(run) => Some(run),
        }
    }
}

/// The queue's items, and the `logId` of the run in progress.
#[derive(Debug, Default)]
pub struct Queue {
    items: VecDeque<Item>,
    running: Option<String>,
}

impl Queue {
    /// Adds a run of `log_id` for `trigger`, or merges the trigger into the
    /// one queued and not started (CMP-R46). Returns whether an item was
    /// added.
    pub fn push(&mut self, log_id: &str, trigger: Trigger) -> bool {
        let props = trigger.props();
        if let Some(queued) = self
            .items
            .iter_mut()
            .filter_map(Item::run_mut)
            .find(|run| run.log_id == log_id)
        {
            queued.props = queued.props.merge(props);
            return false;
        }
        // Not queued: a new run, whether or not one is in progress (which
        // makes this the one more run of CMP-R46).
        self.items.push_back(Item::Run(Run {
            log_id: log_id.to_owned(),
            props,
        }));
        true
    }

    /// Takes the next item and marks it in progress: the first upload run,
    /// or, when no run is waiting, the first other item (CMP-R46).
    pub fn pop(&mut self) -> Option<Item> {
        let at = self
            .items
            .iter()
            .position(|item| item.run().is_some())
            .or(if self.items.is_empty() { None } else { Some(0) })?;
        let item = self.items.remove(at)?;
        self.running = item.run().map(|run| run.log_id.clone());
        Some(item)
    }

    /// The item in progress has ended.
    pub fn done(&mut self) {
        self.running = None;
    }

    /// The `logId` of the run in progress.
    pub fn running(&self) -> Option<&str> {
        self.running.as_deref()
    }

    /// Drops the queued runs of `log_id` (CMP-R41). Returns whether its run
    /// is in progress, which stops after its current request.
    pub fn drop_runs(&mut self, log_id: &str) -> bool {
        self.items
            .retain(|item| item.run().is_none_or(|run| run.log_id != log_id));
        self.running() == Some(log_id)
    }

    /// Drops every queued item (CMP-R63 step 1). Returns whether an item is
    /// in progress, which stops after its current request.
    pub fn clear(&mut self) -> bool {
        self.items.clear();
        self.running.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The queued items, in order.
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3f9a2c1e7b0d4a65";
    const B: &str = "0123456789abcdef";

    fn run(item: Option<Item>) -> Run {
        match item.expect("an item") {
            Item::Run(run) => run,
        }
    }

    #[test]
    fn runs_go_first_in_first_out() {
        // CMP-R46: one queue, first in first out
        let mut queue = Queue::default();
        assert!(queue.push(A, Trigger::Change));
        assert!(queue.push(B, Trigger::Change));
        assert_eq!(run(queue.pop()).log_id, A);
        queue.done();
        assert_eq!(run(queue.pop()).log_id, B);
        queue.done();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn a_trigger_for_a_queued_run_adds_nothing() {
        // CMP-R46: a trigger for a logId with a run queued and not started
        // adds nothing to the queue
        let mut queue = Queue::default();
        queue.push(A, Trigger::Change);
        queue.push(B, Trigger::Change);
        assert!(!queue.push(A, Trigger::Change));
        assert!(!queue.push(A, Trigger::Scan));
        assert_eq!(queue.items().count(), 2);
        // It keeps its place.
        assert_eq!(run(queue.pop()).log_id, A);
    }

    #[test]
    fn a_queued_run_takes_on_the_stronger_properties() {
        // CMP-R46: the queued run takes on the trigger's stronger properties
        // (no skip and no hold for Upload now, the post-link resend of
        // CMP-R101)
        let mut queue = Queue::default();
        queue.push(A, Trigger::Scan);
        queue.push(A, Trigger::UploadNow);
        let props = run(queue.pop()).props;
        assert!(!props.skip);
        assert!(!props.holds);
        assert!(!props.resend);

        queue.done();
        queue.push(A, Trigger::PostLink);
        queue.push(A, Trigger::Scan);
        let props = run(queue.pop()).props;
        assert!(props.resend);
        assert!(!props.holds);
        assert!(!props.skip);
    }

    #[test]
    fn a_change_merged_with_a_scan_has_no_skip() {
        // CMP-R45: only a later scan skips; a change into the same run
        // removes the skip
        let mut queue = Queue::default();
        queue.push(A, Trigger::Scan);
        queue.push(A, Trigger::Change);
        let props = run(queue.pop()).props;
        assert!(!props.skip);
        assert!(props.holds);
        assert!(!props.file_change_only);
    }

    #[test]
    fn each_trigger_has_its_properties() {
        // CMP-R45: holds apply to all but Upload now and linking; the
        // deleted hold to file changes only; the skip to later scans
        assert_eq!(
            Trigger::Change.props(),
            Props {
                holds: true,
                file_change_only: true,
                skip: false,
                resend: false
            }
        );
        for trigger in [Trigger::Start, Trigger::SwitchOn] {
            assert_eq!(
                trigger.props(),
                Props {
                    holds: true,
                    file_change_only: false,
                    skip: false,
                    resend: false
                }
            );
        }
        assert!(Trigger::Scan.props().skip);
        assert!(!Trigger::UploadNow.props().holds);
        assert!(!Trigger::PostLink.props().holds);
        assert!(Trigger::PostLink.props().resend);
    }

    #[test]
    fn a_trigger_during_a_run_queues_one_more_at_most() {
        // CMP-R46: one for a logId whose run is in progress queues one more
        // run, at most
        let mut queue = Queue::default();
        queue.push(A, Trigger::Change);
        run(queue.pop());
        assert_eq!(queue.running(), Some(A));
        assert!(queue.push(A, Trigger::Change));
        assert!(!queue.push(A, Trigger::Change));
        assert_eq!(queue.items().count(), 1);
    }

    #[test]
    fn switching_off_drops_queued_runs_and_flags_the_running_one() {
        // CMP-R41: a queued run of that logId is dropped, and a running one
        // stops after its current request
        let mut queue = Queue::default();
        queue.push(A, Trigger::Change);
        queue.push(B, Trigger::Change);
        assert!(!queue.drop_runs(A));
        assert_eq!(run(queue.pop()).log_id, B);
        queue.push(B, Trigger::Change);
        assert!(queue.drop_runs(B));
        assert!(queue.is_empty());
    }

    #[test]
    fn cancelling_clears_the_queue() {
        // CMP-R63 step 1: cancels every queued and running item
        let mut queue = Queue::default();
        queue.push(A, Trigger::Change);
        assert!(!queue.clear());
        queue.push(A, Trigger::Change);
        queue.push(B, Trigger::Change);
        run(queue.pop());
        assert!(queue.clear());
        assert!(queue.is_empty());
        queue.done();
        assert_eq!(queue.running(), None);
    }
}
