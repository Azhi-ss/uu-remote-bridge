import { parseTermSessions } from '../src/cli';
import { buildTermArgs } from '../src/termBridge';

function check(name: string, ok: boolean): void {
  if (!ok) {
    throw new Error(`FAIL ${name}`);
  }
  console.log(`PASS ${name}`);
}

const sessions = parseTermSessions(
  '[连接] 初始化连接\n' +
    'SESSION_ID\tNAME\tSHELL\tSTATE\tLAST_ACTIVE\n' +
    '2147483649\tsession1\tpowershell\trunning\t1788601231903\n',
);
check('parse session name and id', sessions.length === 1 && sessions[0].label === 'session1' && sessions[0].id === '2147483649');
check(
  'build new session args',
  JSON.stringify(buildTermArgs('dev-1', 'powershell')) ===
    JSON.stringify(['term', '--device-id', 'dev-1', '--shell', 'powershell', '--new-session']),
);
check(
  'build attach session args',
  JSON.stringify(buildTermArgs('dev-1', 'powershell', '2147483649')) ===
    JSON.stringify(['term', '--device-id', 'dev-1', '--shell', 'powershell', '--session-id', '2147483649']),
);
