These files are tiny, self-generated audio fixtures for metadata preservation tests.

Generation notes:
- Generated locally with `ffmpeg` from a synthetic sine wave (`lavfi`).
- No third-party copyrighted audio is included.
- Intended to stay small and deterministic for unit tests.

Formats included:
- `base.mp3`
- `base.flac`
- `base.ogg` (Vorbis)
- `base.opus`
- `base.m4a`
- `base.mp4` (audio-only MP4 container)
- `base.aac` (ADTS)
- `base.wav`
- `base.wv` (WavPack container; used for APE/ID3v1 legacy-tag coverage)

Test behavior:
- Preservation tests copy these fixtures to a temp directory, then seed dense metadata
  (ReplayGain + non-standard/custom fields) before save-roundtrip assertions.
- Legacy-format coverage is seeded in-test:
  - APEv2 fields on MPEG and WavPack fixtures.
  - ID3v1-only scenarios on MPEG and WavPack fixtures.
