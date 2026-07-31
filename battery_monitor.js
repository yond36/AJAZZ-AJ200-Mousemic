// AJAZZ Mouse Battery Monitor
// Battery is reported passively by the device over the 0xFFA0 interface.
// Report format: [0x0A, 0x13, ...version/status...]
//   arr[18] = battery percent
//   arr[17] = charging flag (1 = charging)
//   arr[20] = wireless connected flag (1 = wireless)
// Moving the mouse wakes the device and triggers a status report.
// Usage: node battery_monitor.js
const hid = require('node-hid');
const { execSync } = require('child_process');

const devices = hid.devices().filter(d => d.vendorId === 0x363c);
const targets = devices.filter(d => d.usagePage === 0xffa0 && d.usage === 0x0002);

if (targets.length === 0) {
  console.log('No AJAZZ command interface (0xFFA0) found.');
  process.exit(1);
}

const pids = [...new Set(devices.map(d => d.productId))];
console.log('Device: VID=0x363C ' + pids.map(p => 'PID=0x' + p.toString(16).toUpperCase()).join(', '));
console.log('=== AJAZZ Mouse Battery Monitor ===\n');
console.log('Move the mouse/wake it to refresh battery.\n');
console.log('Battery | Status        | Time');
console.log('--------|---------------|----------------');

let last = -1;

targets.forEach(d => {
  try {
    const dev = new hid.HID(d.path);
    dev.on('data', (data) => {
      const a = Array.from(data);
      if (a[0] === 0x0a && a[1] === 0x13) {
        const battery = a[18];
        const charging = a[17] === 1;
        const wireless = a[20] === 1;
        const status = charging ? 'CHARGING ' : (wireless ? 'WIRELESS ' : 'WIRED    ');
        // Only print when value changes to reduce spam
        if (battery !== last) {
          last = battery;
          const time = new Date().toLocaleTimeString();
          console.log(`${battery}%       | ${status}    | ${time}`);
        }
      }
    });
    dev.on('error', (e) => console.log('Error:', e.message));
    // Wake the mouse once on startup to trigger a status report
    try {
      execSync("powershell -Command \"Add-Type -AssemblyName System.Windows.Forms; $p=[System.Windows.Forms.Cursor]::Position; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point(($p.X+1),($p.Y+1)); Start-Sleep -m 40; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point($p.X,$p.Y)\"");
    } catch (e) {}
    console.log('Listening (move mouse to refresh)... Ctrl+C to exit.\n');
  } catch (e) {
    console.log('Open failed:', e.message);
  }
});

// Periodic wake to keep values fresh (every 15s)
setInterval(() => {
  try {
    execSync("powershell -Command \"Add-Type -AssemblyName System.Windows.Forms; $p=[System.Windows.Forms.Cursor]::Position; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point(($p.X+1),($p.Y+1)); Start-Sleep -m 30; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point($p.X,$p.Y)\"");
  } catch (e) {}
}, 15000);
