#!/usr/bin/env node
// Test environment only (runbook m4-test-environment). forge refuses loopback
// deploy targets (FG-DEPLOY-0013 reason=denied_address), and Docker on this
// PC publishes only on Windows' own addresses. This forwards
//   <listen-host>:<listen-port>  ->  127.0.0.1:<target-port>  (the deploy target's sshd)
// where <listen-host> is the Hyper-V internal address of this PC (the WSL
// vEthernet adapter: not reachable from the LAN). Only connections from this
// PC itself (remote address = the listen address) are accepted.
//
//   node ops/test-env/deploy-forwarder.mjs <listen-host> [listen-port=22022] [target-port=22022] [pid-file]
//
// The pid file (optional) is written at listen and removed on exit; stop the
// forwarder with `Stop-Process -Id (Get-Content <pid-file>)`. At most
// MAX_CONNECTIONS at once; a connection idle for IDLE_MS is closed (ssh sends
// keepalives every 15 s, so a live deploy is never idle that long).
//
// No dependency; a plain byte pipe (ssh is end-to-end, host key pinned in forge).
import { rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';

const MAX_CONNECTIONS = 8;
const IDLE_MS = 5 * 60_000;

const [host, listenPort = '22022', targetPort = '22022', pidFile] = process.argv.slice(2);
if (!host || net.isIP(host) !== 4 || host === '0.0.0.0' || host.startsWith('127.')) {
  console.error('deploy-forwarder: give the internal IPv4 address to listen on (not 0.0.0.0, not loopback)');
  process.exit(2);
}
const log = (event, extra = {}) =>
  console.log(JSON.stringify({ timestamp: new Date().toISOString(), event: `forwarder.${event}`, ...extra }));

const server = net.createServer((client) => {
  const from = client.remoteAddress?.replace(/^::ffff:/, '');
  if (from !== host) {
    log('refused', { from });
    client.destroy();
    return;
  }
  const upstream = net.connect({ host: '127.0.0.1', port: Number(targetPort) });
  upstream.setTimeout(30_000, () => upstream.destroy(new Error('connect timeout')));
  upstream.once('connect', () => {
    upstream.setTimeout(IDLE_MS, () => close('upstream')(new Error('idle timeout')));
    client.setTimeout(IDLE_MS, () => close('client')(new Error('idle timeout')));
  });
  const close = (side) => (err) => {
    if (err) log('error', { side, message: err.message });
    client.destroy();
    upstream.destroy();
  };
  client.on('error', close('client'));
  upstream.on('error', close('upstream'));
  client.pipe(upstream).pipe(client);
});
server.maxConnections = MAX_CONNECTIONS;
server.on('error', (err) => {
  log('listen_failed', { message: err.message });
  process.exit(1);
});
server.listen(Number(listenPort), host, () => {
  if (pidFile) writeFileSync(pidFile, `${process.pid}\n`);
  log('listening', { host, port: Number(listenPort), target: `127.0.0.1:${targetPort}`, max_connections: MAX_CONNECTIONS });
});
const stop = () => {
  if (pidFile) rmSync(pidFile, { force: true });
  process.exit(0);
};
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
