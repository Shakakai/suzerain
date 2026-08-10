/**
 * Deep Research Extension
 *
 * Tavily-based deep web research, exposed to the Pi agent as the
 * `deep_research` tool. The tool decomposes a query into sub-questions,
 * searches the web for each, optionally runs a gap-analysis follow-up
 * round, and synthesizes a cited markdown report using the active model.
 *
 * Requires TAVILY_API_KEY in the environment or a .env file (searched
 * upward from the project root).
 */

import type { AgentToolUpdateCallback, ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { StringEnum } from "@earendil-works/pi-ai";
import type { UserMessage, TextContent } from "@earendil-works/pi-ai";
import * as fs from "node:fs";
import * as path from "node:path";

// ═══════════════════════════════════════════════════════════════════════════════
// UTILITIES
// ═══════════════════════════════════════════════════════════════════════════════

function loadEnv(cwd: string): Record<string, string> {
	const env: Record<string, string> = {};
	for (const [key, value] of Object.entries(process.env)) {
		if (value !== undefined) env[key] = value;
	}

	let dir = cwd;
	const seen = new Set<string>();
	while (dir && !seen.has(dir)) {
		seen.add(dir);
		const envPath = path.join(dir, ".env");
		if (fs.existsSync(envPath)) {
			const text = fs.readFileSync(envPath, "utf-8");
			for (const line of text.split("\n")) {
				const trimmed = line.trim();
				if (trimmed.startsWith("#") || !trimmed.includes("=")) continue;
				const eq = trimmed.indexOf("=");
				const key = trimmed.slice(0, eq).trim();
				let value = trimmed.slice(eq + 1).trim();
				if (
					(value.startsWith('"') && value.endsWith('"')) ||
					(value.startsWith("'") && value.endsWith("'"))
				) {
					value = value.slice(1, -1);
				}
				if (!(key in env)) env[key] = value;
			}
		}
		const parent = path.dirname(dir);
		if (parent === dir) break;
		dir = parent;
	}
	return env;
}

function parseJsonSafe<T>(text: string): T | null {
	try {
		const codeBlock = text.match(/```(?:json)?\s*([\s\S]*?)\s*```/);
		if (codeBlock) return JSON.parse(codeBlock[1]);
		const jsonMatch = text.match(/(\{[\s\S]*\}|\[[\s\S]*\])/);
		if (jsonMatch) return JSON.parse(jsonMatch[1]);
		return JSON.parse(text);
	} catch {
		return null;
	}
}

// ═══════════════════════════════════════════════════════════════════════════════
// TAVILY
// ═══════════════════════════════════════════════════════════════════════════════

interface TavilyResult {
	answer?: string;
	query: string;
	results: Array<{
		title: string;
		url: string;
		content: string;
		score: number;
		raw_content?: string;
	}>;
}

async function tavilySearch(
	query: string,
	apiKey: string,
	opts: { maxResults: number; rawContent: boolean },
	signal?: AbortSignal,
): Promise<TavilyResult> {
	const response = await fetch("https://api.tavily.com/search", {
		method: "POST",
		headers: {
			Authorization: `Bearer ${apiKey}`,
			"Content-Type": "application/json",
		},
		body: JSON.stringify({
			query,
			search_depth: "advanced",
			include_answer: true,
			max_results: opts.maxResults,
			include_raw_content: opts.rawContent,
		}),
		signal,
	});

	if (!response.ok) {
		throw new Error(`Tavily error ${response.status}: ${await response.text()}`);
	}
	return (await response.json()) as TavilyResult;
}

// ═══════════════════════════════════════════════════════════════════════════════
// LLM CALLS (uses active Pi model via pi-ai)
// ═══════════════════════════════════════════════════════════════════════════════

async function callLLM(
	systemPrompt: string,
	userContent: string,
	ctx: ExtensionContext,
	signal?: AbortSignal,
): Promise<string> {
	const model = ctx.model;
	if (!model) throw new Error("No active model selected");

	const message: UserMessage = {
		role: "user",
		content: userContent,
		timestamp: Date.now(),
	};

	const result = await ctx.modelRegistry.complete(
		model,
		{
			systemPrompt,
			messages: [message],
		},
		{
			temperature: 0.3,
			signal: signal ?? ctx.signal,
		},
	);

	if (result.stopReason === "error" || result.stopReason === "aborted") {
		throw new Error(result.errorMessage || "LLM call failed");
	}

	return result.content
		.filter((c): c is TextContent => c.type === "text")
		.map((c) => c.text)
		.join("");
}

// ═══════════════════════════════════════════════════════════════════════════════
// RESEARCH WORKFLOW
// ═══════════════════════════════════════════════════════════════════════════════

type Depth = "quick" | "standard" | "deep" | "exhaustive";

const DEPTH_CONFIG: Record<
	Depth,
	{ subQuestions: number; maxResultsPerQuery: number; gapRound: boolean; rawContent: boolean }
> = {
	quick: { subQuestions: 0, maxResultsPerQuery: 5, gapRound: false, rawContent: false },
	standard: { subQuestions: 3, maxResultsPerQuery: 6, gapRound: false, rawContent: false },
	deep: { subQuestions: 5, maxResultsPerQuery: 8, gapRound: true, rawContent: true },
	exhaustive: { subQuestions: 8, maxResultsPerQuery: 10, gapRound: true, rawContent: true },
};

interface Source {
	title: string;
	url: string;
	content: string;
	subQuestion: string;
}

async function planSubQuestions(query: string, count: number, ctx: ExtensionContext, signal?: AbortSignal): Promise<string[]> {
	const systemPrompt = `You are a research planner. Decompose the user's research question into ${count} focused, complementary sub-questions that together fully answer it. Each sub-question should be a concrete web search query covering a distinct angle (definitions, current state, key players, data/numbers, tradeoffs, recent developments).

Return ONLY a JSON array of strings (no markdown, no extra text).`;

	const response = await callLLM(systemPrompt, `Research question: ${query}`, ctx, signal);
	const parsed = parseJsonSafe<string[]>(response);
	if (parsed && Array.isArray(parsed) && parsed.length > 0) {
		return parsed.filter((q) => typeof q === "string" && q.trim()).slice(0, count);
	}
	// Fallback: just search the original query
	return [query];
}

async function findGaps(query: string, sources: Source[], ctx: ExtensionContext, signal?: AbortSignal): Promise<string[]> {
	const digest = sources
		.slice(0, 30)
		.map((s) => `- [${s.subQuestion}] ${s.title}: ${s.content.slice(0, 200).replace(/\n/g, " ")}`)
		.join("\n");

	const systemPrompt = `You are a research critic. Given a research question and a digest of findings so far, identify the most important missing angles or unanswered aspects. Return up to 3 follow-up web search queries that would fill those gaps.

Return ONLY a JSON array of strings (no markdown, no extra text). Return an empty array [] if coverage is sufficient.`;

	const response = await callLLM(
		systemPrompt,
		`Research question: ${query}\n\nFindings so far:\n${digest}`,
		ctx,
		signal,
	);
	const parsed = parseJsonSafe<string[]>(response);
	if (parsed && Array.isArray(parsed)) {
		return parsed.filter((q) => typeof q === "string" && q.trim()).slice(0, 3);
	}
	return [];
}

async function synthesizeReport(query: string, sources: Source[], ctx: ExtensionContext, signal?: AbortSignal): Promise<string> {
	// Number sources and build the evidence block (cap each source's content)
	const numbered = sources.slice(0, 40);
	const evidence = numbered
		.map((s, i) => `[${i + 1}] ${s.title}\nURL: ${s.url}\n${s.content.slice(0, 1800).replace(/\n{3,}/g, "\n\n")}`)
		.join("\n\n---\n\n");

	const systemPrompt = `You are an expert research analyst. Write a comprehensive, well-structured markdown research report answering the user's question, grounded ONLY in the provided sources.

Requirements:
- Start with a 2-3 sentence executive summary (## Summary).
- Organize the body into themed sections with ## headings.
- Cite claims inline using [n] notation referring to the numbered sources.
- Include specific numbers, dates, and names where the sources provide them.
- Note disagreements or uncertainty between sources explicitly.
- End with ## Sources: a numbered list of "Title — URL" for every source cited.`;

	return callLLM(
		systemPrompt,
		`Research question: ${query}\n\nSources:\n\n${evidence}`,
		ctx,
		signal,
	);
}

async function runDeepResearch(
	query: string,
	depth: Depth,
	onUpdate: AgentToolUpdateCallback<Record<string, unknown>> | undefined,
	ctx: ExtensionContext,
	signal?: AbortSignal,
): Promise<{ content: Array<{ type: "text"; text: string }>; details: Record<string, unknown> }> {
	const config = DEPTH_CONFIG[depth];
	const tavilyKey = loadEnv(ctx.cwd).TAVILY_API_KEY;
	if (!tavilyKey) {
		throw new Error("TAVILY_API_KEY not found in environment or .env");
	}

	const progress = (text: string) => onUpdate?.({ content: [{ type: "text", text }], details: {} });

	// ── 1. Plan ────────────────────────────────────────────────────────────
	let subQuestions: string[];
	if (config.subQuestions === 0) {
		subQuestions = [query];
	} else {
		progress("🧭 Planning research approach...");
		subQuestions = await planSubQuestions(query, config.subQuestions, ctx, signal);
	}

	// ── 2. Search round 1 ──────────────────────────────────────────────────
	progress(`🔍 Searching ${subQuestions.length} quer${subQuestions.length === 1 ? "y" : "ies"} via Tavily...`);
	const sources: Source[] = [];
	const seenUrls = new Set<string>();

	const runSearches = async (queries: string[]) => {
		const results = await Promise.all(
			queries.map((q) =>
				tavilySearch(q, tavilyKey, { maxResults: config.maxResultsPerQuery, rawContent: config.rawContent }, signal).catch(
					(err: Error) => {
						progress(`⚠️ Search failed for "${q.slice(0, 50)}": ${err.message}`);
						return null;
					},
				),
			),
		);
		for (let i = 0; i < results.length; i++) {
			const result = results[i];
			if (!result) continue;
			for (const r of result.results) {
				if (seenUrls.has(r.url)) continue;
				seenUrls.add(r.url);
				sources.push({
					title: r.title,
					url: r.url,
					content: r.raw_content || r.content || "",
					subQuestion: queries[i],
				});
			}
		}
	};

	await runSearches(subQuestions);
	progress(`📚 Collected ${sources.length} unique sources.`);

	if (sources.length === 0) {
		throw new Error("All Tavily searches failed — no sources collected.");
	}

	// ── 3. Gap round ───────────────────────────────────────────────────────
	if (config.gapRound) {
		progress("🔬 Analyzing coverage gaps...");
		const followUps = await findGaps(query, sources, ctx, signal);
		if (followUps.length > 0) {
			progress(`🔍 Follow-up round: ${followUps.length} gap-filling quer${followUps.length === 1 ? "y" : "ies"}...`);
			await runSearches(followUps);
			subQuestions = [...subQuestions, ...followUps];
			progress(`📚 ${sources.length} unique sources after follow-up round.`);
		}
	}

	// ── 4. Synthesize ──────────────────────────────────────────────────────
	progress("✍️ Synthesizing report...");
	const report = await synthesizeReport(query, sources, ctx, signal);

	return {
		content: [{ type: "text", text: report }],
		details: {
			query,
			depth,
			subQuestions,
			sourceCount: sources.length,
			sources: sources.map((s) => ({ title: s.title, url: s.url })),
		},
	};
}

// ═══════════════════════════════════════════════════════════════════════════════
// EXTENSION ENTRY POINT
// ═══════════════════════════════════════════════════════════════════════════════

export default function deepResearchExtension(pi: ExtensionAPI) {
	pi.registerTool({
		name: "deep_research",
		label: "Deep Research",
		description:
			"Perform deep web research on a topic using Tavily. Decomposes the question into sub-queries, searches the web (with an optional gap-filling follow-up round), and returns a synthesized markdown report with inline citations and a source list. Use this when you need current, well-sourced information from the web beyond a simple search.",
		promptSnippet: "research a topic on the web and get a cited synthesis report",
		promptGuidelines: [
			"Use deep_research for questions requiring current web information or multi-source synthesis; choose depth 'quick' for simple lookups, 'standard' for most questions, 'deep'/'exhaustive' for thorough investigations.",
		],
		parameters: Type.Object({
			query: Type.String({ description: "Research topic or question" }),
			depth: Type.Optional(
				StringEnum(["quick", "standard", "deep", "exhaustive"] as const, {
					description:
						"Research depth: quick (single search), standard (3 sub-queries), deep (5 sub-queries + gap round), exhaustive (8 sub-queries + gap round). Default: standard.",
				}),
			),
		}),
		async execute(_toolCallId, params, signal, onUpdate, ctx) {
			try {
				const result = await runDeepResearch(params.query, params.depth || "standard", onUpdate, ctx, signal);
				return {
					content: result.content,
					details: result.details,
				};
			} catch (err: any) {
				return {
					content: [{ type: "text", text: `Research failed: ${err.message}` }],
					details: { error: err.message },
					isError: true,
				};
			}
		},
	});

	pi.registerCommand("research", {
		description: "Deep research a topic on the web: /research [depth] <query>",
		async handler(args, ctx) {
			const trimmed = args.trim();
			if (!trimmed) {
				ctx.ui.notify("Usage: /research [quick|standard|deep|exhaustive] <query>", "warning");
				return;
			}

			const depthMatch = trimmed.match(/^(quick|standard|deep|exhaustive)\s+([\s\S]+)$/);
			const depth = (depthMatch?.[1] ?? "standard") as Depth;
			const query = depthMatch?.[2] ?? trimmed;

			ctx.ui.notify(`Researching (${depth}): ${query.slice(0, 60)}...`, "info");

			// Run in the background: pi's input loop awaits command handlers, so
			// awaiting the full research workflow here would freeze the TUI until done.
			const progress = (update: { content: Array<{ type: string; text?: string }> }) => {
				const text = update.content?.[0]?.text;
				if (text) ctx.ui.notify(text, "info");
			};
			runDeepResearch(query, depth, progress, ctx)
				.then((result) => {
					pi.sendMessage(
						{ customType: "research-report", content: result.content[0].text, display: true },
						{ triggerTurn: false },
					);
					ctx.ui.notify(`Research complete (${result.details.sourceCount} sources)`, "info");
				})
				.catch((err: any) => {
					ctx.ui.notify(`Research failed: ${err.message}`, "error");
				});
		},
	});
}
