/**
 * gondolin-driver — Phase 0 spike (c)/(d) and future castellan sidecar.
 *
 * Protocol: newline-delimited JSON on stdin/stdout (LF only), one object per
 * line, so the Rust daemon can talk to it with the same framing helpers used
 * for pi RPC and the iroh wire protocols.
 *
 * Commands (stdin):
 *   {"cmd":"boot","options":{...}}                 → VM.create(options)
 *   {"cmd":"exec","argv":[...],"cwd": "/workspace"} → buffered exec, one reply
 *   {"cmd":"spawn_agent","argv":[...],"env":{...}}  → long-running streaming exec
 *   {"cmd":"agent_stdin","data":"..."}              → write to agent stdin
 *   {"cmd":"snapshot"}                              → disk checkpoint of the VM
 *   {"cmd":"close"}                                 → shut the VM down
 *
 * Events (stdout):
 *   {"event":"ready"}
 *   {"event":"reply","id":N,"ok":true,"result":{...}}
 *   {"event":"agent_stdout","line":"..."}           (pi RPC JSONL flows here)
 *   {"event":"agent_stderr","line":"..."}
 *   {"event":"agent_exit","exitCode":N}
 *
 * Spike self-test (no castellan needed):
 *   node src/index.mjs --spike
 */

import { VM } from "@earendil-works/gondolin";
import readline from "node:readline";

class Driver {
	constructor() {
		/** @type {VM | null} */
		this.vm = null;
		/** @type {import("@earendil-works/gondolin").ExecProcess | null} */
		this.agent = null;
		this.nextId = 0;
	}

	emit(obj) {
		process.stdout.write(JSON.stringify(obj) + "\n");
	}

	async handle(line) {
		let msg;
		try {
			msg = JSON.parse(line);
		} catch {
			this.emit({ event: "error", error: "bad json" });
			return;
		}
		const id = msg.id ?? null;
		try {
			switch (msg.cmd) {
				case "boot": {
					this.vm = await VM.create(msg.options ?? {});
					this.emit({ event: "reply", id, ok: true, result: { booted: true } });
					break;
				}
				case "exec": {
					const r = await this.vm.exec(msg.argv, { cwd: msg.cwd, env: msg.env });
					this.emit({
						event: "reply",
						id,
						ok: r.ok,
						result: { exitCode: r.exitCode, stdout: r.stdout, stderr: r.stderr },
					});
					break;
				}
				case "spawn_agent": {
					this.agent = this.vm.exec(msg.argv, {
						cwd: msg.cwd,
						env: msg.env,
						stdin: true,
						stdout: "pipe",
						stderr: "pipe",
					});
					// Stream pi's JSONL events up to castellan.
					(async () => {
						for await (const line of this.agent.lines()) {
							this.emit({ event: "agent_stdout", line });
						}
					})();
					(async () => {
						for await (const line of this.agent.stderr ?? []) {
							this.emit({ event: "agent_stderr", line });
						}
					})();
					this.agent.result.then((r) =>
						this.emit({ event: "agent_exit", exitCode: r.exitCode }),
					);
					this.emit({ event: "reply", id, ok: true, result: { spawned: true } });
					break;
				}
				case "agent_stdin": {
					this.agent.write(msg.data);
					break;
				}
				case "snapshot": {
					const snap = await this.vm.snapshot();
					this.emit({ event: "reply", id, ok: true, result: snap });
					break;
				}
				case "close": {
					await this.vm.close();
					this.emit({ event: "reply", id, ok: true, result: { closed: true } });
					break;
				}
				default:
					this.emit({ event: "reply", id, ok: false, error: `unknown cmd ${msg.cmd}` });
			}
		} catch (err) {
			this.emit({ event: "reply", id, ok: false, error: String(err?.message ?? err) });
		}
	}
}

/** Spike (c): boot a VM, check the guest, stream a long-running process's
 *  stdio both ways (standing in for `pi --mode rpc`), and report. */
async function spike() {
	const say = (s) => console.error(`[spike] ${s}`);
	say("booting VM (first run downloads ~200MB of guest assets)…");
	const vm = await VM.create({});
	say("VM booted");

	const uname = await vm.exec(["uname", "-a"]);
	say(`guest: ${uname.stdout.trim()}`);

	// Spike (d) reconnaissance: what does the base Alpine image give us for
	// provisioning node/pi/mise inside the guest?
	for (const probe of [
		["cat", "/etc/alpine-release"],
		["sh", "-lc", "command -v node npm mise git || true"],
		["sh", "-lc", "apk info -e nodejs npm git 2>/dev/null || true"],
	]) {
		const r = await vm.exec(probe);
		say(`$ ${probe.join(" ")} → ${r.stdout.trim() || "(empty)"}`);
	}

	// Long-running bidirectional stdio (the pi-RPC pattern), using `cat` as a
	// line echoer standing in for the agent.
	say("spawning long-running `cat` (echo) as fake agent…");
	const proc = vm.exec(["/bin/cat"], { stdin: true, stdout: "pipe" });
	const lines = proc.lines();
	proc.write('{"id":"1","type":"get_state"}\n');
	const first = await lines.next();
	say(`round-trip 1: ${first.value}`);
	proc.write('{"id":"2","type":"prompt","message":"hello"}\n');
	const second = await lines.next();
	say(`round-trip 2: ${second.value}`);
	if (!first.value.includes("get_state") || !second.value.includes("prompt")) {
		throw new Error("stdio round-trip failed");
	}

	// Close stdin so `cat` exits on its own, then await it before closing the
	// VM (closing with an in-flight exec rejects it noisily).
	proc.end();
	await proc.result.catch(() => {});
	await vm.close();
	say("spike ok");
}

if (process.argv.includes("--spike")) {
	spike().catch((err) => {
		console.error(`[spike] FAILED: ${err?.message ?? err}`);
		process.exit(1);
	});
} else {
	const driver = new Driver();
	driver.emit({ event: "ready" });
	readline
		.createInterface({ input: process.stdin, terminal: false })
		.on("line", (line) => void driver.handle(line));
}
