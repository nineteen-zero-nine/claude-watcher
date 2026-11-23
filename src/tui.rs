//! The interactive view: tabs for a session's context, its subagents, the
//! sessions on this machine and the hook configuration.

mod app;
mod view;
mod worker;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event};

use app::App;
pub use worker::Target;

/// How long to wait for a key before looking for fresh data again.
const POLL: Duration = Duration::from_millis(200);

/// Run the interface until the user quits. The terminal is restored on the
/// way out, including after a panic.
pub fn run(project: PathBuf, target: Target, window: u64) -> Result<()> {
    let (commands, updates) = worker::spawn(project, target.clone());
    let mut app = App::new(target, window);
    ratatui::run(|terminal| -> Result<()> {
        while !app.quit {
            for update in updates.try_iter() {
                app.apply(update);
            }
            terminal.draw(|frame| view::draw(frame, &mut app))?;
            if event::poll(POLL)? {
                if let Event::Key(key) = event::read()? {
                    if let Some(command) = app.on_key(key) {
                        // A worker that is gone leaves the last data on screen.
                        let _ = commands.send(command);
                    }
                }
            }
        }
        Ok(())
    })
}
