//! Interactive, in-memory visual fixture for the production watch TUI renderer.
//!
//! This module is debug-only on purpose. Its event loop owns no production workers or backend
//! handles, so a demo key cannot start a run, open an editor, touch a workspace, or submit.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use time::{Date, Month, OffsetDateTime, PlainDateTime, Time, UtcOffset};

use super::app::WatchApp;
use super::detail_layout::DetailLayout;
use super::submission::{
    SubmissionDisplayState, SubmissionHistoryEntry, SubmissionKey, SubmissionViewState,
    TuiSubmissionAttemptState, TuiSubmissionState,
};
use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::{ShortcutHelpKeyResult, TerminaSession, handle_shortcut_help_key, view};
use crate::atcoder::submission_tracking::{SubmissionResult, SubmissionStatus, Verdict};
use crate::language::Language;
use crate::model::{Contest, Problem};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const SCENARIO_INTERVAL: Duration = Duration::from_millis(650);
const MAX_EVENTS_PER_TICK: usize = 256;

const SCENARIO: [SubmissionDisplayState; 8] = [
    attempt(TuiSubmissionAttemptState::Submitting),
    current(TuiSubmissionState::Accepted),
    current(TuiSubmissionState::Status(
        SubmissionStatus::WaitingForJudge,
    )),
    current(TuiSubmissionState::Status(SubmissionStatus::Judging)),
    current(TuiSubmissionState::Status(
        SubmissionStatus::JudgingProgress {
            judged: 1,
            total: 72,
            provisional: None,
        },
    )),
    current(TuiSubmissionState::Status(
        SubmissionStatus::JudgingProgress {
            judged: 17,
            total: 72,
            provisional: None,
        },
    )),
    current(TuiSubmissionState::Status(
        SubmissionStatus::JudgingProgress {
            judged: 55,
            total: 72,
            provisional: Some(Verdict::RuntimeError),
        },
    )),
    current(TuiSubmissionState::Status(SubmissionStatus::Finished(
        SubmissionResult::with_metrics(Verdict::RuntimeError, Some(31), Some(33_348)),
    ))),
];

const fn current(state: TuiSubmissionState) -> SubmissionDisplayState {
    SubmissionDisplayState {
        current: Some(state),
        attempt: None,
    }
}

const fn attempt(state: TuiSubmissionAttemptState) -> SubmissionDisplayState {
    SubmissionDisplayState {
        current: None,
        attempt: Some(state),
    }
}

const fn composite(
    current: TuiSubmissionState,
    attempt: TuiSubmissionAttemptState,
) -> SubmissionDisplayState {
    SubmissionDisplayState {
        current: Some(current),
        attempt: Some(attempt),
    }
}

#[derive(Debug)]
struct ScenarioPlayback {
    problem: usize,
    next_step: usize,
    next_at: Instant,
}

#[derive(Debug)]
struct DemoHarness {
    app: WatchApp,
    submissions: SubmissionViewState,
    show_help: bool,
    playback: Option<ScenarioPlayback>,
}

impl DemoHarness {
    fn new() -> io::Result<Self> {
        let contest = Contest {
            contest_id: "awc0151".to_string(),
            problems: (b'A'..=b'E')
                .map(|label| {
                    let index = char::from(label).to_string();
                    Problem {
                        index: index.clone(),
                        title: format!("Demo Problem {index}"),
                        task_id: format!("awc0151_{}", index.to_ascii_lowercase()),
                        url: String::new(),
                        sample_count: 2,
                    }
                })
                .collect(),
        };
        let problem_count = contest.problems.len();
        let mut app = WatchApp::new_with_stress_cases(
            &contest,
            vec![2; problem_count],
            vec![None; problem_count],
        )?;
        for (problem, label) in (b'A'..=b'E').enumerate() {
            app.select_source(
                problem,
                PathBuf::from(format!("{}.cpp", char::from(label))),
                Language::Cpp,
            );
        }
        app.toggle_debug();
        app.select_problem(1);
        app.toggle_side_pane_mode();

        let ac = current(TuiSubmissionState::Status(SubmissionStatus::Finished(
            SubmissionResult::with_metrics(Verdict::Accepted, Some(234), Some(33_348)),
        )));
        let re = current(TuiSubmissionState::Status(SubmissionStatus::Finished(
            SubmissionResult::with_metrics(Verdict::RuntimeError, Some(31), Some(33_348)),
        )));
        let waiting = current(TuiSubmissionState::Status(
            SubmissionStatus::WaitingForJudge,
        ));
        let wa = current(TuiSubmissionState::Status(SubmissionStatus::Finished(
            SubmissionResult::with_metrics(Verdict::WrongAnswer, Some(266), Some(297_700)),
        )));
        let progress_re = current(TuiSubmissionState::Status(
            SubmissionStatus::JudgingProgress {
                judged: 7,
                total: 15,
                provisional: Some(Verdict::RuntimeError),
            },
        ));
        Ok(Self {
            app,
            submissions: SubmissionViewState {
                problems: vec![Some(ac), Some(waiting), Some(progress_re), None, None],
                history: vec![
                    demo_history_entry(1, "C", progress_re),
                    demo_history_entry(2, "A", ac),
                    demo_history_entry(3, "B", re),
                    demo_history_entry(4, "B", wa),
                    demo_history_entry(5, "B", waiting),
                ],
            },
            show_help: true,
            playback: None,
        })
    }

    fn selected_problem(&self) -> Option<usize> {
        self.app.selected_problem()
    }

    fn set_selected_submission(&mut self, state: Option<SubmissionDisplayState>) {
        self.playback = None;
        if let Some(problem) = self.selected_problem() {
            self.set_problem_submission(problem, state);
        }
    }

    fn set_problem_submission(&mut self, problem: usize, state: Option<SubmissionDisplayState>) {
        let Some(slot) = self.submissions.problems.get_mut(problem) else {
            return;
        };
        *slot = state;
        let problem = &self.app.problems()[problem];
        let problem_index = problem.index.clone();
        let key = SubmissionKey::new(self.app.contest_id(), problem.task_id.clone());
        if let Some(state) = state {
            if let Some(entry) = self
                .submissions
                .history
                .iter_mut()
                .rev()
                .find(|entry| entry.key == key)
            {
                entry.state = state;
            } else {
                let generation = self
                    .submissions
                    .history
                    .iter()
                    .map(|entry| entry.generation)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let entry = demo_history_entry(generation, &problem_index, state);
                self.submissions.history.push(entry);
            }
        }
    }

    fn cycle_composite(&mut self) {
        let Some(problem) = self.selected_problem() else {
            return;
        };
        let waiting = composite(
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
            TuiSubmissionAttemptState::Submitting,
        );
        let progress = composite(
            TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                judged: 14,
                total: 50,
                provisional: None,
            }),
            TuiSubmissionAttemptState::Submitting,
        );
        let unknown = composite(
            TuiSubmissionState::Status(SubmissionStatus::Finished(SubmissionResult::new(
                Verdict::Accepted,
            ))),
            TuiSubmissionAttemptState::Unknown,
        );
        let state = match self.submissions.problems.get(problem).copied().flatten() {
            Some(state) if state == waiting => progress,
            Some(state) if state == progress => unknown,
            _ => waiting,
        };
        self.set_selected_submission(Some(state));
    }

    fn toggle_scenario(&mut self, now: Instant) {
        if self.playback.take().is_some() {
            return;
        }
        let Some(problem) = self.selected_problem() else {
            return;
        };
        self.set_problem_submission(problem, Some(SCENARIO[0]));
        self.playback = Some(ScenarioPlayback {
            problem,
            next_step: 1,
            next_at: now + SCENARIO_INTERVAL,
        });
    }

    fn advance_scenario(&mut self, now: Instant) -> bool {
        let Some(playback) = self.playback.as_ref() else {
            return false;
        };
        if now < playback.next_at {
            return false;
        }

        let problem = playback.problem;
        let step = playback.next_step;
        self.set_problem_submission(problem, Some(SCENARIO[step]));
        if step + 1 == SCENARIO.len() {
            self.playback = None;
        } else {
            self.playback = Some(ScenarioPlayback {
                problem,
                next_step: step + 1,
                next_at: now + SCENARIO_INTERVAL,
            });
        }
        true
    }

    fn handle_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.control
        {
            return true;
        }
        if key.modifiers.control || key.modifiers.alt || key.modifiers.super_key {
            return false;
        }

        match handle_shortcut_help_key(&mut self.app, key) {
            ShortcutHelpKeyResult::Continue => {}
            ShortcutHelpKeyResult::Dismissed => {}
            ShortcutHelpKeyResult::Consumed { .. } => return false,
        }
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('?') {
            self.show_help = false;
            self.app.show_shortcut_help();
            return false;
        }

        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Char('q') | KeyCode::Escape => return true,
                KeyCode::Char('H') => self.show_help = !self.show_help,
                KeyCode::Char('v') => self.app.toggle_side_pane_mode(),
                KeyCode::Char('s') => self.app.toggle_side_pane(),
                KeyCode::Char('d') => self.app.toggle_debug(),
                KeyCode::Char(label @ 'a'..='e') => {
                    self.app.select_problem(usize::from(label as u8 - b'a'));
                }
                KeyCode::Char('1') => {
                    self.set_selected_submission(None);
                    self.submissions.history.clear();
                }
                KeyCode::Char('2') => self
                    .set_selected_submission(Some(attempt(TuiSubmissionAttemptState::Submitting))),
                KeyCode::Char('3') => {
                    self.set_selected_submission(Some(current(TuiSubmissionState::Accepted)));
                }
                KeyCode::Char('4') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
                ))),
                KeyCode::Char('5') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::WaitingForRejudge),
                ))),
                KeyCode::Char('6') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::Judging),
                ))),
                KeyCode::Char('7') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                        judged: 1,
                        total: 72,
                        provisional: None,
                    }),
                ))),
                KeyCode::Char('8') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                        judged: 55,
                        total: 72,
                        provisional: Some(Verdict::RuntimeError),
                    }),
                ))),
                KeyCode::Char('9') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::Finished(
                        SubmissionResult::with_metrics(Verdict::Accepted, Some(234), Some(33_348)),
                    )),
                ))),
                KeyCode::Char('0') => {
                    self.set_selected_submission(Some(current(TuiSubmissionState::Status(
                        SubmissionStatus::Finished(SubmissionResult::with_metrics(
                            Verdict::WrongAnswer,
                            Some(266),
                            Some(297_700),
                        )),
                    ))))
                }
                KeyCode::Char('r') => {
                    self.set_selected_submission(Some(current(TuiSubmissionState::Status(
                        SubmissionStatus::Finished(SubmissionResult::with_metrics(
                            Verdict::RuntimeError,
                            Some(31),
                            Some(33_348),
                        )),
                    ))))
                }
                KeyCode::Char('t') => {
                    self.set_selected_submission(Some(current(TuiSubmissionState::Status(
                        SubmissionStatus::Finished(SubmissionResult::with_metrics(
                            Verdict::TimeLimitExceeded,
                            Some(2_001),
                            Some(4_096),
                        )),
                    ))))
                }
                KeyCode::Char('n') => {
                    self.set_selected_submission(Some(current(
                        TuiSubmissionState::TrackingUnavailable,
                    )));
                }
                KeyCode::Char('u') => {
                    self.set_selected_submission(Some(attempt(TuiSubmissionAttemptState::Unknown)));
                }
                KeyCode::Char('x') => self.cycle_composite(),
                KeyCode::Char('p') => {
                    let generation = self
                        .submissions
                        .history
                        .last()
                        .map(|entry| entry.generation.saturating_add(1))
                        .unwrap_or(1);
                    self.submissions.history.push(demo_history_entry(
                        generation,
                        "C",
                        current(TuiSubmissionState::Status(
                            SubmissionStatus::JudgingProgress {
                                judged: 7,
                                total: 15,
                                provisional: Some(Verdict::RuntimeError),
                            },
                        )),
                    ));
                }
                KeyCode::Char('o') => {
                    let generation = self
                        .submissions
                        .history
                        .last()
                        .map(|entry| entry.generation.saturating_add(1))
                        .unwrap_or(1);
                    self.submissions.history.push(SubmissionHistoryEntry {
                        generation,
                        key: SubmissionKey::new("abc474", "abc474_b"),
                        problem_index: "B".to_string(),
                        problem_title: None,
                        language_label: "PyPy".to_string(),
                        started_at: SystemTime::now(),
                        submitted_at: None,
                        state: current(TuiSubmissionState::Status(
                            SubmissionStatus::WaitingForJudge,
                        )),
                    });
                }
                KeyCode::Char('g') => {
                    let replacement = Self::new().expect("the in-memory demo fixture must rebuild");
                    self.submissions = replacement.submissions;
                    self.playback = None;
                }
                KeyCode::Char(' ') => self.toggle_scenario(now),
                KeyCode::Enter
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Tab
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char(_) => {}
            }
        }

        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.app.previous_problem();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.app.next_problem();
            }
            _ => {}
        }
        false
    }

    fn wants_animation_redraw(&self) -> bool {
        !self.app.shortcut_help_visible()
            && self
                .submissions
                .history
                .last()
                .is_some_and(|entry| view::submission_animation_target(entry.state))
    }
}

fn demo_history_entry(
    generation: u64,
    problem_index: &str,
    state: SubmissionDisplayState,
) -> SubmissionHistoryEntry {
    SubmissionHistoryEntry {
        generation,
        key: SubmissionKey::new(
            "awc0151",
            format!("awc0151_{}", problem_index.to_ascii_lowercase()),
        ),
        problem_index: problem_index.to_string(),
        problem_title: Some(format!("Demo Problem {problem_index}")),
        language_label: "C++".to_string(),
        started_at: SystemTime::now()
            - Duration::from_secs(6_u64.saturating_sub(generation).saturating_mul(30)),
        submitted_at: Some(demo_submitted_at()),
        state,
    }
}

fn demo_submitted_at() -> OffsetDateTime {
    let date = Date::from_calendar_date(2026, Month::September, 9)
        .expect("the fixed demo submission date must be valid");
    let time = Time::from_hms(9, 18, 25).expect("the fixed demo submission time must be valid");
    let offset = UtcOffset::from_hms(9, 0, 0).expect("the fixed demo offset must be valid");
    PlainDateTime::new(date, time).assume_offset(offset)
}

pub(crate) fn run() -> io::Result<()> {
    let mut terminal = TerminaSession::start()?;
    let result = run_loop(&mut terminal);
    let restore = terminal.restore();
    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(io::Error::other(format!(
            "TUI demo failed: {error}; terminal restore also failed: {restore}"
        ))),
    }
}

fn run_loop(terminal: &mut TerminaSession) -> io::Result<()> {
    let mut demo = DemoHarness::new()?;
    let mut detail_layout = DetailLayout::default();
    let mut dirty = true;
    let submission_animation_epoch = Instant::now();
    let mut rendered_submission_animation_phase = None;

    loop {
        let now = Instant::now();
        dirty |= demo.advance_scenario(now);
        let submission_animation_phase = view::SubmissionAnimationPhase::from_elapsed(
            now.saturating_duration_since(submission_animation_epoch),
        );
        if demo.wants_animation_redraw()
            && rendered_submission_animation_phase != Some(submission_animation_phase)
        {
            dirty = true;
        }

        if dirty {
            let render_mouse_mode = terminal.mouse_mode();
            terminal.draw(|frame| {
                render_demo(
                    frame,
                    &demo,
                    &mut detail_layout,
                    render_mouse_mode,
                    submission_animation_phase,
                );
            })?;
            dirty = false;
            rendered_submission_animation_phase = demo
                .wants_animation_redraw()
                .then_some(submission_animation_phase);
            terminal.note_redraw_completed();
            terminal.refresh_mouse_after_redraw(false)?;
            terminal.retry_high_res_after_redraw(false)?;
            if terminal.mouse_mode() != render_mouse_mode {
                dirty = true;
                continue;
            }
        }

        if !terminal.poll(POLL_INTERVAL)? {
            continue;
        }
        for index in 0..MAX_EVENTS_PER_TICK {
            match terminal.read()? {
                TerminalEvent::Key(key) => {
                    if demo.handle_key(key, Instant::now()) {
                        return Ok(());
                    }
                    dirty = true;
                }
                TerminalEvent::Resize(_) => {
                    terminal.note_resize_dispatched();
                    dirty = true;
                }
                TerminalEvent::Paste(_) | TerminalEvent::Pointer(_) | TerminalEvent::Ignored => {}
            }
            if index + 1 == MAX_EVENTS_PER_TICK || !terminal.poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

fn render_demo(
    frame: &mut Frame<'_>,
    demo: &DemoHarness,
    detail_layout: &mut DetailLayout,
    mouse_mode: super::mouse::MouseMode,
    submission_animation_phase: view::SubmissionAnimationPhase,
) {
    view::render_frontend_with_pointer(
        frame,
        &demo.app,
        detail_layout,
        mouse_mode,
        None,
        None,
        false,
        view::FrontendOverlays {
            submission_view: Some(&demo.submissions),
            submission_animation_phase,
            ..view::FrontendOverlays::default()
        },
    );
    if demo.show_help {
        render_help(frame);
    }
}

fn render_help(frame: &mut Frame<'_>) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(2);
    let height = frame_area.height.saturating_sub(2).min(6);
    if width == 0 || height < 3 {
        return;
    }
    let area = Rect::new(
        frame_area.x.saturating_add(1),
        frame_area.bottom().saturating_sub(height).saturating_sub(1),
        width,
        height,
    );
    let title = Line::from(vec![
        Span::styled(
            "[DEMO controls]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  H hide"),
    ]);
    let help = Text::from(vec![
        Line::raw("a-e / left-right / h-l problem   v samples/submissions   s side pane   d debug"),
        Line::raw(
            "1 empty history  2 Submitting  3 internal Accepted→WJ  4 WJ  5 WR  6 Judging  7 1/72",
        ),
        Line::raw(
            "8 55/72 RE  9 AC  0 WA  r RE  t TLE  n Untracked  u Unknown  x current+attempt  g reset fixture",
        ),
        Line::raw(
            "p receipt C 7/15 RE  o receipt abc474/B WJ  ? shortcuts  Space play/stop  q / Esc quit",
        ),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(help).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::mouse::MouseMode;

    fn submission_animation_phase(index: u64) -> view::SubmissionAnimationPhase {
        view::SubmissionAnimationPhase::from_elapsed(Duration::from_millis(350 * index))
    }

    fn rendered_lines(demo: &DemoHarness, width: u16, height: u16) -> Vec<String> {
        rendered_lines_at_phase(
            demo,
            width,
            height,
            view::SubmissionAnimationPhase::default(),
        )
    }

    fn rendered_lines_at_phase(
        demo: &DemoHarness,
        width: u16,
        height: u16,
        submission_animation_phase: view::SubmissionAnimationPhase,
    ) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut detail_layout = DetailLayout::default();
        terminal
            .draw(|frame| {
                render_demo(
                    frame,
                    demo,
                    &mut detail_layout,
                    MouseMode::Disabled,
                    submission_animation_phase,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer.cell((column, row)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn rendered_text(demo: &DemoHarness, width: u16, height: u16) -> String {
        rendered_lines(demo, width, height).join("\n")
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            kind,
            modifiers: Default::default(),
        }
    }

    #[test]
    fn fixture_shows_compact_overview_and_same_problem_history_rows() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        demo.app.toggle_side_pane();

        let lines = rendered_lines(&demo, 100, 20);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("SUB │ A   B   C   D   E"))
        );
        assert!(lines.iter().any(|line| line.contains("Submissions")));
        for row in ["B  WJ", "B  WA", "B  RE", "A  AC", "C  7/15 RE"] {
            assert!(
                lines.iter().any(|line| line.contains(row)),
                "missing {row:?}"
            );
        }
        assert!(!lines[0].contains("SUB "));
        assert!(lines.iter().any(|line| {
            line.contains("B - Demo Problem B") && line.contains("C++") && line.contains("WJ")
        }));
    }

    #[test]
    fn demo_animates_all_unfinished_receipts_and_keeps_final_receipts_static() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        let unfinished = [
            (attempt(TuiSubmissionAttemptState::Submitting), "Submitting"),
            (current(TuiSubmissionState::Accepted), "WJ"),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                "WJ",
            ),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForRejudge,
                )),
                "WR",
            ),
            (
                current(TuiSubmissionState::Status(SubmissionStatus::Judging)),
                "Judging",
            ),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::JudgingProgress {
                        judged: 16,
                        total: 77,
                        provisional: None,
                    },
                )),
                "16/77",
            ),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::JudgingProgress {
                        judged: 55,
                        total: 72,
                        provisional: Some(Verdict::RuntimeError),
                    },
                )),
                "55/72 RE",
            ),
        ];
        for (state, label) in unfinished {
            demo.set_selected_submission(Some(state));
            assert!(demo.wants_animation_redraw(), "state={state:?}");
            let phase_zero = rendered_lines_at_phase(&demo, 160, 20, submission_animation_phase(0));
            let phase_three =
                rendered_lines_at_phase(&demo, 160, 20, submission_animation_phase(3));
            assert!(phase_zero[18].contains(&format!("{label}   ")));
            assert!(phase_three[18].contains(&format!("{label}...")));
            for row in 0..20 {
                if row != 18 {
                    assert_eq!(phase_three[row], phase_zero[row]);
                }
            }
        }

        for verdict in [Verdict::Accepted, Verdict::RuntimeError] {
            let state = current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                SubmissionResult::with_metrics(verdict, Some(234), Some(33_348)),
            )));
            demo.set_selected_submission(Some(state));
            assert!(!demo.wants_animation_redraw());
            assert_eq!(
                rendered_lines_at_phase(&demo, 160, 20, submission_animation_phase(3)),
                rendered_lines_at_phase(&demo, 160, 20, submission_animation_phase(0)),
            );
        }

        demo.set_selected_submission(Some(current(TuiSubmissionState::Status(
            SubmissionStatus::WaitingForJudge,
        ))));
        demo.app.show_shortcut_help();
        assert!(!demo.wants_animation_redraw());
    }

    #[test]
    fn demo_keys_cover_hidden_samples_submissions_and_empty_history() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        let now = Instant::now();

        assert!(!demo.app.side_pane_enabled());
        assert_eq!(
            demo.app.side_pane_mode(),
            super::super::app::SidePaneMode::Submissions
        );
        demo.handle_key(key(KeyCode::Char('v'), KeyEventKind::Press), now);
        assert!(!demo.app.side_pane_enabled());
        assert_eq!(
            demo.app.side_pane_mode(),
            super::super::app::SidePaneMode::Samples
        );

        demo.handle_key(key(KeyCode::Char('s'), KeyEventKind::Press), now);
        assert!(rendered_text(&demo, 100, 20).contains("Samples"));
        demo.handle_key(key(KeyCode::Char('v'), KeyEventKind::Press), now);
        assert!(demo.app.side_pane_enabled());
        assert!(rendered_text(&demo, 100, 20).contains("Submissions"));

        demo.handle_key(key(KeyCode::Char('1'), KeyEventKind::Press), now);
        assert!(rendered_text(&demo, 100, 20).contains("No submissions"));
        assert!(rendered_text(&demo, 100, 20).contains("? shortcuts"));
        assert!(rendered_text(&demo, 100, 20).contains(": commands"));
        demo.handle_key(key(KeyCode::Char('s'), KeyEventKind::Press), now);
        assert!(!demo.app.side_pane_enabled());
    }

    #[test]
    fn demo_exposes_provisional_and_cross_contest_current_footer_fixtures() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        let now = Instant::now();

        demo.handle_key(key(KeyCode::Char('p'), KeyEventKind::Press), now);
        assert!(rendered_text(&demo, 100, 20).contains("C - Demo Problem C"));
        assert!(rendered_text(&demo, 100, 20).contains("7/15 RE"));

        demo.handle_key(key(KeyCode::Char('o'), KeyEventKind::Press), now);
        assert!(rendered_text(&demo, 100, 20).contains("abc474/B"));
        assert!(rendered_text(&demo, 100, 20).contains("PyPy"));
        assert!(rendered_text(&demo, 100, 20).contains("WJ"));

        let mut final_demo = DemoHarness::new().unwrap();
        final_demo.show_help = false;
        final_demo.handle_key(key(KeyCode::Char('9'), KeyEventKind::Press), now);
        let accepted = rendered_text(&final_demo, 100, 20);
        assert!(accepted.contains("2026-09-09 09:18:25"));
        assert!(accepted.contains("AC"));
        assert!(accepted.contains("234 ms"));
        assert!(accepted.contains("33348 KiB"));
        final_demo.handle_key(key(KeyCode::Char('0'), KeyEventKind::Press), now);
        let wrong_answer = rendered_text(&final_demo, 100, 20);
        assert!(wrong_answer.contains("WA"));
        assert!(wrong_answer.contains("266 ms"));
        assert!(wrong_answer.contains("297700 KiB"));
    }

    #[test]
    fn current_problem_update_does_not_mutate_cross_contest_same_index() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        let now = Instant::now();
        demo.handle_key(key(KeyCode::Char('o'), KeyEventKind::Press), now);
        let cross = demo.submissions.history.last().unwrap();
        let cross_generation = cross.generation;
        let cross_state = cross.state;

        demo.handle_key(key(KeyCode::Char('9'), KeyEventKind::Press), now);

        let cross = demo
            .submissions
            .history
            .iter()
            .find(|entry| entry.generation == cross_generation)
            .unwrap();
        assert_eq!(cross.key, SubmissionKey::new("abc474", "abc474_b"));
        assert_eq!(cross.state, cross_state);
        let current_entry = demo
            .submissions
            .history
            .iter()
            .rev()
            .find(|entry| entry.key == SubmissionKey::new("awc0151", "awc0151_b"))
            .unwrap();
        assert_eq!(
            current_entry.state,
            current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                SubmissionResult::with_metrics(Verdict::Accepted, Some(234), Some(33_348))
            )))
        );
        let receipt = rendered_text(&demo, 100, 20);
        assert!(receipt.contains("abc474/B"));
        assert!(receipt.contains("WJ"));
    }

    #[test]
    fn demo_shortcut_help_is_one_shot_and_preserves_following_actions() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        let now = Instant::now();

        assert!(!demo.app.shortcut_help_visible());
        assert!(!demo.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press), now));
        assert!(demo.app.shortcut_help_visible());
        let help = rendered_text(&demo, 100, 20);
        for label in [": commands", "t submit", "q quit", "s pane", "v view"] {
            assert!(help.contains(label), "missing help label {label:?}");
        }

        assert!(!demo.handle_key(key(KeyCode::Char('?'), KeyEventKind::Repeat), now));
        assert!(!demo.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press), now));
        assert!(demo.app.shortcut_help_visible());

        assert!(!demo.handle_key(key(KeyCode::Char('t'), KeyEventKind::Press), now));
        assert!(!demo.app.shortcut_help_visible());
        assert!(rendered_text(&demo, 100, 20).contains("TLE"));

        demo.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press), now);
        let pane = demo.app.side_pane_enabled();
        demo.handle_key(key(KeyCode::Char('s'), KeyEventKind::Press), now);
        assert!(!demo.app.shortcut_help_visible());
        assert_ne!(demo.app.side_pane_enabled(), pane);

        demo.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press), now);
        assert!(!demo.handle_key(key(KeyCode::Escape, KeyEventKind::Press), now));
        assert!(!demo.app.shortcut_help_visible());
    }

    #[test]
    fn scenario_updates_one_history_entry_to_the_final_verdict() {
        let mut demo = DemoHarness::new().unwrap();
        let mut now = Instant::now();
        let history_len = demo.submissions.history.len();
        demo.toggle_scenario(now);

        for expected in &SCENARIO[1..] {
            now += SCENARIO_INTERVAL;
            assert!(demo.advance_scenario(now));
            assert_eq!(demo.submissions.problems[1], Some(*expected));
            assert_eq!(demo.submissions.history.len(), history_len);
            assert_eq!(demo.submissions.history.last().unwrap().state, *expected);
        }
        assert!(demo.playback.is_none());
        assert_eq!(
            demo.submissions.history.last().unwrap().state,
            SCENARIO.last().copied().unwrap()
        );
    }

    #[test]
    fn narrow_demo_frames_do_not_panic() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        demo.app.toggle_side_pane();
        for (width, height) in [
            (0, 0),
            (1, 1),
            (2, 3),
            (8, 5),
            (28, 8),
            (51, 12),
            (52, 12),
            (53, 12),
        ] {
            let _ = rendered_text(&demo, width, height);
        }
    }

    #[test]
    fn repeat_is_accepted_only_for_problem_navigation() {
        let mut demo = DemoHarness::new().unwrap();
        let now = Instant::now();

        assert!(!demo.handle_key(key(KeyCode::Right, KeyEventKind::Repeat), now));
        assert_eq!(demo.selected_problem(), Some(2));
        let state = demo.app.side_pane_state();
        for code in [
            KeyCode::Char('1'),
            KeyCode::Char('p'),
            KeyCode::Char('o'),
            KeyCode::Char('g'),
            KeyCode::Char(' '),
            KeyCode::Char('v'),
            KeyCode::Char('s'),
            KeyCode::Char('d'),
            KeyCode::Char('?'),
            KeyCode::Char('q'),
        ] {
            assert!(!demo.handle_key(key(code, KeyEventKind::Repeat), now));
        }
        assert_eq!(demo.app.side_pane_state(), state);
        assert!(demo.playback.is_none());
    }
}
