use alacritty_terminal::term::Term;
use iced::widget::canvas::Cache;

#[derive(Clone)]
pub struct EventProxy;

impl alacritty_terminal::event::EventListener for EventProxy {
    fn send_event(&self, _: alacritty_terminal::event::Event) {}
}

pub struct TerminalState {
    pub term: Term<EventProxy>,
    pub pty_sender: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    pub cache: Cache,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16) -> Self {
        use alacritty_terminal::term::{Config, test::TermSize};
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(Config::default(), &size, EventProxy);
        Self {
            term,
            pty_sender: None,
            cache: Cache::new(),
        }
    }

    pub fn process(&mut self, _bytes: &[u8]) {
        // Full VTE parsing implemented in Task 11.
        // For now, just invalidate the render cache so callers compile.
        self.cache.clear();
    }
}
