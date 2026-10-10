//! Developer-only authoring tool; Chafa and ImageMagick are never needed at runtime.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(program: &str, args: &[String], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Run {program}; use nix develop for authoring tools"))?;
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input)?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn main() -> Result<()> {
    let root = Path::new("content/portraits");
    let source: Value = serde_json::from_slice(&std::fs::read(root.join("sources.json"))?)?;
    let mut arguments = std::env::args().skip(1);
    let only = match arguments.next().as_deref() {
        None => None,
        Some("--only") => Some(arguments.next().context("--only needs a portrait key")?),
        _ => bail!("Usage: cargo run --example bake_portraits -- [--only captain]"),
    };
    ensure!(arguments.next().is_none(), "Unexpected authoring argument");
    let mut portraits = if only.is_some() {
        serde_json::from_slice::<Value>(&std::fs::read(root.join("baked.json"))?)?["portraits"]
            .clone()
    } else {
        json!({})
    };
    let mut selected = 0;
    for portrait in source["portraits"]
        .as_array()
        .context("Manifest needs portraits")?
    {
        let key = portrait["key"].as_str().context("Portrait needs key")?;
        if only.as_ref().is_some_and(|only| only != key) {
            continue;
        }
        selected += 1;
        ensure!(
            key.len() <= 40
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "Invalid portrait key"
        );
        let file = root.join(portrait["file"].as_str().context("Portrait needs file")?);
        let dimensions = run(
            "magick",
            &[
                "identify".into(),
                "-format".into(),
                "%w %h".into(),
                file.display().to_string(),
            ],
            None,
        )?;
        let dimensions = String::from_utf8(dimensions)?;
        let dimensions: Vec<f64> = dimensions
            .split_whitespace()
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        ensure!(dimensions.len() == 2, "Invalid PNG dimensions");
        let mut preprocess = vec![file.display().to_string()];
        if let Some(crop) = portrait["crop"].as_array() {
            ensure!(crop.len() == 4, "Crop needs [x,y,width,height]");
            let crop: Vec<f64> = crop
                .iter()
                .map(|n| n.as_f64().context("Invalid crop value"))
                .collect::<Result<_>>()?;
            ensure!(
                crop.iter().all(|n| n.is_finite())
                    && crop[0] >= 0.0
                    && crop[1] >= 0.0
                    && crop[2] > 0.0
                    && crop[3] > 0.0
                    && crop[0] + crop[2] <= 1.0
                    && crop[1] + crop[3] <= 1.0,
                "Crop exceeds PNG bounds"
            );
            let left = (crop[0] * dimensions[0]).round() as u32;
            let top = (crop[1] * dimensions[1]).round() as u32;
            let right = ((crop[0] + crop[2]) * dimensions[0]).round() as u32;
            let bottom = ((crop[1] + crop[3]) * dimensions[1]).round() as u32;
            ensure!(right > left && bottom > top, "Empty crop");
            preprocess.extend([
                "-crop".into(),
                format!("{}x{}+{left}+{top}", right - left, bottom - top),
                "+repage".into(),
            ]);
        }
        preprocess.extend(
            [
                "-background",
                "black",
                "-alpha",
                "remove",
                "-alpha",
                "off",
                "-colorspace",
                "Gray",
            ]
            .map(str::to_owned),
        );
        if let Some(gamma) = portrait["gamma"].as_f64() {
            preprocess.extend(["-gamma".into(), gamma.to_string()]);
        }
        if let Some(contrast) = portrait["contrast"].as_f64() {
            preprocess.extend(["-sigmoidal-contrast".into(), format!("{contrast}x45%")]);
        }
        preprocess.push("png:-".into());
        let png = run("magick", &preprocess, None)?;
        let mut variants = json!({});
        for (name, width, height) in [("compact", 11, 5), ("standard", 15, 7), ("detailed", 21, 9)]
        {
            let size = format!("{width}x{height}");
            let args = [
                "--format",
                "symbols",
                "--colors",
                "none",
                "--symbols",
                "braille",
                "--size",
                &size,
                "--view-size",
                &size,
                "--stretch",
                "--font-ratio",
                "1/2",
                "--dither",
                "none",
                "--preprocess",
                "off",
                "--work",
                "9",
                "--probe",
                "off",
                "--margin-bottom",
                "0",
                "--margin-right",
                "0",
                "--fg",
                "white",
                "--bg",
                "black",
                "--animate",
                "off",
                "-",
            ];
            let output = String::from_utf8(run("chafa", &args.map(str::to_owned), Some(&png))?)?;
            let mut rows: Vec<String> = output
                .replace("\r\n", "\n")
                .split_terminator('\n')
                .map(str::to_owned)
                .collect();
            ensure!(rows.len() <= height, "Too many portrait rows");
            for row in &mut rows {
                ensure!(
                    row.chars()
                        .all(|c| c == ' ' || ('\u{2800}'..='\u{28ff}').contains(&c))
                        && row.chars().count() <= width,
                    "Portrait output is not bounded monochrome Braille"
                );
                row.push_str(&" ".repeat(width - row.chars().count()));
            }
            while rows.len() < height {
                rows.push(" ".repeat(width));
            }
            variants[name] = json!(rows);
        }
        println!(
            "{key}\n{}\n",
            variants["detailed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n")
        );
        portraits[key] = variants;
    }
    ensure!(selected > 0, "Unknown portrait key");
    ensure!(
        source["actors"]
            .as_object()
            .unwrap()
            .values()
            .all(|key| portraits.get(key.as_str().unwrap()).is_some()),
        "Assigned portrait is absent; run a complete bake"
    );
    std::fs::write(
        root.join("baked.json"),
        serde_json::to_string_pretty(
            &json!({"version":2,"format":"braille","actors":source["actors"],"portraits":portraits}),
        )? + "\n",
    )?;
    Ok(())
}
