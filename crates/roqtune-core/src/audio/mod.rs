//! Audio subsystem modules (decode, playback, probing, and option selection).

pub mod audio_converter;
pub mod audio_decoder;
pub mod audio_player;
pub mod audio_probe;
pub mod output_option_selection;
pub mod technical_metadata;

use std::sync::LazyLock;

use symphonia::core::codecs::CodecRegistry;

/// Extended codec registry: all Symphonia built-ins plus Opus via libopus.
///
/// Use this instead of `symphonia::default::get_codecs()` anywhere Opus
/// files need to be decoded.
static CODEC_REGISTRY: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut registry = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut registry);
    registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
    registry
});

/// Returns the extended codec registry (built-in Symphonia codecs + Opus).
pub(crate) fn get_codecs() -> &'static CodecRegistry {
    &CODEC_REGISTRY
}
