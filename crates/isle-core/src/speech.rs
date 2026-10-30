// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! The voice assistant's speech models: whisper.cpp's files and Parakeet's, which
//! `isle-setup-voice` downloads to [`MODELS_DIR`] in the user's data folder and the helpers
//! (`isle-whisper`, `isle-parakeet`) read.

/// The model sizes offered: Parakeet first (the default, a different engine, English only), then
/// whisper.cpp's, smallest first.
pub const MODELS: [&str; 5] = [PARAKEET, "tiny", "base", "small", "medium"];

/// Parakeet's model name in Settings; `isle-parakeet` runs it, `isle-whisper` every other.
pub const PARAKEET: &str = "parakeet";

/// The Parakeet file `isle-setup-voice` keeps, as published by handy-computer.
pub const PARAKEET_FILE: &str = "parakeet-unified-en-0.6b-Q8_0.gguf";

/// Where the models are kept, within the user's data folder (`~/.local/share`).
pub const MODELS_DIR: &str = "isle/models";

/// Silero's voice activity detector, so Whisper only hears the parts where someone speaks.
pub const VAD_FILE: &str = "ggml-silero-v5.1.2.bin";

/// The file a model is kept in, if it is one of [`MODELS`]: 8-bit, about half the size of the
/// full model and as accurate; Parakeet keeps its published GGUF name.
pub fn model_file(model: &str) -> Option<String> {
    if model == PARAKEET {
        Some(PARAKEET_FILE.to_owned())
    } else {
        MODELS
            .contains(&model)
            .then(|| format!("ggml-{model}-q8_0.bin"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_quantized_file() {
        assert_eq!(model_file("small").as_deref(), Some("ggml-small-q8_0.bin"));
        assert_eq!(model_file("tiny").as_deref(), Some("ggml-tiny-q8_0.bin"));
        assert_eq!(
            model_file("parakeet").as_deref(),
            Some("parakeet-unified-en-0.6b-Q8_0.gguf")
        );
    }

    #[test]
    fn refuses_what_is_not_a_model() {
        assert_eq!(model_file("large"), None);
        assert_eq!(model_file(""), None);
        assert_eq!(model_file("../small"), None);
    }
}
