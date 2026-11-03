use super::{Location, Meteogram};
use crate::keymap::KeyEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u32);

#[derive(Debug)]
pub enum ProgramState {
    Loading {
        request_id: RequestId,
        location: Location,
    },
    Ready {
        bitmap: Meteogram,
        scroll_x: u16,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug)]
pub enum Event {
    Completed {
        request_id: RequestId,
        result: Result<Meteogram, String>,
    },
    ScrollLeft,
    ScrollRight,
    Exit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Fetch {
        request_id: RequestId,
        location: Location,
    },
    Redraw,
    Exit,
}

pub struct MeteoProgram {
    state: ProgramState,
}

impl MeteoProgram {
    pub fn start(location: Location, request_id: RequestId) -> (Self, Effect) {
        let effect = Effect::Fetch {
            request_id,
            location: location.clone(),
        };
        (
            Self {
                state: ProgramState::Loading {
                    request_id,
                    location,
                },
            },
            effect,
        )
    }

    pub fn state(&self) -> &ProgramState {
        &self.state
    }

    /// Translate this program's direct controls into state transitions.
    /// Meteo has no text input, so its letter and Backspace keys do not need
    /// the shell's usual modifiers.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let event = match key {
            KeyEvent::Char('y' | 'Y') | KeyEvent::ScrollUp => Event::ScrollLeft,
            KeyEvent::Char('h' | 'H') | KeyEvent::ScrollDown => Event::ScrollRight,
            KeyEvent::Backspace | KeyEvent::Cancel | KeyEvent::Enter => Event::Exit,
            _ => return None,
        };
        self.update(event)
    }

    pub fn update(&mut self, event: Event) -> Option<Effect> {
        match event {
            Event::Completed { request_id, result } => {
                let expected = match &self.state {
                    ProgramState::Loading { request_id, .. } => *request_id,
                    _ => return None,
                };
                if request_id != expected {
                    return None;
                }
                self.state = match result {
                    Ok(bitmap) => ProgramState::Ready {
                        bitmap,
                        scroll_x: 0,
                    },
                    Err(message) => ProgramState::Failed { message },
                };
                Some(Effect::Redraw)
            }
            Event::ScrollLeft => {
                if let ProgramState::Ready { scroll_x, .. } = &mut self.state {
                    let next = scroll_x.saturating_sub(174);
                    if next != *scroll_x {
                        *scroll_x = next;
                        return Some(Effect::Redraw);
                    }
                }
                None
            }
            Event::ScrollRight => {
                if let ProgramState::Ready { bitmap, scroll_x } = &mut self.state {
                    let next = scroll_x.saturating_add(174).min(bitmap.max_scroll_x());
                    if next != *scroll_x {
                        *scroll_x = next;
                        return Some(Effect::Redraw);
                    }
                }
                None
            }
            Event::Exit => Some(Effect::Exit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteo::default_location;

    #[test]
    fn ignores_stale_completion_and_clamps_page_scroll() {
        let (mut program, _) = MeteoProgram::start(default_location(), RequestId(4));
        assert!(program
            .update(Event::Completed {
                request_id: RequestId(3),
                result: Err("old".into())
            })
            .is_none());
        let bitmap = Meteogram::white();
        assert_eq!(
            program.update(Event::Completed {
                request_id: RequestId(4),
                result: Ok(bitmap)
            }),
            Some(Effect::Redraw)
        );
        assert_eq!(program.update(Event::ScrollRight), Some(Effect::Redraw));
        match program.state() {
            ProgramState::Ready { scroll_x, .. } => assert_eq!(*scroll_x, 130),
            _ => panic!("expected ready"),
        }
        assert_eq!(program.update(Event::ScrollRight), None);
        assert_eq!(program.update(Event::ScrollLeft), Some(Effect::Redraw));
    }

    #[test]
    fn direct_keys_scroll_and_backspace_exits() {
        let (mut program, _) = MeteoProgram::start(default_location(), RequestId(1));
        program.update(Event::Completed {
            request_id: RequestId(1),
            result: Ok(Meteogram::white()),
        });

        assert_eq!(
            program.handle_key(KeyEvent::Char('h')),
            Some(Effect::Redraw)
        );
        assert!(matches!(
            program.state(),
            ProgramState::Ready { scroll_x: 130, .. }
        ));
        assert_eq!(
            program.handle_key(KeyEvent::Char('y')),
            Some(Effect::Redraw)
        );
        assert!(matches!(
            program.state(),
            ProgramState::Ready { scroll_x: 0, .. }
        ));
        assert_eq!(program.handle_key(KeyEvent::Backspace), Some(Effect::Exit));
    }
}
