pub mod app;
mod detail;
pub(crate) mod detail_analysis;
mod detail_layout;
mod detail_scrollbar;
pub mod message;
mod mouse;
pub mod reporter;
mod termina_adapter;
mod terminal;
pub mod view;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crate::model::Contest;
use app::WatchApp;
use detail_layout::{DetailAnalysisCommand, DetailAnalysisResult};
use detail_scrollbar::{DetailScrollbarHit, DetailScrollbarStableIdentity};
use message::{Message, RunRequest};
use mouse::{
    MouseMode, TerminalPixelMetrics, normalize_absolute_pixels, project_absolute_pixels_to_cells,
};
pub(crate) use terminal::TerminaSession;
use terminal::{
    KeyCode, KeyEvent, KeyEventKind, PointerButton, PointerEvent, PointerKind, TerminalEvent,
};

const MAX_MESSAGES_PER_TICK: usize = 256;
const MAX_DETAIL_ANALYSIS_RESULTS_PER_TICK: usize = 64;
const MAX_TERMINAL_EVENTS_PER_TICK: usize = 256;
const DETAIL_SCROLL_LINES: usize = 3;
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetailScrollbarDrag {
    identity: DetailScrollbarStableIdentity,
    coordinate: DragCoordinate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragCoordinate {
    Cells {
        grab_offset: u16,
    },
    Pixels {
        grab_offset_px: u64,
        generation: u64,
    },
}

#[derive(Debug, Default)]
struct DetailScrollbarDragState {
    active: Option<DetailScrollbarDrag>,
}

impl DetailScrollbarDragState {
    fn cancel(&mut self) {
        self.active = None;
    }

    fn reconcile_render_info(&mut self, render_info: &view::RenderInfo) {
        if self.active.is_some_and(|drag| {
            render_info
                .detail_scrollbar
                .as_ref()
                .is_none_or(|scrollbar| scrollbar.identity != drag.identity)
        }) {
            self.cancel();
        }
    }
}

fn send_run_request(run_tx: &Sender<RunRequest>, request: RunRequest) -> io::Result<()> {
    run_tx.send(request).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "run worker request channel disconnected",
        )
    })
}

fn queue_problem_run(
    app: &mut WatchApp,
    problem: usize,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    let Some(request) = app.queue_run(problem) else {
        return Ok(false);
    };

    send_run_request(run_tx, request)?;

    Ok(true)
}

fn queue_problem_stress(
    app: &mut WatchApp,
    problem: usize,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    let base_seed = crate::stress::automatic_seed()?;
    let Some(request) = app.queue_stress(problem, base_seed) else {
        return Ok(false);
    };

    send_run_request(run_tx, request)?;

    Ok(true)
}

fn handle_messages(
    app: &mut WatchApp,
    message_rx: &Receiver<Message>,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    let mut changed = false;

    for _ in 0..MAX_MESSAGES_PER_TICK {
        match message_rx.try_recv() {
            Ok(Message::SourceChanged {
                problem,
                path,
                language,
            }) => {
                if app.source_changed(problem, path, language) {
                    changed = true;
                    queue_problem_run(app, problem, run_tx)?;
                }
            }

            Ok(Message::WatcherFailed(error)) => {
                return Err(error);
            }

            Ok(Message::WorkerFailed(error)) => {
                return Err(error);
            }

            Err(TryRecvError::Empty) => {
                break;
            }

            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "background message channel disconnected",
                ));
            }
            Ok(Message::RunStarted { run_id, problem }) => {
                if app.run_started(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunRequeued { run_id, problem }) => {
                if app.run_requeued(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunEvent {
                run_id,
                problem,
                event,
            }) => {
                if app.run_event(problem, run_id, event) {
                    changed = true;
                }
            }

            Ok(Message::StressEvent {
                run_id,
                problem,
                event,
            }) => {
                if app.stress_event(problem, run_id, event) {
                    changed = true;
                }
            }

            Ok(Message::RunCompleted { run_id, problem }) => {
                if app.run_completed(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunFailed {
                run_id,
                problem,
                error,
            }) => {
                if app.run_failed(problem, run_id, error) {
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
}

fn handle_detail_analysis_results(
    detail_layout: &mut detail_layout::DetailLayout,
    current_detail_revision: u64,
    result_rx: &Receiver<DetailAnalysisResult>,
) -> io::Result<bool> {
    let mut changed = false;

    for _ in 0..MAX_DETAIL_ANALYSIS_RESULTS_PER_TICK {
        match result_rx.try_recv() {
            Ok(result) => {
                let result_revision = match &result {
                    DetailAnalysisResult::StructureReady(result) => result.identity.revision,
                    DetailAnalysisResult::Count(result) => result.identity.revision,
                };
                if result_revision == current_detail_revision {
                    changed |= detail_layout.apply_analysis_result(result);
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "detail analysis worker result channel disconnected",
                ));
            }
        }
    }

    Ok(changed)
}

fn apply_detail_scroll_reconciliation(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
) -> bool {
    detail_layout
        .take_scroll_reconciliation()
        .is_some_and(|absolute_row| app.reconcile_detail_scroll(absolute_row))
}

fn send_detail_analysis_command(
    command_tx: &Sender<DetailAnalysisCommand>,
    command: DetailAnalysisCommand,
) -> io::Result<()> {
    command_tx.send(command).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "detail analysis worker request channel disconnected",
        )
    })
}

pub(crate) fn run(
    terminal: &mut TerminaSession,
    contest: &Contest,
    sample_counts: Vec<usize>,
    stress_cases: Vec<Option<crate::model::Sample>>,
    message_rx: Receiver<Message>,
    run_tx: Sender<RunRequest>,
    detail_analysis_tx: Sender<DetailAnalysisCommand>,
    detail_analysis_rx: Receiver<DetailAnalysisResult>,
) -> io::Result<()> {
    let mut app = WatchApp::new_with_stress_cases(contest, sample_counts, stress_cases)?;

    let mut dirty = true;

    let mut render_info = view::RenderInfo::default();
    let mut detail_layout = detail_layout::DetailLayout::default();
    let mut detail_scrollbar_drag = DetailScrollbarDragState::default();
    let mut terminal_events = VecDeque::new();

    while !app.should_quit() {
        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(terminal, Duration::ZERO)?;
        }

        if contains_quit_event(&terminal_events) {
            app.quit();
            break;
        }

        if take_leading_resizes(&mut terminal_events) {
            detail_scrollbar_drag.cancel();
            discard_stale_pixel_events(&mut terminal_events);
            terminal.note_resize_dispatched();
            dirty = true;
        }

        if handle_messages(&mut app, &message_rx, &run_tx)? {
            dirty = true;
        }

        if handle_detail_analysis_results(
            &mut detail_layout,
            app.detail_revision(),
            &detail_analysis_rx,
        )? {
            dirty = true;
        }
        if apply_detail_scroll_reconciliation(&mut app, &mut detail_layout) {
            dirty = true;
        }

        // message batch処理中にqが到着していれば、重いwrap/再描画より優先する。
        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(terminal, Duration::ZERO)?;
        }

        if contains_quit_event(&terminal_events) {
            app.quit();
            break;
        }

        if take_leading_resizes(&mut terminal_events) {
            detail_scrollbar_drag.cancel();
            discard_stale_pixel_events(&mut terminal_events);
            terminal.note_resize_dispatched();
            dirty = true;
        }

        if dirty {
            let mut next_render_info = view::RenderInfo::default();

            terminal.draw(|frame| {
                next_render_info = view::render(frame, &app, &mut detail_layout);
            })?;

            render_info = next_render_info;
            detail_scrollbar_drag.reconcile_render_info(&render_info);

            apply_detail_scroll_reconciliation(&mut app, &mut detail_layout);

            if let Some(max_detail_scroll) = render_info.max_detail_scroll {
                app.clamp_detail_scroll(max_detail_scroll);
            }

            if let Some(command) = detail_layout.take_analysis_command() {
                send_detail_analysis_command(&detail_analysis_tx, command)?;
            }

            dirty = false;
            terminal.note_redraw_completed();
            let resize_pending = resize_event_count(&terminal_events) != 0;
            terminal.refresh_mouse_after_redraw(resize_pending)?;
            terminal.retry_high_res_after_redraw(resize_pending)?;
        }

        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(terminal, TERMINAL_POLL_INTERVAL)?;
        }

        // qは同じbatch内のresize/mouseより先に扱い、再描画を挟まず終了する。
        if contains_quit_event(&terminal_events) {
            app.quit();
            continue;
        }

        let resize_count_before = resize_event_count(&terminal_events);
        if handle_terminal_events_with_mouse_mode(
            &mut app,
            &mut detail_layout,
            &mut detail_scrollbar_drag,
            &render_info,
            &mut terminal_events,
            &run_tx,
            terminal.mouse_mode(),
        )? {
            dirty = true;
        }
        if resize_event_count(&terminal_events) < resize_count_before {
            discard_stale_pixel_events(&mut terminal_events);
            terminal.note_resize_dispatched();
        }
    }

    Ok(())
}

fn read_terminal_events(
    terminal: &mut TerminaSession,
    wait: Duration,
) -> io::Result<VecDeque<TerminalEvent>> {
    let mut events = VecDeque::new();

    if !terminal.poll(wait)? {
        return Ok(events);
    }

    for index in 0..MAX_TERMINAL_EVENTS_PER_TICK {
        let terminal_event = terminal.read()?;
        let should_quit = is_quit_event(&terminal_event);
        events.push_back(terminal_event);

        if should_quit
            || index + 1 == MAX_TERMINAL_EVENTS_PER_TICK
            || !terminal.poll(Duration::ZERO)?
        {
            break;
        }
    }

    Ok(events)
}

fn resize_event_count(events: &VecDeque<TerminalEvent>) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, TerminalEvent::Resize(_)))
        .count()
}

fn discard_stale_pixel_events(events: &mut VecDeque<TerminalEvent>) {
    events.retain(|event| {
        !matches!(
            event,
            TerminalEvent::Pointer(PointerEvent {
                position: terminal::PointerPosition::AbsolutePixels { .. },
                ..
            })
        )
    });
}

#[cfg(test)]
fn read_terminal_events_with(
    wait: Duration,
    mut poll_event: impl FnMut(Duration) -> io::Result<bool>,
    mut read_event: impl FnMut() -> io::Result<TerminalEvent>,
) -> io::Result<VecDeque<TerminalEvent>> {
    let mut events = VecDeque::new();

    if !poll_event(wait)? {
        return Ok(events);
    }

    for index in 0..MAX_TERMINAL_EVENTS_PER_TICK {
        let terminal_event = read_event()?;
        let should_quit = is_quit_event(&terminal_event);
        events.push_back(terminal_event);

        if should_quit || index + 1 == MAX_TERMINAL_EVENTS_PER_TICK || !poll_event(Duration::ZERO)?
        {
            break;
        }
    }

    Ok(events)
}

fn contains_quit_event(events: &VecDeque<TerminalEvent>) -> bool {
    events.iter().any(is_quit_event)
}

fn is_quit_event(terminal_event: &TerminalEvent) -> bool {
    matches!(
        terminal_event,
        TerminalEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            kind: KeyEventKind::Press,
            ..
        })
    )
}

fn take_leading_resizes(events: &mut VecDeque<TerminalEvent>) -> bool {
    let mut found = false;

    while matches!(events.front(), Some(TerminalEvent::Resize(_))) {
        events.pop_front();
        found = true;
    }

    found
}

#[cfg(test)]
fn handle_terminal_events(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    render_info: &view::RenderInfo,
    events: &mut VecDeque<TerminalEvent>,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    handle_terminal_events_with_mouse_mode(
        app,
        detail_layout,
        detail_scrollbar_drag,
        render_info,
        events,
        run_tx,
        MouseMode::Cells,
    )
}

fn handle_terminal_events_with_mouse_mode(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    render_info: &view::RenderInfo,
    events: &mut VecDeque<TerminalEvent>,
    run_tx: &Sender<RunRequest>,
    mouse_mode: MouseMode,
) -> io::Result<bool> {
    let mut changed = false;
    let mut scrollbar_geometry_changed_by_drag = false;

    while let Some(terminal_event) = events.front() {
        if scrollbar_geometry_changed_by_drag
            && matches!(
                terminal_event,
                TerminalEvent::Pointer(PointerEvent {
                    kind: PointerKind::Down(_) | PointerKind::ScrollUp | PointerKind::ScrollDown,
                    ..
                })
            )
        {
            break;
        }

        let terminal_event = events
            .pop_front()
            .expect("front terminal event must still exist");
        if matches!(terminal_event, TerminalEvent::Resize(_)) {
            detail_scrollbar_drag.cancel();
            changed = true;

            // 連続resizeは1回の再描画へまとめる。後続mouseは新しいRectが
            // 描画されてから処理し、古いRenderInfoでhit testしない。
            while matches!(events.front(), Some(TerminalEvent::Resize(_))) {
                events.pop_front();
            }
            break;
        }

        let detail_revision_before = app.detail_revision();
        let samples_pane_before = app.samples_pane_enabled();
        let detail_scroll_before = app.detail_scroll();
        let is_left_drag = matches!(
            terminal_event,
            TerminalEvent::Pointer(PointerEvent {
                kind: PointerKind::Drag(PointerButton::Left),
                ..
            })
        );
        changed |= handle_terminal_event_with_mouse_mode(
            app,
            detail_layout,
            detail_scrollbar_drag,
            terminal_event,
            render_info,
            run_tx,
            mouse_mode,
        )?;
        if app.detail_revision() != detail_revision_before
            || app.samples_pane_enabled() != samples_pane_before
        {
            // The remaining queued pointer events must see geometry rendered
            // for the new document/mode/pane layout. Pure drag bursts do not
            // change either stable identity input and continue to batch.
            break;
        }
        if app.detail_scroll() != detail_scroll_before {
            if is_left_drag {
                // Absolute drag mapping depends only on stable track geometry
                // and remains valid throughout one delivered drag burst.
                scrollbar_geometry_changed_by_drag = true;
            } else {
                // A wheel/seek/cap action changes the rendered thumb. Leave
                // later pointer events queued until that geometry is redrawn.
                break;
            }
        }
    }

    Ok(changed)
}

fn handle_terminal_event_with_mouse_mode(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    terminal_event: TerminalEvent,
    render_info: &view::RenderInfo,
    run_tx: &Sender<RunRequest>,
    mouse_mode: MouseMode,
) -> io::Result<bool> {
    match terminal_event {
        TerminalEvent::Key(key) => handle_key_event(app, key, run_tx),

        TerminalEvent::Pointer(pointer) => Ok(handle_pointer_event_with_mouse_mode(
            app,
            detail_layout,
            detail_scrollbar_drag,
            pointer,
            render_info,
            mouse_mode,
        )),

        TerminalEvent::Resize(_) => {
            detail_scrollbar_drag.cancel();
            Ok(true)
        }

        TerminalEvent::Ignored => Ok(false),
    }
}

fn handle_key_event(
    app: &mut WatchApp,
    key: KeyEvent,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') if key.kind == KeyEventKind::Press => {
            app.quit();
            Ok(true)
        }

        KeyCode::Char('d') if key.kind == KeyEventKind::Press => {
            app.toggle_debug();

            if app.current_source_language() == Some(crate::language::Language::Cpp)
                && let Some(problem) = app.selected_problem()
            {
                queue_problem_run(app, problem, run_tx)?;
            }

            Ok(true)
        }

        KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
            let Some(problem) = app.selected_problem() else {
                return Ok(false);
            };

            queue_problem_run(app, problem, run_tx)
        }

        KeyCode::Char('S') if key.kind == KeyEventKind::Press => {
            let Some(problem) = app.selected_problem() else {
                return Ok(false);
            };

            queue_problem_stress(app, problem, run_tx)
        }

        KeyCode::Char('s') if key.kind == KeyEventKind::Press => {
            app.toggle_samples_pane();
            Ok(true)
        }

        KeyCode::Char('h') | KeyCode::Left => Ok(app.previous_problem()),

        KeyCode::Char('l') | KeyCode::Right => Ok(app.next_problem()),

        KeyCode::Char('j') | KeyCode::Down => Ok(app.next_case()),

        KeyCode::Char('k') | KeyCode::Up => Ok(app.previous_case()),

        _ => Ok(false),
    }
}

fn contains(area: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[cfg(test)]
fn handle_pointer_event(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    pointer: PointerEvent,
    render_info: &view::RenderInfo,
) -> bool {
    handle_pointer_event_with_mouse_mode(
        app,
        detail_layout,
        detail_scrollbar_drag,
        pointer,
        render_info,
        MouseMode::Cells,
    )
}

fn projected_pixel_pointer(
    pointer: PointerEvent,
    mouse_mode: MouseMode,
) -> Option<(u32, u32, TerminalPixelMetrics, u64)> {
    let terminal::PointerPosition::AbsolutePixels { x, y } = pointer.position else {
        return None;
    };
    let MouseMode::Pixels {
        metrics,
        origin,
        generation,
    } = mouse_mode
    else {
        return None;
    };
    if pointer.pixel_generation != Some(generation) {
        return None;
    }
    let (x, y) = normalize_absolute_pixels(metrics, origin, x, y)?;
    Some((x, y, metrics, generation))
}

fn project_pointer_to_cells(pointer: PointerEvent, mouse_mode: MouseMode) -> Option<(u16, u16)> {
    match (pointer.position, mouse_mode) {
        (terminal::PointerPosition::Cells { column, row }, MouseMode::Cells) => Some((column, row)),
        (
            terminal::PointerPosition::AbsolutePixels { x, y },
            MouseMode::Pixels {
                metrics,
                origin,
                generation,
            },
        ) if pointer.pixel_generation == Some(generation) => {
            project_absolute_pixels_to_cells(metrics, origin, x, y)
        }
        _ => None,
    }
}

fn handle_pointer_event_with_mouse_mode(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    pointer: PointerEvent,
    render_info: &view::RenderInfo,
    mouse_mode: MouseMode,
) -> bool {
    if matches!(pointer.kind, PointerKind::Up(_)) {
        detail_scrollbar_drag.cancel();
        return false;
    }

    let Some((column, row)) = project_pointer_to_cells(pointer, mouse_mode) else {
        return false;
    };

    if matches!(pointer.kind, PointerKind::Down(_)) {
        // Every new press terminates a previous interaction before the new hit
        // target is interpreted.
        detail_scrollbar_drag.cancel();
    }

    if let PointerKind::Drag(PointerButton::Left) = pointer.kind {
        let Some(drag) = detail_scrollbar_drag.active else {
            return false;
        };
        let Some(scrollbar) = render_info.detail_scrollbar.as_ref() else {
            detail_scrollbar_drag.cancel();
            return false;
        };
        if scrollbar.identity != drag.identity
            || scrollbar.identity.layout.revision != app.detail_revision()
        {
            detail_scrollbar_drag.cancel();
            return false;
        }

        let target = match drag.coordinate {
            DragCoordinate::Cells { grab_offset }
                if matches!(pointer.position, terminal::PointerPosition::Cells { .. }) =>
            {
                scrollbar.geometry.scroll_for_drag(row, grab_offset)
            }
            DragCoordinate::Pixels {
                grab_offset_px,
                generation,
            } => {
                let Some((_, normalized_y, metrics, event_generation)) =
                    projected_pixel_pointer(pointer, mouse_mode)
                else {
                    return false;
                };
                if generation != event_generation {
                    return false;
                }
                scrollbar.geometry.scroll_for_pixel_drag(
                    normalized_y,
                    grab_offset_px,
                    metrics.cell_height_px,
                )
            }
            _ => return false,
        };
        return set_detail_scroll_from_user(
            app,
            detail_layout,
            target,
            Some(scrollbar.geometry.max_scroll),
        );
    }

    if let Some(samples_area) = render_info.samples_area
        && contains(samples_area, column, row)
    {
        return match pointer.kind {
            PointerKind::ScrollUp => app.previous_case(),

            PointerKind::ScrollDown => app.next_case(),

            _ => false,
        };
    }

    if let PointerKind::Down(PointerButton::Left) = pointer.kind
        && let Some(scrollbar) = render_info.detail_scrollbar.as_ref()
        && scrollbar.identity.layout.revision == app.detail_revision()
        && let Some(hit) = scrollbar.geometry.hit_test(column, row)
    {
        return match hit {
            DetailScrollbarHit::Thumb { grab_offset } => {
                let coordinate = match projected_pixel_pointer(pointer, mouse_mode) {
                    Some((_, normalized_y, metrics, generation)) => {
                        let Some(grab_offset_px) = scrollbar
                            .geometry
                            .pixel_grab_offset(normalized_y, metrics.cell_height_px)
                        else {
                            return false;
                        };
                        DragCoordinate::Pixels {
                            grab_offset_px,
                            generation,
                        }
                    }
                    None => DragCoordinate::Cells { grab_offset },
                };
                detail_scrollbar_drag.active = Some(DetailScrollbarDrag {
                    identity: scrollbar.identity,
                    coordinate,
                });
                false
            }
            DetailScrollbarHit::TopCap => set_detail_scroll_from_user(
                app,
                detail_layout,
                0,
                Some(scrollbar.geometry.max_scroll),
            ),
            DetailScrollbarHit::BottomCap => set_detail_scroll_from_user(
                app,
                detail_layout,
                scrollbar.geometry.max_scroll,
                Some(scrollbar.geometry.max_scroll),
            ),
            DetailScrollbarHit::Track => set_detail_scroll_from_user(
                app,
                detail_layout,
                scrollbar.geometry.scroll_for_track_click(row),
                Some(scrollbar.geometry.max_scroll),
            ),
        };
    }

    if contains(render_info.detail_area, column, row) {
        return match pointer.kind {
            PointerKind::ScrollUp => {
                let target = app.detail_scroll().saturating_sub(DETAIL_SCROLL_LINES);
                set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    target,
                    render_info.max_detail_scroll,
                )
            }

            PointerKind::ScrollDown => {
                let target = app.detail_scroll().saturating_add(DETAIL_SCROLL_LINES);
                set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    target,
                    render_info.max_detail_scroll,
                )
            }

            _ => false,
        };
    }

    false
}

fn set_detail_scroll_from_user(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    target: usize,
    max_scroll: Option<usize>,
) -> bool {
    detail_layout.cancel_pending_scroll_reconciliation_for_user_input();
    app.set_detail_scroll_from_user(max_scroll.map_or(target, |max| target.min(max)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::model::{Contest, Problem};
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use terminal::{PointerButton as MouseButton, PointerKind as MouseEventKind};

    fn app() -> WatchApp {
        app_with_problems(&[3])
    }

    fn app_with_problems(sample_counts: &[usize]) -> WatchApp {
        WatchApp::new(
            &Contest {
                contest_id: "abc123".to_string(),
                problems: sample_counts
                    .iter()
                    .enumerate()
                    .map(|(index, _)| Problem {
                        index: char::from(b'A' + index as u8).to_string(),
                        title: format!("Problem {index}"),
                        task_id: format!("abc123_{index}"),
                        url: format!("https://example.invalid/{index}"),
                    })
                    .collect(),
            },
            sample_counts.to_vec(),
        )
        .unwrap()
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            kind,
            modifiers: terminal::Modifiers::default(),
        }
    }

    fn resize(columns: u16, rows: u16) -> TerminalEvent {
        TerminalEvent::Resize(terminal::TerminalSize { columns, rows })
    }

    fn handle_key(app: &mut WatchApp, code: KeyCode, kind: KeyEventKind) -> bool {
        let (run_tx, _run_rx) = mpsc::channel();

        handle_key_event(app, key(code, kind), &run_tx).unwrap()
    }

    fn handle_terminal_events(
        app: &mut WatchApp,
        render_info: &view::RenderInfo,
        events: &mut VecDeque<TerminalEvent>,
        run_tx: &Sender<RunRequest>,
    ) -> io::Result<bool> {
        let mut detail_layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        super::handle_terminal_events(
            app,
            &mut detail_layout,
            &mut drag,
            render_info,
            events,
            run_tx,
        )
    }

    #[test]
    fn press_and_repeat_are_handled_but_release_is_ignored() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Char('j'), KeyEventKind::Press);
        assert_eq!(app.selected_case(), 1);

        handle_key(&mut app, KeyCode::Down, KeyEventKind::Repeat);
        assert_eq!(app.selected_case(), 2);

        handle_key(&mut app, KeyCode::Char('q'), KeyEventKind::Release);
        assert!(!app.should_quit());
        handle_key(&mut app, KeyCode::Char('q'), KeyEventKind::Press);
        assert!(app.should_quit());
    }

    #[test]
    fn queued_quit_is_processed_before_later_events_without_intermediate_draws() {
        let mut app = app();
        let events = RefCell::new(VecDeque::from([
            TerminalEvent::Ignored,
            resize(80, 24),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
            TerminalEvent::Pointer(pointer(PointerKind::ScrollDown, 5, 5)),
        ]));
        let poll_waits = RefCell::new(Vec::new());

        let queued = read_terminal_events_with(
            TERMINAL_POLL_INTERVAL,
            |wait| {
                poll_waits.borrow_mut().push(wait);
                Ok(!events.borrow().is_empty())
            },
            || {
                events
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| io::Error::other("test event queue is empty"))
            },
        )
        .unwrap();

        assert!(contains_quit_event(&queued));
        app.quit();
        assert!(app.should_quit());
        assert_eq!(app.selected_case(), 0);
        assert_eq!(events.borrow().len(), 1);
        assert_eq!(
            poll_waits.into_inner(),
            [TERMINAL_POLL_INTERVAL, Duration::ZERO, Duration::ZERO,]
        );
    }

    #[test]
    fn ignored_events_preserve_the_raw_event_batch_cap_before_quit() {
        let quit = TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press));
        let mut source = vec![TerminalEvent::Ignored; MAX_TERMINAL_EVENTS_PER_TICK];
        source.push(quit);
        let events = RefCell::new(VecDeque::from(source));

        let queued = read_terminal_events_with(
            Duration::ZERO,
            |_| Ok(!events.borrow().is_empty()),
            || {
                events
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| io::Error::other("test event queue is empty"))
            },
        )
        .unwrap();

        assert_eq!(queued.len(), MAX_TERMINAL_EVENTS_PER_TICK);
        assert!(queued.iter().all(|event| *event == TerminalEvent::Ignored));
        assert!(!contains_quit_event(&queued));
        assert_eq!(events.borrow().len(), 1);
        assert_eq!(events.borrow().front(), Some(&quit));
    }

    #[test]
    fn leading_resizes_only_coalesce_across_contiguous_raw_events() {
        let mut events = VecDeque::from([
            resize(80, 24),
            resize(120, 40),
            TerminalEvent::Ignored,
            resize(160, 50),
            TerminalEvent::Key(key(KeyCode::Char('j'), KeyEventKind::Press)),
        ]);

        assert!(take_leading_resizes(&mut events));
        assert_eq!(
            events,
            VecDeque::from([
                TerminalEvent::Ignored,
                resize(160, 50),
                TerminalEvent::Key(key(KeyCode::Char('j'), KeyEventKind::Press)),
            ])
        );
    }

    #[test]
    fn quit_priority_requires_lowercase_press_but_ignores_modifiers() {
        let modified_q = TerminalEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            kind: KeyEventKind::Press,
            modifiers: terminal::Modifiers {
                control: true,
                alt: true,
                ..terminal::Modifiers::default()
            },
        });

        assert!(is_quit_event(&modified_q));
        assert!(!is_quit_event(&TerminalEvent::Key(key(
            KeyCode::Char('Q'),
            KeyEventKind::Press,
        ))));
        assert!(!is_quit_event(&TerminalEvent::Key(key(
            KeyCode::Char('q'),
            KeyEventKind::Repeat,
        ))));
        assert!(!is_quit_event(&TerminalEvent::Key(key(
            KeyCode::Char('q'),
            KeyEventKind::Release,
        ))));
    }

    #[test]
    fn mouse_after_resize_waits_for_new_render_info() {
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();

        let old_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
        };

        let mut events = VecDeque::from([
            resize(100, 40),
            TerminalEvent::Pointer(pointer(PointerKind::ScrollDown, 5, 5)),
        ]);

        assert!(handle_terminal_events(&mut app, &old_info, &mut events, &run_tx,).unwrap());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(events.len(), 1);

        let new_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 100, 40),
            detail_scrollbar: None,
        };

        assert!(handle_terminal_events(&mut app, &new_info, &mut events, &run_tx,).unwrap());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 3);
    }

    #[test]
    fn repeat_does_not_toggle_debug_repeatedly() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Char('d'), KeyEventKind::Press);
        assert!(app.debug_enabled());
        handle_key(&mut app, KeyCode::Char('d'), KeyEventKind::Repeat);
        assert!(app.debug_enabled());
    }

    #[test]
    fn up_and_k_move_to_the_previous_case() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Up, KeyEventKind::Press);
        assert_eq!(app.selected_case(), 2);
        handle_key(&mut app, KeyCode::Char('k'), KeyEventKind::Press);
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn unknown_and_no_op_navigation_keys_are_not_dirty() {
        let mut app = app_with_problems(&[1]);

        assert!(!handle_key(
            &mut app,
            KeyCode::Char('x'),
            KeyEventKind::Press
        ));
        assert!(!handle_key(&mut app, KeyCode::Right, KeyEventKind::Press));
        assert!(!handle_key(&mut app, KeyCode::Down, KeyEventKind::Press));
    }

    #[test]
    fn source_messages_update_state_and_multiple_messages_use_the_latest_source() {
        let mut app = app_with_problems(&[3, 2]);
        app.previous_case();
        let (tx, rx) = mpsc::channel();
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.py"),
            language: Language::Python,
        })
        .unwrap();

        let (run_tx, _run_rx) = mpsc::channel();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        let problem = app.current_problem().unwrap();
        assert_eq!(problem.index, "B");
        assert_eq!(app.selected_case(), 0);
        let source = problem.source.as_ref().unwrap();
        assert_eq!(source.path, Path::new("B.py"));
        assert_eq!(source.language, Language::Python);
    }

    #[test]
    fn message_drain_is_bounded_to_keep_input_responsive() {
        let mut app = app();

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        // 最初の1tickで処理できる上限いっぱいまでC++の変更を積む
        for _ in 0..MAX_MESSAGES_PER_TICK {
            tx.send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        }

        // 257件目。これは最初のhandle_messagesでは処理されないはず
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.py"),
            language: Language::Python,
        })
        .unwrap();

        // 1tick目
        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(
            app.current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .language,
            Language::Cpp
        );

        // 256件だけRunRequestが作られている
        let first_requests: Vec<_> = run_rx.try_iter().collect();

        assert_eq!(first_requests.len(), MAX_MESSAGES_PER_TICK);
        assert_eq!(first_requests[0].run_id, 1);
        assert_eq!(
            first_requests.last().unwrap().run_id,
            MAX_MESSAGES_PER_TICK as u64
        );

        // 2tick目で残っていたA.pyを処理
        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(
            app.current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .language,
            Language::Python
        );

        let second_requests: Vec<_> = run_rx.try_iter().collect();

        assert_eq!(second_requests.len(), 1);
        assert_eq!(second_requests[0].run_id, MAX_MESSAGES_PER_TICK as u64 + 1);
        assert_eq!(second_requests[0].problem, 0);
        assert_eq!(second_requests[0].language, Language::Python);
    }

    #[test]
    fn watcher_failure_and_disconnected_channel_are_errors() {
        let mut app = app();

        let (tx, rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();

        tx.send(Message::WatcherFailed(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "watch failed",
        )))
        .unwrap();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "watch failed");

        let (tx, rx) = mpsc::channel();
        tx.send(Message::WorkerFailed(io::Error::other("worker panicked")))
            .unwrap();
        let (run_tx, _run_rx) = mpsc::channel();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "worker panicked");

        // background側の全Senderが消えた場合
        let (tx, rx) = mpsc::channel::<Message>();
        drop(tx);

        let (run_tx, _run_rx) = mpsc::channel();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "background message channel disconnected");
    }

    #[test]
    fn detail_analysis_results_update_layout_and_disconnection_is_an_error() {
        let raw = "line\n".repeat(3_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();
        let initial = layout.viewport(&document, 5, 80, 20, 0);
        assert_eq!(initial.max_scroll, None);
        layout.complete_structure_for_test(&document);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::Count(request)) =
            layout.take_analysis_command()
        else {
            panic!("large detail must request background counting");
        };
        let mut never_cancel = || false;
        let count = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                request.anchor,
                &mut never_cancel,
            )
            .unwrap();
        let result = detail_layout::DetailAnalysisResult::Count(detail_layout::DetailCountResult {
            identity: request.identity,
            exact_layout_index: count.exact_layout_index,
            anchor: request.anchor,
            anchor_visual_row: count.anchor_visual_row,
            anchor_row_raw_start: count.anchor_row_raw_start,
        });
        let (tx, rx) = mpsc::channel();
        for _ in 0..=MAX_DETAIL_ANALYSIS_RESULTS_PER_TICK {
            tx.send(result.clone()).unwrap();
        }

        assert!(handle_detail_analysis_results(&mut layout, 5, &rx).unwrap());
        assert!(rx.try_recv().is_ok(), "result draining must stay bounded");
        assert_eq!(
            layout.viewport(&document, 5, 80, 20, 0).max_scroll,
            Some(2_981)
        );

        drop(tx);
        let error = handle_detail_analysis_results(&mut layout, 5, &rx).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            error.to_string(),
            "detail analysis worker result channel disconnected"
        );
    }

    #[test]
    fn detail_scroll_reconciliation_crosses_layout_app_boundary_once() {
        let raw = "a".repeat(100);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();

        layout.viewport(&document, 1, 6, 2, 5);
        let anchored = layout.viewport(&document, 1, 11, 2, 5);
        let mut app = app_with_problems(&[1]);
        app.scroll_detail_down(5);

        assert!(apply_detail_scroll_reconciliation(&mut app, &mut layout));
        assert_eq!(app.detail_scroll(), anchored.effective_scroll);
        assert!(!apply_detail_scroll_reconciliation(&mut app, &mut layout));
    }

    #[test]
    fn mouse_scroll_cancels_pending_width_reconciliation_without_snap_back() {
        let raw = "long normal detail line\n".repeat(4_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();

        layout.viewport(&document, 2, 100, 20, 0);
        layout.complete_structure_for_test(&document);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::Count(initial_request)) =
            layout.take_analysis_command()
        else {
            panic!("completed lazy detail must stage its initial count");
        };
        let mut never_cancel = || false;
        let initial = initial_request
            .structure
            .count_chunks(
                &initial_request.snapshot,
                initial_request.identity.layout_width,
                initial_request.anchor,
                &mut never_cancel,
            )
            .unwrap();
        assert!(layout.apply_count_result(detail_layout::DetailCountResult {
            identity: initial_request.identity,
            exact_layout_index: initial.exact_layout_index,
            anchor: initial_request.anchor,
            anchor_visual_row: initial.anchor_visual_row,
            anchor_row_raw_start: initial.anchor_row_raw_start,
        }));

        let baseline_scroll = 500;
        layout.viewport(&document, 2, 100, 20, baseline_scroll);
        layout.viewport(&document, 2, 70, 20, baseline_scroll);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::Count(anchored_request)) =
            layout.take_analysis_command()
        else {
            panic!("width transition must stage an anchored count");
        };
        assert!(anchored_request.anchor.is_some());
        let delayed = anchored_request
            .structure
            .count_chunks(
                &anchored_request.snapshot,
                anchored_request.identity.layout_width,
                anchored_request.anchor,
                &mut never_cancel,
            )
            .unwrap();

        let mut app = app_with_problems(&[1]);
        app.scroll_detail_down(baseline_scroll);
        let info = view::RenderInfo {
            max_detail_scroll: None,
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 70, 20),
            detail_scrollbar: None,
        };
        assert!(super::handle_pointer_event(
            &mut app,
            &mut layout,
            &mut DetailScrollbarDragState::default(),
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));
        let user_scroll = baseline_scroll + DETAIL_SCROLL_LINES;
        assert_eq!(app.detail_scroll(), user_scroll);

        layout.viewport(&document, 2, 70, 20, app.detail_scroll());
        assert!(layout.apply_count_result(detail_layout::DetailCountResult {
            identity: anchored_request.identity,
            exact_layout_index: delayed.exact_layout_index,
            anchor: anchored_request.anchor,
            anchor_visual_row: delayed.anchor_visual_row,
            anchor_row_raw_start: delayed.anchor_row_raw_start,
        }));
        assert!(!apply_detail_scroll_reconciliation(&mut app, &mut layout));
        assert_eq!(app.detail_scroll(), user_scroll);
    }

    #[test]
    fn stale_structure_result_is_rejected_before_layout_prepares_the_new_document() {
        let raw = "line\n".repeat(100_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();
        layout.viewport(&document, 5, 80, 20, 0);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::BuildStructure(request)) =
            layout.take_analysis_command()
        else {
            panic!("large detail must request background structure discovery");
        };
        let structure =
            detail_layout::build_document_structure_cancellable(&request.snapshot, || false)
                .unwrap();
        let result = detail_layout::DetailAnalysisResult::StructureReady(
            detail_layout::DetailStructureResult {
                identity: request.identity,
                structure,
            },
        );
        let (tx, rx) = mpsc::channel();
        tx.send(result).unwrap();

        assert!(!handle_detail_analysis_results(&mut layout, 6, &rx).unwrap());
        assert_eq!(layout.viewport(&document, 5, 80, 20, 0).max_scroll, None);
    }

    #[test]
    fn key_and_source_message_processing_coexist() {
        let mut app = app_with_problems(&[3, 2]);

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Press), &run_tx,).unwrap()
        );
        assert_eq!(app.selected_case(), 1);

        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.py"),
            language: Language::Python,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(app.current_problem().unwrap().index, "B");
        assert_eq!(app.selected_case(), 0);

        // source変更からworkerへのRunRequestも作られている
        let request = run_rx.try_recv().unwrap();

        assert_eq!(request.run_id, 1);
        assert_eq!(request.problem, 1);
        assert_eq!(request.language, Language::Python);
        assert!(!request.debug);

        // Message処理後もkeyboard操作できる
        assert!(
            handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Press), &run_tx,).unwrap()
        );
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn stale_run_messages_in_the_same_drain_cannot_overwrite_a_newer_request() {
        let mut app = app();
        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::RunStarted {
            run_id: 1,
            problem: 0,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.py"),
            language: Language::Python,
        })
        .unwrap();
        tx.send(Message::RunEvent {
            run_id: 1,
            problem: 0,
            event: message::TestEvent::TestRunFinished {
                accepted: 3,
                total_cases: 3,
            },
        })
        .unwrap();
        tx.send(Message::RunCompleted {
            run_id: 1,
            problem: 0,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        let requests: Vec<_> = run_rx.try_iter().collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].run_id, 1);
        assert_eq!(requests[1].run_id, 2);
        assert_eq!(requests[1].language, Language::Python);

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(2));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Python));
        assert_eq!(run.accepted, 0);
    }

    #[test]
    fn run_requeued_message_updates_state_and_only_marks_real_changes_dirty() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));

        let (tx, rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();
        tx.send(Message::RunRequeued {
            run_id: request.run_id,
            problem: 0,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );

        let current = app.queue_run(0).unwrap();
        tx.send(Message::RunRequeued {
            run_id: request.run_id,
            problem: 0,
        })
        .unwrap();

        assert!(!handle_messages(&mut app, &rx, &run_tx).unwrap());
        assert_eq!(app.current_problem().unwrap().run.id, Some(current.run_id));
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );
    }
    #[test]
    fn samples_pane_toggles_only_on_key_press() {
        let mut app = app();

        assert!(!app.samples_pane_enabled());

        assert!(handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Press
        ),);
        assert!(app.samples_pane_enabled());

        assert!(!handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Repeat
        ),);
        assert!(app.samples_pane_enabled());

        assert!(handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Press
        ),);
        assert!(!app.samples_pane_enabled());
    }
    fn pointer(kind: PointerKind, column: u16, row: u16) -> PointerEvent {
        PointerEvent {
            kind,
            position: terminal::PointerPosition::Cells { column, row },
            modifiers: terminal::Modifiers::default(),
            pixel_generation: None,
        }
    }

    fn pixel_metrics() -> TerminalPixelMetrics {
        TerminalPixelMetrics::validated(100, 40, 1_000, 800, 10, 20).unwrap()
    }

    fn pixel_mode(generation: u64) -> MouseMode {
        MouseMode::Pixels {
            metrics: pixel_metrics(),
            origin: mouse::PixelCoordinateOrigin::ZeroBased,
            generation,
        }
    }

    fn pixel_pointer(kind: PointerKind, x: u32, y: u32, generation: Option<u64>) -> PointerEvent {
        PointerEvent {
            kind,
            position: terminal::PointerPosition::AbsolutePixels { x, y },
            modifiers: terminal::Modifiers::default(),
            pixel_generation: generation,
        }
    }
    fn handle_pointer_event(
        app: &mut WatchApp,
        pointer: PointerEvent,
        render_info: &view::RenderInfo,
    ) -> bool {
        let mut detail_layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        super::handle_pointer_event(app, &mut detail_layout, &mut drag, pointer, render_info)
    }

    fn scrollbar_info(
        app: &WatchApp,
        max_scroll: usize,
        scroll: usize,
        layout_generation: u64,
    ) -> view::RenderInfo {
        let detail_area = ratatui::layout::Rect::new(20, 5, 40, 20);
        let geometry = detail_scrollbar::DetailScrollbarGeometry::new(
            detail_area,
            max_scroll,
            scroll,
            usize::from(detail_area.height),
            &[],
        )
        .unwrap();
        let interaction = detail_scrollbar::DetailScrollbarInteraction::new(
            detail_layout::DetailExactLayoutIdentity {
                document_generation: 1,
                layout_generation,
                revision: app.detail_revision(),
            },
            geometry,
        )
        .unwrap();
        view::RenderInfo {
            max_detail_scroll: Some(max_scroll),
            samples_area: None,
            detail_area,
            detail_scrollbar: Some(interaction),
        }
    }

    fn dispatch_mouse(
        app: &mut WatchApp,
        layout: &mut detail_layout::DetailLayout,
        drag: &mut DetailScrollbarDragState,
        info: &view::RenderInfo,
        kind: PointerKind,
        column: u16,
        row: u16,
    ) -> bool {
        super::handle_pointer_event(app, layout, drag, pointer(kind, column, row), info)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test call sites name every raw pixel input needed by each scenario"
    )]
    fn dispatch_pixel(
        app: &mut WatchApp,
        layout: &mut detail_layout::DetailLayout,
        drag: &mut DetailScrollbarDragState,
        info: &view::RenderInfo,
        mode: MouseMode,
        kind: PointerKind,
        x: u32,
        y: u32,
        generation: Option<u64>,
    ) -> bool {
        super::handle_pointer_event_with_mouse_mode(
            app,
            layout,
            drag,
            pixel_pointer(kind, x, y, generation),
            info,
            mode,
        )
    }

    fn pending_width_layout(
        document: &detail::DetailDocument<'_>,
    ) -> (
        detail_layout::DetailLayout,
        detail_layout::DetailCountResult,
    ) {
        let mut layout = detail_layout::DetailLayout::default();
        layout.viewport(document, 2, 100, 20, 0);
        layout.complete_structure_for_test(document);
        let initial = count_request_from_layout(&mut layout, document);
        assert!(layout.apply_count_result(real_count_result(initial)));
        layout.viewport(document, 2, 100, 20, 500);
        layout.viewport(document, 2, 70, 20, 500);
        let delayed = real_count_result(count_request_from_layout(&mut layout, document));
        assert!(layout.has_pending_width_anchor_for_test());
        (layout, delayed)
    }

    fn count_request_from_layout(
        layout: &mut detail_layout::DetailLayout,
        document: &detail::DetailDocument<'_>,
    ) -> detail_layout::DetailCountRequest {
        layout.stage_analysis_command(document);
        let Some(detail_layout::DetailAnalysisCommand::Count(request)) =
            layout.take_analysis_command()
        else {
            panic!("expected exact Detail count request");
        };
        request
    }

    fn real_count_result(
        request: detail_layout::DetailCountRequest,
    ) -> detail_layout::DetailCountResult {
        let anchor = request.anchor;
        let count = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                anchor,
                || false,
            )
            .unwrap();
        detail_layout::DetailCountResult {
            identity: request.identity,
            exact_layout_index: count.exact_layout_index,
            anchor,
            anchor_visual_row: count.anchor_visual_row,
            anchor_row_raw_start: count.anchor_row_raw_start,
        }
    }
    #[test]
    fn mouse_wheel_over_samples_changes_sample() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
        };

        assert!(handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 5, 5),
            &info,
        ));

        assert_eq!(app.selected_case(), 1);
        assert_eq!(app.detail_scroll(), 0);
    }
    #[test]
    fn mouse_wheel_over_detail_scrolls_detail() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
        };

        assert!(handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 3);
    }

    #[test]
    fn mouse_wheel_does_not_clamp_while_lazy_max_is_unknown() {
        let mut app = app();
        let info = view::RenderInfo {
            max_detail_scroll: None,
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
            detail_scrollbar: None,
        };

        assert!(handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);
    }
    #[test]
    fn mouse_wheel_at_detail_bottom_is_not_dirty() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: Some(10),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
            detail_scrollbar: None,
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));

        assert_eq!(app.detail_scroll(), 10);
    }

    #[test]
    fn mouse_wheel_at_detail_top_is_not_dirty() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(10),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
            detail_scrollbar: None,
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollUp, 30, 5),
            &info,
        ));

        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn samples_and_detail_rect_boundary_is_half_open() {
        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
        };

        let mut samples_app = app();
        assert!(handle_pointer_event(
            &mut samples_app,
            pointer(PointerKind::ScrollDown, 19, 5),
            &info,
        ));
        assert_eq!(samples_app.selected_case(), 1);
        assert_eq!(samples_app.detail_scroll(), 0);

        let mut detail_app = app();
        assert!(handle_pointer_event(
            &mut detail_app,
            pointer(PointerKind::ScrollDown, 20, 5),
            &info,
        ));
        assert_eq!(detail_app.selected_case(), 0);
        assert_eq!(detail_app.detail_scroll(), 3);
    }

    #[test]
    fn samples_wheel_with_one_case_does_not_mark_ui_dirty() {
        let mut app = app_with_problems(&[1]);
        let info = view::RenderInfo {
            max_detail_scroll: Some(0),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 5, 5),
            &info,
        ));
        assert_eq!(app.selected_case(), 0);
    }
    #[test]
    fn mouse_wheel_outside_content_is_ignored() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 5, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 5, 40, 10),
            detail_scrollbar: None,
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 5, 1),
            &info,
        ));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn absolute_pixel_pointer_positions_are_not_projected_as_cells() {
        let mut app = app();
        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
        };
        let pointer = PointerEvent {
            kind: PointerKind::ScrollDown,
            position: terminal::PointerPosition::AbsolutePixels { x: 30, y: 5 },
            modifiers: terminal::Modifiers::default(),
            pixel_generation: None,
        };

        assert!(!handle_pointer_event(&mut app, pointer, &info));
        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn thumb_down_starts_drag_and_track_down_seeks_without_dragging() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        ));
        assert!(drag.active.is_some());

        let track_row = geometry.track_end_row().saturating_sub(1);
        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            track_row,
        ));
        assert!(app.detail_scroll() > 0);
        assert!(drag.active.is_none());
    }

    #[test]
    fn cap_clicks_seek_exact_endpoints() {
        let mut app = app();
        app.set_detail_scroll_from_user(50);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 50, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;

        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.top_cap_row.unwrap(),
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.bottom_cap_row.unwrap(),
        ));
        assert_eq!(app.detail_scroll(), 100);
    }

    #[test]
    fn drag_preserves_grab_offset_and_continues_outside_the_gutter_and_pane() {
        let mut app = app();
        app.set_detail_scroll_from_user(5);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 10, 5, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        assert!(geometry.thumb_len > 1);
        let grab_offset = geometry.thumb_len - 1;
        let pointer_row = geometry.thumb_start_row + grab_offset;

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            pointer_row,
        ));
        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            pointer_row,
        ));
        assert_eq!(app.detail_scroll(), 5);

        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            u16::MAX,
        ));
        assert_eq!(app.detail_scroll(), 10);
        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            u16::MAX,
            0,
        ));
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn pixel_thumb_drag_preserves_sub_cell_grab_offset_and_adjacent_pixels() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 1_000_000, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let mode = pixel_mode(7);
        let x = u32::from(geometry.gutter.x) * 10 + 5;
        let thumb_top = u32::from(geometry.thumb_start_row) * 20;
        let grab_offset_px = 13;

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            x,
            thumb_top + grab_offset_px,
            Some(7),
        ));
        assert!(matches!(
            drag.active,
            Some(DetailScrollbarDrag {
                coordinate: DragCoordinate::Pixels {
                    grab_offset_px: 13,
                    generation: 7,
                },
                ..
            })
        ));

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            0,
            thumb_top + grab_offset_px,
            Some(7),
        ));
        assert_eq!(app.detail_scroll(), 0);

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            0,
            thumb_top + grab_offset_px + 1,
            Some(7),
        ));
        let first = app.detail_scroll();
        assert!(first > 0);
        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            0,
            thumb_top + grab_offset_px + 2,
            Some(7),
        ));
        assert!(app.detail_scroll() > first);

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Up(PointerButton::Left),
            u32::MAX,
            u32::MAX,
            None,
        ));
        assert!(drag.active.is_none());
    }

    #[test]
    fn pixel_drag_clamps_large_ranges_and_rejects_invalid_or_stale_mapping() {
        let mut app = app();
        app.set_detail_scroll_from_user(usize::MAX / 2);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, usize::MAX, usize::MAX / 2, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let mode = pixel_mode(11);
        let x = u32::from(geometry.gutter.x) * 10 + 5;
        let thumb_y = u32::from(geometry.thumb_start_row) * 20 + 7;

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            x,
            thumb_y,
            Some(11),
        );
        assert!(drag.active.is_some());

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            x,
            0,
            Some(11),
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            x,
            799,
            Some(11),
        ));
        assert_eq!(app.detail_scroll(), usize::MAX);

        let unchanged = app.detail_scroll();
        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseMode::Disabled,
            PointerKind::Drag(PointerButton::Left),
            x,
            200,
            None,
        ));
        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(12),
            PointerKind::Drag(PointerButton::Left),
            x,
            200,
            Some(11),
        ));
        assert_eq!(app.detail_scroll(), unchanged);
    }

    #[test]
    fn resize_cancels_pixel_drag_and_a_stale_report_cannot_restart_it() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 10_000, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let x = u32::from(geometry.gutter.x) * 10 + 5;
        let y = u32::from(geometry.thumb_start_row) * 20 + 5;

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(21),
            PointerKind::Down(PointerButton::Left),
            x,
            y,
            Some(21),
        );
        assert!(drag.active.is_some());

        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([resize(100, 40)]);
        assert!(
            super::handle_terminal_events_with_mouse_mode(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
                MouseMode::Disabled,
            )
            .unwrap()
        );
        assert!(drag.active.is_none());

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(22),
            PointerKind::Drag(PointerButton::Left),
            x,
            y + 100,
            Some(21),
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(drag.active.is_none());
    }

    #[test]
    fn pixel_wheel_caps_and_track_click_match_cell_semantics() {
        let mode = pixel_mode(3);
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 1_000, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let gutter_x = u32::from(geometry.gutter.x) * 10 + 5;

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::ScrollDown,
            u32::from(info.detail_area.x) * 10 + 5,
            u32::from(info.detail_area.y) * 20 + 5,
            Some(3),
        ));
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            u32::from(geometry.bottom_cap_row.unwrap()) * 20 + 10,
            Some(3),
        ));
        assert_eq!(app.detail_scroll(), 1_000);

        app.set_detail_scroll_from_user(0);
        let track_row = geometry.track_start_row + geometry.track_len / 2;
        let expected = geometry.scroll_for_track_click(track_row);
        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            u32::from(track_row) * 20 + 19,
            Some(3),
        ));
        assert_eq!(app.detail_scroll(), expected);
    }

    #[test]
    fn drag_without_start_and_pixel_down_are_ignored_but_up_terminates() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            geometry.gutter.x,
            geometry.track_end_row(),
        ));
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(drag.active.is_some());
        assert!(!super::handle_pointer_event(
            &mut app,
            &mut layout,
            &mut drag,
            PointerEvent {
                kind: PointerKind::Down(PointerButton::Right),
                position: terminal::PointerPosition::AbsolutePixels { x: 0, y: 0 },
                modifiers: terminal::Modifiers::default(),
                pixel_generation: None,
            },
            &info,
        ));
        assert!(drag.active.is_some());
        assert!(!super::handle_pointer_event(
            &mut app,
            &mut layout,
            &mut drag,
            PointerEvent {
                kind: PointerKind::Up(PointerButton::Right),
                position: terminal::PointerPosition::AbsolutePixels { x: 0, y: 0 },
                modifiers: terminal::Modifiers::default(),
                pixel_generation: None,
            },
            &info,
        ));
        assert!(drag.active.is_none());
    }

    #[test]
    fn scroll_only_redraw_preserves_drag_but_stable_identity_changes_invalidate_it() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );

        let moved = scrollbar_info(&app, 100, 70, 1);
        drag.reconcile_render_info(&moved);
        assert!(drag.active.is_some(), "thumb start is not stable identity");

        let resized = scrollbar_info(&app, 100, 70, 2);
        drag.reconcile_render_info(&resized);
        assert!(drag.active.is_none());
    }

    #[test]
    fn resize_disappearance_and_another_down_cancel_drag() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let start = |app: &mut WatchApp,
                     layout: &mut detail_layout::DetailLayout,
                     drag: &mut DetailScrollbarDragState| {
            dispatch_mouse(
                app,
                layout,
                drag,
                &info,
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            );
        };

        start(&mut app, &mut layout, &mut drag);
        drag.reconcile_render_info(&view::RenderInfo::default());
        assert!(drag.active.is_none());

        start(&mut app, &mut layout, &mut drag);
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Right),
            0,
            0,
        );
        assert!(drag.active.is_none());

        start(&mut app, &mut layout, &mut drag);
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([resize(100, 40)]);
        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(drag.active.is_none());
    }

    #[test]
    fn stale_drag_geometry_cannot_mutate_a_new_detail_revision() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let old_info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &old_info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &old_info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(app.next_case());
        assert_eq!(app.detail_scroll(), 0);

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &old_info,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            u16::MAX,
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(drag.active.is_none());
    }

    #[test]
    fn one_event_batch_can_start_and_advance_a_valid_drag() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
            TerminalEvent::Pointer(pointer(MouseEventKind::Drag(MouseButton::Left), 0, 15)),
            TerminalEvent::Pointer(pointer(MouseEventKind::Drag(MouseButton::Left), 0, 20)),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(app.detail_scroll() > 0);
        assert!(drag.active.is_some());
    }

    #[test]
    fn non_drag_scroll_queues_later_pointer_geometry_until_redraw() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                MouseEventKind::ScrollDown,
                geometry.gutter.x.saturating_sub(1),
                geometry.thumb_start_row,
            )),
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);
        assert_eq!(events.len(), 1);
        assert!(drag.active.is_none());
    }

    #[test]
    fn track_seek_queues_a_later_drag_until_redraw() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                PointerKind::Down(PointerButton::Left),
                geometry.gutter.x,
                geometry.track_end_row().saturating_sub(1),
            )),
            TerminalEvent::Pointer(pointer(
                PointerKind::Drag(PointerButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(app.detail_scroll() > 0);
        assert_eq!(events.len(), 1);
        assert!(drag.active.is_none());
    }

    #[test]
    fn a_new_pointer_interaction_after_drag_waits_for_redrawn_thumb_geometry() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
            TerminalEvent::Pointer(pointer(MouseEventKind::Drag(MouseButton::Left), 0, 18)),
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(app.detail_scroll() > 0);
        assert_eq!(events.len(), 1);
        assert!(drag.active.is_some());
    }

    #[test]
    fn layout_changing_key_stops_later_mouse_events_until_redraw() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Char('s'), KeyEventKind::Press)),
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.track_end_row().saturating_sub(1),
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert_eq!(events.len(), 1);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn problem_case_and_detail_mode_revisions_invalidate_drag_identity() {
        let mut problem_app = app_with_problems(&[3, 3]);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&problem_app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut problem_app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(problem_app.next_problem());
        drag.reconcile_render_info(&scrollbar_info(&problem_app, 100, 0, 1));
        assert!(drag.active.is_none());

        let mut case_app = app();
        let info = scrollbar_info(&case_app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut case_app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(case_app.next_case());
        drag.reconcile_render_info(&scrollbar_info(&case_app, 100, 0, 1));
        assert!(drag.active.is_none());

        let mut mode_app = app();
        mode_app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let info = scrollbar_info(&mode_app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut mode_app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(mode_app.queue_stress(0, 1).is_some());
        drag.reconcile_render_info(&scrollbar_info(&mode_app, 100, 0, 1));
        assert!(drag.active.is_none());
    }

    #[test]
    fn track_seek_and_drag_cancel_width_reconciliation_but_keep_exact_result_useful() {
        let raw = "long normal detail line\n".repeat(4_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);

        for use_drag in [false, true] {
            let (mut layout, delayed) = pending_width_layout(&document);
            let mut app = app();
            app.set_detail_scroll_from_user(500);
            let info = scrollbar_info(&app, 10_000, 500, 1);
            let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
            let mut drag = DetailScrollbarDragState::default();

            if use_drag {
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    MouseEventKind::Down(MouseButton::Left),
                    geometry.gutter.x,
                    geometry.thumb_start_row,
                );
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    MouseEventKind::Drag(MouseButton::Left),
                    0,
                    geometry.track_end_row(),
                );
            } else {
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    MouseEventKind::Down(MouseButton::Left),
                    geometry.gutter.x,
                    geometry.track_end_row().saturating_sub(1),
                );
            }

            assert!(!layout.has_pending_width_anchor_for_test());
            assert!(layout.apply_count_result(delayed));
            assert!(layout.take_scroll_reconciliation().is_none());
            let viewport = layout.viewport(&document, 2, 70, 20, app.detail_scroll());
            assert!(viewport.exact_layout_identity.is_some());
        }
    }
    #[test]
    fn rerun_key_queues_current_source() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = run_rx.try_recv().unwrap();

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(!request.debug);
    }

    #[test]
    fn stress_key_queues_unbounded_stress_for_current_source() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('S'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = run_rx.try_recv().unwrap();
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(matches!(
            request.kind,
            message::RunKind::Stress { count: None, .. }
        ));
        assert_eq!(
            app.current_problem().unwrap().detail_mode,
            app::DetailMode::Stress
        );
    }

    #[test]
    fn rerun_uses_only_the_selected_problems_confirmed_source() {
        let mut app = app_with_problems(&[1, 1]);
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        app.source_changed(1, PathBuf::from("B.py"), Language::Python);
        assert!(app.select_problem(0));

        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = run_rx.try_recv().unwrap();
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
    }

    #[test]
    fn rerun_key_without_source_is_no_op() {
        let mut app = app();
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn debug_and_rerun_repeat_do_not_change_state_or_queue_requests() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );
        let first = run_rx.try_recv().unwrap();
        assert!(first.debug);

        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Repeat),
                &run_tx,
            )
            .unwrap()
        );
        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Repeat),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());
        assert_eq!(app.current_problem().unwrap().run.id, Some(first.run_id));
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn rerun_request_channel_disconnect_is_a_fatal_error() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();
        drop(run_rx);
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('r'),
            KeyEventKind::Press,
        ))]);

        let error =
            handle_terminal_events(&mut app, &view::RenderInfo::default(), &mut events, &run_tx)
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "run worker request channel disconnected");
    }

    #[test]
    fn debug_toggle_reruns_cpp_with_new_debug_state() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());

        let request = run_rx.try_recv().unwrap();

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(request.debug);

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(!app.debug_enabled());

        let request = run_rx.try_recv().unwrap();
        assert!(!request.debug);
    }
    #[test]
    fn debug_toggle_does_not_rerun_python() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn save_debug_rerun_and_save_keep_monotonic_run_ids_and_latest_state() {
        let mut app = app();
        let (message_tx, message_rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        message_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );
        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        message_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.py"),
                language: Language::Python,
            })
            .unwrap();
        assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());

        let requests: Vec<_> = run_rx.try_iter().collect();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.run_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.language, request.debug))
                .collect::<Vec<_>>(),
            [
                (Language::Cpp, false),
                (Language::Cpp, true),
                (Language::Cpp, true),
                (Language::Python, false),
            ]
        );

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(4));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Python));
        assert!(app.debug_enabled());

        assert!(!app.run_failed(0, 3, "stale failure".to_string()));
        assert_eq!(app.current_problem().unwrap().run.id, Some(4));
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );
    }

    #[test]
    fn rapid_save_debug_and_rerun_requests_keep_monotonic_latest_state() {
        let mut app = app();
        let (message_tx, message_rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        for operation in 0..300 {
            match operation % 3 {
                0 => {
                    message_tx
                        .send(Message::SourceChanged {
                            problem: 0,
                            path: PathBuf::from("A.cpp"),
                            language: Language::Cpp,
                        })
                        .unwrap();
                    assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());
                }
                1 => assert!(
                    handle_key_event(
                        &mut app,
                        key(KeyCode::Char('d'), KeyEventKind::Press),
                        &run_tx,
                    )
                    .unwrap()
                ),
                _ => assert!(
                    handle_key_event(
                        &mut app,
                        key(KeyCode::Char('r'), KeyEventKind::Press),
                        &run_tx,
                    )
                    .unwrap()
                ),
            }
        }

        let requests: Vec<_> = run_rx.try_iter().collect();
        assert_eq!(requests.len(), 300);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.run_id)
                .collect::<Vec<_>>(),
            (1..=300).collect::<Vec<_>>()
        );
        assert!(
            requests
                .iter()
                .all(|request| { request.problem == 0 && request.language == Language::Cpp })
        );

        let latest = requests.last().unwrap();
        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(latest.run_id));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(latest.language));
        assert_eq!(latest.debug, app.debug_enabled());
    }
}
