// AJAZZ Mouse Key Press Monitor
// Detects forward/backward key press, release, and long-press
// Usage: node key_monitor.js
// Press Ctrl+C to exit

const hid = require('node-hid');

const LONG_PRESS_MS = 300;

// Find all potential input interfaces
const devices = hid.devices().filter(d => d.vendorId === 0x363c && d.productId === 0xed05);

// Key event interfaces to try
const ifaces = devices.filter(d => 
  (d.usagePage === 0x0001 && d.usage === 0x0002) ||  // Mouse
  (d.usagePage === 0x000c && d.usage === 0x0001) ||  // Consumer Control
  (d.usagePage === 0xffdf && d.usage === 0x0001)      // Vendor data
);

console.log('=== AJAZZ Mouse Key Monitor ===');
console.log('Press forward/backward keys on your mouse...');
console.log('Long-press threshold: ' + LONG_PRESS_MS + 'ms');
console.log('Press Ctrl+C to exit.\n');

const KEY_NAMES = {
  '0x08': 'FORWARD',
  '0x04': 'BACKWARD',
};

let pressStart = null;
let currentKey = null;

function formatTime(ms) {
  if (ms < 1000) return ms + 'ms';
  return (ms / 1000).toFixed(2) + 's';
}

function handleData(data) {
  // Check for key event pattern: [0x0C, key_byte, 0xEE/0x00]
  if (data[0] !== 0x0C) return;

  const keyByte = data[1];
  const state = data[2];
  const keyHex = '0x' + keyByte.toString(16).padStart(2, '0');
  const keyName = KEY_NAMES[keyHex];

  if (state === 0xEE) {
    // Key pressed
    if (keyName) {
      pressStart = Date.now();
      currentKey = keyName;
      const time = new Date().toLocaleTimeString();
      console.log(`[${time}] ▼ ${keyName} PRESSED`);
    }
  } else if (state === 0x00) {
    // Key released
    if (currentKey && pressStart) {
      const duration = Date.now() - pressStart;
      const isLong = duration >= LONG_PRESS_MS;
      const time = new Date().toLocaleTimeString();
      const type = isLong ? '🔴 LONG PRESS' : '🟢 SHORT PRESS';
      console.log(`[${time}] ▲ ${currentKey} RELEASED (${formatTime(duration)}) → ${type}`);
      pressStart = null;
      currentKey = null;
    }
  }
}

// Open all possible interfaces and listen
const openedDevs = [];
for (const iface of ifaces) {
  const label = 'UP=0x' + (iface.usagePage || 0).toString(16).padStart(4, '0') +
    ' U=0x' + (iface.usage || 0).toString(16).padStart(4, '0');
  try {
    const dev = new hid.HID(iface.path);
    dev.on('data', handleData);
    dev.on('error', (e) => {
      // silently ignore read errors on wrong interfaces
    });
    openedDevs.push({ dev, label });
    console.log(`Listening on: ${label}`);
  } catch (e) {
    console.log(`Failed to open ${label}: ${e.message}`);
  }
}

if (openedDevs.length === 0) {
  console.log('No interfaces could be opened. Is the mouse connected?');
  process.exit(1);
}

// Graceful shutdown
process.on('SIGINT', () => {
  console.log('\nClosing...');
  for (const { dev } of openedDevs) {
    try { dev.close(); } catch (e) {}
  }
  process.exit(0);
});

// Keep alive
setInterval(() => {}, 1000);
