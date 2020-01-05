//! Stream Pager
//!
//! A pager for streams.
#![warn(missing_docs)]

pub use anyhow::Result;
use anyhow::{anyhow, bail};
use std::io::Read;
use std::time;
use termwiz::caps::{Capabilities, ProbeHintsBuilder};
use termwiz::input::InputEvent;
use termwiz::surface::{change::Change, Position};
use termwiz::terminal::{SystemTerminal, Terminal};
use vec_map::VecMap;

mod buffer;
mod command;
mod display;
mod event;
mod file;
mod line;
mod line_cache;
mod overstrike;
mod progress;
mod prompt;
mod refresh;
mod screen;
mod search;

use event::{Event, EventStream};
use file::File;
use line::Line;
use progress::Progress;

/// The main pager state.
pub struct Pager {
    /// The Terminal.
    term: SystemTerminal,

    /// The Terminal's capabilites.
    caps: Capabilities,

    /// Event Stream to process.
    events: EventStream,

    /// Files to load.
    files: Vec<File>,

    /// Error file mapping.  Maps file indices to the associated error files.
    error_files: VecMap<File>,

    /// Progress indicators to display.
    progress: Option<Progress>,

    /// Whether `sp` should wait to see if enough input is generated to fill
    /// the screen.
    delay_fullscreen: bool,
}

/// Determine terminal capabilities and open the terminal.
fn open_terminal() -> Result<(SystemTerminal, Capabilities)> {
    // Get terminal capabilities from the environment, but disable mouse
    // reporting, as we don't want to change the terminal's mouse handling.
    let caps = Capabilities::new_with_hints(
        ProbeHintsBuilder::new_from_env()
            .mouse_reporting(Some(false))
            .build()
            .map_err(|s| anyhow!(s))?,
    )?;
    if cfg!(unix) && caps.terminfo_db().is_none() {
        bail!("terminfo database not found (is $TERM correct?)");
    }
    let mut term = SystemTerminal::new(caps.clone())?;
    term.set_raw_mode()?;
    Ok((term, caps))
}

impl Pager {
    /// Build a `Pager` using the system terminal.
    pub fn new_using_system_terminal() -> Result<Pager> {
        let (term, caps) = open_terminal()?;
        let events = EventStream::new(term.waker());
        let files = Vec::new();
        let error_files = VecMap::new();
        let progress = None;
        let delay_fullscreen = true;

        Ok(Self {
            term,
            caps,
            events,
            files,
            error_files,
            progress,
            delay_fullscreen,
        })
    }

    /// Add an output file to be paged.
    pub fn add_output_stream(
        &mut self,
        stream: impl Read + Send + 'static,
        title: &str,
    ) -> Result<&mut Self> {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let file = File::new_streamed(index, stream, title, event_sender)?;
        self.files.push(file);
        Ok(self)
    }

    /// Attach an error stream to the previously added output stream.
    pub fn add_error_stream(
        &mut self,
        stream: impl Read + Send + 'static,
        title: &str,
    ) -> Result<&mut Self> {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let file = File::new_streamed(index, stream, title, event_sender)?;
        if let Some(out_file) = self.files.last() {
            self.error_files.insert(out_file.index(), file.clone());
        }
        self.files.push(file);
        Ok(self)
    }

    /// Set the progress stream.
    pub fn set_progress_stream(&mut self, stream: impl Read + Send + 'static) -> &mut Self {
        let event_sender = self.events.sender();
        self.progress = Some(Progress::new(stream, event_sender));
        self
    }

    /// Set whether fullscreen should be delayed.
    pub fn set_delay_fullscreen(&mut self, value: bool) -> &mut Self {
        self.delay_fullscreen = value;
        self
    }

    /// Run Stream Pager.
    pub fn run(self) -> Result<()> {
        run(self)
    }
}

/// Run Stream Pager.
fn run(mut spec: Pager) -> Result<()> {
    // If we are delaying fullsceeen (e.g. because wre are paging stdin without --force)
    // then wait for up to two seconds to see if this is a small amount of
    // output that we don't need to page.
    if spec.delay_fullscreen {
        let load_delay = time::Duration::from_millis(2000);
        if wait_for_screenful(&spec.files, &mut spec.term, &mut spec.events, load_delay)? {
            // The input streams have all completed and they fit on a single
            // screen, just write them out and stop.
            let mut changes = Vec::new();
            for file in spec.files.iter() {
                for i in 0..file.lines() {
                    if let Some(line) = file.with_line(i, |line| Line::new(i, line)) {
                        line.render_full(&mut changes)?;
                        changes.push(Change::CursorPosition {
                            x: Position::Absolute(0),
                            y: Position::Relative(1),
                        });
                    }
                }
                spec.term.render(changes.as_slice())?;
                return Ok(());
            }
        }
    }

    display::start(
        spec.term,
        spec.caps,
        spec.events,
        spec.files,
        spec.error_files,
        spec.progress,
    )
}

/// Poll the event stream, waiting until either the file has finished loading,
/// the file definitely doesn't fit on the screen, or the load delay has passed.
///
/// If the file has finished loading and the file fits on the screen, returns
/// true.  Otherwise, returns false.
fn wait_for_screenful<T: Terminal>(
    files: &[File],
    term: &mut T,
    events: &mut EventStream,
    load_delay: time::Duration,
) -> Result<bool> {
    let load_start = time::Instant::now();
    let mut size = term.get_screen_size()?;
    let mut loaded: Vec<bool> = files.iter().map(|_| false).collect();
    while load_start.elapsed() < load_delay {
        match events.get(term, Some(time::Duration::from_millis(50)))? {
            Some(Event::Loaded(i)) => {
                loaded[i] = true;
                if loaded.iter().all(|l| *l) {
                    if files_fit(files, size.cols, size.rows) {
                        return Ok(true);
                    }
                    break;
                }
            }
            Some(Event::Input(InputEvent::Resized { .. })) => {
                size = term.get_screen_size()?;
            }
            Some(Event::Input(InputEvent::Key(_))) => break,
            _ => {}
        }
        if !files_fit(files, size.cols, size.rows) {
            break;
        }
    }
    Ok(false)
}

/// Returns true if the given files fit on a screen of dimensions `w` x `h`.
fn files_fit(files: &[File], w: usize, h: usize) -> bool {
    let mut wrapped_lines = 0;
    for file in files.iter() {
        let lines = file.lines();
        if wrapped_lines + lines > h {
            return false;
        }
        for i in 0..lines {
            wrapped_lines += file
                .with_line(i, |line| {
                    let line = Line::new(i, line);
                    line.height(w)
                })
                .unwrap_or(0);
            if wrapped_lines > h {
                return false;
            }
        }
    }
    true
}