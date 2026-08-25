/**
 * UI test: secrets add-provider flow (drives the real web UI in headless
 * Chrome against a live suzerain). Usage:
 *   node ui-test.mjs [base-url]
 * Env: CHROME (path to Chrome binary), UI_TEST_TIMEOUT_MS.
 */

import puppeteer from "puppeteer-core";

const BASE = process.argv[2] || "http://127.0.0.1:8484";
import { execSync } from "node:child_process";

function resolveChrome() {
	if (process.env.CHROME) return process.env.CHROME;
	const candidates = [
		"google-chrome",
		"chromium",
		"chromium-browser",
		"chrome",
		"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
	];
	for (const c of candidates) {
		try {
			const found = execSync(`which ${c} 2>/dev/null || true`, { encoding: "utf8" }).trim();
			if (found) return found;
		} catch {}
		if (c.startsWith("/")) {
			try {
				execSync(`test -e "${c}"`);
				return c;
			} catch {}
		}
	}
	throw new Error("no Chrome/Chromium binary found (set CHROME env)");
}
const CHROME = resolveChrome();
const TIMEOUT = parseInt(process.env.UI_TEST_TIMEOUT_MS || "30000");

const PROVIDER_ID = `uitest-${Date.now().toString(36)}`;
const PROVIDER_VALUE = "sk-uitest-0000000000000000000000";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
	const browser = await puppeteer.launch({
		executablePath: CHROME,
		headless: "new",
		args: ["--no-sandbox", "--disable-gpu"],
	});
	try {
		const page = await browser.newPage();
		const consoleErrors = [];
		// Known-benign noise that isn't a real regression.
		const ALLOWED_CONSOLE_ERRORS = [/favicon\.ico/];
		page.on("pageerror", (err) => {
			consoleErrors.push(err.message);
			console.log("PAGE ERROR:", err.message);
		});
		page.on("console", (msg) => {
			if (msg.type() !== "error") return;
			const text = msg.text();
			console.log("CONSOLE ERROR:", text);
			if (!ALLOWED_CONSOLE_ERRORS.some((re) => re.test(text))) {
				consoleErrors.push(text);
			}
		});

		console.log(`→ open ${BASE}/#/secrets`);
		await page.goto(`${BASE}/#/secrets`, { waitUntil: "networkidle0", timeout: TIMEOUT });
		await page.waitForSelector("#new-provider", { timeout: TIMEOUT });

		// Fill the add-provider form and submit. TYPE_DELAY_MS simulates a
		// slow human typing across the 5s polling cycle.
		const delay = parseInt(process.env.TYPE_DELAY_MS || "0");
		// #new-provider is a <select> when the provider catalog loads (pick the
		// first option without a configured key), else a free-form <input>.
		let providerId = PROVIDER_ID;
		const isSelect = await page.evaluate(
			() => document.querySelector("#new-provider").tagName === "SELECT",
		);
		if (isSelect) {
			providerId = await page.evaluate(() => {
				const sel = document.querySelector("#new-provider");
				const opt = [...sel.options].find((o) => !o.dataset.hasKey) || sel.options[0];
				if (!opt) throw new Error("provider select has no options");
				sel.value = opt.value;
				return opt.value;
			});
		} else {
			await page.type("#new-provider", PROVIDER_ID, { delay });
		}
		console.log(`→ provider: ${providerId}`);
		await page.type("#new-provider-value", PROVIDER_VALUE, { delay });
		console.log("→ click Add provider");
		await page.evaluate(() => {
			const btn = [...document.querySelectorAll("button")].find((b) =>
				b.textContent.includes("Add provider"),
			);
			if (!btn) throw new Error("Add provider button not found");
			btn.click();
		});

		// The inventory row should appear after the store round-trip.
		const found = await page
			.waitForFunction(
				(id) => document.body.innerText.includes(id),
				{ timeout: TIMEOUT },
				providerId,
			)
			.then(() => true)
			.catch(() => false);

		// Also verify via the API that the value landed.
		const api = await page.evaluate(async (name) => {
			const r = await fetch("/api/v1/secrets");
			const j = await r.json();
			return j.entries.some((e) => e.kind === "provider" && e.name === name);
		}, providerId);

		console.log(`inventory row visible: ${found}`);
		console.log(`API inventory contains provider: ${api}`);

		if (!found || !api) {
			console.error("UI TEST FAILED: provider was not stored");
			process.exitCode = 1;
		} else if (consoleErrors.length > 0) {
			console.error(`UI TEST FAILED: ${consoleErrors.length} unexpected console/page error(s)`);
			process.exitCode = 1;
		} else {
			console.log("UI TEST PASSED");
		}
	} finally {
		await browser.close();
	}
}

main().catch((err) => {
	console.error("UI TEST ERROR:", err.message);
	process.exit(1);
});
