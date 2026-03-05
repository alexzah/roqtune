//! Configuration types for batch audio conversion.
//!
//! This module defines the [`ConversionConfig`] and per-format settings used
//! when batch file operations are run in convert mode.

/// Target format for batch audio conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConversionFormat {
    #[default]
    Wav,
    Flac,
    Opus,
    Mp3,
}

impl ConversionFormat {
    /// Returns the file extension (without leading dot) for this format.
    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Opus => "opus",
            Self::Mp3 => "mp3",
        }
    }
}

/// Bit depth for WAV output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WavBitDepth {
    #[default]
    Bits16,
    Bits24,
    Float32,
}

/// Settings for WAV encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavSettings {
    pub bit_depth: WavBitDepth,
}

impl Default for WavSettings {
    fn default() -> Self {
        Self {
            bit_depth: WavBitDepth::Bits16,
        }
    }
}

/// Bit depth for FLAC output (lossless integer formats).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlacBitDepth {
    #[default]
    Bits16,
    Bits24,
}

/// Settings for FLAC encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlacSettings {
    /// Compression level 0 (fastest, largest file) through 8 (slowest, smallest file).
    pub compression: u8,
    pub bit_depth: FlacBitDepth,
}

impl Default for FlacSettings {
    fn default() -> Self {
        Self {
            compression: 5,
            bit_depth: FlacBitDepth::Bits16,
        }
    }
}

/// Opus output bitrate options (kbps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusBitrate {
    Kbps64,
    Kbps96,
    #[default]
    Kbps128,
    Kbps192,
    Kbps256,
}

impl OpusBitrate {
    /// Bitrate in bits per second for the encoder.
    pub fn bits_per_second(self) -> i32 {
        match self {
            Self::Kbps64 => 64_000,
            Self::Kbps96 => 96_000,
            Self::Kbps128 => 128_000,
            Self::Kbps192 => 192_000,
            Self::Kbps256 => 256_000,
        }
    }
}

/// Settings for Opus encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusSettings {
    pub bitrate: OpusBitrate,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self {
            bitrate: OpusBitrate::Kbps128,
        }
    }
}

/// MP3 encoding mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mp3Mode {
    /// Constant bitrate.
    Cbr,
    /// Variable bitrate (recommended).
    #[default]
    Vbr,
}

/// MP3 CBR bitrate options (kbps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mp3CbrBitrate {
    Kbps128,
    Kbps192,
    #[default]
    Kbps256,
    Kbps320,
}

/// MP3 VBR quality levels (V0 = highest quality, V4 = lower quality but smaller files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mp3VbrQuality {
    V0,
    #[default]
    V2,
    V4,
}

impl Mp3VbrQuality {
    /// LAME VBR quality value (0 = best, 9 = worst).
    pub fn lame_value(self) -> u8 {
        match self {
            Self::V0 => 0,
            Self::V2 => 2,
            Self::V4 => 4,
        }
    }
}

/// Settings for MP3 encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp3Settings {
    pub mode: Mp3Mode,
    pub cbr_bitrate: Mp3CbrBitrate,
    pub vbr_quality: Mp3VbrQuality,
}

impl Default for Mp3Settings {
    fn default() -> Self {
        Self {
            mode: Mp3Mode::Vbr,
            cbr_bitrate: Mp3CbrBitrate::Kbps256,
            vbr_quality: Mp3VbrQuality::V2,
        }
    }
}

/// Complete conversion configuration: target format and all per-format settings.
///
/// All per-format settings are always present; only the active `format` is used
/// during encoding. This makes it easy to preserve settings across format switches.
#[derive(Debug, Clone, Default)]
pub struct ConversionConfig {
    pub format: ConversionFormat,
    pub wav: WavSettings,
    pub flac: FlacSettings,
    pub opus: OpusSettings,
    pub mp3: Mp3Settings,
}
