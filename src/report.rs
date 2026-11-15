//! A plan as text for a person or JSON for a script.

use std::fmt::Write as _;

use crate::lock::Kind;
use crate::plan::{LayerKind, Plan};

/// The plan as a table, one row per layer.
pub fn text(plan: &Plan) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Environment {} on {} (image platform {})",
        plan.environment, plan.platform, plan.oci_platform
    );
    let _ = writeln!(
        out,
        "{}, {}, {}, {}",
        plural(plan.count(Kind::Conda), "conda package"),
        plural(plan.count(Kind::Pypi), "PyPI package"),
        plural(plan.layers.len(), "layer"),
        match plan.size() {
            Some(size) => format!("{} to download", size_text(Some(size))),
            None => "download size unknown".to_string(),
        },
    );
    if plan.layers.is_empty() {
        return out;
    }
    let _ = writeln!(out);
    let rows: Vec<(String, String, String)> = plan
        .layers
        .iter()
        .enumerate()
        .map(|(i, layer)| {
            let what = match layer.kind {
                LayerKind::Conda => {
                    let package = &layer.packages[0];
                    format!("{} {}", package.name, package.version)
                }
                LayerKind::CondaMerged => format!("{} more conda packages", layer.packages.len()),
                LayerKind::Pypi => plural(layer.packages.len(), "PyPI package"),
            };
            ((i + 1).to_string(), what, size_text(layer.size))
        })
        .collect();
    let number_width = rows.iter().map(|row| row.0.len()).max().unwrap_or(1);
    let what_width = rows.iter().map(|row| row.1.len()).max().unwrap_or(5).max("Layer".len());
    let _ = writeln!(out, "  {:>number_width$}  {:<what_width$}  Download", "#", "Layer");
    for (number, what, size) in rows {
        let _ = writeln!(out, "  {number:>number_width$}  {what:<what_width$}  {size}");
    }
    out
}

/// The plan as pretty-printed JSON.
pub fn json(plan: &Plan) -> String {
    serde_json::to_string_pretty(plan).expect("a plan always serializes")
}

/// `1 package`, `2 packages`.
pub(crate) fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Bytes in binary units, to one decimal place above a KiB. Sizes are of the compressed archives the lockfile
/// records, which is what gets downloaded, not what a layer holds once extracted.
pub fn size_text(size: Option<u64>) -> String {
    let Some(bytes) = size else {
        return "unknown".to_string();
    };
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{Locked, Package};
    use crate::plan::plan;

    fn package(name: &str, kind: Kind, size: Option<u64>) -> Package {
        Package {
            name: name.to_string(),
            version: "1.2.3".to_string(),
            kind,
            build: None,
            depends: Vec::new(),
            location: String::new(),
            sha256: None,
            size,
            record: None,
        }
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(size_text(None), "unknown");
        assert_eq!(size_text(Some(0)), "0 B");
        assert_eq!(size_text(Some(1023)), "1023 B");
        assert_eq!(size_text(Some(1024)), "1.0 KiB");
        assert_eq!(size_text(Some(1536)), "1.5 KiB");
        assert_eq!(size_text(Some(5 * 1024 * 1024)), "5.0 MiB");
        assert_eq!(size_text(Some(3 * 1024 * 1024 * 1024)), "3.0 GiB");
        assert_eq!(size_text(Some(u64::MAX)), "16777216.0 TiB");
    }

    #[test]
    fn the_table_names_each_layer() {
        let locked = Locked {
            environment: "prod".to_string(),
            platform: "linux-64".to_string(),
            packages: vec![
                package("a", Kind::Conda, Some(2048)),
                package("b", Kind::Conda, Some(1)),
                package("c", Kind::Conda, Some(1)),
                package("requests", Kind::Pypi, None),
            ],
        };
        let text = text(&plan(locked, 3).unwrap());
        assert_eq!(
            text,
            "Environment prod on linux-64 (image platform linux/amd64)\n\
             3 conda packages, 1 PyPI package, 3 layers, download size unknown\n\
             \n  \
               #  Layer                  Download\n  \
               1  a 1.2.3                2.0 KiB\n  \
               2  2 more conda packages  2 B\n  \
               3  1 PyPI package         unknown\n"
        );
    }

    #[test]
    fn an_empty_plan_has_no_table() {
        let locked = Locked {
            environment: "default".to_string(),
            platform: "linux-aarch64".to_string(),
            packages: Vec::new(),
        };
        let plan = plan(locked, 100).unwrap();
        assert_eq!(
            text(&plan),
            "Environment default on linux-aarch64 (image platform linux/arm64/v8)\n\
             0 conda packages, 0 PyPI packages, 0 layers, 0 B to download\n"
        );
        let json: serde_json::Value = serde_json::from_str(&json(&plan)).unwrap();
        assert_eq!(json["oci_platform"]["variant"], "v8");
        assert_eq!(json["layers"], serde_json::json!([]));
    }
}
