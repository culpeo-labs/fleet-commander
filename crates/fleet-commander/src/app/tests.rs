
use super::*;
use crate::agent::{AgentStatus, HistoryEntry};
use crate::change_source::{ChangeEvent, ChangeKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fleet_commander_core::session::{MessageStatus, ToolCallStatusKind};
use fleet_commander_core::workspace_fs::WorkspaceFs;
use std::path::PathBuf;

fn app_with_agents() -> App {
    let agents = vec![
        Agent::new("a1", "First"),
        Agent::new("a2", "Second"),
        Agent::new("a3", "Third"),
    ];
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(Config::default(), agents, tx)
}

fn press(code: KeyCode) -> AppEvent {
    AppEvent::Input(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn down_then_up_moves_selection() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Char('j')));
    match app.screen {
        Screen::AgentList { selected } => assert_eq!(selected, 1),
        _ => panic!("expected AgentList"),
    }
    app.handle(press(KeyCode::Char('k')));
    match app.screen {
        Screen::AgentList { selected } => assert_eq!(selected, 0),
        _ => panic!("expected AgentList"),
    }
}

#[test]
fn down_wraps_around() {
    let mut app = app_with_agents();
    for _ in 0..app.agents.len() {
        app.handle(press(KeyCode::Char('j')));
    }
    match app.screen {
        Screen::AgentList { selected } => assert_eq!(selected, 0),
        _ => panic!("expected AgentList"),
    }
}

#[test]
fn activate_enters_session_for_selected_agent() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Char('j')));
    app.handle(press(KeyCode::Enter));
    match &app.screen {
        Screen::AgentSession {
            agent_id,
            side_pane,
            focus,
            ..
        } => {
            assert_eq!(agent_id, "a2");
            assert!(side_pane.is_none(), "side pane should start hidden");
            assert_eq!(*focus, SessionFocus::Conversation);
        }
        other => panic!("expected AgentSession, got {other:?}"),
    }
}

#[test]
fn back_returns_to_agent_list_with_prior_selection() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Char('j')));
    app.handle(press(KeyCode::Char('j')));
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Esc));
    match app.screen {
        Screen::AgentList { selected } => assert_eq!(selected, 2),
        _ => panic!("expected AgentList"),
    }
}

#[test]
fn change_event_outside_session_is_ignored() {
    let mut app = app_with_agents();
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/nonexistent"),
        kind: ChangeKind::Modified,
    }));
    assert!(matches!(app.screen, Screen::AgentList { .. }));
}

#[test]
fn change_event_in_session_opens_diff_side_pane() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/definitely/missing.rs"),
        kind: ChangeKind::Modified,
    }));
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Diff { path, .. }),
            ..
        } => {
            assert_eq!(path, &PathBuf::from("/definitely/missing.rs"));
        }
        other => panic!("expected Diff side pane, got {other:?}"),
    }
}

#[test]
fn change_event_does_not_clobber_user_opened_file_view() {
    // A FileView (user opened a file from the explorer) must survive a
    // background fs change event — otherwise the preview flickers away
    // and is replaced by an auto-diff.
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    if let Screen::AgentSession { side_pane, .. } = &mut app.screen {
        *side_pane = Some(SidePane::FileView {
            path: PathBuf::from("/opened.rs"),
            content: "fn main() {}\n".into(),
            scroll: 0,
        });
    }
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/other/changed.rs"),
        kind: ChangeKind::Modified,
    }));
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::FileView { path, .. }),
            ..
        } => assert_eq!(path, &PathBuf::from("/opened.rs")),
        other => panic!("expected FileView to survive, got {other:?}"),
    }
}

#[test]
fn change_event_does_not_clobber_commands_pane() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    if let Screen::AgentSession { side_pane, .. } = &mut app.screen {
        *side_pane = Some(SidePane::Commands {
            commands: vec![],
            scroll: 0,
        });
    }
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/x.rs"),
        kind: ChangeKind::Modified,
    }));
    assert!(matches!(
        &app.screen,
        Screen::AgentSession {
            side_pane: Some(SidePane::Commands { .. }),
            ..
        }
    ));
}

#[test]
fn change_event_replaces_an_existing_auto_diff() {
    // The auto-diff pane is still allowed to refresh to the latest change.
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/first.rs"),
        kind: ChangeKind::Modified,
    }));
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/second.rs"),
        kind: ChangeKind::Modified,
    }));
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Diff { path, .. }),
            ..
        } => assert_eq!(path, &PathBuf::from("/second.rs")),
        other => panic!("expected refreshed Diff, got {other:?}"),
    }
}

#[test]
fn down_up_scroll_the_focused_side_pane() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    if let Screen::AgentSession {
        side_pane, focus, ..
    } = &mut app.screen
    {
        *side_pane = Some(SidePane::FileView {
            path: PathBuf::from("/big.rs"),
            content: "a\nb\nc\nd\n".into(),
            scroll: 0,
        });
        *focus = SessionFocus::SidePane;
    }
    app.handle(press(KeyCode::Char('j')));
    app.handle(press(KeyCode::Char('j')));
    match &mut app.screen {
        Screen::AgentSession {
            side_pane: Some(pane),
            ..
        } => assert_eq!(*pane.scroll_mut(), 2),
        _ => panic!("expected side pane"),
    }
    app.handle(press(KeyCode::Char('k')));
    match &mut app.screen {
        Screen::AgentSession {
            side_pane: Some(pane),
            ..
        } => assert_eq!(*pane.scroll_mut(), 1),
        _ => panic!("expected side pane"),
    }
}

fn permission_with_options(
    opts: Vec<(&str, &str, &str)>,
) -> (
    PendingPermission,
    tokio::sync::oneshot::Receiver<Option<String>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let perm = PendingPermission {
        tool_name: "write_file".into(),
        options: opts
            .into_iter()
            .map(|(a, b, c)| (a.into(), b.into(), c.into()))
            .collect(),
        reply: Arc::new(Mutex::new(Some(tx))),
        selected: 0,
    };
    (perm, rx)
}

#[test]
fn permission_enter_sends_highlighted_option() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter)); // enter a session
    let (perm, mut rx) = permission_with_options(vec![
        ("id-allow", "Allow once", "allow once"),
        ("id-reject", "Reject", "reject once"),
    ]);
    app.permission_pending = Some(perm);
    // Move highlight to the second option, then confirm.
    app.handle(press(KeyCode::Down));
    app.handle(press(KeyCode::Enter));
    assert!(app.permission_pending.is_none(), "popup should close");
    assert_eq!(rx.try_recv().unwrap(), Some("id-reject".to_string()));
}

#[test]
fn permission_number_key_picks_option_directly() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    let (perm, mut rx) = permission_with_options(vec![
        ("id-allow", "Allow once", "allow once"),
        ("id-always", "Allow always", "allow always"),
    ]);
    app.permission_pending = Some(perm);
    app.handle(press(KeyCode::Char('2')));
    assert!(app.permission_pending.is_none());
    assert_eq!(rx.try_recv().unwrap(), Some("id-always".to_string()));
}

#[test]
fn permission_esc_rejects_with_no_option() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    let (perm, mut rx) = permission_with_options(vec![("id", "Allow", "allow once")]);
    app.permission_pending = Some(perm);
    app.handle(press(KeyCode::Esc));
    assert!(app.permission_pending.is_none());
    assert_eq!(rx.try_recv().unwrap(), None);
}

#[test]
fn permission_popup_captures_input_no_leak_to_buffer() {
    // While the popup is open, typing must not leak into the message
    // input buffer behind it.
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('i'))); // enter insert mode
    let (perm, _rx) = permission_with_options(vec![("id", "Allow", "allow once")]);
    app.permission_pending = Some(perm);
    app.handle(press(KeyCode::Char('h')));
    app.handle(press(KeyCode::Char('i')));
    assert!(
        app.input_buffer.is_empty(),
        "keystrokes leaked into input buffer: {:?}",
        app.input_buffer
    );
    assert!(app.permission_pending.is_some(), "popup should stay open");
}

#[test]
fn dismiss_pane_clears_side_pane_and_refocuses_conversation() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(AppEvent::Change(ChangeEvent {
        path: PathBuf::from("/x"),
        kind: ChangeKind::Modified,
    }));
    app.handle(press(KeyCode::Tab));
    app.handle(press(KeyCode::Char('d')));
    match &app.screen {
        Screen::AgentSession {
            side_pane, focus, ..
        } => {
            assert!(side_pane.is_none());
            assert_eq!(*focus, SessionFocus::Conversation);
        }
        _ => panic!("expected AgentSession"),
    }
}

#[test]
fn dismiss_with_no_side_pane_is_a_noop() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('d')));
    match &app.screen {
        Screen::AgentSession {
            side_pane, focus, ..
        } => {
            assert!(side_pane.is_none());
            assert_eq!(*focus, SessionFocus::Conversation);
        }
        _ => panic!("expected AgentSession"),
    }
}

#[test]
fn quit_only_quits_from_agent_list() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('q')));
    assert!(!app.should_quit, "q in session should not quit the app");
    app.handle(press(KeyCode::Esc));
    app.handle(press(KeyCode::Char('q')));
    assert!(app.should_quit);
}

#[test]
fn agent_output_appends_to_history() {
    let mut app = app_with_agents();
    app.handle(AppEvent::Session(SessionEvent::Output {
        agent_id: "a2".into(),
        line: "hello".into(),
    }));
    let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
    assert_eq!(a2.history.len(), 1);
    assert!(matches!(&a2.history[0], HistoryEntry::Info(s) if s == "hello"));
}

#[test]
fn agent_exited_marks_status_stopped() {
    let mut app = app_with_agents();
    app.handle(AppEvent::Session(SessionEvent::Exited {
        agent_id: "a1".into(),
        code: Some(0),
    }));
    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(a1.status, AgentStatus::Stopped);
}

#[tokio::test]
async fn assistant_message_started_appends_handle() {
    use tokio::sync::watch;
    let mut app = app_with_agents();
    let (text_tx, text_rx) = watch::channel(String::new());
    let (status_tx, status_rx) = watch::channel(MessageStatus::Streaming);
    let message = fleet_commander_core::session::AssistantMessage {
        text: text_rx,
        status: status_rx,
    };
    app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
        agent_id: "a1".into(),
        message,
    }));

    let _ = text_tx.send("Hello".to_string());
    let _ = status_tx.send(MessageStatus::Completed);

    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(a1.status, AgentStatus::Running);
    assert_eq!(a1.history.len(), 1);
    match &a1.history[0] {
        HistoryEntry::Assistant(m) => {
            assert_eq!(*m.text.borrow(), "Hello");
            assert_eq!(*m.status.borrow(), MessageStatus::Completed);
        }
        _ => panic!("expected assistant entry"),
    }
}

#[test]
fn session_error_appends_to_history() {
    let mut app = app_with_agents();
    app.handle(AppEvent::Session(SessionEvent::Error {
        agent_id: "a2".into(),
        message: "connection lost".into(),
    }));
    let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
    match a2.history.last().unwrap() {
        HistoryEntry::Error(s) => assert!(s.contains("connection lost")),
        other => panic!("expected error entry, got {other:?}"),
    }
    assert_eq!(a2.status, AgentStatus::Error);
}

#[test]
fn insert_action_enters_input_mode() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter)); // enter session
    app.handle(press(KeyCode::Char('i'))); // insert mode
    match &app.screen {
        Screen::AgentSession { input_mode, .. } => assert!(*input_mode),
        _ => panic!("expected AgentSession"),
    }
}

#[test]
fn esc_in_input_mode_cancels_input() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('i')));
    // Type some text
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Char('h'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.input_buffer, "h");
    // Esc cancels
    app.handle(press(KeyCode::Esc));
    match &app.screen {
        Screen::AgentSession { input_mode, .. } => assert!(!*input_mode),
        _ => panic!("expected AgentSession"),
    }
    assert!(app.input_buffer.is_empty());
}

#[test]
fn alt_enter_inserts_newline_without_sending() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('i')));
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
    )));
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::ALT,
    )));
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.input_buffer, "a\nb");
    match &app.screen {
        Screen::AgentSession { input_mode, .. } => assert!(*input_mode),
        _ => panic!("expected AgentSession"),
    }
}

#[test]
fn shift_enter_inserts_newline_without_sending() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('i')));
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::SHIFT,
    )));
    assert_eq!(app.input_buffer, "\n");
}

#[test]
fn plain_enter_sends_and_exits_input_mode() {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('i')));
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    )));
    app.handle(press(KeyCode::Enter));
    assert!(app.input_buffer.is_empty());
    match &app.screen {
        Screen::AgentSession { input_mode, .. } => assert!(!*input_mode),
        _ => panic!("expected AgentSession"),
    }
}

#[test]
fn agent_connected_sets_idle_status() {
    let mut app = app_with_agents();
    app.handle(AppEvent::Session(SessionEvent::Connected {
        agent_id: "a1".into(),
        session_id: Some("sess_test".into()),
    }));
    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(a1.status, AgentStatus::Idle);
    match a1.history.last().unwrap() {
        HistoryEntry::Info(s) => assert!(s.contains("connected")),
        other => panic!("expected info entry, got {other:?}"),
    }
}

#[test]
fn agent_exited_clears_prompt_tx() {
    let mut app = app_with_agents();
    let (tx, _rx) = mpsc::unbounded_channel::<String>();
    app.agents[0].prompt_tx = Some(tx);
    app.handle(AppEvent::Session(SessionEvent::Exited {
        agent_id: "a1".into(),
        code: Some(0),
    }));
    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(a1.status, AgentStatus::Stopped);
    assert!(a1.prompt_tx.is_none());
}

#[tokio::test]
async fn tool_call_started_appends_handle() {
    use tokio::sync::watch;
    let mut app = app_with_agents();
    let (title_tx, title_rx) = watch::channel("read_file".to_string());
    let (status_tx, status_rx) = watch::channel(ToolCallStatusKind::InProgress);
    let call = fleet_commander_core::session::ToolCall {
        id: "call_1".into(),
        title: title_rx,
        status: status_rx,
    };
    app.handle(AppEvent::Session(SessionEvent::ToolCall {
        agent_id: "a1".into(),
        call,
    }));

    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(a1.history.len(), 1);
    match &a1.history[0] {
        HistoryEntry::Tool(tc) => {
            assert_eq!(tc.id, "call_1");
            assert_eq!(*tc.title.borrow(), "read_file");
            assert_eq!(*tc.status.borrow(), ToolCallStatusKind::InProgress);
        }
        _ => panic!("expected tool entry"),
    }

    // Title rewrites + status flips reflect through the handle without
    // any extra history mutation.
    let _ = title_tx.send("read_file completed".to_string());
    let _ = status_tx.send(ToolCallStatusKind::Completed);

    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(a1.history.len(), 1);
    match &a1.history[0] {
        HistoryEntry::Tool(tc) => {
            assert_eq!(*tc.title.borrow(), "read_file completed");
            assert_eq!(*tc.status.borrow(), ToolCallStatusKind::Completed);
        }
        _ => panic!("expected tool entry"),
    }
}

// ─── scrolling ────────────────────────────────────────────────────────

/// Drive the app into a session screen with a known scroll offset so we
/// can verify how events / key actions mutate it.
fn app_in_session(scroll: usize) -> App {
    let mut app = app_with_agents();
    app.screen = Screen::AgentSession {
        agent_id: "a1".into(),
        focus: super::SessionFocus::Conversation,
        side_pane: None,
        scroll,
        input_mode: false,
    };
    app
}

fn current_scroll(app: &App) -> usize {
    match &app.screen {
        Screen::AgentSession { scroll, .. } => *scroll,
        _ => panic!("expected AgentSession"),
    }
}

#[test]
fn incoming_event_preserves_manual_scroll() {
    // New "sticky scroll" behaviour: an incoming event must NOT yank the
    // viewport back to the bottom. The user has scrolled to line 5;
    // they stay there until they explicitly press `G`.
    let mut app = app_in_session(5);
    app.handle(AppEvent::Session(SessionEvent::Output {
        agent_id: "a1".into(),
        line: "new line".into(),
    }));
    assert_eq!(
        current_scroll(&app),
        5,
        "manual scroll position must persist across incoming events"
    );
}

#[test]
fn incoming_event_preserves_follow_bottom_sentinel() {
    // Conversely, if the user is already following the bottom
    // (scroll == usize::MAX), the sentinel is preserved and the
    // renderer will naturally show the newest content.
    let mut app = app_in_session(usize::MAX);
    app.handle(AppEvent::Session(SessionEvent::Output {
        agent_id: "a1".into(),
        line: "new line".into(),
    }));
    assert_eq!(current_scroll(&app), usize::MAX);
}

#[test]
fn incoming_event_for_other_agent_does_not_change_scroll() {
    let mut app = app_in_session(5);
    // Viewing a1, event arrives for a2.
    app.handle(AppEvent::Session(SessionEvent::Output {
        agent_id: "a2".into(),
        line: "new line".into(),
    }));
    assert_eq!(
        current_scroll(&app),
        5,
        "scroll for a1 must not move when a2 receives content"
    );
}

#[test]
fn repaint_event_preserves_scroll() {
    // Repaint events exist to wake the event loop when a tracked
    // handle ticks; they must not disturb the user's scroll position.
    let mut app = app_in_session(3);
    app.handle(AppEvent::Repaint);
    assert_eq!(current_scroll(&app), 3);
}

#[test]
fn down_action_increments_scroll() {
    let mut app = app_in_session(0);
    app.handle(press(KeyCode::Char('j')));
    assert_eq!(current_scroll(&app), 1);
    app.handle(press(KeyCode::Char('j')));
    assert_eq!(current_scroll(&app), 2);
}

#[test]
fn up_action_saturates_at_zero() {
    let mut app = app_in_session(1);
    app.handle(press(KeyCode::Char('k')));
    assert_eq!(current_scroll(&app), 0);
    app.handle(press(KeyCode::Char('k')));
    assert_eq!(
        current_scroll(&app),
        0,
        "scrolling up past 0 must saturate, not underflow"
    );
}

#[test]
fn manual_scroll_persists_across_events() {
    // After the sticky-scroll refactor, once the user has scrolled
    // away (`scroll` is finite), no streaming event should move them.
    let mut app = app_in_session(usize::MAX);
    // Seed last_effective_top so the Up handler has a known anchor.
    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    a1.last_effective_top.set(7);
    app.handle(press(KeyCode::Char('k'))); // scroll up
    assert_eq!(
        current_scroll(&app),
        6,
        "Up from follow-bottom must anchor at last_effective_top - 1"
    );
    app.handle(AppEvent::Session(SessionEvent::Output {
        agent_id: "a1".into(),
        line: "interrupt".into(),
    }));
    assert_eq!(
        current_scroll(&app),
        6,
        "incoming event must not disturb manual scroll"
    );
}

#[test]
fn follow_bottom_action_re_engages_follow() {
    // After the user has scrolled away, pressing `G` (Shift-G) re-engages
    // follow-bottom by resetting scroll to the sentinel.
    let mut app = app_in_session(5);
    app.handle(AppEvent::Input(KeyEvent::new(
        KeyCode::Char('G'),
        KeyModifiers::SHIFT,
    )));
    assert_eq!(current_scroll(&app), usize::MAX);
}

// ─── file explorer ────────────────────────────────────────────────────

fn ctrl_e() -> AppEvent {
    AppEvent::Input(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
}

#[test]
fn ctrl_e_toggles_the_explorer_pane() {
    let mut app = app_in_session(0);
    assert!(!app.explorer.open);
    app.handle(ctrl_e());
    assert!(app.explorer.open, "Ctrl+E must open the explorer");
    // Focus follows the new pane so arrows immediately navigate it.
    match &app.screen {
        Screen::AgentSession { focus, .. } => {
            assert_eq!(*focus, SessionFocus::Explorer);
        }
        _ => panic!("expected AgentSession"),
    }
    app.handle(ctrl_e());
    assert!(!app.explorer.open, "Ctrl+E must close the explorer");
}

#[test]
fn esc_unfocuses_explorer_without_closing_it() {
    let mut app = app_in_session(0);
    app.handle(ctrl_e());
    app.handle(press(KeyCode::Esc));
    match &app.screen {
        Screen::AgentSession { focus, .. } => {
            assert_eq!(*focus, SessionFocus::Conversation);
        }
        _ => panic!("expected AgentSession"),
    }
    assert!(
        app.explorer.open,
        "Esc from explorer focus must keep the pane open"
    );
}

#[test]
fn dot_toggles_show_ignored_when_explorer_focused() {
    let mut app = app_in_session(0);
    app.handle(ctrl_e());
    assert!(!app.explorer.show_ignored);
    app.handle(press(KeyCode::Char('.')));
    assert!(app.explorer.show_ignored, ". must toggle show_ignored on");
    app.handle(press(KeyCode::Char('.')));
    assert!(!app.explorer.show_ignored);
}

// ─── session rehydration ──────────────────────────────────────────────
//
// During session/load the agent replays prior turns as a sequence of
// SessionEvent::UserMessage and SessionEvent::AssistantMessage events
// (with handles whose status quickly transitions to Completed). The
// app must:
//   - append each entry to history in arrival order;
//   - auto-follow to the bottom after each event so the most recent
//     turn is the one the user sees.

fn replayed_assistant(body: &str) -> fleet_commander_core::session::AssistantMessage {
    use tokio::sync::watch;
    let (text_tx, text_rx) = watch::channel(body.to_string());
    let (status_tx, status_rx) =
        watch::channel(fleet_commander_core::session::MessageStatus::Completed);
    // Senders are dropped after the channels are seeded, which is fine
    // for replayed (terminal) entries — the receiver still yields the
    // last value via `borrow()`.
    let _ = (text_tx, status_tx);
    fleet_commander_core::session::AssistantMessage {
        text: text_rx,
        status: status_rx,
    }
}

fn replayed_user(body: &str) -> fleet_commander_core::session::UserMessage {
    use tokio::sync::watch;
    let (text_tx, text_rx) = watch::channel(body.to_string());
    let (status_tx, status_rx) =
        watch::channel(fleet_commander_core::session::MessageStatus::Completed);
    let _ = (text_tx, status_tx);
    fleet_commander_core::session::UserMessage {
        text: text_rx,
        status: status_rx,
    }
}

#[tokio::test]
async fn session_rehydration_appends_history_in_order_and_follows_bottom() {
    // Start in follow-bottom mode (the default on session entry) so
    // we can verify the sentinel persists across rehydration.
    let mut app = app_in_session(usize::MAX);

    // Simulate session/load replay: a few prior turns.
    let turns = [
        ("first question", "first answer"),
        ("second question", "second answer"),
        ("third question", "third answer"),
    ];
    for (q, a) in turns.iter() {
        app.handle(AppEvent::Session(SessionEvent::UserMessage {
            agent_id: "a1".into(),
            message: replayed_user(q),
        }));
        app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
            agent_id: "a1".into(),
            message: replayed_assistant(a),
        }));
    }

    let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(
        a1.history.len(),
        6,
        "all 6 replayed entries (3 turns) must be in history"
    );

    // Verify order: User, Assistant, User, Assistant, User, Assistant.
    let mut iter = a1.history.iter();
    for (q, a) in turns.iter() {
        match iter.next().unwrap() {
            HistoryEntry::User(u) => assert_eq!(u.text.borrow().as_str(), *q),
            other => panic!("expected User, got {other:?}"),
        }
        match iter.next().unwrap() {
            HistoryEntry::Assistant(m) => assert_eq!(m.text.borrow().as_str(), *a),
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    // Sticky scroll: rehydration events do not move the scroll.
    // The session starts in follow-bottom mode (usize::MAX) and stays
    // there because no manual scroll was performed; the renderer will
    // naturally show the bottom (i.e. the latest turn) — see the
    // `rehydration_renders_latest_turn_visible` UI test for the
    // visible end-to-end behaviour.
    assert_eq!(
        current_scroll(&app),
        usize::MAX,
        "scroll sentinel must be preserved when no user input intervenes"
    );
}

#[tokio::test]
async fn session_rehydration_for_inactive_agent_does_not_move_scroll() {
    let mut app = app_in_session(7);
    // App is viewing a1; rehydration arrives for a2.
    app.handle(AppEvent::Session(SessionEvent::UserMessage {
        agent_id: "a2".into(),
        message: replayed_user("not me"),
    }));
    app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
        agent_id: "a2".into(),
        message: replayed_assistant("nor me"),
    }));
    assert_eq!(
        current_scroll(&app),
        7,
        "scroll for a1 must not move when a2 rehydrates"
    );
    // But a2's history must have grown.
    let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
    assert_eq!(a2.history.len(), 2);
}

fn enter_input_mode(app: &mut App) {
    // Activate first agent then enter insert mode.
    app.handle(press(KeyCode::Enter));
    app.handle(press(KeyCode::Char('i')));
}

fn seed_commands(app: &mut App, agent_id: &str) {
    if let Some(agent) = app.agents.iter_mut().find(|a| a.id == agent_id) {
        agent.available_commands = vec![
            crate::agent::AvailableCommand {
                name: "model".into(),
                description: "Select AI model".into(),
                hint: Some("model".into()),
            },
            crate::agent::AvailableCommand {
                name: "memory".into(),
                description: "Show memory status".into(),
                hint: None,
            },
            crate::agent::AvailableCommand {
                name: "plan".into(),
                description: "Create a plan".into(),
                hint: None,
            },
        ];
    }
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
}

#[test]
fn tab_completes_selected_slash_command_with_trailing_space() {
    let mut app = app_with_agents();
    seed_commands(&mut app, "a1");
    enter_input_mode(&mut app);
    type_str(&mut app, "/me");
    // Selection defaults to 0 → "memory" (only match for "me").
    app.handle(press(KeyCode::Tab));
    assert_eq!(app.input_buffer, "/memory ");
    // After completion, selection should reset.
    assert_eq!(app.slash_selected, 0);
}

#[test]
fn down_and_up_navigate_slash_popover_with_wrap() {
    let mut app = app_with_agents();
    seed_commands(&mut app, "a1");
    enter_input_mode(&mut app);
    // Type just "/" → all three commands match.
    type_str(&mut app, "/");
    assert_eq!(app.slash_selected, 0);
    app.handle(press(KeyCode::Down));
    assert_eq!(app.slash_selected, 1);
    app.handle(press(KeyCode::Down));
    assert_eq!(app.slash_selected, 2);
    // Wrap-around.
    app.handle(press(KeyCode::Down));
    assert_eq!(app.slash_selected, 0);
    // Up from 0 wraps to last.
    app.handle(press(KeyCode::Up));
    assert_eq!(app.slash_selected, 2);
}

#[test]
fn typing_after_completion_does_not_reopen_popover_in_argument_mode() {
    let mut app = app_with_agents();
    seed_commands(&mut app, "a1");
    enter_input_mode(&mut app);
    type_str(&mut app, "/mo");
    // Popover is open (matches: "model").
    assert!(app.slash_matches_for("a1").is_some());
    // After Tab, buffer is "/model " — popover closed because of the
    // trailing space (argument mode).
    app.handle(press(KeyCode::Tab));
    assert_eq!(app.input_buffer, "/model ");
    assert!(app.slash_matches_for("a1").is_none());
    // Typing into the argument doesn't reopen.
    type_str(&mut app, "gpt-5");
    assert!(app.slash_matches_for("a1").is_none());
}

/// A minimal remote [`WorkspaceFs`] double for the explorer-upgrade tests.
#[derive(Debug)]
struct FakeRemoteFs {
    root: PathBuf,
}

impl WorkspaceFs for FakeRemoteFs {
    fn root_display(&self) -> &std::path::Path {
        &self.root
    }
    fn list_dir(
        &self,
        _rel: &std::path::Path,
    ) -> std::io::Result<Vec<fleet_commander_core::workspace_fs::DirEntry>> {
        Ok(Vec::new())
    }
    fn read_file(&self, _rel: &std::path::Path) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn git_branch(&self) -> Option<String> {
        None
    }
    fn git_status(
        &self,
        _include_ignored: bool,
    ) -> Result<
        std::collections::HashMap<PathBuf, fleet_commander_core::git::StatusKind>,
        fleet_commander_core::git::StatusError,
    > {
        Ok(std::collections::HashMap::new())
    }
    fn is_remote(&self) -> bool {
        true
    }
}

fn app_with_container_agent(ws: &str) -> App {
    let agent = Agent::new("a1", "First").with_workspace(PathBuf::from(ws));
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(Config::default(), vec![agent], tx)
}

#[tokio::test]
async fn container_ready_stores_info_on_agent() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(AppEvent::Session(SessionEvent::ContainerReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        remote_user: "vscode".into(),
        remote_workspace_folder: "/workspaces/repo".into(),
    }));
    let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
    let info = agent.container.as_ref().expect("container info stored");
    assert_eq!(info.container_id, "cid");
    assert_eq!(info.remote_user, "vscode");
    assert_eq!(info.remote_workspace_folder, "/workspaces/repo");
}

#[tokio::test]
async fn explorer_diff_ready_opens_diff_pane() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(press(KeyCode::Enter));
    app.handle(AppEvent::ExplorerDiffReady {
        agent_id: "a1".into(),
        root: PathBuf::from("/ws/repo"),
        full_path: PathBuf::from("/ws/repo/a.txt"),
        result: Ok("@@ -1 +1 @@\n-a\n+b\n".into()),
    });
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Diff { content, path, .. }),
            ..
        } => {
            assert!(content.contains("+b"), "{content}");
            assert_eq!(path, &PathBuf::from("/ws/repo/a.txt"));
        }
        other => panic!("expected Diff pane, got {other:?}"),
    }
}

#[tokio::test]
async fn explorer_diff_ready_empty_shows_no_changes() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(press(KeyCode::Enter));
    app.handle(AppEvent::ExplorerDiffReady {
        agent_id: "a1".into(),
        root: PathBuf::from("/ws/repo"),
        full_path: PathBuf::from("/ws/repo/clean.txt"),
        result: Ok(String::new()),
    });
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Diff { content, .. }),
            ..
        } => assert_eq!(content, "No changes."),
        other => panic!("expected Diff pane, got {other:?}"),
    }
}

#[tokio::test]
async fn explorer_diff_ready_ignored_for_different_root() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(press(KeyCode::Enter));
    app.handle(AppEvent::ExplorerDiffReady {
        agent_id: "a1".into(),
        root: PathBuf::from("/some/other/root"),
        full_path: PathBuf::from("/some/other/root/a.txt"),
        result: Ok("diff".into()),
    });
    match &app.screen {
        Screen::AgentSession { side_pane, .. } => assert!(side_pane.is_none()),
        other => panic!("expected AgentSession, got {other:?}"),
    }
}

#[tokio::test]
async fn explorer_fs_ready_upgrades_viewed_agent_to_remote() {
    let mut app = app_with_container_agent("/ws/repo");
    // Enter the session so the explorer is on screen and points at the
    // host LocalFs for /ws/repo.
    app.handle(press(KeyCode::Enter));
    app.explorer.open = true;
    assert!(!app.explorer.fs.as_ref().unwrap().is_remote());

    // The container comes up; the agent records its id.
    app.handle(AppEvent::Session(SessionEvent::ContainerReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        remote_user: "vscode".into(),
        remote_workspace_folder: "/workspaces/repo".into(),
    }));

    // The background connect lands with a remote fs for the same root and
    // the same container.
    app.handle(AppEvent::ExplorerFsReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        fs: Arc::new(FakeRemoteFs {
            root: PathBuf::from("/ws/repo"),
        }) as Arc<dyn WorkspaceFs>,
    });
    assert!(
        app.explorer.fs.as_ref().unwrap().is_remote(),
        "explorer should now be backed by the container service"
    );
}

#[test]
fn explorer_fs_ready_ignored_for_different_root() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(press(KeyCode::Enter));
    app.explorer.open = true;

    // A stale upgrade for a different workspace must not clobber the view.
    app.handle(AppEvent::ExplorerFsReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        fs: Arc::new(FakeRemoteFs {
            root: PathBuf::from("/ws/other"),
        }) as Arc<dyn WorkspaceFs>,
    });
    assert!(!app.explorer.fs.as_ref().unwrap().is_remote());
}

#[tokio::test]
async fn explorer_fs_ready_rejected_for_stale_container() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(press(KeyCode::Enter));
    app.explorer.open = true;

    // Agent is currently backed by container "new".
    app.handle(AppEvent::Session(SessionEvent::ContainerReady {
        agent_id: "a1".into(),
        container_id: "new".into(),
        remote_user: "vscode".into(),
        remote_workspace_folder: "/workspaces/repo".into(),
    }));

    // A handshake that started against the OLD container finally lands.
    // It must be dropped, not installed, to avoid binding the explorer to
    // a dead container.
    app.handle(AppEvent::ExplorerFsReady {
        agent_id: "a1".into(),
        container_id: "old".into(),
        fs: Arc::new(FakeRemoteFs {
            root: PathBuf::from("/ws/repo"),
        }) as Arc<dyn WorkspaceFs>,
    });
    assert!(
        !app.explorer.fs.as_ref().unwrap().is_remote(),
        "a fs bound to a stale container must be rejected"
    );
}

#[tokio::test]
async fn agent_branch_ready_applies_for_matching_container() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(AppEvent::Session(SessionEvent::ContainerReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        remote_user: "vscode".into(),
        remote_workspace_folder: "/workspaces/repo".into(),
    }));
    app.handle(AppEvent::AgentBranchReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        branch: Some("feat/x".into()),
    });
    let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(agent.git_branch.as_deref(), Some("feat/x"));
}

#[tokio::test]
async fn agent_branch_ready_rejected_for_stale_container() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(AppEvent::Session(SessionEvent::ContainerReady {
        agent_id: "a1".into(),
        container_id: "new".into(),
        remote_user: "vscode".into(),
        remote_workspace_folder: "/workspaces/repo".into(),
    }));
    // A branch read from the OLD container must not be applied.
    app.handle(AppEvent::AgentBranchReady {
        agent_id: "a1".into(),
        container_id: "old".into(),
        branch: Some("stale".into()),
    });
    let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(agent.git_branch, None);
}

#[tokio::test]
async fn rebuild_downgrades_explorer_off_remote_fs() {
    let mut app = app_with_container_agent("/ws/repo");
    app.handle(press(KeyCode::Enter));
    app.explorer.open = true;

    // Bring the container up and install a remote fs for it.
    app.handle(AppEvent::Session(SessionEvent::ContainerReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        remote_user: "vscode".into(),
        remote_workspace_folder: "/workspaces/repo".into(),
    }));
    app.handle(AppEvent::ExplorerFsReady {
        agent_id: "a1".into(),
        container_id: "cid".into(),
        fs: Arc::new(FakeRemoteFs {
            root: PathBuf::from("/ws/repo"),
        }) as Arc<dyn WorkspaceFs>,
    });
    assert!(app.explorer.fs.as_ref().unwrap().is_remote());

    // Rebuilding clears the container and drops the remote fs back to the
    // host filesystem.
    app.rebuild_current_container();
    assert!(
        !app.explorer.fs.as_ref().unwrap().is_remote(),
        "rebuild must downgrade the explorer off the dead container"
    );
    let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
    assert!(
        agent.container.is_none(),
        "rebuild must clear the agent's container handle"
    );
}

fn hit(path: &str, line: u64, text: &str) -> fleet_commander_core::fleet_protocol::SearchMatch {
    fleet_commander_core::fleet_protocol::SearchMatch {
        path: path.into(),
        line,
        column: 1,
        text: text.into(),
    }
}

/// Enter agent a1's session with an open, running search pane focused.
fn app_with_search_pane(search_id: u64) -> App {
    let mut app = app_with_agents();
    app.handle(press(KeyCode::Enter)); // enter a1's session
    if let Screen::AgentSession {
        side_pane, focus, ..
    } = &mut app.screen
    {
        *side_pane = Some(SidePane::Search {
            query: "needle".into(),
            search_id,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            running: true,
            summary: None,
        });
        *focus = SessionFocus::SidePane;
    }
    app
}

#[test]
fn search_results_append_to_matching_pane() {
    let mut app = app_with_search_pane(7);
    app.handle(AppEvent::SearchResults {
        agent_id: "a1".into(),
        search_id: 7,
        matches: vec![hit("src/a.rs", 1, "a"), hit("src/b.rs", 2, "b")],
    });
    app.handle(AppEvent::SearchResults {
        agent_id: "a1".into(),
        search_id: 7,
        matches: vec![hit("src/c.rs", 3, "c")],
    });
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Search { matches, .. }),
            ..
        } => assert_eq!(matches.len(), 3),
        other => panic!("expected search pane, got {other:?}"),
    }
}

#[test]
fn search_results_for_stale_id_are_dropped() {
    let mut app = app_with_search_pane(7);
    app.handle(AppEvent::SearchResults {
        agent_id: "a1".into(),
        search_id: 99, // does not match the pane's id
        matches: vec![hit("src/a.rs", 1, "a")],
    });
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Search { matches, .. }),
            ..
        } => assert!(matches.is_empty()),
        other => panic!("expected search pane, got {other:?}"),
    }
}

#[test]
fn search_done_clears_running_and_records_summary() {
    let mut app = app_with_search_pane(7);
    app.handle(AppEvent::SearchDone {
        agent_id: "a1".into(),
        search_id: 7,
        summary: fleet_commander_core::fleet_protocol::SearchSummary {
            count: 4,
            truncated: true,
            cancelled: false,
        },
    });
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Search {
                running, summary, ..
            }),
            ..
        } => {
            assert!(!running);
            assert_eq!(summary.as_ref().map(|s| s.count), Some(4));
        }
        other => panic!("expected search pane, got {other:?}"),
    }
}

#[test]
fn down_moves_search_selection_not_scroll() {
    let mut app = app_with_search_pane(7);
    app.handle(AppEvent::SearchResults {
        agent_id: "a1".into(),
        search_id: 7,
        matches: vec![hit("a", 1, "a"), hit("b", 2, "b"), hit("c", 3, "c")],
    });
    app.handle(press(KeyCode::Char('j')));
    app.handle(press(KeyCode::Char('j')));
    // Extra Down must clamp at the last row, not overflow.
    app.handle(press(KeyCode::Char('j')));
    match &app.screen {
        Screen::AgentSession {
            side_pane: Some(SidePane::Search { selected, .. }),
            ..
        } => assert_eq!(*selected, 2),
        other => panic!("expected search pane, got {other:?}"),
    }
}

#[test]
fn activate_search_hit_sets_pending_open_with_line() {
    let mut app = app_with_search_pane(7);
    app.handle(AppEvent::SearchResults {
        agent_id: "a1".into(),
        search_id: 7,
        matches: vec![hit("src/a.rs", 10, "x"), hit("src/b.rs", 42, "y")],
    });
    app.handle(press(KeyCode::Char('j'))); // select the second hit
    app.handle(press(KeyCode::Enter));
    assert_eq!(app.explorer.pending_open, Some(PathBuf::from("src/b.rs")));
    assert_eq!(app.explorer.pending_open_line, Some(42));
}
