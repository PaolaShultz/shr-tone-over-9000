use crate::audio::{AudioClient, AudioTelemetry, TelemetrySnapshot};
use crate::midi::{MidiAction, MidiController};
use crate::model_dir::ModelDirectory;
use crate::nam::{ModelMetadata, NamModel};
use crate::params::{ParamId, Parameters};
use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Span, Spans};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::fs;
use std::io::{self, Stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const WIDTH: u16 = 40;
const HEIGHT: u16 = 13;
const TICK: Duration = Duration::from_millis(33);
const STATUS_LIFETIME: Duration = Duration::from_secs(3);

pub fn run(
    audio: &mut AudioClient,
    mut models: ModelDirectory,
    mut midi: MidiController,
    params: Arc<Parameters>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        disable_raw_mode().ok();
        return Err(error).context("enter TUI terminal mode");
    }
    let _terminal_mode = TerminalModeGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_loop(
        &mut terminal,
        audio,
        &mut models,
        &mut midi,
        params,
        shutdown,
    );

    terminal.show_cursor().ok();
    result
}

struct TerminalModeGuard;

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        let mut stdout = io::stdout();
        execute!(stdout, DisableMouseCapture, LeaveAlternateScreen).ok();
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    midi: &mut MidiController,
    params: Arc<Parameters>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let telemetry = audio.telemetry();
    let mut app = App::new(telemetry, params);
    if let Some(error) = midi.startup_error() {
        app.set_status(format!("MIDI {error}"), false);
    }
    let mut last_draw = Instant::now() - TICK;

    while !shutdown.load(Ordering::Relaxed) && !app.quit {
        audio.drain_retired();
        if let Err(error) = models.poll_watch() {
            app.set_status(error.to_string(), true);
        }
        while let Some(action) = midi.next_action(&app.params) {
            handle_midi(action, &mut app, audio, models, midi);
        }
        app.refresh_temperature();

        if last_draw.elapsed() >= TICK {
            let cpu = audio.jack_cpu_load();
            terminal
                .draw(|frame| draw(frame, &app, models, midi, audio.period_frames(), cpu))
                .context("draw TUI")?;
            last_draw = Instant::now();
        }

        let timeout = TICK.saturating_sub(last_draw.elapsed());
        if event::poll(timeout).context("poll terminal input")? {
            match event::read().context("read terminal input")? {
                Event::Key(key) => handle_key(key, &mut app, audio, models, midi),
                Event::Mouse(mouse) => handle_mouse(mouse, &mut app, audio, models, midi),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
    }
    Ok(())
}

struct App {
    params: Arc<Parameters>,
    telemetry: AudioTelemetry,
    loaded: Option<ModelMetadata>,
    focus: ParamId,
    help: bool,
    quit: bool,
    status: Option<StatusMessage>,
    temperature_c: Option<f32>,
    temperature_read: Instant,
    drag: Option<DragState>,
}

struct StatusMessage {
    text: String,
    since: Instant,
    fault: bool,
}

struct DragState {
    parameter: ParamId,
    previous_row: u16,
}

impl App {
    fn new(telemetry: AudioTelemetry, params: Arc<Parameters>) -> Self {
        Self {
            params,
            telemetry,
            loaded: None,
            focus: ParamId::InputGain,
            help: false,
            quit: false,
            status: None,
            temperature_c: None,
            temperature_read: Instant::now() - Duration::from_secs(2),
            drag: None,
        }
    }

    fn set_status(&mut self, text: impl Into<String>, fault: bool) {
        self.status = Some(StatusMessage {
            text: text.into(),
            since: Instant::now(),
            fault,
        });
    }

    fn refresh_temperature(&mut self) {
        if self.temperature_read.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.temperature_read = Instant::now();
        self.temperature_c = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
            .ok()
            .and_then(|text| text.trim().parse::<f32>().ok())
            .map(|value| value / 1_000.0);
    }
}

fn handle_key(
    key: KeyEvent,
    app: &mut App,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    midi: &mut MidiController,
) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    if app.help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            app.help = false;
        }
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.help = true,
        KeyCode::Up | KeyCode::Char('-') => models.select_relative(-1),
        KeyCode::Down | KeyCode::Char('+') => models.select_relative(1),
        KeyCode::Enter => load_selected(app, audio, models, midi),
        KeyCode::Char('r') => reload_current(app, audio, midi),
        KeyCode::Char('i' | 'I') => {
            app.focus = ParamId::InputGain;
            app.params
                .adjust_db(ParamId::InputGain, gain_step(key.modifiers));
        }
        KeyCode::Char('o' | 'O') => {
            app.focus = ParamId::OutputGain;
            app.params
                .adjust_db(ParamId::OutputGain, gain_step(key.modifiers));
        }
        KeyCode::Char('b') => {
            app.focus = ParamId::Bypass;
            app.params.toggle_bypass();
        }
        KeyCode::Char('m') => {
            if app.focus == ParamId::Oversampling {
                app.set_status("OVERSAMPLING OFF IN V1", false);
            } else {
                midi.enter_learn(app.focus);
                app.set_status(format!("LEARN {} · MOVE NEXT CC", app.focus.label()), false);
            }
        }
        _ => {}
    }
}

fn gain_step(modifiers: KeyModifiers) -> f32 {
    if modifiers.contains(KeyModifiers::SHIFT) {
        3.0
    } else {
        0.5
    }
}

fn handle_mouse(
    mouse: MouseEvent,
    app: &mut App,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    midi: &mut MidiController,
) {
    if app.help {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.help = false;
        }
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => models.select_relative(-1),
        MouseEventKind::ScrollDown => models.select_relative(1),
        MouseEventKind::Down(MouseButton::Left) => match mouse.row {
            5 => load_selected(app, audio, models, midi),
            6 | 7 => {
                let parameter = parameter_at_column(mouse.column);
                app.focus = parameter;
                match parameter {
                    ParamId::InputGain | ParamId::OutputGain => {
                        app.drag = Some(DragState {
                            parameter,
                            previous_row: mouse.row,
                        });
                    }
                    ParamId::Bypass => app.params.toggle_bypass(),
                    ParamId::Oversampling => app.set_status("OVERSAMPLING OFF IN V1", false),
                }
            }
            8 => {
                if app.focus != ParamId::Oversampling {
                    midi.enter_learn(app.focus);
                    app.set_status(format!("LEARN {} · MOVE NEXT CC", app.focus.label()), false);
                }
            }
            _ => {}
        },
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(drag) = app.drag.as_mut() {
                let rows = i32::from(drag.previous_row) - i32::from(mouse.row);
                if rows != 0 {
                    app.params.adjust_db(drag.parameter, rows as f32 * 0.5);
                    drag.previous_row = mouse.row;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.drag = None,
        _ => {}
    }
}

fn parameter_at_column(column: u16) -> ParamId {
    match column {
        0..=9 => ParamId::InputGain,
        10..=19 => ParamId::OutputGain,
        20..=29 => ParamId::Bypass,
        _ => ParamId::Oversampling,
    }
}

fn handle_midi(
    action: MidiAction,
    app: &mut App,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    midi: &mut MidiController,
) {
    match action {
        MidiAction::Select(direction) => models.select_relative(direction),
        MidiAction::Load => load_selected(app, audio, models, midi),
        MidiAction::Parameter(parameter) => app.focus = parameter,
        MidiAction::Learned {
            parameter,
            channel,
            cc,
        } => {
            app.focus = parameter;
            app.set_status(
                format!("LEARNED {} · CH{} CC{}", parameter.label(), channel + 1, cc),
                false,
            );
        }
        MidiAction::OversamplingOff => app.set_status("OVERSAMPLING OFF IN V1", false),
    }
}

fn load_selected(
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
) {
    let Some(entry) = models.selected() else {
        app.set_status(
            format!("NO .NAM FILES IN {}", models.root().display()),
            true,
        );
        return;
    };
    load_path(&entry.path, app, audio, midi);
}

fn reload_current(app: &mut App, audio: &mut AudioClient, midi: &mut MidiController) {
    let Some(path) = app.loaded.as_ref().map(|metadata| metadata.path.clone()) else {
        app.set_status("NO CURRENT MODEL TO RELOAD", true);
        return;
    };
    load_path(&path, app, audio, midi);
}

fn load_path(path: &Path, app: &mut App, audio: &mut AudioClient, midi: &mut MidiController) {
    match NamModel::load(path, crate::audio::SAMPLE_RATE, audio.period_frames()) {
        Ok(model) => {
            let metadata = model.metadata().clone();
            if let Err(error) = audio.queue_model(model) {
                app.set_status(error.to_string(), true);
                return;
            }
            app.params.reset_for_model();
            midi.arm_pickup(&app.params);
            app.loaded = Some(metadata.clone());
            app.set_status(format!("LOADED {}", metadata.name), false);
        }
        Err(error) => app.set_status(error.to_string(), true),
    }
}

fn draw(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    app: &App,
    models: &ModelDirectory,
    midi: &MidiController,
    period: u32,
    cpu: f32,
) {
    let size = frame.size();
    if size.width < WIDTH || size.height < HEIGHT {
        frame.render_widget(Clear, size);
        frame.render_widget(
            Paragraph::new("NEED 40x13 TERMINAL").style(Style::default().fg(Color::LightYellow)),
            size,
        );
        return;
    }
    let area = Rect::new(size.x, size.y, WIDTH, HEIGHT);
    // Rows 1-12 are body-owned. Row 13 is cleared and replaced only by
    // draw_status(), matching shr-daw's shared working-screen contract.
    frame.render_widget(Clear, Rect::new(area.x, area.y, area.width, 12));
    draw_model_header(frame, area, app.loaded.as_ref());
    let telemetry = app.telemetry.snapshot();
    draw_meter(
        frame,
        row(area, 3),
        "IN",
        telemetry.input_peak,
        telemetry.input_rms,
    );
    draw_meter(
        frame,
        row(area, 4),
        "OUT",
        telemetry.output_peak,
        telemetry.output_rms,
    );
    draw_selection(frame, row(area, 5), models);
    draw_parameters(frame, area, app);
    draw_midi(frame, row(area, 8), app, midi);
    draw_hints(frame, area);
    draw_status(frame, row(area, 12), app, midi, telemetry, period, cpu);
    if app.help {
        draw_help(frame, area);
    }
}

fn draw_model_header(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    metadata: Option<&ModelMetadata>,
) {
    let Some(metadata) = metadata else {
        draw_text(frame, row(area, 0), "MODEL --", Color::White);
        draw_text(
            frame,
            row(area, 1),
            "SR 48000  FILE --  LOAD --",
            Color::Gray,
        );
        draw_text(frame, row(area, 2), "META --", Color::Gray);
        return;
    };
    draw_text(
        frame,
        row(area, 0),
        &format!("MODEL {} [{}]", metadata.name, metadata.architecture),
        Color::White,
    );
    draw_text(
        frame,
        row(area, 1),
        &format!(
            "SR {} FILE {} LOAD {}",
            metadata.expected_sample_rate.unwrap_or(48_000),
            human_bytes(metadata.file_bytes),
            metadata.loaded_at_utc
        ),
        Color::Gray,
    );
    draw_text(
        frame,
        row(area, 2),
        &format!(
            "META IN {} OUT {} {}W v{}",
            level_text(metadata.input_level_dbu),
            level_text(metadata.output_level_dbu),
            metadata.weight_count,
            metadata.version
        ),
        Color::Gray,
    );
}

fn draw_meter(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    label: &str,
    peak: f32,
    rms: f32,
) {
    const CELLS: usize = 16;
    let peak_db = amplitude_db(peak);
    let rms_db = amplitude_db(rms);
    let peak_cells = meter_cells(peak_db, CELLS);
    let rms_cells = meter_cells(rms_db, CELLS);
    let mut spans = vec![Span::styled(
        format!("{label} P{:>5} R{:>5} ", db_text(peak_db), db_text(rms_db)),
        Style::default().fg(Color::Gray),
    )];
    for index in 0..CELLS {
        let (symbol, color) = if index < rms_cells {
            ('=', threshold_color(cell_db(index, CELLS)))
        } else if index < peak_cells {
            ('+', threshold_color(cell_db(index, CELLS)))
        } else {
            ('.', Color::DarkGray)
        };
        spans.push(Span::styled(symbol.to_string(), Style::default().fg(color)));
    }
    frame.render_widget(Paragraph::new(Spans::from(spans)), area);
}

fn draw_selection(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    models: &ModelDirectory,
) {
    let text = models.selected().map_or_else(
        || "FILES 0/0 > --".to_owned(),
        |entry| {
            format!(
                "FILES {}/{} > {} {}",
                models.selected_index() + 1,
                models.entries().len(),
                entry.name,
                human_bytes(entry.file_bytes)
            )
        },
    );
    draw_text(frame, area, &text, Color::LightYellow);
}

fn draw_parameters(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, app: &App) {
    let parameters = [
        ParamId::InputGain,
        ParamId::OutputGain,
        ParamId::Bypass,
        ParamId::Oversampling,
    ];
    for (index, parameter) in parameters.into_iter().enumerate() {
        let cell = Rect::new(area.x + index as u16 * 10, area.y + 6, 10, 1);
        frame.render_widget(
            Paragraph::new(parameter.label())
                .alignment(Alignment::Center)
                .style(Style::default().fg(if app.focus == parameter {
                    Color::White
                } else {
                    Color::Gray
                })),
            cell,
        );
        let value = match parameter {
            ParamId::InputGain => format!("{:+.1} dB", app.params.snapshot().input_gain_db),
            ParamId::OutputGain => format!("{:+.1} dB", app.params.snapshot().output_gain_db),
            ParamId::Bypass => {
                if app.params.snapshot().bypass {
                    "ON".to_owned()
                } else {
                    "OFF".to_owned()
                }
            }
            ParamId::Oversampling => "OFF".to_owned(),
        };
        let mut style = Style::default().fg(parameter_color(app.params.indicator_delta(parameter)));
        if app.focus == parameter {
            style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        }
        frame.render_widget(
            Paragraph::new(value)
                .alignment(Alignment::Center)
                .style(style),
            Rect::new(cell.x, area.y + 7, cell.width, 1),
        );
    }
}

fn draw_midi(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    app: &App,
    midi: &MidiController,
) {
    let learn = midi
        .learning()
        .map(|parameter| format!("LEARN {}", parameter.label()))
        .unwrap_or_else(|| "LEARN OFF".to_owned());
    let last = midi
        .last_cc()
        .map(|(channel, cc)| format!("LAST CH{} CC{}", channel + 1, cc))
        .unwrap_or_else(|| "LAST CH-- CC--".to_owned());
    draw_text(
        frame,
        area,
        &format!("MIDI {learn} · {last} · FOCUS {}", app.focus.label()),
        if midi.learning().is_some() {
            Color::LightYellow
        } else {
            Color::Gray
        },
    );
}

fn draw_hints(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect) {
    draw_halves(frame, row(area, 10), "[UP/DN +/-] SELECT", "[ENTER] LOAD");
    draw_halves(
        frame,
        row(area, 11),
        "[I/O] GAIN [B] BYP",
        "[M] LEARN [?] HELP",
    );
}

fn draw_status(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    app: &App,
    midi: &MidiController,
    telemetry: TelemetrySnapshot,
    period: u32,
    cpu: f32,
) {
    let automatic_fault = if telemetry.server_lost {
        Some("JACK SERVER LOST")
    } else if telemetry.process_fault {
        Some("AUDIO PROCESS FAULT")
    } else {
        None
    };
    let visible_status = app
        .status
        .as_ref()
        .filter(|status| status.fault || status.since.elapsed() < STATUS_LIFETIME);
    let (message, message_color) = if let Some(fault) = automatic_fault {
        (fault.to_owned(), Color::LightYellow)
    } else if let Some(status) = visible_status {
        (
            status.text.clone(),
            if status.fault {
                Color::LightYellow
            } else {
                Color::Gray
            },
        )
    } else {
        let model = app
            .loaded
            .as_ref()
            .map(|metadata| fit_cells(&metadata.name, 8))
            .unwrap_or_else(|| "--".to_owned());
        let midi = fit_cells(midi.device_label(), 6);
        let temperature = app
            .temperature_c
            .map(|value| format!("T{value:.0}C"))
            .unwrap_or_else(|| "T--C".to_owned());
        (
            format!(
                "{model} X{} P{period} M:{midi} C{cpu:.0}% {temperature}",
                telemetry.xruns
            ),
            Color::Gray,
        )
    };
    frame.render_widget(
        Paragraph::new(Spans::from(vec![
            Span::styled(
                "■",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(fit_cells(&message, 38), Style::default().fg(message_color)),
        ]))
        .style(Style::default().bg(Color::Black)),
        area,
    );
}

fn draw_help(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect) {
    let popup = Rect::new(area.x + 1, area.y + 1, 38, 8);
    frame.render_widget(Clear, popup);
    let help = [
        "UP/DOWN or +/-  choose model",
        "ENTER load · R reload · B bypass",
        "I/O +0.5 dB · Shift I/O +3 dB",
        "Tap a control; drag gains vertically",
        "M learns next positional CC",
        "Q or ESC quits · ? closes help",
    ]
    .join("\n");
    frame.render_widget(
        Paragraph::new(help)
            .block(
                Block::default()
                    .title(" HELP ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightYellow)),
            )
            .style(Style::default().fg(Color::White)),
        popup,
    );
}

fn draw_text(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, text: &str, color: Color) {
    frame.render_widget(
        Paragraph::new(fit_cells(text, usize::from(area.width))).style(Style::default().fg(color)),
        area,
    );
}

fn draw_halves(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, left: &str, right: &str) {
    frame.render_widget(
        Paragraph::new(fit_cells(left, 20))
            .alignment(Alignment::Left)
            .style(Style::default().fg(Color::White)),
        Rect::new(area.x, area.y, 20, 1),
    );
    frame.render_widget(
        Paragraph::new(fit_cells(right, 20))
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::White)),
        Rect::new(area.x + 20, area.y, 20, 1),
    );
}

fn row(area: Rect, index: u16) -> Rect {
    Rect::new(area.x, area.y + index, area.width, 1)
}

fn parameter_color(delta: f32) -> Color {
    // Owned by shr-daw AGENTS.md and docs/CONTROLLER_INTERFACE.md.
    if delta < -0.03 {
        Color::Green
    } else if delta > 0.03 {
        Color::Red
    } else {
        Color::LightYellow
    }
}

fn amplitude_db(value: f32) -> f32 {
    if value > 0.000_001 {
        20.0 * value.log10()
    } else {
        -60.0
    }
}

fn db_text(value: f32) -> String {
    if value <= -60.0 {
        "-inf".to_owned()
    } else {
        format!("{value:.1}")
    }
}

fn meter_cells(db: f32, cells: usize) -> usize {
    (((db.clamp(-60.0, 0.0) + 60.0) / 60.0) * cells as f32).ceil() as usize
}

fn cell_db(index: usize, cells: usize) -> f32 {
    -60.0 + (index + 1) as f32 * (60.0 / cells as f32)
}

fn threshold_color(db: f32) -> Color {
    if db < -12.0 {
        Color::Green
    } else if db <= -3.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn level_text(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:+.1}dBu"))
        .unwrap_or_else(|| "--".to_owned())
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.0}K", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes}B")
    }
}

fn fit_cells(text: &str, cells: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > cells {
            break;
        }
        output.push(character);
        used += width;
    }
    output
}
