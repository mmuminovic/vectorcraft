//! The agent panel in the whole window: where it opens, and that typing in it reaches the agent
//! rather than the tool shortcuts.

use std::sync::{Arc, Mutex};

use serde_json::json;
use vectorcraft_engine::Session;

use crate::VectorcraftApp;
use crate::agent_panel::{AgentEvent, AgentRequest, AgentRun};
use crate::state::AgentKind;

/// One headless frame of the whole window with `events`.
fn frame(app: &mut VectorcraftApp, ctx: &egui::Context, events: Vec<egui::Event>) {
    let raw =
        egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), events, ..Default::default() };
    let mut out = ctx.run_ui(raw, |ui| {
        app.logic(ui.ctx());
        app.ui(ui);
    });
    out.textures_delta.clear();
}

fn app(with_doc: bool, sent: Arc<Mutex<Vec<String>>>) -> VectorcraftApp {
    let mut app = VectorcraftApp::new(Session::new(), crate::Services::default());
    if with_doc {
        app.run("file.new", json!({"width": 400, "height": 300})).unwrap();
    }
    app.services.agent = Some(Box::new(move |req: AgentRequest, _ctx| {
        sent.lock().unwrap().push(req.prompt);
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(AgentEvent::Text("ok".into())).unwrap();
        tx.send(AgentEvent::Done).unwrap();
        Ok(AgentRun { events: rx, cancel: Box::new(|| {}), control: None })
    }));
    app.ui.agent_panel = Some(AgentKind::Claude);
    app
}

fn panel_rect(ctx: &egui::Context) -> egui::Rect {
    ctx.memory(|m| m.area_rect(egui::Id::new("agent-panel"))).expect("the agent panel is shown")
}

#[test]
fn the_panel_opens_in_the_document_areas_bottom_right_corner() {
    for with_doc in [false, true] {
        let mut app = app(with_doc, Arc::default());
        let ctx = egui::Context::default();
        for _ in 0..4 {
            frame(&mut app, &ctx, vec![]);
        }
        let area = app.agent.area.expect("the document area is recorded");
        let panel = panel_rect(&ctx);
        assert!(area.contains_rect(panel), "with_doc={with_doc}: panel {panel:?} inside the document area {area:?}");
        assert!((area.right() - panel.right()) < 40.0 && (area.bottom() - panel.bottom()) < 40.0, "with_doc={with_doc}: {panel:?} in {area:?}");
    }
}

#[test]
fn typing_goes_to_the_agent_not_the_tool_shortcuts() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let mut app = app(true, Arc::clone(&sent));
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&mut app, &ctx, vec![]);
    }
    let tool = app.session.tool_id().to_string();
    ctx.memory_mut(|m| m.request_focus(egui::Id::new("agent-input")));
    frame(&mut app, &ctx, vec![]);
    // "p" would pick the Pen tool, "m" the Rectangle tool if the shortcuts saw them.
    let keys = |k: egui::Key| {
        [true, false].map(|pressed| egui::Event::Key { key: k, physical_key: None, pressed, repeat: false, modifiers: Default::default() })
    };
    let mut events: Vec<egui::Event> = keys(egui::Key::P).to_vec();
    events.extend(keys(egui::Key::M));
    events.push(egui::Event::Text("pm".into()));
    frame(&mut app, &ctx, events);
    frame(&mut app, &ctx, vec![]);
    assert_eq!(app.session.tool_id(), tool, "the tool didn't change");
    assert_eq!(app.agent.input, "pm");
    frame(&mut app, &ctx, keys(egui::Key::Enter).to_vec());
    frame(&mut app, &ctx, vec![]);
    frame(&mut app, &ctx, vec![]);
    assert_eq!(*sent.lock().unwrap(), vec!["pm".to_string()]);
    let conv = app.agent.conversation(AgentKind::Claude);
    assert_eq!(conv.entries.len(), 2, "the message and the reply: {:?}", conv.entries);
}
