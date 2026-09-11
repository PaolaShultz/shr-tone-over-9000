use crate::audio::{AudioClient, AudioTelemetry, TelemetrySnapshot};
use crate::hub::{self, HubSearchPage};
use crate::midi::{MidiAction, MidiController};
use crate::model_dir::ModelDirectory;
use crate::nam::{ModelMetadata, NamChain};
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
use qrcode::types::Color as QrColor;
use qrcode::{EcLevel, QrCode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Span, Spans};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::fs;
use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const WIDTH: u16 = 40;
const HEIGHT: u16 = 13;
const TICK: Duration = Duration::from_millis(33);
const STATUS_LIFETIME: Duration = Duration::from_secs(3);
const MAX_CHAIN_MODELS: usize = 4;
const MAX_HUB_INPUT_BYTES: usize = 512;

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
    if let Some(warning) = models.take_metadata_warning() {
        app.set_status(warning, true);
    }
    if let Some(error) = midi.startup_error() {
        app.set_status(format!("MIDI {error}"), false);
    }
    let mut last_draw = Instant::now() - TICK;

    while !shutdown.load(Ordering::Relaxed) && !app.quit {
        audio.drain_retired();
        poll_hub(&mut app, models);
        if let Err(error) = models.poll_watch() {
            app.set_status(error.to_string(), true);
        }
        if let Some(warning) = models.take_metadata_warning() {
            app.set_status(warning, true);
        }
        while let Some(action) = midi.next_action(&app.params) {
            if app.hub.is_open() {
                handle_hub_midi(action, &mut app);
            } else {
                handle_midi(action, &mut app, audio, models, midi);
            }
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
                Event::Paste(text) => handle_paste(&mut app, &text),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
            }
        }
        if let Some(text) = app.external_text.take() {
            show_external_text(terminal, &text)?;
            last_draw = Instant::now() - TICK;
        }
    }
    Ok(())
}

struct App {
    params: Arc<Parameters>,
    telemetry: AudioTelemetry,
    chain: Vec<ModelMetadata>,
    cabinet_variants: Vec<Vec<ModelMetadata>>,
    chain_focus: usize,
    mic_focus: bool,
    focus: ParamId,
    help: bool,
    quit: bool,
    status: Option<StatusMessage>,
    temperature_c: Option<f32>,
    temperature_read: Instant,
    drag: Option<DragState>,
    hub: HubUi,
    external_text: Option<String>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HubScreen {
    Closed,
    ClientId,
    Connecting,
    Query,
    Searching,
    Results,
    Detail,
    Downloading,
}

struct HubUi {
    screen: HubScreen,
    input: TextInput,
    query: String,
    architecture: String,
    results: Option<HubSearchPage>,
    selected: usize,
    authorization_qr: Option<Vec<String>>,
    error: Option<String>,
    operation_id: u64,
    cancelled: Arc<AtomicBool>,
    events: Receiver<HubEvent>,
    event_sender: Sender<HubEvent>,
}

enum HubEvent {
    AuthorizationReady {
        operation_id: u64,
        qr: Result<Vec<String>, String>,
    },
    Connected(u64, Result<(), String>),
    SearchFinished(u64, Result<HubSearchPage, String>),
    DownloadFinished(u64, Result<PathBuf, String>),
}

impl HubEvent {
    fn operation_id(&self) -> u64 {
        match self {
            Self::AuthorizationReady { operation_id, .. }
            | Self::Connected(operation_id, _)
            | Self::SearchFinished(operation_id, _)
            | Self::DownloadFinished(operation_id, _) => *operation_id,
        }
    }
}

#[derive(Default)]
struct TextInput {
    value: String,
    cursor: usize,
}

impl TextInput {
    fn set(&mut self, value: String) {
        self.value = value;
        self.cursor = self.value.len();
    }

    fn insert(&mut self, text: &str) {
        let room = MAX_HUB_INPUT_BYTES.saturating_sub(self.value.len());
        let mut accepted = text
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        while accepted.len() > room {
            accepted.pop();
        }
        self.value.insert_str(self.cursor, &accepted);
        self.cursor += accepted.len();
    }

    fn backspace(&mut self) {
        if let Some((previous, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.value.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.value
                .drain(self.cursor..self.cursor + character.len_utf8());
        }
    }

    fn move_left(&mut self) {
        if let Some((previous, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.cursor = previous;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }
}

impl HubUi {
    fn new() -> Self {
        let (event_sender, events) = mpsc::channel();
        Self {
            screen: HubScreen::Closed,
            input: TextInput::default(),
            query: String::new(),
            architecture: "2".to_owned(),
            results: None,
            selected: 0,
            authorization_qr: None,
            error: None,
            operation_id: 0,
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            event_sender,
        }
    }

    fn is_open(&self) -> bool {
        self.screen != HubScreen::Closed
    }

    fn selected_item(&self) -> Option<&hub::HubSearchItem> {
        self.results.as_ref()?.items.get(self.selected)
    }

    fn select_relative(&mut self, direction: i8) {
        let Some(results) = &self.results else {
            return;
        };
        if results.items.is_empty() {
            self.selected = 0;
        } else {
            self.selected = relative_index(self.selected, results.items.len(), direction);
        }
    }

    fn begin_operation(&mut self) -> u64 {
        self.operation_id = self.operation_id.wrapping_add(1);
        self.operation_id
    }
}

impl App {
    fn new(telemetry: AudioTelemetry, params: Arc<Parameters>) -> Self {
        Self {
            params,
            telemetry,
            chain: Vec::new(),
            cabinet_variants: Vec::new(),
            chain_focus: 0,
            mic_focus: false,
            focus: ParamId::InputGain,
            help: false,
            quit: false,
            status: None,
            temperature_c: None,
            temperature_read: Instant::now() - Duration::from_secs(2),
            drag: None,
            hub: HubUi::new(),
            external_text: None,
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
    if app.hub.is_open() {
        handle_hub_key(key, app, models);
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.help = true,
        KeyCode::Char('h' | 'H') => open_hub(app),
        KeyCode::Up | KeyCode::Char('-') => browse_or_select_mic(app, audio, models, midi, -1),
        KeyCode::Down | KeyCode::Char('+') => browse_or_select_mic(app, audio, models, midi, 1),
        KeyCode::Left => focus_chain_relative(app, -1),
        KeyCode::Right => focus_chain_relative(app, 1),
        KeyCode::Enter => replace_focused(app, audio, models, midi),
        KeyCode::Char('a') => add_selected(app, audio, models, midi),
        KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
            remove_focused(app, audio, models, midi)
        }
        KeyCode::Char('[') => move_focused(app, audio, models, midi, -1),
        KeyCode::Char(']') => move_focused(app, audio, models, midi, 1),
        KeyCode::Char('r') => reload_chain(app, audio, models, midi),
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

fn open_hub(app: &mut App) {
    match hub::is_connected() {
        Ok(true) => {
            app.hub.error = None;
            app.hub.screen = HubScreen::Query;
            app.hub.input.set(app.hub.query.clone());
            app.set_status("HUB · TYPE AN EXACT METADATA SEARCH", false);
        }
        Ok(false) => match hub::configured_client_id() {
            Ok(client_id) => {
                app.hub.error = None;
                app.hub.screen = HubScreen::ClientId;
                app.hub.input.set(client_id);
                app.set_status("HUB FIRST USE · CONNECT TONE3000", false);
            }
            Err(error) => app.set_status(format!("HUB CONFIG · {error}"), true),
        },
        Err(error) => app.set_status(format!("HUB CREDENTIALS · {error}"), true),
    }
}

fn handle_paste(app: &mut App, text: &str) {
    if matches!(app.hub.screen, HubScreen::ClientId | HubScreen::Query) {
        app.hub.input.insert(text);
    }
}

fn handle_hub_key(key: KeyEvent, app: &mut App, models: &mut ModelDirectory) {
    match app.hub.screen {
        HubScreen::Closed => {}
        HubScreen::ClientId => match key.code {
            KeyCode::Esc => app.hub.screen = HubScreen::Closed,
            KeyCode::Enter => start_hub_connect(app),
            _ => {
                app.hub.error = None;
                edit_hub_input(key, &mut app.hub.input);
            }
        },
        HubScreen::Connecting => {
            if key.code == KeyCode::Esc {
                app.hub.cancelled.store(true, Ordering::Relaxed);
                app.hub.begin_operation();
                app.hub.screen = HubScreen::ClientId;
                app.set_status("TONE3000 CONNECTION CANCELLED", false);
            }
        }
        HubScreen::Query => match key.code {
            KeyCode::Esc => app.hub.screen = HubScreen::Closed,
            KeyCode::Enter => start_hub_search(app, 1),
            KeyCode::Tab => {
                app.hub.architecture = match app.hub.architecture.as_str() {
                    "2" => "1",
                    "1" => "custom",
                    _ => "2",
                }
                .to_owned();
            }
            _ => {
                app.hub.error = None;
                edit_hub_input(key, &mut app.hub.input);
            }
        },
        HubScreen::Searching => {
            if key.code == KeyCode::Esc {
                app.hub.cancelled.store(true, Ordering::Relaxed);
                app.set_status("CANCELLING HUB SEARCH …", false);
            }
        }
        HubScreen::Results => match key.code {
            KeyCode::Esc => app.hub.screen = HubScreen::Closed,
            KeyCode::Up => app.hub.select_relative(-1),
            KeyCode::Down => app.hub.select_relative(1),
            KeyCode::Enter => {
                if app.hub.selected_item().is_some() {
                    app.hub.screen = HubScreen::Detail;
                }
            }
            KeyCode::Char('d' | 'D') => start_hub_download(app, models),
            KeyCode::Char('n' | 'N') => {
                app.hub.input.set(app.hub.query.clone());
                app.hub.screen = HubScreen::Query;
            }
            KeyCode::Left => change_hub_page(app, -1),
            KeyCode::Right => change_hub_page(app, 1),
            _ => {}
        },
        HubScreen::Detail => match key.code {
            KeyCode::Esc | KeyCode::Enter => app.hub.screen = HubScreen::Results,
            KeyCode::Up => app.hub.select_relative(-1),
            KeyCode::Down => app.hub.select_relative(1),
            KeyCode::Char('d' | 'D') => start_hub_download(app, models),
            KeyCode::Char('u' | 'U') => {
                if let Some(item) = app.hub.selected_item() {
                    app.external_text = Some(format!(
                        "TONE3000 source for t3k:model:{}\n\n{}\n\nPress Enter to return to the TUI.",
                        item.model_id, item.source_url
                    ));
                }
            }
            _ => {}
        },
        HubScreen::Downloading => {
            if key.code == KeyCode::Esc {
                app.set_status("FINISHING ONE VERIFIED DOWNLOAD · PLEASE WAIT", false);
            }
        }
    }
}

fn edit_hub_input(key: KeyEvent, input: &mut TextInput) {
    match key.code {
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            input.insert(&character.to_string());
        }
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.cursor = 0,
        KeyCode::End => input.cursor = input.value.len(),
        _ => {}
    }
}

fn start_hub_connect(app: &mut App) {
    let client_id = app.hub.input.value.trim().to_owned();
    let redirect_uri = match hub::suggested_redirect_uri() {
        Ok(uri) => uri,
        Err(error) => {
            app.hub.error = Some(error.to_string());
            app.set_status(error.to_string(), true);
            return;
        }
    };
    app.hub.cancelled = Arc::new(AtomicBool::new(false));
    app.hub.error = None;
    app.hub.authorization_qr = None;
    app.hub.screen = HubScreen::Connecting;
    let operation_id = app.hub.begin_operation();
    let cancelled = Arc::clone(&app.hub.cancelled);
    let qr_cancelled = Arc::clone(&app.hub.cancelled);
    let sender = app.hub.event_sender.clone();
    thread::spawn(move || {
        let progress_sender = sender.clone();
        let result = hub::connect_for_tui(
            client_id,
            redirect_uri,
            &cancelled,
            move |handoff_url, _redirect_uri| {
                let qr = terminal_qr(&handoff_url).map_err(|error| error.to_string());
                if qr.is_err() {
                    qr_cancelled.store(true, Ordering::Relaxed);
                }
                progress_sender
                    .send(HubEvent::AuthorizationReady { operation_id, qr })
                    .ok();
            },
        )
        .map_err(|error| error.to_string());
        sender.send(HubEvent::Connected(operation_id, result)).ok();
    });
}

fn start_hub_search(app: &mut App, page: usize) {
    let query = app.hub.input.value.trim().to_owned();
    if let Err(error) = crate::model_search::ModelSearch::parse(&query) {
        app.hub.error = Some(error.to_string());
        app.set_status(error.to_string(), true);
        return;
    }
    app.hub.query = query.clone();
    app.hub.error = None;
    app.hub.cancelled = Arc::new(AtomicBool::new(false));
    app.hub.screen = HubScreen::Searching;
    let operation_id = app.hub.begin_operation();
    let architecture = app.hub.architecture.clone();
    let cancelled = Arc::clone(&app.hub.cancelled);
    let sender = app.hub.event_sender.clone();
    thread::spawn(move || {
        let result = hub::search_for_tui(&query, page, &architecture, &cancelled)
            .map_err(|error| error.to_string());
        sender
            .send(HubEvent::SearchFinished(operation_id, result))
            .ok();
    });
}

fn change_hub_page(app: &mut App, direction: i8) {
    let Some(results) = &app.hub.results else {
        return;
    };
    let next = if direction < 0 {
        results.page.checked_sub(1)
    } else {
        let page = results.page + 1;
        (page <= results.total_pages).then_some(page)
    };
    if let Some(page) = next {
        app.hub.input.set(app.hub.query.clone());
        start_hub_search(app, page);
    } else {
        app.set_status("ALREADY AT HUB PAGE EDGE", false);
    }
}

fn start_hub_download(app: &mut App, models: &mut ModelDirectory) {
    let Some(item) = app.hub.selected_item() else {
        app.set_status("NO EXACT MODEL SELECTED", true);
        return;
    };
    let model_id = item.model_id;
    match hub::installed_model_path(model_id, models.root()) {
        Ok(Some(path)) => {
            models.select_path(&path);
            app.hub.screen = HubScreen::Closed;
            app.set_status("ALREADY DOWNLOADED · ENTER TO LOAD", false);
            return;
        }
        Ok(None) => {}
        Err(error) => {
            app.hub.error = Some(error.to_string());
            app.set_status(error.to_string(), true);
            return;
        }
    }
    let models_dir = models.root().to_path_buf();
    app.hub.error = None;
    app.hub.screen = HubScreen::Downloading;
    let operation_id = app.hub.begin_operation();
    let sender = app.hub.event_sender.clone();
    thread::spawn(move || {
        let result =
            hub::download_for_tui(model_id, &models_dir).map_err(|error| error.to_string());
        sender
            .send(HubEvent::DownloadFinished(operation_id, result))
            .ok();
    });
}

fn poll_hub(app: &mut App, models: &mut ModelDirectory) {
    loop {
        match app.hub.events.try_recv() {
            Ok(event) => {
                if event.operation_id() != app.hub.operation_id {
                    continue;
                }
                match event {
                    HubEvent::AuthorizationReady { qr, .. } => {
                        if app.hub.screen == HubScreen::Connecting {
                            match qr {
                                Ok(qr) => {
                                    app.hub.authorization_qr = Some(qr);
                                    app.set_status("SCAN QR WITH PHONE · ESC CANCELS", false);
                                }
                                Err(error) => {
                                    app.hub.begin_operation();
                                    app.hub.screen = HubScreen::ClientId;
                                    app.hub.error = Some(error.clone());
                                    app.set_status(error, true);
                                }
                            }
                        }
                    }
                    HubEvent::Connected(_, result) => match result {
                        Ok(()) => {
                            if app.hub.screen == HubScreen::Connecting {
                                app.hub.input.set(app.hub.query.clone());
                                app.hub.screen = HubScreen::Query;
                                app.set_status("TONE3000 CONNECTED · ENTER A SEARCH", false);
                            }
                        }
                        Err(error) => {
                            if app.hub.screen == HubScreen::Connecting {
                                app.hub.screen = HubScreen::ClientId;
                            }
                            app.hub.error = Some(error.clone());
                            app.set_status(error, true);
                        }
                    },
                    HubEvent::SearchFinished(_, result) => match result {
                        Ok(results) => {
                            let count = results.items.len();
                            app.hub.results = Some(results);
                            app.hub.selected = 0;
                            app.hub.screen = HubScreen::Results;
                            app.set_status(format!("HUB · {count} EXACT MATCH(ES)"), false);
                        }
                        Err(error) => {
                            app.hub.input.set(app.hub.query.clone());
                            app.hub.screen = HubScreen::Query;
                            app.hub.error = Some(error.clone());
                            app.set_status(error, true);
                        }
                    },
                    HubEvent::DownloadFinished(_, result) => match result {
                        Ok(path) => match models.refresh() {
                            Ok(()) => {
                                models.select_path(&path);
                                app.hub.screen = HubScreen::Closed;
                                app.set_status("DOWNLOADED ONE MODEL · ENTER TO LOAD", false);
                            }
                            Err(error) => {
                                app.hub.screen = HubScreen::Results;
                                app.set_status(
                                    format!("DOWNLOADED; REFRESH FAILED · {error}"),
                                    true,
                                );
                            }
                        },
                        Err(error) => {
                            app.hub.screen = HubScreen::Detail;
                            app.hub.error = Some(error.clone());
                            app.set_status(error, true);
                        }
                    },
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                app.set_status("HUB WORKER CHANNEL STOPPED", true);
                break;
            }
        }
    }
}

fn show_external_text(terminal: &mut Terminal<CrosstermBackend<Stdout>>, text: &str) -> Result<()> {
    disable_raw_mode().context("pause raw mode for external text")?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("show external text outside TUI")?;
    println!("\n{text}\n");
    print!("> ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("wait before returning to TUI")?;
    enable_raw_mode().context("restore terminal raw mode")?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("restore TUI after external text")?;
    terminal.clear().ok();
    Ok(())
}

fn handle_mouse(
    mouse: MouseEvent,
    app: &mut App,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    midi: &mut MidiController,
) {
    if app.hub.is_open() {
        if matches!(app.hub.screen, HubScreen::Results | HubScreen::Detail) {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.hub.select_relative(-1),
                MouseEventKind::ScrollDown => app.hub.select_relative(1),
                _ => {}
            }
        }
        return;
    }
    if app.help {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.help = false;
        }
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => browse_or_select_mic(app, audio, models, midi, -1),
        MouseEventKind::ScrollDown => browse_or_select_mic(app, audio, models, midi, 1),
        MouseEventKind::Down(MouseButton::Left) => match mouse.row {
            0 => focus_chain_relative(app, if mouse.column < 20 { -1 } else { 1 }),
            5 => replace_focused(app, audio, models, midi),
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
            9 => match mouse.column {
                0..=9 => focus_chain_relative(app, -1),
                10..=19 if app.mic_focus => select_mic_variant(app, audio, models, midi, -1),
                20..=29 if app.mic_focus => select_mic_variant(app, audio, models, midi, 1),
                10..=19 => add_selected(app, audio, models, midi),
                20..=29 => remove_focused(app, audio, models, midi),
                _ => focus_chain_relative(app, 1),
            },
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
        MidiAction::Select(direction) => browse_or_select_mic(app, audio, models, midi, direction),
        MidiAction::Load => replace_focused(app, audio, models, midi),
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

fn handle_hub_midi(action: MidiAction, app: &mut App) {
    match action {
        MidiAction::Select(direction)
            if matches!(app.hub.screen, HubScreen::Results | HubScreen::Detail) =>
        {
            app.hub.select_relative(direction);
        }
        MidiAction::Parameter(parameter) => app.focus = parameter,
        MidiAction::Learned { parameter, .. } => app.focus = parameter,
        MidiAction::OversamplingOff => app.set_status("OVERSAMPLING OFF IN V1", false),
        MidiAction::Select(_) | MidiAction::Load => {}
    }
}

fn selected_path(models: &ModelDirectory) -> Option<PathBuf> {
    models.selected().map(|entry| entry.path.clone())
}

fn replace_focused(
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
) {
    if app.mic_focus {
        app.set_status("UP/DOWN SELECTS THIS CAB'S MIC VARIANT", false);
        return;
    }
    let Some(path) = selected_path(models) else {
        app.set_status(
            format!("NO .NAM/.WAV FILES IN {}", models.root().display()),
            true,
        );
        return;
    };
    let mut paths = chain_paths(app);
    let focus = app.chain_focus.min(paths.len().saturating_sub(1));
    if paths.is_empty() {
        paths.push(path);
    } else {
        paths[focus] = path;
    }
    publish_chain(paths, (focus, false), "REPLACED", app, audio, models, midi);
}

fn add_selected(
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
) {
    if app.mic_focus {
        app.set_status("MIC IS VIRTUAL · LEFT TO ADD A DSP SLOT", false);
        return;
    }
    if app.chain.len() >= MAX_CHAIN_MODELS {
        app.set_status(format!("CHAIN LIMIT IS {MAX_CHAIN_MODELS} SLOTS"), true);
        return;
    }
    let Some(path) = selected_path(models) else {
        app.set_status(
            format!("NO .NAM/.WAV FILES IN {}", models.root().display()),
            true,
        );
        return;
    };
    let mut paths = chain_paths(app);
    let focus = if paths.is_empty() {
        paths.push(path);
        0
    } else {
        let index = app.chain_focus.min(paths.len() - 1) + 1;
        paths.insert(index, path);
        index
    };
    publish_chain(paths, (focus, false), "ADDED", app, audio, models, midi);
}

fn remove_focused(
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
) {
    if app.mic_focus {
        app.set_status("MIC IS BAKED INTO CAB · DELETE CAB SLOT", false);
        return;
    }
    if app.chain.is_empty() {
        app.set_status("CHAIN IS ALREADY EMPTY", true);
        return;
    }
    let mut paths = chain_paths(app);
    paths.remove(app.chain_focus);
    let focus = app.chain_focus.min(paths.len().saturating_sub(1));
    publish_chain(paths, (focus, false), "REMOVED", app, audio, models, midi);
}

fn move_focused(
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
    direction: i8,
) {
    if app.mic_focus {
        app.set_status("MIC MOVES WITH ITS CABINET", false);
        return;
    }
    if app.chain.len() < 2 {
        app.set_status("ADD ANOTHER SLOT BEFORE REORDERING", true);
        return;
    }
    let target = if direction < 0 {
        app.chain_focus.checked_sub(1)
    } else {
        let next = app.chain_focus + 1;
        (next < app.chain.len()).then_some(next)
    };
    let Some(target) = target else {
        app.set_status("SLOT IS ALREADY AT CHAIN EDGE", false);
        return;
    };
    let mut paths = chain_paths(app);
    paths.swap(app.chain_focus, target);
    publish_chain(paths, (target, false), "MOVED", app, audio, models, midi);
}

fn reload_chain(
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
) {
    if app.chain.is_empty() {
        app.set_status("NO CHAIN TO RELOAD", true);
        return;
    }
    publish_chain(
        chain_paths(app),
        (app.chain_focus, app.mic_focus),
        "RELOADED",
        app,
        audio,
        models,
        midi,
    );
}

fn focus_chain_relative(app: &mut App, direction: i8) {
    if app.chain.is_empty() {
        app.chain_focus = 0;
        app.mic_focus = false;
        return;
    }
    let targets = focus_targets(&app.chain);
    let current = targets
        .iter()
        .position(|target| *target == (app.chain_focus, app.mic_focus))
        .unwrap_or(0);
    let next = relative_index(current, targets.len(), direction);
    (app.chain_focus, app.mic_focus) = targets[next];
}

fn focus_targets(chain: &[ModelMetadata]) -> Vec<(usize, bool)> {
    let mut targets = Vec::with_capacity(chain.len() * 2);
    for (index, metadata) in chain.iter().enumerate() {
        targets.push((index, false));
        if is_cabinet_path(&metadata.path) {
            targets.push((index, true));
        }
    }
    if targets.is_empty() {
        targets.push((0, false));
    }
    targets
}

fn is_cabinet_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
}

fn relative_index(current: usize, len: usize, direction: i8) -> usize {
    debug_assert!(len > 0);
    if direction < 0 {
        current.checked_sub(1).unwrap_or(len - 1)
    } else {
        (current + 1) % len
    }
}

fn browse_or_select_mic(
    app: &mut App,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    midi: &mut MidiController,
    direction: i8,
) {
    if app.mic_focus {
        select_mic_variant(app, audio, models, midi, direction);
    } else {
        models.select_relative(direction);
    }
}

fn select_mic_variant(
    app: &mut App,
    audio: &mut AudioClient,
    models: &mut ModelDirectory,
    _midi: &mut MidiController,
    direction: i8,
) {
    let Some(active) = app.chain.get(app.chain_focus) else {
        app.mic_focus = false;
        return;
    };
    let Some(variants) = app.cabinet_variants.get(app.chain_focus) else {
        app.set_status("CABINET PACK IS NOT LOADED", true);
        return;
    };
    if variants.len() < 2 {
        app.set_status("NO DOCUMENTED MIC VARIANTS FOR THIS CAB", false);
        return;
    }
    let current = variants
        .iter()
        .position(|variant| variant.path == active.path)
        .unwrap_or(0);
    let next = relative_index(current, variants.len(), direction);
    let selected = variants[next].clone();
    if let Err(error) = audio.queue_cabinet_variant(app.chain_focus, next) {
        app.set_status(error.to_string(), true);
        return;
    }
    app.chain[app.chain_focus] = selected.clone();
    models.select_path(&selected.path);
    app.set_status("MIC VARIANT · LIVE SWITCH", false);
}

fn chain_paths(app: &App) -> Vec<PathBuf> {
    app.chain
        .iter()
        .map(|metadata| metadata.path.clone())
        .collect()
}

fn publish_chain(
    paths: Vec<PathBuf>,
    focus: (usize, bool),
    verb: &str,
    app: &mut App,
    audio: &mut AudioClient,
    models: &ModelDirectory,
    midi: &mut MidiController,
) -> bool {
    let (focus, mic_focus) = focus;
    let cabinet_variants = paths
        .iter()
        .map(|path| {
            let variants = models.ir_variants(path);
            if variants.is_empty() && is_cabinet_path(path) {
                vec![path.clone()]
            } else {
                variants
                    .into_iter()
                    .map(|entry| entry.path.clone())
                    .collect()
            }
        })
        .collect::<Vec<_>>();
    match NamChain::load_with_cabinet_variants(
        &paths,
        &cabinet_variants,
        crate::audio::SAMPLE_RATE,
        audio.period_frames(),
    ) {
        Ok(chain) => {
            let metadata = chain.metadata();
            let variant_metadata = chain.cabinet_variant_metadata();
            if let Err(error) = audio.queue_chain(chain) {
                app.set_status(error.to_string(), true);
                return false;
            }
            midi.arm_pickup(&app.params);
            app.chain = metadata;
            app.cabinet_variants = variant_metadata;
            app.chain_focus = focus.min(app.chain.len().saturating_sub(1));
            app.mic_focus = mic_focus
                && app
                    .chain
                    .get(app.chain_focus)
                    .is_some_and(|metadata| is_cabinet_path(&metadata.path));
            app.set_status(format!("{verb} · {} SLOT(S)", app.chain.len()), false);
            true
        }
        Err(error) => {
            app.set_status(error.to_string(), true);
            false
        }
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
    let telemetry = app.telemetry.snapshot();
    if app.hub.is_open() {
        draw_hub(frame, area, app);
        draw_status(frame, row(area, 12), app, midi, telemetry, period, cpu);
        return;
    }
    draw_chain_header(frame, area, app, models);
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
    draw_chain_controls(frame, row(area, 9), app);
    draw_hints(frame, area, app);
    draw_status(frame, row(area, 12), app, midi, telemetry, period, cpu);
    if app.help {
        draw_help(frame, area);
    }
}

fn draw_hub(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, app: &App) {
    match app.hub.screen {
        HubScreen::Closed => {}
        HubScreen::ClientId => {
            draw_text(frame, row(area, 0), "HUB · CONNECT TONE3000", Color::White);
            draw_text(
                frame,
                row(area, 1),
                "INSTALLATION PUBLIC CLIENT ID",
                Color::Gray,
            );
            draw_hub_input(frame, Rect::new(area.x, area.y + 2, 40, 5), &app.hub.input);
            draw_text(
                frame,
                row(area, 7),
                "PUBLISHABLE ID ONLY · NEVER SECRET KEY",
                Color::LightYellow,
            );
            if let Some(error) = &app.hub.error {
                draw_hub_error(frame, Rect::new(area.x, area.y + 8, 40, 2), error);
            } else {
                draw_text(
                    frame,
                    row(area, 8),
                    "CALLBACK USES THIS PI'S CURRENT WIFI IP",
                    Color::Gray,
                );
            }
            draw_text(
                frame,
                row(area, 10),
                "ENTER CONNECT · ESC BACK",
                Color::White,
            );
            draw_text(
                frame,
                row(area, 11),
                "TOKEN STORED OWNER-ONLY ON THIS PI",
                Color::Gray,
            );
        }
        HubScreen::Connecting => {
            if let Some(qr) = &app.hub.authorization_qr {
                let height = qr.len() as u16;
                let width = qr.first().map_or(0, |line| line.chars().count()) as u16;
                let qr_area = Rect::new(
                    area.x + area.width.saturating_sub(width) / 2,
                    area.y + 11_u16.saturating_sub(height) / 2,
                    width.min(area.width),
                    height.min(11),
                );
                frame.render_widget(
                    Paragraph::new(qr.join("\n"))
                        .style(Style::default().fg(Color::Black).bg(Color::White)),
                    qr_area,
                );
                draw_text(
                    frame,
                    row(area, 11),
                    "SCAN QR WITH PHONE · ESC CANCEL",
                    Color::White,
                );
            } else {
                draw_text(
                    frame,
                    row(area, 0),
                    "HUB · AUTHORIZE TONE3000",
                    Color::White,
                );
                draw_text(
                    frame,
                    row(area, 3),
                    "BUILDING SECURE LOGIN QR …",
                    Color::LightYellow,
                );
                draw_text(
                    frame,
                    row(area, 11),
                    "ESC CANCELS AND KEEPS CLIENT ID",
                    Color::White,
                );
            }
        }
        HubScreen::Query => {
            draw_text(
                frame,
                row(area, 0),
                &format!("SMART HUB SEARCH · NAM{}", app.hub.architecture),
                Color::White,
            );
            draw_text(
                frame,
                row(area, 1),
                "EXACT DOCUMENTED CAPTURE SETTINGS",
                Color::Gray,
            );
            draw_hub_input(frame, Rect::new(area.x, area.y + 2, 40, 6), &app.hub.input);
            if let Some(error) = &app.hub.error {
                draw_hub_error(frame, Rect::new(area.x, area.y + 8, 40, 2), error);
            } else {
                draw_text(
                    frame,
                    row(area, 8),
                    "EX: JCM BASS 3-4 MID 6+ HIGH 2-3",
                    Color::Gray,
                );
                draw_text(frame, row(area, 9), "TAB ARCH: 2 / 1 / CUSTOM", Color::Gray);
            }
            draw_text(frame, row(area, 10), "ENTER SEARCH METADATA", Color::White);
            draw_text(
                frame,
                row(area, 11),
                "ESC BACK · INPUT IS KEPT ON ERRORS",
                Color::Gray,
            );
        }
        HubScreen::Searching => {
            draw_text(
                frame,
                row(area, 0),
                "HUB · SEARCHING METADATA",
                Color::White,
            );
            draw_text(frame, row(area, 2), &app.hub.query, Color::LightYellow);
            draw_text(
                frame,
                row(area, 5),
                "NO MODEL FILES ARE BEING DOWNLOADED",
                Color::Gray,
            );
            draw_text(
                frame,
                row(area, 7),
                "CHECKING EACH CAPTURE'S OWN SETTINGS",
                Color::Gray,
            );
            draw_text(
                frame,
                row(area, 10),
                "PLEASE WAIT · AUDIO STAYS LIVE",
                Color::White,
            );
            draw_text(
                frame,
                row(area, 11),
                "ESC CANCELS AFTER CURRENT REQUEST",
                Color::Gray,
            );
        }
        HubScreen::Results => draw_hub_result(frame, area, app, false),
        HubScreen::Detail => draw_hub_result(frame, area, app, true),
        HubScreen::Downloading => {
            draw_text(
                frame,
                row(area, 0),
                "HUB · DOWNLOADING ONE MODEL",
                Color::White,
            );
            if let Some(item) = app.hub.selected_item() {
                draw_text(frame, row(area, 2), &item.tone_title, Color::LightYellow);
                draw_text(frame, row(area, 3), &item.model_name, Color::White);
                draw_text(frame, row(area, 4), &item.settings, Color::Gray);
                draw_text(
                    frame,
                    row(area, 6),
                    &format!("EXACT ID t3k:model:{}", item.model_id),
                    Color::Gray,
                );
            }
            draw_text(
                frame,
                row(area, 8),
                "VALIDATING NAM BEFORE PUBLISHING",
                Color::Gray,
            );
            draw_text(
                frame,
                row(area, 10),
                "AUDIO STAYS ON CURRENT CHAIN",
                Color::White,
            );
            draw_text(
                frame,
                row(area, 11),
                "DOWNLOAD FINISHES ATOMICALLY",
                Color::Gray,
            );
        }
    }
}

fn draw_hub_input(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, input: &TextInput) {
    let mut value = input.value.clone();
    value.insert(input.cursor, '▏');
    frame.render_widget(
        Paragraph::new(value)
            .block(
                Block::default()
                    .title(" TYPE OR PASTE ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightYellow)),
            )
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn terminal_qr(value: &str) -> Result<Vec<String>> {
    const QUIET_ZONE: usize = 4;
    const QR_ROWS: usize = 11;

    let code = QrCode::with_error_correction_level(value.as_bytes(), EcLevel::L)
        .context("build authorization QR")?;
    let module_width = code.width();
    let total = module_width + QUIET_ZONE * 2;
    let columns = total.div_ceil(2);
    let rows = total.div_ceil(4);
    if columns > usize::from(WIDTH) || rows > QR_ROWS {
        anyhow::bail!("authorization QR does not fit the 40x13 display");
    }

    let mut output = Vec::with_capacity(rows);
    for cell_y in 0..rows {
        let mut line = String::with_capacity(columns * 3);
        for cell_x in 0..columns {
            let mut dots = 0_u32;
            for dot_y in 0..4 {
                for dot_x in 0..2 {
                    let x = cell_x * 2 + dot_x;
                    let y = cell_y * 4 + dot_y;
                    let in_code = x >= QUIET_ZONE
                        && y >= QUIET_ZONE
                        && x < QUIET_ZONE + module_width
                        && y < QUIET_ZONE + module_width;
                    if in_code && code[(x - QUIET_ZONE, y - QUIET_ZONE)] == QrColor::Dark {
                        dots |= braille_dot(dot_x, dot_y);
                    }
                }
            }
            line.push(char::from_u32(0x2800 + dots).expect("valid Braille code point"));
        }
        output.push(line);
    }
    Ok(output)
}

fn braille_dot(x: usize, y: usize) -> u32 {
    match (x, y) {
        (0, 0) => 1 << 0,
        (0, 1) => 1 << 1,
        (0, 2) => 1 << 2,
        (1, 0) => 1 << 3,
        (1, 1) => 1 << 4,
        (1, 2) => 1 << 5,
        (0, 3) => 1 << 6,
        (1, 3) => 1 << 7,
        _ => 0,
    }
}

fn draw_hub_result(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    app: &App,
    detail: bool,
) {
    let Some(results) = &app.hub.results else {
        draw_text(
            frame,
            row(area, 0),
            "HUB · NO RESULT STATE",
            Color::LightYellow,
        );
        return;
    };
    if results.items.is_empty() {
        draw_text(frame, row(area, 0), "HUB · 0 EXACT MATCHES", Color::White);
        draw_text(
            frame,
            row(area, 2),
            &format!("{} MODEL RECORDS SCANNED", results.models_scanned),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 3),
            &format!(
                "{} MISSING REQUESTED METADATA",
                results.models_missing_settings
            ),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 5),
            "CONSTRAINTS WERE NOT RELAXED",
            Color::LightYellow,
        );
        draw_text(
            frame,
            row(area, 8),
            &format!(
                "TONE PAGE {}/{} · {} TOTAL",
                results.page, results.total_pages, results.tones_total
            ),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 10),
            "LEFT/RIGHT PAGE · N NEW SEARCH",
            Color::White,
        );
        draw_text(frame, row(area, 11), "ESC BACK TO AMP", Color::Gray);
        return;
    }
    let Some(item) = app.hub.selected_item() else {
        return;
    };
    draw_text(
        frame,
        row(area, 0),
        &format!(
            "HUB {} {}/{} · PAGE {}/{}",
            if detail { "DETAIL" } else { "RESULT" },
            app.hub.selected + 1,
            results.items.len(),
            results.page,
            results.total_pages
        ),
        Color::White,
    );
    draw_text(frame, row(area, 1), &item.tone_title, Color::LightYellow);
    draw_text(frame, row(area, 2), &item.model_name, Color::White);
    draw_text(frame, row(area, 3), &item.settings, Color::LightYellow);
    if detail {
        draw_text(
            frame,
            row(area, 4),
            &format!("MAKE {}", item.make),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 5),
            &format!("MATCH {}", item.matched),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 6),
            &format!("FROM {}", item.setting_sources),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 7),
            &format!(
                "@{} · {} · {} · NAM{}",
                item.creator, item.license, item.size, item.architecture
            ),
            Color::Gray,
        );
        if let Some(error) = &app.hub.error {
            draw_hub_error(frame, Rect::new(area.x, area.y + 8, 40, 2), error);
        } else {
            draw_text(frame, row(area, 8), &item.source_url, Color::Gray);
            draw_text(
                frame,
                row(area, 9),
                &format!("EXACT ID t3k:model:{}", item.model_id),
                Color::Gray,
            );
        }
        draw_text(
            frame,
            row(area, 10),
            "D DOWNLOAD ONE · U FULL SOURCE",
            Color::White,
        );
        draw_text(
            frame,
            row(area, 11),
            "UP/DN RESULT · ENTER/ESC LIST",
            Color::Gray,
        );
    } else {
        draw_text(
            frame,
            row(area, 4),
            &format!("MATCH {}", item.matched),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 5),
            &format!("FROM {}", item.setting_sources),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 6),
            &format!("@{} · {}", item.creator, item.license),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 7),
            &format!(
                "EXACT ID t3k:model:{} · NAM{}",
                item.model_id, item.architecture
            ),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 9),
            "UP/DN RESULT · LEFT/RIGHT PAGE",
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 10),
            "ENTER DETAILS · D DOWNLOAD ONE",
            Color::White,
        );
        draw_text(
            frame,
            row(area, 11),
            "N NEW SEARCH · ESC BACK TO AMP",
            Color::Gray,
        );
    }
}

fn draw_hub_error(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, error: &str) {
    frame.render_widget(
        Paragraph::new(format!("ERROR · {error}"))
            .style(Style::default().fg(Color::LightYellow))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_chain_header(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    app: &App,
    models: &ModelDirectory,
) {
    let Some(metadata) = app.chain.get(app.chain_focus) else {
        draw_text(
            frame,
            row(area, 0),
            &format!("CHAIN 0/{MAX_CHAIN_MODELS} · EMPTY"),
            Color::White,
        );
        draw_text(
            frame,
            row(area, 1),
            "SLOT -- · SELECT A MODEL, THEN ADD",
            Color::Gray,
        );
        draw_text(frame, row(area, 2), "META --", Color::Gray);
        return;
    };
    if app.mic_focus {
        draw_mic_header(frame, area, app, metadata, models);
        return;
    }
    let previous = app
        .chain_focus
        .checked_sub(1)
        .and_then(|index| app.chain.get(index))
        .map(|item| fit_cells(&item.name, 7))
        .unwrap_or_else(|| "--".to_owned());
    let next = if is_cabinet_path(&metadata.path) {
        "MIC*".to_owned()
    } else {
        app.chain
            .get(app.chain_focus + 1)
            .map(|item| fit_cells(&item.name, 7))
            .unwrap_or_else(|| "--".to_owned())
    };
    draw_text(
        frame,
        row(area, 0),
        &format!(
            "CHAIN {}/{} <{} [{}] {}>",
            app.chain_focus + 1,
            app.chain.len(),
            previous,
            fit_cells(&metadata.name, 9),
            next
        ),
        Color::White,
    );
    if let Some(profile) = models.ir_profile(&metadata.path) {
        draw_text(
            frame,
            row(area, 1),
            &format!("CAB {} · SPK {}", profile.cabinet, profile.speaker),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 2),
            &format!("MIC {} · VOICE {} >", profile.microphone, profile.variant),
            Color::Gray,
        );
    } else {
        draw_text(
            frame,
            row(area, 1),
            &format!(
                "SLOT {} {} [{}]",
                app.chain_focus + 1,
                metadata.name,
                metadata.architecture
            ),
            Color::Gray,
        );
        draw_text(
            frame,
            row(area, 2),
            &format!(
                "v{} SR{} {} {} IN{} OUT{}",
                metadata.version,
                metadata.expected_sample_rate.unwrap_or(48_000),
                human_bytes(metadata.file_bytes),
                metadata.detail,
                level_text(metadata.input_level_dbu),
                level_text(metadata.output_level_dbu)
            ),
            Color::Gray,
        );
    }
}

fn draw_mic_header(
    frame: &mut Frame<CrosstermBackend<Stdout>>,
    area: Rect,
    app: &App,
    metadata: &ModelMetadata,
    models: &ModelDirectory,
) {
    let variants = app
        .cabinet_variants
        .get(app.chain_focus)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let selected = variants
        .iter()
        .position(|variant| variant.path == metadata.path)
        .map(|index| index + 1)
        .unwrap_or(1);
    if let Some(profile) = models.ir_profile(&metadata.path) {
        draw_text(
            frame,
            row(area, 0),
            &format!(
                "MIC* {}/{} · {}",
                selected,
                variants.len().max(1),
                profile.cabinet
            ),
            Color::White,
        );
        draw_text(
            frame,
            row(area, 1),
            &format!("MIC {} · POS {}", profile.microphone, profile.position),
            Color::LightYellow,
        );
        draw_text(
            frame,
            row(area, 2),
            &format!("VOICE {} · BAKED INTO CAB IR", profile.variant),
            Color::Gray,
        );
    } else {
        draw_text(
            frame,
            row(area, 0),
            "MIC* 1/1 · USER CABINET IR",
            Color::White,
        );
        draw_text(
            frame,
            row(area, 1),
            "MIC UNKNOWN · POSITION UNKNOWN",
            Color::LightYellow,
        );
        draw_text(
            frame,
            row(area, 2),
            "BAKED INTO IR · ADD PACK METADATA",
            Color::Gray,
        );
    }
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

fn draw_hints(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, app: &App) {
    draw_text(
        frame,
        row(area, 10),
        "H HUB · PEDAL > AMP > CAB > MIC*",
        Color::White,
    );
    if app.mic_focus {
        draw_halves(frame, row(area, 11), "UP/DN MIC VOICE", "LEFT CAB · ? HELP");
    } else {
        draw_halves(frame, row(area, 11), "UP/DN BROWSE", "ENTER LOAD · ? HELP");
    }
}

fn draw_chain_controls(frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect, app: &App) {
    let labels = if app.mic_focus {
        ["< CAB", "▲ MIC", "▼ MIC", "NEXT >"]
    } else {
        ["< SLOT", "+ ADD", "- DEL", "SLOT >"]
    };
    for (index, label) in labels.into_iter().enumerate() {
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::White)),
            Rect::new(area.x + index as u16 * 10, area.y, 10, 1),
        );
    }
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
        let model = if app.mic_focus {
            "MIC*".to_owned()
        } else {
            app.chain
                .get(app.chain_focus)
                .map(|metadata| fit_cells(&metadata.name, 7))
                .unwrap_or_else(|| "--".to_owned())
        };
        let midi = fit_cells(midi.device_label(), 6);
        let temperature = app
            .temperature_c
            .map(|value| format!("T{value:.0}C"))
            .unwrap_or_else(|| "T--C".to_owned());
        (
            format!(
                "S{} {model} X{} P{period} M:{midi} C{cpu:.0}% {temperature}",
                telemetry.chain_len, telemetry.xruns
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
    let popup = Rect::new(area.x + 1, area.y + 1, 38, 10);
    frame.render_widget(Clear, popup);
    let help = [
        "UP/DOWN browse · LEFT/RIGHT stage",
        "On MIC*: UP/DOWN changes cab IR",
        "A add · ENTER replace · D delete",
        "[ / ] move slot · R reload chain",
        "I/O +0.5 dB · Shift I/O +3 dB",
        "H smart hub search/download one model",
        "MIC* is virtual; one cabinet DSP slot",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(path: &str) -> ModelMetadata {
        ModelMetadata {
            path: PathBuf::from(path),
            name: path.to_owned(),
            architecture: String::new(),
            version: String::new(),
            file_bytes: 0,
            detail: String::new(),
            expected_sample_rate: None,
            input_level_dbu: None,
            output_level_dbu: None,
        }
    }

    #[test]
    fn cabinet_adds_one_virtual_microphone_focus_target() {
        let chain = [
            metadata("pedal.nam"),
            metadata("cab.wav"),
            metadata("delay.nam"),
        ];
        assert_eq!(
            focus_targets(&chain),
            vec![(0, false), (1, false), (1, true), (2, false)]
        );
    }

    #[test]
    fn virtual_selector_wraps_both_directions() {
        assert_eq!(relative_index(0, 3, -1), 2);
        assert_eq!(relative_index(2, 3, 1), 0);
        assert_eq!(relative_index(1, 3, 1), 2);
    }

    #[test]
    fn hub_text_input_edits_unicode_without_losing_the_query() {
        let mut input = TextInput::default();
        input.insert("mid 6+ treble ≤3");
        input.move_left();
        input.backspace();
        input.insert("≤");
        input.move_right();
        input.backspace();
        input.insert("2");
        assert_eq!(input.value, "mid 6+ treble ≤2");
    }

    #[test]
    fn hub_text_input_filters_terminal_controls() {
        let mut input = TextInput::default();
        input.insert("JCM\n\u{1b}[31m bass 3");
        assert_eq!(input.value, "JCM[31m bass 3");
    }

    #[test]
    fn authorization_qr_fits_the_fixed_terminal() {
        let qr = terminal_qr("http://192.168.100.200:43900/").unwrap();
        assert!(qr.len() <= 11);
        assert!(qr.iter().all(|line| line.chars().count() <= 40));
        assert!(qr
            .iter()
            .flat_map(|line| line.chars())
            .any(|character| character != '\u{2800}'));
    }

    #[test]
    fn braille_qr_dot_positions_are_distinct() {
        let mut combined = 0_u32;
        for y in 0..4 {
            for x in 0..2 {
                let dot = braille_dot(x, y);
                assert_eq!(dot.count_ones(), 1);
                assert_eq!(combined & dot, 0);
                combined |= dot;
            }
        }
        assert_eq!(combined, 0xff);
    }
}
