//! MIDI routing, velocity adjustment, and file timing.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use serde_json::{json, Value};

pub const MAX_VELOCITY_BOOST: f64 = 40.0;
pub const DEFAULT_CHANNELS: [bool; 16] = {
    let mut channels = [true; 16];
    channels[9] = false; // General MIDI uses channel 10 for drums.
    channels
};
const AUTO_TARGET: f64 = 110.0;
const AUTO_LEARN_NOTES: usize = 16;
const VELOCITY_HISTORY_SIZE: usize = 256;
pub(crate) const SUSTAIN_PEDAL: u8 = 64;
pub(crate) const ALL_SOUND_OFF: u8 = 120;
pub(crate) const RESET_CONTROLLERS: u8 = 121;
pub(crate) const ALL_NOTES_OFF: u8 = 123;
static ROUTER_IDS: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VelocityMode {
    Original,
    Manual,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouterMessage {
    pub kind: MessageKind,
    pub channel: u8,
    pub note: u8,
    pub velocity: u8,
    pub control: u8,
    pub value: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    NoteOn,
    NoteOff,
    ControlChange,
    ProgramChange,
}

impl RouterMessage {
    pub fn note_on(channel: u8, note: u8, velocity: u8) -> Self {
        Self {
            kind: MessageKind::NoteOn,
            channel,
            note,
            velocity,
            control: 0,
            value: 0,
        }
    }

    pub fn note_off(channel: u8, note: u8) -> Self {
        Self {
            kind: MessageKind::NoteOff,
            channel,
            note,
            velocity: 0,
            control: 0,
            value: 0,
        }
    }

    pub fn program_change(channel: u8, program: u8) -> Self {
        Self {
            kind: MessageKind::ProgramChange,
            channel,
            note: 0,
            velocity: 0,
            control: 0,
            value: program,
        }
    }

    pub fn control(channel: u8, control: u8, value: u8) -> Self {
        Self {
            kind: MessageKind::ControlChange,
            channel,
            note: 0,
            velocity: 0,
            control,
            value,
        }
    }
}

#[derive(Debug)]
pub enum MidiError {
    Io(std::io::Error),
    Parse(midly::Error),
    InvalidSpeed,
    UnsupportedFormat2,
    UnsupportedTiming,
    InvalidTiming,
    InvalidTempo(u32),
    InvalidTranspose(i16),
    InvalidMinimumVelocity(u8),
    InvalidBoost(f64),
    InvalidOffset(i16),
}

impl std::fmt::Display for MidiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "invalid MIDI: {e}"),
            Self::InvalidSpeed => write!(f, "speed must be finite and greater than zero"),
            Self::UnsupportedFormat2 => write!(f, "MIDI type 2 is not supported"),
            Self::UnsupportedTiming => write!(f, "SMPTE MIDI timing is not supported"),
            Self::InvalidTiming => {
                write!(f, "MIDI ticks-per-quarter-note must be greater than zero")
            }
            Self::InvalidTempo(tempo) => {
                write!(f, "MIDI tempo must be greater than zero, got {tempo}")
            }
            Self::InvalidTranspose(t) => write!(f, "transpose must be -127..127, got {t}"),
            Self::InvalidMinimumVelocity(v) => {
                write!(f, "minimum velocity must be 1..127, got {v}")
            }
            Self::InvalidBoost(b) => write!(f, "velocity boost must be 1..40, got {b}"),
            Self::InvalidOffset(o) => write!(f, "velocity offset must be -64..64, got {o}"),
        }
    }
}

impl std::error::Error for MidiError {}
impl From<std::io::Error> for MidiError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<midly::Error> for MidiError {
    fn from(e: midly::Error) -> Self {
        Self::Parse(e)
    }
}

#[derive(Clone, Debug)]
struct Voice {
    note: u8,
    channel: u8,
}

pub struct MidiRouter {
    channels: [bool; 16],
    channel_names: [String; 16],
    transpose: i16,
    min_velocity: u8,
    counter: u64,
    voice_prefix: String,
    voices: HashMap<String, Voice>,
    note_queues: HashMap<(u8, u8), VecDeque<Option<String>>>,
    pedals: [bool; 16],
    pedal_values: [u8; 16],
    velocity_mode: VelocityMode,
    velocity_boost: f64,
    velocity_offset: i16,
    velocity_history: VecDeque<u8>,
    velocity_count: usize,
    file_velocity: Option<u8>,
    auto_boost: f64,
}

impl Default for MidiRouter {
    fn default() -> Self {
        Self::new(DEFAULT_CHANNELS, 0, 1).expect("valid defaults")
    }
}

impl MidiRouter {
    pub fn new(channels: [bool; 16], transpose: i16, min_velocity: u8) -> Result<Self, MidiError> {
        let mut router = Self {
            channels,
            channel_names: std::array::from_fn(|channel| instrument_name(channel, None).into()),
            transpose: 0,
            min_velocity: 1,
            counter: 0,
            voice_prefix: format!("rust{:x}", ROUTER_IDS.fetch_add(1, Ordering::Relaxed)),
            voices: HashMap::new(),
            note_queues: HashMap::new(),
            pedals: [false; 16],
            pedal_values: [0; 16],
            velocity_mode: VelocityMode::Original,
            velocity_boost: 1.5,
            velocity_offset: 0,
            velocity_history: VecDeque::with_capacity(VELOCITY_HISTORY_SIZE),
            velocity_count: 0,
            file_velocity: None,
            auto_boost: 1.0,
        };
        router.configure_routing(channels, transpose, min_velocity)?;
        Ok(router)
    }

    pub fn channel_names(&self) -> &[String; 16] {
        &self.channel_names
    }

    pub fn channel_info(&self) -> Value {
        json!({ "type": "channels", "names": self.channel_names })
    }

    pub fn describe_file(&mut self, schedule: &[(f64, RouterMessage)]) -> Value {
        self.channel_names = std::array::from_fn(|_| "Unused".into());
        let mut seen = [false; 16];
        for (_, message) in schedule {
            let channel = message.channel as usize;
            if channel >= 16 || seen[channel] {
                continue;
            }
            let program = match message.kind {
                MessageKind::ProgramChange => Some(message.value),
                MessageKind::NoteOn => None,
                _ => continue,
            };
            self.channel_names[channel] = instrument_name(channel, program).into();
            seen[channel] = true;
        }
        self.channel_info()
    }

    pub fn reset_channel_names(&mut self) -> Value {
        self.channel_names = std::array::from_fn(|channel| instrument_name(channel, None).into());
        self.channel_info()
    }

    pub fn configure_routing(
        &mut self,
        channels: [bool; 16],
        transpose: i16,
        min_velocity: u8,
    ) -> Result<(), MidiError> {
        if !(-127..=127).contains(&transpose) {
            return Err(MidiError::InvalidTranspose(transpose));
        }
        if !(1..=127).contains(&min_velocity) {
            return Err(MidiError::InvalidMinimumVelocity(min_velocity));
        }
        self.channels = channels;
        self.transpose = transpose;
        self.min_velocity = min_velocity;
        Ok(())
    }

    pub fn configure_velocity(
        &mut self,
        mode: VelocityMode,
        boost: f64,
        offset: i16,
    ) -> Result<(), MidiError> {
        if !boost.is_finite() || !(1.0..=MAX_VELOCITY_BOOST).contains(&boost) {
            return Err(MidiError::InvalidBoost(boost));
        }
        if !(-64..=64).contains(&offset) {
            return Err(MidiError::InvalidOffset(offset));
        }
        self.velocity_mode = mode;
        self.velocity_boost = boost;
        self.velocity_offset = offset;
        if let Some(value) = self.file_velocity {
            self.auto_boost = Self::boost_for(value);
        } else if self.velocity_history.len() >= AUTO_LEARN_NOTES {
            self.auto_boost = Self::boost_for(Self::reference_velocity(&self.velocity_history));
        }
        Ok(())
    }

    pub fn velocity_status(&self) -> String {
        match self.velocity_mode {
            VelocityMode::Original => "Original MIDI velocity".to_string(),
            VelocityMode::Manual => format!("Manual boost: {:.2}×", self.velocity_boost),
            VelocityMode::Auto
                if self.file_velocity.is_none()
                    && self.velocity_history.len() < AUTO_LEARN_NOTES =>
            {
                format!(
                    "Auto: learning ({}/16 notes), offset {:+}",
                    self.velocity_history.len(),
                    self.velocity_offset
                )
            }
            VelocityMode::Auto => format!(
                "Auto boost: {:.2}×, offset {:+}",
                self.auto_boost, self.velocity_offset
            ),
        }
    }

    /// Route a complete message from a MIDI input callback.
    pub fn handle_bytes(&mut self, bytes: &[u8]) -> Vec<Value> {
        if bytes.len() < 2 {
            return Vec::new();
        }
        let status = bytes[0];
        if status & 0x80 == 0 {
            return Vec::new();
        }
        let channel = status & 0x0f;
        match status & 0xf0 {
            0x80 if bytes.len() >= 3 => {
                self.handle(RouterMessage::note_off(channel, bytes[1] & 0x7f))
            }
            0x90 if bytes.len() >= 3 => {
                let velocity = bytes[2] & 0x7f;
                if velocity == 0 {
                    self.handle(RouterMessage::note_off(channel, bytes[1] & 0x7f))
                } else {
                    self.handle(RouterMessage::note_on(channel, bytes[1] & 0x7f, velocity))
                }
            }
            0xb0 if bytes.len() >= 3 => self.handle(RouterMessage::control(
                channel,
                bytes[1] & 0x7f,
                bytes[2] & 0x7f,
            )),
            0xc0 if bytes.len() >= 2 => {
                self.handle(RouterMessage::program_change(channel, bytes[1] & 0x7f))
            }
            _ => Vec::new(),
        }
    }

    fn accepts_channel(&self, channel: u8) -> bool {
        channel < 16 && self.channels[channel as usize]
    }

    fn transpose_note(&self, note: u8) -> Option<u8> {
        let note = i16::from(note) + self.transpose;
        (0..=127).contains(&note).then_some(note as u8)
    }

    fn reference_velocity(values: &VecDeque<u8>) -> u8 {
        let mut values: Vec<u8> = values.iter().copied().collect();
        values.sort_unstable();
        let percentile_index = (values.len() * 9 / 10).saturating_sub(1);
        values.get(percentile_index).copied().unwrap_or(127)
    }

    fn boost_for(value: u8) -> f64 {
        if f64::from(value) >= AUTO_TARGET {
            1.0
        } else {
            (f64::from(value) / 127.0).ln() / (AUTO_TARGET / 127.0).ln()
        }
    }

    fn record_velocity(&mut self, velocity: u8) {
        self.velocity_history.push_back(velocity);
        if self.velocity_history.len() > VELOCITY_HISTORY_SIZE {
            self.velocity_history.pop_front();
        }
        self.velocity_count += 1;
        if self.velocity_history.len() < AUTO_LEARN_NOTES
            || !self.velocity_count.is_multiple_of(AUTO_LEARN_NOTES)
        {
            return;
        }

        let reference = Self::reference_velocity(&self.velocity_history);
        let target = Self::boost_for(reference);
        self.auto_boost = if self.velocity_count == AUTO_LEARN_NOTES {
            target
        } else {
            self.auto_boost * 0.8 + target * 0.2
        };
    }

    fn velocity(&mut self, source: u8) -> u8 {
        if self.file_velocity.is_none() {
            self.record_velocity(source);
        }
        let boost = match self.velocity_mode {
            VelocityMode::Original => 1.0,
            VelocityMode::Manual => self.velocity_boost,
            VelocityMode::Auto => self.auto_boost,
        };
        let normalized = f64::from(source) / 127.0;
        let mut value = (127.0 * normalized.powf(1.0 / boost)).round() as i16;
        if self.velocity_mode == VelocityMode::Auto {
            value += self.velocity_offset;
        }
        value.clamp(1, 127) as u8
    }

    pub fn prepare_file(&mut self, schedule: &[(f64, RouterMessage)]) {
        let mut values = VecDeque::new();
        for (_, message) in schedule {
            if message.kind == MessageKind::NoteOn
                && message.velocity >= self.min_velocity
                && self.accepts_channel(message.channel)
                && self.transpose_note(message.note).is_some()
            {
                values.push_back(message.velocity);
            }
        }
        let reference = Self::reference_velocity(&values);
        self.file_velocity = Some(reference);
        self.auto_boost = Self::boost_for(reference);
    }

    pub fn finish_file(&mut self) {
        self.file_velocity = None;
        self.auto_boost = if self.velocity_history.len() >= AUTO_LEARN_NOTES {
            Self::boost_for(Self::reference_velocity(&self.velocity_history))
        } else {
            1.0
        };
    }

    pub fn recalibrate_velocity(&mut self) {
        self.velocity_history.clear();
        self.velocity_count = 0;
        self.auto_boost = self.file_velocity.map(Self::boost_for).unwrap_or(1.0);
    }

    pub fn handle(&mut self, message: RouterMessage) -> Vec<Value> {
        let mut output = Vec::new();
        if message.channel > 15 {
            return output;
        }
        match message.kind {
            MessageKind::ControlChange => {
                self.control_change(message.channel, message.control, message.value, &mut output);
            }
            MessageKind::ProgramChange => {
                let name = instrument_name(message.channel as usize, Some(message.value));
                if self.channel_names[message.channel as usize] != name {
                    self.channel_names[message.channel as usize] = name.into();
                    output.push(self.channel_info());
                }
            }
            MessageKind::NoteOn if message.velocity != 0 => self.start_note(message, &mut output),
            MessageKind::NoteOff | MessageKind::NoteOn => self.stop_note(message, &mut output),
        }
        output
    }

    fn start_note(&mut self, message: RouterMessage, output: &mut Vec<Value>) {
        let key = (message.channel, message.note);
        let note = if self.accepts_channel(message.channel) && message.velocity >= self.min_velocity
        {
            self.transpose_note(message.note)
        } else {
            None
        };
        let Some(note) = note else {
            // A filtered strike still needs to consume its matching note-off.
            self.note_queues.entry(key).or_default().push_back(None);
            return;
        };

        self.counter += 1;
        let voice = format!(
            "{}:{}:{}:{}",
            self.voice_prefix, message.channel, message.note, self.counter
        );
        self.note_queues
            .entry(key)
            .or_default()
            .push_back(Some(voice.clone()));
        self.voices.insert(
            voice.clone(),
            Voice {
                note,
                channel: message.channel,
            },
        );
        let velocity = self.velocity(message.velocity);
        let channel = message.channel as usize;
        output.push(json!({
            "type": "note_on",
            "voice": voice,
            "note": note,
            "channel": message.channel,
            "velocity": velocity,
            "pedal": self.pedals[channel],
            "pedal_value": self.pedal_values[channel],
        }));
    }

    fn stop_note(&mut self, message: RouterMessage, output: &mut Vec<Value>) {
        if let Some(queue) = self.note_queues.get_mut(&(message.channel, message.note)) {
            if let Some(Some(voice)) = queue.pop_front() {
                self.release_voice(&voice, output);
            }
        }
    }

    fn release_voice(&mut self, voice: &str, output: &mut Vec<Value>) {
        let Some(note) = self.voices.remove(voice) else {
            return;
        };
        let channel = note.channel as usize;
        output.push(json!({
            "type": "note_off",
            "voice": voice,
            "note": note.note,
            "channel": note.channel,
            "velocity": 0,
            "pedal": self.pedals[channel],
            "pedal_value": self.pedal_values[channel],
        }));
    }

    fn set_pedal(&mut self, channel: u8, value: u8, output: &mut Vec<Value>) {
        self.pedals[channel as usize] = value >= 64;
        self.pedal_values[channel as usize] = value;
        if self.accepts_channel(channel) {
            output.push(json!({
                "type": "pedal",
                "channel": channel,
                "pedal": self.pedals[channel as usize],
                "pedal_value": value,
            }));
        }
    }

    fn control_change(&mut self, channel: u8, control: u8, value: u8, output: &mut Vec<Value>) {
        match control {
            SUSTAIN_PEDAL => self.set_pedal(channel, value, output),
            ALL_SOUND_OFF => {
                self.voices.retain(|_, voice| voice.channel != channel);
                self.note_queues
                    .retain(|(queued_channel, _), _| *queued_channel != channel);
                if self.accepts_channel(channel) {
                    output.push(json!({ "type": "panic", "channel": channel }));
                    self.set_pedal(channel, 0, output);
                }
            }
            ALL_NOTES_OFF => {
                let voices: Vec<String> = self
                    .voices
                    .iter()
                    .filter(|(_, voice)| voice.channel == channel)
                    .map(|(id, _)| id.clone())
                    .collect();
                for voice in voices {
                    self.release_voice(&voice, output);
                }
                self.note_queues
                    .retain(|(queued_channel, _), _| *queued_channel != channel);
            }
            RESET_CONTROLLERS => self.set_pedal(channel, 0, output),
            _ => {}
        }
    }

    pub fn panic(&mut self) -> Vec<Value> {
        let voices: Vec<String> = self.voices.keys().cloned().collect();
        let mut output = Vec::new();
        for voice in voices {
            self.release_voice(&voice, &mut output);
        }
        for channel in 0..16 {
            if self.pedals[channel] && self.accepts_channel(channel as u8) {
                output.push(json!({
                    "type": "pedal",
                    "channel": channel,
                    "pedal": false,
                    "pedal_value": 0,
                }));
            }
        }
        self.note_queues.clear();
        self.pedals = [false; 16];
        self.pedal_values = [0; 16];
        output
    }
}

struct TimedEvent {
    tick: u64,
    track: usize,
    event: FileEvent,
}

enum FileEvent {
    Message(RouterMessage),
    Tempo(u32),
}

pub struct MidiFilePlayer;

impl MidiFilePlayer {
    pub fn read(path: impl AsRef<Path>) -> Result<Vec<u8>, MidiError> {
        Ok(fs::read(path)?)
    }

    /// Merge tracks and tempo changes into a playback schedule, in seconds.
    pub fn schedule(bytes: &[u8], speed: f64) -> Result<Vec<(f64, RouterMessage)>, MidiError> {
        if !speed.is_finite() || speed <= 0.0 {
            return Err(MidiError::InvalidSpeed);
        }
        let smf = Smf::parse(bytes)?;
        if matches!(smf.header.format, midly::Format::Sequential) {
            return Err(MidiError::UnsupportedFormat2);
        }
        let ppqn = match smf.header.timing {
            Timing::Metrical(value) if value.as_int() > 0 => u64::from(value.as_int()),
            Timing::Metrical(_) => return Err(MidiError::InvalidTiming),
            Timing::Timecode(_, _) => return Err(MidiError::UnsupportedTiming),
        };

        let mut events = Vec::new();
        for (track_index, track) in smf.tracks.iter().enumerate() {
            let mut tick = 0u64;
            for event in track {
                tick += u64::from(event.delta.as_int());
                let event = match event.kind {
                    TrackEventKind::Midi { channel, message } => {
                        let channel = channel.as_int();
                        let message = match message {
                            MidiMessage::NoteOn { key, vel } if vel.as_int() != 0 => {
                                Some(RouterMessage::note_on(channel, key.as_int(), vel.as_int()))
                            }
                            MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
                                Some(RouterMessage::note_off(channel, key.as_int()))
                            }
                            MidiMessage::ProgramChange { program } => {
                                Some(RouterMessage::program_change(channel, program.as_int()))
                            }
                            MidiMessage::Controller { controller, value } => {
                                Some(RouterMessage::control(
                                    channel,
                                    controller.as_int(),
                                    value.as_int(),
                                ))
                            }
                            _ => None,
                        };
                        message.map(FileEvent::Message)
                    }
                    TrackEventKind::Meta(MetaMessage::Tempo(value)) => {
                        Some(FileEvent::Tempo(value.as_int()))
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    events.push(TimedEvent {
                        tick,
                        track: track_index,
                        event,
                    });
                }
            }
        }
        events.sort_by_key(|event| (event.tick, event.track));

        let mut seconds = 0.0;
        let mut previous_tick = 0u64;
        let mut micros_per_quarter = 500_000u64;
        let mut result = Vec::new();
        for TimedEvent { tick, event, .. } in events {
            seconds += (tick - previous_tick) as f64 * micros_per_quarter as f64
                / ppqn as f64
                / 1_000_000.0
                / speed;
            previous_tick = tick;
            match event {
                FileEvent::Tempo(tempo) => {
                    if tempo == 0 {
                        return Err(MidiError::InvalidTempo(tempo));
                    }
                    micros_per_quarter = u64::from(tempo);
                }
                FileEvent::Message(message) => result.push((seconds, message)),
            }
        }
        Ok(result)
    }
}

fn instrument_name(channel: usize, program: Option<u8>) -> &'static str {
    if channel == 9 {
        "Drums"
    } else {
        program
            .and_then(|program| GM_PROGRAMS.get(program as usize).copied())
            .unwrap_or("Unspecified")
    }
}

const GM_PROGRAMS: [&str; 128] = [
    "Acoustic Grand Piano",
    "Bright Acoustic Piano",
    "Electric Grand Piano",
    "Honky-tonk Piano",
    "Electric Piano 1",
    "Electric Piano 2",
    "Harpsichord",
    "Clavinet",
    "Celesta",
    "Glockenspiel",
    "Music Box",
    "Vibraphone",
    "Marimba",
    "Xylophone",
    "Tubular Bells",
    "Dulcimer",
    "Drawbar Organ",
    "Percussive Organ",
    "Rock Organ",
    "Church Organ",
    "Reed Organ",
    "Accordion",
    "Harmonica",
    "Tango Accordion",
    "Acoustic Guitar (nylon)",
    "Acoustic Guitar (steel)",
    "Electric Guitar (jazz)",
    "Electric Guitar (clean)",
    "Electric Guitar (muted)",
    "Overdriven Guitar",
    "Distortion Guitar",
    "Guitar Harmonics",
    "Acoustic Bass",
    "Electric Bass (finger)",
    "Electric Bass (pick)",
    "Fretless Bass",
    "Slap Bass 1",
    "Slap Bass 2",
    "Synth Bass 1",
    "Synth Bass 2",
    "Violin",
    "Viola",
    "Cello",
    "Contrabass",
    "Tremolo Strings",
    "Pizzicato Strings",
    "Orchestral Harp",
    "Timpani",
    "String Ensemble 1",
    "String Ensemble 2",
    "Synth Strings 1",
    "Synth Strings 2",
    "Choir Aahs",
    "Voice Oohs",
    "Synth Voice",
    "Orchestra Hit",
    "Trumpet",
    "Trombone",
    "Tuba",
    "Muted Trumpet",
    "French Horn",
    "Brass Section",
    "Synth Brass 1",
    "Synth Brass 2",
    "Soprano Sax",
    "Alto Sax",
    "Tenor Sax",
    "Baritone Sax",
    "Oboe",
    "English Horn",
    "Bassoon",
    "Clarinet",
    "Piccolo",
    "Flute",
    "Recorder",
    "Pan Flute",
    "Blown Bottle",
    "Shakuhachi",
    "Whistle",
    "Ocarina",
    "Lead 1 (square)",
    "Lead 2 (sawtooth)",
    "Lead 3 (calliope)",
    "Lead 4 (chiff)",
    "Lead 5 (charang)",
    "Lead 6 (voice)",
    "Lead 7 (fifths)",
    "Lead 8 (bass + lead)",
    "Pad 1 (new age)",
    "Pad 2 (warm)",
    "Pad 3 (polysynth)",
    "Pad 4 (choir)",
    "Pad 5 (bowed)",
    "Pad 6 (metallic)",
    "Pad 7 (halo)",
    "Pad 8 (sweep)",
    "FX 1 (rain)",
    "FX 2 (soundtrack)",
    "FX 3 (crystal)",
    "FX 4 (atmosphere)",
    "FX 5 (brightness)",
    "FX 6 (goblins)",
    "FX 7 (echoes)",
    "FX 8 (sci-fi)",
    "Sitar",
    "Banjo",
    "Shamisen",
    "Koto",
    "Kalimba",
    "Bag Pipe",
    "Fiddle",
    "Shanai",
    "Tinkle Bell",
    "Agogo",
    "Steel Drums",
    "Woodblock",
    "Taiko Drum",
    "Melodic Tom",
    "Synth Drum",
    "Reverse Cymbal",
    "Guitar Fret Noise",
    "Breath Noise",
    "Seashore",
    "Bird Tweet",
    "Telephone Ring",
    "Helicopter",
    "Applause",
    "Gunshot",
];
