//! `report <file.json>...`: prints the tables of a run as Markdown. With more files than one,
//! the runs come one after the other in one document, each under the name of its OS.

use std::{collections::BTreeMap, env, fmt::Write as _, fs, process};

use driver::{
  record::{Latency, Phase, Report, Run},
  scenarios::SCENARIOS,
};

fn main() {
  let files: Vec<String> = env::args().skip(1).collect();
  if files.is_empty() {
    eprintln!("report <file.json>...   (prints Markdown)");
    process::exit(2);
  }
  let reports: Vec<Report> = files
    .iter()
    .map(|file| {
      let text = fs::read_to_string(file).expect("the JSON file can be read");
      serde_json::from_str(&text).expect("the file is a report of `bench`")
    })
    .collect();
  if let [report] = &reports[..] {
    print!("{}", markdown(report));
    return;
  }
  println!("# pathwatch benchmark\n");
  println!(
    "The last full run on each OS, one after the other. What the columns mean, and how the \
     numbers are taken: [README.md](README.md).\n"
  );
  for report in &reports {
    println!("## {}\n", os(&report.machine.kernel));
    // Without the title, and every heading one level down.
    for line in markdown(report).lines().skip(2) {
      let deeper = if line.starts_with('#') { "#" } else { "" };
      println!("{deeper}{line}");
    }
  }
}

fn os(kernel: &str) -> &'static str {
  match kernel.split(' ').next() {
    Some("Darwin") => "macOS",
    Some("Windows") => "Windows",
    _ => "Linux",
  }
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
  // Windows has no such limit.
  if machine.open_files_limit > 0 {
    let _ = writeln!(out, "- Open files limit of the adapters: {}", machine.open_files_limit);
  }
  if let Some(setting) = &machine.huge_pages {
    let malloc = if report.settings.keep_huge_pages { "as the system has it" } else { "off" };
    let _ =
      writeln!(out, "- Transparent huge pages: {setting}; for malloc of the adapters: {malloc}");
  }
  if machine.kernel.starts_with("Windows") {
    // Windows has no load average: see `machine::machine`.
    let _ = writeln!(out, "- Busy cores at the start (half a second): {one:.1}{busy}");
  } else {
    let _ = writeln!(out, "- Load average at the start: {one:.1} {five:.1} {fifteen:.1}{busy}");
  }
  let full = if (report.settings.scale - 1.0).abs() < f64::EPSILON {
    "the full sizes".to_owned()
  } else {
    format!("**sizes times {}: not the full sizes, not for comparison**", report.settings.scale)
  };
  let _ = writeln!(out, "- Sizes: {full}; a quiet phase is {} s", report.settings.quiet_secs);
  if report.settings.repeat > 1 {
    let _ = writeln!(
      out,
      "- Each scenario ran {} times with each adapter: a time or a size is the middle one of \
       the runs, a count (missed, extra, errors) is the worst one.",
      report.settings.repeat
    );
  }
  let waited: f64 = report.runs.iter().map(|run| run.os_wait_ms).sum();
  if waited > 0.0 {
    let gave_up: usize = report.runs.iter().map(|run| run.os_waits_given_up).sum();
    let gave_up = if gave_up == 0 {
      String::new()
    } else {
      format!(", and gave up {gave_up} times after {} s", driver::os::LONGEST.as_secs())
    };
    let _ = writeln!(
      out,
      "- Before each run and each phase the driver waited until fseventsd was idle: {:.0} s in \
       all{gave_up}.",
      waited / 1000.0
    );
  }
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

  behind(&mut out, report);

  let middle_runs = middle_runs(&report.runs);
  for scenario in SCENARIOS {
    let runs: Vec<&Run> = middle_runs.iter().filter(|run| run.scenario == scenario.name).collect();
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

/// The middle one of some numbers. For an even count: the lower of the two in the middle.
fn middle(mut numbers: Vec<f64>) -> f64 {
  numbers.sort_by(f64::total_cmp);
  numbers.get(numbers.len().saturating_sub(1) / 2).copied().unwrap_or_default()
}

/// The runs of each scenario with each adapter as one run: the middle one of each time and of
/// each size, and the worst of each count. A run that failed is left out, if another did not.
fn middle_runs(runs: &[Run]) -> Vec<Run> {
  let mut groups: Vec<Vec<&Run>> = Vec::new();
  for run in runs {
    let same =
      |group: &&mut Vec<&Run>| group[0].scenario == run.scenario && group[0].adapter == run.adapter;
    match groups.iter_mut().find(same) {
      Some(group) => group.push(run),
      None => groups.push(vec![run]),
    }
  }
  groups.iter().map(|group| middle_run(group)).collect()
}

fn middle_run(all: &[&Run]) -> Run {
  let runs: Vec<&Run> = all.iter().copied().filter(|run| run.status == "ok").collect();
  let Some(first) = runs.first() else { return all[0].clone() };
  let time = |of: fn(&Run) -> f64| middle(runs.iter().map(|run| of(run)).collect());
  let size = |of: fn(&Run) -> u64| middle(runs.iter().map(|run| of(run) as f64).collect()) as u64;
  let part = |of: fn(&Run) -> Option<u64>| {
    let sizes: Vec<f64> = runs.iter().filter_map(|run| of(run)).map(|size| size as f64).collect();
    (!sizes.is_empty()).then(|| middle(sizes) as u64)
  };
  let worst = runs.iter().max_by_key(|run| run.errors).unwrap_or(first);
  let mut run = Run {
    errors: worst.errors,
    error_samples: worst.error_samples.clone(),
    setup_ms: time(|run| run.setup_ms),
    setup_cpu_ms: time(|run| run.setup_cpu_ms),
    rss_ready_kb: size(|run| run.rss_ready_kb),
    footprint_ready_kb: part(|run| run.footprint_ready_kb),
    rss_end_kb: size(|run| run.rss_end_kb),
    footprint_end_kb: part(|run| run.footprint_end_kb),
    peak_rss_kb: size(|run| run.peak_rss_kb),
    total_cpu_ms: time(|run| run.total_cpu_ms),
    ..(*first).clone()
  };
  // A note of any run: the system refused a change in one of them, say.
  for note in runs.iter().flat_map(|run| &run.notes) {
    if !run.notes.contains(note) {
      run.notes.push(note.clone());
    }
  }
  if runs.len() < all.len() {
    run.notes.push(format!(
      "{} of {} runs failed and are left out",
      all.len() - runs.len(),
      all.len()
    ));
  }
  for phase in &mut run.phases {
    let name = phase.name.clone();
    let same: Vec<&Phase> =
      runs.iter().filter_map(|run| run.phases.iter().find(|phase| phase.name == name)).collect();
    *phase = middle_phase(&same);
  }
  run
}

fn middle_phase(phases: &[&Phase]) -> Phase {
  let time = |of: fn(&Phase) -> f64| middle(phases.iter().map(|phase| of(phase)).collect());
  let count = |of: fn(&Phase) -> usize| phases.iter().map(|phase| of(phase)).max().unwrap_or(0);
  let latencies: Vec<Latency> = phases.iter().filter_map(|phase| phase.latency).collect();
  let latency = |of: fn(&Latency) -> f64| middle(latencies.iter().map(of).collect());
  let mut info: BTreeMap<String, Vec<f64>> = BTreeMap::new();
  for (name, number) in phases.iter().flat_map(|phase| &phase.info) {
    info.entry(name.clone()).or_default().push(*number);
  }
  // The samples of the run that missed the most, and of the one with the most extra events.
  let most_missed = phases.iter().max_by_key(|phase| phase.missed).expect("a phase of a run");
  let most_extra = phases.iter().max_by_key(|phase| phase.extra).expect("a phase of a run");
  Phase {
    missed: most_missed.missed,
    missed_samples: most_missed.missed_samples.clone(),
    duplicates: count(|phase| phase.duplicates),
    late: count(|phase| phase.late),
    dir_events: count(|phase| phase.dir_events),
    extra: most_extra.extra,
    extra_samples: most_extra.extra_samples.clone(),
    access: count(|phase| phase.access),
    latency: (!latencies.is_empty()).then(|| Latency {
      p50_ms: latency(|latency| latency.p50_ms),
      p95_ms: latency(|latency| latency.p95_ms),
      max_ms: latency(|latency| latency.max_ms),
    }),
    write_ms: time(|phase| phase.write_ms),
    wall_ms: time(|phase| phase.wall_ms),
    cpu_ms: time(|phase| phase.cpu_ms),
    // The full count: a run that stopped early is said by `abandoned`.
    changes: count(|phase| phase.changes),
    abandoned: phases.iter().any(|phase| phase.abandoned),
    info: info.into_iter().map(|(name, numbers)| (name, middle(numbers))).collect(),
    ..phases[0].clone()
  }
}

/// One number of the tables, for every adapter that has it: each run gives one value.
struct Number {
  scenario: String,
  /// `setup`, or the name of a phase.
  place: String,
  name: &'static str,
  is_memory: bool,
  values: BTreeMap<String, Vec<f64>>,
}

/// Every number in which two adapters can be compared: lower is better in each. A run in
/// which an adapter gave up gives no number of the setup for that adapter: what it set up did
/// not work. A phase in
/// which an adapter missed a change, or gave up, gives no time for that adapter: it did not
/// do the job.
fn numbers(report: &Report) -> Vec<Number> {
  let mut numbers: Vec<Number> = Vec::new();
  let mut add = |run: &Run, place: &str, name: &'static str, value: f64| {
    let at = numbers.iter().position(|number| {
      (&*number.scenario, &*number.place, number.name) == (&*run.scenario, place, name)
    });
    let at = at.unwrap_or_else(|| {
      numbers.push(Number {
        scenario: run.scenario.clone(),
        place: place.to_owned(),
        name,
        is_memory: name == "memory above base",
        values: BTreeMap::new(),
      });
      numbers.len() - 1
    });
    numbers[at].values.entry(run.adapter.clone()).or_default().push(value);
  };
  for run in report.runs.iter().filter(|run| run.status == "ok") {
    // A watcher that stopped to report did not set up what it was asked for.
    let set_up = !run.phases.iter().any(|phase| phase.abandoned);
    if set_up {
      add(run, "setup", "time", run.setup_ms);
      add(run, "setup", "CPU", run.setup_cpu_ms);
    }
    if set_up && let Some(base) = report.bases.get(&run.adapter) {
      // The memory of the process alone where both have it, the resident size where not.
      let (with, without) = match (run.footprint_ready_kb, base.footprint_kb) {
        (Some(with), Some(without)) => (with, without),
        _ => (run.rss_ready_kb, base.rss_kb),
      };
      add(run, "setup", "memory above base", (with as f64 - without as f64).max(0.0) / 1024.0);
    }
    for phase in &run.phases {
      if phase.changes == 0 {
        add(run, &phase.name, "CPU", phase.cpu_ms);
      } else if let Some(latency) = phase.latency.filter(|_| phase.missed == 0 && !phase.abandoned)
      {
        add(run, &phase.name, "p50", latency.p50_ms);
        add(run, &phase.name, "max", latency.max_ms);
        add(run, &phase.name, "CPU", phase.cpu_ms);
      }
    }
  }
  numbers
}

/// The summary: every number in which pathwatch is behind another adapter that did the job,
/// and every place where pathwatch itself was wrong.
fn behind(out: &mut String, report: &Report) {
  const OURS: [&str; 2] = ["pathwatch", "pathwatch-0"];
  if !report.runs.iter().any(|run| OURS.contains(&run.adapter.as_str())) {
    return;
  }
  let _ = writeln!(out, "## Where pathwatch is behind\n");
  let _ = writeln!(
    out,
    "Every number in which pathwatch is more than 10% behind the best other library, and by \
     more than 1 ms or 0.5 MiB. Only a library that did the job counts: it missed no change of \
     the phase, and for the setup it stopped early in no phase of the run. `pathwatch` has the default settings, and `pathwatch-0` is the same without the \
     quiet time that pathwatch waits on purpose. `times` is `pathwatch-0` next to the best other, \
     and `pathwatch` where there is no `pathwatch-0`. With more runs than one, a number is \
     `middle (lowest to highest)`.\n"
  );
  let cell = |number: &Number, values: Option<&Vec<f64>>| {
    let Some(values) = values else { return String::new() };
    let show = |value: f64| if number.is_memory { format!("{value:.1}") } else { time(value) };
    let (least, most) = values
      .iter()
      .fold((f64::MAX, f64::MIN), |(least, most), value| (least.min(*value), most.max(*value)));
    let middle = show(middle(values.clone()));
    if values.len() > 1 { format!("{middle} ({} to {})", show(least), show(most)) } else { middle }
  };
  // The rows of the three tables: behind with both settings, only with the wait, only without.
  let mut rows: [Vec<Vec<String>>; 3] = Default::default();
  let (mut compared, mut near_best) = (0, [0, 0]);
  for number in numbers(report) {
    let others = number.values.iter().filter(|(adapter, _)| !OURS.contains(&adapter.as_str()));
    let Some((best_name, best)) =
      others.min_by(|a, b| middle(a.1.clone()).total_cmp(&middle(b.1.clone())))
    else {
      continue;
    };
    let [with_wait, without_wait] = OURS.map(|name| number.values.get(name));
    if with_wait.is_none() && without_wait.is_none() {
      continue;
    }
    let (best_middle, floor) = (middle(best.clone()), if number.is_memory { 0.5 } else { 1.0 });
    let is_behind = |values: Option<&Vec<f64>>| {
      values
        .map(|values| middle(values.clone()))
        .is_some_and(|ours| ours > best_middle * 1.1 && ours - best_middle > floor)
    };
    compared += 1;
    near_best[0] += usize::from(with_wait.is_some() && !is_behind(with_wait));
    near_best[1] += usize::from(without_wait.is_some() && !is_behind(without_wait));
    let table = match (is_behind(with_wait), is_behind(without_wait)) {
      (false, false) => continue,
      // A setting that did not do the job has no number: the other one decides.
      (true, true) => 0,
      (true, false) => usize::from(without_wait.is_some()),
      (false, true) => {
        if with_wait.is_some() {
          2
        } else {
          0
        }
      }
    };
    let nearest = without_wait.or(with_wait).map_or(0.0, |values| middle(values.clone()));
    let times =
      if best_middle >= 0.05 { format!("{:.1}", nearest / best_middle) } else { "-".to_owned() };
    rows[table].push(vec![
      number.scenario.to_uppercase(),
      number.place.clone(),
      number.name.to_owned(),
      cell(&number, with_wait),
      cell(&number, without_wait),
      cell(&number, Some(best)),
      best_name.clone(),
      times,
    ]);
  }
  let head =
    ["scenario", "where", "number", "pathwatch", "pathwatch-0", "best other", "which", "times"];
  let titles = [
    "Behind with and without the wait",
    "Behind only with the wait: the price of the quiet time",
    "Behind only without the wait",
  ];
  for (title, rows) in titles.iter().zip(rows) {
    let _ = writeln!(out, "### {title}\n");
    if rows.is_empty() {
      let _ = writeln!(out, "In no number.\n");
    } else {
      table(out, &head, rows);
    }
  }
  let _ = writeln!(
    out,
    "Of {compared} numbers that can be compared, `pathwatch` is the best or near it (inside \
     the limits above) in {}, and `pathwatch-0` in {}.\n",
    near_best[0], near_best[1]
  );

  let ours = |run: &&Run| OURS.contains(&run.adapter.as_str());
  let mut wrong = Vec::new();
  // Every run, not the middle one: a run that failed is left out of the middle.
  let mut numbers: BTreeMap<(&str, &str), usize> = BTreeMap::new();
  for run in report.runs.iter().filter(ours) {
    let number = numbers.entry((&run.scenario, &run.adapter)).or_default();
    *number += 1;
    if run.status != "ok" || run.errors > 0 {
      let which =
        if report.settings.repeat > 1 { format!(" (run {number})") } else { String::new() };
      wrong.push(format!(
        "{} {}{which}: {}, {} errors",
        run.scenario.to_uppercase(),
        run.adapter,
        run.status,
        run.errors
      ));
    }
  }
  for run in middle_runs(&report.runs).iter().filter(ours) {
    for phase in &run.phases {
      // In a quiet phase every event is one too many.
      let quiet = phase.changes == 0;
      let events = phase.extra + phase.duplicates + phase.late + phase.dir_events;
      let what = if quiet {
        (events > 0).then(|| format!("{events} events in a quiet time"))
      } else {
        (phase.missed + phase.extra > 0 || phase.abandoned).then(|| {
          let early = if phase.abandoned { ", stopped early" } else { "" };
          format!("{} missed, {} extra{early}", phase.missed, phase.extra)
        })
      };
      if let Some(what) = what {
        wrong.push(format!(
          "{} {}, {}: {what}",
          run.scenario.to_uppercase(),
          run.adapter,
          phase.name
        ));
      }
    }
  }
  if wrong.is_empty() {
    let _ = writeln!(out, "pathwatch missed no change, gave no event too many, and no error.\n");
  } else {
    let _ = writeln!(out, "Where pathwatch was wrong:\n");
    for line in wrong {
      let _ = writeln!(out, "- {line}");
    }
    let _ = writeln!(out);
  }
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
      lines.push(format!("{}: {}", run.adapter, short(note, &run.root)));
    }
    if run.errors > 0 {
      let first = short(&run.error_samples[0], &run.root);
      lines.push(format!("{}: {} errors, the first: `{first}`", run.adapter, run.errors));
    }
    for phase in &run.phases {
      if let Some(sample) = phase.missed_samples.first() {
        let sample = short(sample, &run.root);
        lines.push(format!("{}, {}: missed for example `{sample}`", run.adapter, phase.name));
      }
      if let Some(sample) = phase.extra_samples.first() {
        let sample = short(sample, &run.root);
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

/// The part of a path above the directory of the run says nothing, and names the machine. An
/// error of a library can hold the path the way `Debug` prints it, with doubled backslashes.
fn short(text: &str, root: &str) -> String {
  text.replace(&root.replace('\\', "\\\\"), "...").replace(root, "...")
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
