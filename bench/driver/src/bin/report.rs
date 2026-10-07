//! `report <file.json>`: prints the tables of a run as Markdown.

use std::{env, fmt::Write as _, fs, process};

use driver::{
  record::{Phase, Report, Run},
  scenarios::SCENARIOS,
};

fn main() {
  let Some(file) = env::args().nth(1) else {
    eprintln!("report <file.json>   (prints Markdown)");
    process::exit(2);
  };
  let text = fs::read_to_string(&file).expect("the JSON file can be read");
  let report: Report = serde_json::from_str(&text).expect("the file is a report of `bench`");
  print!("{}", markdown(&report));
}

fn markdown(report: &Report) -> String {
  let mut out = String::new();
  let machine = &report.machine;
  let [one, five, fifteen] = machine.load_average;
  let _ = writeln!(out, "# pathwatch benchmark\n");
  let _ = writeln!(out, "- Started: {}", report.started);
  let _ = writeln!(
    out,
    "- Machine: {}, {} cores, {} GB, {} ({}), temporary directory on {}",
    machine.cpu,
    machine.cores,
    machine.memory_gb,
    machine.os,
    machine.kernel,
    machine.tmp_filesystem
  );
  let busy =
    if one > machine.cores as f64 / 4.0 { " **busy machine: times are rough**" } else { "" };
  let _ = writeln!(out, "- Open files limit of the adapters: {}", machine.open_files_limit);
  if let Some(setting) = &machine.huge_pages {
    let malloc = if report.settings.keep_huge_pages { "as the system has it" } else { "off" };
    let _ =
      writeln!(out, "- Transparent huge pages: {setting}; for malloc of the adapters: {malloc}");
  }
  let _ = writeln!(out, "- Load average at the start: {one:.1} {five:.1} {fifteen:.1}{busy}");
  let full = if (report.settings.scale - 1.0).abs() < f64::EPSILON {
    "the full sizes".to_owned()
  } else {
    format!("**sizes times {}: not the full sizes, not for comparison**", report.settings.scale)
  };
  let _ = writeln!(out, "- Sizes: {full}; a quiet phase is {} s", report.settings.quiet_secs);
  let _ = writeln!(out, "- Times are in milliseconds, memory is in MiB.\n");

  let _ = writeln!(out, "## Versions\n");
  table(
    &mut out,
    &["name", "version"],
    report.versions.iter().map(|(name, version)| vec![name.clone(), version.clone()]).collect(),
  );

  let _ = writeln!(out, "## Bases: each adapter when it watches nothing\n");
  table(
    &mut out,
    &["adapter", "start to ready", "CPU", "RSS", "footprint"],
    report
      .bases
      .iter()
      .map(|(name, base)| {
        vec![
          name.clone(),
          time(base.setup_ms),
          time(base.cpu_ms),
          mib(base.rss_kb),
          opt_mib(base.footprint_kb),
        ]
      })
      .collect(),
  );

  for scenario in SCENARIOS {
    let runs: Vec<&Run> = report.runs.iter().filter(|run| run.scenario == scenario.name).collect();
    if runs.is_empty() {
      continue;
    }
    let _ = writeln!(out, "## {}: {}\n", scenario.name.to_uppercase(), scenario.title);
    if let Some(run) = runs.iter().find(|run| !run.sizes.is_empty()) {
      let sizes: Vec<String> =
        run.sizes.iter().map(|(name, size)| format!("{name} {size}")).collect();
      let _ = writeln!(out, "Sizes: {}.\n", sizes.join(", "));
    }
    setup_table(&mut out, report, &runs);
    for name in phase_names(&runs) {
      phase_table(&mut out, &name, &runs);
    }
    notes(&mut out, &runs);
  }
  out
}

/// The names of the phases, in the order in which they ran.
fn phase_names(runs: &[&Run]) -> Vec<String> {
  let mut names: Vec<String> = Vec::new();
  for phase in runs.iter().flat_map(|run| &run.phases) {
    if !names.contains(&phase.name) {
      names.push(phase.name.clone());
    }
  }
  names
}

fn setup_table(out: &mut String, report: &Report, runs: &[&Run]) {
  let _ = writeln!(out, "### Setup and memory\n");
  let rows = runs
    .iter()
    .map(|run| {
      if run.status != "ok" {
        return vec![run.adapter.clone(), run.status.clone()];
      }
      let base = report.bases.get(&run.adapter);
      let above = base.map_or_else(String::new, |base| {
        format!("{:+.1}", (run.rss_ready_kb as f64 - base.rss_kb as f64) / 1024.0)
      });
      vec![
        run.adapter.clone(),
        time(run.setup_ms),
        time(run.setup_cpu_ms),
        mib(run.rss_ready_kb),
        above,
        opt_mib(run.footprint_ready_kb),
        mib(run.rss_end_kb),
        mib(run.peak_rss_kb),
        time(run.total_cpu_ms),
        run.errors.to_string(),
      ]
    })
    .collect();
  table(
    out,
    &[
      "adapter",
      "setup",
      "setup CPU",
      "RSS ready",
      "RSS above base",
      "footprint ready",
      "RSS end",
      "RSS peak",
      "CPU whole run",
      "errors",
    ],
    rows,
  );
}

fn phase_table(out: &mut String, name: &str, runs: &[&Run]) {
  let phases: Vec<(&Run, &Phase)> = runs
    .iter()
    .filter_map(|run| Some((*run, run.phases.iter().find(|phase| phase.name == name)?)))
    .collect();
  let _ = writeln!(out, "### {name}\n");
  // A phase with no changes is a quiet one: only the CPU time and the events count.
  if phases.iter().all(|(_, phase)| phase.changes == 0) {
    let rows = phases
      .iter()
      .map(|(run, phase)| {
        let events = phase.extra + phase.dir_events + phase.duplicates + phase.late;
        vec![run.adapter.clone(), time(phase.cpu_ms), time(phase.wall_ms), events.to_string()]
      })
      .collect();
    table(out, &["adapter", "CPU", "in (wall)", "events (none is right)"], rows);
    return;
  }
  let rows = phases
    .iter()
    .map(|(run, phase)| {
      let latency = phase.latency;
      let part = |value: Option<f64>| value.map_or_else(|| "-".to_owned(), time);
      let words: Vec<String> =
        phase.kinds.iter().map(|(word, count)| format!("{word} {count}")).collect();
      let mut changes = phase.changes.to_string();
      if phase.abandoned {
        changes += " (stopped early)";
      }
      vec![
        run.adapter.clone(),
        changes,
        phase.missed.to_string(),
        phase.extra.to_string(),
        phase.dir_events.to_string(),
        (phase.duplicates + phase.late).to_string(),
        part(latency.map(|latency| latency.p50_ms)),
        part(latency.map(|latency| latency.p95_ms)),
        part(latency.map(|latency| latency.max_ms)),
        time(phase.write_ms),
        time(phase.cpu_ms),
        words.join(", "),
      ]
    })
    .collect();
  table(
    out,
    &[
      "adapter",
      "changes",
      "missed",
      "extra",
      "dir events",
      "repeats",
      "p50",
      "p95",
      "max",
      "write",
      "CPU",
      "first word",
    ],
    rows,
  );
  for (run, phase) in &phases {
    if !phase.info.is_empty() {
      let numbers: Vec<String> =
        phase.info.iter().map(|(name, number)| format!("{name} {number}")).collect();
      let _ = writeln!(out, "- {}: {}", run.adapter, numbers.join(", "));
    }
  }
  if phases.iter().any(|(_, phase)| !phase.info.is_empty()) {
    let _ = writeln!(out);
  }
}

/// What the adapters said, their errors, and samples of the paths that were missed or extra.
fn notes(out: &mut String, runs: &[&Run]) {
  let mut lines = Vec::new();
  for run in runs {
    for note in &run.notes {
      lines.push(format!("{}: {}", run.adapter, note.replace(&run.root, "...")));
    }
    if run.errors > 0 {
      let first = run.error_samples[0].replace(&run.root, "...");
      lines.push(format!("{}: {} errors, the first: `{first}`", run.adapter, run.errors));
    }
    for phase in &run.phases {
      // The part of a path above the directory of the run says nothing.
      let short = |sample: &str| sample.replace(&run.root, "...");
      if let Some(sample) = phase.missed_samples.first() {
        let sample = short(sample);
        lines.push(format!("{}, {}: missed for example `{sample}`", run.adapter, phase.name));
      }
      if let Some(sample) = phase.extra_samples.first() {
        let sample = short(sample);
        lines.push(format!("{}, {}: extra for example `{sample}`", run.adapter, phase.name));
      }
    }
  }
  if lines.is_empty() {
    return;
  }
  let _ = writeln!(out, "### Notes\n");
  for line in lines {
    let _ = writeln!(out, "- {line}");
  }
  let _ = writeln!(out);
}

fn table(out: &mut String, head: &[&str], rows: Vec<Vec<String>>) {
  let _ = writeln!(out, "| {} |", head.join(" | "));
  let rules: Vec<&str> =
    head.iter().enumerate().map(|(at, _)| if at == 0 { "---" } else { "---:" }).collect();
  let _ = writeln!(out, "| {} |", rules.join(" | "));
  for row in rows {
    let _ = writeln!(out, "| {} |", row.join(" | "));
  }
  let _ = writeln!(out);
}

/// Milliseconds: one digit after the point below 100, none above.
fn time(ms: f64) -> String {
  if ms < 100.0 { format!("{ms:.1}") } else { format!("{ms:.0}") }
}

fn mib(kb: u64) -> String {
  format!("{:.1}", kb as f64 / 1024.0)
}

fn opt_mib(kb: Option<u64>) -> String {
  kb.map_or_else(|| "-".to_owned(), mib)
}
