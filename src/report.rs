//! A plan as text for a person or JSON for a script.

use std::fmt::Write as _;

use crate::lock::Kind;
use crate::plan::{Layer, LayerKind, Plan};

/// What a layer holds, in a few words: the package for a package's own layer, a count otherwise.
pub fn describe(layer: &Layer) -> String {
    let count = layer.packages.len();
    match layer.kind {
        LayerKind::Conda => {
            let package = &layer.packages[0];
            format!("{} {}", package.name, package.version)
        }
        LayerKind::CondaGroup => plural(count, "conda package"),
        LayerKind::CondaMerged => format!("{count} more conda packages"),
        LayerKind::Pypi => plural(count, "PyPI package"),
        LayerKind::Environment => format!("the environment, {}", plural(count, "package")),
    }
}

/// The plan as a table, one row per layer; with `packages`, every layer of several packages lists them below it.
pub fn text(plan: &Plan, packages: bool) -> String {
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
    let shared: Vec<&Layer> = plan
        .layers
        .iter()
        .filter(|layer| matches!(layer.kind, LayerKind::CondaGroup | LayerKind::CondaMerged))
        .collect();
    if !shared.is_empty() {
        let alone = plan
            .layers
            .iter()
            .filter(|layer| layer.kind == LayerKind::Conda)
            .count();
        let _ = writeln!(
            out,
            "{} in layers of their own, {} sharing {}",
            plural(alone, "conda package"),
            shared.iter().map(|layer| layer.packages.len()).sum::<usize>(),
            plural(shared.len(), "layer"),
        );
    }
    let _ = writeln!(out);
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for (i, layer) in plan.layers.iter().enumerate() {
        rows.push(((i + 1).to_string(), describe(layer), size_text(layer.size)));
        if packages && layer.kind != LayerKind::Conda {
            for package in &layer.packages {
                let what = format!("  {} {}", package.name, package.version);
                rows.push((String::new(), what, size_text(package.size)));
            }
        }
    }
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
    use crate::plan::{Layering, plan};

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
        let text = text(&plan(locked, Layering::PerPackage, 3).unwrap(), false);
        assert_eq!(
            text,
            "Environment prod on linux-64 (image platform linux/amd64)\n\
             3 conda packages, 1 PyPI package, 3 layers, download size unknown\n\
             1 conda package in layers of their own, 2 sharing 1 layer\n\
             \n  \
               #  Layer                  Download\n  \
               1  a 1.2.3                2.0 KiB\n  \
               2  2 more conda packages  2 B\n  \
               3  1 PyPI package         unknown\n"
        );
    }

    #[test]
    fn packages_lists_what_each_shared_layer_holds() {
        let locked = Locked {
            environment: "prod".to_string(),
            platform: "linux-64".to_string(),
            packages: vec![
                package("big", Kind::Conda, Some(5 << 20)),
                package("tiny", Kind::Conda, Some(10)),
                package("zlib", Kind::Conda, Some(20)),
                package("requests", Kind::Pypi, None),
            ],
        };
        let plan = plan(locked, Layering::Grouped, 100).unwrap();
        let expected = [
            "Environment prod on linux-64 (image platform linux/amd64)",
            "3 conda packages, 1 PyPI package, 3 layers, download size unknown",
            "1 conda package in layers of their own, 2 sharing 1 layer",
            "",
            "  #  Layer             Download",
            "  1  big 1.2.3         5.0 MiB",
            "  2  2 conda packages  30 B",
            "       tiny 1.2.3      10 B",
            "       zlib 1.2.3      20 B",
            "  3  1 PyPI package    unknown",
            "       requests 1.2.3  unknown",
        ];
        assert_eq!(text(&plan, true), expected.join("\n") + "\n");
        // Without the flag, the same plan has a row per layer.
        assert_eq!(text(&plan, false).lines().count(), 8);
    }

    #[test]
    fn the_whole_environment_is_one_row() {
        let locked = Locked {
            environment: "prod".to_string(),
            platform: "linux-64".to_string(),
            packages: vec![package("a", Kind::Conda, Some(1)), package("b", Kind::Conda, Some(1))],
        };
        let plan = plan(locked, Layering::Single, 100).unwrap();
        let text = text(&plan, false);
        assert!(text.contains("  1  the environment, 2 packages  2 B\n"), "{text}");
        assert!(!text.contains("layers of their own"));
    }

    #[test]
    fn an_empty_plan_has_no_table() {
        let locked = Locked {
            environment: "default".to_string(),
            platform: "linux-aarch64".to_string(),
            packages: Vec::new(),
        };
        let plan = plan(locked, Layering::Grouped, 100).unwrap();
        assert_eq!(
            text(&plan, false),
            "Environment default on linux-aarch64 (image platform linux/arm64/v8)\n\
             0 conda packages, 0 PyPI packages, 0 layers, 0 B to download\n"
        );
        let json: serde_json::Value = serde_json::from_str(&json(&plan)).unwrap();
        assert_eq!(json["oci_platform"]["variant"], "v8");
        assert_eq!(json["layers"], serde_json::json!([]));
    }
}
