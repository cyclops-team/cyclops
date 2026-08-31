//! Semantic keys understood by the reusable watch presentation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    /// Ctrl-D: end of input. Sends the reply being composed.
    ///
    /// Enter cannot do it. A pasted newline is indistinguishable from a
    /// typed one once it reaches this layer, so a composer that sent on
    /// Enter would send half a paste. Ctrl-D is the terminal's own
    /// end-of-input convention and no paste contains it as text.
    CtrlD,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    End,
    Home,
    CtrlC,
    /// Left-button press, 0-based screen cell. Releases and drags are
    /// dropped in the decoder: a click is the press, the same rule every
    /// terminal list uses.
    Click {
        x: u16,
        y: u16,
    },
    WheelUp,
    WheelDown,
    /// The terminal is about to send pasted bytes, not keystrokes.
    ///
    /// Everything between these two markers is text somebody copied. It
    /// must never be read as commands: a pasted second line beginning
    /// with `q` would otherwise quit, and a pasted newline would send a
    /// half-written reply. Requires bracketed paste (DECSET 2004), which
    /// `term::Term::enter` turns on.
    PasteStart,
    PasteEnd,
    /// A paste was discarded because it exceeded its byte or time bound.
    PasteRejected,
}
