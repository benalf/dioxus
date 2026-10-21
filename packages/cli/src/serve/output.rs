use crate::Result;
use crate::{
    BuildId, BuildStage, BuilderUpdate, BundleFormat, TraceContent, TraceMsg, TraceSrc,
    serve::{ServeUpdate, WebServer},
};
use anyhow::{Context, anyhow, bail};
use cargo_metadata::diagnostic::Diagnostic;
use crossterm::{
    ExecutableCommand,
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, Event,
        EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    },
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    TerminalOptions, Viewport,
    prelude::*,
    widgets::{Block, BorderType, Borders, LineGauge, Paragraph, Wrap},
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{self, stdout},
    rc::Rc,
    time::Duration,
};
use tracing::Level;

use super::{AppServer, HotReloadMode};

const TICK_RATE_MS: u64 = 100;
const VIEWPORT_MAX_WIDTH: u16 = 92;
const VIEWPORT_HEIGHT_SMALL: u16 = 5;
const VIEWPORT_HEIGHT_BIG: u16 = 14;

/// The TUI that drives the console output.
///
/// We try not to store too much state about the world here, just the state about the tui itself.
/// This is to prevent out-of-sync issues with the rest of the build engine and to use the components
/// of the serve engine as the source of truth.
///
/// Please please, do not add state here that does not belong here. We should only be storing state
/// here that is used to change how we display *other* state. Things like throbbers, modals, etc.
pub struct Output {
    term: Rc<RefCell<Option<Terminal<CrosstermBackend<io::Stdout>>>>>,
    events: Option<EventStream>,

    // A list of all messages from build, dev, app, and more.
    more_modal_open: bool,
    interactive: bool,

    // Whether to show verbose logs or not
    // We automatically hide "debug" logs if verbose is false (only showing "info" / "warn" / "error")
    verbose: bool,
    trace: bool,

    // Pending logs
    pending_logs: VecDeque<TraceMsg>,

    dx_version: String,
    tick_animation: bool,

    tick_interval: tokio::time::Interval,

    // ! needs to be wrapped in an &mut since `render stateful widget` requires &mut... but our
    // "render" method only borrows &self (for no particular reason at all...)
    throbber: RefCell<throbber_widgets_tui::ThrobberState>,
}

#[derive(Clone, Copy)]
struct RenderState<'a> {
    runner: &'a AppServer,
    server: &'a WebServer,
}

impl Output {
    pub(crate) async fn start(interactive: bool) -> crate::Result<Self> {
        let mut output = Self {
            interactive,
            term: Rc::new(RefCell::new(None)),
            dx_version: format!(
                "{}-{}",
                env!("CARGO_PKG_VERSION"),
                crate::dx_build_info::GIT_COMMIT_HASH_SHORT.unwrap_or("main")
            ),
            events: None,
            more_modal_open: false,
            pending_logs: VecDeque::new(),
            throbber: RefCell::new(throbber_widgets_tui::ThrobberState::default()),
            trace: crate::logging::VERBOSITY.get().unwrap().trace,
            verbose: crate::logging::VERBOSITY.get().unwrap().verbose,
            tick_animation: false,
            tick_interval: {
                let mut interval = tokio::time::interval(Duration::from_millis(TICK_RATE_MS));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval
            },
        };

        output.startup()?;

        Ok(output)
    }

    /// Call the startup functions that might mess with the terminal settings.
    /// This is meant to be paired with "shutdown" to restore the terminal to its original state.
    fn startup(&mut self) -> Result<()> {
        if self.interactive {
            // Check if writing the terminal is going to block infinitely.
            // If it does, we should disable interactive mode. This ensures we work with programs like `bg`
            // which suspend the process and cause us to block when writing output.
            if Self::enable_raw_mode().is_err() {
                self.term.take();
                self.interactive = false;
                return Ok(());
            }

            self.term.replace(
                Terminal::with_options(
                    CrosstermBackend::new(stdout()),
                    TerminalOptions {
                        viewport: Viewport::Inline(VIEWPORT_HEIGHT_SMALL),
                    },
                )
                .ok(),
            );

            // Initialize the event stream here - this is optional because an EvenStream in a non-interactive
            // terminal will cause a panic instead of simply doing nothing.
            // https://github.com/crossterm-rs/crossterm/issues/659
            self.events = Some(EventStream::new());
        }

        Ok(())
    }

    /// Enable raw mode, but don't let it block forever.
    ///
    /// This lets us check if writing to tty is going to block forever and then recover, allowing
    /// interoperability with programs like `bg`.
    fn enable_raw_mode() -> Result<()> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            // Ignore SIGTSTP, SIGTTIN, and SIGTTOU
            _ = signal(SignalKind::from_raw(20))?; // SIGTSTP
            _ = signal(SignalKind::from_raw(21))?; // SIGTTIN
            _ = signal(SignalKind::from_raw(22))?; // SIGTTOU
        }

        use std::io::IsTerminal;

        if !stdout().is_terminal() {
            bail!("Cannot enable raw mode on a non-terminal output");
        }

        enable_raw_mode()?;
        stdout()
            .execute(Hide)?
            .execute(EnableFocusChange)?
            .execute(EnableBracketedPaste)?;

        Ok(())
    }

    pub(crate) fn remote_shutdown(interactive: bool) -> Result<()> {
        if interactive && crossterm::terminal::is_raw_mode_enabled().unwrap_or(true) {
            stdout()
                .execute(Show)?
                .execute(DisableFocusChange)?
                .execute(DisableBracketedPaste)?;
            disable_raw_mode()?;

            // print a line to force the cursor down (no tearing)
            println!();
        }

        Ok(())
    }

    pub(crate) async fn wait(&mut self) -> ServeUpdate {
        use futures_util::StreamExt;
        use futures_util::future::OptionFuture;

        if !self.interactive {
            return std::future::pending().await;
        }

        // Wait for the next user event or animation tick
        loop {
            let next = OptionFuture::from(self.events.as_mut().map(|f| f.next()));
            let event = tokio::select! {
                biased; // Always choose the event over the animation tick to not lose the event
                Some(Some(Ok(event))) = next => event,
                _ = self.tick_interval.tick(), if self.tick_animation => {
                    self.throbber.borrow_mut().calc_next();
                    return ServeUpdate::Redraw
                },
                else => futures_util::future::pending().await
            };

            match self.handle_input(event) {
                Ok(Some(update)) => return update,
                Err(ee) => {
                    return ServeUpdate::Exit {
                        error: Some(anyhow::anyhow!(ee)),
                    };
                }
                Ok(None) => {}
            }
        }
    }

    /// Handle an input event, returning `true` if the event should cause the program to restart.
    fn handle_input(&mut self, input: Event) -> Result<Option<ServeUpdate>> {
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_keypress(key),
            _ => Ok(Some(ServeUpdate::Redraw)),
        }
    }

    fn handle_keypress(&mut self, key: KeyEvent) -> Result<Option<ServeUpdate>> {
        // Some dev helpers for testing panic propagation and error handling. Remove this eventually.
        if cfg!(debug_assertions) && std::env::var("DEBUG_PANICS").is_ok() {
            match key.code {
                KeyCode::Char('Z') => panic!("z pressed so we panic -> {}", uuid::Uuid::new_v4()),
                KeyCode::Char('X') => bail!("x pressed so we bail -> {}", uuid::Uuid::new_v4()),
                KeyCode::Char('E') => {
                    Err(anyhow!(
                        "E pressed so we bail with context -> {}",
                        uuid::Uuid::new_v4()
                    ))
                    .context("With a message")
                    .context("With a context")?;
                }
                KeyCode::Char('C') => {
                    return Ok(Some(ServeUpdate::Exit {
                        error: Some(anyhow!(
                            "C pressed, so exiting with safe error -> {}",
                            uuid::Uuid::new_v4()
                        )),
                    }));
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Some(ServeUpdate::Exit { error: None }));
            }
            KeyCode::Char('r') => return Ok(Some(ServeUpdate::RequestRebuild)),
            KeyCode::Char('o') => return Ok(Some(ServeUpdate::OpenApp)),
            KeyCode::Char('p') => return Ok(Some(ServeUpdate::CycleHotreloadMode)),
            KeyCode::Char('v') => {
                self.verbose = !self.verbose;
                tracing::info!(
                    "Verbose logging is now {}",
                    if self.verbose { "on" } else { "off" }
                );
            }
            KeyCode::Char('t') => {
                self.trace = !self.trace;
                tracing::info!("Tracing is now {}", if self.trace { "on" } else { "off" });
            }
            KeyCode::Char('D') => {
                return Ok(Some(ServeUpdate::OpenDebugger {
                    id: BuildId::SECONDARY,
                }));
            }
            KeyCode::Char('d') => {
                return Ok(Some(ServeUpdate::OpenDebugger {
                    id: BuildId::PRIMARY,
                }));
            }
            KeyCode::Char('c') => {
                stdout()
                    .execute(Clear(ClearType::All))?
                    .execute(Clear(ClearType::Purge))?;

                // Clear the terminal and push the frame to the bottom
                _ = self.term.borrow_mut().as_mut().map(|t| {
                    let frame_rect = t.get_frame().area();
                    let term_size = t.size().unwrap();
                    let remaining_space = term_size
                        .height
                        .saturating_sub(frame_rect.y + frame_rect.height);
                    t.insert_before(remaining_space, |_| {})
                });
            }

            // Toggle the more modal by swapping the terminal with a new one
            // This is a bit of a hack since crossterm doesn't technically support changing the
            // size of an inline viewport.
            KeyCode::Char('/') => {
                if let Some(terminal) = self.term.borrow_mut().as_mut() {
                    // Toggle the more modal, which will change our current viewport height
                    self.more_modal_open = !self.more_modal_open;

                    // Clear the terminal before resizing it, such that it doesn't tear
                    terminal.clear()?;

                    // And then set the new viewport, which essentially mimics a resize
                    *terminal = Terminal::with_options(
                        CrosstermBackend::new(stdout()),
                        TerminalOptions {
                            viewport: Viewport::Inline(self.viewport_current_height()),
                        },
                    )?;
                }
            }

            _ => {}
        }

        // Out of safety, we always redraw, since it's relatively cheap operation
        Ok(Some(ServeUpdate::Redraw))
    }

    /// Push a TraceMsg to be printed on the next render
    pub fn push_log(&mut self, message: TraceMsg) {
        self.pending_logs.push_front(message);
    }

    pub fn push_cargo_log(&mut self, message: Diagnostic) {
        use cargo_metadata::diagnostic::DiagnosticLevel;

        if self.trace || !matches!(message.level, DiagnosticLevel::Note) {
            self.push_log(TraceMsg::cargo(message));
        }
    }

    /// Add a message from stderr to the logs
    /// This will queue the stderr message as a TraceMsg and print it on the next render
    /// We'll use the `App` TraceSrc for the msg, and whatever level is provided
    pub fn push_stdio(&mut self, bundle: BundleFormat, msg: String, level: Level) {
        match bundle {
            // If tracing is disabled, we need to filter out all the noise from Android logcat
            BundleFormat::Android if !self.trace => {
                // By default (trace off): only show RustStdoutStderr logs with proper log levels
                // Filter out raw output like GL bindings, WebView internals, etc.
                let is_rust_log = msg.contains("RustStdoutStderr");
                let is_fatal = msg.contains("AndroidRuntime") || msg.starts_with('F');
                let mut rendered_msg = None;

                // If we're not in trace mode, then we need to filter out non-log messages and clean them up
                // Only show logs with standard tracing level prefixes (check in the message after the colon)
                if is_rust_log {
                    if let Some(colon_pos) = msg.find(':') {
                        let content = &msg[colon_pos + 1..];
                        if content.contains(" TRACE ")
                            || content.contains(" DEBUG ")
                            || content.contains(" INFO ")
                            || content.contains(" WARN ")
                            || content.contains(" ERROR ")
                        {
                            rendered_msg = Some(content.trim().to_string());
                        }
                    }
                }

                // Always show fatal errors, even if they don't come from Rust logging
                if is_fatal {
                    rendered_msg = Some(msg);
                }

                if let Some(msg) = rendered_msg {
                    self.push_log(TraceMsg::text(TraceSrc::App(bundle), level, msg));
                }
            }

            _ => self.push_log(TraceMsg::text(TraceSrc::App(bundle), level, msg)),
        }
    }

    /// Push a message from the websocket to the logs
    pub fn push_ws_message(&mut self, bundle: BundleFormat, message: &axum::extract::ws::Message) {
        use dioxus_devtools_types::ClientMsg;

        // We can only handle text messages from the websocket...
        let axum::extract::ws::Message::Text(text) = message else {
            return;
        };

        // ...and then decode them into a ClientMsg
        let res = serde_json::from_str::<ClientMsg>(text.as_str());

        // Client logs being errors aren't fatal, but we should still report them them
        let msg = match res {
            Ok(msg) => msg,
            Err(err) => {
                tracing::error!(dx_src = ?TraceSrc::Dev, "Error parsing message from {}: {} -> {:?}", bundle, err, text.as_str());
                return;
            }
        };

        let ClientMsg::Log { level, messages } = msg else {
            return;
        };

        // FIXME(jon): why are we pulling only the first message here?
        let content = messages.first().unwrap_or(&String::new()).clone();

        let level = match level.as_str() {
            "trace" => Level::TRACE,
            "debug" => Level::DEBUG,
            "info" => Level::INFO,
            "warn" => Level::WARN,
            "error" => Level::ERROR,
            _ => Level::INFO,
        };

        // We don't care about logging the app's message so we directly push it instead of using tracing.
        self.push_log(TraceMsg::text(TraceSrc::App(bundle), level, content));
    }

    /// Change internal state based on the build engine's update
    ///
    /// We want to keep internal state as limited as possible, so currently we're only setting our
    /// animation tick. We could, in theory, just leave animation running and have no internal state,
    /// but that seems a bit wasteful. We might eventually change this to be more of a "requestAnimationFrame"
    /// approach, but then we'd need to do that *everywhere* instead of simply performing a react-like
    /// re-render when external state changes. Ratatui will diff the intermediate buffer, so we at least
    /// we won't be drawing it.
    pub(crate) fn new_build_update(&mut self, update: &BuilderUpdate) {
        match update {
            BuilderUpdate::Progress {
                stage: BuildStage::Starting { .. },
            } => self.tick_animation = true,
            BuilderUpdate::BuildReady { .. } => self.tick_animation = false,
            BuilderUpdate::BuildFailed { .. } => self.tick_animation = false,
            _ => {}
        }
    }

    /// Render the current state of everything to the console screen
    pub fn render(&mut self, runner: &AppServer, server: &WebServer) {
        if !self.interactive {
            self.emit_logs_to_tracing();
            return;
        }

        // Get a handle to the terminal with a different lifetime so we can continue to call &self methods
        let owned_term = self.term.clone();
        let mut term = owned_term.borrow_mut();
        let Some(term) = term.as_mut() else {
            return;
        };

        // First, dequeue any logs that have built up from event handling
        while let Some(log) = self.pending_logs.pop_back() {
            _ = self.render_log(term, log);
        }

        // Then, draw the frame, passing along all the state of the TUI so we can render it properly
        _ = term.draw(|frame| {
            self.render_frame(frame, RenderState { runner, server });
        });
    }

    fn render_frame(&self, frame: &mut Frame, state: RenderState) {
        // Use the max size of the viewport, but shrunk to a sensible max width
        let mut area = frame.area();
        area.width = area.width.clamp(0, VIEWPORT_MAX_WIDTH);

        let [_top, body, _bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .horizontal_margin(1)
        .areas(area);

        self.render_borders(frame, area);
        self.render_body(frame, body, state);
        self.render_body_title(frame, _top, state);
    }

    fn render_body_title(&self, frame: &mut Frame<'_>, area: Rect, _state: RenderState) {
        frame.render_widget(
            Line::from(vec![
                " ".dark_gray(),
                match self.more_modal_open {
                    true => "/:more".light_yellow(),
                    false => "/:more".dark_gray(),
                },
                " ".dark_gray(),
            ])
            .right_aligned(),
            area,
        );
    }

    fn render_body(&self, frame: &mut Frame<'_>, area: Rect, state: RenderState) {
        let [_title, body, more, _foot] = Layout::vertical([
            Constraint::Length(0),
            Constraint::Length(VIEWPORT_HEIGHT_SMALL - 2),
            Constraint::Fill(1),
            Constraint::Length(0),
        ])
        .horizontal_margin(1)
        .areas(area);

        let [col1, col2] = Layout::horizontal([Constraint::Length(50), Constraint::Fill(1)])
            .horizontal_margin(1)
            .areas(body);

        self.render_gauges(frame, col1, state);
        self.render_stats(frame, col2, state);

        if self.more_modal_open {
            self.render_more_modal(frame, more, state);
        }
    }

    fn render_gauges(&self, frame: &mut Frame<'_>, area: Rect, state: RenderState) {
        let [gauge_area, _margin] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(3)]).areas(area);

        let [app_progress, second_progress, status_line]: [_; 3] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(gauge_area);

        let client = &state.runner.client();
        self.render_single_gauge(
            frame,
            app_progress,
            client.compile_progress(),
            "App:    ",
            state,
            client.compile_duration(),
        );

        if state.runner.is_fullstack() {
            self.render_single_gauge(
                frame,
                second_progress,
                state.runner.server_compile_progress(),
                "Server: ",
                state,
                client.compile_duration(),
            );
        } else {
            self.render_single_gauge(
                frame,
                second_progress,
                client.bundle_progress(),
                "Bundle: ",
                state,
                client.bundle_duration(),
            );
        }

        let mut lines = vec!["Status:  ".white()];
        match &client.stage {
            BuildStage::Initializing => lines.push("Initializing".yellow()),
            BuildStage::Starting { patch, .. } => {
                if *patch {
                    lines.push("Hot-patching...".yellow())
                } else {
                    lines.push("Starting build".yellow())
                }
            }
            BuildStage::InstallingTooling => lines.push("Installing tooling".yellow()),
            BuildStage::Compiling {
                current,
                total,
                krate,
                ..
            } => {
                lines.push("Compiling ".yellow());
                lines.push(format!("{current}/{total} ").gray());
                lines.push(krate.as_str().dark_gray())
            }
            BuildStage::OptimizingWasm => lines.push("Optimizing wasm".yellow()),
            BuildStage::SplittingBundle => lines.push("Splitting bundle".yellow()),
            BuildStage::CompressingAssets => lines.push("Compressing assets".yellow()),
            BuildStage::RunningBindgen => lines.push("Running wasm-bindgen".yellow()),
            BuildStage::RunningGradle => lines.push("Running gradle assemble".yellow()),
            BuildStage::CompilingNativePlugins { detail } => {
                // detail is "Swift build: name" — split into label (yellow) and name (white)
                if let Some((label, name)) = detail.split_once(": ") {
                    lines.push(format!("{label}: ").yellow());
                    lines.push(name.white());
                } else {
                    lines.push(detail.clone().yellow());
                }
            }
            BuildStage::CodeSigning => lines.push("Code signing app".yellow()),
            BuildStage::Bundling => lines.push("Bundling app".yellow()),
            BuildStage::CopyingAssets {
                current,
                total,
                path,
            } => {
                lines.push("Copying asset ".yellow());
                lines.push(format!("{current}/{total} ").gray());
                if let Some(name) = path.file_name().and_then(|f| f.to_str()) {
                    lines.push(name.dark_gray())
                }
            }
            BuildStage::Success => {
                lines.push("Serving ".yellow());
                lines.push(client.build.executable_name().white());
                lines.push(" 🚀 ".green());
                if let Some(comp_time) = client.total_build_time() {
                    lines.push(format!("{:.1}s", comp_time.as_secs_f32()).dark_gray());
                }
            }
            BuildStage::Failed => lines.push("Failed".red()),
            BuildStage::Aborted => lines.push("Aborted".red()),
            BuildStage::Restarting => lines.push("Restarting".yellow()),
            BuildStage::Linking => lines.push("Linking".yellow()),
            BuildStage::Hotpatching => lines.push("Hot-patching...".yellow()),
            BuildStage::ExtractingAssets => lines.push("Extracting assets".yellow()),
            BuildStage::Prerendering => lines.push("Pre-rendering...".yellow()),
            _ => {}
        };

        frame.render_widget(Line::from(lines), status_line);
    }

    fn render_single_gauge(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        value: f64,
        label: &str,
        state: RenderState,
        time_taken: Option<Duration>,
    ) {
        let failed = state.runner.client.stage == BuildStage::Failed;
        let value = if failed { 1.0 } else { value.clamp(0.0, 1.0) };

        let [gauge_row, _, icon] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Length(10),
        ])
        .areas(area);

        frame.render_widget(
            LineGauge::default()
                .filled_style(Style::default().fg(match value {
                    1.0 if failed => Color::Red,
                    1.0 => Color::Green,
                    _ => Color::Yellow,
                }))
                .unfilled_style(Style::default().fg(Color::DarkGray))
                .label(label.gray())
                .filled_symbol(symbols::line::THICK.horizontal)
                .unfilled_symbol(symbols::line::THICK.horizontal)
                .ratio(if !failed { value } else { 1.0 }),
            gauge_row,
        );

        let [throbber_frame, time_frame] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(3), Constraint::Fill(1)])
            .areas(icon);

        if value != 1.0 {
            let throb = throbber_widgets_tui::Throbber::default()
                .style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan))
                .throbber_style(
                    ratatui::style::Style::default()
                        .fg(ratatui::style::Color::White)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )
                .throbber_set(throbber_widgets_tui::BLACK_CIRCLE)
                .use_type(throbber_widgets_tui::WhichUse::Spin);
            frame.render_stateful_widget(throb, throbber_frame, &mut self.throbber.borrow_mut());
        } else {
            frame.render_widget(
                Line::from(vec![if failed {
                    "❌ ".white()
                } else {
                    "🎉 ".white()
                }])
                .left_aligned(),
                throbber_frame,
            );
        }

        if let Some(time_taken) = time_taken {
            if !failed {
                frame.render_widget(
                    Line::from(vec![
                        format!("{:.1}s", time_taken.as_secs_f32()).dark_gray(),
                    ])
                    .left_aligned(),
                    time_frame,
                );
            }
        }
    }

    fn render_stats(&self, frame: &mut Frame<'_>, area: Rect, state: RenderState) {
        let [current_platform, hotreload_mode, serve_address]: [_; 3] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let client = &state.runner.client();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                "Platform: ".gray(),
                client.build.bundle.expected_name().yellow(),
                if state.runner.is_fullstack() {
                    " + fullstack".yellow()
                } else {
                    " ".dark_gray()
                },
            ])),
            current_platform,
        );

        let (label, indicator) = match state.runner.hotreload_mode {
            HotReloadMode::Hotpatch => ("hot-patching".yellow(), " ✓".yellow()),
            HotReloadMode::RsxOnly => ("rsx and assets".yellow(), " ~".yellow()),
            HotReloadMode::Disabled => ("disabled".dark_gray(), " ✗".dark_gray()),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec!["Hotreload: ".gray(), label, indicator])),
            hotreload_mode,
        );

        // todo(jon) should we write https ?
        let address = match state.server.displayed_address() {
            Some(address) => format!(
                "http://{}{}",
                address,
                state
                    .runner
                    .client
                    .build
                    .base_path()
                    .map(|f| format!("/{f}/"))
                    .unwrap_or_default()
            )
            .blue(),
            None => "no server address".dark_gray(),
        };

        frame.render_widget(
            Paragraph::new(Line::from(vec!["Address:  ".gray(), address])),
            serve_address,
        );
    }

    fn render_feature_list(&self, frame: &mut Frame<'_>, area: Rect, state: RenderState) {
        frame.render_widget(
            Paragraph::new(Line::from({
                let mut lines = vec!["Features: ".gray(), "[".yellow()];

                let feature_list: Vec<String> = state.runner.client().build.all_target_features();
                let num_features = feature_list.len();

                for (idx, feature) in feature_list.into_iter().enumerate() {
                    lines.push("\"".yellow());
                    lines.push(feature.yellow());
                    lines.push("\"".yellow());
                    if idx != num_features - 1 {
                        lines.push(", ".dark_gray());
                    }
                }

                lines.push("]".yellow());

                lines
            })),
            area,
        );
    }

    fn render_more_modal(&self, frame: &mut Frame<'_>, area: Rect, state: RenderState) {
        let [col1, col2] =
            Layout::horizontal([Constraint::Length(50), Constraint::Fill(1)]).areas(area);

        let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(2)])
            .horizontal_margin(1)
            .areas(col1);

        let meta_list: [_; 6] = Layout::vertical([
            Constraint::Length(1), // spacing
            Constraint::Length(1), // item 1
            Constraint::Length(1), // item 2
            Constraint::Length(1), // item 3
            Constraint::Length(1), // item 4
            Constraint::Length(1), // Spacing
        ])
        .areas(top);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                "dx version: ".gray(),
                self.dx_version.as_str().yellow(),
            ])),
            meta_list[1],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                "rustc: ".gray(),
                state.runner.workspace.rustc_version.as_str().yellow(),
            ])),
            meta_list[2],
        );
        self.render_feature_list(frame, meta_list[3], state);

        // space
        frame.render_widget(Paragraph::new(Line::from(vec![" ".gray()])), meta_list[4]);

        let links_list: [_; 2] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(bottom);

        if state.runner.client.build.using_dioxus_explicitly {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    "Read the docs: ".gray(),
                    "https://dioxuslabs.com/learn/0.7/".blue(),
                ])),
                links_list[0],
            );

            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    "Video tutorials: ".gray(),
                    "https://youtube.com/@DioxusLabs".blue(),
                ])),
                links_list[1],
            );
        }

        let cmds = [
            "",
            "r: rebuild the app",
            "o: open the app",
            "p: cycle hotreload mode",
            "v: toggle verbose logs",
            "t: toggle tracing logs",
            "c: clear the screen",
            "d: attach debugger",
            "/: toggle more commands",
        ];
        let layout: [_; 9] = Layout::vertical(cmds.iter().map(|_| Constraint::Length(1)))
            .horizontal_margin(1)
            .areas(col2);
        for (idx, cmd) in cmds.iter().enumerate() {
            if cmd.is_empty() {
                continue;
            }

            let (cmd, detail) = cmd.split_once(": ").unwrap_or((cmd, ""));
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    cmd.gray(),
                    ": ".gray(),
                    detail.dark_gray(),
                ])),
                layout[idx],
            );
        }
    }

    /// Render borders around the terminal, forcing an inner clear while we're at it
    fn render_borders(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }

    /// Print logs to the terminal as close to a regular "println!()" as possible.
    ///
    /// We don't want alternate screens or other terminal tricks because we want these logs to be as
    /// close to real as possible. Once the log is printed, it is lost, so we need to be very careful
    /// here to not print it incorrectly.
    ///
    /// We hand the rendered lines to `Terminal::insert_before`, which owns the full responsibility
    /// of making room for them - either by pushing the viewport further down the screen, or by
    /// scrolling the screen above it upwards once the viewport is pinned to the bottom. This keeps
    /// ratatui's own bookkeeping of where the viewport lives in sync with what's physically on the
    /// terminal.
    ///
    /// Previously this method computed the scroll amount by hand and then wrote the log text
    /// directly via raw crossterm cursor movements, bypassing `insert_before`'s own scrolling (it
    /// was called with a no-op draw callback purely to borrow its screen-scrolling side effect).
    /// Those two mechanisms didn't agree on how far the screen had moved, so ratatui's next `draw`
    /// call would diff against a buffer that no longer matched reality - rows would be redrawn
    /// blank one at a time instead of new text appearing, and old lines wouldn't scroll up as a
    /// unit. Rendering straight into the buffer that `insert_before` hands us avoids the desync
    /// entirely.
    fn render_log<B>(&mut self, terminal: &mut Terminal<B>, log: TraceMsg) -> Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // Only show debug logs if verbose is enabled
        if log.level == Level::DEBUG && !self.verbose {
            return Ok(());
        }

        if log.level == Level::TRACE && !self.trace {
            return Ok(());
        }

        // Render the log into styled lines, with metadata like the timestamp and source attached.
        let lines = Self::tracemsg_to_lines(log);
        if lines.is_empty() {
            return Ok(());
        }

        // Use ratatui's own cached viewport width here, not `terminal.size()`. `size()` queries
        // the backend live and can be ahead of what `insert_before` actually uses internally
        // (`self.viewport_area`, only resynced by `autoresize()` inside `Terminal::draw`). If a
        // resize lands in between two `draw()` calls, computing our height against the live width
        // while `insert_before` renders into a buffer sized by the stale cached width makes the
        // two disagree on how many rows the paragraph needs - so it draws over however many rows
        // we guessed, which can land on top of the gauges below. `get_frame().area()` reads the
        // same `viewport_area` that `insert_before` renders into, so the two always agree.
        let width = terminal.get_frame().area().width;
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let height = (paragraph.line_count(width) as u16).max(1);

        // Let ratatui render straight into the space it inserts, instead of guessing where that
        // space landed and writing to it ourselves afterwards.
        terminal.insert_before(height, |buf| {
            paragraph.render(buf.area, buf);
        })?;

        Ok(())
    }

    fn viewport_current_height(&self) -> u16 {
        match self.more_modal_open {
            true => VIEWPORT_HEIGHT_BIG,
            false => VIEWPORT_HEIGHT_SMALL,
        }
    }

    fn tracemsg_to_lines(log: TraceMsg) -> Vec<Line<'static>> {
        use ansi_to_tui::IntoText;
        use chrono::Timelike;

        let rendered = match log.content {
            TraceContent::Cargo(msg) => msg.rendered.unwrap_or_default(),
            TraceContent::Text(text) => text,
        };

        let mut lines = vec![];

        for (idx, raw_line) in rendered.lines().enumerate() {
            let line_as_text = raw_line.into_text().unwrap();
            let is_pretending_to_be_frame = !raw_line.is_empty()
                && raw_line
                    .chars()
                    .all(|c| c == '=' || c == '-' || c == ' ' || c == '─');

            for (subline_idx, mut line) in line_as_text.lines.into_iter().enumerate() {
                if idx == 0 && subline_idx == 0 {
                    let mut formatted_line = Line::default();

                    formatted_line.push_span(
                        Span::raw(format!(
                            "{:02}:{:02}:{:02} ",
                            log.timestamp.hour(),
                            log.timestamp.minute(),
                            log.timestamp.second()
                        ))
                        .dark_gray(),
                    );

                    formatted_line.push_span(
                        Span::raw(format!(
                            "[{src}] {padding}",
                            src = log.source,
                            padding =
                                " ".repeat(3usize.saturating_sub(log.source.to_string().len()))
                        ))
                        .style(match log.source {
                            TraceSrc::App(_platform) => match log.level {
                                Level::ERROR => Style::new().red(),
                                Level::WARN => Style::new().yellow(),
                                Level::INFO => Style::new().magenta(),
                                Level::DEBUG => Style::new().magenta(),
                                Level::TRACE => Style::new().magenta(),
                            },
                            TraceSrc::Dev => match log.level {
                                Level::ERROR => Style::new().red(),
                                Level::WARN => Style::new().yellow(),
                                Level::INFO => Style::new().blue(),
                                Level::DEBUG => Style::new().blue(),
                                Level::TRACE => Style::new().blue(),
                            },
                            TraceSrc::Cargo => Style::new().yellow(),
                            TraceSrc::Build => Style::new().blue(),
                            TraceSrc::Bundle => Style::new().blue(),
                            TraceSrc::Unknown => Style::new().gray(),
                        }),
                    );

                    for span in line.spans {
                        formatted_line.push_span(span);
                    }

                    line = formatted_line;
                }

                if is_pretending_to_be_frame {
                    line = line.dark_gray();
                }

                lines.push(line);
            }
        }

        lines
    }

    /// Drains the current log queue to tracing so they still get printed even if the output is non-interactive
    fn emit_logs_to_tracing(&mut self) {
        // In non-interactive mode (CI, piped output), drain pending logs to tracing
        // so they still appear in stdout/stderr instead of being silently dropped.
        while let Some(log) = self.pending_logs.pop_back() {
            let msg = match &log.content {
                TraceContent::Text(s) => s.as_str(),
                TraceContent::Cargo(d) => d.message.as_str(),
            };
            match log.level {
                Level::ERROR => tracing::error!("{msg}"),
                Level::WARN => tracing::warn!("{msg}"),
                Level::INFO => tracing::info!("{msg}"),
                Level::DEBUG => tracing::debug!("{msg}"),
                Level::TRACE => tracing::trace!("{msg}"),
            }
        }
    }
}

impl std::ops::Drop for Output {
    fn drop(&mut self) {
        if !self.interactive {
            self.emit_logs_to_tracing();
            return;
        }

        // Get a handle to the terminal with a different lifetime so we can continue to call &self methods
        let owned_term = self.term.clone();
        let mut term = owned_term.borrow_mut();
        let Some(term) = term.as_mut() else {
            return;
        };

        // First, dequeue any logs that have built up from event handling
        while let Some(log) = self.pending_logs.pop_back() {
            _ = self.render_log(term, log);
        }

        // And then lets move the cursor to the bottom of the terminal
        let frame_rect = term.get_frame().area();
        _ = term.set_cursor_position(Position {
            x: 0,
            y: frame_rect.y + frame_rect.height,
        });

        // Tear down the TUI if it's active at this point.
        _ = crate::serve::Output::remote_shutdown(self.interactive);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Builds an `Output` without touching the real terminal, so `render_log` can be exercised
    /// against a `TestBackend` instead of `CrosstermBackend<io::Stdout>`.
    fn test_output() -> Output {
        Output {
            interactive: true,
            term: Rc::new(RefCell::new(None)),
            dx_version: "test".into(),
            events: None,
            more_modal_open: false,
            pending_logs: VecDeque::new(),
            throbber: RefCell::new(throbber_widgets_tui::ThrobberState::default()),
            trace: false,
            verbose: false,
            tick_animation: false,
            tick_interval: {
                let mut interval = tokio::time::interval(Duration::from_millis(TICK_RATE_MS));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval
            },
        }
    }

    fn log(text: &str) -> TraceMsg {
        TraceMsg::text(TraceSrc::Dev, Level::INFO, text.to_string())
    }

    /// Renders the "gauges" area, standing in for the real `render_frame` call in `render()`.
    /// What matters for this test is only that *something* occupies the viewport so we can check
    /// it survives log rendering untouched.
    fn draw_viewport<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
        terminal
            .draw(|f| f.render_widget(Paragraph::new("VIEWPORT MARKER"), f.area()))
            .unwrap();
    }

    /// Flattens a `TestBackend`'s buffer into one string per row, for easy substring assertions.
    fn rows_of(backend: &TestBackend) -> Vec<String> {
        let buffer = backend.buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Reproduces the reported bug: pin the inline viewport at the bottom of a small terminal
    /// (the steady state for `dx serve`, where there's no space below the gauges), then push
    /// several distinct log lines one at a time the same way `render()` drains `pending_logs`.
    ///
    /// Before the fix, `render_log` called `insert_before` with a no-op draw closure purely to
    /// borrow its scrolling side effect, then wrote the actual text separately via raw crossterm
    /// cursor math that didn't agree with where `insert_before` had actually moved the viewport.
    /// That desync is what produced the reported symptom: rows going blank instead of showing the
    /// new line, and old lines not scrolling up as a unit. Rendering happens entirely through
    /// `insert_before`'s own buffer now, so there's nothing left to desync.
    #[tokio::test]
    async fn log_lines_scroll_without_blanking() {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(3),
            },
        )
        .unwrap();

        // Establish the viewport, then push it down to the very bottom of the screen by
        // inserting filler lines - matching `dx serve`'s steady state where there's no slack
        // below the gauges and new lines must scroll the screen itself.
        draw_viewport(&mut terminal);
        terminal.insert_before(3, |_| {}).unwrap();
        draw_viewport(&mut terminal);

        let mut output = test_output();

        for i in 0..6 {
            output
                .render_log(&mut terminal, log(&format!("log line {i}")))
                .unwrap();
            draw_viewport(&mut terminal);
        }

        let rows = rows_of(terminal.backend());
        let screen = rows.join("\n");

        // The most recent lines must be visible and intact - not blanked out.
        assert!(
            screen.contains("log line 5"),
            "newest log line missing from screen:\n{screen}"
        );
        assert!(
            screen.contains("log line 4"),
            "previous log line missing from screen (should still be visible pre-scroll-off):\n{screen}"
        );

        // The viewport itself must still be intact and present exactly once - not overwritten by
        // a desynced log print, and not duplicated by a desynced scroll.
        let marker_rows = rows
            .iter()
            .filter(|r| r.contains("VIEWPORT MARKER"))
            .count();
        assert_eq!(
            marker_rows, 1,
            "expected exactly one intact viewport row, got {marker_rows}:\n{screen}"
        );

        // No row should be entirely blank in between two rows that have content - a blank gap
        // surrounded by content is exactly the "line turned blank" symptom.
        let first_content = rows.iter().position(|r| !r.is_empty());
        let last_content = rows.iter().rposition(|r| !r.is_empty());
        if let (Some(first), Some(last)) = (first_content, last_content) {
            for (idx, row) in rows.iter().enumerate().take(last + 1).skip(first) {
                assert!(
                    !row.is_empty(),
                    "row {idx} went blank between content rows {first} and {last}:\n{screen}"
                );
            }
        }
    }
}
