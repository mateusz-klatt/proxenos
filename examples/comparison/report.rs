//! JSON report schema and Markdown table rendering.

use std::fmt::Write as _;

use serde::Serialize;

use crate::scenarios::{Part1Results, Part2Results, ProxenosProbe};
use crate::stats::Summary;

#[derive(Clone, Debug, Serialize)]
pub struct HostMetadata {
    pub os: String,
    pub arch: String,
    pub kernel: String,
    pub cpu: String,
    pub node_version: String,
    pub delegator_version: String,
    pub proxenos_version: String,
    pub proxenos_commit: String,
    pub proxenos_bin: String,
    pub stub_bin: String,
    pub delegator_dir: String,
    pub mode: String,
    pub runs: usize,
    pub timeout_ms: u64,
    pub concurrency: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeatureDifferenceRow {
    pub dimension: String,
    pub proxenos: String,
    pub claude_delegator: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FullReport {
    pub schema_version: u32,
    pub metadata: HostMetadata,
    pub proxenos_probe: Option<ProxenosProbe>,
    pub feature_differences: Vec<FeatureDifferenceRow>,
    pub part1: Option<Part1Results>,
    pub part2: Option<Part2Results>,
}

pub fn build_feature_differences(probe: Option<&ProxenosProbe>) -> Vec<FeatureDifferenceRow> {
    let binary_providers = probe
        .map(|p| p.tools_listed.join(", "))
        .unwrap_or_else(|| "codex, codex-reply, antigravity, antigravity-reply".into());
    let has_background = probe.is_some_and(|p| p.has_await && p.has_wait_param);
    let background_proxenos = if has_background {
        "Implemented in probed binary (`wait: false`, `await`, `cancel` listed in `tools/list`; D17)"
    } else {
        "Not yet in probed binary (`tools/list` has no `await`/`cancel` or `wait` param at this commit); designed in D17 and merged on newer `main`"
    };
    let busy_proxenos = if has_background {
        "Implemented in probed binary (max 3 concurrent delegations, immediate `busy` failure without queuing; D17)"
    } else {
        "Not yet in probed binary at this commit; designed in D17 (max 3 concurrent, immediate `busy` failure)"
    };
    let durable_proxenos = if has_background {
        "Implemented in probed binary (persisted session settings per D19, in addition to in-process continuity)"
    } else {
        "In-memory only in probed binary (bounded 1,024-entry `StartStore`, lost on server restart; file persistence designed in D19)"
    };
    let accounting_proxenos = if has_background {
        "Implemented in probed binary (process-group / job memory & CPU accounting per D18; Codex token counts in `reported.usage`; `usage` tool planned in D9)"
    } else {
        "Codex token counts in `reported.usage`; no memory/CPU accounting or `usage` tool in probed binary yet (designed in D9, D18)"
    };
    let integrate_proxenos = "Not yet in probed binary (`proxenos` subcommands are `--version` and `mcp --client <name>`); `integrate` and `doctor` designed in D10";

    vec![
        FeatureDifferenceRow {
            dimension: "Providers supported".into(),
            proxenos: format!(
                "2 providers in probed binary ({binary_providers}); 7 planned in D5"
            ),
            claude_delegator: "7 providers (claude, codex, agy, kimi, copilot, grok, cursor) plus separate `usage` MCP server; one Node process per provider".into(),
            source: "code/docs + tools/list probe".into(),
        },
        FeatureDifferenceRow {
            dimension: "Background or await".into(),
            proxenos: background_proxenos.into(),
            claude_delegator: "Foreground only; no `wait: false`, `await`, or `cancel` tool (relies on MCP `notifications/cancelled`)".into(),
            source: "code/docs + tools/list probe".into(),
        },
        FeatureDifferenceRow {
            dimension: "Busy limit".into(),
            proxenos: busy_proxenos.into(),
            claude_delegator: "No server-side concurrency limit (`activeChildren` is an unbounded `Set`)".into(),
            source: "code/docs + tools/list probe".into(),
        },
        FeatureDifferenceRow {
            dimension: "Durable sessions".into(),
            proxenos: durable_proxenos.into(),
            claude_delegator: "Stateless bridges; no remembered start settings on disk or in memory (caller must repeat `model`, `effort`, `sandbox`, `cwd` on reply)".into(),
            source: "code/docs + tools/list probe".into(),
        },
        FeatureDifferenceRow {
            dimension: "Accounting".into(),
            proxenos: accounting_proxenos.into(),
            claude_delegator: "Separate `usage` MCP server reads subscription windows and model cost across all 7 CLIs without spending tokens; bridges report no per-turn memory, CPU, or token usage".into(),
            source: "code/docs + tools/list probe".into(),
        },
        FeatureDifferenceRow {
            dimension: "Integrate or doctor".into(),
            proxenos: integrate_proxenos.into(),
            claude_delegator: "Claude Code plugin manifest + `scripts/setup-mcp.js` for Copilot, Grok, and Kimi + `test/mcp-probe.mjs`; no `doctor` subcommand".into(),
            source: "code/docs".into(),
        },
        FeatureDifferenceRow {
            dimension: "Result envelope".into(),
            proxenos: "Versioned JSON envelope (`schema_version: 1`) separating `observed` (state, typed `failure.kind`, `passed`, `access_mechanism`, `attempts` with exit code/signal/byte counts, optional `git` snapshots), `reported` (`thread_id`, `content`, `model`, `usage`, `failure`, `diagnostic`), and `not_claimed`; 16 KiB inline excerpt + 4 MiB archive file".into(),
            claude_delegator: "Success: `{\"threadId\",\"content\"}` JSON text (plus sibling `threadId` field); failure: `isError: true` plain text (`Error: ...`) with no structured failure kind; no output capture cap or excerpting".into(),
            source: "code/docs".into(),
        },
    ]
}

fn fmt_summary(s: Option<&Summary>) -> String {
    match s {
        Some(s) => format!("{:.2} / {:.2} / {:.2}", s.median, s.p90, s.max),
        None => "—".into(),
    }
}

fn fmt_bytes_summary(s: Option<&Summary>) -> String {
    match s {
        Some(s) => format!("{:.0} / {:.0} / {:.0}", s.median, s.p90, s.max),
        None => "—".into(),
    }
}

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', "<br>")
}

pub fn render_markdown(report: &FullReport) -> String {
    let mut md = String::new();

    if let Some(p1) = &report.part1 {
        if !p1.scenario_a_advisory.is_empty() {
            let _ = writeln!(
                md,
                "### Scenario a: Successful advisory turn (fixed answer)\n"
            );
            let _ = writeln!(
                md,
                "| Server | Provider | Wall time med / p90 / max (ms) | Result text size med / p90 / max (B) | Full frame size med / p90 / max (B) | Answer intact |"
            );
            let _ = writeln!(md, "|---|---|---:|---:|---:|---:|");
            for r in &p1.scenario_a_advisory {
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {} | {} | {}/{} |",
                    r.target.server,
                    r.target.provider,
                    fmt_summary(r.wall_ms.as_ref()),
                    fmt_bytes_summary(r.result_bytes.as_ref()),
                    fmt_bytes_summary(r.frame_bytes.as_ref()),
                    r.intact_count,
                    r.trials.len()
                );
            }
            md.push('\n');
        }

        if !p1.scenario_b_timeout.is_empty() {
            let _ = writeln!(md, "### Scenario b: Timeout on hanging stub\n");
            let _ = writeln!(
                md,
                "| Server | Provider | Requested timeout (ms) | Time to result med / p90 / max (ms) | Structured kind | Reported failure |"
            );
            let _ = writeln!(md, "|---|---|---:|---:|---|---|");
            for r in &p1.scenario_b_timeout {
                let kind = r
                    .structured_kind_sample
                    .as_deref()
                    .map(|k| format!("`{k}`"))
                    .unwrap_or_else(|| "none (text only)".into());
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {} | {} | {} |",
                    r.target.server,
                    r.target.provider,
                    r.requested_timeout_ms,
                    fmt_summary(r.wall_ms.as_ref()),
                    kind,
                    escape_cell(&r.reported_text_sample)
                );
            }
            md.push('\n');
        }

        if !p1.scenario_c_cancel.is_empty() {
            let _ = writeln!(
                md,
                "### Scenario c: Client cancellation (`notifications/cancelled`) during hanging turn\n"
            );
            let _ = writeln!(
                md,
                "| Server | Provider | Ack/result med / p90 / max (ms) | Response to cancelled id | CLI gone @2s | Grandchild gone @2s | CLI gone @10s | Grandchild gone @10s |"
            );
            let _ = writeln!(md, "|---|---|---:|---|---:|---:|---:|---:|");
            for r in &p1.scenario_c_cancel {
                let n = r.trials.len();
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {} ({}/{}) | {}/{} | {}/{} | {}/{} | {}/{} |",
                    r.target.server,
                    r.target.provider,
                    fmt_summary(r.ack_ms.as_ref()),
                    escape_cell(&r.response_summary_sample),
                    r.cancel_response_count,
                    n,
                    r.cli_gone_2s_count,
                    n,
                    r.gc_gone_2s_count,
                    n,
                    r.cli_gone_10s_count,
                    n,
                    r.gc_gone_10s_count,
                    n
                );
            }
            md.push('\n');
        }

        if !p1.scenario_d_sigkill.is_empty() {
            let _ = writeln!(
                md,
                "### Scenario d: Server killed (`SIGKILL`) during hanging turn\n"
            );
            let _ = writeln!(
                md,
                "| Server | Provider | Pre-kill verified | CLI survivors @2s (runs) | Grandchild survivors @2s (runs) | CLI survivors @10s (runs) | Grandchild survivors @10s (runs) |"
            );
            let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|");
            for r in &p1.scenario_d_sigkill {
                let n = r.trials.len();
                let _ = writeln!(
                    md,
                    "| {} | {} | {}/{} | {}/{} | {}/{} | {}/{} | {}/{} |",
                    r.target.server,
                    r.target.provider,
                    r.pre_verified_count,
                    n,
                    r.cli_survived_2s_runs,
                    n,
                    r.gc_survived_2s_runs,
                    n,
                    r.cli_survived_10s_runs,
                    n,
                    r.gc_survived_10s_runs,
                    n
                );
            }
            md.push('\n');
        }

        if !p1.scenario_e_stdin_eof.is_empty() {
            let _ = writeln!(
                md,
                "### Scenario e: Server `stdin` closed during hanging turn\n"
            );
            let _ = writeln!(
                md,
                "| Server | Provider | Exit time med / p90 / max (ms) | Clean exit (0) | Drain reply | Survivors at exit (CLI / GC) | Survivors +2s post-exit (CLI / GC) |"
            );
            let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|");
            for r in &p1.scenario_e_stdin_eof {
                let n = r.trials.len();
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {}/{} | {}/{} | {}/{} / {}/{} | {}/{} / {}/{} |",
                    r.target.server,
                    r.target.provider,
                    fmt_summary(r.exit_ms.as_ref()),
                    r.clean_exit_count,
                    n,
                    r.drain_response_count,
                    n,
                    r.cli_survived_exit_runs,
                    n,
                    r.gc_survived_exit_runs,
                    n,
                    r.cli_survived_post_exit_2s_runs,
                    n,
                    r.gc_survived_post_exit_2s_runs,
                    n
                );
            }
            md.push('\n');
        }

        if !p1.scenario_f_failures.is_empty() {
            let _ = writeln!(md, "### Scenario f: Injected failures\n");
            let _ = writeln!(
                md,
                "| Case | Server | Provider | Time med / p90 / max (ms) | Client channel | Structured kind | Exit code | Message / text |"
            );
            let _ = writeln!(md, "|---|---|---|---:|---|---|---:|---|");
            for r in &p1.scenario_f_failures {
                let kind = r
                    .structured_kind_sample
                    .as_deref()
                    .map(|k| format!("`{k}`"))
                    .unwrap_or_else(|| "none".into());
                let code = r
                    .exit_code_sample
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "—".into());
                let mut detail = match &r.diagnostic_sample {
                    Some(diag) => format!("{} (diagnostic: {})", r.message_sample, diag),
                    None => r.message_sample.clone(),
                };
                if let Some(first) = r.trials.first()
                    && let (Some(total), Some(captured)) =
                        (first.stdout_total, first.stdout_captured)
                    && total > captured
                {
                    let _ = write!(detail, " [stdout_bytes={total}, captured={captured}]");
                }
                let _ = writeln!(
                    md,
                    "| `{}` | {} | {} | {} | `{}` | {} | {} | {} |",
                    r.case,
                    r.target.server,
                    r.target.provider,
                    fmt_summary(r.wall_ms.as_ref()),
                    r.channel_sample,
                    kind,
                    code,
                    escape_cell(&detail)
                );
            }
            md.push('\n');
        }

        if !p1.scenario_g_reply.is_empty() {
            let _ = writeln!(
                md,
                "### Scenario g: Reply after start (same server process)\n"
            );
            let _ = writeln!(
                md,
                "| Server | Provider | Bare reply med / p90 / max (ms) | Bare reply OK | Model carried | Effort carried | Access carried | CWD carried | Notes |"
            );
            let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|---:|---|");
            for r in &p1.scenario_g_reply {
                let n = r.trials.len();
                let effort = match r.effort_carried_count {
                    Some(c) => format!("{c}/{n}"),
                    None => "n/a".into(),
                };
                let mut notes = Vec::new();
                if let Some(err) = &r.bare_reply_error_sample {
                    notes.push(format!("bare reply rejected: {err}"));
                }
                if let Some(ok) = r.explicit_model_reply_succeeded_count {
                    notes.push(format!(
                        "with repeated model: OK {ok}/{n} ({} ms), access carried {}/{n}, cwd carried {}/{n}",
                        fmt_summary(r.explicit_model_reply_wall_ms.as_ref()),
                        r.explicit_model_access_carried_count.unwrap_or(0),
                        r.explicit_model_cwd_carried_count.unwrap_or(0),
                    ));
                }
                let note_text = if notes.is_empty() {
                    "—".into()
                } else {
                    escape_cell(&notes.join("; "))
                };
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {}/{} | {}/{} | {} | {}/{} | {}/{} | {} |",
                    r.target.server,
                    r.target.provider,
                    fmt_summary(r.bare_reply_wall_ms.as_ref()),
                    r.bare_reply_succeeded_count,
                    n,
                    r.model_carried_count,
                    n,
                    effort,
                    r.access_carried_count,
                    n,
                    r.cwd_carried_count,
                    n,
                    note_text
                );
            }
            md.push('\n');
        }
    }

    if let Some(p2) = &report.part2 {
        let _ = writeln!(md, "### Part 2: Real models — Scenario a (advisory turn)\n");
        let _ = writeln!(
            md,
            "| Server | Provider | Wall time med / p90 / max (ms) | Result text size med / p90 / max (B) | Canary intact |"
        );
        let _ = writeln!(md, "|---|---|---:|---:|---:|");
        for r in &p2.scenario_a_advisory {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {}/{} |",
                r.target.server,
                r.target.provider,
                fmt_summary(r.wall_ms.as_ref()),
                fmt_bytes_summary(r.result_bytes.as_ref()),
                r.intact_count,
                r.trials.len()
            );
        }
        md.push('\n');

        let _ = writeln!(md, "### Part 2: Real models — Scenario g (start + reply)\n");
        let _ = writeln!(
            md,
            "| Server | Provider | Start med / p90 / max (ms) | Reply med / p90 / max (ms) | Start OK | Reply OK | Codeword recalled |"
        );
        let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|");
        for r in &p2.scenario_g_reply {
            let n = r.trials.len();
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {}/{} | {}/{} | {}/{} |",
                r.target.server,
                r.target.provider,
                fmt_summary(r.start_wall_ms.as_ref()),
                fmt_summary(r.reply_wall_ms.as_ref()),
                r.start_succeeded_count,
                n,
                r.reply_succeeded_count,
                n,
                r.reply_canary_intact_count,
                n
            );
        }
        md.push('\n');
    }

    let _ = writeln!(
        md,
        "### Feature differences (from code, docs, and `tools/list` probe — not scored)\n"
    );
    let _ = writeln!(
        md,
        "| Dimension | Proxenos | claude-delegator 3.5.1 | Source |"
    );
    let _ = writeln!(md, "|---|---|---|---|");
    for row in &report.feature_differences {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} |",
            escape_cell(&row.dimension),
            escape_cell(&row.proxenos),
            escape_cell(&row.claude_delegator),
            escape_cell(&row.source)
        );
    }

    md
}
