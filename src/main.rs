use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;
use serde_json::Value;
use which::which;

#[derive(Debug, Deserialize)]
struct Chapter {
    title: String,
    start_time: f64,
    end_time: f64,
}

#[derive(Debug, Parser)]
#[command(
    name = "slycer",
    version,
    about = "Download and split YouTube audio by chapters"
)]
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    /// `YouTube` video URL
    input: String,

    /// Output audio file path
    #[arg(short = 'o', long = "output", default_value = "out.mp3")]
    output: PathBuf,

    /// Audio format for yt-dlp extraction
    #[arg(short = 'f', long = "audio-format", default_value = "mp3")]
    audio_format: String,

    /// Auto-approve installing missing dependencies (`yt-dlp`, `ffmpeg`)
    #[arg(short = 'y', long = "yes", default_value_t = false)]
    yes: bool,

    /// Keep the downloaded combined audio file (do not delete after splitting)
    #[arg(short = 'k', long = "keep", default_value_t = false)]
    keep: bool,

    /// Destination directory for split tracks
    #[arg(short = 'd', long = "dest")]
    dest: Option<PathBuf>,

    /// Prefix for output track filenames
    #[arg(long = "prefix")]
    prefix: Option<String>,

    /// Prepend zero-padded track numbers to filenames
    #[arg(long = "numbers", default_value_t = false)]
    numbers: bool,

    /// Use video title (processed) as prefix
    #[arg(long = "prefix-name", default_value_t = false)]
    prefix_name: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    ensure_binaries_present(cli.yes)?;

    let mp = MultiProgress::new();

    // Resolve input: file with URLs or single URL
    let maybe_path = Path::new(&cli.input);
    if maybe_path.is_file() {
        // batch mode
        let content = fs::read_to_string(maybe_path).context("Failed to read input file")?;
        let urls: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(ToOwned::to_owned)
            .collect();
        if urls.is_empty() {
            bail!("Input file contains no URLs");
        }

        // Filter only valid https:// links; log invalid lines in red
        let mut valid_urls: Vec<String> = Vec::new();
        let overall = mp.add(ProgressBar::new(u64::try_from(urls.len()).unwrap_or(0)));
        if let Ok(style) = ProgressStyle::with_template(
            "{spinner:.green} \x1b[90m{elapsed_precise}\x1b[0m [{bar:40.cyan/blue}] {pos}/{len} {msg}",
        ) {
            overall.set_style(style.progress_chars("#>-"));
        }
        overall.set_message("Processing URLs");
        overall.set_position(0);
        overall.enable_steady_tick(Duration::from_millis(100));

        for line in urls {
            if line.starts_with("https://") {
                valid_urls.push(line);
            } else {
                overall.println(format!("\x1b[31mSkipping invalid URL: {line}\x1b[0m"));
                overall.inc(1);
            }
        }

        for url in valid_urls {
            match download_and_split(&mp, &cli, &url) {
                Ok(()) => {}
                Err(err) => {
                    overall.println(format!("\x1b[31m{url}: {err}\x1b[0m"));
                }
            }
            overall.inc(1);
        }
        overall.finish_with_message("All done");
        Ok(())
    } else {
        // single URL
        download_and_split(&mp, &cli, &cli.input)
    }
}

#[allow(clippy::too_many_lines)]
fn download_and_split(mp: &MultiProgress, cli: &Cli, url: &str) -> Result<()> {
    // Download progress bar (starts as bar; will remain bar even if no percent)
    let dl_bar = mp.add(ProgressBar::new(1000));
    if let Ok(style) = ProgressStyle::with_template(
        "{spinner:.green} \x1b[90m{elapsed_precise}\x1b[0m [{bar:40.cyan/blue}] {msg}",
    ) {
        dl_bar.set_style(style.progress_chars("#>-"));
    }
    dl_bar.set_message("Downloading audio");
    dl_bar.set_position(0);
    dl_bar.enable_steady_tick(Duration::from_millis(100));

    // Logs bars: show last 5 non-progress lines (each on its own line, gray, no spinners)
    let mut logs_bars: Vec<ProgressBar> = Vec::with_capacity(5);
    for _ in 0..5 {
        let bar = mp.add(ProgressBar::new(0));
        if let Ok(style) = ProgressStyle::with_template("\x1b[90m{msg}\x1b[0m") {
            bar.set_style(style.progress_chars("#>-"));
        }
        logs_bars.push(bar);
    }

    // Build yt-dlp command
    let mut ytdlp = Command::new("yt-dlp");
    ytdlp.args([
        "--extract-audio",
        "--audio-format",
        &cli.audio_format,
        "--no-playlist",
        "--newline",
        "--output",
        &cli.output.to_string_lossy(),
        url,
    ]);
    run_ytdlp_with_progress(&dl_bar, &logs_bars, &mut ytdlp).context("yt-dlp failed")?;
    dl_bar.finish_and_clear();
    for bar in &logs_bars {
        bar.finish_and_clear();
    }

    // Metadata
    let json_spinner = mp.add(ProgressBar::new_spinner());
    if let Ok(style) =
        ProgressStyle::with_template("{spinner:.green} \x1b[90m{elapsed_precise}\x1b[0m {msg}")
    {
        json_spinner.set_style(style.progress_chars("#>-"));
    }
    json_spinner.enable_steady_tick(Duration::from_millis(100));
    json_spinner.set_message("Fetching video metadata");
    let metadata = fetch_metadata_json(url)?;
    json_spinner.finish_and_clear();
    // no top white logs

    let chapters = extract_chapters(&metadata)?;
    if chapters.is_empty() {
        bail!("No chapters found in the video metadata");
    }

    let total = u64::try_from(chapters.len()).unwrap_or(u64::MAX);
    let split_bar = mp.add(ProgressBar::new(total));
    if let Ok(style) = ProgressStyle::with_template(
        "{spinner:.green} \x1b[90m{elapsed_precise}\x1b[0m [{bar:40.cyan/blue}] {pos}/{len} {msg}",
    ) {
        split_bar.set_style(style.progress_chars("#>-"));
    }
    split_bar.set_message("Splitting audio");

    if let Some(ref dest_dir) = cli.dest {
        fs::create_dir_all(dest_dir).context("Failed to create destination directory")?;
    }

    let pad_width = compute_pad_width(cli.numbers, chapters.len());

    for (index, ch) in chapters.iter().enumerate() {
        let safe_title = sanitize(&ch.title).unwrap_or_else(|| format!("part-{}", index + 1));
        let title_prefix = if cli.prefix_name {
            make_title_prefix(&metadata)
        } else {
            None
        };
        let filename =
            build_output_filename(cli, index, pad_width, &safe_title, title_prefix.as_deref());
        let out_path = match &cli.dest {
            Some(dir) => dir.join(filename),
            None => PathBuf::from(&filename),
        };

        let start = ch.start_time.max(0.0);
        let duration = (ch.end_time - ch.start_time).max(0.0);
        if !duration.is_finite() || duration < 1.0 {
            split_bar.println(format!(
                "\x1b[90mSkipping '{}' (<1s duration)\x1b[0m",
                ch.title
            ));
            split_bar.inc(1);
            continue;
        }
        run_command(Command::new("ffmpeg").args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            &format!("{start:.3}"),
            "-t",
            &format!("{duration:.3}"),
            "-i",
            &cli.output.to_string_lossy(),
            "-c",
            "copy",
            &out_path.to_string_lossy(),
        ]))
        .with_context(|| format!("ffmpeg failed to split '{}'", ch.title))?;

        split_bar.inc(1);
    }
    split_bar.finish_and_clear();

    if !cli.keep {
        let _ = fs::remove_file(&cli.output);
    }
    Ok(())
}

fn ensure_binaries_present(auto_yes: bool) -> Result<()> {
    let required = ["yt-dlp", "ffmpeg"];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|bin| which(bin).is_err())
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let msg_list = missing.join(", ");
    if !auto_yes {
        eprint!("Missing binaries: {msg_list}. Install automatically? [y/N]: ");
        io::stderr().flush().ok();

        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("Failed to read user input")?;
        let ans = answer.trim().to_ascii_lowercase();
        let yes = matches!(ans.as_str(), "y" | "yes" | "д" | "да");
        if !yes {
            bail!("Required: {msg_list}. Install manually or run with --yes for auto-install");
        }
    }

    install_missing(&missing)?;

    // Re-check
    for bin in &missing {
        which(bin).with_context(|| format!("Бинарник '{bin}' не найден после установки"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn install_missing(missing: &[&str]) -> Result<()> {
    let spinner = new_spinner("Installing dependencies");

    #[allow(clippy::match_same_arms)]
    let installer = match std::env::consts::OS {
        "macos" => choose_first_available(&["brew"]),
        "linux" => choose_first_available(&["apt-get", "dnf", "yum", "pacman", "zypper", "apk"]),
        "windows" => choose_first_available(&["winget", "choco", "scoop"]),
        _ => None,
    };

    let Some(installer) = installer else {
        spinner.finish_with_message("Auto-install is unavailable");
        bail!(
            "Cannot determine package manager. Install manually: {}",
            missing.join(", ")
        );
    };

    // Run installers with minimal dependency footprint where possible
    let result: Result<()> = if cfg!(target_os = "macos") && installer == "brew" {
        // Disable auto-update and cleanup, install only requested formulae
        let mut cmd = Command::new("brew");
        cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
            .env("HOMEBREW_NO_ANALYTICS", "1")
            .args(["install", "--formula"])
            .args(missing);
        run_streaming_lines(&spinner, &mut cmd)
    } else if cfg!(target_os = "linux") {
        match installer {
            "apt-get" => {
                // Update index only, then install without recommends and without upgrading existing pkgs
                run_streaming_lines(
                    &spinner,
                    Command::new("sudo").args(["-n", "apt-get", "update"]),
                )?;
                run_streaming_lines(
                    &spinner,
                    Command::new("sudo")
                        .args([
                            "-n",
                            "apt-get",
                            "install",
                            "-y",
                            "--no-install-recommends",
                            "--no-upgrade",
                        ])
                        .args(missing),
                )
            }
            "dnf" => run_streaming_lines(
                &spinner,
                Command::new("sudo")
                    .args([
                        "-n",
                        "dnf",
                        "install",
                        "-y",
                        "--setopt=install_weak_deps=False",
                    ])
                    .args(missing),
            ),
            "yum" => run_streaming_lines(
                &spinner,
                Command::new("sudo")
                    .args(["-n", "yum", "install", "-y"])
                    .args(missing),
            ),
            "pacman" => run_streaming_lines(
                &spinner,
                Command::new("sudo")
                    .args(["-n", "pacman", "-S", "--noconfirm", "--needed"])
                    .args(missing),
            ),
            "zypper" => run_streaming_lines(
                &spinner,
                Command::new("sudo")
                    .args(["-n", "zypper", "install", "-y", "--no-recommends"])
                    .args(missing),
            ),
            "apk" => run_streaming_lines(
                &spinner,
                Command::new("sudo")
                    .args(["-n", "apk", "add", "--no-cache"])
                    .args(missing),
            ),
            _ => Err(anyhow::anyhow!("unsupported installer")),
        }
    } else if cfg!(target_os = "windows") {
        match installer {
            // Map to more exact IDs when possible
            "winget" => {
                let mapped: Vec<String> = missing
                    .iter()
                    .map(|&p| match p {
                        "ffmpeg" => "Gyan.FFmpeg".to_string(),
                        "yt-dlp" => "yt-dlp.yt-dlp".to_string(),
                        other => other.to_string(),
                    })
                    .collect();
                run_streaming_lines(
                    &spinner,
                    Command::new("winget")
                        .args([
                            "install",
                            "--silent",
                            "--accept-package-agreements",
                            "--accept-source-agreements",
                            "--exact",
                        ])
                        .args(&mapped),
                )
            }
            "choco" => run_streaming_lines(
                &spinner,
                Command::new("choco")
                    .args(["install", "-y", "--no-progress"])
                    .args(missing),
            ),
            "scoop" => run_streaming_lines(
                &spinner,
                Command::new("scoop").args(["install"]).args(missing),
            ),
            _ => Err(anyhow::anyhow!("unsupported installer")),
        }
    } else {
        Err(anyhow::anyhow!("unsupported os"))
    };

    if result.is_ok() {
        spinner.finish_with_message("Dependencies installed");
        Ok(())
    } else {
        spinner.finish_with_message("Auto-install failed");
        bail!(
            "Failed to install: {}. Install manually via package manager",
            missing.join(", ")
        );
    }
}

fn choose_first_available<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .find(|&&name| which(name).is_ok())
        .copied()
}

fn fetch_metadata_json(url: &str) -> Result<Value> {
    let output = Command::new("yt-dlp")
        .args(["-J", url])
        .output()
        .context("Failed to execute yt-dlp for JSON metadata")?;

    if !output.status.success() {
        bail!("yt-dlp -J returned non-zero exit code");
    }

    let json = serde_json::from_slice(&output.stdout).context("Invalid JSON from yt-dlp")?;
    Ok(json)
}

fn extract_chapters(v: &Value) -> Result<Vec<Chapter>> {
    let Some(chapters_val) = v.get("chapters") else {
        bail!("No 'chapters' field in metadata");
    };
    let chapters: Vec<Chapter> =
        serde_json::from_value(chapters_val.clone()).context("Failed to parse chapters")?;
    Ok(chapters)
}

fn sanitize(title: &str) -> Option<String> {
    let filtered: String = title
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | ' ' => ch,
            _ => ' ',
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");

    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn new_spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(100));
    if let Ok(style) = ProgressStyle::with_template("{spinner:.green} {msg}") {
        pb.set_style(style);
    }
    pb.set_message(msg.to_string());
    pb
}

fn run_command(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().context("Failed to start process")?;
    if status.success() {
        Ok(())
    } else {
        bail!("Process exited with status: {status}")
    }
}

// note: removed generic streaming helper in favor of yt-dlp specific progress handler

fn run_streaming_lines(pb: &ProgressBar, cmd: &mut Command) -> Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("Failed to start process")?;

    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;

    let pb_out = pb.clone();
    let out_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            pb_out.println(line);
        }
    });

    let pb_err = pb.clone();
    let err_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            pb_err.println(line);
        }
    });

    let status = child.wait().context("Failed to wait for process")?;
    let _ = out_handle.join();
    let _ = err_handle.join();
    if status.success() {
        Ok(())
    } else {
        bail!("Process exited with status: {status}")
    }
}

fn run_ytdlp_with_progress(
    pb: &ProgressBar,
    logs: &[ProgressBar],
    cmd: &mut Command,
) -> Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("Failed to start yt-dlp")?;

    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;

    // yt-dlp prints progress lines like:
    // "[download]  81.6% of   59.10MiB at    3.47MiB/s ETA 00:01"
    let logs_buffer = Arc::new(Mutex::new(VecDeque::with_capacity(5)));
    let pb_out = pb.clone();
    let logs_out_vec: Vec<ProgressBar> = logs.to_vec();
    let logs_out_buf = Arc::clone(&logs_buffer);
    let out_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some((permille, speed, _eta)) = parse_ytdlp_progress(&line) {
                pb_out.set_length(1000);
                pb_out.set_position(permille.min(1000));
                let percent_int = permille / 10;
                let percent_frac = permille % 10;
                let fixed_spd = speed
                    .as_deref()
                    .map_or_else(|| "          ".to_string(), |s| format!("{s:>10}"));
                let right = format!(" | {fixed_spd}");
                pb_out.set_message(format!(
                    "{percent_int}.{percent_frac}%\x1b[90m{right}\x1b[0m"
                ));
            } else {
                // hide non-progress logs during download
                pb_out.set_message("Downloading audio");
                if let Ok(mut dq) = logs_out_buf.lock() {
                    if dq.len() == 5 {
                        dq.pop_front();
                    }
                    dq.push_back(line);
                    // update log bars with last lines
                    let lines: Vec<String> = dq.iter().cloned().collect();
                    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
                    let slice = &lines[start..];
                    // clear all first
                    for b in &logs_out_vec {
                        b.set_message(String::new());
                    }
                    for (i, l) in slice.iter().enumerate() {
                        logs_out_vec[i].set_message(format!("\x1b[90m{l}\x1b[0m"));
                    }
                }
            }
        }
    });

    let pb_err = pb.clone();
    let logs_err_vec: Vec<ProgressBar> = logs.to_vec();
    let logs_err_buf = Arc::clone(&logs_buffer);
    let err_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if let Some((permille, speed, _eta)) = parse_ytdlp_progress(&line) {
                pb_err.set_length(1000);
                pb_err.set_position(permille.min(1000));
                let percent_int = permille / 10;
                let percent_frac = permille % 10;
                let fixed_spd = speed
                    .as_deref()
                    .map_or_else(|| "          ".to_string(), |s| format!("{s:>10}"));
                let right = format!(" | {fixed_spd}");
                pb_err.set_message(format!(
                    "{percent_int}.{percent_frac}%\x1b[90m{right}\x1b[0m"
                ));
            } else {
                // hide during success path, but keep logs for potential error reporting
                pb_err.set_message("Downloading audio");
                if let Ok(mut dq) = logs_err_buf.lock() {
                    if dq.len() == 5 {
                        dq.pop_front();
                    }
                    dq.push_back(line);
                    let lines: Vec<String> = dq.iter().cloned().collect();
                    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
                    let slice = &lines[start..];
                    for b in &logs_err_vec {
                        b.set_message(String::new());
                    }
                    for (i, l) in slice.iter().enumerate() {
                        logs_err_vec[i].set_message(format!("\x1b[90m{l}\x1b[0m"));
                    }
                }
            }
        }
        // no logs collected currently
        Vec::<String>::new()
    });

    let status = child.wait().context("Failed to wait for yt-dlp")?;
    let _ = out_handle.join();
    let err_logs = err_handle.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        for line in err_logs {
            eprintln!("{line}");
        }
        bail!("yt-dlp exited with status: {status}")
    }
}

fn parse_ytdlp_progress(line: &str) -> Option<(u64, Option<String>, Option<String>)> {
    if !line.starts_with("[download]") || !line.contains('%') {
        return None;
    }
    // Percent with one decimal
    let percent_part = line.split('%').next()?;
    let token = percent_part.split_whitespace().last()?;
    let mut it = token.split('.');
    let whole = it.next()?;
    let whole_num: u64 = whole.parse().ok()?;
    let frac_digit: u64 = it
        .next()
        .and_then(|s| s.chars().next())
        .and_then(|c| c.to_digit(10))
        .map_or(0, u64::from);
    let permille = whole_num
        .saturating_mul(10)
        .saturating_add(frac_digit)
        .min(1000);

    // Speed and ETA
    let mut pieces = line.split_whitespace();
    let mut speed: Option<String> = None;
    let mut eta: Option<String> = None;
    while let Some(word) = pieces.next() {
        if word == "at" {
            if let Some(val) = pieces.next() {
                if val == "Unknown" {
                    // skip unit after Unknown if present
                    let _ = pieces.next();
                } else {
                    let unit = pieces.next().unwrap_or("");
                    // some yt-dlp lines include trailing 'ETA' in the token stream; cut speed only
                    speed = Some(format!("{val} {unit}").replace(" ETA", ""));
                }
            }
        }
        if word == "ETA" {
            if let Some(val) = pieces.next() {
                if val != "Unknown" {
                    eta = Some(val.to_string());
                }
            }
        }
    }

    Some((permille, speed, eta))
}

fn compute_pad_width(use_numbers: bool, count: usize) -> usize {
    if !use_numbers {
        return 0;
    }
    match count {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        _ => 4,
    }
}

fn make_title_prefix(metadata: &Value) -> Option<String> {
    let title = metadata.get("title")?.as_str()?;
    // cut at first delimiter among " - ", "(", "["
    let mut cut_pos = title.len();
    if let Some(p) = title.find(" - ") {
        cut_pos = cut_pos.min(p);
    }
    if let Some(p) = title.find('(') {
        cut_pos = cut_pos.min(p);
    }
    if let Some(p) = title.find('[') {
        cut_pos = cut_pos.min(p);
    }
    let slice = &title[..cut_pos];
    let lowered = slice.to_lowercase();
    let sanitized = sanitize(&lowered)?;
    let mut chars = sanitized.chars().take(40).collect::<String>();
    // trim trailing underscore if cut in the middle of a word boundary
    while chars.ends_with('_') {
        chars.pop();
    }
    if chars.is_empty() { None } else { Some(chars) }
}

fn build_output_filename(
    cli: &Cli,
    index: usize,
    pad_width: usize,
    safe_title: &str,
    title_prefix: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(pfx) = &cli.prefix {
        if !pfx.is_empty() {
            parts.push(pfx.clone());
        }
    }
    if let Some(tp) = title_prefix {
        if !tp.is_empty() {
            parts.push(tp.to_string());
        }
    }
    if cli.numbers && pad_width > 0 {
        parts.push(format!("{:0width$}", index + 1, width = pad_width));
    }
    parts.push(safe_title.to_string());
    let name = parts.join("_");
    format!("{}.{}", name, cli.audio_format)
}
