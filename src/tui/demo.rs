//! Interactive, in-memory visual fixture for the production watch TUI renderer.
//!
//! This module is debug-only on purpose. Its event loop owns no production workers or backend
//! handles, so a demo key cannot start a run, open an editor, touch a workspace, or submit.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::app::WatchApp;
use super::detail_layout::DetailLayout;
use super::submission::{
    SubmissionDisplayState, SubmissionHeaderState, SubmissionViewState, TuiSubmissionAttemptState,
    TuiSubmissionState,
};
use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::{TerminaSession, view};
use crate::atcoder::submission_tracking::{SubmissionStatus, Verdict};
use crate::language::Language;
use crate::model::{Contest, Problem};
use view::SubmissionAnimationPhase;

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const ANIMATION_REDRAW_INTERVAL: Duration = Duration::from_millis(80);
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
        Verdict::RuntimeError,
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
    cross_contest_header: bool,
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
        app.toggle_problem_status_mode();

        let ac = current(TuiSubmissionState::Status(SubmissionStatus::Finished(
            Verdict::Accepted,
        )));
        let re = current(TuiSubmissionState::Status(SubmissionStatus::Finished(
            Verdict::RuntimeError,
        )));
        Ok(Self {
            app,
            submissions: SubmissionViewState {
                latest: Some(SubmissionHeaderState {
                    problem_label: "B".to_string(),
                    state: re,
                }),
                problems: vec![Some(ac), Some(re), None, None, None],
            },
            show_help: true,
            cross_contest_header: false,
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
        self.cross_contest_header = false;
        self.submissions.latest = state.map(|state| SubmissionHeaderState {
            problem_label: self.app.problems()[problem].index.clone(),
            state,
        });
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
            TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
            TuiSubmissionAttemptState::Unknown,
        );
        let state = match self.submissions.problems.get(problem).copied().flatten() {
            Some(state) if state == waiting => progress,
            Some(state) if state == progress => unknown,
            _ => waiting,
        };
        self.set_selected_submission(Some(state));
    }

    fn toggle_cross_contest_header(&mut self) {
        self.playback = None;
        self.cross_contest_header = !self.cross_contest_header;
        if self.cross_contest_header {
            self.submissions.latest = Some(SubmissionHeaderState {
                problem_label: "abc474/A".to_string(),
                state: current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted,
                ))),
            });
        } else {
            self.refresh_selected_header();
        }
    }

    fn refresh_selected_header(&mut self) {
        self.submissions.latest = self.selected_problem().and_then(|problem| {
            self.submissions
                .problems
                .get(problem)
                .copied()
                .flatten()
                .map(|state| SubmissionHeaderState {
                    problem_label: self.app.problems()[problem].index.clone(),
                    state,
                })
        });
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

        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Char('q') | KeyCode::Escape => return true,
                KeyCode::Char('?') => self.show_help = !self.show_help,
                KeyCode::Char('v') => self.app.toggle_problem_status_mode(),
                KeyCode::Char('s') => self.app.toggle_samples_pane(),
                KeyCode::Char('d') => self.app.toggle_debug(),
                KeyCode::Char(label @ 'a'..='e') => {
                    self.app.select_problem(usize::from(label as u8 - b'a'));
                }
                KeyCode::Char('1') => self.set_selected_submission(None),
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
                    TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
                ))),
                KeyCode::Char('0') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::WrongAnswer)),
                ))),
                KeyCode::Char('r') => self.set_selected_submission(Some(current(
                    TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::RuntimeError)),
                ))),
                KeyCode::Char('t') => {
                    self.set_selected_submission(Some(current(TuiSubmissionState::Status(
                        SubmissionStatus::Finished(Verdict::TimeLimitExceeded),
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
                KeyCode::Char('g') => self.toggle_cross_contest_header(),
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
        self.submissions
            .latest
            .as_ref()
            .is_some_and(|latest| view::submission_animation_target(latest.state))
            || (self.app.problem_status_mode() == super::app::ProblemStatusMode::Submissions
                && self
                    .submissions
                    .problems
                    .iter()
                    .flatten()
                    .copied()
                    .any(view::submission_animation_target))
    }
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
    let animation_epoch = Instant::now();
    let mut next_animation_redraw = Instant::now();

    loop {
        let now = Instant::now();
        dirty |= demo.advance_scenario(now);
        if demo.wants_animation_redraw() && now >= next_animation_redraw {
            dirty = true;
            next_animation_redraw = now + ANIMATION_REDRAW_INTERVAL;
        }

        if dirty {
            let render_mouse_mode = terminal.mouse_mode();
            let submission_animation_phase =
                SubmissionAnimationPhase::from_elapsed(now.duration_since(animation_epoch));
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
    submission_animation_phase: SubmissionAnimationPhase,
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
        Span::raw("  ? hide"),
    ]);
    let help = Text::from(vec![
        Line::raw(
            "a-e / left-right / h-l problem   v samples/submissions   s samples pane   d debug",
        ),
        Line::raw("1 none  2 Submitting  3 internal Accepted→WJ  4 WJ  5 WR  6 Judging  7 1/72"),
        Line::raw(
            "8 55/72 RE  9 AC  0 WA  r RE  t TLE  n Untracked  u Unknown  x current+attempt  g cross-contest",
        ),
        Line::raw("Space play/stop scenario   q / Esc / Ctrl-C quit"),
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
    use ratatui::backend::TestBackend;
    use ratatui::{Terminal, buffer::Buffer};

    use super::*;
    use crate::tui::mouse::MouseMode;

    fn rendered_buffer(demo: &DemoHarness, width: u16, height: u16) -> Buffer {
        rendered_buffer_at_phase(demo, width, height, SubmissionAnimationPhase::default())
    }

    fn rendered_buffer_at_phase(
        demo: &DemoHarness,
        width: u16,
        height: u16,
        submission_animation_phase: SubmissionAnimationPhase,
    ) -> Buffer {
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
        terminal.backend().buffer().clone()
    }

    fn rendered_text(demo: &DemoHarness, width: u16, height: u16) -> String {
        rendered_buffer(demo, width, height)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn find_cell_sequence(buffer: &Buffer, needle: &str) -> (u16, u16) {
        for y in 0..buffer.area.height {
            for start_x in 0..buffer.area.width {
                let mut candidate = String::new();
                for x in start_x..buffer.area.width {
                    candidate.push_str(buffer.cell((x, y)).unwrap().symbol());
                    if candidate == needle {
                        return (start_x, y);
                    }
                    if !needle.starts_with(&candidate) {
                        break;
                    }
                }
            }
        }
        panic!("could not find {needle:?} in rendered demo buffer");
    }

    #[test]
    fn in_memory_fixture_renders_through_the_production_frontend() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;

        let rendered = rendered_text(&demo, 160, 20);
        assert!(rendered.contains("awc0151 │ B.cpp │ C++ │ DEBUG ON │ Idle │ SUB B RE"));
        assert!(rendered.contains("SUB │ A AC   B RE   C ·   D ·   E ·"));
    }

    #[test]
    fn demo_states_render_the_user_visible_submission_vocabulary() {
        let cases = [
            (
                attempt(TuiSubmissionAttemptState::Submitting),
                "Submitting",
                Color::Yellow,
            ),
            (current(TuiSubmissionState::Accepted), "WJ", Color::Yellow),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                "WJ",
                Color::Yellow,
            ),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForRejudge,
                )),
                "WR",
                Color::Yellow,
            ),
            (
                current(TuiSubmissionState::Status(SubmissionStatus::Judging)),
                "Judging",
                Color::Yellow,
            ),
            (
                current(TuiSubmissionState::Status(
                    SubmissionStatus::JudgingProgress {
                        judged: 1,
                        total: 72,
                        provisional: None,
                    },
                )),
                "1/72",
                Color::Yellow,
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
                Color::Yellow,
            ),
            (
                current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted,
                ))),
                "AC",
                Color::Green,
            ),
            (
                current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::WrongAnswer,
                ))),
                "WA",
                Color::Red,
            ),
            (
                current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::RuntimeError,
                ))),
                "RE",
                Color::Red,
            ),
            (
                current(TuiSubmissionState::TrackingUnavailable),
                "Untracked",
                Color::Yellow,
            ),
            (
                attempt(TuiSubmissionAttemptState::Unknown),
                "Unknown",
                Color::Red,
            ),
        ];

        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        for (state, expected, expected_color) in cases {
            demo.set_selected_submission(Some(state));
            let buffer = rendered_buffer(&demo, 160, 20);
            let rendered = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                rendered.contains(&format!("SUB B {expected}")),
                "state={state:?}\n{rendered}"
            );
            assert!(
                rendered.contains(&format!("B {expected}")),
                "state={state:?}\n{rendered}"
            );
            assert!(!rendered.contains("NEW"), "state={state:?}\n{rendered}");
            assert!(
                !rendered.contains("Accepted"),
                "state={state:?}\n{rendered}"
            );

            let header = format!("SUB B {expected}");
            let (header_x, header_y) = find_cell_sequence(&buffer, &header);
            let status_x = header_x + u16::try_from("SUB B ".chars().count()).unwrap();
            assert_eq!(
                buffer.cell((status_x, header_y)).unwrap().fg,
                expected_color,
                "state={state:?}"
            );
            if expected == "55/72 RE" {
                let verdict_x = header_x + u16::try_from("SUB B 55/72 ".chars().count()).unwrap();
                assert_eq!(buffer.cell((verdict_x, header_y)).unwrap().fg, Color::Red);
            }
        }
    }

    #[test]
    fn demo_animates_only_wj_and_judging_through_the_production_renderer() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        for (status, expected) in [
            (
                SubmissionStatus::WaitingForJudge,
                ["WJ   ", "WJ.  ", "WJ.. ", "WJ..."],
            ),
            (
                SubmissionStatus::Judging,
                ["Judging   ", "Judging.  ", "Judging.. ", "Judging..."],
            ),
        ] {
            demo.set_selected_submission(Some(current(TuiSubmissionState::Status(status))));
            assert!(demo.wants_animation_redraw());
            for (index, expected) in expected.into_iter().enumerate() {
                let phase = SubmissionAnimationPhase::from_elapsed(Duration::from_millis(
                    350 * index as u64,
                ));
                let rendered = rendered_buffer_at_phase(&demo, 160, 20, phase)
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    rendered.contains(&format!("SUB B {expected}")),
                    "status={status:?} phase={index}\n{rendered}"
                );
            }
        }

        for status in [
            SubmissionStatus::JudgingProgress {
                judged: 14,
                total: 72,
                provisional: None,
            },
            SubmissionStatus::Finished(Verdict::Accepted),
        ] {
            demo.set_selected_submission(Some(current(TuiSubmissionState::Status(status))));
            assert!(!demo.wants_animation_redraw());
            let phase_zero =
                rendered_buffer_at_phase(&demo, 160, 20, SubmissionAnimationPhase::default());
            let phase_three = rendered_buffer_at_phase(
                &demo,
                160,
                20,
                SubmissionAnimationPhase::from_elapsed(Duration::from_millis(1050)),
            );
            assert_eq!(phase_zero, phase_three);
        }
    }

    #[test]
    fn overview_selection_moves_only_problem_label_modifiers() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;

        let selected_b = rendered_buffer(&demo, 160, 20);
        let (row_x, row_y) = find_cell_sequence(&selected_b, "SUB │ A AC   B RE");
        let offset = |prefix: &str| row_x + u16::try_from(prefix.chars().count()).unwrap();
        let a_label_x = offset("SUB │ ");
        let ac_x = offset("SUB │ A ");
        let b_label_x = offset("SUB │ A AC   ");
        let re_x = offset("SUB │ A AC   B ");
        assert!(
            !selected_b
                .cell((a_label_x, row_y))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            selected_b
                .cell((b_label_x, row_y))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        let ac_style = selected_b.cell((ac_x, row_y)).unwrap().style();
        let re_style = selected_b.cell((re_x, row_y)).unwrap().style();
        assert_eq!(ac_style.fg, Some(Color::Green));
        assert_eq!(re_style.fg, Some(Color::Red));
        assert_eq!(ac_style.add_modifier, Modifier::empty());
        assert_eq!(re_style.add_modifier, Modifier::empty());

        demo.app.select_problem(0);
        let selected_a = rendered_buffer(&demo, 160, 20);
        let (row_x, row_y) = find_cell_sequence(&selected_a, "SUB │ A AC   B RE");
        let offset = |prefix: &str| row_x + u16::try_from(prefix.chars().count()).unwrap();
        assert!(
            selected_a
                .cell((offset("SUB │ "), row_y))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert!(
            !selected_a
                .cell((offset("SUB │ A AC   "), row_y))
                .unwrap()
                .modifier
                .intersects(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(
            selected_a
                .cell((offset("SUB │ A "), row_y))
                .unwrap()
                .style(),
            ac_style
        );
        assert_eq!(
            selected_a
                .cell((offset("SUB │ A AC   B "), row_y))
                .unwrap()
                .style(),
            re_style
        );
    }

    #[test]
    fn current_and_attempt_fixture_shows_only_the_attempt() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        demo.app.select_problem(0);

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
            TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
            TuiSubmissionAttemptState::Unknown,
        );

        demo.cycle_composite();

        let rendered = rendered_text(&demo, 160, 20);
        assert_eq!(demo.submissions.problems[0], Some(waiting));
        assert!(rendered.contains("SUB A Submitting"));
        assert!(!rendered.contains("SUB A WJ"));
        assert!(!rendered.contains("NEW"));
        assert!(rendered.contains("A Submitting"));

        demo.cycle_composite();
        let rendered = rendered_text(&demo, 160, 20);
        assert_eq!(demo.submissions.problems[0], Some(progress));
        assert!(rendered.contains("SUB A Submitting"));
        assert!(!rendered.contains("SUB A 14/50"));
        assert!(!rendered.contains("NEW"));
        assert!(rendered.contains("A Submitting"));

        demo.cycle_composite();
        let rendered = rendered_text(&demo, 160, 20);
        assert_eq!(demo.submissions.problems[0], Some(unknown));
        assert!(rendered.contains("SUB A Unknown"));
        assert!(rendered.contains("SUB │ A Unknown"));
        assert!(!rendered.contains("SUB A AC"));
        assert!(!rendered.contains("A AC"));
        assert!(!rendered.contains("AC ·"));
        assert!(!rendered.contains(" · NEW"));

        demo.cycle_composite();
        assert_eq!(demo.submissions.problems[0], Some(waiting));
    }

    #[test]
    fn both_problem_status_modes_and_narrow_frames_render_without_panicking() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        assert!(rendered_text(&demo, 100, 12).contains("SUB │"));
        demo.app.toggle_problem_status_mode();
        assert!(!rendered_text(&demo, 100, 12).contains("SUB │"));

        for width in [1, 2, 8, 16, 28] {
            for height in [1, 2, 3, 5, 8] {
                let _ = rendered_text(&demo, width, height);
            }
        }
    }

    #[test]
    fn global_latest_and_cross_contest_header_follow_demo_updates() {
        let mut demo = DemoHarness::new().unwrap();
        demo.show_help = false;
        demo.app.select_problem(2);
        demo.set_selected_submission(Some(current(TuiSubmissionState::Status(
            SubmissionStatus::WaitingForJudge,
        ))));
        assert!(rendered_text(&demo, 160, 20).contains("SUB C WJ"));

        demo.toggle_cross_contest_header();
        assert!(rendered_text(&demo, 160, 20).contains("SUB abc474/A AC"));
    }

    #[test]
    fn scenario_advances_in_memory_to_the_final_verdict() {
        let mut demo = DemoHarness::new().unwrap();
        let mut now = Instant::now();
        demo.toggle_scenario(now);
        assert_eq!(demo.submissions.problems[1], Some(SCENARIO[0]));

        for expected in &SCENARIO[1..] {
            now += SCENARIO_INTERVAL;
            assert!(demo.advance_scenario(now));
            assert_eq!(demo.submissions.problems[1], Some(*expected));
        }
        assert!(demo.playback.is_none());
        assert_eq!(
            demo.submissions.latest.as_ref().map(|latest| latest.state),
            SCENARIO.last().copied()
        );
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            kind,
            modifiers: Default::default(),
        }
    }

    #[test]
    fn repeat_is_accepted_only_for_problem_navigation() {
        let mut demo = DemoHarness::new().unwrap();
        let now = Instant::now();

        assert!(!demo.handle_key(key(KeyCode::Right, KeyEventKind::Repeat), now));
        assert_eq!(demo.selected_problem(), Some(2));
        assert!(!demo.handle_key(key(KeyCode::Char('h'), KeyEventKind::Repeat), now));
        assert_eq!(demo.selected_problem(), Some(1));

        let submissions = demo.submissions.clone();
        let show_help = demo.show_help;
        for code in [
            KeyCode::Char('4'),
            KeyCode::Char('x'),
            KeyCode::Char('g'),
            KeyCode::Char(' '),
            KeyCode::Char('v'),
            KeyCode::Char('d'),
            KeyCode::Char('?'),
            KeyCode::Char('q'),
        ] {
            assert!(!demo.handle_key(key(code, KeyEventKind::Repeat), now));
        }
        assert_eq!(demo.submissions, submissions);
        assert_eq!(demo.show_help, show_help);
        assert!(!demo.cross_contest_header);
        assert!(demo.playback.is_none());
        assert!(demo.app.debug_enabled());
        assert!(rendered_text(&demo, 100, 12).contains("SUB │"));

        assert!(!demo.handle_key(key(KeyCode::Right, KeyEventKind::Release), now));
        assert_eq!(demo.selected_problem(), Some(1));
    }
}
