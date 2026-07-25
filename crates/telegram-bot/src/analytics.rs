//! The analytics half of the bot: fetch aggregates from the detector, render
//! them, and push them into chat as pictures, text or a file.
//!
//! The bot still talks to exactly two things — the detector and the Telegram
//! Bot API. ClickHouse credentials live in the detector; the bot only ever sees
//! JSON. Rendering happens here because this is the presentation layer.
//!
//! Every chart is sent with a caption carrying the same headline numbers it
//! plots. That is deliberate: a picture is not a readable data source on its
//! own, so nothing in the analytics is reachable *only* by looking at colour.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::charts::plots::{num, rows, text_of};
use crate::charts::{count, dur_ms, pct, plots, usd};
use crate::{esc, Bot};

/// Windows offered in chat. The label is what the user types and what the
/// chart's subtitle shows; the value is what the API parses.
pub const PERIODS: &[&str] = &["24h", "7d", "30d", "90d"];

/// One renderable view: its command, its button label, and the endpoint behind
/// it. Held in one table so the command list, the menu and the callback router
/// can never drift apart.
pub struct Chart {
    pub key: &'static str,
    pub label: &'static str,
    pub blurb: &'static str,
}

pub const CHARTS: &[Chart] = &[
    Chart {
        key: "report",
        label: "📊 Full report",
        blurb: "everything at a glance",
    },
    Chart {
        key: "edge",
        label: "📈 Edge distribution",
        blurb: "where to set your cut-off",
    },
    Chart {
        key: "decay",
        label: "⏱ Edge decay",
        blurb: "how long an arb survives",
    },
    Chart {
        key: "timeline",
        label: "🕒 Over time",
        blurb: "count and edge per bucket",
    },
    Chart {
        key: "markets",
        label: "🏷 Top markets",
        blurb: "which markets produce edges",
    },
    Chart {
        key: "heatmap",
        label: "🗓 When they appear",
        blurb: "weekday × hour",
    },
    Chart {
        key: "size",
        label: "⚖️ Size vs edge",
        blurb: "does your own size eat it",
    },
];

fn valid_period(p: &str) -> &'static str {
    PERIODS
        .iter()
        .find(|x| x.eq_ignore_ascii_case(p))
        .copied()
        .unwrap_or("7d")
}

impl Bot {
    /// GET one analytics endpoint. A 503 means analytics is off or still
    /// connecting — that is a normal state worth showing plainly, not an error.
    async fn analytics(&self, path: &str, query: &str) -> Result<Value> {
        let url = format!("{}/analytics/{path}?{query}", self.cfg.backend_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("backend unreachable")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{}", body.trim_matches('"'));
        }
        serde_json::from_str(&body).context("backend sent malformed analytics")
    }

    /// The menu: one button per view, plus the window switcher.
    pub async fn cmd_analytics(&self, chat_id: &str, arg: &str) {
        let period = valid_period(arg.split_whitespace().next().unwrap_or(""));
        let mut text = String::from(
            "<b>Analytics</b>\n\
             Pick a view. Every chart is also a command — <code>/edge 30d</code>, \
             <code>/decay 24h</code> — and <code>/stats</code> gives the same numbers as text.\n\n",
        );
        for c in CHARTS {
            text.push_str(&format!("{} — <i>{}</i>\n", esc(c.label), esc(c.blurb)));
        }
        text.push_str(&format!("\nwindow: <b>{period}</b>"));

        let mut keyboard: Vec<Vec<Value>> = CHARTS
            .chunks(2)
            .map(|pair| {
                pair.iter()
                    .map(|c| {
                        serde_json::json!({
                            "text": c.label,
                            "callback_data": format!("an:{}:{period}", c.key),
                        })
                    })
                    .collect()
            })
            .collect();
        keyboard.push(period_row("report", period));

        if let Err(e) = self.send_keyboard(chat_id, &text, keyboard).await {
            tracing::warn!("analytics menu failed: {e}");
        }
    }

    /// Render one chart and send it as a photo, captioned with its own numbers
    /// and followed by the window switcher.
    pub async fn send_chart(&self, chat_id: &str, key: &str, period: &str) {
        let period = valid_period(period);
        let tz = self.cfg.tz_offset_minutes;
        let w = format!("window={period}");

        let built = match key {
            "report" => {
                // The dashboard needs five endpoints; fetching them in parallel
                // keeps it a one-round-trip wait rather than five.
                let mk_query = format!("{w}&limit=6");
                let (summary, hist, decay, tl, mk) = tokio::join!(
                    self.analytics("summary", &w),
                    self.analytics("edge_histogram", &w),
                    self.analytics("decay", &w),
                    self.analytics("timeline", &w),
                    self.analytics("markets", &mk_query),
                );
                match (summary, hist, decay, tl, mk) {
                    (Ok(s), Ok(h), Ok(d), Ok(t), Ok(m)) => {
                        plots::report(&s, &h, &d, &t, &m, period)
                            .map(|png| (png, summary_caption(&s, period)))
                    }
                    (Err(e), ..) => Err(e),
                    (_, Err(e), ..) => Err(e),
                    (_, _, Err(e), ..) => Err(e),
                    (_, _, _, Err(e), _) => Err(e),
                    (_, _, _, _, Err(e)) => Err(e),
                }
            }
            "edge" => match self.analytics("edge_histogram", &w).await {
                Ok(v) => plots::edge_histogram(&v, period).map(|p| (p, edge_caption(&v))),
                Err(e) => Err(e),
            },
            "decay" => match self.analytics("decay", &w).await {
                Ok(v) => plots::decay(&v, period).map(|p| (p, decay_caption(&v))),
                Err(e) => Err(e),
            },
            "timeline" => match self.analytics("timeline", &w).await {
                Ok(v) => plots::timeline(&v, period, tz).map(|p| (p, timeline_caption(&v))),
                Err(e) => Err(e),
            },
            "markets" => match self.analytics("markets", &format!("{w}&limit=14")).await {
                Ok(v) => plots::markets(&v, period).map(|p| (p, markets_caption(&v))),
                Err(e) => Err(e),
            },
            "heatmap" => match self.analytics("heatmap", &format!("{w}&tz={tz}")).await {
                Ok(v) => plots::heatmap(&v, period).map(|p| (p, heatmap_caption(&v))),
                Err(e) => Err(e),
            },
            "size" => match self.analytics("size_curve", &w).await {
                Ok(v) => plots::size_curve(&v, period).map(|p| (p, size_caption(&v))),
                Err(e) => Err(e),
            },
            other => Err(anyhow::anyhow!("unknown chart '{other}'")),
        };

        match built {
            Ok((png, caption)) => {
                let keyboard = vec![period_row(key, period)];
                if let Err(e) = self.send_photo(chat_id, png, &caption, keyboard).await {
                    tracing::warn!("send chart failed: {e}");
                    let _ = self
                        .send(
                            chat_id,
                            &format!("chart send failed: {}", esc(&e.to_string())),
                        )
                        .await;
                }
            }
            Err(e) => {
                let _ = self
                    .send(chat_id, &format!("📉 {}", esc(&e.to_string())))
                    .await;
            }
        }
    }

    /// The whole summary as text — the readable twin of the charts, and the
    /// version that survives being forwarded or read on a slow connection.
    pub async fn cmd_stats(&self, arg: &str) -> String {
        let period = valid_period(arg.split_whitespace().next().unwrap_or(""));
        let v = match self.analytics("summary", &format!("window={period}")).await {
            Ok(v) => v,
            Err(e) => return format!("📉 {}", esc(&e.to_string())),
        };

        let by = rows(&v, "by_strategy");
        let life = rows(&v, "lifetime");
        if by.is_empty() {
            return format!(
                "<b>Analytics — {period}</b>\n\nNo opportunities recorded in this window.\n\
                 <i>Real set-arb is rare; an empty window is usually correct.</i>"
            );
        }

        let mut s = format!("<b>Analytics — {period}</b>\n");
        for r in &by {
            let name = text_of(r, "strategy");
            let occ = num(r, "occurrences");
            let prof = num(r, "profitable");
            s.push_str(&format!(
                "\n<b>{}</b>\n\
                 occurrences: <b>{}</b> on {} markets\n\
                 cleared costs: <b>{}</b> ({:.0}%)\n\
                 edge — median <b>{}</b> · p90 <b>{}</b> · best <b>{}</b>\n\
                 lost to depth: <code>{}</code> per set (top-of-book {} → realised {})\n\
                 size: <code>{:.0}</code> sets  capital: <code>{}</code>\n\
                 fees: <code>{}</code>  gas: <code>{}</code>  theoretical net: <b>{}</b>\n\
                 detect latency: <code>{:.0}µs</code> (p90 <code>{:.0}µs</code>)\n",
                esc(&name),
                count(occ),
                count(num(r, "markets")),
                count(prof),
                if occ > 0.0 { prof / occ * 100.0 } else { 0.0 },
                pct(num(r, "edge_p50")),
                pct(num(r, "edge_p90")),
                pct(num(r, "edge_max")),
                pct(num(r, "depth_haircut")),
                pct(num(r, "gross_avg")),
                pct(num(r, "edge_avg")),
                num(r, "size_avg"),
                usd(num(r, "capital_avg")),
                usd(num(r, "fee_avg")),
                usd(num(r, "gas_avg")),
                usd(num(r, "net_total")),
                num(r, "latency_us_avg"),
                num(r, "latency_us_p90"),
            ));
            if let Some(l) = life.iter().find(|l| text_of(l, "strategy") == name) {
                let closed = num(l, "closed");
                s.push_str(&format!(
                    "lifetime — median <b>{}</b> · p90 <b>{}</b> · longest <b>{}</b>\n\
                     gone inside 250ms: <b>{:.0}%</b> of {} closed\n",
                    dur_ms(num(l, "lived_p50")),
                    dur_ms(num(l, "lived_p90")),
                    dur_ms(num(l, "lived_max")),
                    if closed > 0.0 {
                        num(l, "under_250ms") / closed * 100.0
                    } else {
                        0.0
                    },
                    count(closed),
                ));
            }
        }

        let cov = v.get("coverage").cloned().unwrap_or_default();
        s.push_str(&format!(
            "\n<i>coverage: {} book samples across {} markets · {} trade prints \
             ({}) · {} execution rows</i>\n\
             <i>Detection only — nothing here was traded. \"Theoretical net\" is what \
             the book offered before execution risk.</i>",
            count(num(&cov, "book_samples")),
            count(num(&cov, "markets_sampled")),
            count(num(&cov, "trades")),
            usd(num(&cov, "traded_usd")),
            count(num(&cov, "executions")),
        ));
        s
    }

    /// Ship raw rows as a CSV file, for anyone who would rather use their own
    /// tooling than my charts.
    pub async fn cmd_export(&self, chat_id: &str, arg: &str) {
        let mut it = arg.split_whitespace();
        let table = it.next().unwrap_or("opportunities").to_lowercase();
        let period = valid_period(it.next().unwrap_or(""));

        let url = format!(
            "{}/analytics/export?table={table}&window={period}&limit=100000",
            self.cfg.backend_url
        );
        let csv = match self.http.get(&url).send().await {
            Ok(r) if r.status().is_success() => r.bytes().await.unwrap_or_default(),
            Ok(r) => {
                let msg = r.text().await.unwrap_or_default();
                let _ = self
                    .send(chat_id, &format!("📉 {}", esc(msg.trim_matches('"'))))
                    .await;
                return;
            }
            Err(e) => {
                let _ = self
                    .send(
                        chat_id,
                        &format!("backend unreachable: {}", esc(&e.to_string())),
                    )
                    .await;
                return;
            }
        };

        // A header-only file means the window is empty; say so rather than
        // sending a file that looks broken.
        let lines = csv.iter().filter(|b| **b == b'\n').count();
        if lines <= 1 {
            let _ = self
                .send(
                    chat_id,
                    &format!("No <code>{}</code> rows in the last {period}.", esc(&table)),
                )
                .await;
            return;
        }

        let caption = format!(
            "<b>{}</b> · last {period} · {} rows",
            esc(&table),
            lines.saturating_sub(1)
        );
        let name = format!("arb-{table}-{period}.csv");
        if let Err(e) = self
            .send_document(chat_id, csv.to_vec(), &name, &caption)
            .await
        {
            tracing::warn!("csv export failed: {e}");
        }
    }

    // --- Telegram transport ------------------------------------------------

    async fn send_keyboard(
        &self,
        chat_id: &str,
        text: &str,
        keyboard: Vec<Vec<Value>>,
    ) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
            "reply_markup": { "inline_keyboard": keyboard },
        });
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("sendMessage failed")?
            .error_for_status()
            .context("sendMessage error status")?;
        Ok(())
    }

    async fn send_photo(
        &self,
        chat_id: &str,
        png: Vec<u8>,
        caption: &str,
        keyboard: Vec<Vec<Value>>,
    ) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("chart.png")
            .mime_str("image/png")?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", clamp_caption(caption))
            .text("parse_mode", "HTML")
            .text(
                "reply_markup",
                serde_json::json!({ "inline_keyboard": keyboard }).to_string(),
            )
            .part("photo", part);

        self.http
            .post(format!("{}/sendPhoto", self.api_base))
            .multipart(form)
            .send()
            .await
            .context("sendPhoto failed")?
            .error_for_status()
            .context("sendPhoto error status")?;
        Ok(())
    }

    async fn send_document(
        &self,
        chat_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        caption: &str,
    ) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("text/csv")?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", clamp_caption(caption))
            .text("parse_mode", "HTML")
            .part("document", part);

        self.http
            .post(format!("{}/sendDocument", self.api_base))
            .multipart(form)
            .send()
            .await
            .context("sendDocument failed")?
            .error_for_status()
            .context("sendDocument error status")?;
        Ok(())
    }
}

/// The window switcher, with the active one marked so the keyboard says which
/// slice you are looking at.
fn period_row(chart: &str, active: &str) -> Vec<Value> {
    PERIODS
        .iter()
        .map(|p| {
            serde_json::json!({
                "text": if *p == active { format!("· {p} ·") } else { p.to_string() },
                "callback_data": format!("an:{chart}:{p}"),
            })
        })
        .collect()
}

/// Telegram caps a caption at 1024 characters; cut on a line boundary so the
/// last thing shown is never half a number.
fn clamp_caption(s: &str) -> String {
    const LIMIT: usize = 1_000;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let mut out = String::new();
    for line in s.lines() {
        if out.chars().count() + line.chars().count() + 1 > LIMIT {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push('…');
    out
}

// --- Captions -------------------------------------------------------------
//
// Each chart's numbers in text, so the picture is never the only way to read
// them.

fn summary_caption(v: &Value, period: &str) -> String {
    let by = rows(v, "by_strategy");
    let life = rows(v, "lifetime");
    let occ: f64 = by.iter().map(|r| num(r, "occurrences")).sum();
    let prof: f64 = by.iter().map(|r| num(r, "profitable")).sum();
    let net: f64 = by.iter().map(|r| num(r, "net_total")).sum();
    let lived = life.iter().map(|r| num(r, "lived_p50")).fold(0.0, f64::max);
    format!(
        "<b>Arbitrage analytics — {period}</b>\n\
         {} opportunities · {:.0}% cleared costs · median life {} · theoretical net {}\n\
         <i>/stats for the full numbers as text</i>",
        count(occ),
        if occ > 0.0 { prof / occ * 100.0 } else { 0.0 },
        dur_ms(lived),
        usd(net),
    )
}

fn edge_caption(v: &Value) -> String {
    let all = rows(v, "rows");
    let bucket = v.get("bucket").and_then(Value::as_f64).unwrap_or(0.0025);
    let total: f64 = all.iter().map(|r| num(r, "n")).sum();
    if total <= 0.0 {
        return "<b>Edge distribution</b>\nNothing recorded in this window.".into();
    }
    let kept = |threshold: f64| -> f64 {
        let min_bin = (threshold / bucket).round();
        all.iter()
            .filter(|r| num(r, "bin") >= min_bin)
            .map(|r| num(r, "n"))
            .sum::<f64>()
            / total
            * 100.0
    };
    format!(
        "<b>Edge distribution</b>\n\
         {} occurrences. Raising the cut-off keeps: \
         ≥0.5% → <b>{:.0}%</b> · ≥1% → <b>{:.0}%</b> · ≥2% → <b>{:.0}%</b> · ≥3% → <b>{:.0}%</b>\n\
         <i>Fewer, fatter signals beat many thin ones once gas is fixed per trade.</i>",
        count(total),
        kept(0.005),
        kept(0.01),
        kept(0.02),
        kept(0.03),
    )
}

fn decay_caption(v: &Value) -> String {
    let all = rows(v, "rows");
    if all.is_empty() {
        return "<b>Edge decay</b>\nNo closed crossings in this window.".into();
    }
    let mut s = String::from("<b>Edge decay</b>\n");
    for r in &all {
        let n = num(r, "n");
        s.push_str(&format!(
            "{}: {} closed · median <b>{}</b> · p90 {} · p99 {}\n",
            esc(&text_of(r, "strategy")),
            count(n),
            dur_ms(num(r, "p50")),
            dur_ms(num(r, "p90")),
            dur_ms(num(r, "p99")),
        ));
    }
    s.push_str("<i>If the median is under a few hundred ms, this is a latency race.</i>");
    s
}

fn timeline_caption(v: &Value) -> String {
    let all = rows(v, "rows");
    let total: f64 = all.iter().map(|r| num(r, "n")).sum();
    let best = all.iter().map(|r| num(r, "edge_max")).fold(0.0, f64::max);
    format!(
        "<b>Opportunities over time</b>\n\
         {} in total · best single edge {}\n\
         <i>Counts and edge share one time axis, in two panels — never two y-scales.</i>",
        count(total),
        pct(best),
    )
}

fn markets_caption(v: &Value) -> String {
    let all = rows(v, "rows");
    if all.is_empty() {
        return "<b>Top markets</b>\nNothing recorded in this window.".into();
    }
    let mut s = String::from("<b>Top markets</b>\n");
    for r in all.iter().take(3) {
        s.push_str(&format!(
            "• {} — {} times, avg {}\n",
            esc(&text_of(r, "slug")),
            count(num(r, "occurrences")),
            pct(num(r, "edge_avg")),
        ));
    }
    s.push_str("<i>Frequency is not opportunity: check the median life beside it.</i>");
    s
}

fn heatmap_caption(v: &Value) -> String {
    let all = rows(v, "rows");
    let total: f64 = all.iter().map(|r| num(r, "n")).sum();
    let hottest = all
        .iter()
        .max_by(|a, b| num(a, "n").partial_cmp(&num(b, "n")).unwrap());
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let peak = hottest.map_or("—".to_string(), |r| {
        let d = (num(r, "dow") as usize).clamp(1, 7) - 1;
        format!(
            "{} {:02}:00 ({})",
            DAYS[d],
            num(r, "hour"),
            count(num(r, "n"))
        )
    });
    format!(
        "<b>When opportunities appear</b>\n\
         {} in total · busiest slot: <b>{peak}</b>\n\
         <i>Shown at your configured timezone.</i>",
        count(total),
    )
}

fn size_caption(v: &Value) -> String {
    let all = rows(v, "rows");
    if all.is_empty() {
        return "<b>Size vs edge</b>\nNothing recorded in this window.".into();
    }
    let worst = all
        .iter()
        .max_by(|a, b| {
            num(a, "depth_haircut")
                .partial_cmp(&num(b, "depth_haircut"))
                .unwrap()
        })
        .unwrap();
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let bucket = labels
        .get(num(worst, "bucket") as usize)
        .and_then(Value::as_str)
        .unwrap_or("?");
    format!(
        "<b>Does size eat the edge?</b>\n\
         Worst depth cost at <b>{}</b> sets: <b>{}</b> per set ({} on the trade)\n\
         <i>The bar is what clears; the gap up to the marker is your own sweep down the ladder.</i>",
        esc(bucket),
        pct(num(worst, "depth_haircut")),
        usd(num(worst, "depth_cost_usd")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_windows_fall_back_instead_of_reaching_the_api() {
        assert_eq!(valid_period("30d"), "30d");
        assert_eq!(valid_period("24H"), "24h");
        assert_eq!(valid_period(""), "7d");
        assert_eq!(valid_period("'; DROP TABLE --"), "7d");
    }

    #[test]
    fn the_window_row_marks_the_active_slice() {
        let row = period_row("edge", "7d");
        assert_eq!(row.len(), PERIODS.len());
        assert_eq!(row[1]["text"], "· 7d ·");
        assert_eq!(row[0]["text"], "24h");
        assert_eq!(row[0]["callback_data"], "an:edge:24h");
    }

    #[test]
    fn captions_are_cut_on_a_line_not_mid_number() {
        let long = (0..200)
            .map(|i| format!("line {i} with some padding text"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = clamp_caption(&long);
        assert!(out.chars().count() <= 1_010);
        assert!(out.ends_with('…'));
        // Whatever survived is whole lines, so no half-rendered figure.
        assert!(out.trim_end_matches('…').ends_with('\n'));
        // Short captions pass through untouched.
        assert_eq!(clamp_caption("short"), "short");
    }

    #[test]
    fn every_chart_key_is_routable() {
        // The menu, the commands and the callback router all read this table,
        // so a key here with no branch in send_chart would be a dead button.
        for c in CHARTS {
            assert!(!c.key.is_empty() && !c.label.is_empty());
            assert!(
                matches!(
                    c.key,
                    "report" | "edge" | "decay" | "timeline" | "markets" | "heatmap" | "size"
                ),
                "unrouted chart key: {}",
                c.key
            );
        }
    }

    #[test]
    fn captions_survive_an_empty_window() {
        let empty = serde_json::json!({ "rows": [], "by_strategy": [], "lifetime": [] });
        for caption in [
            summary_caption(&empty, "7d"),
            edge_caption(&empty),
            decay_caption(&empty),
            timeline_caption(&empty),
            markets_caption(&empty),
            heatmap_caption(&empty),
            size_caption(&empty),
        ] {
            assert!(!caption.is_empty());
            assert!(!caption.contains("NaN"), "NaN leaked into a caption");
        }
    }
}
