//! Incremental decoding of terminal input bytes into editing keys.
//!
//! A serial console splits escape sequences and multibyte characters across
//! reads, so decoding is byte-at-a-time and holds partial state between calls.

/// The most CSI parameter bytes a recognized sequence ever carries.
///
/// Real parameters are a handful of digits and semicolons; this bounds how
/// much a noisy serial link can make an abandoned sequence accumulate.
const MAX_PARAMETER_BYTES: usize = 16;

/// One editing intent recovered from the input stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    KillToEnd,
    KillToStart,
    KillWordBack,
    Cancel,
    EndOfFileOrDelete,
    Enter,
}

#[derive(Debug)]
enum State {
    Ground,
    /// An ESC was seen; the next byte selects the sequence family.
    Escape,
    /// Inside a CSI sequence, accumulating parameter bytes.
    ControlSequence,
    /// Accumulating the continuation bytes of a UTF-8 character.
    Utf8,
}

/// A byte-at-a-time terminal input decoder.
#[derive(Debug)]
pub struct KeyDecoder {
    state: State,
    /// Pending CSI parameter bytes, or pending UTF-8 bytes.
    pending: Vec<u8>,
    /// How many bytes the in-progress UTF-8 character still needs.
    utf8_remaining: usize,
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            pending: Vec::new(),
            utf8_remaining: 0,
        }
    }

    /// Consume one input byte, returning a key when a sequence completes.
    pub fn push(&mut self, byte: u8) -> Option<Key> {
        match self.state {
            State::Ground => self.push_ground(byte),
            State::Escape => self.push_escape(byte),
            State::ControlSequence => self.push_control_sequence(byte),
            State::Utf8 => self.push_utf8(byte),
        }
    }

    fn push_ground(&mut self, byte: u8) -> Option<Key> {
        match byte {
            0x1b => {
                self.state = State::Escape;
                None
            }
            0x01 => Some(Key::Home),
            0x03 => Some(Key::Cancel),
            0x04 => Some(Key::EndOfFileOrDelete),
            0x05 => Some(Key::End),
            0x08 | 0x7f => Some(Key::Backspace),
            0x0a | 0x0d => Some(Key::Enter),
            0x0b => Some(Key::KillToEnd),
            0x15 => Some(Key::KillToStart),
            0x17 => Some(Key::KillWordBack),
            // Remaining C0 controls carry no editing meaning here.
            0x00..=0x1f => None,
            0x20..=0x7f => Some(Key::Char(char::from(byte))),
            _ => self.begin_utf8(byte),
        }
    }

    fn begin_utf8(&mut self, byte: u8) -> Option<Key> {
        let width = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            // A continuation byte or an invalid leader with no character to
            // start: drop it rather than emitting replacement text.
            _ => return None,
        };
        self.pending.clear();
        self.pending.push(byte);
        self.utf8_remaining = width - 1;
        self.state = State::Utf8;
        None
    }

    fn push_utf8(&mut self, byte: u8) -> Option<Key> {
        if !(0x80..=0xbf).contains(&byte) {
            // A truncated character: abandon it and reinterpret this byte.
            self.reset();
            return self.push_ground(byte);
        }
        self.pending.push(byte);
        self.utf8_remaining -= 1;
        if self.utf8_remaining > 0 {
            return None;
        }
        let decoded = std::str::from_utf8(&self.pending)
            .ok()
            .and_then(|text| text.chars().next());
        self.reset();
        decoded.map(Key::Char)
    }

    fn push_escape(&mut self, byte: u8) -> Option<Key> {
        match byte {
            b'[' | b'O' => {
                self.pending.clear();
                self.state = State::ControlSequence;
                None
            }
            0x1b => None,
            _ => {
                // Not a sequence family this editor recognizes: abandon the
                // escape and reinterpret the byte as ordinary ground input,
                // the same way a truncated UTF-8 sequence is recovered.
                self.reset();
                self.push_ground(byte)
            }
        }
    }

    fn push_control_sequence(&mut self, byte: u8) -> Option<Key> {
        if byte == 0x1b {
            // A new sequence starts; abandon the unterminated one.
            self.pending.clear();
            self.state = State::Escape;
            return None;
        }
        if byte.is_ascii_digit() || byte == b';' {
            // Real CSI parameters are a handful of bytes; a noisy serial
            // link could otherwise drive this buffer without bound.
            if self.pending.len() >= MAX_PARAMETER_BYTES {
                self.reset();
                return None;
            }
            self.pending.push(byte);
            return None;
        }
        let parameters = std::mem::take(&mut self.pending);
        self.reset();
        match (byte, parameters.as_slice()) {
            (b'A', _) => Some(Key::Up),
            (b'B', _) => Some(Key::Down),
            (b'C', _) => Some(Key::Right),
            (b'D', _) => Some(Key::Left),
            (b'H', _) => Some(Key::Home),
            (b'F', _) => Some(Key::End),
            (b'~', b"1" | b"7") => Some(Key::Home),
            (b'~', b"4" | b"8") => Some(Key::End),
            (b'~', b"3") => Some(Key::Delete),
            // Every other final byte is a sequence this editor ignores.
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.pending.clear();
        self.utf8_remaining = 0;
    }
}
