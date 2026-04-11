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
use log::{debug, trace};
use ratatui::{Terminal, layout::Rect};
use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};
use time::OffsetDateTime;

use sshmux::app::App;
use sshmux::color_backend::ColorBackend;
use sshmux::input::{self, Action};
use sshmux::keybindings::KeyBindings;
use sshmux::pane::pane_inner;

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--reset-kb") {
        let kb = KeyBindings::default();
        match kb.save() {
            Ok(()) => {
                eprintln!("keybindings reset to defaults");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("failed to reset keybindings: {e}");
                std::process::exit(1);
            }
        }
    }

    let log_level = std::env::args().find_map(|a| a.strip_prefix("--log=").map(String::from));
    if let Some(level_str) = log_level {
        let level = match level_str.to_lowercase().as_str() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            other => {
                eprintln!(
                    "unknown log level '{}', expected: trace, debug, info, warn, error",
                    other
                );
                std::process::exit(1);
            }
        };
        let now = OffsetDateTime::now_utc();
        let (y, mo, d) = now.to_calendar_date();
        let (h, m, s) = now.to_hms();
        let mo = mo as u8;
        let filename = format!("sshmux-{level_str}-{y:04}{mo:02}{d:02}_{h:02}{m:02}{s:02}.log");
        let file = std::fs::File::create(&filename)?;
        WriteLogger::init(
            level,
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

    let backend = ColorBackend::new(stdout);
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
    let mut first_frame = true;
    loop {
        // Skip sleep during paste accumulation to drain chars fast.
        if !app.paste_accumulating() {
            std::thread::sleep(Duration::from_millis(5));
        }

        app.tick_browsers();

        let needs_draw = app.take_dirty();

        let mut had_event = false;
        let mut quit = false;
        while event::poll(Duration::ZERO)? {
            had_event = true;
            let ev = event::read()?;
            trace!("raw event: {:?}", ev);
            match ev {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

                    if input::handle_key(&mut app, key.code, ctrl, alt, shift, last_area)
                        == Action::Quit
                    {
                        quit = true;
                        break;
                    }
                }

                Event::Mouse(mouse) => {
                    if input::handle_mouse(&mut app, mouse.kind, mouse.column, mouse.row, last_area)
                        == Action::Quit
                    {
                        quit = true;
                        break;
                    }
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
                    app.context_menu = None;
                    app.resize_all(last_area);
                    debug!("resize {}x{}", w, h);
                }
                _ => {}
            }
        }
        if quit {
            break;
        }

        if (needs_draw || had_event || first_frame) && !app.paste_accumulating() {
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

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    Ok(())
}
