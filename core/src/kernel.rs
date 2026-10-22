//! The kernel: all OS state and logic, pure, driven one event at a time
//! (`docs/os-design.md`, "The kernel").
//!
//! The device's main loop is: wait for an event (or the deadline), call
//! [`Kernel::handle`], carry out the [`Effect`]s. Hardware is reached only
//! through ports (today [`SpiPort`]); the shell's programs still get the
//! blocking drivers in `ExecContext` until they move onto the kernel
//! (migration steps 5–6). That is a known, temporary break of os-design
//! rule 4.
//!
//! What lives here today:
//!   - Lock state ([`Power`]): Alt+L locks and unlocks; other keys are
//!     ignored while locked.
//!   - The terminal and shell, in the body zone; the status bar zone.
//!   - The display service: the kernel owns the framebuffer and sends at
//!     most one draw at a time to the spi task, committing it only when the
//!     draw succeeds (`FrameBuffer::begin_flush` / `end_flush`). The panel's
//!     drive voltages go off after [`PANEL_IDLE_MS`] without a draw, and at
//!     once when locking.
//!
//! Callee invariants:
//!   - At most one spi request in flight; every response is matched to it.
//!   - What the framebuffer believes is on the panel changes only on a
//!     successful draw.
//!   - While locked, keys other than Alt+L change nothing.
//!   - [`Kernel::display_idle`] is true only once nothing is in flight and
//!     nothing is left to draw or power off: safe to sleep.
//!
//! Caller invariants:
//!   - Every spi response is passed back as [`Event::Spi`].
//!   - `handle(Event::Deadline, ..)` is called at [`Kernel::next_deadline`].
//!   - `now` never decreases.

use crate::font::FontSize;
use crate::framebuffer::FrameBuffer;
use crate::keymap::KeyEvent;
use crate::lockscreen;
use crate::power::{Power, PowerAction};
use crate::programs::ExecContext;
use crate::screen::LAYOUT;
use crate::shell::{Shell, ShellAction};
use crate::spi::epaper;
use crate::spi::scheduler::{Request, RequestId, Response};
use crate::spi::Millis;
use crate::status_bar;
use crate::surface::Surface;
use crate::terminal::{Terminal, TerminalAction};
use crate::text_grid::TextGrid;

/// Font for the terminal.
pub const TERM_FONT: FontSize = FontSize::Medium;

/// Drive voltages off after this long without a draw. A partial draw
/// powers the panel on anyway (each one starts with a soft reset), so this
/// costs nothing when typing resumes.
pub const PANEL_IDLE_MS: Millis = 1_000;

/// The spi task, as the kernel sees it.
pub trait SpiPort {
    fn submit(&mut self, request: Request) -> RequestId;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Spi(RequestId, Response),
    /// [`Kernel::next_deadline`] has come.
    Deadline,
}

/// What the device must do (things only it can do).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Just locked: switch the radios off. Sleep while
    /// [`Kernel::is_locked`] and [`Kernel::display_idle`].
    Locked,
    /// Just unlocked.
    Unlocked,
    /// Switch everything off and enter deep sleep, once the display is
    /// idle (the goodbye message is on screen).
    PowerOff,
    /// Switch everything off and enter ship mode, once the display is idle.
    ShipMode,
    /// A voice call was dialled on the old modem driver: run the in-call
    /// loop. Temporary, until `phone` moves onto `CallManager`.
    Call(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Draw,
    PowerOff,
}

/// The framebuffer and the one spi request in flight for it.
struct Display {
    fb: FrameBuffer,
    in_flight: Option<(RequestId, Op)>,
    /// Drive voltages may be on (a draw finished since the last power-off).
    panel_on: bool,
    /// Power off as soon as nothing else is pending (locking).
    power_off_now: bool,
    last_draw: Millis,
}

impl Display {
    fn pump<P: SpiPort>(&mut self, spi: &mut P, now: Millis) {
        if self.in_flight.is_some() {
            return;
        }
        if let Some(flush) = self.fb.begin_flush() {
            let id = spi.submit(Request::Epaper(flush.into()));
            self.in_flight = Some((id, Op::Draw));
        } else if self.panel_on && (self.power_off_now || now >= self.last_draw + PANEL_IDLE_MS) {
            let id = spi.submit(Request::Epaper(epaper::Request::PowerOff));
            self.in_flight = Some((id, Op::PowerOff));
        }
    }

    fn on_response(&mut self, id: RequestId, response: &Response, now: Millis) {
        let Some((expected, op)) = self.in_flight else { return };
        if id != expected {
            log::warn!("kernel: spi response for {id:?}, expecting {expected:?}");
            return;
        }
        self.in_flight = None;
        match op {
            Op::Draw => {
                let ok = *response == Response::Epaper(epaper::Done::Drawn(Ok(())));
                if !ok {
                    log::warn!("kernel: draw failed: {response:?}");
                }
                self.fb.end_flush(ok);
                self.panel_on = true;
                self.last_draw = now;
            }
            Op::PowerOff => {
                self.panel_on = false;
                self.power_off_now = false;
            }
        }
    }

    fn idle(&self) -> bool {
        self.in_flight.is_none() && !self.fb.needs_flush() && !(self.panel_on && self.power_off_now)
    }

    fn deadline(&self) -> Option<Millis> {
        (self.in_flight.is_none() && self.panel_on).then_some(self.last_draw + PANEL_IDLE_MS)
    }
}

pub struct Kernel {
    power: Power,
    term: Terminal,
    shell: Shell,
    grid: TextGrid,
    display: Display,
}

impl Kernel {
    /// Status bar and an empty terminal, ready for the first (full) draw.
    pub fn new() -> Self {
        let grid = TextGrid::new(TERM_FONT, LAYOUT.body.w, LAYOUT.body.h);
        let mut shell = Shell::new();
        shell.set_display_rows(grid.rows());
        let mut k = Kernel {
            power: Power::new(),
            term: Terminal::new("> ", grid.cols(), grid.rows()),
            shell,
            grid,
            display: Display { fb: FrameBuffer::new(), in_flight: None, panel_on: false, power_off_now: false, last_draw: 0 },
        };
        k.render_all();
        k
    }

    /// Add a line of output (e.g. the boot log) and redraw.
    pub fn push_output(&mut self, text: &str) {
        self.term.push_output(text);
        self.render_terminal();
    }

    pub fn is_locked(&self) -> bool {
        self.power.is_locked()
    }

    /// Nothing in flight, nothing left to draw or power off.
    pub fn display_idle(&self) -> bool {
        self.display.idle()
    }

    /// When to deliver [`Event::Deadline`].
    pub fn next_deadline(&self) -> Option<Millis> {
        self.display.deadline()
    }

    /// Handle one event, then send whatever the display needs next.
    pub fn handle<P: SpiPort>(&mut self, event: Event, now: Millis, ctx: &mut ExecContext, spi: &mut P) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            Event::Key(key) => self.on_key(key, ctx, &mut effects),
            Event::Spi(id, response) => self.display.on_response(id, &response, now),
            Event::Deadline => {}
        }
        self.display.pump(spi, now);
        effects
    }

    /// Handle a spi response or a deadline outside `handle` (for the
    /// temporary in-call loop, which has no `ExecContext` at hand).
    pub fn handle_display<P: SpiPort>(&mut self, event: Event, now: Millis, spi: &mut P) {
        match event {
            Event::Spi(id, response) => self.display.on_response(id, &response, now),
            Event::Deadline => {}
            Event::Key(_) => debug_assert!(false, "keys go through handle"),
        }
        self.display.pump(spi, now);
    }

    /// The terminal, for the temporary blocking in-call loop on the device.
    /// Call [`Kernel::redraw`] after changing it.
    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.term
    }

    /// Re-render the terminal and send the display what it needs.
    pub fn redraw<P: SpiPort>(&mut self, spi: &mut P, now: Millis) {
        self.render_terminal();
        self.display.pump(spi, now);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut ExecContext, effects: &mut Vec<Effect>) {
        match self.power.handle_key(key) {
            PowerAction::EnterLock => {
                lockscreen::render(&mut self.display.fb);
                self.display.fb.invalidate(); // full refresh: a clean logo
                self.display.power_off_now = true;
                effects.push(Effect::Locked);
                return;
            }
            PowerAction::ExitLock => {
                self.display.fb.invalidate(); // full refresh: no lockscreen ghost
                self.render_all();
                effects.push(Effect::Unlocked);
                return;
            }
            PowerAction::None if self.power.is_locked() => return,
            PowerAction::None => {}
        }
        if self.shell.is_interactive() {
            let action = self.shell.handle_interactive_key(key, ctx);
            match action {
                ShellAction::Output(output) if !output.is_empty() => {
                    self.term.clear();
                    self.term.push_output(&output);
                }
                ShellAction::Output(_) => {}
                ShellAction::Clear => self.term.clear(),
                other => self.shell_effect(other, effects),
            }
            if !self.shell.is_interactive() {
                self.term.set_show_input(true);
            }
            self.render_terminal();
            return;
        }
        match self.term.handle_key(key) {
            TerminalAction::None => {}
            TerminalAction::Redraw => self.render_terminal(),
            TerminalAction::Execute(cmd) => {
                match self.shell.execute(&cmd, ctx) {
                    ShellAction::Output(output) => {
                        if self.shell.is_interactive() {
                            self.term.set_show_input(false);
                            self.term.clear();
                        }
                        self.term.push_output(&output);
                    }
                    ShellAction::Clear => self.term.clear(),
                    other => self.shell_effect(other, effects),
                }
                self.render_terminal();
            }
        }
    }

    fn shell_effect(&mut self, action: ShellAction, effects: &mut Vec<Effect>) {
        match action {
            ShellAction::PowerOff => {
                self.term.set_show_input(false);
                self.term.push_output("powering off… press the button to wake");
                self.display.power_off_now = true;
                effects.push(Effect::PowerOff);
            }
            ShellAction::ShipMode => {
                self.term.set_show_input(false);
                self.term.push_output("ship mode: connect USB to wake");
                self.display.power_off_now = true;
                effects.push(Effect::ShipMode);
            }
            ShellAction::CallActive(number) => {
                self.term.set_show_input(false);
                self.term.push_output(&format!("calling {number}..."));
                self.term.push_output(crate::call::HELP);
                effects.push(Effect::Call(number));
            }
            ShellAction::Output(_) | ShellAction::Clear => unreachable!("handled by the caller"),
        }
    }

    fn render_all(&mut self) {
        status_bar::render(&mut Surface::new(&mut self.display.fb, LAYOUT.status_bar));
        self.render_terminal();
    }

    fn render_terminal(&mut self) {
        if self.power.is_locked() {
            return; // the lockscreen owns the screen
        }
        let mut body = Surface::new(&mut self.display.fb, LAYOUT.body);
        self.grid.draw_cells(&mut body, &self.term.render());
    }

    #[cfg(test)]
    fn fb(&self) -> &FrameBuffer {
        &self.display.fb
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
