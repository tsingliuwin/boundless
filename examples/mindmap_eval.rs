//! Headless mind-map evaluation loop.
//!
//! Runs the drawing agent against the fixed exam prompt (高效学习方法思维导图)
//! without any GPUI/UI, replays the emitted CanvasOps into a virtual canvas,
//! and scores the result against the mind-map acceptance criteria in
//! `ai::eval::evaluate_mindmap`. The full tool-call JSONL log (same machinery
//! the app uses) and the review report are written to `~/.boundless/agent-logs/`
//! for later analysis.
//!
//! Usage: `cargo run --example mindmap_eval`
//! Exit code: 0 = 达标, 1 = 未达标, 2 = 运行失败（配置/网络等）。

use boundless::ai::agent::{AgentEvent, BoundlessAgent};
use boundless::ai::eval;
use boundless::ai::settings::AiSettings;
use futures::StreamExt;
use std::sync::{Arc, Mutex};

/// The fixed exam prompt — keep it identical across runs so rounds are
/// comparable. The structural minimums it grades against live in
/// `SYSTEM_PROMPT` (4~6 一级分支 × 2~5 要点) and `eval::evaluate_mindmap`.
const EXAM_PROMPT: &str = "请画一张关于「高效学习方法」的思维导图，结构清晰、分支均衡、要点精炼。";

fn main() {
    if let Err(e) = run() {
        eprintln!("评测运行失败: {e:#}");
        std::process::exit(2);
    }
}

fn run() -> anyhow::Result<()> {
    let settings = AiSettings::load();
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("未配置 API Key（~/.boundless/config.json 或环境变量 OPENAI_API_KEY）");
    }

    let snapshot = Arc::new(Mutex::new(Vec::new()));
    let request = BoundlessAgent::stream(
        &settings,
        EXAM_PROMPT.to_string(),
        Vec::new(),
        snapshot.clone(),
        String::new(),
    )?;
    println!("模型: {} — 考题[思维导图]: {EXAM_PROMPT}", settings.model);

    let log_path = boundless::ai::log::begin_run(EXAM_PROMPT, &settings.model);

    // The virtual board: CanvasOps are applied here live (mirroring the
    // board's semantics via eval::apply) and the tool's oneshot reply is
    // answered with the outcome — an unanswered reply reads as
    // "画布操作被取消" on the tool side and kills the whole run.
    let mut canvas = eval::VirtualCanvas::default();
    let mut total_calls = 0usize;
    let mut drew = false;
    let mut final_text = String::new();
    let mut run_error: Option<String> = None;

    futures::executor::block_on(async {
        let mut events = request.events;
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::CanvasOp {
                    op,
                    pre_assigned_id,
                    reply,
                } => {
                    let assigned = pre_assigned_id.map(|u| u.to_string()[..8].to_string());
                    let outcome = eval::apply(&mut canvas, &op, assigned.as_deref())
                        .map_err(boundless::ai::canvas_ops::CanvasOpError::invalid_args);
                    let applied = outcome.is_ok();
                    let _ = reply.send(outcome);
                    // Refresh the shared snapshot after every applied op —
                    // exactly what the in-app panel does — so list_elements
                    // and update/delete id checks see live state.
                    if applied {
                        *snapshot.lock().unwrap_or_else(|e| e.into_inner()) = canvas.snapshot();
                    }
                }
                AgentEvent::ToolCall { name, args, .. } => {
                    total_calls += 1;
                    boundless::ai::log::log_tool_call(&name, &args);
                }
                AgentEvent::ToolResult {
                    result, is_error, ..
                } => {
                    boundless::ai::log::log_tool_result(is_error, &result);
                }
                AgentEvent::Done {
                    text,
                    drew_anything,
                } => {
                    drew = drew_anything;
                    final_text = text;
                    break;
                }
                AgentEvent::Error(e) => {
                    run_error = Some(e);
                    break;
                }
                _ => {}
            }
        }
    });

    if let Some(e) = &run_error {
        boundless::ai::log::log_error(e);
    }
    boundless::ai::log::end_run(drew, &final_text);
    if let Some(p) = &log_path {
        println!("执行日志: {}", p.display());
    }

    if let Some(e) = run_error {
        anyhow::bail!("agent 运行出错: {e}");
    }

    // The model narrated but drew nothing — report and fail.
    let report = eval::evaluate_mindmap(&canvas, drew, total_calls);
    let report_text = report.to_text();
    println!("{report_text}");

    if let Some(log_path) = &log_path {
        let report_path = log_path.with_extension("report.txt");
        std::fs::write(&report_path, &report_text)?;
        println!("评测报告: {}", report_path.display());
    }
    println!("最终回复: {}", final_text.trim());

    if !report.passed {
        std::process::exit(1);
    }
    Ok(())
}
