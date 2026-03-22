use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use log::debug;
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Module declarations
// ---------------------------------------------------------------------------

mod app;
mod browser;
mod input;
mod pane;
mod ssh_config;
mod tab;
mod terminal;

use app::App;
use input::Action;
use pane::pane_inner;

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let is_debug = std::env::args().any(|a| a == "--debug");
    if is_debug {
        let now = OffsetDateTime::now_utc();
        let (y, mo, d) = now.to_calendar_date();
        let (h, m, s) = now.to_hms();
        let mo = mo as u8;
        let filename = format!("sshmux-debug-{y:04}{mo:02}{d:02}_{h:02}{m:02}{s:02}.log");
        let file = std::fs::File::create(&filename)?;
        WriteLogger::init(
            LevelFilter::Debug,
            ConfigBuilder::new()
                .set_time_format_custom(time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]:[second]"
                ))
                .build(),
            file,
        )
        .ok();
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let mut last_area = {
        let s = terminal.size()?;
        Rect {
            x: 0,
            y: 0,
            width: s.width,
            height: s.height,
        }
    };
    let mut host_mouse_captured = false;
    let mut first_frame = true;

    loop {
        std::thread::sleep(Duration::from_millis(5));

        app.tick_browsers();

        let needs_draw = app.any_dirty();

        if !host_mouse_captured {
            execute!(terminal.backend_mut(), EnableMouseCapture)?;
            host_mouse_captured = true;
        }

        let mut had_event = false;
        while event::poll(Duration::ZERO)? {
            had_event = true;
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);

                    match input::handle_key(&mut app, key.code, ctrl, alt, last_area) {
                        Action::Continue => {}
                        Action::Quit => {
                            disable_raw_mode()?;
                            execute!(
                                terminal.backend_mut(),
                                LeaveAlternateScreen,
                                DisableMouseCapture,
                                DisableBracketedPaste
                            )?;
                            terminal.show_cursor()?;
                            return Ok(());
                        }
                    }
                }

                Event::Mouse(mouse) => {
                    input::handle_mouse(&mut app, mouse.kind, mouse.column, mouse.row, last_area);
                }

                Event::Paste(text) => {
                    input::handle_paste(&mut app, &text);
                }

                Event::Resize(w, h) => {
                    last_area = Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    };
                    app.resize_all(last_area);
                    debug!("resize {}x{}", w, h);
                }
                _ => {}
            }
        }

        if needs_draw || had_event || first_frame {
            first_frame = false;
            terminal.draw(|f| {
                last_area = f.area();
                if needs_draw {
                    app.resize_all(last_area);
                }
                app.render(last_area, f.buffer_mut());
                let content = pane_inner(last_area);
                if let Some((cx, cy)) = app.tabs[app.selected_tab].focused_cursor(content) {
                    f.set_cursor_position((cx, cy));
                }
            })?;
        }
    }
}
