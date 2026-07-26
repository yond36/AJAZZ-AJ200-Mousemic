// AJAZZ Mouse AI Key Control Script
// Usage: node ai_key_control.js [enable|disable|forward|backward|status]

const hid = require('node-hid');

const REPORT_ID = 0x0B;
const CMD = 0x55;
const SUB_CMD = 0x1A;

// Find AJAZZ command interface
function findDevice() {
  const devices = hid.devices();
  return devices.find(d => 
    d.vendorId === 0x363c && 
    d.usagePage === 0xffa0  // Command interface
  );
}

// Send AI key command to mouse
function sendCommand(mask, enable, fwShortDefault, fwLongDefault, bwShortDefault, bwLongDefault) {
  const devInfo = findDevice();
  if (!devInfo) {
    console.error('ERROR: No AJAZZ device found! Make sure the dongle is plugged in.');
    return false;
  }

  const pid = '0x' + devInfo.productId.toString(16).padStart(4, '0');
  console.log('Device: PID=' + pid + ' Path=' + devInfo.path.split('#')[1]?.split('#')[0]);

  const payload = [
    CMD, SUB_CMD,
    mask, enable,           // key mask, AI enable
    1, 0, 0, 2, 0, 0, 4, 0, 0, 8,
    bwShortDefault, bwLongDefault,
    16,
    fwShortDefault, fwLongDefault,
    32, 0, 0, 64, 0, 0, 128, 0, 0
  ];

  try {
    const dev = new hid.HID(devInfo.path);
    const buf = Buffer.alloc(32);
    buf[0] = REPORT_ID;
    for (let i = 0; i < payload.length; i++) buf[i + 1] = payload[i];
    
    console.log('Data:', buf.toString('hex').match(/.{2}/g).join(' '));
    dev.write(buf);
    dev.close();
    return true;
  } catch(e) {
    console.error('HID write failed:', e.message);
    return false;
  }
}

// ─── COMMAND HANDLERS ───

function enableBoth() {
  console.log('\n=== ENABLING both forward & backward AI keys ===');
  const mask = 0x10 | 0x08;  // forward(16) + backward(8)
  // 0 = AI custom behavior, 2 = default forward, 1 = default backward
  if (sendCommand(mask, 1, 0, 0, 0, 0)) {
    console.log('SUCCESS! Both AI keys enabled.');
    console.log('  Forward: long-press=Voice Input, short-press=Enter');
    console.log('  Backward: long-press=Voice Translate, short-press=AI Agent');
  }
}

function disableAll() {
  console.log('\n=== DISABLING all AI keys (restore default forward/backward) ===');
  // mask = both keys, enable = 0 (disabled), default modes = true
  if (sendCommand(0x18, 0, 2, 2, 1, 1)) {
    console.log('SUCCESS! AI keys disabled. Forward/backward restored to normal.');
  }
}

async function enableForwardOnly() {
  console.log('\n=== Step 1/2: Disable all AI keys ===');
  if (!sendCommand(0x18, 0, 2, 2, 1, 1)) return;
  await sleep(200);
  console.log('=== Step 2/2: Enable forward only (mask=0x10) ===');
  if (sendCommand(0x10, 1, 0, 0, 1, 1)) {
    console.log('SUCCESS! Forward=AI, Backward=normal mouse key.');
  }
}

async function enableBackwardOnly() {
  console.log('\n=== Step 1/2: Disable all AI keys ===');
  if (!sendCommand(0x18, 0, 2, 2, 1, 1)) return;
  await sleep(200);
  console.log('=== Step 2/2: Enable backward only (mask=0x08) ===');
  if (sendCommand(0x08, 1, 2, 2, 0, 0)) {
    console.log('SUCCESS! Backward=AI, Forward=normal mouse key.');
  }
}

function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

function showStatus() {
  console.log('\n=== CONNECTED AJAZZ DEVICES ===');
  const devices = hid.devices().filter(d => d.vendorId === 0x363c);
  if (devices.length === 0) {
    console.log('No AJAZZ devices found.');
    return;
  }
  
  const pids = [...new Set(devices.map(d => d.productId))];
  for (const pid of pids) {
    const d = devices.find(d => d.productId === pid);
    console.log('  PID: 0x' + pid.toString(16).padStart(4,'0') + ' - ' + (d.product || 'Unknown'));
  }
  
  console.log('\nInterfaces:');
  for (const d of devices) {
    console.log('  UsagePage=0x' + (d.usagePage||0).toString(16).padStart(4,'0') +
      ' Usage=0x' + (d.usage||0).toString(16).padStart(4,'0') +
      ' Path=' + d.path.split('#')[1]?.split('#')[0]);
  }
}

// ─── MAIN ───

(async () => {
const cmd = (process.argv[2] || 'enable').toLowerCase();
switch(cmd) {
  case 'enable':
  case 'on':
    enableBoth();
    break;
  case 'disable':
  case 'off':
    disableAll();
    break;
  case 'forward':
  case 'fw':
    await enableForwardOnly();
    break;
  case 'backward':
  case 'bw':
    await enableBackwardOnly();
    break;
  case 'status':
  case 'list':
    showStatus();
    break;
  default:
    console.log('Usage: node ai_key_control.js [enable|disable|forward|backward|status]');
    console.log('  enable   - Enable AI on both forward and backward keys');
    console.log('  disable  - Disable AI, restore normal forward/backward');
    console.log('  forward  - Enable AI only on forward key (disables backward)');
    console.log('  backward - Enable AI only on backward key (disables forward)');
    console.log('  status   - Show connected AJAZZ devices');
}
})();
