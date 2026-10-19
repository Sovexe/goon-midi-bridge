use crate::engine::{
    MessageKind, MidiRouter, RouterMessage, ALL_NOTES_OFF, ALL_SOUND_OFF, RESET_CONTROLLERS,
    SUSTAIN_PEDAL,
};
use serde_json::{json, Value};
use std::{
    sync::{mpsc, Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

enum Command {
    Pause,
    Resume,
    Seek(f64),
    SetSpeed(f64),
    Stop,
}

#[derive(Clone, Copy)]
struct Clock {
    position: f64,
    started: Option<Instant>,
    duration: f64,
    speed: f64,
}

impl Clock {
    fn position(&self) -> f64 {
        (self.position
            + self
                .started
                .map_or(0.0, |start| start.elapsed().as_secs_f64() * self.speed))
        .min(self.duration)
    }
}

pub struct Playback {
    commands: mpsc::Sender<Command>,
    clock: Arc<Mutex<Clock>>,
    worker: Option<JoinHandle<()>>,
}

impl Playback {
    pub fn start(
        schedule: Vec<(f64, RouterMessage)>,
        position: f64,
        speed: f64,
        router: Arc<Mutex<MidiRouter>>,
        output: broadcast::Sender<Value>,
    ) -> Self {
        let duration = schedule.last().map_or(0.0, |(at, _)| *at);
        let clock = Arc::new(Mutex::new(Clock {
            position: position.clamp(0.0, duration),
            started: Some(Instant::now()),
            duration,
            speed,
        }));
        let (commands, receiver) = mpsc::channel();
        let worker_clock = clock.clone();
        let worker = std::thread::spawn(move || {
            let send = |events: Vec<Value>| {
                for event in events {
                    let _ = output.send(event);
                }
            };
            let silence = || {
                send(router.lock().unwrap().panic());
                let _ = output.send(json!({ "type": "panic" }));
            };
            let restore = |next: usize| {
                let mut router = router.lock().unwrap();
                send(vec![router.describe_file(&schedule)]);
                for message in state_at(&schedule[..next]) {
                    send(router.handle(message));
                }
            };

            let position = worker_clock.lock().unwrap().position;
            let mut next = schedule.partition_point(|(at, _)| *at < position);
            if position > 0.0 && position < duration {
                restore(next);
            }
            worker_clock.lock().unwrap().started = Some(Instant::now());
            loop {
                let clock = *worker_clock.lock().unwrap();
                let command = if clock.started.is_none() {
                    receiver.recv().ok()
                } else if let Some((at, _)) = schedule.get(next) {
                    let wait =
                        Duration::from_secs_f64((at - clock.position()).max(0.0) / clock.speed);
                    match receiver.recv_timeout(wait) {
                        Ok(command) => Some(command),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            send(router.lock().unwrap().handle(schedule[next].1));
                            next += 1;
                            continue;
                        }
                    }
                } else {
                    worker_clock.lock().unwrap().position = duration;
                    break;
                };
                match command {
                    Some(Command::Pause) => {
                        let mut clock = worker_clock.lock().unwrap();
                        clock.position = clock.position();
                        clock.started = None;
                        drop(clock);
                        silence();
                    }
                    Some(Command::Resume) => {
                        if clock.started.is_none() {
                            if clock.position < duration {
                                restore(next);
                            }
                            worker_clock.lock().unwrap().started = Some(Instant::now());
                        }
                    }
                    Some(Command::Seek(position)) => {
                        silence();
                        let position = position.clamp(0.0, duration);
                        next = schedule.partition_point(|(at, _)| *at < position);
                        if clock.started.is_some() && position < duration {
                            restore(next);
                        }
                        let mut updated = worker_clock.lock().unwrap();
                        updated.position = position;
                        updated.started = clock.started.map(|_| Instant::now());
                    }
                    Some(Command::SetSpeed(speed)) => {
                        let mut clock = worker_clock.lock().unwrap();
                        clock.position = clock.position();
                        clock.started = clock.started.map(|_| Instant::now());
                        clock.speed = speed;
                    }
                    Some(Command::Stop) | None => break,
                }
            }
            let mut clock = worker_clock.lock().unwrap();
            clock.position = clock.position();
            clock.started = None;
            drop(clock);
            let mut router = router.lock().unwrap();
            send(router.panic());
            router.finish_file();
        });
        Self {
            commands,
            clock,
            worker: Some(worker),
        }
    }

    pub fn position(&self) -> f64 {
        self.clock.lock().unwrap().position()
    }

    pub fn is_paused(&self) -> bool {
        self.clock.lock().unwrap().started.is_none()
    }

    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    pub fn resume(&self) {
        let _ = self.commands.send(Command::Resume);
    }

    pub fn seek(&self, position: f64) {
        if position.is_finite() {
            let _ = self.commands.send(Command::Seek(position));
        }
    }

    pub fn set_speed(&self, speed: f64) {
        if speed.is_finite() && (0.01..=3.0).contains(&speed) {
            let _ = self.commands.send(Command::SetSpeed(speed));
        }
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Rebuild notes and controllers at a seek point from the original MIDI events.
fn state_at(schedule: &[(f64, RouterMessage)]) -> Vec<RouterMessage> {
    let mut pedals = [0; 16];
    let mut programs = [None; 16];
    let mut notes: Vec<(RouterMessage, bool)> = Vec::new();
    for (_, message) in schedule {
        let channel = message.channel as usize;
        if channel >= 16 {
            continue;
        }
        match message.kind {
            MessageKind::NoteOn if message.velocity > 0 => notes.push((*message, false)),
            MessageKind::NoteOff | MessageKind::NoteOn => {
                if let Some(index) = notes.iter().position(|(on, released)| {
                    on.channel == message.channel && on.note == message.note && !released
                }) {
                    if pedals[channel] >= 64 {
                        notes[index].1 = true;
                    } else {
                        notes.remove(index);
                    }
                }
            }
            MessageKind::ProgramChange => programs[channel] = Some(*message),
            MessageKind::ControlChange => match message.control {
                SUSTAIN_PEDAL | RESET_CONTROLLERS => {
                    pedals[channel] = if message.control == SUSTAIN_PEDAL {
                        message.value
                    } else {
                        0
                    };
                    if pedals[channel] < 64 {
                        notes.retain(|(on, released)| on.channel != message.channel || !released);
                    }
                }
                ALL_SOUND_OFF => {
                    notes.retain(|(on, _)| on.channel != message.channel);
                    pedals[channel] = 0;
                }
                ALL_NOTES_OFF => {
                    for (on, released) in &mut notes {
                        if on.channel == message.channel {
                            *released = true;
                        }
                    }
                    if pedals[channel] < 64 {
                        notes.retain(|(on, _)| on.channel != message.channel);
                    }
                }
                _ => {}
            },
        }
    }
    let mut state: Vec<_> = programs.into_iter().flatten().collect();
    for (channel, value) in pedals.into_iter().enumerate() {
        if value > 0 {
            state.push(RouterMessage::control(channel as u8, SUSTAIN_PEDAL, value));
        }
    }
    for (on, released) in notes {
        state.push(on);
        if released {
            state.push(RouterMessage::note_off(on.channel, on.note));
        }
    }
    state
}
