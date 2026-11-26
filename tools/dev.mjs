// tools/dev.mjs — 開発版（cargo run の debug ビルド）起動ランチャー
//
// 仕様（diagrams/env-launch.mmd が正本）:
//   - 通常版・開発版とも :8586 を使う
//   - dev 起動時は対象ポートの占有プロセスを強制停止（taskkill /F /T）してから起動
//   - 検証用の並走インスタンスは PORT=<任意> npm run dev で適宜ポートを指定
import { execFileSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const port = Number(process.env.PORT || 8586);

function pidsOnPort(p) {
    let out;
    try {
        out = execFileSync('netstat', ['-ano', '-p', 'tcp'], { encoding: 'utf8' });
    } catch {
        return [];
    }
    const pids = new Set();
    for (const line of out.split(/\r?\n/)) {
        const c = line.trim().split(/\s+/);
        if (c[0] === 'TCP' && /LISTENING/i.test(c[3] || '') && c[1]?.endsWith(`:${p}`)) {
            pids.add(c[4]);
        }
    }
    return [...pids].filter((pid) => pid !== String(process.pid));
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

for (const pid of pidsOnPort(port)) {
    console.log(`[dev] :${port} を占有する PID ${pid} を強制停止します`);
    try {
        execFileSync('taskkill', ['/F', '/T', '/PID', pid], { stdio: 'inherit' });
    } catch (e) {
        console.error(`[dev] taskkill PID ${pid} 失敗: ${e.message}`);
    }
}

// ポート解放を待つ（上限10秒）
const deadline = Date.now() + 10_000;
while (pidsOnPort(port).length && Date.now() < deadline) await wait(200);
const rest = pidsOnPort(port);
if (rest.length) {
    console.error(`[dev] :${port} が解放されません（残 PID: ${rest.join(', ')}）`);
    process.exit(1);
}

console.log(`[dev] cargo run を起動します（PORT=${port}）`);
const child = spawn('cargo', ['run', '--manifest-path', 'server/Cargo.toml'], {
    cwd: root,
    stdio: 'inherit',
    env: process.env,
});
child.on('exit', (code) => process.exit(code ?? 0));
