use std::collections::{HashMap, HashSet, VecDeque};

use crate::tui::message::{RunId, RunKind, RunRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestArrival {
    IgnoredStale,
    Accepted { cancel_active: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StressCancellation {
    Ignored,
    Accepted { cancel_active: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveLogicalState {
    Latest,
    Obsolete,
    Cancelled,
}

#[derive(Debug)]
struct ActiveRequest {
    request: RunRequest,
    logical_state: ActiveLogicalState,
    cancel_requested: bool,
}

#[derive(Debug)]
pub(super) struct RetiredActive {
    request: RunRequest,
    is_latest: bool,
    requeue_eligible: bool,
}

impl RetiredActive {
    pub(super) fn request(&self) -> RunRequest {
        self.request
    }

    pub(super) fn is_latest(&self) -> bool {
        self.is_latest
    }
}

#[derive(Debug, Default)]
pub(super) struct RunScheduler {
    active: Option<ActiveRequest>,
    foreground: Option<RunRequest>,
    pending: VecDeque<RunRequest>,
    latest_seen: HashMap<usize, RunId>,
}

impl RunScheduler {
    pub(super) fn active_request(&self) -> Option<RunRequest> {
        self.active.as_ref().map(|active| active.request)
    }

    pub(super) fn request_arrived(&mut self, request: RunRequest) -> RequestArrival {
        if self
            .latest_seen
            .get(&request.problem)
            .is_some_and(|latest| request.run_id <= *latest)
        {
            return RequestArrival::IgnoredStale;
        }

        self.latest_seen.insert(request.problem, request.run_id);
        self.remove_pending_problem(request.problem);

        if let Some(previous_foreground) = self.foreground.take()
            && previous_foreground.problem != request.problem
            && previous_foreground.kind.preserve_on_preemption()
        {
            self.push_pending_latest(previous_foreground);
        }

        let cancel_active = if let Some(active) = self.active.as_mut() {
            if active.request.problem == request.problem {
                active.logical_state = ActiveLogicalState::Obsolete;
            }

            if active.cancel_requested {
                false
            } else {
                active.cancel_requested = true;
                true
            }
        } else {
            false
        };

        self.foreground = Some(request);
        debug_assert!(self.invariants_hold());

        RequestArrival::Accepted { cancel_active }
    }

    pub(super) fn cancel_stress(&mut self, problem: usize, run_id: RunId) -> StressCancellation {
        let matches_stress = |request: &RunRequest| {
            request.problem == problem
                && request.run_id == run_id
                && matches!(request.kind, RunKind::Stress { .. })
        };

        if self.foreground.as_ref().is_some_and(matches_stress) {
            self.foreground = None;
            debug_assert!(self.invariants_hold());
            return StressCancellation::Accepted {
                cancel_active: false,
            };
        }

        if let Some(position) = self.pending.iter().position(matches_stress) {
            self.pending.remove(position);
            debug_assert!(self.invariants_hold());
            return StressCancellation::Accepted {
                cancel_active: false,
            };
        }

        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| matches_stress(&active.request))
        else {
            debug_assert!(self.invariants_hold());
            return StressCancellation::Ignored;
        };

        active.logical_state = ActiveLogicalState::Cancelled;
        let cancel_active = !std::mem::replace(&mut active.cancel_requested, true);
        debug_assert!(self.invariants_hold());

        StressCancellation::Accepted { cancel_active }
    }

    pub(super) fn start_next(&mut self) -> Option<RunRequest> {
        if self.active.is_some() {
            return None;
        }

        let request = self
            .foreground
            .take()
            .or_else(|| self.pending.pop_front())?;

        debug_assert_eq!(
            self.latest_seen.get(&request.problem),
            Some(&request.run_id)
        );

        self.active = Some(ActiveRequest {
            request,
            logical_state: ActiveLogicalState::Latest,
            cancel_requested: false,
        });
        debug_assert!(self.invariants_hold());

        Some(request)
    }

    pub(super) fn retire_active(&mut self) -> Option<RetiredActive> {
        let active = self.active.take()?;
        let is_latest = active.logical_state == ActiveLogicalState::Latest
            && self.latest_seen.get(&active.request.problem) == Some(&active.request.run_id)
            && !self.has_logical_request_for(active.request.problem);
        let requeue_eligible = is_latest && active.request.kind.preserve_on_preemption();

        debug_assert!(self.invariants_hold());

        Some(RetiredActive {
            request: active.request,
            is_latest,
            requeue_eligible,
        })
    }

    pub(super) fn requeue_retired(&mut self, retired: RetiredActive) -> bool {
        let request = retired.request;

        if !retired.requeue_eligible
            || self.latest_seen.get(&request.problem) != Some(&request.run_id)
            || self.has_logical_request_for(request.problem)
        {
            debug_assert!(self.invariants_hold());
            return false;
        }

        self.pending.push_back(request);
        debug_assert!(self.invariants_hold());
        true
    }

    fn remove_pending_problem(&mut self, problem: usize) -> Option<RunRequest> {
        let position = self
            .pending
            .iter()
            .position(|request| request.problem == problem)?;
        self.pending.remove(position)
    }

    fn push_pending_latest(&mut self, request: RunRequest) {
        self.remove_pending_problem(request.problem);
        self.pending.push_back(request);
    }

    fn has_logical_request_for(&self, problem: usize) -> bool {
        self.foreground
            .is_some_and(|request| request.problem == problem)
            || self
                .pending
                .iter()
                .any(|request| request.problem == problem)
    }

    fn invariants_hold(&self) -> bool {
        let mut logical_problems = HashSet::new();

        if let Some(foreground) = self.foreground
            && (!logical_problems.insert(foreground.problem)
                || self.latest_seen.get(&foreground.problem) != Some(&foreground.run_id))
        {
            return false;
        }

        for pending in &self.pending {
            if !logical_problems.insert(pending.problem)
                || self.latest_seen.get(&pending.problem) != Some(&pending.run_id)
            {
                return false;
            }
        }

        if let Some(active) = &self.active {
            match active.logical_state {
                ActiveLogicalState::Latest => {
                    if !logical_problems.insert(active.request.problem)
                        || self.latest_seen.get(&active.request.problem)
                            != Some(&active.request.run_id)
                    {
                        return false;
                    }
                }
                ActiveLogicalState::Obsolete => {
                    // The newer same-problem request may already have been explicitly
                    // cancelled, so latest_seen remains authoritative even when no queued
                    // replacement remains.
                    if self
                        .latest_seen
                        .get(&active.request.problem)
                        .is_none_or(|latest| *latest <= active.request.run_id)
                    {
                        return false;
                    }
                }
                ActiveLogicalState::Cancelled => {
                    if !matches!(active.request.kind, RunKind::Stress { .. })
                        || self
                            .latest_seen
                            .get(&active.request.problem)
                            .is_none_or(|latest| *latest < active.request.run_id)
                    {
                        return false;
                    }
                }
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::tui::message::RunKind;

    fn request(problem: usize, run_id: RunId) -> RunRequest {
        RunRequest {
            run_id,
            problem,
            language: Language::Cpp,
            debug: false,
            kind: RunKind::Samples,
        }
    }

    fn ids(requests: impl IntoIterator<Item = RunRequest>) -> Vec<(usize, RunId)> {
        requests
            .into_iter()
            .map(|request| (request.problem, request.run_id))
            .collect()
    }

    fn pending_ids(scheduler: &RunScheduler) -> Vec<(usize, RunId)> {
        ids(scheduler.pending.iter().copied())
    }

    fn foreground_id(scheduler: &RunScheduler) -> Option<(usize, RunId)> {
        scheduler
            .foreground
            .map(|request| (request.problem, request.run_id))
    }

    fn assert_invariants(scheduler: &RunScheduler) {
        assert!(scheduler.invariants_hold());

        let pending_problems = scheduler
            .pending
            .iter()
            .map(|request| request.problem)
            .collect::<HashSet<_>>();
        assert_eq!(pending_problems.len(), scheduler.pending.len());

        if let Some(foreground) = scheduler.foreground {
            assert!(!pending_problems.contains(&foreground.problem));
        }
    }

    fn start(scheduler: &mut RunScheduler, request: RunRequest) {
        assert_eq!(
            scheduler.request_arrived(request),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );
        assert_eq!(scheduler.start_next(), Some(request));
    }

    #[test]
    fn empty_scheduler_accepts_a_foreground_request() {
        let mut scheduler = RunScheduler::default();
        let a1 = request(0, 1);

        assert_eq!(
            scheduler.request_arrived(a1),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );
        assert_eq!(foreground_id(&scheduler), Some((0, 1)));
        assert!(scheduler.pending.is_empty());
        assert_invariants(&scheduler);
    }

    #[test]
    fn same_foreground_problem_is_replaced_without_becoming_pending() {
        let mut scheduler = RunScheduler::default();
        scheduler.request_arrived(request(0, 1));

        assert_eq!(
            scheduler.request_arrived(request(0, 2)),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );
        assert_eq!(foreground_id(&scheduler), Some((0, 2)));
        assert!(scheduler.pending.is_empty());
        assert_invariants(&scheduler);
    }

    #[test]
    fn different_foreground_problem_demotes_the_previous_one_to_the_tail() {
        let mut scheduler = RunScheduler::default();
        scheduler.request_arrived(request(1, 1));
        scheduler.request_arrived(request(2, 2));

        assert_eq!(foreground_id(&scheduler), Some((2, 2)));
        assert_eq!(pending_ids(&scheduler), [(1, 1)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn pending_first_problem_is_replaced_and_promoted_without_reordering_others() {
        let mut scheduler = scheduler_with_three_pending();

        assert_eq!(
            scheduler.request_arrived(request(0, 10)),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(foreground_id(&scheduler), Some((0, 10)));
        assert_eq!(pending_ids(&scheduler), [(1, 3), (2, 4)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn pending_middle_problem_is_replaced_and_promoted_without_reordering_others() {
        let mut scheduler = scheduler_with_three_pending();

        assert_eq!(
            scheduler.request_arrived(request(1, 10)),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(foreground_id(&scheduler), Some((1, 10)));
        assert_eq!(pending_ids(&scheduler), [(0, 2), (2, 4)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn stale_request_does_not_replace_foreground_or_request_cancellation() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(9, 1));
        scheduler.request_arrived(request(0, 3));

        assert_eq!(
            scheduler.request_arrived(request(0, 2)),
            RequestArrival::IgnoredStale
        );
        assert_eq!(foreground_id(&scheduler), Some((0, 3)));
        assert!(scheduler.pending.is_empty());
        assert_invariants(&scheduler);
    }

    #[test]
    fn stale_request_does_not_replace_pending_latest() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(9, 1));
        scheduler.request_arrived(request(0, 3));
        scheduler.request_arrived(request(1, 4));
        assert_eq!(pending_ids(&scheduler), [(0, 3)]);

        assert_eq!(
            scheduler.request_arrived(request(0, 2)),
            RequestArrival::IgnoredStale
        );
        assert_eq!(foreground_id(&scheduler), Some((1, 4)));
        assert_eq!(pending_ids(&scheduler), [(0, 3)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn same_problem_request_makes_the_active_attempt_obsolete_and_not_requeueable() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));

        assert_eq!(
            scheduler.request_arrived(request(0, 2)),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(
            scheduler.active.as_ref().unwrap().logical_state,
            ActiveLogicalState::Obsolete
        );

        let retired = scheduler.retire_active().unwrap();
        assert!(!retired.requeue_eligible);
        assert!(!scheduler.requeue_retired(retired));
        assert_eq!(foreground_id(&scheduler), Some((0, 2)));
        assert_invariants(&scheduler);
    }

    #[test]
    fn different_problem_request_keeps_the_active_attempt_requeue_eligible() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));

        assert_eq!(
            scheduler.request_arrived(request(1, 2)),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(
            scheduler.active.as_ref().unwrap().logical_state,
            ActiveLogicalState::Latest
        );

        let retired = scheduler.retire_active().unwrap();
        assert!(retired.requeue_eligible);
        assert!(scheduler.requeue_retired(retired));
        assert_eq!(foreground_id(&scheduler), Some((1, 2)));
        assert_eq!(pending_ids(&scheduler), [(0, 1)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn active_becomes_obsolete_when_same_problem_arrives_during_preemption() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));
        scheduler.request_arrived(request(1, 2));

        assert_eq!(
            scheduler.request_arrived(request(0, 3)),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );
        assert_eq!(foreground_id(&scheduler), Some((0, 3)));
        assert_eq!(pending_ids(&scheduler), [(1, 2)]);

        let retired = scheduler.retire_active().unwrap();
        assert!(!retired.requeue_eligible);
        assert!(!scheduler.requeue_retired(retired));
        assert_invariants(&scheduler);
    }

    #[test]
    fn rapid_foreground_changes_preserve_each_displaced_latest_in_fifo_order() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));

        assert_eq!(
            scheduler.request_arrived(request(1, 2)),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(
            scheduler.request_arrived(request(2, 3)),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );
        assert_eq!(
            scheduler.request_arrived(request(3, 4)),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );

        assert_eq!(foreground_id(&scheduler), Some((3, 4)));
        assert_eq!(pending_ids(&scheduler), [(1, 2), (2, 3)]);
        assert_eq!(
            scheduler.active.as_ref().unwrap().logical_state,
            ActiveLogicalState::Latest
        );
        assert_invariants(&scheduler);
    }

    #[test]
    fn long_sequence_keeps_only_each_problems_latest_request() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));

        for request in [
            request(1, 2),
            request(0, 3),
            request(2, 4),
            request(1, 5),
            request(0, 6),
            request(2, 7),
        ] {
            scheduler.request_arrived(request);
            assert_invariants(&scheduler);
        }

        assert_eq!(foreground_id(&scheduler), Some((2, 7)));
        assert_eq!(pending_ids(&scheduler), [(1, 5), (0, 6)]);
        assert_eq!(
            scheduler.active.as_ref().unwrap().logical_state,
            ActiveLogicalState::Obsolete
        );
    }

    #[test]
    fn quiescence_runs_foreground_then_pending_fifo() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));
        scheduler.request_arrived(request(1, 2));
        scheduler.request_arrived(request(2, 3));
        scheduler.request_arrived(request(3, 4));

        // active Aは自然完了したものとしてdropし、requeueしない。
        assert!(scheduler.retire_active().is_some());

        let mut order = Vec::new();
        while let Some(next) = scheduler.start_next() {
            order.push((next.problem, next.run_id));
            assert!(scheduler.retire_active().is_some());
        }

        assert_eq!(order, [(3, 4), (1, 2), (2, 3)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn cancelled_active_returns_after_older_pending_requests() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 1));
        scheduler.request_arrived(request(1, 2));
        scheduler.request_arrived(request(2, 3));
        scheduler.request_arrived(request(3, 4));

        let retired = scheduler.retire_active().unwrap();
        assert!(scheduler.requeue_retired(retired));
        assert_eq!(foreground_id(&scheduler), Some((3, 4)));
        assert_eq!(pending_ids(&scheduler), [(1, 2), (2, 3), (0, 1)]);

        let mut order = Vec::new();
        while let Some(next) = scheduler.start_next() {
            order.push((next.problem, next.run_id));
            assert!(scheduler.retire_active().is_some());
        }
        assert_eq!(order, [(3, 4), (1, 2), (2, 3), (0, 1)]);
        assert_invariants(&scheduler);
    }

    #[test]
    fn stale_request_cannot_resurrect_after_latest_request_finishes() {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(0, 3));
        assert!(scheduler.retire_active().is_some());

        assert_eq!(
            scheduler.request_arrived(request(0, 2)),
            RequestArrival::IgnoredStale
        );
        assert!(scheduler.start_next().is_none());
        assert_invariants(&scheduler);
    }

    #[test]
    fn duplicate_run_id_is_ignored_without_replacing_the_payload() {
        let mut scheduler = RunScheduler::default();
        let original = request(0, 3);
        scheduler.request_arrived(original);

        let duplicate = RunRequest {
            run_id: 3,
            problem: 0,
            language: Language::Python,
            debug: true,
            kind: RunKind::Samples,
        };
        assert_eq!(
            scheduler.request_arrived(duplicate),
            RequestArrival::IgnoredStale
        );
        assert_eq!(scheduler.start_next(), Some(original));
        assert_invariants(&scheduler);
    }

    #[test]
    fn stress_foreground_is_discarded_instead_of_demoted_to_pending() {
        let mut scheduler = RunScheduler::default();
        let stress = RunRequest {
            kind: RunKind::Stress {
                base_seed: 42,
                count: None,
            },
            ..request(0, 1)
        };
        let b = request(1, 2);

        assert_eq!(
            scheduler.request_arrived(stress),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );
        assert_eq!(
            scheduler.request_arrived(b),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );

        assert_eq!(scheduler.start_next(), Some(b));
        assert!(scheduler.pending.is_empty());
        assert_invariants(&scheduler);
    }

    #[test]
    fn cancelled_stress_request_is_never_requeued() {
        let mut scheduler = RunScheduler::default();
        let stress = RunRequest {
            kind: RunKind::Stress {
                base_seed: 42,
                count: None,
            },
            ..request(0, 1)
        };

        start(&mut scheduler, stress);
        let retired = scheduler.retire_active().unwrap();

        assert!(retired.is_latest());
        assert!(!retired.requeue_eligible);
        assert!(!scheduler.requeue_retired(retired));
        assert!(scheduler.start_next().is_none());
        assert_invariants(&scheduler);
    }

    #[test]
    fn queued_stress_cancellation_removes_exact_generation_before_it_can_start() {
        let mut scheduler = RunScheduler::default();
        let sample = request(0, 1);
        let stress = RunRequest {
            kind: RunKind::Stress {
                base_seed: 42,
                count: None,
            },
            ..request(1, 2)
        };
        start(&mut scheduler, sample);
        assert_eq!(
            scheduler.request_arrived(stress),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );

        assert_eq!(
            scheduler.cancel_stress(stress.problem, stress.run_id),
            StressCancellation::Accepted {
                cancel_active: false
            }
        );
        let retired = scheduler.retire_active().unwrap();
        assert!(scheduler.requeue_retired(retired));
        assert_eq!(scheduler.start_next(), Some(sample));
        assert!(scheduler.retire_active().is_some());
        assert!(scheduler.start_next().is_none());
        assert_invariants(&scheduler);
    }

    #[test]
    fn cancelling_same_problem_queued_stress_does_not_resurrect_obsolete_attempt() {
        let mut scheduler = RunScheduler::default();
        let sample = request(0, 1);
        let stress = RunRequest {
            kind: RunKind::Stress {
                base_seed: 42,
                count: None,
            },
            ..request(0, 2)
        };
        start(&mut scheduler, sample);
        assert_eq!(
            scheduler.request_arrived(stress),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );

        assert_eq!(
            scheduler.cancel_stress(stress.problem, stress.run_id),
            StressCancellation::Accepted {
                cancel_active: false
            }
        );
        let retired = scheduler.retire_active().unwrap();
        assert!(!retired.is_latest());
        assert!(!scheduler.requeue_retired(retired));
        assert!(scheduler.start_next().is_none());
        assert_invariants(&scheduler);
    }

    #[test]
    fn active_stress_cancellation_is_identity_scoped_and_terminal() {
        let mut scheduler = RunScheduler::default();
        let stress = RunRequest {
            kind: RunKind::Stress {
                base_seed: 42,
                count: None,
            },
            ..request(0, 10)
        };
        start(&mut scheduler, stress);

        assert_eq!(scheduler.cancel_stress(0, 9), StressCancellation::Ignored);
        assert_eq!(scheduler.cancel_stress(1, 10), StressCancellation::Ignored);
        assert_eq!(
            scheduler.cancel_stress(0, 10),
            StressCancellation::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(
            scheduler.cancel_stress(0, 10),
            StressCancellation::Accepted {
                cancel_active: false
            }
        );

        let retired = scheduler.retire_active().unwrap();
        assert!(!retired.is_latest());
        assert!(!scheduler.requeue_retired(retired));
        assert!(scheduler.start_next().is_none());
        assert_invariants(&scheduler);
    }

    #[test]
    fn stale_stress_cancellation_cannot_remove_a_newer_generation() {
        let mut scheduler = RunScheduler::default();
        let old = RunRequest {
            kind: RunKind::Stress {
                base_seed: 10,
                count: None,
            },
            ..request(0, 10)
        };
        let new = RunRequest {
            kind: RunKind::Stress {
                base_seed: 11,
                count: None,
            },
            ..request(0, 11)
        };
        scheduler.request_arrived(old);
        scheduler.request_arrived(new);

        assert_eq!(
            scheduler.cancel_stress(old.problem, old.run_id),
            StressCancellation::Ignored
        );
        assert_eq!(scheduler.start_next(), Some(new));
        assert_invariants(&scheduler);
    }

    #[test]
    fn cancelling_old_active_stress_before_new_arrival_preserves_new_generation() {
        let mut scheduler = RunScheduler::default();
        let old = RunRequest {
            kind: RunKind::Stress {
                base_seed: 10,
                count: None,
            },
            ..request(0, 10)
        };
        let new = RunRequest {
            kind: RunKind::Stress {
                base_seed: 11,
                count: None,
            },
            ..request(0, 11)
        };
        start(&mut scheduler, old);

        assert_eq!(
            scheduler.cancel_stress(old.problem, old.run_id),
            StressCancellation::Accepted {
                cancel_active: true
            }
        );
        assert_eq!(
            scheduler.request_arrived(new),
            RequestArrival::Accepted {
                cancel_active: false
            }
        );

        let retired = scheduler.retire_active().unwrap();
        assert!(!retired.is_latest());
        assert!(!scheduler.requeue_retired(retired));
        assert_eq!(scheduler.start_next(), Some(new));
        assert_invariants(&scheduler);
    }

    #[test]
    fn newer_active_replacement_then_old_stop_preserves_new_generation() {
        let mut scheduler = RunScheduler::default();
        let old = RunRequest {
            kind: RunKind::Stress {
                base_seed: 10,
                count: None,
            },
            ..request(0, 10)
        };
        let new = RunRequest {
            kind: RunKind::Stress {
                base_seed: 11,
                count: None,
            },
            ..request(0, 11)
        };
        start(&mut scheduler, old);
        assert_eq!(
            scheduler.request_arrived(new),
            RequestArrival::Accepted {
                cancel_active: true
            }
        );

        assert_eq!(
            scheduler.cancel_stress(old.problem, old.run_id),
            StressCancellation::Accepted {
                cancel_active: false
            }
        );
        let retired = scheduler.retire_active().unwrap();
        assert!(!retired.is_latest());
        assert!(!scheduler.requeue_retired(retired));
        assert_eq!(scheduler.start_next(), Some(new));
        assert_invariants(&scheduler);
    }

    #[test]
    fn accepted_request_preserves_opaque_payload_fields() {
        let mut scheduler = RunScheduler::default();
        let python_debug = RunRequest {
            run_id: 1,
            problem: 0,
            language: Language::Python,
            debug: true,
            kind: RunKind::Samples,
        };

        scheduler.request_arrived(python_debug);

        assert_eq!(scheduler.start_next(), Some(python_debug));
        assert_invariants(&scheduler);
    }

    fn scheduler_with_three_pending() -> RunScheduler {
        let mut scheduler = RunScheduler::default();
        start(&mut scheduler, request(9, 1));
        scheduler.request_arrived(request(0, 2));
        scheduler.request_arrived(request(1, 3));
        scheduler.request_arrived(request(2, 4));
        scheduler.request_arrived(request(8, 5));

        // Xを完了させ、現在foregroundのYをactiveへ進めることで、
        // pending A/B/Cだけを自然なtransitionで構築する。
        assert!(scheduler.retire_active().is_some());
        assert_eq!(scheduler.start_next(), Some(request(8, 5)));
        assert_eq!(pending_ids(&scheduler), [(0, 2), (1, 3), (2, 4)]);
        scheduler
    }
}
