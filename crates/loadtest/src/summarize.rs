// `payment-loadtest summarize <report.json>...`: reports grouped by --label (first-seen order),
// one row per (label, op) with the median and the min–max spread over the repeats, as a
// Markdown table. Interleave A/B repeats on a noisy box and compare medians, not single runs.

use std::collections::BTreeMap;

use serde_json::Value;

fn median(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let mid = if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    };
    (mid, v[0], v[n - 1])
}

fn spread(v: Vec<f64>, digits: usize) -> String {
    if v.is_empty() {
        return "–".into();
    }
    let (m, lo, hi) = median(v);
    format!("{m:.digits$} ({lo:.digits$}–{hi:.digits$})")
}

#[derive(Default)]
struct Group {
    runs: usize,
    passed: usize,
    ops: BTreeMap<String, BTreeMap<&'static str, Vec<f64>>>,
}

pub fn run(paths: &[String]) -> Result<String, String> {
    if paths.is_empty() {
        return Err("summarize needs report files".into());
    }
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for path in paths {
        let raw = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let r: Value = serde_json::from_slice(&raw).map_err(|e| format!("{path}: {e}"))?;
        let label = r["label"].as_str().map(str::to_string).unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        if !groups.contains_key(&label) {
            order.push(label.clone());
        }
        let g = groups.entry(label).or_default();
        g.runs += 1;
        g.passed += usize::from(r["verification"]["passed"].as_bool() == Some(true));
        let cpu = &r["cpu"]["per_op_ms"];
        let ops = r["summary"]["ops"]
            .as_object()
            .ok_or_else(|| format!("{path}: not a payment-loadtest report"))?;
        let rows = ops
            .iter()
            .map(|(k, v)| (k.clone(), v))
            .chain(std::iter::once((
                "total".to_string(),
                &r["summary"]["total"],
            )));
        for (op, o) in rows {
            let m = g.ops.entry(op.clone()).or_default();
            let mut put = |k: &'static str, v: &Value| {
                if let Some(x) = v.as_f64() {
                    m.entry(k).or_default().push(x);
                }
            };
            put("ok/s", &o["ok_per_sec"]);
            put("p50", &o["latency_ms"]["p50"]);
            put("p99", &o["latency_ms"]["p99"]);
            put("p99.9", &o["latency_ms"]["p999"]);
            if op == "total" {
                put("pg cpu", &cpu["postgres"]);
                put("server cpu", &cpu["server"]);
            }
        }
    }
    let mut out = String::from(
        "| label | op | runs | ok/s | p50 ms | p99 ms | p99.9 ms | PG CPU ms/op | server CPU ms/op | verified |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for label in &order {
        let g = &groups[label];
        for (op, m) in &g.ops {
            let col = |k: &str, d: usize| spread(m.get(k).cloned().unwrap_or_default(), d);
            out.push_str(&format!(
                "| {label} | {op} | {} | {} | {} | {} | {} | {} | {} | {}/{} |\n",
                g.runs,
                col("ok/s", 0),
                col("p50", 1),
                col("p99", 1),
                col("p99.9", 1),
                col("pg cpu", 3),
                col("server cpu", 3),
                g.passed,
                g.runs
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians_and_spread() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), (2.0, 1.0, 3.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), (2.5, 1.0, 4.0));
        assert_eq!(spread(vec![], 1), "–");
        assert_eq!(spread(vec![1.0, 9.0, 5.0], 1), "5.0 (1.0–9.0)");
    }

    #[test]
    fn groups_reports_by_label() {
        let dir = std::env::temp_dir().join(format!("lt-summary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for (i, (label, rate)) in [("A", 100.0), ("B", 200.0), ("A", 300.0)]
            .iter()
            .enumerate()
        {
            let report = serde_json::json!({
                "label": label,
                "verification": {"passed": true},
                "cpu": {"per_op_ms": {"postgres": 1.5}},
                "summary": {
                    "ops": {"transfer": {"ok_per_sec": rate, "latency_ms": {"p50": 1.0, "p99": 2.0, "p999": 3.0}}},
                    "total": {"ok_per_sec": rate, "latency_ms": {"p50": 1.0, "p99": 2.0, "p999": 3.0}}
                }
            });
            let p = dir.join(format!("{i}.json"));
            std::fs::write(&p, report.to_string()).unwrap();
            paths.push(p.to_string_lossy().into_owned());
        }
        let table = run(&paths).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let a = table
            .lines()
            .position(|l| l.starts_with("| A | transfer"))
            .unwrap();
        let b = table
            .lines()
            .position(|l| l.starts_with("| B | transfer"))
            .unwrap();
        assert!(a < b, "first-seen order:\n{table}");
        assert!(
            table.contains("| A | transfer | 2 | 200 (100–300) |"),
            "{table}"
        );
        assert!(table.contains("| A | total | 2 | 200 (100–300) | 1.0 (1.0–1.0) | 2.0 (2.0–2.0) | 3.0 (3.0–3.0) | 1.500 (1.500–1.500) | – | 2/2 |"), "{table}");
    }
}
