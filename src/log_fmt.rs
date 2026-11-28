use crate::types::{AnalystRole, PipelineTrace};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const WHITE: &str = "\x1b[37m";

const B_RED: &str = "\x1b[91m";
const B_GREEN: &str = "\x1b[92m";
const B_YELLOW: &str = "\x1b[93m";
const B_BLUE: &str = "\x1b[94m";
const B_MAGENTA: &str = "\x1b[95m";
const B_CYAN: &str = "\x1b[96m";
const B_WHITE: &str = "\x1b[97m";

fn stance_color(stance: &str) -> &'static str {
    let s = stance.to_ascii_lowercase();
    if s.contains("bull") || s.contains("buy") || s.contains("approve") || s.contains("long") {
        B_GREEN
    } else if s.contains("bear") || s.contains("sell") || s.contains("reject") || s.contains("short") {
        B_RED
    } else {
        B_YELLOW
    }
}

fn role_style(role: AnalystRole) -> (&'static str, &'static str) {
    match role {
        AnalystRole::Technical => (B_BLUE, "Technical Analyst"),
        AnalystRole::OptionsVolatility => (B_MAGENTA, "Options/IV Analyst"),
        AnalystRole::RegimeSentiment => (B_YELLOW, "Regime/Sentiment Analyst"),
    }
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let single_line = s.replace(['\r', '\n'], " ");
    let trimmed = single_line.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(max_chars.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

/// Print a colorful breakdown of every agent's findings and the final trade decision.
pub fn print_pipeline_trace(context_label: &str, trace: &PipelineTrace) {
    eprintln!(
        "\n{BOLD}{B_CYAN}══════════════════════════════════════════════════════════════════════{RESET}"
    );
    eprintln!(
        "{BOLD}{B_CYAN}[AGENT PIPELINE]{RESET} {B_WHITE}{context_label}{RESET} {DIM}| memories_recalled={}{RESET}",
        trace.retrieved_memories.len()
    );

    // Stage 1: Analysts
    eprintln!("{BOLD}{CYAN}── Stage 1: Analysts ─────────────────────────────────────────────────{RESET}");
    for report in &trace.analyst_reports {
        let (r_color, r_name) = role_style(report.role);
        let s_color = stance_color(&report.stance);
        let conf = report.confidence_bps as f64 / 100.0;
        eprintln!(
            "  {BOLD}{r_color}{r_name:<24}{RESET} {s_color}{:<10}{RESET} {DIM}({conf:>5.1}%){RESET} {}",
            report.stance.to_uppercase(),
            truncate_str(&report.summary, 140),
        );
        if !report.key_levels.is_empty() {
            eprintln!(
                "    {DIM}levels: {WHITE}{}{RESET}",
                report.key_levels.join(", ")
            );
        }
        if !report.evidence.trim().is_empty() {
            eprintln!(
                "    {DIM}evidence: {}{RESET}",
                truncate_str(&report.evidence, 140)
            );
        }
    }

    // Stage 2: Bull/Bear Debate & Research Manager
    eprintln!("{BOLD}{CYAN}── Stage 2: Bull vs Bear & Research Manager ──────────────────────────{RESET}");
    if let Some(bull) = trace.invest_debate.bull_history.last() {
        eprintln!("  {B_GREEN}Bull Case:{RESET} {}", truncate_str(bull, 150));
    }
    if let Some(bear) = trace.invest_debate.bear_history.last() {
        eprintln!("  {B_RED}Bear Case:{RESET} {}", truncate_str(bear, 150));
    }
    let rm = &trace.research_decision;
    let rm_color = stance_color(&rm.stance);
    let rm_conf = rm.confidence_bps as f64 / 100.0;
    let right = rm.preferred_right.as_deref().unwrap_or("-");
    eprintln!(
        "  {BOLD}{B_WHITE}Research Verdict:{RESET} {rm_color}{}{RESET} {DIM}({rm_conf:.1}%, right={B_CYAN}{right}{DIM}){RESET}",
        rm.stance.to_uppercase()
    );
    eprintln!("    {DIM}thesis:{RESET} {}", truncate_str(&rm.thesis, 160));
    if !rm.investment_plan.trim().is_empty() {
        eprintln!("    {DIM}plan:{RESET}   {}", truncate_str(&rm.investment_plan, 160));
    }
    if !rm.key_risks.is_empty() {
        eprintln!("    {DIM}risks:{RESET}  {YELLOW}{}{RESET}", rm.key_risks.join(" | "));
    }

    // Stage 3: Trader Proposal
    eprintln!("{BOLD}{CYAN}── Stage 3: Trader Proposal ──────────────────────────────────────────{RESET}");
    let tp = &trace.trader_proposal;
    let tp_color = stance_color(&tp.action);
    let tp_conf = tp.confidence_bps as f64 / 100.0;
    eprintln!(
        "  {BOLD}{B_WHITE}Action:{RESET} {BOLD}{tp_color}{}{RESET} {DIM}({tp_conf:.1}%){RESET}",
        tp.action.to_uppercase()
    );
    eprintln!("    {DIM}why:{RESET} {}", truncate_str(&tp.rationale, 180));
    if let Some(sig) = &tp.signal {
        eprintln!(
            "    {B_CYAN}plan:{RESET} {B_WHITE}{}{RESET} qty={} entry={B_YELLOW}₹{:.2}{RESET} | target={B_GREEN}₹{:.2}{RESET} | SL={B_RED}₹{:.2}{RESET} | TSL={MAGENTA}₹{:.2}{RESET}",
            sig.symbol,
            sig.quantity,
            sig.entry_price,
            sig.target_price,
            sig.stop_loss_price,
            sig.trailing_stop_loss_price,
        );
    }

    // Stage 4: Risk Committee
    eprintln!("{BOLD}{CYAN}── Stage 4: Risk Committee Verdict ───────────────────────────────────{RESET}");
    let rv = &trace.risk_verdict;
    let rv_color = if rv.approved { B_GREEN } else { B_RED };
    let rv_label = if rv.approved { "APPROVED" } else { "REJECTED" };
    let rv_conf = rv.adjusted_confidence_bps as f64 / 100.0;
    eprintln!(
        "  {BOLD}{rv_color}{rv_label}{RESET} {DIM}(decision={}, adj_conf={rv_conf:.1}%){RESET}",
        rv.decision
    );
    eprintln!("    {DIM}verdict:{RESET} {}", truncate_str(&rv.verdict_rationale, 180));

    match &trace.final_signal {
        Some(sig) => {
            eprintln!(
                "  {BOLD}{B_GREEN}▶ FINAL DECISION: ENTER {:?} {} @ ₹{:.2} (Target ₹{:.2} | SL ₹{:.2} | TSL ₹{:.2}){RESET}",
                sig.side,
                sig.symbol,
                sig.entry_price,
                sig.target_price,
                sig.stop_loss_price,
                sig.trailing_stop_loss_price,
            );
        }
        None => {
            eprintln!("  {DIM}▶ FINAL DECISION: HOLD / NO TRADE{RESET}");
        }
    }
    eprintln!(
        "{BOLD}{B_CYAN}══════════════════════════════════════════════════════════════════════{RESET}"
    );
}

pub fn print_trade_entry(
    context_label: &str,
    symbol: &str,
    qty: u32,
    entry: f64,
    target: f64,
    sl: f64,
    tsl: f64,
    fees: f64,
    rationale: &str,
) {
    eprintln!(
        "{BOLD}{B_GREEN}▲ [TRADE ENTERED]{RESET} {B_WHITE}{context_label}{RESET} | {BOLD}{B_CYAN}{symbol}{RESET} qty={qty} @ {B_YELLOW}₹{entry:.2}{RESET} (fees ₹{fees:.2}) | Target {B_GREEN}₹{target:.2}{RESET} | SL {B_RED}₹{sl:.2}{RESET} | TSL {MAGENTA}₹{tsl:.2}{RESET}"
    );
    if !rationale.trim().is_empty() {
        eprintln!("  {GREEN}reason:{RESET} {}", truncate_str(rationale, 180));
    }
}

pub fn print_trade_exit(
    context_label: &str,
    symbol: &str,
    reason: &str,
    entry: f64,
    exit: f64,
    pnl_rupees: f64,
    return_bps: i32,
) {
    let (badge_color, arrow) = if pnl_rupees >= 0.0 {
        (B_GREEN, "✔")
    } else {
        (B_RED, "✖")
    };
    let sign = if pnl_rupees >= 0.0 { "+" } else { "" };
    eprintln!(
        "{BOLD}{badge_color}{arrow} [TRADE CLOSED]{RESET} {B_WHITE}{context_label}{RESET} | {B_CYAN}{symbol}{RESET} ({YELLOW}{reason}{RESET}) | ₹{entry:.2} → ₹{exit:.2} | {BOLD}{badge_color}P&L: {sign}₹{pnl_rupees:.2} ({return_bps:+} bps){RESET}"
    );
}

pub fn print_trade_rejected(context_label: &str, reason: &str) {
    eprintln!(
        "{YELLOW}⚠ [TRADE SKIPPED]{RESET} {B_WHITE}{context_label}{RESET} | {DIM}{reason}{RESET}"
    );
}

pub fn print_backtest_progress(
    elapsed_s: u64,
    pct: usize,
    bar: usize,
    total_bars: usize,
    ts_ist: &str,
    spot: f64,
    trades: usize,
    has_position: bool,
    eta_str: &str,
) {
    let pos_badge = if has_position {
        format!("{BOLD}{B_GREEN}POSITION{RESET}")
    } else {
        format!("{DIM}{WHITE}FLAT{RESET}")
    };
    eprintln!(
        "{CYAN}[backtest {elapsed_m:>3}m{elapsed_sec:02}s]{RESET} {BOLD}{B_YELLOW}{pct:>3}%{RESET} | bar {B_WHITE}{bar}/{total_bars}{RESET} | {BLUE}{ts_ist} IST{RESET} | spot={B_CYAN}{spot:.0}{RESET} | trades={B_MAGENTA}{trades}{RESET} | {pos_badge} {DIM}{eta_str}{RESET}",
        elapsed_m = elapsed_s / 60,
        elapsed_sec = elapsed_s % 60,
    );
}
