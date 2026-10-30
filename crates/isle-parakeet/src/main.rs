// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! `isle-parakeet`: speech to text for isled, through transcribe.cpp (Parakeet) on the GPU through
//! Vulkan or on the processor. Says `{"ready": true, "device": ...}` once loaded, then answers each
//! `{"audio": path}` line (raw 16 kHz mono s16le) with `{"text"}` or `{"error"}`.

mod protocol;
mod speech;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use transcribe_cpp::{Backend, DeviceType};

const USAGE: &str = "\
Usage: isle-parakeet --model <parakeet> [--language <codes>] [--cpu]
       isle-parakeet --probe [--cpu]
  --model     the speech model, downloaded by isle-setup-voice
  --language  accepted for compatibility; Parakeet speaks English only
  --cpu       never use the GPU
  --probe     print where speech recognition would run, as JSON, and quit
It reads {\"audio\": \"path\"} lines (raw 16 kHz mono s16le) and answers {\"text\"} or {\"error\"}.
ISLE_WHISPER_THREADS sets the processor threads (default: half the cores).";

/// What to run with, from the command line.
#[derive(Debug, Default, PartialEq)]
struct Args {
    model: String,
    cpu: bool,
    probe: bool,
}

/// What the command line asked for.
enum Parsed {
    Run(Args),
    Help,
    Version,
}

/// Reads the arguments; the model is `parakeet` unless one is named.
fn parse(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut parsed = Args {
        model: "parakeet".into(),
        ..Args::default()
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--model" => parsed.model = args.next().ok_or("--model needs a name")?,
            // Parakeet is English only; accepted so isled can pass the setting unchanged.
            "--language" => {
                args.next().ok_or("--language needs codes")?;
            }
            "--cpu" => parsed.cpu = true,
            "--probe" => parsed.probe = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Parsed::Run(parsed))
}

/// The threads isled allows (half the cores, so transcribing never takes the whole computer).
fn threads() -> i32 {
    std::env::var("ISLE_WHISPER_THREADS")
        .ok()
        .and_then(|n| n.trim().parse::<i32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1) as i32)
        })
}

/// Where `isle-setup-voice` put the models: `$XDG_DATA_HOME`, else `~/.local/share`.
fn models_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_default()
        .join(isle_core::speech::MODELS_DIR)
}

/// One JSON line to isled.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    // isled gone: nothing left to tell.
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// The GPUs transcribe.cpp found, with their names and whether they are integrated.
fn gpus() -> Vec<(String, bool)> {
    // Static builds already registered their backends; this is a no-op there.
    transcribe_cpp::init_backends_default().ok();
    transcribe_cpp::devices()
        .into_iter()
        .filter_map(|device| match device.device_type {
            DeviceType::Gpu => Some((device.description, false)),
            DeviceType::Igpu => Some((device.description, true)),
            _ => None,
        })
        .collect()
}

/// The first discrete GPU, else none (the processor): an integrated one was slower than the CPU in
/// the whisper path and is left alone unless the user pinned it with `GGML_VK_VISIBLE_DEVICES`.
fn choose(gpus: &[(String, bool)], pinned: bool) -> Option<&(String, bool)> {
    if pinned {
        gpus.first()
    } else {
        gpus.iter().find(|(_, integrated)| !integrated)
    }
}

/// Loads the model, then transcribes one request per line until isled closes stdin.
fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("isle-parakeet {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("isle-parakeet: {err}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // transcribe.cpp and ggml log every step to stderr; ISLE_WHISPER_DEBUG=1 shows it.
    if std::env::var_os("ISLE_WHISPER_DEBUG").is_none() {
        transcribe_cpp::disable_logging();
    }

    let pinned = std::env::var_os("GGML_VK_VISIBLE_DEVICES").is_some();
    let found = if args.cpu { Vec::new() } else { gpus() };
    let gpu = choose(&found, pinned);
    let device = gpu.map_or("cpu", |(name, _)| name.as_str());
    if args.probe {
        let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        say(&serde_json::json!({"device": device, "gpus": names}).to_string());
        return ExitCode::SUCCESS;
    }

    // The processor when no GPU was chosen (or `--cpu`), else let transcribe.cpp pick the device.
    let backend = if gpu.is_some() {
        Backend::Auto
    } else {
        Backend::Cpu
    };
    let mut model = match speech::Model::load(&models_dir(), &args.model, backend, threads()) {
        Ok(model) => model,
        Err(err) => {
            say(&protocol::error(&err));
            return ExitCode::FAILURE;
        }
    };
    say(&protocol::ready(model.device()));

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let reply = protocol::request(&line)
            .and_then(|path| {
                std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))
            })
            .and_then(|bytes| model.transcribe(&protocol::samples(&bytes)));
        say(&match reply {
            Ok(text) => protocol::text(&text),
            Err(err) => protocol::error(&err),
        });
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Args, String> {
        match parse(args.iter().map(|arg| arg.to_string()))? {
            Parsed::Run(args) => Ok(args),
            _ => Err("not a run".into()),
        }
    }

    #[test]
    fn reads_what_isled_passes() {
        let args = run(&["--model", "parakeet", "--language", "en"]).unwrap();
        assert_eq!(args.model, "parakeet");
        assert!(!args.cpu && !args.probe);
    }

    #[test]
    fn parakeet_by_default() {
        assert_eq!(run(&[]).unwrap().model, "parakeet");
        assert!(run(&["--probe", "--cpu"]).unwrap().cpu);
    }

    #[test]
    fn refuses_unknown_arguments() {
        assert!(run(&["--gpu"]).is_err());
        assert!(run(&["--model"]).is_err());
        assert!(run(&["--language"]).is_err());
    }

    #[test]
    fn takes_the_discrete_gpu() {
        let found = [
            ("Intel UHD".to_owned(), true),
            ("NVIDIA GTX 1650".to_owned(), false),
        ];
        assert_eq!(choose(&found, false).unwrap().0, "NVIDIA GTX 1650");
        assert_eq!(choose(&found, true).unwrap().0, "Intel UHD");
        assert!(choose(&[], false).is_none());
    }
}
