// Instrument adapter embedded in Goon MIDI Bridge.
(() => {
  'use strict';
  const url = '__MIDI_BRIDGE_URL__';
  if (!document.querySelector('.instruments__piano')) {
    throw new Error('Open an instrument before linking.');
  }
  if (!/^ws:\/\/127\.0\.0\.1:\d+\//.test(url)) {
    throw new Error('Link this instrument from the MIDI Bridge app.');
  }
  window.mirpMidiBridge?.stop();
  const held = new Map();
  const released = new Map();
  const pedals = Array(16).fill(false);
  const state = {
    notes: null,
    volume: null,
    transpose: null,
    keybindToggle: null,
    supportsSustain: true,
    legacyMode: false,
  };
  const socket = new WebSocket(url);
  let stopped = false;
  const panel = document.createElement('details');
  panel.style.cssText = 'position:fixed;right:8px;bottom:8px;z-index:2147483647;' +
    'padding:7px;background:#222;color:#fff;border:1px solid #888;font:12px sans-serif;';
  const status = document.createElement('span');
  status.textContent = 'MIDI: connecting ';
  const summary = document.createElement('summary');
  summary.style.cursor = 'pointer';
  summary.append(status);
  panel.append(summary);
  const disconnect = document.createElement('button');
  disconnect.type = 'button';
  disconnect.textContent = 'Disconnect';
  panel.append(disconnect);
  const channels = document.createElement('fieldset');
  channels.style.cssText = 'margin:7px 0 0;padding:6px;border:1px solid #666;';
  const legend = document.createElement('legend');
  legend.textContent = 'Channels';
  channels.append(legend);
  const channelCheckboxes = [];
  const channelNames = [];
  for (const [label, selected] of [['All', true], ['None', false]]) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    button.addEventListener('click', () => {
      channelCheckboxes.forEach((checkbox, index) => {
        if (checkbox.checked && !selected) panicMidi(index);
        checkbox.checked = selected;
      });
    });
    channels.append(button);
  }
  const grid = document.createElement('div');
  grid.style.cssText = 'display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:4px 10px;margin-top:5px;width:min(500px,calc(100vw - 48px));';
  for (let i = 0; i < 16; i++) {
    const label = document.createElement('label');
    label.style.cssText = 'display:flex;align-items:center;gap:3px;cursor:pointer;';
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.checked = true;
    checkbox.setAttribute('aria-label', `Channel ${i + 1}`);
    checkbox.addEventListener('change', () => {
      if (!checkbox.checked) panicMidi(i);
    });
    const name = document.createElement('span');
    name.style.cssText = 'overflow:hidden;text-overflow:ellipsis;white-space:nowrap;';
    name.textContent = `${i + 1} · ${i === 9 ? 'Drums' : 'Unspecified'}`;
    label.append(checkbox, name);
    grid.append(label);
    channelCheckboxes.push(checkbox);
    channelNames.push(name);
  }
  channels.append(grid);
  panel.append(channels);
  document.body.append(panel);

  function enabled() {
    const indicator = document.querySelector('.instrument__keybind-indicator');
    return indicator && getComputedStyle(indicator).backgroundColor === 'rgb(27, 155, 55)';
  }
  function sendAction(action, payload = {}) {
    window.Byond?.sendMessage?.(`act/${action}`, payload);
  }
  function updateState(type, payload) {
    if (stopped || type !== 'update' || !payload?.data) return;
    const wasSustainEnabled = sustainEnabled();
    const data = payload.data;
    if (Array.isArray(data.notes)) state.notes = data.notes.slice();
    if (Number.isFinite(data.volume)) state.volume = data.volume;
    if (Number.isFinite(data.transpose)) state.transpose = data.transpose;
    if (typeof data.keybindToggle === 'boolean') state.keybindToggle = data.keybindToggle;
    else if (data.keybindToggle === 0 || data.keybindToggle === 1) state.keybindToggle = Boolean(data.keybindToggle);
    if (typeof data.supportsSustain === 'boolean') state.supportsSustain = data.supportsSustain;
    else if (data.supportsSustain === 0 || data.supportsSustain === 1)
      state.supportsSustain = Boolean(data.supportsSustain);
    if (typeof data.legacyMode === 'boolean') state.legacyMode = data.legacyMode;
    else if (data.legacyMode === 0 || data.legacyMode === 1)
      state.legacyMode = Boolean(data.legacyMode);
    if (wasSustainEnabled !== sustainEnabled()) panicMidi();
    if (state.keybindToggle === false) panicMidi();
  }
  if (window.Byond?.subscribe) {
    window.Byond.subscribe(updateState);
    window.Byond.sendMessage?.('ready');
  }
  function ready() {
    return Array.isArray(state.notes) && Number.isFinite(state.volume) &&
      Number.isFinite(state.transpose) && typeof state.keybindToggle === 'boolean';
  }
  function noteIndexForMidi(midiNote) {
    if (!Number.isInteger(midiNote) || midiNote < 0 || midiNote > 127) return -1;
    const names = ['c', 'c#', 'd', 'd#', 'e', 'f', 'f#', 'g', 'g#', 'a', 'a#', 'b'];
    const wanted = `${names[midiNote % 12]}${Math.floor(midiNote / 12) - 1}`;
    return state.notes.findIndex((note) => note.replace('-', '#').toLowerCase() === wanted);
  }
  function sustainEnabled() {
    return state.supportsSustain && !state.legacyMode;
  }
  function pruneReleased() {
    const cutoff = Date.now() - 15000;
    for (const [voice, note] of released) {
      if (note.timestamp < cutoff) released.delete(voice);
    }
  }
  function release(voice, immediate = false) {
    const note = held.get(voice);
    if (!note) return;
    held.delete(voice);
    sendAction('stop_note', immediate ? { voice, immediate: true } : { voice });
    if (!immediate) released.set(voice, { channel: note.channel, timestamp: Date.now() });
  }
  function releaseChannel(channel, immediate = false) {
    for (const [voice, note] of Array.from(held.entries())) {
      if (note.channel === channel) release(voice, immediate);
    }
  }
  function releaseSustained(channel) {
    for (const [voice, note] of Array.from(held.entries())) {
      if (note.channel === channel && note.released) release(voice);
    }
  }
  function panicMidi(channel) {
    pruneReleased();
    if (channel === undefined) {
      sendAction('stop_notes', {});
      held.clear();
      released.clear();
      pedals.fill(false);
    } else {
      releaseChannel(channel, true);
      for (const [voice, note] of Array.from(released.entries())) {
        if (note.channel === channel) {
          sendAction('stop_note', { voice, immediate: true });
          released.delete(voice);
        }
      }
      pedals[channel] = false;
    }
  }
  function channelAllowed(number) {
    return Number.isInteger(number) && number >= 0 && number <= 15 &&
      channelCheckboxes[number].checked;
  }
  socket.addEventListener('message', ({ data }) => {
    pruneReleased();
    let event;
    try { event = JSON.parse(data); } catch { return; }
    if (event.type === 'channels') {
      if (!Array.isArray(event.names) || event.names.length !== 16 ||
          !event.names.every(name => typeof name === 'string' && name.length <= 128)) return;
      event.names.forEach((name, index) => {
        channelNames[index].textContent = `${index + 1} · ${name}`;
        channelNames[index].title = name;
      });
      return;
    }
    if (event.type === 'panic') {
      if (event.channel !== undefined && !channelAllowed(event.channel)) return;
      panicMidi(event.channel);
      return;
    }
    if (event.type === 'pedal') {
      if (!channelAllowed(event.channel) || typeof event.pedal !== 'boolean' ||
          !Number.isInteger(event.pedal_value) || event.pedal_value < 0 ||
          event.pedal_value > 127 || (event.pedal_value >= 64) !== event.pedal) return;
      pedals[event.channel] = event.pedal;
      if (!sustainEnabled()) {
        pedals[event.channel] = false;
        releaseSustained(event.channel);
      } else if (!pedals[event.channel]) {
        releaseSustained(event.channel);
      }
      return;
    }
    if (event.type === 'note_off') {
      if (!channelAllowed(event.channel) || typeof event.voice !== 'string') return;
      const note = held.get(event.voice);
      if (!note || note.channel !== event.channel) return;
      note.released = true;
      if (!sustainEnabled() || !pedals[event.channel]) release(event.voice);
      return;
    }
    if (event.type !== 'note_on' || !enabled() || !ready() || !state.keybindToggle || held.has(event.voice)) return;
    if (!channelAllowed(event.channel)) return;
    if (!Number.isInteger(event.note) || event.note < 0 || event.note > 127 ||
        typeof event.voice !== 'string' || event.voice.length === 0 || event.voice.length > 48) return;
    if (!Number.isInteger(event.velocity) || event.velocity <= 0 || event.velocity > 127 ||
        typeof event.pedal !== 'boolean') return;
    const pedalValue = event.pedal_value;
    if (!Number.isInteger(pedalValue) || pedalValue < 0 || pedalValue > 127 ||
        (pedalValue >= 64) !== event.pedal) return;
    const index = noteIndexForMidi(event.note);
    const note = index + state.transpose;
    if (index < 0 || note < 0 || note >= state.notes.length) return;
    pedals[event.channel] = event.pedal;
    if (!sustainEnabled()) {
      pedals[event.channel] = false;
    }
    held.set(event.voice, { channel: event.channel, released: false });
    const payload = {
      note,
      volume: state.volume,
      voice: event.voice,
      velocity: event.velocity,
    };
    if (sustainEnabled() && event.pedal) payload.resonant = pedalValue >= 65;
    sendAction('start_note', payload);
  });
  socket.addEventListener('open', () => {
    status.textContent = 'MIDI: connected ';
  });
  socket.addEventListener('close', () => {
    if (!stopped) panicMidi();
    status.textContent = 'MIDI: disconnected ';
  });
  socket.addEventListener('error', () => { status.textContent = 'MIDI: connection failed '; });
  let wasEnabled = true;
  const timer = setInterval(() => {
    if (!document.querySelector('.instruments__piano')) stop();
    else if (!enabled() && wasEnabled) { panicMidi(); wasEnabled = false; }
    else if (enabled()) wasEnabled = true;
  }, 100);
  function stop() {
    if (stopped) return;
    stopped = true;
    panicMidi();
    clearInterval(timer);
    socket.close();
    panel.remove();
    window.removeEventListener('pagehide', stop);
  }
  disconnect.addEventListener('click', stop);
  window.addEventListener('pagehide', stop);
  window.mirpMidiBridge = { stop };
})();
