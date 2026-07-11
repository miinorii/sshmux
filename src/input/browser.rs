//! Browser pane key dispatch (shared for SFTP and SCP).

use crossterm::event::KeyCode;
use log::debug;

use super::Action;
use crate::app::App;
use crate::components::browser::{BrowserKeyAction, handle_browser_key};
use crate::pane::Pane;

/// Returns `Some(Action)` if the focused pane is a browser (FileBrowser or SshBrowser).
///
/// SSH password prompts are handled as a special case before the shared path.
pub(super) fn handle_browser_key_dispatch(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<Action> {
    // SSH-specific: password prompt must be handled before the generic browser path.
    if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut()
        && browser.waiting_password
    {
        browser.handle_password_key(code);
        return Some(Action::Continue);
    }

    // Borrow the bindings and the focused browser from disjoint App fields
    // (field access, not accessor methods, so the borrows can coexist).
    let browser_bindings = &app.keybindings.browser;
    let selected_tab = app.selected_tab;
    let browser = app
        .tabs
        .get_mut(selected_tab)?
        .focused_pane_mut()
        .and_then(|p| p.as_browser_mut())?;

    // While connecting/authenticating, forward raw keystrokes.
    if browser.is_connecting() {
        browser.send_connect_key(code);
        return Some(Action::Continue);
    }

    // Log all key events while paste is in progress to diagnose
    // characters that go missing (e.g. backslash on Windows drag-and-drop).
    if !browser.core_mut().paste_buf.is_empty() {
        debug!(
            "paste key event: code={:?} ctrl={} alt={} shift={}",
            code, ctrl, alt, shift
        );
    }

    // Ignore ctrl/alt chars — they are not browser actions and must not
    // trigger paste accumulation.  However, during active paste accumulation
    // let them through: Windows sends backslash as Ctrl+Alt+\ (AltGr) in
    // drag-and-drop paths.
    if (ctrl || alt) && matches!(code, KeyCode::Char(_)) && browser.core_mut().paste_buf.is_empty()
    {
        return Some(Action::Continue);
    }

    match handle_browser_key(browser.core_mut(), code, ctrl, alt, shift, browser_bindings) {
        BrowserKeyAction::Enter => browser.enter(),
        BrowserKeyAction::GoUp => browser.go_up(),
        BrowserKeyAction::Download => browser.download(),
        BrowserKeyAction::Upload => browser.upload(),
        BrowserKeyAction::Delete => browser.delete_focused(),
        BrowserKeyAction::ConfirmDeleteYes => browser.confirm_delete_yes(),
        BrowserKeyAction::Handled => {}
    }

    Some(Action::Continue)
}
