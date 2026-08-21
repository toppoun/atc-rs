#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

#[cfg(not(windows))]
compile_error!("mouse-probe Phase 0 intentionally supports only Windows");

mod sgr;

use sgr::{MouseKind, MouseReport, SgrMouseParser};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const ENABLE_MOUSE: &[u8] = b"\x1b[?1002h\x1b[?1016h";
const DISABLE_MOUSE: &[u8] = b"\x1b[?1016l\x1b[?1002l";
const QUERY_METRICS: &[u8] = b"\x1b[14t\x1b[16t";
const DISTINCT_LIMIT: usize = 4096;
const FIRST_LIMIT: usize = 12;
const LAST_LIMIT: usize = 12;

fn main() {
    if let Err(error) = run() {
        eprintln!("mouse-probe error: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    println!("atc-rs pixel mouse probe");
    println!();
    println!("Hold the LEFT mouse button and slowly drag vertically.");
    println!("Try moving less than one full text row.");
    println!("Release the button, then press q to finish.");
    println!();

    let mut terminal = TerminalSession::enter()?;
    let stdin = io::stdin();
    let mut input = stdin.lock();

    let metrics = query_metrics(terminal.input_handle(), &mut input)?;
    print_metrics_before_capture(&metrics);

    terminal.enable_mouse()?;
    println!("Capture active. No mouse events will be printed until q is pressed.");
    io::stdout().flush()?;

    let capture_result = capture(terminal.input_handle(), &mut input);
    let restore_result = terminal.restore();
    let stats = capture_result?;

    println!();
    print_results(&stats, &metrics);
    restore_result
}

struct TerminalSession {
    input_handle: HANDLE,
    output_handle: HANDLE,
    original_input_mode: u32,
    original_output_mode: u32,
    restored: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        let input_handle = get_std_handle(STD_INPUT_HANDLE)?;
        let output_handle = get_std_handle(STD_OUTPUT_HANDLE)?;
        let original_input_mode = get_console_mode(input_handle)?;
        let original_output_mode = get_console_mode(output_handle)?;

        set_console_mode(
            output_handle,
            original_output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )?;

        let input_mode =
            (original_input_mode | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS)
                & !(ENABLE_ECHO_INPUT
                    | ENABLE_LINE_INPUT
                    | ENABLE_PROCESSED_INPUT
                    | ENABLE_QUICK_EDIT_MODE);
        if let Err(error) = set_console_mode(input_handle, input_mode) {
            let _ = set_console_mode(output_handle, original_output_mode);
            return Err(error);
        }

        Ok(Self {
            input_handle,
            output_handle,
            original_input_mode,
            original_output_mode,
            restored: false,
        })
    }

    fn input_handle(&self) -> HANDLE {
        self.input_handle
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        write_and_flush(ENABLE_MOUSE)
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;
        if let Err(error) = write_and_flush(DISABLE_MOUSE) {
            first_error = Some(error);
        }
        if let Err(error) = set_console_mode(self.input_handle, self.original_input_mode)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = set_console_mode(self.output_handle, self.original_output_mode)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.restored = true;

        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn get_std_handle(kind: u32) -> io::Result<HANDLE> {
    // SAFETY: GetStdHandle has no pointer arguments and returns a borrowed OS handle.
    let handle = unsafe { GetStdHandle(kind) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn get_console_mode(handle: HANDLE) -> io::Result<u32> {
    let mut mode = 0;
    // SAFETY: `mode` is a valid out pointer and `handle` came from GetStdHandle.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(mode)
    }
}

fn set_console_mode(handle: HANDLE, mode: u32) -> io::Result<()> {
    // SAFETY: `handle` came from GetStdHandle and `mode` is a bitmask value.
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_and_flush(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

#[derive(Default)]
struct Metrics {
    terminal_pixels: Option<(u32, u32)>,
    cell_pixels: Option<(u32, u32)>,
}

fn query_metrics<R: Read>(input_handle: HANDLE, input: &mut R) -> io::Result<Metrics> {
    write_and_flush(QUERY_METRICS)?;

    let deadline = Instant::now() + Duration::from_millis(700);
    let mut reply = Vec::with_capacity(128);
    let mut buffer = [0_u8; 128];
    while Instant::now() < deadline && reply.len() < 2048 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(50));
        if !wait_for_input(input_handle, wait)? {
            continue;
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        reply.extend_from_slice(&buffer[..read]);
        let metrics = parse_metrics(&reply);
        if metrics.terminal_pixels.is_some() && metrics.cell_pixels.is_some() {
            return Ok(metrics);
        }
    }

    Ok(parse_metrics(&reply))
}

fn parse_metrics(reply: &[u8]) -> Metrics {
    let mut metrics = Metrics::default();
    let mut index = 0;
    while index + 4 <= reply.len() {
        if !reply[index..].starts_with(b"\x1b[") {
            index += 1;
            continue;
        }
        let Some(relative_end) = reply[index + 2..].iter().position(|byte| *byte == b't') else {
            break;
        };
        let end = index + 2 + relative_end;
        if let Ok(body) = std::str::from_utf8(&reply[index + 2..end]) {
            let values: Vec<_> = body
                .split(';')
                .map(str::parse::<u32>)
                .collect::<Result<_, _>>()
                .unwrap_or_default();
            match values.as_slice() {
                [4, height, width] => metrics.terminal_pixels = Some((*width, *height)),
                [6, height, width] => metrics.cell_pixels = Some((*width, *height)),
                _ => {}
            }
        }
        index = end + 1;
    }
    metrics
}

fn print_metrics_before_capture(metrics: &Metrics) {
    match metrics.terminal_pixels {
        Some((width, height)) => println!("Reported terminal pixel size: {width} x {height} px"),
        None => println!("Reported terminal pixel size: unavailable (CSI 14 t had no reply)"),
    }
    match metrics.cell_pixels {
        Some((width, height)) => println!("Reported cell pixel size: {width} x {height} px"),
        None => println!("Reported cell pixel size: unavailable (CSI 16 t had no reply)"),
    }
    println!();
}

fn wait_for_input(handle: HANDLE, timeout: Duration) -> io::Result<bool> {
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    // SAFETY: `handle` remains valid for the process lifetime; this only waits on it.
    let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if result == WAIT_OBJECT_0 {
        Ok(true)
    } else if result == WAIT_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(false)
    }
}

fn capture<R: Read>(input_handle: HANDLE, input: &mut R) -> io::Result<CaptureStats> {
    let mut parser = SgrMouseParser::default();
    let mut stats = CaptureStats::default();
    let mut buffer = [0_u8; 512];

    loop {
        if !wait_for_input(input_handle, Duration::from_millis(100))? {
            continue;
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];
        for report in parser.feed(bytes) {
            stats.record(report);
        }
        if bytes.iter().any(|byte| matches!(byte, b'q' | b'Q' | 3)) {
            break;
        }
    }

    Ok(stats)
}

#[derive(Default)]
struct CaptureStats {
    total: u64,
    down: u64,
    drag: u64,
    up: u64,
    min_x: Option<u32>,
    max_x: Option<u32>,
    min_y: Option<u32>,
    max_y: Option<u32>,
    distinct_x: BTreeSet<u32>,
    distinct_y: BTreeSet<u32>,
    x_saturated: bool,
    y_saturated: bool,
    first: Vec<MouseReport>,
    last: VecDeque<MouseReport>,
}

impl CaptureStats {
    fn record(&mut self, report: MouseReport) {
        self.total += 1;
        match report.kind {
            MouseKind::Down => self.down += 1,
            MouseKind::Drag => self.drag += 1,
            MouseKind::Up => self.up += 1,
        }
        update_min_max(&mut self.min_x, &mut self.max_x, report.raw_x);
        update_min_max(&mut self.min_y, &mut self.max_y, report.raw_y);
        insert_bounded(&mut self.distinct_x, report.raw_x, &mut self.x_saturated);
        insert_bounded(&mut self.distinct_y, report.raw_y, &mut self.y_saturated);

        if self.first.len() < FIRST_LIMIT {
            self.first.push(report.clone());
        }
        if self.last.len() == LAST_LIMIT {
            self.last.pop_front();
        }
        self.last.push_back(report);
    }
}

fn update_min_max(minimum: &mut Option<u32>, maximum: &mut Option<u32>, value: u32) {
    *minimum = Some(minimum.map_or(value, |current| current.min(value)));
    *maximum = Some(maximum.map_or(value, |current| current.max(value)));
}

fn insert_bounded(values: &mut BTreeSet<u32>, value: u32, saturated: &mut bool) {
    if values.contains(&value) {
        return;
    }
    if values.len() < DISTINCT_LIMIT {
        values.insert(value);
    } else {
        *saturated = true;
    }
}

fn print_results(stats: &CaptureStats, metrics: &Metrics) {
    println!("total parsed mouse reports: {}", stats.total);
    println!("DOWN count: {}", stats.down);
    println!("DRAG count: {}", stats.drag);
    println!("UP count: {}", stats.up);
    println!();
    print_distinct("X", &stats.distinct_x, stats.x_saturated);
    print_distinct("Y", &stats.distinct_y, stats.y_saturated);
    print_range("X", stats.min_x, stats.max_x);
    print_range("Y", stats.min_y, stats.max_y);
    println!();
    print_optional_size("reported terminal pixel size", metrics.terminal_pixels);
    print_optional_size("reported cell pixel size", metrics.cell_pixels);
    println!();
    print_representative_events(stats);
    println!();

    let confirmed = print_sub_cell_evidence(stats, metrics.cell_pixels.map(|(_, height)| height));
    println!();
    if stats.total == 0 {
        println!("RESULT: NO PIXEL MOUSE REPORTS");
    } else if confirmed {
        println!("RESULT: PIXEL MOUSE CONFIRMED");
    } else {
        println!("RESULT: MOUSE REPORTS RECEIVED BUT SUB-CELL MOVEMENT NOT CONFIRMED");
    }
}

fn print_distinct(axis: &str, values: &BTreeSet<u32>, saturated: bool) {
    let count = if saturated {
        format!(">= {}", values.len())
    } else {
        values.len().to_string()
    };
    let sample = values
        .iter()
        .take(32)
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if values.len() > 32 { ", ..." } else { "" };
    println!("distinct {axis} values: {count} [{sample}{suffix}]");
}

fn print_range(axis: &str, minimum: Option<u32>, maximum: Option<u32>) {
    match (minimum, maximum) {
        (Some(minimum), Some(maximum)) => println!("min/max {axis}: {minimum} / {maximum}"),
        _ => println!("min/max {axis}: unavailable"),
    }
}

fn print_optional_size(label: &str, size: Option<(u32, u32)>) {
    match size {
        Some((width, height)) => println!("{label}: {width} x {height} px"),
        None => println!("{label}: unavailable"),
    }
}

fn print_representative_events(stats: &CaptureStats) {
    println!("representative parsed events and raw SGR sequences:");
    if stats.total == 0 {
        println!("  (none)");
        return;
    }
    for report in &stats.first {
        print_report("first", report);
    }
    if stats.total as usize > FIRST_LIMIT {
        for report in &stats.last {
            print_report("last ", report);
        }
    }
}

fn print_report(label: &str, report: &MouseReport) {
    println!(
        "  {label} {:<4} x={} y={} raw={}",
        report.kind,
        report.raw_x,
        report.raw_y,
        escape_bytes(&report.raw)
    );
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for byte in bytes {
        match byte {
            b'\x1b' => escaped.push_str("\\x1b"),
            0x20..=0x7e => escaped.push(char::from(*byte)),
            _ => escaped.push_str(&format!("\\x{byte:02x}")),
        }
    }
    escaped
}

fn print_sub_cell_evidence(stats: &CaptureStats, cell_height: Option<u32>) -> bool {
    let Some(cell_height) = cell_height.filter(|height| *height > 0) else {
        println!("sub-cell evidence: cannot classify automatically without cell pixel height");
        return false;
    };

    let zero_based = find_same_row_pair(&stats.distinct_y, cell_height, false);
    let one_based = find_same_row_pair(&stats.distinct_y, cell_height, true);
    print_origin_witness("zero-based", zero_based);
    print_origin_witness("one-based", one_based);

    if stats.distinct_y.contains(&0) {
        println!(
            "coordinate-origin evidence: raw Y=0 was observed, proving zero-based coordinates"
        );
        zero_based.is_some()
    } else {
        println!("coordinate-origin evidence: no raw zero observed; origin remains ambiguous");
        zero_based.is_some() && one_based.is_some()
    }
}

fn find_same_row_pair(
    values: &BTreeSet<u32>,
    cell_height: u32,
    one_based: bool,
) -> Option<(u32, u32, u32)> {
    let mut first_by_row = BTreeMap::new();
    for &raw_y in values {
        let adjusted = if one_based {
            raw_y.checked_sub(1)?
        } else {
            raw_y
        };
        let row = adjusted / cell_height;
        if let Some(&first) = first_by_row.get(&row) {
            return Some((first, raw_y, row));
        }
        first_by_row.insert(row, raw_y);
    }
    None
}

fn print_origin_witness(label: &str, witness: Option<(u32, u32, u32)>) {
    match witness {
        Some((first, second, row)) => println!(
            "sub-cell evidence ({label} interpretation): raw Y {first} and {second} share text row {row}"
        ),
        None => println!("sub-cell evidence ({label} interpretation): not observed"),
    }
}
