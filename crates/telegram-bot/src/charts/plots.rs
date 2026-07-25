//! The chart set. Each function takes the detector's analytics JSON verbatim
//! and returns a PNG.
//!
//! Two rules run through all of them. **One axis**: where two measures of
//! different scale belong together (a count and an edge, a count and a survival
//! share) they get stacked panels on a shared x, never a second y-scale — a
//! dual axis invents a correlation the data does not contain. And **labels are
//! selective**: the extreme, the endpoint, or the threshold that answers the
//! question, never a number on every mark.

use anyhow::Result;
use serde_json::Value;

use super::*;

const W: u32 = 1200;
const H: u32 = 760;

// --- JSON access ----------------------------------------------------------
//
// ClickHouse returns 64-bit integers as JSON *strings* (they do not survive a
// double), so every numeric read has to accept both.

pub fn num(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

pub fn text_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn rows<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn labels(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Strategies present in a row set, mint first so the legend order is stable.
fn strategies(rows: &[&Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let s = text_of(r, "strategy");
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out.sort();
    out
}

// --- Plot furniture -------------------------------------------------------

#[derive(Clone, Copy)]
struct Panel {
    x0: i32,
    x1: i32,
    y0: i32,
    y1: i32,
}

impl Panel {
    /// Centre and available width of band `i` of `n` across the panel.
    fn band(&self, i: usize, n: usize) -> (i32, i32) {
        let n = n.max(1) as i32;
        let w = (self.x1 - self.x0) / n;
        (self.x0 + w * i as i32 + w / 2, w)
    }
}

/// Grouped columns: one band per category, one column per series inside it,
/// separated by the 2px surface gap.
#[allow(clippy::too_many_arguments)]
fn grouped_columns(
    c: &Canvas,
    p: &Panel,
    s: &Scale,
    nbands: usize,
    series: &[(RGBColor, Vec<f64>)],
    label_top: bool,
    fmt: &dyn Fn(f64) -> String,
) {
    let k = series.len().max(1) as i32;
    for band in 0..nbands {
        let (cx, bw) = p.band(band, nbands);
        // Leave a third of the band as air even before the 24px cap applies.
        let inner = (bw as f64 * 0.66) as i32;
        let each = ((inner - GAP * (k - 1)) / k).clamp(3, BAR_MAX);
        let total = each * k + GAP * (k - 1);
        for (i, (color, values)) in series.iter().enumerate() {
            let v = values.get(band).copied().unwrap_or(0.0);
            let x = cx - total / 2 + (each + GAP) * i as i32 + each / 2;
            let top = s.map(v);
            c.column(x, each, top, p.y1, *color);
            // Exactly one number per series — the peak. Labelling every column
            // is noise, and it is the extreme that the reader is looking for.
            if label_top && v > 0.0 && argmax(values) == Some(band) {
                c.text(x, top - 12, &fmt(v), 13, INK2, Align::Center);
            }
        }
    }
}

/// Index of the largest value, or `None` when they are all zero. First wins on
/// a tie, so exactly one mark is ever labelled.
fn argmax(values: &[f64]) -> Option<usize> {
    values
        .iter()
        .enumerate()
        .filter(|(_, v)| **v > 0.0)
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .map(|(i, _)| i)
}

/// Category labels under a banded x axis, thinned until they stop colliding.
fn band_labels(c: &Canvas, p: &Panel, names: &[String], size: i32) {
    let widest = names
        .iter()
        .map(|n| c.measure(n, size, false).0)
        .max()
        .unwrap_or(0);
    let band_w = (p.x1 - p.x0) / names.len().max(1) as i32;
    let step = ((widest + 12) as f64 / band_w.max(1) as f64)
        .ceil()
        .max(1.0) as usize;
    for (i, name) in names.iter().enumerate() {
        if i % step != 0 {
            continue;
        }
        let (cx, _) = p.band(i, names.len());
        c.text(cx, p.y1 + 18, name, size, MUTED, Align::Center);
    }
}

fn axis_rule(c: &Canvas, p: &Panel) {
    c.hairline_h(p.x0, p.x1, p.y1, AXIS);
}

/// A plotted line ready for its end-label decision: the series name, its
/// pixel points, and the value at its right edge.
type EndLabelled = (String, Vec<(i32, i32)>, f64);

/// A series as a 2px line with an end marker and one direct end-label.
fn line_series(c: &Canvas, pts: &[(i32, i32)], color: RGBColor, end_label: Option<&str>) {
    c.line(pts, color);
    if let Some(&(x, y)) = pts.last() {
        c.marker(x, y, 4, color);
        if let Some(l) = end_label {
            c.text(x - 10, y - 16, l, 13, INK2, Align::Right);
        }
    }
}

// --- 1. Edge distribution -------------------------------------------------

/// Where the edges actually land, and what a given cut-off would keep. The
/// bottom panel is the one that answers "should I arb from 0.5% or from 2%?":
/// it is the share of occurrences that survive a threshold, so a cliff in the
/// curve is a threshold worth sitting just below.
pub fn edge_histogram(v: &Value, window: &str) -> Result<Vec<u8>> {
    let bucket = v.get("bucket").and_then(Value::as_f64).unwrap_or(0.0025);
    let all = rows(v, "rows");
    let strats = strategies(&all);

    // Bins present, capped so a lone outlier can't stretch the axis flat.
    let nbins = all
        .iter()
        .map(|r| num(r, "bin") as usize + 1)
        .max()
        .unwrap_or(0)
        .clamp(1, 40);

    render(W, H, |c| {
        let top = header(
            c,
            "Edge distribution at signal",
            &format!(
                "Realised (volume-weighted) edge per set · {window} · bucket {}",
                pct(bucket)
            ),
        );
        if all.is_empty() {
            empty_note(c, top, "No opportunities recorded in this window.");
            return Ok(());
        }

        let names: Vec<String> = (0..nbins).map(|i| pct(i as f64 * bucket)).collect();
        let counts: Vec<(RGBColor, Vec<f64>)> = strats
            .iter()
            .map(|s| {
                let mut v = vec![0.0; nbins];
                for r in all.iter().filter(|r| text_of(r, "strategy") == *s) {
                    let b = num(r, "bin") as usize;
                    if b < nbins {
                        v[b] = num(r, "n");
                    }
                }
                (strategy_color(s), v)
            })
            .collect();

        // Top panel — the raw shape.
        let a = Panel {
            x0: 110,
            x1: 1150,
            y0: top + 34,
            y1: 430,
        };
        let peak = counts
            .iter()
            .flat_map(|(_, v)| v.iter().cloned())
            .fold(0.0, f64::max);
        let ys = Scale::new(0.0, headroom(peak), a.y1, a.y0);
        y_grid(c, &ys, a.x0, a.x1, 4, count);
        grouped_columns(c, &a, &ys, nbins, &counts, true, &count);
        axis_rule(c, &a);
        c.text(
            a.x0,
            top + 12,
            "occurrences per bucket",
            14,
            INK2,
            Align::Left,
        );
        if strats.len() > 1 {
            let entries: Vec<Key> = strats
                .iter()
                .map(|s| Key::fill(s.as_str(), strategy_color(s)))
                .collect();
            c.legend_right(a.x1, top + 12, &entries, 14);
        }

        // Bottom panel — the same x, read as a cut-off decision.
        let b = Panel {
            x0: 110,
            x1: 1150,
            y0: 500,
            y1: 660,
        };
        c.text(
            b.x0,
            478,
            "kept if you only take edges >= x",
            14,
            INK2,
            Align::Left,
        );
        let ys2 = Scale::new(0.0, 1.0, b.y1, b.y0);
        y_grid(c, &ys2, b.x0, b.x1, 4, |t| format!("{:.0}%", t * 100.0));

        for (i, s) in strats.iter().enumerate() {
            let total: f64 = counts[i].1.iter().sum();
            if total <= 0.0 {
                continue;
            }
            let mut pts = Vec::with_capacity(nbins);
            for bin in 0..nbins {
                let kept: f64 = counts[i].1[bin..].iter().sum();
                let (cx, _) = b.band(bin, nbins);
                pts.push((cx, ys2.map(kept / total)));
            }
            line_series(c, &pts, strategy_color(s), None);
        }
        axis_rule(c, &b);
        band_labels(c, &b, &names, 12);

        // Two reference cut-offs, so the curve is read against a decision and
        // not just admired. Labelled, because a bare rule is not a value.
        for (threshold, note) in [(0.01, "1%"), (0.02, "2%")] {
            let bin = (threshold / bucket).round() as usize;
            if bin == 0 || bin >= nbins {
                continue;
            }
            let (cx, _) = b.band(bin, nbins);
            c.hairline_v(cx, b.y0, b.y1, AXIS);
            let kept: Vec<String> = strats
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let total: f64 = counts[i].1.iter().sum();
                    (total > 0.0).then(|| {
                        let k: f64 = counts[i].1[bin..].iter().sum();
                        format!("{s} {:.0}%", k / total * 100.0)
                    })
                })
                .collect();
            c.text(cx + 6, b.y0 + 12, note, 13, INK, Align::Left);
            c.text(cx + 6, b.y0 + 30, &kept.join(" · "), 12, MUTED, Align::Left);
        }

        footer(
            c,
            "Realised edge is what clears at size after sweeping both ladders — never above the top-of-book edge.",
        );
        Ok(())
    })
}

// --- 2. Edge decay --------------------------------------------------------

/// How long an edge stays on screen. The survival curve is the competitiveness
/// test: if the mass sits left of a few hundred milliseconds, the trade is
/// already gone by the time any non-colocated executor could act.
pub fn decay(v: &Value, window: &str) -> Result<Vec<u8>> {
    let names = labels(v, "labels");
    let all = rows(v, "rows");
    let strats = strategies(&all);
    let edges = v
        .get("buckets_ms")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect::<Vec<_>>())
        .unwrap_or_default();

    render(W, H, |c| {
        let top = header(
            c,
            "Edge decay — how long an arb survives",
            &format!("Closed micro-crossings, by how long they stayed visible · {window}"),
        );
        if all.is_empty() || names.is_empty() {
            empty_note(c, top, "No closed crossings in this window.");
            return Ok(());
        }

        let nb = names.len();
        let counts: Vec<(RGBColor, Vec<f64>)> = strats
            .iter()
            .map(|s| {
                let r = all.iter().find(|r| text_of(r, "strategy") == *s);
                let v = (0..nb)
                    .map(|i| r.map_or(0.0, |r| num(r, &format!("b{i}"))))
                    .collect();
                (strategy_color(s), v)
            })
            .collect();

        let a = Panel {
            x0: 110,
            x1: 1150,
            y0: top + 34,
            y1: 420,
        };
        let peak = counts
            .iter()
            .flat_map(|(_, v)| v.iter().cloned())
            .fold(0.0, f64::max);
        let ys = Scale::new(0.0, headroom(peak), a.y1, a.y0);
        y_grid(c, &ys, a.x0, a.x1, 4, count);
        grouped_columns(c, &a, &ys, nb, &counts, true, &count);
        axis_rule(c, &a);
        c.text(
            a.x0,
            top + 12,
            "crossings by lifetime",
            14,
            INK2,
            Align::Left,
        );
        if strats.len() > 1 {
            let entries: Vec<Key> = strats
                .iter()
                .map(|s| Key::fill(s.as_str(), strategy_color(s)))
                .collect();
            c.legend_right(a.x1, top + 12, &entries, 14);
        }

        // Survival: share still open at each bucket edge.
        let b = Panel {
            x0: 110,
            x1: 1150,
            y0: 492,
            y1: 652,
        };
        c.text(b.x0, 470, "still open after x", 14, INK2, Align::Left);
        let ys2 = Scale::new(0.0, 1.0, b.y1, b.y0);
        y_grid(c, &ys2, b.x0, b.x1, 4, |t| format!("{:.0}%", t * 100.0));
        for (i, s) in strats.iter().enumerate() {
            let total: f64 = counts[i].1.iter().sum();
            if total <= 0.0 {
                continue;
            }
            let pts: Vec<(i32, i32)> = (0..nb)
                .map(|bin| {
                    let alive: f64 = counts[i].1[bin..].iter().sum();
                    let (cx, _) = b.band(bin, nb);
                    (cx, ys2.map(alive / total))
                })
                .collect();
            line_series(c, &pts, strategy_color(s), None);
        }
        axis_rule(c, &b);
        band_labels(c, &b, &names, 12);

        // The one number that decides whether you can compete at all.
        let median: Vec<String> = all
            .iter()
            .map(|r| {
                format!(
                    "{} median {}  ·  p90 {}",
                    text_of(r, "strategy"),
                    dur_ms(num(r, "p50")),
                    dur_ms(num(r, "p90"))
                )
            })
            .collect();
        c.text(b.x1, 470, &median.join("     "), 13, INK, Align::Right);

        // Mark the 250ms line — below it, latency decides, not the edge. The
        // rule belongs on the bucket *boundary*, not the band centre, or it
        // reads as marking the bucket it sits in.
        if let Some(bin) = edges.iter().position(|e| *e == 250) {
            let (cx, bw) = b.band(bin, nb);
            let x = cx + bw / 2;
            c.hairline_v(x, b.y0, b.y1, AXIS);
            c.text(x + 6, b.y0 + 12, "250ms", 12, MUTED, Align::Left);
        }

        footer(
            c,
            "Anything closing inside a few hundred ms is a latency race, not an edge you can size into.",
        );
        Ok(())
    })
}

// --- 3. Timeline ----------------------------------------------------------

/// Occurrences and average edge over the window, on one shared time axis.
pub fn timeline(v: &Value, window: &str, tz_min: i64) -> Result<Vec<u8>> {
    let all = rows(v, "rows");
    let strats = strategies(&all);
    let bucket = v
        .get("bucket_secs")
        .and_then(Value::as_u64)
        .unwrap_or(3_600);

    render(W, H, |c| {
        let top = header(
            c,
            "Opportunities over time",
            &format!(
                "Count and average realised edge per {} · {window}",
                dur_secs(bucket)
            ),
        );
        if all.is_empty() {
            empty_note(c, top, "No opportunities recorded in this window.");
            return Ok(());
        }

        let mut times: Vec<f64> = all.iter().map(|r| num(r, "t")).collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        times.dedup();
        let nt = times.len();
        let idx = |t: f64| times.iter().position(|x| *x == t);

        let by_strategy = |s: &str, field: &str| -> Vec<f64> {
            let mut out = vec![0.0; nt];
            for r in all.iter().filter(|r| text_of(r, "strategy") == s) {
                if let Some(i) = idx(num(r, "t")) {
                    out[i] = num(r, field);
                }
            }
            out
        };

        // Counts stack: the total is the story, the split is the detail.
        let a = Panel {
            x0: 110,
            x1: 1150,
            y0: top + 34,
            y1: 400,
        };
        let series: Vec<(String, Vec<f64>)> = strats
            .iter()
            .map(|s| (s.clone(), by_strategy(s, "n")))
            .collect();
        let totals: Vec<f64> = (0..nt)
            .map(|i| series.iter().map(|(_, v)| v[i]).sum())
            .collect();
        let ys = Scale::new(
            0.0,
            headroom(totals.iter().cloned().fold(0.0, f64::max)),
            a.y1,
            a.y0,
        );
        y_grid(c, &ys, a.x0, a.x1, 4, count);

        let (_, bw) = a.band(0, nt);
        let colw = (bw as f64 * 0.7) as i32;
        for i in 0..nt {
            let (cx, _) = a.band(i, nt);
            let mut base = a.y1;
            for (name, values) in &series {
                let v = values[i];
                if v <= 0.0 {
                    continue;
                }
                // Each segment is measured as its own height from the baseline
                // rather than as the difference of two cumulative positions, so
                // rounding cannot make the stack drift away from its total.
                let height = ys.map(0.0) - ys.map(v);
                let topy = base - height;
                c.column(cx, colw, topy, base, strategy_color(name));
                base = topy - GAP;
            }
        }
        axis_rule(c, &a);
        c.text(a.x0, top + 12, "occurrences", 14, INK2, Align::Left);
        if strats.len() > 1 {
            let entries: Vec<Key> = strats
                .iter()
                .map(|s| Key::fill(s.as_str(), strategy_color(s)))
                .collect();
            c.legend_right(a.x1, top + 12, &entries, 14);
        }

        // Average edge — same x, its own panel, never a second y-scale.
        let b = Panel {
            x0: 110,
            x1: 1150,
            y0: 470,
            y1: 650,
        };
        c.text(b.x0, 448, "average realised edge", 14, INK2, Align::Left);
        let emax = all.iter().map(|r| num(r, "edge_avg")).fold(0.0, f64::max);
        let ys2 = Scale::new(0.0, headroom(emax), b.y1, b.y0);
        y_grid(c, &ys2, b.x0, b.x1, 4, pct);

        let drawn: Vec<EndLabelled> = strats
            .iter()
            .map(|s| {
                let vals = by_strategy(s, "edge_avg");
                let pts: Vec<(i32, i32)> = (0..nt)
                    .filter(|i| vals[*i] > 0.0)
                    .map(|i| (b.band(i, nt).0, ys2.map(vals[i])))
                    .collect();
                let last = *vals.iter().rfind(|v| **v > 0.0).unwrap_or(&0.0);
                (s.clone(), pts, last)
            })
            .collect();

        // End-labels only survive if the lines separate at the right edge.
        // When they converge, nudging the labels apart would detach them from
        // their lines — the legend already carries identity, so drop them.
        let converged = drawn
            .iter()
            .filter_map(|(_, p, _)| p.last().map(|(_, y)| *y))
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| (w[0] - w[1]).abs() < 18);
        for (s, pts, last) in &drawn {
            let label = (!converged).then(|| pct(*last));
            line_series(c, pts, strategy_color(s), label.as_deref());
        }
        axis_rule(c, &b);

        let names: Vec<String> = times
            .iter()
            .map(|t| clock_label(*t as i64, tz_min, bucket))
            .collect();
        band_labels(c, &b, &names, 12);

        footer(c, &format!("Times shown at {}.", crate::tz_label(tz_min)));
        Ok(())
    })
}

// --- 4. Markets -----------------------------------------------------------

/// Which markets actually produce edges. Nominal categories, so one hue for
/// every bar — length already encodes the magnitude, and colouring by it would
/// spend the only free channel on information the chart shows twice.
pub fn markets(v: &Value, window: &str) -> Result<Vec<u8>> {
    let all = rows(v, "rows");

    render(W, H, |c| {
        let top = header(
            c,
            "Where the edges come from",
            &format!("Markets by number of separate opportunities · {window}"),
        );
        if all.is_empty() {
            empty_note(c, top, "No opportunities recorded in this window.");
            return Ok(());
        }

        let n = all.len().min(14);
        let label_w = 360;
        let p = Panel {
            x0: 40 + label_w,
            x1: 940,
            y0: top + 30,
            y1: H as i32 - 60,
        };
        let peak = all
            .iter()
            .map(|r| num(r, "occurrences"))
            .fold(0.0, f64::max);
        let xs = Scale::new(0.0, headroom(peak), p.x0, p.x1);

        c.text(40, top + 8, "market", 13, MUTED, Align::Left);
        c.text(p.x0, top + 8, "opportunities", 13, MUTED, Align::Left);
        c.text(1000, top + 8, "avg edge", 13, MUTED, Align::Left);
        c.text(1160, top + 8, "median life", 13, MUTED, Align::Right);

        let row_h = ((p.y1 - p.y0) / n.max(1) as i32).min(46);
        for (i, r) in all.iter().take(n).enumerate() {
            let cy = p.y0 + row_h * i as i32 + row_h / 2;
            let question = text_of(r, "question");
            let name = if question.is_empty() {
                text_of(r, "slug")
            } else {
                question
            };
            c.text(
                40,
                cy,
                &ellipsize(c, &name, 14, label_w - 20),
                14,
                INK2,
                Align::Left,
            );

            let occ = num(r, "occurrences");
            let end = xs.map(occ);
            c.bar(cy, row_h - 14, p.x0, end, MINT);
            // Value at the tip, outside the bar — never risk clipping it inside.
            c.text(end + 10, cy, &count(occ), 13, INK, Align::Left);

            c.text(1000, cy, &pct(num(r, "edge_avg")), 13, INK2, Align::Left);
            let life = num(r, "lived_p50");
            let life = if life > 0.0 {
                dur_ms(life)
            } else {
                "—".into()
            };
            c.text(1160, cy, &life, 13, INK2, Align::Right);
        }
        c.hairline_v(p.x0, p.y0, p.y0 + row_h * n as i32, AXIS);

        footer(
            c,
            "A market that fires often with a short median life is a crowded one — frequency alone is not opportunity.",
        );
        Ok(())
    })
}

// --- 5. Heatmap -----------------------------------------------------------

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// When edges appear, by weekday and hour. Magnitude on a grid, so one hue
/// light→dark with a scale legend — never a rainbow.
pub fn heatmap(v: &Value, window: &str) -> Result<Vec<u8>> {
    let all = rows(v, "rows");
    let tz = v
        .get("tz_offset_minutes")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    render(W, 640, |c| {
        let top = header(
            c,
            "When opportunities appear",
            &format!(
                "Occurrences by weekday and hour, {} · {window}",
                crate::tz_label(tz)
            ),
        );
        if all.is_empty() {
            empty_note(c, top, "No opportunities recorded in this window.");
            return Ok(());
        }

        let mut grid = [[0.0f64; 24]; 7];
        for r in &all {
            // ClickHouse toDayOfWeek is 1 = Monday.
            let d = (num(r, "dow") as usize).clamp(1, 7) - 1;
            let h = (num(r, "hour") as usize).min(23);
            grid[d][h] = num(r, "n");
        }
        let peak = grid.iter().flatten().cloned().fold(0.0, f64::max);

        let (x0, y0) = (86, top + 30);
        let cw = (1150 - x0) / 24;
        let ch = 52;

        for (h, _) in (0..24).enumerate() {
            if h % 2 == 0 {
                c.text(
                    x0 + cw * h as i32 + cw / 2,
                    y0 - 14,
                    &format!("{h:02}"),
                    12,
                    MUTED,
                    Align::Center,
                );
            }
        }
        for (d, day) in DAYS.iter().enumerate() {
            let cy = y0 + ch * d as i32 + ch / 2;
            c.text(x0 - 14, cy, day, 13, MUTED, Align::Right);
            // The hour is a coordinate on the axis, not just a way into `grid`,
            // so it stays an index rather than becoming an iterator.
            #[allow(clippy::needless_range_loop)]
            for h in 0..24 {
                let v = grid[d][h];
                let cx = x0 + cw * h as i32;
                let color = ramp(v, peak);
                c.cell(
                    cx,
                    y0 + ch * d as i32,
                    cx + cw,
                    y0 + ch * (d as i32 + 1),
                    color,
                );
                // A number only on the hot cells — the ramp and its legend
                // carry every other value, and a figure in all 168 cells is a
                // table pretending to be a chart. Ink is chosen against the
                // fill, which at this end of the ramp is always light.
                if v > 0.0 && peak > 0.0 && v >= peak * 0.85 {
                    let s = count(v);
                    if c.measure(&s, 12, false).0 < cw - 8 {
                        c.text(
                            cx + cw / 2,
                            y0 + ch * d as i32 + ch / 2,
                            &s,
                            12,
                            PLANE,
                            Align::Center,
                        );
                    }
                }
            }
        }

        // Scale legend — a continuous encoding always ships its key.
        let ly = y0 + ch * 7 + 40;
        c.text(x0, ly, "fewer", 12, MUTED, Align::Right);
        for (i, color) in SEQ.iter().enumerate() {
            let sx = x0 + 10 + i as i32 * 34;
            c.rect(sx, ly - 7, sx + 32 - GAP, ly + 7, *color);
        }
        c.text(
            x0 + 10 + SEQ.len() as i32 * 34 + 4,
            ly,
            &format!("more (peak {})", count(peak)),
            12,
            MUTED,
            Align::Left,
        );

        footer(
            c,
            "Empty cells are genuine — most hours produce nothing at all.",
        );
        Ok(())
    })
}

fn ramp(v: f64, peak: f64) -> RGBColor {
    if v <= 0.0 || peak <= 0.0 {
        return GRID;
    }
    // Square root, so the many small values stay distinguishable from zero.
    let t = (v / peak).sqrt().clamp(0.0, 1.0);
    SEQ[((t * (SEQ.len() - 1) as f64).round() as usize).min(SEQ.len() - 1)]
}

// --- 6. Size curve --------------------------------------------------------

/// Whether your own size eats the edge. The bar is what actually clears; the
/// marker above it is the top-of-book edge you would have quoted — the gap
/// between them is the ladder sweep, in the same units, on one axis.
pub fn size_curve(v: &Value, window: &str) -> Result<Vec<u8>> {
    let names = labels(v, "labels");
    let all = rows(v, "rows");
    let strats = strategies(&all);

    render(W, H, |c| {
        let top = header(
            c,
            "Does size eat the edge?",
            &format!("Realised vs top-of-book edge by position size, in sets · {window}"),
        );
        if all.is_empty() || names.is_empty() {
            empty_note(c, top, "No opportunities recorded in this window.");
            return Ok(());
        }

        let nb = names.len();
        let pick = |s: &str, field: &str| -> Vec<f64> {
            let mut out = vec![0.0; nb];
            for r in all.iter().filter(|r| text_of(r, "strategy") == s) {
                let b = num(r, "bucket") as usize;
                if b < nb {
                    out[b] = num(r, field);
                }
            }
            out
        };

        let a = Panel {
            x0: 110,
            x1: 1150,
            y0: top + 34,
            y1: 420,
        };
        let realised: Vec<(RGBColor, Vec<f64>)> = strats
            .iter()
            .map(|s| (strategy_color(s), pick(s, "edge_avg")))
            .collect();
        let gross: Vec<Vec<f64>> = strats.iter().map(|s| pick(s, "gross_avg")).collect();
        let peak = gross.iter().flatten().cloned().fold(0.0, f64::max);
        let ys = Scale::new(0.0, headroom(peak), a.y1, a.y0);
        y_grid(c, &ys, a.x0, a.x1, 4, pct);
        grouped_columns(c, &a, &ys, nb, &realised, false, &pct);

        // The top-of-book marker sits over its own column, with a connector so
        // the haircut is a length you can read rather than a subtraction. Only
        // the worst haircut per series gets a figure — the gap is the encoding.
        let worst: Vec<Option<usize>> = strats
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let gaps: Vec<f64> = (0..nb)
                    .map(|b| (gross[i][b] - realised[i].1[b]).max(0.0))
                    .collect();
                argmax(&gaps)
            })
            .collect();
        let k = strats.len().max(1) as i32;
        // `band` is the x position as well as the index into every series, so
        // iterating one of them would obscure what the loop is walking.
        #[allow(clippy::needless_range_loop)]
        for band in 0..nb {
            let (cx, bw) = a.band(band, nb);
            let inner = (bw as f64 * 0.66) as i32;
            let each = ((inner - GAP * (k - 1)) / k).clamp(3, BAR_MAX);
            let total = each * k + GAP * (k - 1);
            for (i, s) in strats.iter().enumerate() {
                let g = gross[i][band];
                let r = realised[i].1[band];
                if g <= 0.0 {
                    continue;
                }
                let x = cx - total / 2 + (each + GAP) * i as i32 + each / 2;
                c.hairline_v(x, ys.map(g), ys.map(r), AXIS);
                c.marker(x, ys.map(g), 4, strategy_color(s));
                if g > r && worst[i] == Some(band) {
                    let note = format!("-{} to depth", pct(g - r));
                    c.text(x, ys.map(g) - 18, &note, 12, INK2, Align::Center);
                }
            }
        }
        axis_rule(c, &a);
        c.text(a.x0, top + 12, "edge per set", 14, INK2, Align::Left);
        let mut entries: Vec<Key> = strats
            .iter()
            .map(|s| Key::fill(s.as_str(), strategy_color(s)))
            .collect();
        entries.push(Key::dot("top of book", MUTED));
        c.legend_right(a.x1, top + 12, &entries, 14);

        // Second panel: how often a size in that bucket actually cleared costs.
        let b = Panel {
            x0: 110,
            x1: 1150,
            y0: 492,
            y1: 652,
        };
        c.text(
            b.x0,
            470,
            "share that cleared fees + gas",
            14,
            INK2,
            Align::Left,
        );
        let ys2 = Scale::new(0.0, 1.0, b.y1, b.y0);
        y_grid(c, &ys2, b.x0, b.x1, 4, |t| format!("{:.0}%", t * 100.0));
        let rates: Vec<(RGBColor, Vec<f64>)> = strats
            .iter()
            .map(|s| (strategy_color(s), pick(s, "profitable_rate")))
            .collect();
        grouped_columns(c, &b, &ys2, nb, &rates, false, &pct);
        axis_rule(c, &b);
        band_labels(c, &b, &names, 12);

        footer(
            c,
            "The bar is what clears at that size; the drop from the marker is your own sweep down the ladder.",
        );
        Ok(())
    })
}

// --- 7. Dashboard ---------------------------------------------------------

/// One picture with the headline numbers and the four charts that matter, for
/// when you want the whole picture in a single message.
pub fn report(
    summary: &Value,
    hist: &Value,
    dec: &Value,
    tl: &Value,
    mk: &Value,
    window: &str,
) -> Result<Vec<u8>> {
    let by_strategy = rows(summary, "by_strategy");
    let lifetime = rows(summary, "lifetime");

    render(1400, 1000, |c| {
        let top = header(
            c,
            "Arbitrage analytics",
            &format!("Detection-side summary · {window}"),
        );
        // One key for the whole figure: two of the panels below are split by
        // strategy, and neither has room for a legend of its own.
        let strats = strategies(&by_strategy);
        if strats.len() > 1 {
            let keys: Vec<Key> = strats
                .iter()
                .map(|s| Key::fill(s.as_str(), strategy_color(s)))
                .collect();
            c.legend_right(1372, 34, &keys, 14);
        }

        // KPI row. These are numbers, not charts — a one-bar bar chart for
        // "how many" would be worse in every way.
        let occurrences: f64 = by_strategy.iter().map(|r| num(r, "occurrences")).sum();
        let profitable: f64 = by_strategy.iter().map(|r| num(r, "profitable")).sum();
        let net: f64 = by_strategy.iter().map(|r| num(r, "net_total")).sum();
        let edge_p50 = by_strategy
            .iter()
            .map(|r| num(r, "edge_p50"))
            .fold(0.0, f64::max);
        let lived_p50 = lifetime
            .iter()
            .map(|r| num(r, "lived_p50"))
            .fold(0.0, f64::max);
        let share = if occurrences > 0.0 {
            profitable / occurrences
        } else {
            0.0
        };

        // A status colour is only worn where it *means* good or bad, and never
        // alone — each sits beside its own label. In between the two ends the
        // number stays neutral rather than being nudged into a verdict.
        let verdict = |v: f64, good: f64, bad: f64| {
            if v >= good {
                GOOD
            } else if v <= bad {
                CRITICAL
            } else {
                INK
            }
        };
        let tiles: [(&str, String, RGBColor); 5] = [
            ("opportunities", count(occurrences), INK),
            ("median edge", pct(edge_p50), INK),
            ("median lifetime", dur_ms(lived_p50), INK),
            (
                "cleared costs",
                format!("{:.0}%", share * 100.0),
                verdict(share, 0.5, 0.25),
            ),
            ("theoretical net", usd(net), verdict(net, 0.0, 0.0)),
        ];
        let tw = (1400 - 56 - 4 * 16) / 5;
        for (i, (label, value, color)) in tiles.iter().enumerate() {
            let x = 28 + (tw + 16) * i as i32;
            c.rect(x, top, x + tw, top + 92, PLANE);
            c.text(x + 18, top + 30, label, 13, MUTED, Align::Left);
            c.text_bold(x + 18, top + 62, value, 30, *color, Align::Left);
        }
        let grid_top = top + 122;

        // Four mini panels on the same grid. Each is a reduced version of the
        // standalone chart, so a reader who has seen one recognises all of them.
        let pw = (1400 - 56 - 24) / 2;
        let ph = (1000 - grid_top - 46 - 24) / 2;
        let cells = [
            (28, grid_top),
            (28 + pw + 24, grid_top),
            (28, grid_top + ph + 24),
            (28 + pw + 24, grid_top + ph + 24),
        ];

        mini_edge(c, hist, cells[0], pw, ph);
        mini_survival(c, dec, cells[1], pw, ph);
        mini_timeline(c, tl, cells[2], pw, ph);
        mini_markets(c, mk, cells[3], pw, ph);

        footer(
            c,
            "Detection only — no order was placed. \"Theoretical net\" is what the book offered, before execution risk.",
        );
        Ok(())
    })
}

fn mini_frame(c: &Canvas, at: (i32, i32), w: i32, h: i32, title: &str) -> Panel {
    c.rect(at.0, at.1, at.0 + w, at.1 + h, PLANE);
    c.text(at.0 + 16, at.1 + 20, title, 14, INK2, Align::Left);
    Panel {
        x0: at.0 + 56,
        x1: at.0 + w - 20,
        y0: at.1 + 40,
        y1: at.1 + h - 34,
    }
}

fn mini_edge(c: &Canvas, v: &Value, at: (i32, i32), w: i32, h: i32) {
    let p = mini_frame(c, at, w, h, "edge distribution");
    let all = rows(v, "rows");
    let bucket = v.get("bucket").and_then(Value::as_f64).unwrap_or(0.0025);
    let nbins = all
        .iter()
        .map(|r| num(r, "bin") as usize + 1)
        .max()
        .unwrap_or(0)
        .clamp(1, 40);
    if all.is_empty() {
        c.text(
            (p.x0 + p.x1) / 2,
            (p.y0 + p.y1) / 2,
            "no data",
            14,
            MUTED,
            Align::Center,
        );
        return;
    }
    let strats = strategies(&all);
    let series: Vec<(RGBColor, Vec<f64>)> = strats
        .iter()
        .map(|s| {
            let mut vals = vec![0.0; nbins];
            for r in all.iter().filter(|r| text_of(r, "strategy") == *s) {
                let b = num(r, "bin") as usize;
                if b < nbins {
                    vals[b] = num(r, "n");
                }
            }
            (strategy_color(s), vals)
        })
        .collect();
    let peak = series
        .iter()
        .flat_map(|(_, v)| v.iter().cloned())
        .fold(0.0, f64::max);
    let ys = Scale::new(0.0, headroom(peak), p.y1, p.y0);
    y_grid(c, &ys, p.x0, p.x1, 3, count);
    grouped_columns(c, &p, &ys, nbins, &series, false, &count);
    axis_rule(c, &p);
    let names: Vec<String> = (0..nbins).map(|i| pct(i as f64 * bucket)).collect();
    band_labels(c, &p, &names, 11);
}

fn mini_survival(c: &Canvas, v: &Value, at: (i32, i32), w: i32, h: i32) {
    let p = mini_frame(c, at, w, h, "still open after x");
    let names = labels(v, "labels");
    let all = rows(v, "rows");
    if all.is_empty() || names.is_empty() {
        c.text(
            (p.x0 + p.x1) / 2,
            (p.y0 + p.y1) / 2,
            "no data",
            14,
            MUTED,
            Align::Center,
        );
        return;
    }
    let ys = Scale::new(0.0, 1.0, p.y1, p.y0);
    y_grid(c, &ys, p.x0, p.x1, 3, |t| format!("{:.0}%", t * 100.0));
    for s in strategies(&all) {
        let Some(r) = all.iter().find(|r| text_of(r, "strategy") == s) else {
            continue;
        };
        let vals: Vec<f64> = (0..names.len()).map(|i| num(r, &format!("b{i}"))).collect();
        let total: f64 = vals.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let pts: Vec<(i32, i32)> = (0..names.len())
            .map(|i| {
                let alive: f64 = vals[i..].iter().sum();
                (p.band(i, names.len()).0, ys.map(alive / total))
            })
            .collect();
        line_series(c, &pts, strategy_color(&s), None);
    }
    axis_rule(c, &p);
    band_labels(c, &p, &names, 11);
}

fn mini_timeline(c: &Canvas, v: &Value, at: (i32, i32), w: i32, h: i32) {
    let p = mini_frame(c, at, w, h, "occurrences over time");
    let all = rows(v, "rows");
    if all.is_empty() {
        c.text(
            (p.x0 + p.x1) / 2,
            (p.y0 + p.y1) / 2,
            "no data",
            14,
            MUTED,
            Align::Center,
        );
        return;
    }
    let mut times: Vec<f64> = all.iter().map(|r| num(r, "t")).collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times.dedup();
    let totals: Vec<f64> = times
        .iter()
        .map(|t| {
            all.iter()
                .filter(|r| num(r, "t") == *t)
                .map(|r| num(r, "n"))
                .sum()
        })
        .collect();
    let ys = Scale::new(
        0.0,
        headroom(totals.iter().cloned().fold(0.0, f64::max)),
        p.y1,
        p.y0,
    );
    y_grid(c, &ys, p.x0, p.x1, 3, count);
    let (_, bw) = p.band(0, times.len());
    for (i, v) in totals.iter().enumerate() {
        let (cx, _) = p.band(i, times.len());
        c.column(cx, (bw as f64 * 0.7) as i32, ys.map(*v), p.y1, MINT);
    }
    axis_rule(c, &p);
}

fn mini_markets(c: &Canvas, v: &Value, at: (i32, i32), w: i32, h: i32) {
    let p = mini_frame(c, at, w, h, "top markets");
    let all = rows(v, "rows");
    if all.is_empty() {
        c.text(
            (p.x0 + p.x1) / 2,
            (p.y0 + p.y1) / 2,
            "no data",
            14,
            MUTED,
            Align::Center,
        );
        return;
    }
    let n = all.len().min(6);
    let label_w = 150;
    let x0 = at.0 + 16 + label_w;
    let peak = all
        .iter()
        .map(|r| num(r, "occurrences"))
        .fold(0.0, f64::max);
    let xs = Scale::new(0.0, headroom(peak), x0, at.0 + w - 60);
    let row_h = ((p.y1 - p.y0) / n.max(1) as i32).min(40);
    for (i, r) in all.iter().take(n).enumerate() {
        let cy = p.y0 + row_h * i as i32 + row_h / 2;
        let name = {
            let q = text_of(r, "question");
            if q.is_empty() {
                text_of(r, "slug")
            } else {
                q
            }
        };
        c.text(
            at.0 + 16,
            cy,
            &ellipsize(c, &name, 12, label_w - 10),
            12,
            INK2,
            Align::Left,
        );
        let occ = num(r, "occurrences");
        let end = xs.map(occ);
        c.bar(cy, row_h - 12, x0, end, MINT);
        c.text(end + 8, cy, &count(occ), 12, INK, Align::Left);
    }
}

// --- Small helpers --------------------------------------------------------

fn dur_secs(s: u64) -> String {
    match s {
        s if s < 3_600 => format!("{}min", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Axis label for a time bucket: `14:00` inside a day, `25 Jul` beyond one.
fn clock_label(ts: i64, tz_min: i64, bucket: u64) -> String {
    let (_, m, d, hh, mm, _) = arb_core::time::wall_clock(ts.max(0) as u64, tz_min);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    if bucket >= 86_400 {
        format!("{d} {}", MONTHS[(m as usize - 1).min(11)])
    } else {
        format!("{hh:02}:{mm:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_survive_clickhouses_stringified_integers() {
        let v = json!({ "n": "1599", "edge": 0.0125, "missing": null });
        assert_eq!(num(&v, "n"), 1599.0);
        assert_eq!(num(&v, "edge"), 0.0125);
        assert_eq!(num(&v, "missing"), 0.0);
        assert_eq!(num(&v, "absent"), 0.0);
    }

    #[test]
    fn bands_divide_the_panel_evenly() {
        let p = Panel {
            x0: 100,
            x1: 500,
            y0: 0,
            y1: 100,
        };
        assert_eq!(p.band(0, 4), (150, 100));
        assert_eq!(p.band(3, 4), (450, 100));
    }

    #[test]
    fn every_chart_renders_an_empty_window_without_failing() {
        // An empty result is the normal case for this project, so it has to
        // produce a readable picture rather than an error or a bare frame.
        let empty = json!({ "rows": [], "labels": [], "buckets_ms": [] });
        for png in [
            edge_histogram(&empty, "24h").unwrap(),
            decay(&empty, "24h").unwrap(),
            timeline(&empty, "24h", 0).unwrap(),
            markets(&empty, "24h").unwrap(),
            heatmap(&empty, "24h").unwrap(),
            size_curve(&empty, "24h").unwrap(),
            report(&empty, &empty, &empty, &empty, &empty, "24h").unwrap(),
        ] {
            assert!(png.len() > 1_000, "suspiciously small PNG");
            assert_eq!(&png[1..4], b"PNG");
        }
    }

    /// Payloads shaped like the detector's, with both strategies present, so
    /// the plotting math (stacking, banding, survival curves, ladders) is
    /// actually walked and not just the empty-state early return.
    fn populated() -> (Value, Value, Value, Value, Value, Value, Value) {
        let hist = json!({"bucket": 0.0025, "rows": (0..12).flat_map(|b| {
            ["mint", "split"].map(|s| json!({
                "strategy": s, "bin": b, "n": (60 - b * 4).to_string(),
                "net": 12.5, "size_avg": 120.0, "profitable": "3",
            }))
        }).collect::<Vec<_>>()});
        let decay_rows: Vec<Value> = ["mint", "split"]
            .iter()
            .map(|s| {
                let mut row = serde_json::Map::new();
                row.insert("strategy".into(), json!(s));
                row.insert("n".into(), json!("300"));
                for i in 0..10 {
                    row.insert(format!("b{i}"), json!((40 - i * 3).to_string()));
                }
                for (k, v) in [("p50", 340.0), ("p90", 4200.0), ("p99", 26000.0)] {
                    row.insert(k.into(), json!(v));
                }
                row.insert("edge_avg".into(), json!(0.014));
                Value::Object(row)
            })
            .collect();
        let decay_v = json!({
            "buckets_ms": [50, 100, 250, 500, 1000, 2000, 5000, 10000, 30000],
            "labels": decay_labels(),
            "rows": decay_rows,
        });
        let tl = json!({"bucket_secs": 1800, "rows": (0..24).flat_map(|i| {
            ["mint", "split"].map(|s| json!({
                "t": 1_784_996_400i64 + i * 1800, "strategy": s,
                "n": (3 + i % 5).to_string(), "markets": "2",
                "edge_avg": 0.008 + (i % 7) as f64 * 0.002, "edge_max": 0.03, "net": 4.0,
            }))
        }).collect::<Vec<_>>()});
        let mk = json!({"rows": (0..14).map(|i| json!({
            "condition_id": format!("0xc{i}"), "slug": format!("market-{i}"),
            "question": "Will this rather long market question resolve YES before the deadline?",
            "category": "crypto", "liquidity_tier": "liquid", "market_volume": 250000.0,
            "occurrences": (40 - i * 2).to_string(), "profitable": "5",
            "edge_avg": 0.012, "edge_max": 0.04, "net_total": 90.0,
            "size_avg": 210.0, "lived_avg": 900.0, "lived_p50": 340.0,
        })).collect::<Vec<_>>()});
        let hm = json!({"tz_offset_minutes": 180, "rows": (1..=7).flat_map(|d| {
            (0..24).map(move |h| json!({
                "dow": d, "hour": h, "n": ((d * h) % 19).to_string(),
                "edge_avg": 0.01, "net": 3.0,
            }))
        }).collect::<Vec<_>>()});
        let size = json!({
            "buckets": [10.0, 50.0, 100.0, 500.0, 1000.0],
            "labels": ["0–10", "10–50", "50–100", "100–500", "500–1k", "1k+"],
            "rows": (0..6).flat_map(|b| ["mint", "split"].map(move |s| json!({
                "strategy": s, "bucket": b, "n": "50", "size_avg": 100.0,
                "gross_avg": 0.019, "edge_avg": 0.019 - 0.002 * b as f64,
                "depth_haircut": 0.002 * b as f64, "depth_cost_usd": 1.4 * b as f64,
                "net_avg": 2.0, "profitable_rate": 0.45,
            }))).collect::<Vec<_>>(),
        });
        let per_strategy: Vec<Value> = ["mint", "split"]
            .iter()
            .map(|s| {
                json!({
                    "strategy": s, "occurrences": "1599", "markets": "40", "profitable": "758",
                    "edge_avg": 0.0113, "edge_p50": 0.0062, "edge_p90": 0.029, "edge_max": 0.052,
                    "gross_avg": 0.0172, "depth_haircut": 0.0059, "net_total": -3678.0,
                    "net_avg": -2.3, "size_avg": 467.0, "capital_avg": 439.0,
                    "fee_avg": 7.6, "gas_avg": 0.02, "latency_us_avg": 1417.0,
                    "latency_us_p90": 2439.0, "secs_to_resolution_avg": 5268185.0,
                })
            })
            .collect();
        let per_lifetime: Vec<Value> = ["mint", "split"]
            .iter()
            .map(|s| {
                json!({
                    "strategy": s, "closed": "1066", "lived_avg": 8000.0, "lived_p50": 2373.0,
                    "lived_p90": 25571.0, "lived_max": 39000.0, "under_250ms": "233",
                    "under_1s": "448", "time_to_peak_ms": 700.0, "peak_edge_avg": 0.014,
                    "reopen_gap_avg": 400000.0,
                })
            })
            .collect();
        let summary = json!({
            "by_strategy": per_strategy,
            "lifetime": per_lifetime,
            "coverage": {"book_samples": "8000", "markets_sampled": "40",
                         "trades": "5000", "traded_usd": 622695.0, "executions": "0"},
        });
        (summary, hist, decay_v, tl, mk, hm, size)
    }

    fn decay_labels() -> Vec<String> {
        [
            "0ms–50ms",
            "50ms–100ms",
            "100ms–250ms",
            "250ms–500ms",
            "500ms–1s",
            "1s–2s",
            "2s–5s",
            "5s–10s",
            "10s–30s",
            "30s+",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn every_chart_renders_a_populated_window() {
        let (summary, hist, dec, tl, mk, hm, size) = populated();
        for (name, png) in [
            ("edge", edge_histogram(&hist, "7d").unwrap()),
            ("decay", decay(&dec, "7d").unwrap()),
            ("timeline", timeline(&tl, "24h", 180).unwrap()),
            ("markets", markets(&mk, "7d").unwrap()),
            ("heatmap", heatmap(&hm, "30d").unwrap()),
            ("size", size_curve(&size, "30d").unwrap()),
            (
                "report",
                report(&summary, &hist, &dec, &tl, &mk, "7d").unwrap(),
            ),
        ] {
            assert_eq!(&png[1..4], b"PNG", "{name} is not a PNG");
            // A drawn chart compresses to far more than a blank canvas does;
            // this catches a plot that silently rendered nothing.
            assert!(
                png.len() > 15_000,
                "{name} looks empty ({} bytes)",
                png.len()
            );
        }
    }

    #[test]
    fn strategies_are_listed_once_in_a_stable_order() {
        let rows = [
            json!({"strategy": "split"}),
            json!({"strategy": "mint"}),
            json!({"strategy": "split"}),
        ];
        let refs: Vec<&Value> = rows.iter().collect();
        assert_eq!(strategies(&refs), vec!["mint", "split"]);
    }
}
