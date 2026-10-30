// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! Speech to text with Parakeet through transcribe.cpp: the whole recording goes to the model,
//! which writes it down; a recording too quiet to hold speech is answered empty.

use std::path::Path;

use transcribe_cpp::{
    Backend, Model as Context, ModelOptions, RunOptions, Session, SessionOptions,
};

const SAMPLE_RATE: usize = 16_000;
/// A very short "yes" is padded, so the model is never handed an empty utterance.
const MIN_SAMPLES: usize = SAMPLE_RATE / 2;
/// Below this peak a recording is silence, not speech (-46 dBFS).
const QUIET: f32 = 0.005;

/// Whether a recording is too quiet to hold speech: no sample reaches [`QUIET`].
pub fn quiet(samples: &[f32]) -> bool {
    samples.iter().all(|sample| sample.abs() < QUIET)
}

/// A loaded Parakeet model and the session that runs it.
pub struct Model {
    session: Session,
    device: String,
}

impl Model {
    /// Loads `name` from `dir`, on `backend` if it is not the processor.
    pub fn load(dir: &Path, name: &str, backend: Backend, threads: i32) -> Result<Self, String> {
        let file = isle_core::speech::model_file(name)
            .ok_or_else(|| format!("unknown speech model {name}"))?;
        let path = dir.join(file);
        if !path.is_file() {
            // isled matches "not downloaded" to tell the user to run isle-setup-voice.
            return Err(format!("model {name} not downloaded"));
        }
        let options = ModelOptions {
            backend,
            ..ModelOptions::default()
        };
        let context = Context::load_with(&path, &options)
            .map_err(|err| format!("could not load the speech model: {err}"))?;
        // What it actually runs on ("cpu", "vulkan"), which tells isled a GPU was skipped.
        // Lower-cased: isled checks for "cpu" exactly.
        let device = context.backend().to_lowercase();
        let session = context
            .session_with(&SessionOptions {
                n_threads: threads,
                ..SessionOptions::default()
            })
            .map_err(|err| format!("could not load the speech model: {err}"))?;
        Ok(Self { session, device })
    }

    /// Where the model runs, as transcribe.cpp reports it.
    pub fn device(&self) -> &str {
        &self.device
    }

    /// What was said in `samples` (16 kHz mono), or empty for silence.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String, String> {
        if samples.is_empty() || quiet(samples) {
            return Ok(String::new());
        }
        let padded;
        let pcm = if samples.len() < MIN_SAMPLES {
            padded = {
                let mut pcm = samples.to_vec();
                pcm.resize(MIN_SAMPLES, 0.0);
                pcm
            };
            padded.as_slice()
        } else {
            samples
        };
        let transcript = self
            .session
            .run(pcm, &RunOptions::default())
            .map_err(|err| format!("speech recognition failed: {err}"))?;
        Ok(transcript.text.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_quiet() {
        assert!(quiet(&[]));
        assert!(quiet(&vec![0.0; SAMPLE_RATE]));
        assert!(quiet(&vec![0.004; SAMPLE_RATE]));
    }

    #[test]
    fn speech_is_not_quiet() {
        let mut speech = vec![0.0; SAMPLE_RATE];
        speech[100] = 0.2;
        assert!(!quiet(&speech));
    }
}
