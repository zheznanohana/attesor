'use strict';
// Headless Electron-as-Node process launch/IPC probe; no application bootstrap.
const { fork } = require('child_process');
setTimeout(() => { throw Error('process probe timed out'); }, 60000).unref();
if (process.argv[2] === 'child') {
  if (process.env.P_PROCESS_TOKEN !== 'inherited') throw Error('environment');
  const sum = Array.from({length: 2000}, (_, i) => i).reduce((a, b) => a + b, 0);
  process.send({sum, pid: process.pid, arg: process.argv[3]});
} else {
  const count = 6;
  let done = 0;
  for (let i = 0; i < count; i++) {
    const child = fork(__filename, ['child', String(i)], {
      env: {...process.env, P_PROCESS_TOKEN: 'inherited'}, stdio: ['ignore', 'pipe', 'pipe', 'ipc']
    });
    let received = false;
    child.stderr.pipe(process.stderr); child.stdout.pipe(process.stdout);
    child.on('message', m => {
      if (m.sum !== 1999000 || m.arg !== String(i) || m.pid !== child.pid) throw Error('IPC/argv');
      received = true;
    });
    child.on('error', e => { throw e; });
    child.on('exit', (code, signal) => {
      if (code !== 0 || signal || !received) throw Error(`child ${i}: ${code}/${signal}/${received}`);
      if (++done === count) console.log('PASS fork: 6 children, env argv IPC exit');
    });
  }
}
