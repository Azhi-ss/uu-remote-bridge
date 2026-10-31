/**
 * uu-bridge: standalone CLI for UU Remote (GameViewer) terminal channel.
 * Reuses TermBridge from song-chaoyang/uu-remote-vscode (MIT-style reuse for local automation).
 *
 * Usage:
 *   node uu-bridge.cjs doctor [device_id]          (只读预检;带设备 ID 时探测终端通道)
 *   node uu-bridge.cjs list
 *   node uu-bridge.cjs exec  <device_id> "<command>" [--shell powershell|cmd|zsh|bash] [--session-name <name>|--session-id <id>]
 *   node uu-bridge.cjs read  <device_id> <remote_path>
 *   node uu-bridge.cjs write <device_id> <remote_path> <local_file>
 *   node uu-bridge.cjs pty   <device_id>            (interactive: stdin lines -> screen snapshots, 'exit' quits)
 *
 * CLI 路径可用环境变量 UU_CLI_PATH 指定;否则自动探测常见安装位置。
 */
import { execCliText, listDevices, parseTermSessions, resolveCliPath, platformName, tryParseJson } from './cli';
import { runDoctor } from './doctor';
import { TermBridge } from './termBridge';
import type { ShellKind } from './types';

/** CLI 路径：UU_CLI_PATH 优先，否则探测常见安装位置（多盘符 / LOCALAPPDATA / PATH） */
const resolveCli = () => resolveCliPath(process.env['UU_CLI_PATH']);

function takeOption(argv: string[], names: string[]): string | undefined {
  for (const name of names) {
    const exact = argv.indexOf(name);
    if (exact >= 0) {
      const value = argv[exact + 1];
      if (!value || value.startsWith('--')) {
        console.error(`option requires a value: ${name}`);
        process.exit(2);
      }
      argv.splice(exact, 2);
      return value;
    }
    const prefix = `${name}=`;
    const inline = argv.findIndex((arg) => arg.startsWith(prefix));
    if (inline >= 0) {
      const value = argv[inline].slice(prefix.length);
      if (!value) {
        console.error(`option requires a value: ${name}`);
        process.exit(2);
      }
      argv.splice(inline, 1);
      return value;
    }
  }
  return undefined;
}

export function parseArgs(argv: string[]) {
  const shellIdx = argv.indexOf('--shell');
  let shell: ShellKind = 'powershell';
  if (shellIdx >= 0) {
    shell = argv[shellIdx + 1] as ShellKind;
    argv.splice(shellIdx, 2);
  }
  const sessionId = takeOption(argv, ['--session-id']);
  const sessionName = takeOption(argv, ['--session', '--session-name']);
  if (sessionId && sessionName) {
    console.error('use only one of --session-id or --session-name');
    process.exit(2);
  }
  return { shell, sessionId, sessionName, args: argv };
}

async function resolveSessionId(cli: string, deviceId: string, sessionName: string): Promise<string> {
  let raw: string;
  try {
    raw = await execCliText(cli, ['term', '--device-id', deviceId, '--list-sessions'], { timeoutMs: 30000 });
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    // 旧版 CLI 仍能通过 `term -d <device> list --json` 列出 GUI 会话，
    // 但不会返回可供管道附加的 session ID。把这个事实明确告诉调用者。
    try {
      const legacyRaw = await execCliText(cli, ['term', '-d', deviceId, 'list', '--json'], { timeoutMs: 30000 });
      const legacy = tryParseJson(legacyRaw) as { data?: { items?: Array<{ name?: unknown }> } } | undefined;
      const names = Array.isArray(legacy?.data?.items)
        ? legacy.data.items.map((item) => String(item.name ?? '')).filter(Boolean)
        : [];
      if (names.includes(sessionName)) {
        throw new Error(
          `已找到远程会话「${sessionName}」，但当前 uuyc-cli 只支持 GUI 附加，` +
            `没有可供无窗口管道使用的 session-id。请升级 UU Remote。`,
        );
      }
    } catch (legacyError) {
      if (legacyError instanceof Error && legacyError.message.includes('已找到远程会话')) {
        throw legacyError;
      }
    }
    throw new Error(
      `无法按名称附加远程会话「${sessionName}」：当前 uuyc-cli 不支持会话管道或会话列表。` +
        `请升级 UU Remote；原始错误：${detail}`,
    );
  }
  const sessions = parseTermSessions(raw);
  const matches = sessions.filter((s) => s.label === sessionName);
  if (matches.length === 0) {
    const known = sessions.map((s) => `${s.label}(${s.id})`).join(', ');
    throw new Error(`找不到远程会话「${sessionName}」${known ? `；当前会话：${known}` : ''}`);
  }
  if (matches.length > 1) {
    throw new Error(`远程会话名称「${sessionName}」不唯一，请改用 --session-id <id>`);
  }
  return matches[0].id;
}

type LegacySessionItem = {
  name?: unknown;
  shell?: unknown;
  state?: unknown;
  createdAt?: unknown;
  created_at_ms?: unknown;
};

/** 兼容旧版 CLI 的 GUI 会话列表；旧版没有数字 session-id，只能用于识别和诊断。 */
async function listRemoteSessions(cli: string, deviceId: string): Promise<string> {
  try {
    const raw = await execCliText(cli, ['term', '--device-id', deviceId, '--list-sessions'], { timeoutMs: 30000 });
    return raw.trim() || '(no sessions)';
  } catch (newError) {
    try {
      const raw = await execCliText(cli, ['term', '-d', deviceId, 'list', '--json'], { timeoutMs: 30000 });
      const obj = tryParseJson(raw) as { data?: { items?: LegacySessionItem[] } } | undefined;
      const items = Array.isArray(obj?.data?.items) ? obj.data.items : [];
      if (items.length === 0) {
        return 'LEGACY\t(no sessions)\t(no numeric session-id; GUI only)';
      }
      const lines = ['LEGACY_ID\tNAME\tSHELL\tSTATE\tCREATED_AT'];
      for (const item of items) {
        const name = String(item.name ?? '');
        const shell = String(item.shell ?? '');
        const state = String(item.state ?? '');
        const created = String(item.createdAt ?? item.created_at_ms ?? '');
        lines.push(`GUI:${name}\t${name}\t${shell}\t${state}\t${created}`);
      }
      return lines.join('\n');
    } catch {
      const detail = newError instanceof Error ? newError.message : String(newError);
      throw new Error(`无法读取远程会话列表；新版和旧版 CLI 均失败：${detail}`);
    }
  }
}

async function main() {
  const { shell, sessionId: requestedSessionId, sessionName, args } = parseArgs(process.argv.slice(2));
  const [cmd, ...rest] = args;

  if (cmd === 'doctor') {
    process.exitCode = await runDoctor(process.env['UU_CLI_PATH'], rest[0]);
    return;
  }

  const cli = await resolveCli();

  if (cmd === 'list') {
    const devices = await listDevices(cli);
    for (const d of devices) {
      console.log(`${d.deviceId}\t${d.deviceName}\tonline=${d.isOnline}\tplatform=${platformName(d.platform) || d.platform}`);
    }
    return;
  }

  const deviceId = rest[0];
  if (!deviceId) {
    console.error('device id required');
    process.exit(2);
  }

  if (cmd !== 'pty' && shell === 'cmd') {
    console.error('ERROR: exec/read/write 仅支持 powershell(协议限制); cmd 会话请用 pty 交互模式');
    process.exit(2);
  }

  if (cmd === 'sessions') {
    console.log(await listRemoteSessions(cli, deviceId));
    return;
  }

  if (cmd === 'kill') {
    const sid = rest[1];
    if (!sid) {
      console.error('usage: kill <device_id> <session_id>');
      process.exit(2);
    }
    const { execCliText } = await import('./cli');
    const out = await execCliText(cli, ['term', '--device-id', deviceId, '--kill-session', sid], { timeoutMs: 30000 });
    console.log(out.trim() || `session ${sid} killed`);
    return;
  }

  const sessionId = sessionName ? await resolveSessionId(cli, deviceId, sessionName) : requestedSessionId;

  if (cmd === 'exec') {
    const command = rest.slice(1).join(' ');
    if (!command) {
      console.error('command required');
      process.exit(2);
    }
    const bridge = new TermBridge(cli, deviceId, shell, undefined, sessionId);
    try {
      const rows = await bridge.execRows(command, { timeoutMs: 90000 });
      console.log(rows.length > 0 ? rows.join('\n') : '(no output)');
    } finally {
      await bridge.dispose();
    }
    return;
  }

  if (cmd === 'read') {
    const path = rest[1];
    const bridge = new TermBridge(cli, deviceId, shell, undefined, sessionId);
    try {
      const rows = await bridge.readFileB64(path, 256 * 1024);
      const first = rows[0] ?? '';
      if (first === 'UU_F_MISS') throw new Error(`远程文件不存在或不可访问: ${path}`);
      if (first === 'ISDIR') throw new Error(`${path} is a directory`);
      if (first.startsWith('TOOBIG')) throw new Error(`file too large: ${first}`);
      const b64 = rows.map((r) => r.trim()).join('');
      process.stdout.write(Buffer.from(b64, 'base64'));
    } finally {
      await bridge.dispose();
    }
    return;
  }

  if (cmd === 'write') {
    const path = rest[1];
    const localFile = rest[2];
    if (!path || !localFile) {
      console.error('usage: write <device> <remote_path> <local_file>');
      process.exit(2);
    }
    const { readFileSync } = await import('fs');
    const content = readFileSync(localFile);
    const bridge = new TermBridge(cli, deviceId, shell, undefined, sessionId);
    try {
      await bridge.writeFile(path, content);
      console.log(`written ${path} (${content.length} bytes)`);
    } finally {
      await bridge.dispose();
    }
    return;
  }

  if (cmd === 'pty') {
    const bridge = new TermBridge(cli, deviceId, shell, undefined, sessionId);
    await bridge.ensureReady();
    console.log('--- pty ready; type lines, Ctrl-D / "exit" to quit ---');
    const readline = await import('readline');
    const rl = readline.createInterface({ input: process.stdin });
    for await (const line of rl) {
      if (line === 'exit' || line === '__quit__') break;
      bridge.writeRaw(line + '\r');
      await new Promise((r) => setTimeout(r, 800));
      console.log(bridge.snapshot().join('\n'));
    }
    await bridge.dispose();
    return;
  }

  console.error('unknown command:', cmd);
  process.exit(2);
}

main().catch((e) => {
  console.error('ERROR:', e instanceof Error ? e.message : String(e));
  process.exit(1);
});
