// Integration fixture for an unmodified Electron binary's Node entry point.
// No application modules, network, credentials, or user files are accessed.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const {spawnSync} = require('node:child_process');
const {Worker} = require('node:worker_threads');

async function main() {
  const input = Buffer.alloc(256 * 1024);
  for (let i = 0; i < input.length; i++) input[i] = (i * 31 + (i >>> 7)) & 255;
  assert.deepEqual(zlib.inflateSync(zlib.deflateSync(input)), input);
  const key = Buffer.alloc(32, 7), iv = Buffer.alloc(12, 3);
  const cipher = crypto.createCipheriv('aes-256-gcm', key, iv);
  const encrypted = Buffer.concat([cipher.update(input), cipher.final()]);
  const decipher = crypto.createDecipheriv('aes-256-gcm', key, iv);
  decipher.setAuthTag(cipher.getAuthTag());
  assert.deepEqual(Buffer.concat([decipher.update(encrypted), decipher.final()]), input);
  const argv0 = 'custom electron worker λ';
  const child = spawnSync(process.execPath,
    ['-e', 'process.stdout.write(JSON.stringify([process.argv0,process.argv[1]]))', 'child value λ'],
    {argv0, encoding: 'utf8', timeout: 60000});
  assert.equal(child.status, 0, String(child.error || child.stderr));
  assert.deepEqual(JSON.parse(child.stdout), [argv0, 'child value λ']);
  const results = await Promise.all([1, 2, 3].map(seed => new Promise((resolve, reject) => {
    const worker = new Worker(`const {parentPort,workerData}=require('node:worker_threads');
      let x=workerData;for(let i=0;i<100000;i++)x=(Math.imul(x,1664525)+1013904223)>>>0;
      parentPort.postMessage(x);`, {eval: true, workerData: seed});
    let value;
    worker.once('message', message => { value = message; });
    worker.once('error', reject);
    worker.once('exit', code => code === 0 && value !== undefined ? resolve(value) : reject(new Error('worker exit '+code)));
  })));
  for (let n = 0; n < 3; n++) {
    let expected = n + 1;
    for (let i = 0; i < 100000; i++) expected = (Math.imul(expected, 1664525) + 1013904223) >>> 0;
    assert.equal(results[n], expected);
  }
  console.log(JSON.stringify({sha256: crypto.createHash('sha256').update(input).digest('hex'),
    ciphertext: crypto.createHash('sha256').update(encrypted).digest('hex'), workers: results, child: 'PASS'}));
}
main().catch(error => { console.error(error); process.exitCode = 1; });
