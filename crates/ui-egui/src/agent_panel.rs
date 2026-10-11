//! The AI agent panel: a chat in the bottom-right corner where Claude Code or Codex drives
//! VectorCraft through its MCP tools (the same ones `vectorcraft-cli mcp --connect` serves).
//!
//! This crate only draws the panel and keeps the conversation. Starting the agent CLI is a
//! platform service ([`crate::Services::agent`]): the desktop app spawns `claude -p` / `codex exec`
//! with the user's own login (their subscription, not an API key) and an MCP config that connects
//! back to this window's control channel. The web build has no service, so no buttons show.

use std::sync::mpsc::{Receiver, TryRecvError};

use egui::{CornerRadius, RichText, Stroke, vec2};

use crate::ControlRequest;
use crate::VectorcraftApp;
use crate::state::AgentKind;
use crate::theme::Tokens;
use crate::widgets;

/// Entries kept per conversation; older ones scroll away.
const MAX_ENTRIES: usize = 400;
/// Characters kept per entry (a tool can return a lot of JSON).
const MAX_ENTRY_CHARS: usize = 8_000;
const WIDTH: f32 = 400.0;
const HEIGHT: f32 = 460.0;
/// Room the two app bar buttons need.
pub const BUTTONS_WIDTH: f32 = 190.0;

/// What the agent process reports while it works.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    /// The agent's conversation id, to continue it with the next message.
    Session(String),
    /// Text the agent wrote for the user.
    Text(String),
    /// A tool call, already summarised for display (`command_run shape.rectangle`).
    Tool(String),
    Error(String),
    /// The process ended; nothing follows.
    Done,
}

/// One message for the agent.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRequest {
    pub kind: AgentKind,
    pub prompt: String,
    /// The conversation to continue, from an earlier [`AgentEvent::Session`].
    pub session: Option<String>,
}

/// A running agent process.
pub struct AgentRun {
    pub events: Receiver<AgentEvent>,
    /// Stops the process.
    pub cancel: Box<dyn FnMut()>,
    /// The control channel the service started for the agent's MCP server, when the window had
    /// none yet: the app serves it from now on.
    pub control: Option<Receiver<ControlRequest>>,
}

/// Starts the agent for a message. The context is for waking the UI when events arrive.
pub type AgentStartFn = Box<dyn FnMut(AgentRequest, egui::Context) -> Result<AgentRun, String>>;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    User(String),
    Agent(String),
    Tool(String),
    Error(String),
}

#[derive(Default)]
pub struct Conversation {
    pub entries: Vec<Entry>,
    pub session: Option<String>,
}

impl Conversation {
    fn push(&mut self, entry: Entry) {
        let entry = match entry {
            Entry::User(s) => Entry::User(clip(s)),
            Entry::Agent(s) => Entry::Agent(clip(s)),
            Entry::Tool(s) => Entry::Tool(clip(s)),
            Entry::Error(s) => Entry::Error(clip(s)),
        };
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            let extra = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..extra);
        }
    }

    /// Records one event; returns whether the run ended.
    pub fn apply(&mut self, event: AgentEvent) -> bool {
        match event {
            AgentEvent::Session(id) => self.session = Some(id),
            AgentEvent::Text(s) if s.trim().is_empty() => {}
            AgentEvent::Text(s) => self.push(Entry::Agent(s)),
            AgentEvent::Tool(s) => self.push(Entry::Tool(s)),
            AgentEvent::Error(s) => self.push(Entry::Error(s)),
            AgentEvent::Done => return true,
        }
        false
    }
}

/// Keeps at most [`MAX_ENTRY_CHARS`] characters, cutting at a char boundary.
fn clip(s: String) -> String {
    match s.char_indices().nth(MAX_ENTRY_CHARS) {
        Some((at, _)) => format!("{}…", s.get(..at).unwrap_or_default()),
        None => s,
    }
}

/// The panel's runtime state (the open/closed state and the agent are `UiState::agent_panel`).
#[derive(Default)]
pub struct Runtime {
    claude: Conversation,
    codex: Conversation,
    pub input: String,
    run: Option<(AgentKind, AgentRun)>,
    /// The document area this frame: the panel opens in its bottom-right corner.
    pub(crate) area: Option<egui::Rect>,
}

impl Runtime {
    pub fn conversation(&self, kind: AgentKind) -> &Conversation {
        match kind {
            AgentKind::Claude => &self.claude,
            AgentKind::Codex => &self.codex,
        }
    }

    fn conversation_mut(&mut self, kind: AgentKind) -> &mut Conversation {
        match kind {
            AgentKind::Claude => &mut self.claude,
            AgentKind::Codex => &mut self.codex,
        }
    }

    /// The agent working on a message, if any.
    pub fn running(&self) -> Option<AgentKind> {
        self.run.as_ref().map(|(k, _)| *k)
    }
}

/// Opens the panel on `kind`, or closes it when it already shows that agent.
pub fn toggle(app: &mut VectorcraftApp, kind: AgentKind) {
    app.ui.agent_panel = if app.ui.agent_panel == Some(kind) { None } else { Some(kind) };
}

/// Sends the typed message to `kind`.
pub fn send(app: &mut VectorcraftApp, ctx: &egui::Context, kind: AgentKind) {
    let prompt = app.agent.input.trim().to_string();
    if prompt.is_empty() || app.agent.run.is_some() {
        return;
    }
    let Some(start) = app.services.agent.as_mut() else {
        app.agent.conversation_mut(kind).push(Entry::Error(tl!("AI agents are only available in the desktop app.").to_string()));
        return;
    };
    let session = app.agent.conversation(kind).session.clone();
    app.agent.input.clear();
    app.agent.conversation_mut(kind).push(Entry::User(prompt.clone()));
    match start(AgentRequest { kind, prompt, session }, ctx.clone()) {
        Ok(mut run) => {
            if let Some(rx) = run.control.take()
                && app.control_rx.is_none()
            {
                app.control_rx = Some(rx);
            }
            app.agent.run = Some((kind, run));
        }
        Err(e) => app.agent.conversation_mut(kind).push(Entry::Error(e)),
    }
}

/// Stops the running agent.
pub fn stop(app: &mut VectorcraftApp) {
    if let Some((kind, mut run)) = app.agent.run.take() {
        (run.cancel)();
        app.agent.conversation_mut(kind).push(Entry::Error(tl!("Stopped.").to_string()));
    }
}

/// Moves the running agent's events into its conversation.
fn poll(app: &mut VectorcraftApp, ctx: &egui::Context) {
    let Some((kind, run)) = app.agent.run.as_mut() else { return };
    let kind = *kind;
    let mut events = Vec::new();
    let mut ended = false;
    loop {
        match run.events.try_recv() {
            Ok(e) => events.push(e),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                ended = true;
                break;
            }
        }
    }
    let conv = app.agent.conversation_mut(kind);
    for e in events {
        ended |= conv.apply(e);
    }
    if ended {
        app.agent.run = None;
    } else {
        // The service wakes the UI on each event; this is only a fallback for the spinner.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// Whether the app bar shows the agent buttons (desktop only).
pub fn has_buttons(app: &VectorcraftApp) -> bool {
    app.services.agent.is_some()
}

/// The two app bar buttons, in a right-to-left layout.
pub fn app_bar_buttons(app: &mut VectorcraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    // Right to left: the first button lands nearest the end, so add them in reverse.
    for kind in AgentKind::ALL.into_iter().rev() {
        let on = app.ui.agent_panel == Some(kind);
        let color = if on { t.text_strong } else { t.text_dim };
        let icon = if kind == AgentKind::Claude { "sparkles" } else { "wand-sparkles" };
        let button = egui::Button::image_and_text(
            egui::Image::new(crate::icons::source(icon)).fit_to_exact_size(vec2(14.0, 14.0)).tint(color),
            RichText::new(kind.label()).color(color).size(12.0),
        )
        .frame(on);
        if ui.add(button).on_hover_text(kind.label()).clicked() {
            toggle(app, kind);
        }
    }
}

/// Draws the panel (when open) and collects the running agent's output.
pub fn show(app: &mut VectorcraftApp, ctx: &egui::Context) {
    poll(app, ctx);
    let Some(kind) = app.ui.agent_panel else { return };
    let t = Tokens::get(ctx);
    let frame = egui::Frame::NONE
        .fill(t.panel)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(8))
        .shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: egui::Color32::from_black_alpha(90) })
        .inner_margin(egui::Margin::same(10));
    let corner = app.agent.area.filter(|r| r.is_positive()).unwrap_or_else(|| ctx.content_rect()).right_bottom();
    let mut open = true;
    egui::Window::new("agent-panel")
        .id(egui::Id::new("agent-panel"))
        .title_bar(false)
        .frame(frame)
        .pivot(egui::Align2::RIGHT_BOTTOM)
        .default_pos(corner - vec2(12.0, 12.0))
        .default_size(vec2(WIDTH, HEIGHT))
        .min_size(vec2(280.0, 240.0))
        .resizable(true)
        .show(ctx, |ui| {
            header(app, ui, &t, kind, &mut open);
            ui.separator();
            let input_height = 64.0;
            let list_height = (ui.available_height() - input_height - 16.0).max(80.0);
            transcript(app, ui, &t, kind, list_height);
            ui.separator();
            composer(app, ui, &t, kind);
        });
    if !open {
        app.ui.agent_panel = None;
    }
}

fn header(app: &mut VectorcraftApp, ui: &mut egui::Ui, t: &Tokens, kind: AgentKind, open: &mut bool) {
    ui.horizontal(|ui| {
        for k in AgentKind::ALL {
            if ui.selectable_label(k == kind, k.label()).clicked() {
                app.ui.agent_panel = Some(k);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, "x", tl!("Close"), false, 20.0).clicked() {
                *open = false;
            }
            let busy = app.agent.running().is_some();
            let new_chat = ui.add_enabled(!busy, egui::Button::new(RichText::new(tl!("New Chat")).size(12.0).color(t.text_dim)).frame(false));
            if new_chat.clicked() {
                *app.agent.conversation_mut(kind) = Conversation::default();
            }
        });
    });
}

fn transcript(app: &mut VectorcraftApp, ui: &mut egui::Ui, t: &Tokens, kind: AgentKind, height: f32) {
    let running = app.agent.running() == Some(kind);
    egui::ScrollArea::vertical()
        .id_salt("agent-transcript")
        .max_height(height)
        .min_scrolled_height(height)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let conv = app.agent.conversation(kind);
            if conv.entries.is_empty() {
                ui.add_space(8.0);
                let hint = tl!("Describe what to change, for example: “Draw a red circle in the middle of the artboard.”");
                ui.label(RichText::new(hint).color(t.text_disabled).size(12.0));
            }
            for entry in &conv.entries {
                match entry {
                    Entry::User(s) => {
                        ui.add_space(6.0);
                        egui::Frame::NONE.fill(t.row_selected).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(8, 6)).show(
                            ui,
                            |ui| {
                                ui.set_width(ui.available_width());
                                ui.add(egui::Label::new(RichText::new(s).color(t.text_strong)).selectable(true));
                            },
                        );
                    }
                    Entry::Agent(s) => {
                        ui.add_space(4.0);
                        ui.add(egui::Label::new(RichText::new(s).color(t.text)).selectable(true));
                    }
                    Entry::Tool(s) => {
                        ui.add(egui::Label::new(RichText::new(format!("⚙ {s}")).color(t.text_disabled).size(11.0).monospace()).truncate());
                    }
                    Entry::Error(s) => {
                        ui.add_space(2.0);
                        ui.add(egui::Label::new(RichText::new(s).color(t.error).size(12.0)).selectable(true));
                    }
                }
            }
            if running {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(tl!("Working…")).color(t.text_dim).size(12.0));
                });
            }
        });
}

fn composer(app: &mut VectorcraftApp, ui: &mut egui::Ui, t: &Tokens, kind: AgentKind) {
    let busy = app.agent.running().is_some();
    let mut submit = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 84.0).max(80.0);
        let edit = egui::TextEdit::multiline(&mut app.agent.input)
            .id(egui::Id::new("agent-input"))
            .hint_text(format!("{} {}…", tl!("Message"), kind.label()))
            .desired_rows(3)
            .desired_width(width)
            .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter)));
        let response = ui.add(edit);
        // Enter sends, Shift+Enter is a new line.
        if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) {
            submit = true;
        }
        ui.vertical(|ui| {
            if busy {
                if ui.add(egui::Button::new(RichText::new(tl!("Stop")).color(t.error)).min_size(vec2(72.0, 28.0))).clicked() {
                    stop(app);
                }
            } else if widgets::primary_button(ui, tl!("Send")).clicked() {
                submit = true;
            }
        });
    });
    if submit && !busy {
        send(app, ui.ctx(), kind);
        ui.ctx().memory_mut(|m| m.request_focus(egui::Id::new("agent-input")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn events_build_the_conversation_and_done_ends_the_run() {
        let mut c = Conversation::default();
        assert!(!c.apply(AgentEvent::Session("abc".into())));
        assert!(!c.apply(AgentEvent::Text("  ".into())));
        assert!(!c.apply(AgentEvent::Text("Drew a circle.".into())));
        assert!(!c.apply(AgentEvent::Tool("command_run".into())));
        assert!(!c.apply(AgentEvent::Error("oops".into())));
        assert!(c.apply(AgentEvent::Done));
        assert_eq!(c.session.as_deref(), Some("abc"));
        assert_eq!(c.entries, vec![Entry::Agent("Drew a circle.".into()), Entry::Tool("command_run".into()), Entry::Error("oops".into())]);
    }

    #[test]
    fn long_entries_and_conversations_are_capped() {
        let mut c = Conversation::default();
        c.apply(AgentEvent::Text("ž".repeat(MAX_ENTRY_CHARS + 50)));
        let Entry::Agent(s) = &c.entries[0] else { panic!() };
        assert_eq!(s.chars().count(), MAX_ENTRY_CHARS + 1);
        for i in 0..MAX_ENTRIES + 10 {
            c.apply(AgentEvent::Tool(i.to_string()));
        }
        assert_eq!(c.entries.len(), MAX_ENTRIES);
        assert_eq!(c.entries.last(), Some(&Entry::Tool((MAX_ENTRIES + 9).to_string())));
    }

    fn app() -> VectorcraftApp {
        VectorcraftApp::new(vectorcraft_engine::Session::new(), crate::Services::default())
    }

    #[test]
    fn send_without_a_service_reports_an_error_instead_of_failing() {
        let mut app = app();
        app.agent.input = "draw".into();
        send(&mut app, &egui::Context::default(), AgentKind::Codex);
        assert!(matches!(app.agent.conversation(AgentKind::Codex).entries.as_slice(), [Entry::Error(_)]));
        assert!(app.agent.running().is_none());
    }

    #[test]
    fn a_run_streams_into_its_conversation_and_installs_the_control_channel() {
        let mut app = app();
        let (tx, rx) = channel();
        let mut rx = Some(rx);
        app.services.agent = Some(Box::new(move |req: AgentRequest, _ctx| {
            assert_eq!(req.prompt, "draw a star");
            assert_eq!(req.session, None);
            let (_ctl_tx, ctl_rx) = channel();
            Ok(AgentRun { events: rx.take().ok_or("one run")?, cancel: Box::new(|| {}), control: Some(ctl_rx) })
        }));
        app.agent.input = "  draw a star ".into();
        let ctx = egui::Context::default();
        send(&mut app, &ctx, AgentKind::Claude);
        assert_eq!(app.agent.running(), Some(AgentKind::Claude));
        assert!(app.control_rx.is_some());
        assert!(app.agent.input.is_empty());
        tx.send(AgentEvent::Session("s1".into())).unwrap();
        tx.send(AgentEvent::Text("Drew it.".into())).unwrap();
        tx.send(AgentEvent::Done).unwrap();
        poll(&mut app, &ctx);
        assert!(app.agent.running().is_none());
        let c = app.agent.conversation(AgentKind::Claude);
        assert_eq!(c.session.as_deref(), Some("s1"));
        assert_eq!(c.entries, vec![Entry::User("draw a star".into()), Entry::Agent("Drew it.".into())]);
    }

    #[test]
    fn toggle_opens_switches_and_closes() {
        let mut app = app();
        toggle(&mut app, AgentKind::Claude);
        assert_eq!(app.ui.agent_panel, Some(AgentKind::Claude));
        toggle(&mut app, AgentKind::Codex);
        assert_eq!(app.ui.agent_panel, Some(AgentKind::Codex));
        toggle(&mut app, AgentKind::Codex);
        assert_eq!(app.ui.agent_panel, None);
    }
}
